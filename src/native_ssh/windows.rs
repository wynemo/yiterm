use super::{auth::Prompt, config, transfer};
use anyhow::{Context, Result, bail, ensure};
use std::{ffi::OsString, path::Path, sync::Arc, time::Duration};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, LocalFree},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SDDL_REVISION_1,
        },
        GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    Storage::FileSystem::{ReadFile, WriteFile},
    System::{
        Console::*,
        Pipes::GetNamedPipeServerProcessId,
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
};

fn check(ok: i32) -> Result<()> {
    if ok == 0 {
        Err(std::io::Error::last_os_error().into())
    } else {
        Ok(())
    }
}
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

// A pipe name is unguessable between panes, but the ACL is the security boundary:
// only this Windows account can connect; remote pipe clients are also rejected.
struct PipeSecurity(*mut std::ffi::c_void);
impl PipeSecurity {
    fn new() -> Result<Self> {
        unsafe {
            let mut token = std::ptr::null_mut();
            check(OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_QUERY,
                &mut token,
            ))?;
            let mut needed = 0;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
            // u64 supplies alignment for TOKEN_USER and the SID that follows it.
            let mut storage = vec![0u64; (needed as usize).div_ceil(8)];
            let ok = GetTokenInformation(
                token,
                TokenUser,
                storage.as_mut_ptr().cast(),
                needed,
                &mut needed,
            );
            let error = if ok == 0 {
                Some(std::io::Error::last_os_error())
            } else {
                None
            };
            CloseHandle(token);
            if let Some(error) = error {
                return Err(error.into());
            }
            let user = &*(storage.as_ptr().cast::<TOKEN_USER>());
            let mut sid = std::ptr::null_mut();
            check(ConvertSidToStringSidW(user.User.Sid, &mut sid))?;
            let mut len = 0;
            while *sid.add(len) != 0 {
                len += 1;
            }
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(sid, len));
            LocalFree(sid.cast());
            let sddl = wide(&format!("D:P(A;;GA;;;{text})"));
            let mut descriptor = std::ptr::null_mut();
            check(ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            ))?;
            Ok(Self(descriptor))
        }
    }
    fn server(&self, name: &str) -> Result<NamedPipeServer> {
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        };
        Ok(unsafe {
            ServerOptions::new()
                .first_pipe_instance(true)
                .max_instances(1)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(
                    name,
                    (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
                )?
        })
    }
}
impl Drop for PipeSecurity {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

struct Console {
    input: HANDLE,
    output: HANDLE,
    input_mode: u32,
    output_mode: u32,
    input_cp: u32,
    output_cp: u32,
}
impl Console {
    fn enter() -> Result<Self> {
        unsafe {
            let input = GetStdHandle(STD_INPUT_HANDLE);
            let output = GetStdHandle(STD_OUTPUT_HANDLE);
            ensure!(
                !input.is_null()
                    && input != INVALID_HANDLE_VALUE
                    && !output.is_null()
                    && output != INVALID_HANDLE_VALUE,
                "Built-in SSH needs an interactive console; use ssh --system for pipelines"
            );
            let (mut input_mode, mut output_mode) = (0, 0);
            check(GetConsoleMode(input, &mut input_mode))
                .context("Built-in SSH needs console input")?;
            check(GetConsoleMode(output, &mut output_mode))?;
            let console = Self {
                input,
                output,
                input_mode,
                output_mode,
                input_cp: GetConsoleCP(),
                output_cp: GetConsoleOutputCP(),
            };
            check(SetConsoleCP(65001))?;
            check(SetConsoleOutputCP(65001))?;
            check(SetConsoleMode(
                input,
                (input_mode
                    & !(ENABLE_ECHO_INPUT
                        | ENABLE_LINE_INPUT
                        | ENABLE_PROCESSED_INPUT
                        | ENABLE_QUICK_EDIT_MODE))
                    | ENABLE_EXTENDED_FLAGS
                    | ENABLE_VIRTUAL_TERMINAL_INPUT,
            ))?;
            check(SetConsoleMode(
                output,
                output_mode
                    | ENABLE_PROCESSED_OUTPUT
                    | ENABLE_VIRTUAL_TERMINAL_PROCESSING
                    | DISABLE_NEWLINE_AUTO_RETURN,
            ))?;
            Ok(console)
        }
    }
    fn size(&self) -> Result<(u32, u32)> {
        let mut info = unsafe { std::mem::zeroed::<CONSOLE_SCREEN_BUFFER_INFO>() };
        check(unsafe { GetConsoleScreenBufferInfo(self.output, &mut info) })?;
        Ok((
            (info.srWindow.Right - info.srWindow.Left + 1).max(1) as u32,
            (info.srWindow.Bottom - info.srWindow.Top + 1).max(1) as u32,
        ))
    }
}
impl Drop for Console {
    fn drop(&mut self) {
        unsafe {
            SetConsoleMode(self.input, self.input_mode);
            SetConsoleMode(self.output, self.output_mode);
            SetConsoleCP(self.input_cp);
            SetConsoleOutputCP(self.output_cp);
        }
    }
}

fn output(bytes: &[u8]) -> Result<()> {
    let mut offset = 0;
    while offset < bytes.len() {
        let mut written = 0;
        check(unsafe {
            WriteFile(
                GetStdHandle(STD_OUTPUT_HANDLE),
                bytes[offset..].as_ptr(),
                (bytes.len() - offset) as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        })?;
        ensure!(written > 0, "Console output closed");
        offset += written as usize;
    }
    Ok(())
}
fn input(bytes: &mut [u8]) -> Result<usize> {
    let mut count = 0;
    check(unsafe {
        ReadFile(
            GetStdHandle(STD_INPUT_HANDLE),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            &mut count,
            std::ptr::null_mut(),
        )
    })?;
    Ok(count as usize)
}

struct ConsolePrompt;
impl Prompt for ConsolePrompt {
    fn ask(&self, message: &str, secret: bool) -> Result<String> {
        // Authentication text is supplied by a server. Keep it printable rather
        // than permitting terminal escapes to disguise a host-key question.
        let message: String = message
            .chars()
            .filter(|c| !c.is_control() || matches!(c, '\r' | '\n' | '\t'))
            .collect();
        output(message.as_bytes())?;
        let mut answer = zeroize::Zeroizing::new(Vec::new());
        loop {
            let mut byte = [0];
            ensure!(input(&mut byte)? == 1, "Console input closed");
            match byte[0] {
                3 | 4 | 27 => {
                    output(b"\r\n")?;
                    bail!("SSH authentication cancelled");
                }
                b'\r' | b'\n' => {
                    output(b"\r\n")?;
                    return String::from_utf8(answer.to_vec()).context("Invalid UTF-8 input");
                }
                8 | 127 => {
                    if !answer.is_empty() {
                        while answer.pop().is_some_and(|b| b & 0xc0 == 0x80) {}
                        if !secret {
                            output(b"\x08 \x08")?;
                        }
                    }
                }
                b if b >= 32 => {
                    ensure!(answer.len() < 16384, "Authentication response is too long");
                    answer.push(b);
                    if !secret {
                        output(&byte)?;
                    }
                }
                _ => {}
            }
        }
    }
}

pub fn run(real_ssh: &Path, directory: &Path, args: Vec<OsString>) -> Result<i32> {
    let config = config::resolve(real_ssh, &args)?;
    let console = Console::enter()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let handle = Arc::new(super::connect(config, Arc::new(ConsolePrompt)).await?);
        let mut size = console.size()?;
        let mut channel = super::shell(&handle, size.0, size.1).await?;
        let name = super::pipe_name(directory, std::process::id())?;
        let security = PipeSecurity::new()?;
        let mut pipe = security.server(&name)?;
        drop(security);
        let connection = handle.clone();
        let uploads = tokio::spawn(async move {
            loop {
                if pipe.connect().await.is_err() { break; }
                // Bound malformed clients as well as SFTP. A failed upload
                // leaves the shell and future uploads usable.
                let _ = tokio::time::timeout(Duration::from_secs(95), transfer::serve_one(&mut pipe, &connection)).await;
                if pipe.disconnect().is_err() { break; }
            }
        });
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        // A normal thread avoids Tokio runtime shutdown waiting for console
        // input after the remote shell exits. The shim exits immediately after
        // restoring console state, which also terminates this reader.
        std::thread::spawn(move || {
            loop {
                let mut bytes = vec![0; 16384];
                let result = input(&mut bytes).map(|count| { bytes.truncate(count); bytes });
                let ended = result.as_ref().map_or(true, |bytes| bytes.is_empty());
                if tx.blocking_send(result).is_err() || ended { break; }
            }
        });
        let mut resize = tokio::time::interval(Duration::from_millis(100));
        let mut exit_status = 255;
        let mut input_open = true;
        let result = async {
            loop {
                tokio::select! {
                    bytes = rx.recv(), if input_open => {
                        match bytes {
                            Some(Ok(bytes)) if !bytes.is_empty() => channel.data(&bytes[..]).await?,
                            Some(Err(error)) => return Err(error),
                            _ => { input_open = false; channel.eof().await?; }
                        }
                    }
                    message = channel.wait() => {
                        match message {
                            Some(russh::ChannelMsg::Data { data }) | Some(russh::ChannelMsg::ExtendedData { data, .. }) => output(&data)?,
                            Some(russh::ChannelMsg::ExitStatus { exit_status: status }) => exit_status = status as i32,
                            Some(russh::ChannelMsg::ExitSignal { .. }) => exit_status = 128,
                            Some(russh::ChannelMsg::Close) | None => break,
                            _ => {},
                        }
                    }
                    _ = resize.tick() => {
                        let next = console.size()?;
                        if next != size { channel.window_change(next.0, next.1, 0, 0).await?; size = next; }
                    }
                }
            }
            Ok(exit_status)
        }.await;
        uploads.abort();
        let _ = uploads.await;
        let _ = tokio::time::timeout(Duration::from_secs(2), handle.disconnect(russh::Disconnect::ByApplication, "Session closed", "en")).await;
        result
    });
    runtime.shutdown_timeout(Duration::from_secs(1));
    drop(console);
    result
}

pub fn upload(path: &Path, pipe_name: &str, expected_pid: u32) -> Result<String> {
    let metadata = std::fs::metadata(path)?;
    ensure!(
        metadata.len() <= transfer::MAX_IMAGE_BYTES as u64,
        "Image exceeds 50 MiB"
    );
    let bytes = std::fs::read(path)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(90), async {
            // A completed request may still be releasing its SFTP channel.
            // Retry only a busy pipe; a missing pipe means login is not ready.
            let mut pipe =
                loop {
                    match ClientOptions::new().open(pipe_name) {
                        Ok(pipe) => break pipe,
                        Err(error) if error.raw_os_error() == Some(231) => {
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                        Err(error) => return Err(error).context(
                            "SSH upload connection is unavailable; finish logging in or reconnect",
                        ),
                    }
                };
            use std::os::windows::io::AsRawHandle;
            let mut pid = 0;
            check(unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle().cast(), &mut pid) })?;
            ensure!(pid == expected_pid, "SSH session changed before upload");
            transfer::request(&mut pipe, &bytes).await
        })
        .await
        .context("Image upload timed out; path was not pasted")?
    })
}
