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
