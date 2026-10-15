//! The Windows PATH shim owns one authenticated SSH connection. Its terminal and
//! image transfers use independent channels; credentials never cross GUI IPC.
pub mod auth;
pub mod config;
pub mod transfer;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{run, upload};

use anyhow::{Context, Result, bail};
use auth::{Handler, Prompt};
use config::Config;
use russh::{Channel, ChannelMsg, client};
use std::{
    ffi::OsString,
    io,
    path::Path,
    pin::Pin,
    process::Stdio,
    sync::Arc,
    task::{Context as TaskContext, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub async fn connect(config: Config, prompt: Arc<dyn Prompt>) -> Result<client::Handle<Handler>> {
    let socket = tokio::time::timeout(
        config.timeout,
        tokio::net::TcpStream::connect((config.host.as_str(), config.port)),
    )
    .await
    .context("SSH connection timed out")??;
    connect_stream(config, prompt, socket, None).await
}

pub async fn connect_via_proxyjump(
    config: Config,
    prompt: Arc<dyn Prompt>,
    real_ssh: &Path,
) -> Result<client::Handle<Handler>> {
    let proxy_jump = config
        .proxy_jump
        .as_deref()
        .context("Missing ProxyJump configuration")?;
    let mut command = tokio::process::Command::new(real_ssh);
    command
        .args(proxyjump_args(&config.host, config.port, proxy_jump))
        .env_remove("YITERM_REAL_SSH")
        .env_remove("YITERM_SSH_DIRECTORY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .context("Could not start OpenSSH ProxyJump")?;
    let stdin = child.stdin.take().context("OpenSSH stdin was not piped")?;
    let stdout = child
        .stdout
        .take()
        .context("OpenSSH stdout was not piped")?;
    connect_stream(config, prompt, OpenSshStream { stdout, stdin }, Some(child)).await
}

fn proxyjump_args(host: &str, port: u16, proxy_jump: &str) -> Vec<OsString> {
    let hops: Vec<_> = proxy_jump.split(',').map(str::trim).collect();
    let destination = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let mut args = vec![OsString::from("-W"), OsString::from(destination)];
    if hops.len() > 1 {
        args.extend([
            OsString::from("-J"),
            OsString::from(hops[..hops.len() - 1].join(",")),
        ]);
    }
    if let Some(last_hop) = hops.last() {
        args.push(OsString::from(last_hop));
    }
    args
}

async fn connect_stream<S>(
    config: Config,
    prompt: Arc<dyn Prompt>,
    stream: S,
    proxy_process: Option<tokio::process::Child>,
) -> Result<client::Handle<Handler>>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let options = Arc::new(client::Config {
        keepalive_interval: config.keepalive,
        keepalive_max: config.keepalive_max,
        ..Default::default()
    });
    let handler = Handler {
        config: config.clone(),
        prompt: prompt.clone(),
        _proxy_process: proxy_process,
    };
    let interactive_timeout = std::time::Duration::from_secs(120);
    let mut handle = tokio::time::timeout(
        interactive_timeout,
        client::connect_stream(options, stream, handler),
    )
    .await
    .context("SSH handshake or host-key confirmation timed out")??;
    tokio::time::timeout(
        interactive_timeout,
        auth::authenticate(&mut handle, &config, &prompt),
    )
    .await
    .context("SSH authentication timed out")??;
    Ok(handle)
}

struct OpenSshStream {
    stdout: tokio::process::ChildStdout,
    stdin: tokio::process::ChildStdin,
}

impl AsyncRead for OpenSshStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stdout).poll_read(cx, buffer)
    }
}

impl AsyncWrite for OpenSshStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().stdin).poll_write(cx, buffer)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stdin).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stdin).poll_shutdown(cx)
    }
}

pub async fn shell(
    handle: &client::Handle<Handler>,
    cols: u32,
    rows: u32,
) -> Result<Channel<client::Msg>> {
    let mut channel = handle.channel_open_session().await?;
    channel
        .request_pty(true, "xterm-256color", cols, rows, 0, 0, &[])
        .await?;
    expect_success(&mut channel)
        .await
        .context("Server rejected the terminal")?;
    channel.request_shell(true).await?;
    Ok(channel)
}

async fn expect_success(channel: &mut Channel<client::Msg>) -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Success) => return Ok(()),
                Some(ChannelMsg::Failure) => bail!("SSH channel request failed"),
                Some(ChannelMsg::WindowAdjusted { .. }) => {}
                _ => bail!("SSH channel request failed"),
            }
        }
    })
    .await?
}

#[cfg(any(windows, test))]
fn shell_request_reply(message: &ChannelMsg, pending: &mut bool) -> Result<bool> {
    if !*pending {
        return Ok(false);
    }
    match message {
        ChannelMsg::Success => {
            *pending = false;
            Ok(true)
        }
        ChannelMsg::Failure => {
            *pending = false;
            bail!("Server rejected the shell")
        }
        _ => Ok(false),
    }
}

/// Stable only within this pane's private integration directory and process.
/// A new process receives a fresh pipe, and the client also checks the server PID.
pub fn pipe_name(directory: &std::path::Path, pid: u32) -> Result<String> {
    let name = directory
        .file_name()
        .and_then(|s| s.to_str())
        .context("Invalid SSH integration directory")?;
    anyhow::ensure!(
        name.starts_with("yiterm-") && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "Invalid SSH integration directory"
    );
    Ok(format!(r"\\.\pipe\{name}-ssh-{pid}"))
}

#[cfg(test)]
mod proxyjump_tests {
    use super::proxyjump_args;

    #[test]
    fn proxyjump_opens_stdio_forward_to_target_through_last_hop() {
        let args = proxyjump_args("server.internal", 2222, "alice@jump1,jump2:2200");
        assert_eq!(
            args.iter()
                .map(|arg| arg.to_string_lossy())
                .collect::<Vec<_>>(),
            [
                "-W",
                "server.internal:2222",
                "-J",
                "alice@jump1",
                "jump2:2200"
            ]
        );
    }

    #[test]
    fn proxyjump_brackets_ipv6_targets() {
        let args = proxyjump_args("2001:db8::5", 22, "bastion");
        assert_eq!(
            args.iter()
                .map(|arg| arg.to_string_lossy())
                .collect::<Vec<_>>(),
            ["-W", "[2001:db8::5]:22", "bastion"]
        );
    }
}

#[cfg(test)]
mod shell_tests {
    use super::{ChannelMsg, shell_request_reply};

    #[test]
    fn shell_reply_can_arrive_after_initial_output() {
        let mut pending = true;
        assert!(
            !shell_request_reply(
                &ChannelMsg::Data {
                    data: b"login banner".as_slice().into()
                },
                &mut pending
            )
            .unwrap()
        );
        assert!(pending);
        assert!(shell_request_reply(&ChannelMsg::Success, &mut pending).unwrap());
        assert!(!pending);
    }

    #[test]
    fn shell_failure_is_reported_after_any_initial_output() {
        let mut pending = true;
        assert!(
            !shell_request_reply(
                &ChannelMsg::Data {
                    data: b"login banner".as_slice().into()
                },
                &mut pending
            )
            .unwrap()
        );
        assert!(shell_request_reply(&ChannelMsg::Failure, &mut pending).is_err());
        assert!(!pending);
    }
}
