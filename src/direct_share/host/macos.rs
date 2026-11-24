use super::*;
use portable_pty::{Child, CommandBuilder, native_pty_system};
use std::os::fd::RawFd;
use tokio::io::unix::AsyncFd;

fn capture_command(o: &Options, virtual_display: Option<u32>) -> Result<CommandBuilder> {
    let mut command = CommandBuilder::new(std::env::current_exe()?);
    command.arg("--share-screen");
    for (flag, value) in [
        ("--share-display", o.share_display),
        ("--share-window", o.share_window),
        ("--share-width", Some(o.share_width)),
        ("--share-fps", Some(o.share_fps)),
    ] {
        if let Some(value) = value {
            command.args([flag, &value.to_string()]);
        }
    }
    if let Some(id) = virtual_display {
        command.args(["--share-display", &id.to_string()]);
        command.env("YITERM_VIRTUAL_DISPLAY_ID", id.to_string());
    }
    if let Some(quality) = &o.share_quality {
        command.args(["--share-quality", quality]);
    }
    for (flag, enabled) in [
        ("--share-read-only", o.share_read_only),
        ("--share-fixed-resolution", o.share_fixed_resolution),
        ("--share-adaptive-resolution", o.share_adaptive_resolution),
    ] {
        if enabled {
            command.arg(flag);
        }
    }
    command.env("TERM", "xterm-256color");
    Ok(command)
}

fn configure_pty(fd: RawFd) -> io::Result<()> {
    let mut mode = unsafe { std::mem::zeroed::<libc::termios>() };
    // Disable echo before the child can receive any bytes, including during startup.
    unsafe {
        if libc::tcgetattr(fd, &mut mode) == -1 {
            return Err(io::Error::last_os_error());
        }
        libc::cfmakeraw(&mut mode);
        if libc::tcsetattr(fd, libc::TCSANOW, &mode) == -1 {
            return Err(io::Error::last_os_error());
        }
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags == -1 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

async fn read_pty(fd: &AsyncFd<RawFd>, bytes: &mut [u8]) -> io::Result<usize> {
    loop {
        let mut ready = fd.readable().await?;
        if let Ok(result) = ready.try_io(|fd| {
            let n = unsafe { libc::read(*fd.get_ref(), bytes.as_mut_ptr().cast(), bytes.len()) };
            if n < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        }) {
            return result;
        }
    }
}

async fn write_pty(fd: &AsyncFd<RawFd>, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        let mut ready = fd.writable().await?;
        if let Ok(result) = ready.try_io(|fd| {
            let n = unsafe { libc::write(*fd.get_ref(), bytes.as_ptr().cast(), bytes.len()) };
            match n {
                ..0 => Err(io::Error::last_os_error()),
                0 => Err(io::ErrorKind::WriteZero.into()),
                _ => Ok(n as usize),
            }
        }) {
            bytes = &bytes[result?..];
        }
    }
    Ok(())
}

async fn reap(mut child: Box<dyn Child + Send + Sync>) {
    // Leave time for InputPump to release any mouse button before forcing exit.
    if let Some(pid) = child.process_id() {
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
    let _ = timeout(Duration::from_secs(2), async {
        loop {
            if child.try_wait()?.is_some() {
                return Ok::<_, io::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

pub(super) async fn session(
    stream: ServerStream,
    size: PtySize,
    options: &Options,
    mut stop: Stop<'_, impl std::future::Future<Output = ()>>,
    mut desktop: Option<&mut VirtualDesktop>,
) -> Result<()> {
    let pair = native_pty_system().openpty(size)?;
    let fd = pair.master.as_raw_fd().context("Cannot open capture PTY")?;
    configure_pty(fd)?;
    let fd = AsyncFd::new(fd)?;
    let child = pair.slave.spawn_command(capture_command(options, desktop.as_ref().map(|d| d.id()).transpose()?)?)?;
    drop(pair.slave);
    eprintln!("YiTerm viewer connected.");
    let (mut reader, mut writer) = tokio::io::split(stream);
    let result = {
        let (sizes, mut requested_sizes) = mpsc::channel::<[u16; 3]>(8);
        let receive = receive_with_desktop(
            &mut reader,
            async |bytes: &[u8]| Ok(write_pty(&fd, bytes).await?),
            |size| pair.master.resize(size),
            |requested| {
                let owner = desktop.as_mut().context("This session has no virtual display")?;
                owner.resize(requested)?;
                sizes.try_send(requested).context("Virtual display resize queue full or capture ended")
            },
        );
        let update_capture = async {
            while let Some([w, h, scale]) = requested_sizes.recv().await {
                let report = format!("\x1b[?9004;{w};{h};{scale}t");
                write_pty(&fd, report.as_bytes()).await?;
            }
            Ok::<_, anyhow::Error>(())
        };
        let send = async {
            let mut buffer = [0; MAX_INPUT];
            loop {
                let n = match read_pty(&fd, &mut buffer).await {
                    Ok(0) => break,
                    Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                    result => result?,
                };
                timeout(IO_TIMEOUT, async {
                    writer.write_all(&buffer[..n]).await?;
                    writer.flush().await
                })
                .await??;
            }
            Ok(())
        };
        tokio::select! {
            _ = stop.as_mut() => Ok(()),
            result = receive => result,
            result = send => result,
            result = update_capture => result,
        }
    };
    // All exit paths must close TLS, including PTY input/resize failures.
    // Otherwise the viewer sees rustls UnexpectedEof instead of a clean end.
    let _ = timeout(IO_TIMEOUT, writer.shutdown()).await;
    drop(fd);
    drop(pair.master);
    reap(child).await;
    result
}
