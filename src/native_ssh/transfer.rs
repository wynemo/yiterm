use super::{connection::Connection, expect_success};
use anyhow::{Context, Result, ensure};
use russh_sftp::{
    client::SftpSession,
    protocol::{FileAttributes, OpenFlags},
};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_IMAGE_BYTES: usize = 50 * 1024 * 1024;
pub const TRANSFER_TIMEOUT: Duration = Duration::from_secs(60);
const MAGIC: &[u8; 8] = b"YITIMG01";
const PNG: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

pub async fn request<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    bytes: &[u8],
) -> Result<String> {
    ensure!(
        bytes.starts_with(PNG) && bytes.len() <= MAX_IMAGE_BYTES,
        "Invalid PNG or image exceeds 50 MiB"
    );
    stream
        .write_all(MAGIC)
        .await
        .context("writing upload protocol header")?;
    stream
        .write_u64(bytes.len() as u64)
        .await
        .context("writing upload image size")?;
    stream
        .write_all(bytes)
        .await
        .context("writing upload image data")?;
    stream.flush().await.context("flushing upload request")?;
    let status = stream
        .read_u8()
        .await
        .context("reading upload response status")?;
    let size = stream
        .read_u32()
        .await
        .context("reading upload response size")? as usize;
    ensure!(size <= 8192, "Invalid upload response");
    let mut message = vec![0; size];
    stream
        .read_exact(&mut message)
        .await
        .context("reading upload response body")?;
    let message = String::from_utf8(message)?;
    ensure!(status == 0, "Image upload failed: {message}");
    ensure!(valid_remote_path(&message), "Invalid uploaded image path");
    Ok(crate::paste::quote_posix(&message))
}

fn valid_remote_path(path: &str) -> bool {
    path.strip_prefix("/tmp/yiterm-drop-")
        .and_then(|s| s.strip_suffix(".png"))
        .is_some_and(|s| uuid::Uuid::parse_str(s).is_ok())
}

async fn receive<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Vec<u8>> {
    let mut magic = [0; 8];
    stream.read_exact(&mut magic).await?;
    ensure!(&magic == MAGIC, "Unknown image upload protocol");
    let size = stream.read_u64().await?;
    ensure!(
        (8..=MAX_IMAGE_BYTES as u64).contains(&size),
        "Image exceeds 50 MiB or is empty"
    );
    let mut bytes = vec![0; size as usize];
    stream.read_exact(&mut bytes).await?;
    ensure!(
        bytes.starts_with(PNG),
        "Only PNG clipboard images are accepted"
    );
    Ok(bytes)
}

async fn respond<S: AsyncWrite + Unpin>(stream: &mut S, result: &Result<String>) -> Result<()> {
    let (status, mut text) = match result {
        Ok(path) => (0, path.clone()),
        Err(error) => (1, format!("{error:#}")),
    };
    // Keep the framing bounded even for an unexpectedly verbose server error.
    while text.len() > 8192 {
        text.pop();
    }
    stream.write_u8(status).await?;
    stream.write_u32(text.len() as u32).await?;
    stream.write_all(text.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

async fn sftp(handle: &Connection) -> Result<SftpSession> {
    let mut channel = handle.open_session().await?;
    channel.request_subsystem(true, "sftp").await?;
    expect_success(&mut channel)
        .await
        .context("Server does not support SFTP")?;
    let sftp = SftpSession::new(channel.into_stream()).await?;
    Ok(sftp)
}

async fn write_image(
    sftp: &SftpSession,
    path: &str,
    bytes: &[u8],
    created: &mut bool,
) -> Result<()> {
    let mut file = sftp
        .open_with_flags_and_attributes(
            path,
            OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
            FileAttributes {
                permissions: Some(0o600),
                ..FileAttributes::empty()
            },
        )
        .await?;
    *created = true;
    file.write_all(bytes).await?;
    // shutdown sends CLOSE and waits for its status, so a failed final write
    // can never result in pasting a path to an incomplete file.
    file.shutdown().await?;
    Ok(())
}

pub async fn serve_one<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    handle: &Connection,
) -> Result<()> {
    let bytes = match tokio::time::timeout(Duration::from_secs(15), receive(stream)).await {
        Ok(Ok(bytes)) => bytes,
        result => {
            let error = match result {
                Ok(Err(e)) => e,
                Err(e) => e.into(),
                _ => unreachable!(),
            };
            let _ = respond(stream, &Err(error)).await;
            return Ok(());
        }
    };
    let sftp = match tokio::time::timeout(Duration::from_secs(15), sftp(handle)).await {
        Ok(Ok(sftp)) => sftp,
        result => {
            let error = match result {
                Ok(Err(e)) => e,
                Err(e) => e.into(),
                _ => unreachable!(),
            };
            respond(stream, &Err(error)).await?;
            return Ok(());
        }
    };
    let path = format!("/tmp/yiterm-drop-{}.png", uuid::Uuid::new_v4());
    let mut cancelled = [0; 1];
    let mut created = false;
    let result = tokio::select! {
        result = tokio::time::timeout(TRANSFER_TIMEOUT, write_image(&sftp, &path, &bytes, &mut created)) => {
            match result { Ok(result) => result.map(|_| path.clone()), Err(_) => Err(anyhow::anyhow!("Image upload timed out")) }
        }
        // No further client bytes are valid until the reply. EOF cancels the
        // transfer when the pane/window is closed or the GUI request times out.
        _ = stream.read(&mut cancelled) => Err(anyhow::anyhow!("Image upload cancelled")),
    };
    let response = tokio::time::timeout(Duration::from_secs(5), respond(stream, &result)).await;
    if created && (result.is_err() || !matches!(response, Ok(Ok(())))) {
        let _ = tokio::time::timeout(Duration::from_secs(5), sftp.remove_file(&path)).await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), sftp.close()).await;
    result?;
    response??;
    Ok(())
}
