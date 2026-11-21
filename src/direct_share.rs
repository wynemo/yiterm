//! TLS transport for YiTerm screen sharing, with raw VT output and framed input.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
pub mod host;

use anyhow::{Context, Result, ensure};
use portable_pty::PtySize;
use rustls::{
    ClientConfig, DigitallySignedStruct, Error, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{WebPkiSupportedAlgorithms, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use std::{
    fs,
    io::{self, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{mpsc, oneshot, watch},
    time::{interval, timeout},
};
use zeroize::Zeroizing;

const ALPN: &[u8] = b"yiterm-screen/1";
const MAX_INPUT: usize = 16 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const HEARTBEAT: Duration = Duration::from_secs(5);
const INPUT: u8 = 1;
const RESIZE: u8 = 2;
const PING: u8 = 3;
const RECENT_LIMIT: usize = 5;
const INPUT_QUEUE_FULL: &str = "YiTerm input queue full; disconnected";

fn data_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    let path = std::env::var_os("YITERM_SHARE_IDENTITY_DIR")
        .map(PathBuf::from)
        .unwrap_or(yiterm::logging::config_path()?.with_file_name("direct-share"));
    #[cfg(not(windows))]
    let path = yiterm::logging::config_path()?.with_file_name("direct-share");
    fs::create_dir_all(&path)?;
    Ok(path)
}

pub fn recent_addresses() -> Result<Vec<SocketAddr>> {
    load_recent(&data_dir()?.join("recent.json"))
}

fn load_recent(path: &Path) -> Result<Vec<SocketAddr>> {
    match fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).context("Cannot decode YiTerm connection history")
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).context("Cannot read YiTerm connection history"),
    }
}

fn remember_address(directory: &Path, address: SocketAddr) -> Result<()> {
    // Keep the lock on a separate file so it survives atomic history replacement.
    let lock = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("recent.lock"))?;
    lock.lock()?;
    let path = directory.join("recent.json");
    let mut addresses = load_recent(&path)?;
    if addresses.first() == Some(&address) {
        return Ok(());
    }
    addresses.retain(|saved| *saved != address);
    addresses.insert(0, address);
    addresses.truncate(RECENT_LIMIT);
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(&mut file, &addresses)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn read_saved(path: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
    use std::io::Read;
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "Invalid direct sharing identity file"
    );
    Ok(Some(bytes))
}

fn save_once(path: &Path, bytes: &[u8]) -> Result<bool> {
    let mut file =
        tempfile::NamedTempFile::new_in(path.parent().context("Missing identity directory")?)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    match file.persist_noclobber(path) {
        Ok(_) => Ok(true),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error.error.into()),
    }
}

#[derive(Debug)]
struct ServerIdentity {
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for ServerIdentity {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        // Direct sharing encrypts transport without persisting or pinning host identity.
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::General("YiTerm requires TLS 1.3".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(message, cert, signature, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

pub struct Client {
    pub address: SocketAddr,
    input: mpsc::Sender<Zeroizing<Vec<u8>>>,
    size: watch::Sender<PtySize>,
    stop: Option<oneshot::Sender<StopReason>>,
}

enum StopReason {
    Closed,
    InputQueueFull,
}

type Output = mpsc::Receiver<Vec<u8>>;

impl Client {
    pub fn start(address: SocketAddr, size: PtySize) -> Result<(Self, Output)> {
        ensure!(address.port() != 0, "Enter a port between 1 and 65535");
        let (input, rx) = mpsc::channel(64);
        let (size, sizes) = watch::channel(size);
        let (output, reader) = mpsc::channel(16);
        let (stop, stopped) = oneshot::channel();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        std::thread::Builder::new().name("yiterm-direct-share".into()).spawn(move || {
            runtime.block_on(async {
                let result = tokio::select! {
                    biased;
                    reason = stopped => match reason {
                        Ok(StopReason::InputQueueFull) => Err(anyhow::anyhow!(INPUT_QUEUE_FULL)),
                        _ => return,
                    },
                    result = connect(address, rx, sizes, &output) => result,
                };
                let message = match result {
                    Ok(()) => "YiTerm sharing ended".to_owned(),
                    Err(error) => {
                        tracing::warn!(%address, error = %format!("{error:#}"), "YiTerm connection ended");
                        format!("YiTerm direct connection: {error:#}")
                    }
                };
                let message: String = message.chars().filter(|c| !c.is_control()).collect();
                let bytes = format!("\x18\x18\x1b_Ga=d,d=A,q=2;\x1b\\\x1b[?2026l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1016l\x1b[?1004l\x1b[?2004l\x1b[?25h\x1b[?1049l\r\n{message}\r\nUse Pane > Reconnect to try again.\r\n").into_bytes();
                let _ = output.send(bytes).await;
            });
        })?;
        Ok((
            Self {
                address,
                input,
                size,
                stop: Some(stop),
            },
            reader,
        ))
    }

    pub fn resize(&self, size: PtySize) {
        self.size.send_replace(size);
    }

    pub fn close(&mut self) {
        self.stop(StopReason::Closed);
    }

    fn stop(&mut self, reason: StopReason) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(reason);
        }
    }
}

impl Write for Client {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = bytes.len().min(MAX_INPUT);
        if n == 0 {
            return Ok(0);
        }
        if self.stop.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "YiTerm connection closed",
            ));
        }
        match self.input.try_send(Zeroizing::new(bytes[..n].to_vec())) {
            Ok(()) => Ok(n),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.stop(StopReason::InputQueueFull);
                Err(io::Error::new(io::ErrorKind::WouldBlock, INPUT_QUEUE_FULL))
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "YiTerm connection closed",
            )),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.close();
    }
}

async fn connect(
    address: SocketAddr,
    mut input: mpsc::Receiver<Zeroizing<Vec<u8>>>,
    mut sizes: watch::Receiver<PtySize>,
    output: &mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    let directory = data_dir()?;
    let provider = rustls::crypto::ring::default_provider();
    let verifier = ServerIdentity {
        algorithms: provider.signature_verification_algorithms,
    };
    let mut config = ClientConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    config.alpn_protocols = vec![ALPN.to_vec()];
    let socket = timeout(IO_TIMEOUT, TcpStream::connect(address)).await??;
    socket.set_nodelay(true)?;
    let tls = timeout(
        IO_TIMEOUT,
        tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(ServerName::IpAddress(address.ip().into()), socket),
    )
    .await??;
    let connection = tls.get_ref().1;
    ensure!(
        connection.alpn_protocol() == Some(ALPN),
        "Incompatible YiTerm sharing protocol"
    );
    if let Err(error) = remember_address(&directory, address) {
        tracing::warn!(%error, "Cannot save YiTerm connection history");
    }

    let (mut reader, mut writer) = tokio::io::split(tls);
    let receive = async {
        let mut buffer = [0; MAX_INPUT];
        loop {
            let n = reader.read(&mut buffer).await?;
            if n == 0 || output.send(buffer[..n].to_vec()).await.is_err() {
                return Ok(());
            }
        }
    };
    let send = async {
        let mut heartbeat = interval(HEARTBEAT);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut size = *sizes.borrow_and_update();
        loop {
            let mut frame = [RESIZE; 9];
            for (field, bytes) in [size.cols, size.rows, size.pixel_width, size.pixel_height]
                .into_iter()
                .zip(frame[1..].chunks_exact_mut(2))
            {
                bytes.copy_from_slice(&field.to_be_bytes());
            }
            timeout(IO_TIMEOUT, async {
                writer.write_all(&frame).await?;
                writer.flush().await
            })
            .await??;
            loop {
                tokio::select! {
                    changed = sizes.changed() => {
                        changed?;
                        size = *sizes.borrow_and_update();
                        break;
                    }
                    bytes = input.recv() => {
                        let Some(bytes) = bytes else { return Ok(()); };
                        let [hi, lo] = (bytes.len() as u16).to_be_bytes();
                        timeout(IO_TIMEOUT, async {
                            writer.write_all(&[INPUT, hi, lo]).await?;
                            writer.write_all(&bytes).await?;
                            writer.flush().await
                        }).await??;
                    }
                    _ = heartbeat.tick() => timeout(IO_TIMEOUT, async {
                        writer.write_all(&[PING]).await?;
                        writer.flush().await
                    }).await??,
                }
            }
        }
    };
    tokio::select! { result = receive => result, result = send => result }
}
