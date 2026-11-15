//! Read a single password from the existing encrypted SSH terminal, without echo.
//! Never accept credentials from arguments/environment/files or retry submissions.
use anyhow::{Result, bail, ensure};
use std::{
    io::{IsTerminal, Read, Write},
    sync::atomic::Ordering,
};
use zeroize::{Zeroize, Zeroizing};
unsafe extern "C" {
    fn yiterm_capture_locked() -> i32;
    fn yiterm_capture_unlock_ready() -> i32;
    fn yiterm_capture_unlock(
        bytes: *const u8,
        len: usize,
        cancelled: Option<extern "C" fn() -> i32>,
    ) -> i32;
}
pub fn locked() -> bool {
    unsafe { yiterm_capture_locked() == 1 }
}
extern "C" fn cancelled() -> i32 {
    i32::from(super::macos::STOP.load(Ordering::Relaxed))
}
struct Prompt {
    mode: libc::termios,
    signals: Vec<(i32, libc::sighandler_t)>,
}
impl Prompt {
    fn enter() -> Result<Self> {
        ensure!(
            std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
            "Mac is locked; unlock requires an interactive SSH terminal (ssh -t)"
        );
        let mut mode = unsafe { std::mem::zeroed::<libc::termios>() };
        ensure!(
            unsafe { libc::tcgetattr(0, &mut mode) } == 0,
            "Cannot read password terminal mode"
        );
        let mut raw = mode;
        raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG | libc::IEXTEN);
        raw.c_iflag &= !(libc::IXON | libc::IXOFF | libc::ICRNL | libc::INLCR | libc::IGNCR);
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;
        ensure!(
            unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) } == 0,
            "Cannot disable password echo"
        );
        let signals = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGPIPE]
            .into_iter()
            .map(|s| {
                (s, unsafe {
                    libc::signal(s, super::macos::stop as *const () as libc::sighandler_t)
                })
            })
            .collect();
        unsafe {
            libc::tcflush(0, libc::TCIFLUSH);
        }
        Ok(Self { mode, signals })
    }
}
impl Drop for Prompt {
    fn drop(&mut self) {
        unsafe {
            libc::tcflush(0, libc::TCIFLUSH);
            libc::tcsetattr(0, libc::TCSANOW, &self.mode);
            for &(signal, handler) in &self.signals {
                libc::signal(signal, handler);
            }
        }
        let _ = std::io::stderr().write_all(b"\r\n");
    }
}
pub fn submit_once(read_only: bool) -> Result<()> {
    if !locked() {
        return Ok(());
    }
    ensure!(
        !read_only,
        "Mac is locked; read-only sharing does not collect an unlock password"
    );
    ensure!(
        unsafe { yiterm_capture_unlock_ready() } >= 0,
        "Cannot unlock this session: requires the current logged-in console user, Accessibility permission and a recognized Apple loginwindow password field. No password collected"
    );
    if !locked() {
        return Ok(());
    }
    let _prompt = Prompt::enter()?;
    std::io::stderr().write_all(b"Mac locked. Login password (hidden; Ctrl+] cancels): ")?;
    std::io::stderr().flush()?;
    let mut password = Zeroizing::new(Vec::new());
    loop {
        ensure!(cancelled() == 0, "Unlock cancelled");
        let mut poll = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut poll, 1, 100) } < 0 {
            continue;
        }
        ensure!(
            poll.revents & (libc::POLLHUP | libc::POLLERR) == 0,
            "SSH disconnected; unlock cancelled"
        );
        if poll.revents & libc::POLLIN == 0 {
            continue;
        }
        let mut byte = [0];
        if std::io::stdin().read(&mut byte)? == 0 {
            continue;
        }
        match byte[0] {
            3 | 29 | 27 => bail!("Unlock cancelled"),
            b'\r' | b'\n' => break,
            8 | 127 => {
                if let Some(start) = password.iter().rposition(|b| b & 0xc0 != 0x80) {
                    password[start..].zeroize();
                    password.truncate(start);
                }
            }
            21 => {
                password.zeroize();
                password.clear();
            }
            0 => bail!("Invalid unlock password"),
            b => {
                ensure!(password.len() < 4096, "Unlock password exceeds size limit");
                password.push(b);
            }
        }
    }
    ensure!(
        !password.is_empty() && std::str::from_utf8(&password).is_ok(),
        "Unlock password must be nonempty UTF-8"
    );
    match unsafe { yiterm_capture_unlock(password.as_ptr(), password.len(), Some(cancelled)) } {
        0 => Ok(()),
        1 => bail!("Mac did not unlock. Password submitted once; no automatic retry"),
        -2 => bail!("Unlock rate limit reached; no password resubmitted"),
        _ => bail!("Unlock unavailable or cancelled; no automatic retry"),
    }
}
