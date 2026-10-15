//! Local IPC only. The GUI's tracing-appender owns all file IO and rotation.
use anyhow::{Context, Result, ensure};
use std::{
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use tracing_appender::non_blocking::NonBlocking;

const TIMEOUT: Duration = Duration::from_secs(2);
const MAX_RECORD: usize = 64 * 1024;
static DESTINATION: OnceLock<Vec<(&'static str, String)>> = OnceLock::new();

/// Applied to the terminal shell so typed SSH shims inherit the GUI's logger.
pub fn child_environment() -> Vec<(&'static str, String)> {
    let mut environment = vec![("YITERM_LOG_CHILD", "1".into())];
    if let Some(destination) = DESTINATION.get() {
        environment.extend(destination.clone());
    }
    environment
}

pub(super) struct Server {
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Server {
    pub(super) fn start(mut writer: NonBlocking, directory: &Path, filter: &str) -> Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let token = uuid::Uuid::new_v4().to_string();
        let environment = vec![
            ("YITERM_LOG_RELAY", address.to_string()),
            ("YITERM_LOG_TOKEN", token.clone()),
            (
                "YITERM_LOG_DIRECTORY",
                directory.to_string_lossy().into_owned(),
            ),
            ("YITERM_LOG", filter.to_owned()),
        ];
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = thread::Builder::new()
            .name("yiterm-log-relay".into())
            .spawn(move || {
                while !stopped.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            if let Err(error) = receive(&mut stream, token.as_bytes(), &mut writer)
                            {
                                tracing::warn!(%error, "SSH log relay rejected a record");
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => {
                            tracing::error!(%error, "SSH log relay stopped");
                            break;
                        }
                    }
                }
            })?;
        let _ = DESTINATION.set(environment);
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn receive(stream: &mut TcpStream, token: &[u8], writer: &mut NonBlocking) -> Result<()> {
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let mut supplied_token = vec![0; token.len()];
    stream.read_exact(&mut supplied_token)?;
    ensure!(supplied_token == token, "Invalid log relay token");
    let mut size = [0; 4];
    stream.read_exact(&mut size)?;
    let size = u32::from_le_bytes(size) as usize;
    ensure!(size <= MAX_RECORD, "Log record exceeds 64 KiB");
    let mut record = vec![0; size];
    stream.read_exact(&mut record)?;
    writer.write_all(&record)?;
    stream.write_all(&[0])?;
    Ok(())
}

pub(super) struct Client {
    address: SocketAddr,
    token: String,
}

pub(super) fn client_from_environment() -> Result<Option<Client>> {
    let Ok(address) = std::env::var("YITERM_LOG_RELAY") else {
        // File logging is disabled in the GUI; the shim must not create a file.
        return Ok(None);
    };
    let address: SocketAddr = address.parse().context("Invalid log relay address")?;
    ensure!(address.ip().is_loopback(), "Log relay must be on loopback");
    let token = std::env::var("YITERM_LOG_TOKEN").context("Missing log relay token")?;
    let mut client = Client { address, token };
    client.write(b"").context("Connecting to GUI log relay")?;
    Ok(Some(client))
}

impl Write for Client {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_RECORD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Log record exceeds 64 KiB",
            ));
        }
        let mut stream = TcpStream::connect_timeout(&self.address, TIMEOUT)?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        stream.write_all(self.token.as_bytes())?;
        stream.write_all(&(bytes.len() as u32).to_le_bytes())?;
        stream.write_all(bytes)?;
        let mut acknowledgment = [1];
        stream.read_exact(&mut acknowledgment)?;
        if acknowledgment != [0] {
            return Err(io::Error::other("Log relay rejected record"));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
