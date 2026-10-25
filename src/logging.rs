//! Process-wide diagnostic logging. Terminal and clipboard contents are excluded.
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};
use tracing_appender::non_blocking::{ErrorCounter, WorkerGuard};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

mod relay;
pub use relay::child_environment;

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
    #[serde(rename = "font")]
    _font: crate::settings::FontSettings,
    #[serde(rename = "browser")]
    _browser: crate::settings::BrowserSettings,
    #[serde(rename = "shell")]
    _shell: crate::settings::ShellSettings,
    #[serde(rename = "files")]
    _files: crate::settings::FilesSettings,
}

pub struct Logging {
    pub directory: PathBuf,
    pub warning: Option<String>,
    counter: Option<ErrorCounter>,
    _relay: Option<relay::Server>,
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
    directory.join(format!("yiterm.{}.log", utc_date()))
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
    let filter_text = override_filter
        .or(environment.as_deref())
        .unwrap_or(&config.filter);
    let filter = EnvFilter::try_new(filter_text).context("Invalid logging filter")?;
    let directory = std::env::var_os("YITERM_LOG_DIRECTORY")
        .map(PathBuf::from)
        .or_else(|| config.directory.clone())
        .map(Ok)
        .unwrap_or_else(default_directory)?;
    let directory = std::path::absolute(directory)?;
    let mut warning = None;
    let mut guard = None;
    let mut counter = None;
    let mut relay_server = None;
    let writer = if config.file || std::env::var_os("YITERM_LOG_CHILD").is_some() {
        let result = (|| -> Result<Option<Box<dyn Write + Send>>> {
            if std::env::var_os("YITERM_LOG_CHILD").is_some() {
                return relay::client_from_environment()
                    .map(|client| client.map(|client| Box::new(client) as Box<dyn Write + Send>));
            }
            std::fs::create_dir_all(&directory)?;
            let appender = tracing_appender::rolling::RollingFileAppender::builder()
                .rotation(tracing_appender::rolling::Rotation::DAILY)
                .filename_prefix("yiterm")
                .filename_suffix("log")
                .max_log_files(config.max_files)
                .build(&directory)?;
            Ok(Some(Box::new(appender)))
        })();
        match result {
            Ok(Some(appender)) => {
                let (writer, worker) =
                    tracing_appender::non_blocking::NonBlockingBuilder::default()
                        .buffered_lines_limit(8192)
                        .lossy(true)
                        .finish(appender);
                counter = Some(writer.error_counter());
                guard = Some(worker);
                if std::env::var_os("YITERM_LOG_CHILD").is_none() {
                    match relay::Server::start(writer.clone(), &directory, filter_text) {
                        Ok(server) => relay_server = Some(server),
                        Err(error) => {
                            warning = Some(format!("SSH log relay unavailable: {error:#}"))
                        }
                    }
                }
                Some(writer)
            }
            Ok(None) => None,
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
            .with_writer(writer)
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
        _relay: relay_server,
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
