//! Process-wide diagnostic logging. Terminal and clipboard contents are excluded.
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    process::Command,
};
use tracing_appender::non_blocking::{ErrorCounter, WorkerGuard};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

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
// Retention is shared across PID-specific files; never delete another live process's logs.
fn prune(directory: &Path, max_files: usize) -> Result<()> {
    let mut files = Vec::new();
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(rest) = name.strip_prefix("yiterm-") else {
            continue;
        };
        let Some((pid, _)) = rest.split_once('.') else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        if !name.ends_with(".log") || system.process(sysinfo::Pid::from_u32(pid)).is_some() {
            continue;
        }
        let metadata = entry.metadata()?;
        if metadata.is_file() {
            files.push((metadata.modified()?, entry.path()));
        }
    }
    files.sort_by_key(|(time, _)| *time);
    let remove = files.len().saturating_sub(max_files.saturating_sub(1));
    for (_, path) in files.into_iter().take(remove) {
        std::fs::remove_file(path)?;
    }
    Ok(())
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
    let directory = config
        .directory
        .clone()
        .map(Ok)
        .unwrap_or_else(default_directory)?;
    let directory = std::path::absolute(directory)?;
    let mut warning = None;
    let mut guard = None;
    let mut counter = None;
    let writer = if config.file {
        let result = (|| -> Result<_> {
            std::fs::create_dir_all(&directory)?;
            prune(&directory, config.max_files)?;
            Ok(tracing_appender::rolling::RollingFileAppender::builder()
                .rotation(tracing_appender::rolling::Rotation::DAILY)
                .filename_prefix(format!("yiterm-{}", std::process::id()))
                .filename_suffix("log")
                .max_log_files(config.max_files)
                .build(&directory)?)
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
        _guard: guard,
    })
}
pub fn open_directory(directory: &Path) -> Result<()> {
    std::fs::create_dir_all(directory).context("Cannot create log directory")?;
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer.exe"
    } else {
        "xdg-open"
    };
    Command::new(program)
        .arg(directory)
        .spawn()
        .context("Cannot open log directory")?;
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
        for day in ["01", "02", "03"] {
            std::fs::write(
                dir.path()
                    .join(format!("yiterm-4294967294.2026-10-{day}.log")),
                "log",
            )
            .unwrap();
        }
        std::fs::write(dir.path().join("keep.txt"), "keep").unwrap();
        prune(dir.path(), 2).unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
        assert!(dir.path().join("keep.txt").exists());
    }
}
