use anyhow::{Context, Result, bail};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

pub fn quote_posix(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn quote_powershell(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

pub fn save_image(image: arboard::ImageData<'_>) -> Result<PathBuf> {
    let width = u32::try_from(image.width)?;
    let height = u32::try_from(image.height)?;
    let buffer = image::RgbaImage::from_raw(width, height, image.bytes.into_owned())
        .context("Invalid clipboard image dimensions")?;
    save_png(image::DynamicImage::ImageRgba8(buffer))
}

/// Invalid or oversized image text remains ordinary clipboard text.
/// Once an image is recognized, file I/O failures must reach the caller.
pub fn save_base64_png(text: &str) -> Result<Option<PathBuf>> {
    const MAX_TEXT_BYTES: usize = 72 * 1024 * 1024;
    const MAX_PNG_BYTES: usize = 50 * 1024 * 1024;
    const MAX_BASE64_BYTES: usize = MAX_PNG_BYTES.div_ceil(3) * 4;

    if text.len() > MAX_TEXT_BYTES {
        return Ok(None);
    }
    let text = text.trim();
    let payload = text.strip_prefix("data:image/png;base64,").unwrap_or(text);
    let bytes = payload.bytes().filter(|byte| !byte.is_ascii_whitespace());
    // A PNG signature always starts with these Base64 characters.
    if !bytes.clone().take(11).eq(b"iVBORw0KGgo".iter().copied()) {
        return Ok(None);
    }
    let encoded: Vec<u8> = bytes.take(MAX_BASE64_BYTES + 1).collect();
    if encoded.len() > MAX_BASE64_BYTES {
        return Ok(None);
    }
    let Ok(png) = STANDARD
        .decode(&encoded)
        .or_else(|_| STANDARD_NO_PAD.decode(&encoded))
    else {
        return Ok(None);
    };
    if png.len() > MAX_PNG_BYTES || !png.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Ok(None);
    }
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(png), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let Ok(image) = reader.decode() else {
        return Ok(None);
    };
    save_png(image).map(Some)
}

fn save_png(image: image::DynamicImage) -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("yiterm-drop-{}.png", uuid::Uuid::new_v4()));
    // Create exclusively; on Unix keep clipboard contents private.
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path)?;
    image.write_to(&mut std::io::BufWriter::new(file), image::ImageFormat::Png)?;
    Ok(path)
}

pub fn upload(path: &Path, remote: &crate::ssh::Remote, scp: &Path) -> Result<String> {
    let name = path
        .file_name()
        .context("Missing filename")?
        .to_str()
        .context("Non-UTF8 filename")?;
    let destination = format!("/tmp/{name}");
    let mut command = Command::new(scp);
    command.args([
        "-o",
        "BatchMode=yes",
        "-o",
        "ControlMaster=no",
        "-o",
        "ConnectTimeout=8",
        "-o",
        "ServerAliveInterval=5",
        "-o",
        "ServerAliveCountMax=2",
        "-o",
        "ClearAllForwardings=yes",
        "-o",
        "ForwardAgent=no",
        "-o",
        "ForwardX11=no",
        "-o",
        "PermitLocalCommand=no",
        "-o",
        "RemoteCommand=none",
    ]);
    command
        .args(&remote.options)
        .arg("--")
        .arg(path)
        .arg(format!("{}:{destination}", remote.scp_destination()));
    let started = Instant::now();
    tracing::info!("Image upload started");
    let output = run_transfer(command, Duration::from_secs(60))?;
    tracing::info!(
        success = output.0.success(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "Image upload completed"
    );
    if !output.0.success() {
        bail!(
            "Image upload failed: {}",
            String::from_utf8_lossy(&output.1).trim()
        );
    }
    Ok(quote_posix(&destination))
}

pub fn upload_detected(
    path: &Path,
    detected: &crate::ssh::Detection,
    scp: &Path,
) -> Result<String> {
    #[cfg(windows)]
    if let Some(pipe) = &detected.upload_pipe {
        return crate::native_ssh::upload(path, pipe, detected.identity.pid);
    }
    upload(path, &detected.remote, scp)
}

// A stalled transfer must never leave the terminal permanently busy.
fn run_transfer(
    mut command: Command,
    timeout: Duration,
) -> Result<(std::process::ExitStatus, Vec<u8>)> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Background scp (and its ssh child) must not allocate a console window.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .context("Could not start scp (install OpenSSH)")?;
    let mut stderr = child.stderr.take().context("Missing scp error pipe")?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            match stderr.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let keep = n.min(65536usize.saturating_sub(output.len()));
                    output.extend_from_slice(&buffer[..keep]);
                }
            }
        }
        let _ = tx.send(output);
    });
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            let output = rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
            return Ok((status, output));
        }
        if Instant::now() >= deadline {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            bail!("Image upload timed out; path was not pasted");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
