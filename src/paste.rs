use anyhow::{Context, Result, bail};
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
    image::DynamicImage::ImageRgba8(buffer)
        .write_to(&mut std::io::BufWriter::new(file), image::ImageFormat::Png)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shell_paths_are_single_arguments() {
        assert_eq!(quote_posix("/tmp/a'b $(x).png"), "'/tmp/a'\\''b $(x).png'");
        assert_eq!(quote_powershell("C:\\a'b.png"), "'C:\\a''b.png'");
    }
    #[test]
    fn stalled_upload_has_a_deadline() {
        let mut command = if cfg!(windows) {
            let mut command = Command::new("powershell.exe");
            command.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"]);
            command
        } else {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "exec sleep 30"]);
            command
        };
        command.env("TERM", "xterm");
        let start = Instant::now();
        let result = run_transfer(command, Duration::from_millis(100));
        assert!(result.unwrap_err().to_string().contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(5));
    }
    #[cfg(unix)]
    #[test]
    fn upload_uses_detected_connection_and_never_returns_a_path_on_failure() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let scp = dir.path().join("scp");
        let log = dir.path().join("arguments");
        std::fs::write(
            &scp,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n",
                quote_posix(&log.to_string_lossy())
            ),
        )
        .unwrap();
        std::fs::set_permissions(&scp, std::fs::Permissions::from_mode(0o700)).unwrap();
        let remote =
            crate::ssh::parse(&["-p2222".into(), "-S/socket".into(), "alice@host".into()]).unwrap();
        let result = upload(&dir.path().join("yiterm-drop-test.png"), &remote, &scp).unwrap();
        assert_eq!(result, "'/tmp/yiterm-drop-test.png'");
        let arguments = std::fs::read_to_string(log).unwrap();
        assert!(arguments.contains("-P\n2222\n"));
        assert!(arguments.contains("ControlPath=/socket\n"));
        assert!(arguments.contains("alice@host:/tmp/yiterm-drop-test.png"));
        std::fs::write(&scp, "#!/bin/sh\necho upload-denied >&2\nexit 1\n").unwrap();
        assert!(
            upload(&dir.path().join("image.png"), &remote, &scp)
                .unwrap_err()
                .to_string()
                .contains("upload-denied")
        );
    }
    #[test]
    fn clipboard_image_roundtrips() {
        let path = save_image(arboard::ImageData {
            width: 1,
            height: 1,
            bytes: std::borrow::Cow::Owned(vec![255, 0, 0, 255]),
        })
        .unwrap();
        assert_eq!(
            image::open(&path).unwrap().to_rgba8().into_raw(),
            vec![255, 0, 0, 255]
        );
        std::fs::remove_file(path).unwrap();
    }
}
