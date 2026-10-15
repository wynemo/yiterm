#![cfg(windows)]

use portable_pty::{CommandBuilder, PtySize};
use std::{io::Read, path::Path, sync::mpsc, time::Duration};

#[test]
fn ssh_shim_returns_output_to_conpty_without_a_new_console() {
    let executable = Path::new(env!("CARGO_BIN_EXE_yiterm"));
    let original = std::fs::read(executable).unwrap();
    let integration = yiterm::ssh::Integration::new(executable).unwrap();
    let pair = portable_pty::native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    // PowerShell must wait for the shim, and OpenSSH's output must return
    // through the original PTY rather than a newly allocated console.
    let mut command = CommandBuilder::new("powershell.exe");
    command.args(["-NoProfile", "-Command", "ssh -V; exit $LASTEXITCODE"]);
    integration.configure(&mut command).unwrap();
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut text = Vec::new();
        let mut buffer = [0; 4096];
        while let Ok(count) = reader.read(&mut buffer) {
            if count == 0 {
                break;
            }
            text.extend_from_slice(&buffer[..count]);
            if String::from_utf8_lossy(&text).contains("OpenSSH") {
                break;
            }
        }
        let _ = sender.send(text);
    });
    let output = receiver.recv_timeout(Duration::from_secs(15));
    let _ = child.kill();
    let output = output.expect("SSH output did not return to the original ConPTY");
    assert!(String::from_utf8_lossy(&output).contains("OpenSSH"));
    assert_eq!(std::fs::read(executable).unwrap(), original);
}

#[path = "common/native_server.rs"]
mod native_server;

#[test]
fn password_login_and_image_upload_share_native_windows_session() {
    use std::{io::Write, sync::atomic::Ordering, time::Instant};
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let fixture = runtime.block_on(native_server::Fixture::new());
    let config = fixture.directory.path().join("config");
    std::fs::write(&config, format!(
        "Host fixture\n HostName 127.0.0.1\n Port {}\n User alice\n UserKnownHostsFile \"{}\"\n GlobalKnownHostsFile none\n StrictHostKeyChecking yes\n PubkeyAuthentication no\n KbdInteractiveAuthentication no\n PreferredAuthentications password\n",
        fixture.config.port, fixture.config.save_host_key.to_string_lossy().replace('\\', "/")
    )).unwrap();
    let integration =
        yiterm::ssh::Integration::new(Path::new(env!("CARGO_BIN_EXE_yiterm"))).unwrap();
    let pair = portable_pty::native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new("powershell.exe");
    command.args([
        "-NoProfile",
        "-Command",
        &format!(
            "ssh -F {} fixture; exit $LASTEXITCODE",
            yiterm::paste::quote_powershell(&config.to_string_lossy())
        ),
    ]);
    integration.configure(&mut command).unwrap();
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut writer = pair.master.take_writer().unwrap();
    let mut reader = pair.master.try_clone_reader().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = [0; 4096];
        while let Ok(count) = reader.read(&mut buffer) {
            if count == 0 || tx.send(buffer[..count].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut transcript = String::new();
    let result = (|| -> anyhow::Result<()> {
        let mut wait_for = |marker: &str| -> anyhow::Result<()> {
            let deadline = Instant::now() + Duration::from_secs(15);
            while !transcript.contains(marker) {
                let bytes = rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
                transcript.push_str(&String::from_utf8_lossy(&bytes));
            }
            Ok(())
        };
        wait_for("password:")?;
        writer.write_all(b"fixture-password\r")?;
        writer.flush()?;
        wait_for("fixture-shell-ready")?;
        let detected = yiterm::ssh::detect(child.process_id().unwrap(), None)?.unwrap();
        let pipe = detected
            .upload_pipe
            .as_ref()
            .expect("Native wrapper was not detected");
        let image_path = fixture.directory.path().join("image.png");
        let image = b"\x89PNG\r\n\x1a\nfixture-image";
        std::fs::write(&image_path, image)?;
        assert!(yiterm::native_ssh::upload(&image_path, pipe, detected.identity.pid + 1).is_err());
        for _ in 0..2 {
            let path = yiterm::paste::upload_detected(
                &image_path,
                &detected,
                Path::new("must-not-run-scp"),
            )?;
            assert_eq!(
                fixture.state.images.lock().unwrap()[path.trim_matches('\'')],
                image
            );
        }
        assert_eq!(fixture.state.connections.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.state.authentications.load(Ordering::SeqCst), 1);
        pair.master.resize(PtySize {
            rows: 35,
            cols: 90,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while *fixture.state.size.lock().unwrap() != (90, 35) {
            anyhow::ensure!(Instant::now() < deadline, "Remote terminal did not resize");
            std::thread::sleep(Duration::from_millis(50));
        }
        writer.write_all(b"\x03still-connected")?;
        writer.flush()?;
        wait_for("still-connected")?;
        writer.write_all(b"exit\r")?;
        writer.flush()?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait()? {
                assert_eq!(status.exit_code(), 7);
                break;
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "SSH did not return its remote exit status"
            );
            std::thread::sleep(Duration::from_millis(30));
        }
        assert!(!yiterm::ssh::target_is_alive(detected.identity, None));
        assert!(
            !transcript.contains("fixture-password"),
            "Password was echoed to the terminal"
        );
        Ok(())
    })();
    let _ = child.kill();
    result.unwrap_or_else(|error| panic!("{error:#}; terminal output: {transcript}"));
}
