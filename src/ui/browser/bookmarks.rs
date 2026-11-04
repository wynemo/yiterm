use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use std::{io::Write, path::PathBuf};

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Bookmark {
    pub title: String,
    pub url: String,
}

fn path() -> anyhow::Result<PathBuf> {
    Ok(directories::BaseDirs::new()
        .context("Cannot locate local browser data directory")?
        .data_local_dir()
        .join("yiterm/browser/bookmarks.json"))
}

pub(super) fn load() -> anyhow::Result<Vec<Bookmark>> {
    let path = path()?;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("Cannot read bookmarks"),
    };
    let mut bookmarks: Vec<Bookmark> =
        serde_json::from_slice(&bytes).context("Cannot decode bookmarks")?;
    bookmarks.retain(|bookmark| crate::browser_url(&bookmark.url).is_some());
    Ok(bookmarks)
}

pub(super) fn save(bookmarks: &[Bookmark]) -> anyhow::Result<()> {
    let path = path()?;
    let directory = path.parent().context("Cannot locate bookmarks directory")?;
    std::fs::create_dir_all(directory)?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(&serde_json::to_vec_pretty(bookmarks)?)?;
    file.as_file().sync_all()?;
    file.persist(&path).map_err(|error| error.error)?;
    Ok(())
}
