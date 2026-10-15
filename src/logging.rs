//! Process-wide diagnostic logging. Terminal and clipboard contents are excluded.
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{
    fs::OpenOptions,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};
use tracing_appender::non_blocking::{ErrorCounter, NonBlocking, WorkerGuard};
use tracing_subscriber::{
    EnvFilter, fmt::writer::MakeWriter, layer::SubscriberExt, util::SubscriberInitExt,
};

struct ProcessFormat;
impl<S, N> tracing_subscriber::fmt::FormatEvent<S, N> for ProcessFormat
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        write!(writer, "pid={} ", std::process::id())?;
        tracing_subscriber::fmt::format()
            .with_thread_ids(true)
            .format_event(ctx, writer, event)
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    pub filter: String,
    // Accept legacy configurations without enabling console output.
    #[serde(rename = "console")]
    _console: bool,
    pub file: bool,
    pub directory: Option<PathBuf>,
    pub rotation: String,
    pub max_files: usize,
}
impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            filter: "warn,yiterm=info".into(),
            _console: false,
            file: true,
            directory: None,
            rotation: "daily".into(),
            max_files: 14,
        }
    }
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    logging: LoggingConfig,
    #[serde(rename = "cursor")]
    _cursor: crate::settings::CursorSettings,
    #[serde(rename = "appearance")]
    _appearance: crate::settings::AppearanceSettings,
}

pub struct Logging {
    pub directory: PathBuf,
    pub warning: Option<String>,
    counter: Option<ErrorCounter>,
    _guard: Option<WorkerGuard>,
}
impl Drop for Logging {
    fn drop(&mut self) {
        let dropped = self.counter.as_ref().map_or(0, ErrorCounter::dropped_lines);
        tracing::info!(dropped_lines = dropped, "Application logging shutdown");
    }
}
fn project_dirs() -> Result<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "yiterm")
        .context("Cannot locate user configuration directory")
}
pub fn config_path() -> Result<PathBuf> {
    Ok(project_dirs()?.config_dir().join("config.toml"))
}
pub fn default_directory() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        Ok(directories::BaseDirs::new()
            .context("Cannot locate home directory")?
            .home_dir()
            .join("Library/Logs/yiterm"))
    }
    #[cfg(windows)]
    {
        Ok(directories::BaseDirs::new()
            .context("Cannot locate local data directory")?
            .data_local_dir()
            .join("yiterm/logs"))
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
    {
        Ok(project_dirs()?.data_local_dir().join("logs"))
    }
}
fn load(path: &Path, explicit: bool) -> Result<LoggingConfig> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(toml::from_str::<Config>(&text)
            .with_context(|| format!("Invalid configuration: {}", path.display()))?
            .logging),
        Err(error) if !explicit && error.kind() == std::io::ErrorKind::NotFound => {
            Ok(LoggingConfig::default())
        }
        Err(error) => {
            Err(error).with_context(|| format!("Cannot read configuration: {}", path.display()))
        }
    }
}
fn utc_date() -> String {
    let date = time::OffsetDateTime::now_utc().date();
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        date.month() as u8,
        date.day()
    )
}

pub fn current_log_path(directory: &Path) -> PathBuf {
    directory.join(format!("yiterm-{}.log", utc_date()))
}

fn legacy_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("yiterm-")?.strip_suffix(".log")?;
    let (pid, date) = rest.split_once('.')?;
    if date.len() != 10
        || !date.bytes().enumerate().all(|(index, byte)| match index {
            4 | 7 => byte == b'-',
            _ => byte.is_ascii_digit(),
        })
    {
        return None;
    }
    pid.parse().ok()
}

fn is_daily_log(name: &str) -> bool {
    let Some(date) = name
        .strip_prefix("yiterm-")
        .and_then(|name| name.strip_suffix(".log"))
    else {
        return false;
    };
    date.len() == 10
        && date.bytes().enumerate().all(|(index, byte)| match index {
            4 | 7 => byte == b'-',
            _ => byte.is_ascii_digit(),
        })
}

// New logs are shared by every YiTerm process. Old per-PID logs are kept only
// while that process is still alive, so a running older instance can finish.
fn prune(directory: &Path, max_files: usize) -> Result<()> {
    let mut files = Vec::new();
    let current = current_log_path(directory);
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("yiterm-") {
            continue;
        }
        if !name.ends_with(".log") {
            continue;
        }
        if let Some(pid) = legacy_pid(&name)
            && system.process(sysinfo::Pid::from_u32(pid)).is_some()
        {
            continue;
        }
        if !is_daily_log(&name) && legacy_pid(&name).is_none() {
            continue;
        }
        let metadata = entry.metadata()?;
        let path = entry.path();
        if metadata.is_file() && path != current {
            files.push((metadata.modified()?, path));
        }
    }
    files.sort_by_key(|(time, _)| *time);
    let remove = files.len().saturating_sub(max_files.saturating_sub(1));
    for (_, path) in files.into_iter().take(remove) {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// One formatted event is buffered before it enters the non-blocking worker.
/// This lets the shared file writer hold its cross-process lock for the full
/// record, rather than allowing two processes to interleave formatter writes.
#[derive(Clone)]
struct BufferedMakeWriter(NonBlocking);

struct EventBuffer {
    writer: NonBlocking,
    bytes: Vec<u8>,
}

impl Write for EventBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for EventBuffer {
    fn drop(&mut self) {
        if !self.bytes.is_empty() {
            let _ = self.writer.write_all(&self.bytes);
        }
    }
}

impl<'a> MakeWriter<'a> for BufferedMakeWriter {
    type Writer = EventBuffer;

    fn make_writer(&'a self) -> Self::Writer {
        EventBuffer {
            writer: self.0.clone(),
            bytes: Vec::new(),
        }
    }
}

#[derive(Clone)]
struct SharedFileAppender {
    directory: PathBuf,
    thread_lock: Arc<Mutex<()>>,
}

impl SharedFileAppender {
    fn new(directory: PathBuf) -> Result<Self> {
        let appender = Self {
            directory,
            thread_lock: Arc::new(Mutex::new(())),
        };
        // Validate locking synchronously so failures reach the status bar,
        // rather than disappearing in the background logging worker.
        appender.append_record(b"")?;
        Ok(appender)
    }

    fn append_record(&self, bytes: &[u8]) -> io::Result<()> {
        let _thread_lock = self
            .thread_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = current_log_path(&self.directory);
        // Windows cannot lock an append-only handle; read access is required.
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)?;
        file.lock()?;
        let result = file.write_all(bytes);
        let unlock = file.unlock();
        result?;
        unlock
    }
}

impl Write for SharedFileAppender {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.append_record(bytes)?;
        Ok(bytes.len())
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.append_record(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
pub fn init(path: Option<&Path>, override_filter: Option<&str>) -> Result<Logging> {
    let default_path = config_path()?;
    let config = load(path.unwrap_or(&default_path), path.is_some())?;
    if config.rotation != "daily" {
        bail!("logging.rotation must be daily");
    }
    if config.max_files == 0 {
        bail!("logging.max_files must be greater than zero");
    }
    let environment = std::env::var("YITERM_LOG").ok();
    let filter = EnvFilter::try_new(
        override_filter
            .or(environment.as_deref())
            .unwrap_or(&config.filter),
    )
    .context("Invalid logging filter")?;
    let directory = std::env::var_os("YITERM_LOG_DIRECTORY")
        .map(PathBuf::from)
        .or_else(|| config.directory.clone())
        .map(Ok)
        .unwrap_or_else(default_directory)?;
    let directory = std::path::absolute(directory)?;
    let mut warning = None;
    let mut guard = None;
    let mut counter = None;
    let writer = if config.file {
        let result = (|| -> Result<_> {
            std::fs::create_dir_all(&directory)?;
            let appender = SharedFileAppender::new(directory.clone())?;
            prune(&directory, config.max_files)?;
            Ok(appender)
        })();
        match result {
            Ok(appender) => {
                let (writer, worker) =
                    tracing_appender::non_blocking::NonBlockingBuilder::default()
                        .buffered_lines_limit(8192)
                        .lossy(true)
                        .finish(appender);
                counter = Some(writer.error_counter());
                guard = Some(worker);
                Some(writer)
            }
            Err(error) => {
                warning = Some(format!("File logging unavailable: {error:#}"));
                None
            }
        }
    } else {
        None
    };
    let file = writer.map(|writer| {
        tracing_subscriber::fmt::layer()
            .with_writer(BufferedMakeWriter(writer))
            .with_ansi(false)
            .event_format(ProcessFormat)
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(file)
        .try_init()?;
    if let Some(message) = &warning {
        tracing::warn!("{message}");
    }
    tracing::info!(pid = std::process::id(), version = env!("CARGO_PKG_VERSION"),
        build = option_env!("YITERM_BUILD_REV").unwrap_or("local"), directory = %directory.display(), "Application started");
    Ok(Logging {
        directory,
        warning,
        counter,
        _guard: guard,
    })
}
pub fn open_current_file(directory: &Path) -> Result<()> {
    std::fs::create_dir_all(directory).context("Cannot create log directory")?;
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer.exe"
    } else {
        "xdg-open"
    };
    let path = current_log_path(directory);
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("Cannot open log file {}", path.display()))?;
    Command::new(program)
        .arg(&path)
        .spawn()
        .context("Cannot open log file")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_accepts_saved_settings_and_example_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, include_str!("../config.example.toml")).unwrap();
        assert!(load(&path, true).is_ok());
        for theme in [
            crate::theme::ThemeMode::Dark,
            crate::theme::ThemeMode::Light,
        ] {
            crate::settings::save_all(&path, crate::settings::CursorSettings::default(), theme)
                .unwrap();
            assert!(load(&path, true).is_ok());
            assert_eq!(crate::settings::load_theme(&path).unwrap(), theme);
        }
    }

    #[test]
    fn configuration_defaults_and_validation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(load(&path, false).unwrap().filter, "warn,yiterm=info");
        assert!(load(&path, true).is_err());
        std::fs::write(
            &path,
            "[logging]\nfilter = 'warn,yiterm::mouse=trace'\nconsole = false\n",
        )
        .unwrap();
        let config = load(&path, true).unwrap();
        assert!(!config._console);
        assert!(config.file);
        assert!(EnvFilter::try_new(config.filter).is_ok());
        std::fs::write(&path, "[logging]\nmax_files = 'bad'\n").unwrap();
        assert!(load(&path, true).is_err());
    }
    #[test]
    fn retention_preserves_unrelated_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(current_log_path(dir.path()), "current").unwrap();
        for day in ["01", "02", "03"] {
            std::fs::write(dir.path().join(format!("yiterm-2026-10-{day}.log")), "log").unwrap();
        }
        std::fs::write(dir.path().join("keep.txt"), "keep").unwrap();
        prune(dir.path(), 2).unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 3);
        assert!(dir.path().join("keep.txt").exists());
        assert!(current_log_path(dir.path()).exists());
    }

    #[test]
    fn concurrent_writers_keep_complete_records_in_one_daily_file() {
        let dir = tempfile::tempdir().unwrap();
        let writer = SharedFileAppender::new(dir.path().to_path_buf()).unwrap();
        let mut threads = Vec::new();
        for process in 0..4 {
            let mut writer = writer.clone();
            threads.push(std::thread::spawn(move || {
                for record in 0..50 {
                    let line = format!("pid={process} record={record}\n");
                    writer.write_all(line.as_bytes()).unwrap();
                }
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }

        let text = std::fs::read_to_string(current_log_path(dir.path())).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 200);
        assert!(
            lines
                .iter()
                .all(|line| line.starts_with("pid=") && line.contains(" record="))
        );
    }
}
