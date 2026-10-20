#![cfg(windows)]

use portable_pty::{CommandBuilder, PtySize};
use std::{
    io::{Read, Write},
    path::Path,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

type TerminalWriter = Arc<Mutex<Box<dyn Write + Send>>>;

// portable-pty requests inherited cursor position. Unlike the GUI's VT parser,
// a raw pipe reader must explicitly answer DSR or ConPTY can block startup.
// Keep draining after assertions so ClosePseudoConsole cannot block on output.
fn terminal_io(master: &dyn portable_pty::MasterPty) -> (TerminalWriter, mpsc::Receiver<Vec<u8>>) {
    let writer = Arc::new(Mutex::new(master.take_writer().unwrap()));
    let replies = writer.clone();
    let mut reader = master.try_clone_reader().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut tail = Vec::new();
        let mut buffer = [0; 4096];
        while let Ok(count) = reader.read(&mut buffer) {
            if count == 0 {
                break;
            }
            tail.extend_from_slice(&buffer[..count]);
            for query in tail.windows(4) {
                if query == b"\x1b[6n" {
                    let mut writer = replies.lock().unwrap();
                    let _ = writer.write_all(b"\x1b[1;1R");
                    let _ = writer.flush();
                }
            }
            if tail.len() > 3 {
                tail.drain(..tail.len() - 3);
            }
            let _ = tx.send(buffer[..count].to_vec());
        }
    });
    (writer, rx)
}

#[test]
fn packaged_ssh_returns_output_and_exit_status_to_each_shell() {
    let release = tempfile::tempdir().unwrap();
    let app = release.path().join("release with spaces");
    std::fs::create_dir_all(app.join("bin")).unwrap();
    let executable = app.join("yiterm.exe");
    let helper = app.join("bin/ssh.exe");
    std::fs::copy(env!("CARGO_BIN_EXE_yiterm"), &executable).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_ssh"), &helper).unwrap();
    let original = std::fs::read(&helper).unwrap();
    let pe = u32::from_le_bytes(original[0x3c..0x40].try_into().unwrap()) as usize;
    assert_eq!(
        u16::from_le_bytes(original[pe + 92..pe + 94].try_into().unwrap()),
        3
    );
    let integration = yiterm::ssh::Integration::new(&executable).unwrap();
    for shell in ["powershell.exe", "pwsh.exe", "cmd.exe"] {
        check_console(&integration, shell);
    }
    let bash = Path::new(r"C:\Program Files\Git\bin\bash.exe");
    if bash.is_file() {
        check_console(&integration, bash.to_str().unwrap());
    }
    assert_eq!(std::fs::read(&helper).unwrap(), original);
}

fn check_console(integration: &yiterm::ssh::Integration, shell: &str) {
    let pair = portable_pty::native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    // --system is understood only by our helper, so this also proves PATH
    // selected it. Output and exit status must return through the same PTY.
    let mut command = CommandBuilder::new(shell);
    if shell == "cmd.exe" {
        command.args(["/d", "/c", "ssh --system -V"]);
    } else if shell.ends_with("bash.exe") {
        command.args(["--noprofile", "--norc", "-c", "ssh --system -V"]);
    } else {
        command.args([
            "-NoProfile",
            "-Command",
            "ssh --system -V; exit $LASTEXITCODE",
        ]);
    }
    integration.configure(&mut command).unwrap();
    let (_writer, receiver) = terminal_io(pair.master.as_ref());
    eprintln!("Starting {shell} with packaged SSH helper");
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let result = || {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut output = Vec::new();
        while !String::from_utf8_lossy(&output).contains("OpenSSH") {
            let bytes = receiver
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .unwrap_or_else(|error| {
                    panic!(
                        "{shell}: {error}; terminal: {}",
                        String::from_utf8_lossy(&output)
                    )
                });
            output.extend(bytes);
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert_eq!(status.exit_code(), 0, "{shell}");
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{shell} did not wait/exit correctly"
            );
            std::thread::sleep(Duration::from_millis(30));
        }
    };
    // Keep cleanup outside assertions even if the test fails.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(result));
    let _ = child.kill();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[path = "common/native_server.rs"]
mod native_server;

#[test]
fn password_login_and_image_upload_share_native_windows_session() {
    use std::{sync::atomic::Ordering, time::Instant};
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
    let (writer, rx) = terminal_io(pair.master.as_ref());
    eprintln!("Starting native SSH password/upload session");
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
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
        writer.lock().unwrap().write_all(b"fixture-password\r")?;
        writer.lock().unwrap().flush()?;
        wait_for("fixture-shell-ready")?;
        let detected = yiterm::ssh::detect(
            child.process_id().unwrap(),
            None,
            &integration.detection_context(),
        )?
        .unwrap();
        let pipe = detected
            .upload_pipe
            .as_ref()
            .expect("Native wrapper was not detected");
        let image_path = fixture.directory.path().join("image.png");
        let image = b"\x89PNG\r\n\x1a\nfixture-image";
        std::fs::write(&image_path, image)?;
        assert!(yiterm::native_ssh::upload(&image_path, pipe, detected.identity.pid + 1).is_err());
        // Reuse the same SSH session and pipe repeatedly to catch cases where
        // the server is not already accepting the next paste after a reply.
        for _ in 0..8 {
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
        writer.lock().unwrap().write_all(b"\x03still-connected")?;
        writer.lock().unwrap().flush()?;
        wait_for("still-connected")?;
        writer.lock().unwrap().write_all(b"exit\r")?;
        writer.lock().unwrap().flush()?;
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
