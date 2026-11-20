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

type ServerStream = tokio_rustls::server::TlsStream<TcpStream>;
type Stop<'a, F> = std::pin::Pin<&'a mut F>;

pub fn run(address: SocketAddr, options: &Options) -> Result<()> {
    #[cfg(windows)]
    yiterm::screen_share::windows::require_user_desktop()?;
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
}

impl Host {
    pub fn start(address: SocketAddr, options: Options) -> Result<Self> {
        #[cfg(windows)]
        yiterm::screen_share::windows::require_user_desktop()?;
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
    let result = platform::session(stream, size, options, stop).await;
    eprintln!("YiTerm viewer disconnected.");
    result
}

async fn receive(
    reader: &mut (impl AsyncReadExt + Unpin),
    mut input: impl AsyncFnMut(&[u8]) -> Result<()>,
    mut resize: impl FnMut(PtySize) -> Result<()>,
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
            Ok(_) => anyhow::bail!("Unknown YiTerm input frame"),
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }
}
