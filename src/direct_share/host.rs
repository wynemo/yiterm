use super::*;
use futures_util::future::{FusedFuture, FutureExt};
use portable_pty::{Child, CommandBuilder, native_pty_system};
use rustls::{
    ServerConfig,
    pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
};
use std::os::fd::RawFd;
use tokio::{io::unix::AsyncFd, net::TcpListener};
use yiterm::screen_share::Options;

pub fn run(address: SocketAddr, options: &Options) -> Result<()> {
    runtime()?.block_on(async {
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let listener = bind(address)?;
        eprintln!(
            "YiTerm sharing on {}. Press Ctrl+C to stop.",
            listener.local_addr()?
        );
        listen(listener, config()?, options, async {
            tokio::select! { _ = interrupt.recv() => {}, _ = terminate.recv() => {} }
        })
        .await
    })
}

pub struct Host {
    pub address: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Host {
    pub fn start(address: SocketAddr, options: Options) -> Result<Self> {
        let config = config()?;
        let runtime = runtime()?;
        let listener = {
            let _runtime = runtime.enter();
            bind(address)?
        };
        let address = listener.local_addr()?;
        let (stop, stopped) = oneshot::channel();
        let worker = std::thread::Builder::new()
            .name("yiterm-share-host".into())
            .spawn(move || {
                let result = runtime.block_on(listen(listener, config, &options, async {
                    let _ = stopped.await;
                }));
                if let Err(error) = result {
                    tracing::warn!(%error, "YiTerm sharing listener stopped");
                }
                super::super::wake_ui();
            })?;
        Ok(Self {
            address,
            stop: Some(stop),
            worker: Some(worker),
        })
    }

    pub fn is_finished(&self) -> bool {
        self.worker
            .as_ref()
            .is_some_and(|worker| worker.is_finished())
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn runtime() -> io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}

fn bind(address: SocketAddr) -> Result<TcpListener> {
    let listener = std::net::TcpListener::bind(address)
        .with_context(|| format!("Cannot listen on {address}"))?;
    listener.set_nonblocking(true)?;
    Ok(TcpListener::from_std(listener)?)
}

async fn listen(
    listener: TcpListener,
    config: ServerConfig,
    options: &Options,
    stop: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let stop = stop.fuse();
    tokio::pin!(stop);
    loop {
        let (socket, peer) = tokio::select! {
            biased;
            _ = &mut stop => return Ok(()),
            result = listener.accept() => result?,
        };
        let result = serve(socket, &acceptor, options, stop.as_mut()).await;
        if stop.is_terminated() {
            return Ok(());
        }
        if let Err(error) = result {
            eprintln!("YiTerm connection from {peer} ended: {error:#}");
        }
    }
}

fn config() -> Result<ServerConfig> {
    let path = data_dir()?.join("host-identity");
    let load = || -> Result<_> { Ok(read_saved(&path, 8192)?.map(Zeroizing::new)) };
    let saved = if let Some(saved) = load()? {
        saved
    } else {
        let identity = rcgen::generate_simple_self_signed(vec!["yiterm-direct.invalid".into()])?;
        let cert = identity.cert.der();
        let key = Zeroizing::new(identity.signing_key.serialize_der());
        let mut bytes = Zeroizing::new(Vec::with_capacity(2 + cert.len() + key.len()));
        bytes.extend_from_slice(&u16::try_from(cert.len())?.to_be_bytes());
        bytes.extend_from_slice(cert);
        bytes.extend_from_slice(&key);
        if save_once(&path, &bytes)? {
            bytes
        } else {
            load()?.context("Missing YiTerm host identity")?
        }
    };
    let header: [u8; 2] = saved
        .get(..2)
        .context("Invalid YiTerm host identity")?
        .try_into()?;
    let boundary = 2 + usize::from(u16::from_be_bytes(header));
    let cert = saved
        .get(2..boundary)
        .context("Invalid YiTerm host certificate")?;
    let key = saved.get(boundary..).context("Invalid YiTerm host key")?;
    let mut config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(cert.to_vec())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.to_vec())),
            )?;
    config.alpn_protocols = vec![ALPN.to_vec()];
    config.send_tls13_tickets = 0;
    Ok(config)
}

fn capture_command(o: &Options) -> Result<CommandBuilder> {
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

async fn read_size(reader: &mut (impl AsyncReadExt + Unpin)) -> Result<PtySize> {
    let cols = reader.read_u16().await?;
    let rows = reader.read_u16().await?;
    let pixel_width = reader.read_u16().await?;
    let pixel_height = reader.read_u16().await?;
    ensure!(cols > 0 && rows > 0, "Invalid YiTerm terminal size");
    Ok(PtySize {
        cols,
        rows,
        pixel_width,
        pixel_height,
    })
}

async fn serve(
    socket: TcpStream,
    acceptor: &tokio_rustls::TlsAcceptor,
    options: &Options,
    mut stop: std::pin::Pin<&mut impl std::future::Future<Output = ()>>,
) -> Result<()> {
    socket.set_nodelay(true)?;
    let setup = async {
        let mut stream = acceptor.accept(socket).await?;
        ensure!(
            stream.get_ref().1.alpn_protocol() == Some(ALPN),
            "Incompatible YiTerm sharing protocol"
        );
        ensure!(
            stream.read_u8().await? == RESIZE,
            "Missing initial YiTerm terminal size"
        );
        let size = read_size(&mut stream).await?;
        Ok::<_, anyhow::Error>((stream, size))
    };
    let (stream, size) = tokio::select! {
        _ = stop.as_mut() => return Ok(()),
        result = timeout(IO_TIMEOUT, setup) => result??,
    };
    let pair = native_pty_system().openpty(size)?;
    let fd = pair.master.as_raw_fd().context("Cannot open capture PTY")?;
    configure_pty(fd)?;
    let fd = AsyncFd::new(fd)?;
    let child = pair.slave.spawn_command(capture_command(options)?)?;
    drop(pair.slave);
    eprintln!("YiTerm viewer connected.");
    let (mut reader, mut writer) = tokio::io::split(stream);
    let result = {
        let receive = async {
            let mut buffer = Zeroizing::new(vec![0; MAX_INPUT]);
            loop {
                match timeout(HEARTBEAT * 3, reader.read_u8()).await? {
                    Ok(INPUT) => {
                        timeout(IO_TIMEOUT, async {
                            let n = usize::from(reader.read_u16().await?);
                            ensure!((1..=MAX_INPUT).contains(&n), "Invalid YiTerm input length");
                            reader.read_exact(&mut buffer[..n]).await?;
                            write_pty(&fd, &buffer[..n]).await?;
                            use zeroize::Zeroize;
                            buffer[..n].zeroize();
                            Ok::<_, anyhow::Error>(())
                        })
                        .await??
                    }
                    Ok(RESIZE) => pair
                        .master
                        .resize(timeout(IO_TIMEOUT, read_size(&mut reader)).await??)?,
                    Ok(PING) => {}
                    Ok(_) => anyhow::bail!("Unknown YiTerm input frame"),
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                    Err(error) => return Err(error.into()),
                }
            }
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
            let _ = timeout(IO_TIMEOUT, writer.shutdown()).await;
            Ok(())
        };
        tokio::select! {
            _ = stop.as_mut() => Ok(()),
            result = receive => result,
            result = send => result,
        }
    };
    drop(fd);
    drop(pair.master);
    reap(child).await;
    eprintln!("YiTerm viewer disconnected.");
    result
}
