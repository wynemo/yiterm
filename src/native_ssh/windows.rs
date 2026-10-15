use super::{auth::Prompt, config, transfer};
use anyhow::{Context, Result, bail, ensure};
use std::{
    ffi::OsString,
    path::Path,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};
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
    fn server(&self, name: &str, first_pipe_instance: bool) -> Result<NamedPipeServer> {
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        };
        Ok(unsafe {
            ServerOptions::new()
                .first_pipe_instance(first_pipe_instance)
                .max_instances(2)
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

const MAX_OSC52_FRAME_BYTES: usize = 2 * 1024 * 1024;

pub struct ClipboardRequests {
    receiver: mpsc::Receiver<Vec<u8>>,
    shutdown: tokio::sync::watch::Sender<bool>,
    _thread: thread::JoinHandle<()>,
}

impl ClipboardRequests {
    pub fn listen(pipe_name: String) -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(16);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let (shutdown, shutdown_receiver) = tokio::sync::watch::channel(false);
        let worker_ready = ready_sender.clone();
        let worker = thread::Builder::new()
            .name("yiterm-osc52-pipe".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = worker_ready.send(Err(error.to_string()));
                        tracing::error!(%error, "Could not start OSC 52 pipe runtime");
                        return;
                    }
                };
                if let Err(error) = runtime.block_on(serve_clipboard_requests(
                    pipe_name,
                    sender,
                    ready_sender,
                    shutdown_receiver,
                )) {
                    tracing::error!(error = %format!("{error:#}"), "OSC 52 pipe listener stopped");
                }
            })?;

        match ready_receiver.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                receiver,
                shutdown,
                _thread: worker,
            }),
            Ok(Err(error)) => anyhow::bail!("starting OSC 52 pipe listener: {error}"),
            Err(error) => anyhow::bail!("waiting for OSC 52 pipe listener: {error}"),
        }
    }

    pub fn try_recv(&self) -> std::result::Result<Vec<u8>, mpsc::TryRecvError> {
        self.receiver.try_recv()
    }
}

impl Drop for ClipboardRequests {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

async fn serve_clipboard_requests(
    pipe_name: String,
    sender: mpsc::SyncSender<Vec<u8>>,
    ready: mpsc::SyncSender<std::result::Result<(), String>>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let security = PipeSecurity::new()?;
    let mut pipe = security.server(&pipe_name, true)?;
    drop(security);
    let _ = ready.send(Ok(()));
    tracing::info!("OSC 52 named pipe listener ready");

    loop {
        tokio::select! {
            result = pipe.connect() => result.context("accepting OSC 52 pipe client")?,
            _ = shutdown.changed() => return Ok(()),
        }

        // Arm the next instance before acknowledging this request so a burst
        // of OSC 52 writes can reconnect without racing pipe creation.
        let security = PipeSecurity::new()?;
        let next_pipe = security.server(&pipe_name, false)?;
        drop(security);

        let request = tokio::select! {
            result = read_osc52_request(&mut pipe) => result,
            _ = shutdown.changed() => return Ok(()),
        };
        match request {
            Ok(bytes) => {
                let accepted = sender.send(bytes).is_ok();
                pipe.write_u8(u8::from(!accepted)).await?;
                pipe.flush().await?;
                if !accepted {
                    return Ok(());
                }
            }
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "Rejected OSC 52 pipe request");
                pipe.write_u8(1).await?;
                pipe.flush().await?;
            }
        }
        pipe = next_pipe;
    }
}

async fn read_osc52_request(pipe: &mut NamedPipeServer) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;

    let size = pipe.read_u32_le().await? as usize;
    ensure!(
        (6..=MAX_OSC52_FRAME_BYTES).contains(&size),
        "Invalid OSC 52 pipe frame size"
    );
    let mut bytes = vec![0; size];
    pipe.read_exact(&mut bytes).await?;
    ensure!(
        bytes.starts_with(b"\x1b]52;"),
        "Pipe frame is not an OSC 52 sequence"
    );
    ensure!(
        bytes.ends_with(b"\x07") || bytes.ends_with(b"\x1b\\") || bytes.ends_with(&[0x9c]),
        "OSC 52 sequence has no string terminator"
    );
    Ok(bytes)
}

async fn forward_osc52_request(pipe_name: &str, bytes: &[u8]) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut pipe = loop {
        match ClientOptions::new().open(pipe_name) {
            Ok(pipe) => break pipe,
            Err(error)
                if matches!(error.raw_os_error(), Some(2 | 231)) && Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => return Err(error).context("connecting to YiTerm OSC 52 pipe"),
        }
    };

    tokio::time::timeout(Duration::from_secs(5), async {
        pipe.write_u32_le(bytes.len() as u32).await?;
        pipe.write_all(bytes).await?;
        pipe.flush().await?;
        match pipe.read_u8().await? {
            0 => Ok(()),
            status => anyhow::bail!("YiTerm rejected OSC 52 pipe request with status {status}"),
        }
    })
    .await
    .context("waiting for YiTerm OSC 52 pipe response")?
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
                        | ENABLE_QUICK_EDIT_MODE
                        | ENABLE_VIRTUAL_TERMINAL_INPUT))
                    | ENABLE_EXTENDED_FLAGS,
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

    fn enable_terminal_input(&self) -> Result<()> {
        // Keep VT input disabled while reading passwords: terminal mouse reports
        // begin with ESC and must not be mistaken for prompt cancellation. Once
        // authentication is complete, translate special keys (arrows, Home,
        // function keys) into VT sequences for the remote terminal.
        let mut mode = 0;
        check(unsafe { GetConsoleMode(self.input, &mut mode) })?;
        check(unsafe { SetConsoleMode(self.input, mode | ENABLE_VIRTUAL_TERMINAL_INPUT) })
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
    let _logging = match crate::logging::init(None, None) {
        Ok(logging) => {
            if let Some(warning) = &logging.warning {
                eprintln!("SSH diagnostic logging: {warning}");
            }
            Some(logging)
        }
        Err(error) => {
            eprintln!("SSH diagnostic logging unavailable: {error:#}");
            None
        }
    };
    tracing::info!("Built-in SSH wrapper started");
    let result = run_inner(real_ssh, directory, args);
    if let Err(error) = &result {
        tracing::error!(error = %format!("{error:#}"), "Built-in SSH session failed");
    }
    result
}

fn run_inner(real_ssh: &Path, directory: &Path, args: Vec<OsString>) -> Result<i32> {
    let config = config::resolve(real_ssh, &args)?;
    let osc52_pipe = super::osc52_pipe_name(directory)?;
    let console = Console::enter()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let prompt: Arc<dyn Prompt> = Arc::new(ConsolePrompt);
        let handle = Arc::new(if config.proxy_jump.is_some() {
            super::connect_via_proxyjump(config, prompt, real_ssh).await?
        } else {
            super::connect(config, prompt).await?
        });
        console.enable_terminal_input()?;
        let mut size = console.size()?;
        let mut channel = super::shell(&handle, size.0, size.1).await?;
        let name = super::pipe_name(directory, std::process::id())?;
        let security = PipeSecurity::new()?;
        let mut pipe = security.server(&name, true)?;
        drop(security);
        let connection = handle.clone();
        let uploads = tokio::spawn(async move {
            let mut pending_connection = tokio::spawn(async move {
                pipe.connect().await?;
                Ok::<_, std::io::Error>(pipe)
            });
            loop {
                let mut connected = match pending_connection.await {
                    Ok(Ok(pipe)) => pipe,
                    Ok(Err(error)) => {
                        tracing::error!(error = %error, "SSH upload pipe accept failed");
                        break;
                    }
                    Err(error) => {
                        tracing::error!(error = %error, "SSH upload pipe accept task failed");
                        break;
                    }
                };
                // Start waiting on the next instance before processing this
                // upload. Merely creating it is insufficient: Windows does not
                // accept clients until ConnectNamedPipe is pending.
                let next_connection = match PipeSecurity::new()
                    .and_then(|security| security.server(&name, false))
                {
                    Ok(mut next_pipe) => Some(tokio::spawn(async move {
                        next_pipe.connect().await?;
                        Ok::<_, std::io::Error>(next_pipe)
                    })),
                    Err(error) => {
                        tracing::error!(error = %error, "Could not rearm SSH upload pipe");
                        None
                    }
                };
                // Bound malformed clients as well as SFTP. A failed upload
                // leaves the shell and future uploads usable.
                let started = std::time::Instant::now();
                match tokio::time::timeout(
                    Duration::from_secs(95),
                    transfer::serve_one(&mut connected, &connection),
                )
                .await
                {
                    Ok(Ok(())) => tracing::debug!(
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "SSH upload pipe request completed"
                    ),
                    Ok(Err(error)) => tracing::warn!(
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        error = %format!("{error:#}"),
                        "SSH upload pipe request failed"
                    ),
                    Err(error) => tracing::warn!(
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        error = %error,
                        "SSH upload pipe request timed out"
                    ),
                }
                drop(connected);
                let Some(next_connection) = next_connection else {
                    break;
                };
                pending_connection = next_connection;
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
        let mut shell_reply_pending = true;
        let mut osc52_filter = super::osc52::Filter::default();
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
                            Some(message) => {
                                if super::shell_request_reply(&message, &mut shell_reply_pending)? {
                                    continue;
                                }
                                match message {
                                    russh::ChannelMsg::Data { data } | russh::ChannelMsg::ExtendedData { data, .. } => {
                                        let (terminal_bytes, requests) = osc52_filter.advance(&data);
                                        for request in requests {
                                            match forward_osc52_request(&osc52_pipe, &request).await {
                                                Ok(()) => tracing::info!(bytes = request.len(), "Forwarded OSC 52 request to YiTerm"),
                                                Err(error) => {
                                                    tracing::warn!(bytes = request.len(), error = %format!("{error:#}"), "Could not forward OSC 52 request to YiTerm");
                                                    output(&request)?;
                                                }
                                            }
                                        }
                                        output(&terminal_bytes)?;
                                    }
                                    russh::ChannelMsg::ExitStatus { exit_status: status } => exit_status = status as i32,
                                    russh::ChannelMsg::ExitSignal { .. } => exit_status = 128,
                                    russh::ChannelMsg::Close if shell_reply_pending => bail!("SSH channel closed before the shell request was accepted"),
                                    russh::ChannelMsg::Close => {
                                        output(&osc52_filter.finish())?;
                                        break;
                                    }
                                    _ => {},
                                }
                            }
                            None if shell_reply_pending => bail!("SSH channel closed before the shell request was accepted"),
                            None => {
                                output(&osc52_filter.finish())?;
                                break;
                            }
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
    let started = std::time::Instant::now();
    let result = upload_inner(path, pipe_name, expected_pid);
    match &result {
        Ok(_) => tracing::info!(
            expected_pid,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "Native SSH image upload completed"
        ),
        Err(error) => tracing::warn!(
            expected_pid,
            elapsed_ms = started.elapsed().as_millis() as u64,
            error = %format!("{error:#}"),
            "Native SSH image upload failed"
        ),
    }
    result
}

fn upload_inner(path: &Path, pipe_name: &str, expected_pid: u32) -> Result<String> {
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
            transfer::request(&mut pipe, &bytes)
                .await
                .context("SSH upload pipe protocol failed")
        })
        .await
        .context("Image upload timed out; path was not pasted")?
    })
}
