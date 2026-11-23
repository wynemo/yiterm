use super::*;
use futures_util::future::{FusedFuture, FutureExt};
use rustls::{
    ServerConfig,
    pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
};
use tokio::net::TcpListener;
use yiterm::screen_share::Options;

#[cfg(target_os = "macos")]
#[path = "host/macos.rs"]
mod platform;
#[cfg(any(target_os = "linux", windows))]
#[path = "host/desktop.rs"]
mod platform;

#[cfg(target_os = "macos")]
#[path = "host/virtual_desktop.rs"]
mod virtual_desktop;
#[cfg(target_os = "macos")]
use virtual_desktop::VirtualDesktop;
#[cfg(target_os = "macos")]
type HostWorkspace = Option<VirtualDesktop>;
#[cfg(not(target_os = "macos"))]
type HostWorkspace = ();

type ServerStream = tokio_rustls::server::TlsStream<TcpStream>;
type Stop<'a, F> = std::pin::Pin<&'a mut F>;

pub fn run(address: SocketAddr, options: &Options) -> Result<()> {
    #[cfg(windows)]
    {
        use yiterm::screen_share::windows::{prepare, privilege};
        if privilege::elevated()? && !privilege::effective_system() {
            let process = privilege::launch(std::env::args_os().skip(1).collect(), data_dir()?)?;
            // The replacement owns its console and Ctrl+C handler.
            process.detach();
            return Ok(());
        }
        prepare()?;
    }
    #[cfg(target_os = "linux")]
    yiterm::screen_share::linux::require_desktop(options)?;
    runtime()?.block_on(async {
        #[cfg(unix)]
        let stopped = {
            let mut interrupt =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            async move {
                tokio::select! { _ = interrupt.recv() => {}, _ = terminate.recv() => {} }
            }
        };
        #[cfg(windows)]
        let stopped = {
            let mut interrupt = tokio::signal::windows::ctrl_c()?;
            async move {
                let _ = interrupt.recv().await;
            }
        };
        let listener = bind(address)?;
        eprintln!(
            "YiTerm sharing on {}. Press Ctrl+C to stop.",
            listener.local_addr()?
        );
        listen(listener, config()?, options, stopped).await
    })
}

pub struct Host {
    pub address: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
    #[cfg(windows)]
    process: Option<yiterm::screen_share::windows::privilege::SharingProcess>,
}

impl Host {
    pub fn start(address: SocketAddr, options: Options) -> Result<Self> {
        #[cfg(windows)]
        {
            use yiterm::screen_share::windows::{privilege, require_user_desktop};
            if privilege::elevated()? && !privilege::effective_system() {
                // Resolve an ephemeral port before handing the listener to the child.
                let socket = std::net::TcpListener::bind(address)?;
                let address = socket.local_addr()?;
                drop(socket);
                let mut args: Vec<std::ffi::OsString> = vec![
                    "--share-screen".into(),
                    "--share-listen".into(),
                    address.to_string().into(),
                    "--share-width".into(),
                    options.share_width.to_string().into(),
                    "--share-fps".into(),
                    options.share_fps.to_string().into(),
                ];
                for (name, value) in [
                    ("--share-display", options.share_display),
                    ("--share-window", options.share_window),
                ] {
                    if let Some(value) = value {
                        args.extend([name.into(), value.to_string().into()]);
                    }
                }
                if let Some(quality) = &options.share_quality {
                    args.extend(["--share-quality".into(), quality.into()]);
                }
                for (name, enabled) in [
                    ("--share-read-only", options.share_read_only),
                    (
                        "--share-adaptive-resolution",
                        options.share_adaptive_resolution,
                    ),
                    ("--share-fixed-resolution", options.share_fixed_resolution),
                ] {
                    if enabled {
                        args.push(name.into());
                    }
                }
                let process = privilege::launch(args, data_dir()?)?;
                return Ok(Self {
                    address,
                    stop: None,
                    worker: None,
                    process: Some(process),
                });
            }
            require_user_desktop()?;
        }
        #[cfg(target_os = "linux")]
        yiterm::screen_share::linux::require_desktop(&options)?;
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
            #[cfg(windows)]
            process: None,
        })
    }

    pub fn is_finished(&self) -> bool {
        #[cfg(windows)]
        if let Some(process) = &self.process {
            return process.is_finished();
        }
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
            // The macOS listener can be waiting on CoreGraphics work on the
            // AppKit main queue. Keep that queue free while cleanup finishes.
            #[cfg(target_os = "macos")]
            let _ = std::thread::spawn(move || { let _ = worker.join(); });
            #[cfg(not(target_os = "macos"))]
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
    let mut workspace = HostWorkspace::default();
    let stop = stop.fuse();
    tokio::pin!(stop);
    loop {
        let (socket, peer) = tokio::select! {
            biased;
            _ = &mut stop => return Ok(()),
            result = listener.accept() => result?,
        };
        let result = serve(socket, &acceptor, options, stop.as_mut(), &mut workspace).await;
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
    #[cfg(target_os = "macos")]
    config.alpn_protocols.insert(0, DESKTOP_ALPN.to_vec());
    #[cfg(windows)]
    config.alpn_protocols.insert(0, WINDOWS_ALPN.to_vec());
    config.send_tls13_tickets = 0;
    Ok(config)
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
    workspace: &mut HostWorkspace,
) -> Result<()> {
    socket.set_nodelay(true)?;
    let setup = async {
        let mut stream = acceptor.accept(socket).await?;
        let desktop = stream.get_ref().1.alpn_protocol() == Some(DESKTOP_ALPN);
        let windows = stream.get_ref().1.alpn_protocol() == Some(WINDOWS_ALPN);
        ensure!(stream.get_ref().1.alpn_protocol() == Some(ALPN) || desktop || windows,
                "Incompatible YiTerm sharing protocol");
        ensure!(!windows || cfg!(windows), "Windows desktop protocol requires Windows");
        ensure!(!desktop || cfg!(target_os = "macos"), "Virtual desktop requires macOS");
        ensure!(!desktop || (options.share_window.is_none() && options.share_display.is_none()),
                "Tenimon virtual desktop requires sharing without --share-window or --share-display");
        ensure!(
            stream.read_u8().await? == RESIZE,
            "Missing initial YiTerm terminal size"
        );
        let size = read_size(&mut stream).await?;
        let desktop_size = if desktop {
            ensure!(stream.read_u8().await? == DESKTOP_SIZE, "Missing initial iPad desktop size");
            Some(read_desktop_size(&mut stream).await?)
        } else { None };
        Ok::<_, anyhow::Error>((stream, size, desktop_size, windows))
    };
    let (mut stream, size, desktop_size, windows) = tokio::select! {
        _ = stop.as_mut() => return Ok(()),
        result = timeout(IO_TIMEOUT, setup) => result??,
    };
    #[cfg(target_os = "macos")]
    let result = {
        let _ = windows;
        let prepared = (|| -> Result<()> {
            if let Some(requested) = desktop_size {
                if workspace.is_none() { *workspace = Some(VirtualDesktop::create(requested)?); }
                workspace.as_mut().context("Missing virtual display owner")?.resize(requested)?;
            }
            Ok(())
        })();
        if let Err(error) = prepared {
            let message = format!("\r\nYiTerm virtual desktop: {error:#}\r\n");
            let _ = timeout(IO_TIMEOUT, stream.write_all(message.as_bytes())).await;
            return Err(error);
        }
        let desktop = if desktop_size.is_some() { workspace.as_mut() } else { None };
        platform::session(stream, size, options, stop, desktop).await
    };
    #[cfg(not(target_os = "macos"))]
    let result = {
        let _ = (workspace, desktop_size);
        platform::session(stream, size, options, stop, windows).await
    };
    eprintln!("YiTerm viewer disconnected.");
    result
}

async fn read_desktop_size(reader: &mut (impl AsyncReadExt + Unpin)) -> Result<[u16; 3]> {
    let request = [reader.read_u16().await?, reader.read_u16().await?, reader.read_u16().await?];
    ensure!((320..=1920).contains(&request[0]) && (320..=1920).contains(&request[1])
        && (100..=400).contains(&request[2]), "Invalid iPad virtual desktop size");
    Ok(request)
}

async fn receive(
    reader: &mut (impl AsyncReadExt + Unpin),
    input: impl AsyncFnMut(&[u8]) -> Result<()>,
    resize: impl FnMut(PtySize) -> Result<()>,
) -> Result<()> {
    receive_with_desktop(reader, input, resize, |_| anyhow::bail!("This session has no virtual display")).await
}

async fn receive_with_desktop(
    reader: &mut (impl AsyncReadExt + Unpin),
    mut input: impl AsyncFnMut(&[u8]) -> Result<()>,
    mut resize: impl FnMut(PtySize) -> Result<()>,
    mut desktop: impl FnMut([u16; 3]) -> Result<()>,
) -> Result<()> {
    use zeroize::Zeroize;

    let mut buffer = Zeroizing::new(vec![0; MAX_INPUT]);
    loop {
        match timeout(HEARTBEAT * 3, reader.read_u8()).await? {
            Ok(INPUT) => {
                timeout(IO_TIMEOUT, async {
                    let n = usize::from(reader.read_u16().await?);
                    ensure!((1..=MAX_INPUT).contains(&n), "Invalid YiTerm input length");
                    reader.read_exact(&mut buffer[..n]).await?;
                    input(&buffer[..n]).await?;
                    buffer[..n].zeroize();
                    Ok::<_, anyhow::Error>(())
                })
                .await??;
            }
            Ok(RESIZE) => resize(timeout(IO_TIMEOUT, read_size(reader)).await??)?,
            Ok(PING) => {}
            Ok(DESKTOP_SIZE) => desktop(timeout(IO_TIMEOUT, read_desktop_size(reader)).await??)?,
            Ok(_) => anyhow::bail!("Unknown YiTerm input frame"),
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }
}
