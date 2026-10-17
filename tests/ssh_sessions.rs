#![cfg(unix)]
use portable_pty::{MasterPty, PtySize};
use std::{
    io::{Read, Write},
    time::{Duration, Instant},
};
use yiterm::ssh;

struct Session {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    reader: Option<Box<dyn Read + Send>>,
    // Keep bootstrap scripts/sockets alive until the PTY is closed.
    _integration: ssh::Integration,
}
impl Session {
    fn new(shell: &str) -> Self {
        let ssh = std::process::Command::new("/usr/bin/ssh")
            .arg("-V")
            .output()
            .expect("SSH session tests require OpenSSH at /usr/bin/ssh");
        assert!(
            ssh.status.success(),
            "OpenSSH could not start: {}",
            String::from_utf8_lossy(&ssh.stderr)
        );
        let integration =
            ssh::Integration::new(std::path::Path::new(env!("CARGO_BIN_EXE_yiterm"))).unwrap();
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let command = integration.shell(shell).unwrap();
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        Self {
            child,
            writer: pair.master.take_writer().unwrap(),
            reader: Some(pair.master.try_clone_reader().unwrap()),
            master: pair.master,
            _integration: integration,
        }
    }
    fn send(&mut self, command: &str) {
        self.writer.write_all(command.as_bytes()).unwrap();
        self.writer.flush().unwrap();
    }
    fn detect(&self) -> anyhow::Result<Option<ssh::Detection>> {
        ssh::detect(
            self.child.process_id().unwrap(),
            self.master.process_group_leader(),
        )
    }
    fn wait_remote(&self) -> ssh::Detection {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let result = self.detect();
            if let Ok(Some(remote)) = result {
                return remote;
            }
            if Instant::now() >= deadline {
                if let Some(tty) = self.master.tty_name() {
                    let output = std::process::Command::new("/bin/ps")
                        .args([
                            "-t",
                            &tty.to_string_lossy(),
                            "-o",
                            "pid,ppid,pgid,comm,args",
                        ])
                        .output()
                        .unwrap();
                    eprintln!(
                        "Fixture processes: {}",
                        String::from_utf8_lossy(&output.stdout)
                    );
                }
                panic!(
                    "Foreground SSH was not detected (group {:?}, error {:?})",
                    self.master.process_group_leader(),
                    result.err()
                );
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if let Some(group) = self.master.process_group_leader() {
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
        // Close PTY handles before waiting: macOS can block process teardown
        // while the master is still open. The test process reaps on exit.
    }
}

#[test]
fn typed_ssh_is_detected_and_exit_restores_local_routing() {
    let mut session = Session::new("/bin/sh");
    assert!(session.detect().unwrap().is_none());
    // ProxyCommand provides a live SSH process without contacting any host.
    session.send("ssh -F /dev/null -oProxyCommand='exec sleep 60' -oConnectTimeout=30 alice@yiterm-test.invalid\r");
    let detected = session.wait_remote();
    assert_eq!(detected.remote.host, "alice@yiterm-test.invalid");
    assert!(
        detected
            .remote
            .options
            .iter()
            .any(|arg| arg.starts_with("ControlPath=/tmp/yiterm-"))
    );
    assert!(ssh::target_is_alive(
        detected.identity,
        session.master.process_group_leader()
    ));
    unsafe {
        libc::kill(-detected.identity.group.unwrap(), libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while !matches!(session.detect(), Ok(None)) {
        assert!(
            Instant::now() < deadline,
            "Local routing did not return after SSH exit"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(!ssh::target_is_alive(
        detected.identity,
        session.master.process_group_leader()
    ));
}

#[test]
fn shell_bootstrap_preserves_typed_ssh_and_enables_multiplexing() {
    let mut shells = vec!["/bin/sh", "/bin/bash"];
    if std::path::Path::new("/bin/zsh").exists() {
        shells.push("/bin/zsh");
    }
    if std::path::Path::new("/opt/homebrew/bin/fish").exists() {
        shells.push("/opt/homebrew/bin/fish");
    }
    for shell in shells {
        let mut session = Session::new(shell);
        let mut reader = session.reader.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut output = Vec::new();
            let mut buffer = [0; 4096];
            while let Ok(n) = reader.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                output.extend_from_slice(&buffer[..n]);
                if String::from_utf8_lossy(&output).contains("\nyiterm-done-marker\r\n") {
                    break;
                }
            }
            let _ = tx.send(output);
        });
        session.send("ssh -G -F /dev/null alice@yiterm-test.invalid; echo yiterm-done-marker\r");
        let output = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("SSH config query timed out");
        let output = String::from_utf8_lossy(&output);
        assert!(
            output.contains("controlmaster auto"),
            "{shell}: SSH did not get the multiplexing default; PTY output:\n{output}"
        );
        assert!(
            output.contains("controlpath /tmp/yiterm-"),
            "{shell}: SSH did not get a private socket path; PTY output:\n{output}"
        );
    }
}

#[test]
fn jump_transport_child_does_not_replace_interactive_destination() {
    let mut session = Session::new("/bin/sh");
    // Keep the outer SSH and its -W proxy child alive without network access.
    session.send("ssh -F /dev/null -oProxyCommand='exec /usr/bin/ssh -F /dev/null -oProxyCommand=\"exec sleep 60\" -oConnectTimeout=30 -W target.invalid:22 jump.invalid' -oConnectTimeout=30 alice@target.invalid\r");
    let deadline = Instant::now() + Duration::from_secs(10);
    // Wait until the proxy child really exists so the regression is exercised.
    loop {
        let output = std::process::Command::new("/bin/ps")
            .args([
                "-t",
                &session.master.tty_name().unwrap().to_string_lossy(),
                "-o",
                "args=",
            ])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        if text.lines().any(|line| {
            line.trim_start().starts_with("/usr/bin/ssh -F /dev/null")
                && line.contains("-W target.invalid:22")
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "SSH proxy child did not start: {text}"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    let detected = session
        .detect()
        .expect("Transport child must not cause a -W error")
        .unwrap();
    assert_eq!(detected.remote.host, "alice@target.invalid");
    assert!(ssh::target_is_alive(
        detected.identity,
        session.master.process_group_leader()
    ));
}
