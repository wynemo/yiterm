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
use std::sync::Arc;

pub async fn connect(config: Config, prompt: Arc<dyn Prompt>) -> Result<client::Handle<Handler>> {
    let options = Arc::new(client::Config {
        keepalive_interval: config.keepalive,
        keepalive_max: config.keepalive_max,
        ..Default::default()
    });
    // TCP has a deadline, but interactive host-key/authentication prompts must
    // not be cut off by ConnectTimeout while the user is reading them.
    let socket = tokio::time::timeout(
        config.timeout,
        tokio::net::TcpStream::connect((config.host.as_str(), config.port)),
    )
    .await
    .context("SSH connection timed out")??;
    let handler = Handler {
        config: config.clone(),
        prompt: prompt.clone(),
    };
    let interactive_timeout = std::time::Duration::from_secs(120);
    let mut handle = tokio::time::timeout(
        interactive_timeout,
        client::connect_stream(options, socket, handler),
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
    expect_success(&mut channel)
        .await
        .context("Server rejected the shell")?;
    Ok(channel)
}

async fn expect_success(channel: &mut Channel<client::Msg>) -> Result<()> {
    match tokio::time::timeout(std::time::Duration::from_secs(15), channel.wait()).await? {
        Some(ChannelMsg::Success) => Ok(()),
        _ => bail!("SSH channel request failed"),
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
