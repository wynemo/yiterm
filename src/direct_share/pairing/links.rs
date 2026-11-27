//! Private, leased exports of the links currently shown by running YiTerm hosts.
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::Read,
    path::PathBuf,
};

#[derive(serde::Serialize, serde::Deserialize)]
struct Record {
    port: u16,
    url: String,
}

fn directory() -> Result<PathBuf> {
    let path = directories::ProjectDirs::from("", "", "yiterm")
        .context("Cannot locate YiTerm data directory")?
        .data_local_dir()
        .join("sharing-links");
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&path)?;
    Ok(path)
}

pub(super) struct PublishedLink {
    lease: Option<File>,
    record: PathBuf,
    lock: PathBuf,
}
impl PublishedLink {
    pub(super) fn new(port: u16, url: &str) -> Result<Self> {
        let directory = directory()?;
        let id = uuid::Uuid::new_v4().to_string();
        let lock = directory.join(format!("{id}.lock"));
        let record = directory.join(format!("{id}.json"));
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lease = options.open(&lock)?;
        let published = Self {
            lease: Some(lease),
            record,
            lock,
        };
        published.lease.as_ref().unwrap().lock()?;
        // NamedTempFile is private and atomic publication never exposes partial links.
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer(
            &mut temporary,
            &Record {
                port,
                url: url.to_owned(),
            },
        )?;
        temporary
            .persist(&published.record)
            .map_err(|error| error.error)?;
        Ok(published)
    }
}
impl Drop for PublishedLink {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.record);
        drop(self.lease.take());
        let _ = fs::remove_file(&self.lock);
    }
}

pub(super) fn current(port: Option<u16>) -> Result<String> {
    let mut records = Vec::new();
    for entry in fs::read_dir(directory()?)? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let lock = path.with_extension("lock");
        let Ok(lease) = OpenOptions::new().read(true).write(true).open(&lock) else {
            continue;
        };
        match lease.try_lock() {
            Err(TryLockError::WouldBlock) => {}
            // A crashed GUI releases its lease even if it left a record behind.
            Ok(()) => {
                let _ = fs::remove_file(&path);
                let _ = fs::remove_file(&lock);
                continue;
            }
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        let Ok(file) = File::open(&path) else {
            continue;
        };
        let mut bytes = Vec::new();
        file.take(65537).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 65536, "Invalid sharing link record");
        let record: Record = serde_json::from_slice(&bytes)?;
        if port.is_some_and(|port| port != record.port) {
            continue;
        }
        let url = url::Url::parse(&record.url)?;
        ensure!(
            url.scheme() == "tenimon"
                && url.host_str() == Some("yiterm")
                && url.path() == "/connect",
            "Invalid sharing link record"
        );
        records.push(record);
    }
    match records.len() {
        0 => bail!(
            "No active connection link. Start screen sharing in YiTerm and wait for its QR code, then run this command again."
        ),
        1 => Ok(records.remove(0).url),
        _ => {
            records.sort_by_key(|record| record.port);
            let ports = records
                .iter()
                .map(|record| record.port.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "Multiple sharing sessions are active (ports: {ports}). Select one with --share-link --share-link-port PORT."
            )
        }
    }
}
