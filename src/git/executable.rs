//! Discover once per terminal session, on the serialized Git worker.
use super::*;
use std::sync::Mutex;

type Cached = Option<(u64, Result<PathBuf, String>)>;

pub struct Session {
    shell: String,
    revision: AtomicU64,
    cached: Mutex<Cached>,
}

impl Session {
    pub fn new(shell: String) -> Self {
        Self {
            shell,
            revision: AtomicU64::new(0),
            cached: Mutex::new(None),
        }
    }

    pub fn invalidate(&self) {
        self.revision.fetch_add(1, Ordering::AcqRel);
    }

    pub fn resolve(
        &self,
        folder: &Path,
        generation: &AtomicU64,
        expected: u64,
    ) -> Result<PathBuf, String> {
        let revision = self.revision.load(Ordering::Acquire);
        if let Some((cached_revision, result)) = &*self.cached.lock().unwrap() {
            if *cached_revision == revision {
                return result.clone();
            }
        }
        let result = discover(&self.shell, folder, generation, expected);
        if let Ok(path) = &result {
            tracing::info!(shell = %self.shell, git = %path.display(), "Resolved session Git executable");
        }
        // Cancellation is transient, and must not poison the session's cache.
        if generation.load(Ordering::Acquire) == expected {
            *self.cached.lock().unwrap() = Some((revision, result.clone()));
        }
        result
    }
}

fn discover(
    shell: &str,
    folder: &Path,
    generation: &AtomicU64,
    expected: u64,
) -> Result<PathBuf, String> {
    if generation.load(Ordering::Acquire) != expected {
        return Err("Cancelled".into());
    }
    let mut command = Command::new(shell);
    command.current_dir(folder).args(["-ic", "which git"]);
    let (ok, output, error) = capture(
        command,
        64 * 1024,
        generation,
        expected,
        Duration::from_secs(5),
    )
    .map_err(|e| format!("Cannot detect Git using {shell}: {e}"))?;
    if !ok {
        let detail = if error.is_empty() {
            String::from_utf8_lossy(&output)
                .trim()
                .chars()
                .take(4096)
                .collect()
        } else {
            error
        };
        return Err(format!("Cannot detect Git using {shell}: {detail}"));
    }
    // `which` may return an executable path, or (e.g. zsh/tcsh) describe
    // an alias. Extract only the path; never execute an alias body.
    for line in output.split(|b| *b == b'\n') {
        let line = line.trim_ascii();
        let path = if let Some(alias) = line.strip_prefix(b"git:") {
            alias
                .trim_ascii_start()
                .strip_prefix(b"aliased to ")
                .map(<[u8]>::trim_ascii)
        } else if line.starts_with(b"/") {
            Some(line)
        } else {
            None
        };
        let Some(mut path) = path else { continue };
        if path.len() >= 2
            && matches!(
                (path[0], path[path.len() - 1]),
                (b'\'', b'\'') | (b'"', b'"')
            )
        {
            path = &path[1..path.len() - 1];
        }
        let path = path_from_bytes(path)?;
        use std::os::unix::fs::PermissionsExt;
        if path.is_absolute()
            && fs::metadata(&path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        {
            return Ok(path);
        }
        return Err(format!(
            "which git returned an unavailable executable: {}",
            path.display()
        ));
    }
    Err(format!(
        "which git did not return an executable path using {shell}."
    ))
}
