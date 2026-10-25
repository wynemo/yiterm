use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Beam,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CursorSettings {
    pub shape: CursorShape,
    pub blinking: bool,
}
impl Default for CursorSettings {
    fn default() -> Self {
        Self {
            shape: CursorShape::Block,
            blinking: true,
        }
    }
}

pub fn load(path: &Path) -> Result<CursorSettings> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let value: toml::Value = toml::from_str(&text)?;
            value
                .get("cursor")
                .cloned()
                .map(|v| v.try_into::<CursorSettings>())
                .transpose()
                .map(|v| v.unwrap_or_default())
                .context("Invalid cursor settings")
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(CursorSettings::default()),
        Err(error) => Err(error.into()),
    }
}

pub fn save(path: &Path, cursor: CursorSettings) -> Result<()> {
    save_values(path, cursor, None, None, None, None)
}

fn save_values(
    path: &Path,
    cursor: CursorSettings,
    theme: Option<crate::theme::ThemeMode>,
    font: Option<&FontSettings>,
    browser: Option<BrowserSettings>,
    shell: Option<&ShellSettings>,
) -> Result<()> {
    let mut value = match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str::<toml::Value>(&text)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            toml::Value::Table(Default::default())
        }
        Err(error) => return Err(error.into()),
    };
    value
        .as_table_mut()
        .context("Expected configuration table")?
        .insert("cursor".into(), toml::Value::try_from(cursor)?);
    if let Some(theme) = theme {
        value
            .as_table_mut()
            .context("Expected configuration table")?
            .insert(
                "appearance".into(),
                toml::Value::try_from(AppearanceSettings { theme })?,
            );
    }
    if let Some(font) = font {
        font.validate()?;
        value
            .as_table_mut()
            .context("Expected configuration table")?
            .insert("font".into(), toml::Value::try_from(font)?);
    }
    if let Some(browser) = browser {
        value
            .as_table_mut()
            .context("Expected configuration table")?
            .insert("browser".into(), toml::Value::try_from(browser)?);
    }
    if let Some(shell) = shell {
        shell.validate()?;
        value
            .as_table_mut()
            .context("Expected configuration table")?
            .insert("shell".into(), toml::Value::try_from(shell)?);
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let directory = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    use std::io::Write;
    file.write_all(toml::to_string_pretty(&value)?.as_bytes())?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct AppearanceSettings {
    theme: crate::theme::ThemeMode,
}

pub fn load_theme(path: &Path) -> Result<crate::theme::ThemeMode> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let value: toml::Value = toml::from_str(&text)?;
            Ok(value
                .get("appearance")
                .cloned()
                .map(|v| v.try_into::<AppearanceSettings>())
                .transpose()
                .context("Invalid appearance settings")?
                .unwrap_or_default()
                .theme)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Err(error) => Err(error.into()),
    }
}

pub fn save_all(path: &Path, cursor: CursorSettings, theme: crate::theme::ThemeMode) -> Result<()> {
    save_values(path, cursor, Some(theme), None, None, None)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FontSettings {
    pub family: String,
    pub size: f32,
}
impl Default for FontSettings {
    fn default() -> Self {
        Self {
            family: "Lilex".into(),
            size: 16.0,
        }
    }
}
impl FontSettings {
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.family.trim().is_empty(),
            "Font family cannot be empty"
        );
        anyhow::ensure!(
            self.size.is_finite() && (8.0..=48.0).contains(&self.size),
            "Font size must be between 8 and 48 pixels"
        );
        Ok(())
    }
}
pub fn load_font(path: &Path) -> Result<FontSettings> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(FontSettings::default()),
        Err(e) => return Err(e.into()),
    };
    let value: toml::Value = toml::from_str(&text)?;
    let font = value
        .get("font")
        .cloned()
        .map(|v| v.try_into::<FontSettings>())
        .transpose()
        .context("Invalid font settings")?
        .unwrap_or_default();
    font.validate()?;
    Ok(font)
}
pub fn save_with_font(
    path: &Path,
    cursor: CursorSettings,
    theme: crate::theme::ThemeMode,
    font: &FontSettings,
) -> Result<()> {
    save_values(path, cursor, Some(theme), Some(font), None, None)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrowserSettings {
    pub open_links_in_browser: bool,
}

impl Default for BrowserSettings {
    fn default() -> Self {
        Self {
            open_links_in_browser: true,
        }
    }
}

pub fn load_browser(path: &Path) -> Result<BrowserSettings> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(error) => return Err(error.into()),
    };
    let value: toml::Value = toml::from_str(&text)?;
    value
        .get("browser")
        .cloned()
        .map(|value| value.try_into::<BrowserSettings>())
        .transpose()
        .map(|value| value.unwrap_or_default())
        .context("Invalid browser settings")
}

pub fn save_preferences(
    path: &Path,
    cursor: CursorSettings,
    theme: crate::theme::ThemeMode,
    font: &FontSettings,
    browser: BrowserSettings,
) -> Result<()> {
    save_values(path, cursor, Some(theme), Some(font), Some(browser), None)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellMode {
    #[default]
    Default,
    Program,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShellSettings {
    pub mode: ShellMode,
    pub program: String,
}

pub fn default_shell() -> String {
    if cfg!(windows) {
        "powershell.exe".into()
    } else {
        std::env::var("SHELL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "/bin/sh".into())
    }
}

impl Default for ShellSettings {
    fn default() -> Self {
        Self {
            mode: ShellMode::Default,
            program: default_shell(),
        }
    }
}

impl ShellSettings {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.mode != ShellMode::Program || !self.program.trim().is_empty(),
            "Shell program cannot be empty"
        );
        anyhow::ensure!(
            !self.program.contains('\0'),
            "Shell program contains a null character"
        );
        Ok(())
    }

    pub fn executable(&self, cli: Option<&str>) -> Result<String> {
        if let Some(cli) = cli {
            return Ok(cli.to_owned());
        }
        self.validate()?;
        Ok(match self.mode {
            ShellMode::Default => default_shell(),
            ShellMode::Program => self.program.clone(),
        })
    }
}

pub fn load_shell(path: &Path) -> Result<ShellSettings> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(error) => return Err(error.into()),
    };
    let value: toml::Value = toml::from_str(&text)?;
    let shell = value
        .get("shell")
        .cloned()
        .map(|v| v.try_into::<ShellSettings>())
        .transpose()
        .context("Invalid shell settings")?
        .unwrap_or_default();
    shell.validate()?;
    Ok(shell)
}

pub fn save_terminal_preferences(
    path: &Path,
    cursor: CursorSettings,
    theme: crate::theme::ThemeMode,
    font: &FontSettings,
    browser: BrowserSettings,
    shell: &ShellSettings,
) -> Result<()> {
    save_values(
        path,
        cursor,
        Some(theme),
        Some(font),
        Some(browser),
        Some(shell),
    )
}

/// Discover known shells without executing anything from PATH.
pub fn available_shell_programs() -> Vec<String> {
    let paths =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect::<Vec<_>>();
    // Desktop launchers can provide a smaller PATH than a login shell.
    #[cfg(unix)]
    let paths = paths
        .into_iter()
        .chain(["/bin", "/usr/bin", "/usr/local/bin"].map(std::path::PathBuf::from))
        .collect::<Vec<_>>();
    discover_shell_programs(&paths, cfg!(windows))
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

fn discover_shell_programs(paths: &[std::path::PathBuf], windows: bool) -> Vec<std::path::PathBuf> {
    use std::collections::HashSet;
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    let mut directories = HashSet::new();
    let mut add = |path: std::path::PathBuf| {
        if !path.is_file() {
            return;
        }
        #[cfg(unix)]
        if !windows {
            use std::os::unix::fs::PermissionsExt;
            if !std::fs::metadata(&path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0) {
                return;
            }
        }
        let key = std::fs::canonicalize(&path)
            .unwrap_or_else(|_| path.clone())
            .to_string_lossy()
            .into_owned();
        let key = if windows { key.to_lowercase() } else { key };
        if seen.insert(key) {
            found.push(path);
        }
    };
    for directory in paths {
        let Ok(directory) = std::path::absolute(directory) else {
            continue;
        };
        let key = std::fs::canonicalize(&directory)
            .unwrap_or_else(|_| directory.clone())
            .to_string_lossy()
            .into_owned();
        if !directories.insert(if windows { key.to_lowercase() } else { key }) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        let mut entries = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .collect::<Vec<_>>();
        entries.sort();
        for path in entries {
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let name = if windows {
                name.to_ascii_lowercase()
            } else {
                name.to_owned()
            };
            let name = if windows {
                let Some(name) = name.strip_suffix(".exe") else {
                    continue;
                };
                name
            } else {
                &name
            };
            if [
                "powershell",
                "pwsh",
                "cmd",
                "bash",
                "sh",
                "zsh",
                "fish",
                "nu",
                "elvish",
                "xonsh",
                "wsl",
                "git-bash",
                "mintty",
                "mitty",
                "dash",
                "ksh",
                "tcsh",
            ]
            .contains(&name)
            {
                add(path);
            } else if windows && name == "git" {
                // Git's installer commonly adds only Git/cmd to PATH.
                if let Some(root) = directory.parent() {
                    add(root.join("git-bash.exe"));
                    add(root.join("bin").join("bash.exe"));
                    add(root.join("usr").join("bin").join("mintty.exe"));
                }
            }
        }
    }
    #[cfg(unix)]
    if !windows {
        if let Some(shell) = std::env::var_os("SHELL").filter(|s| !s.is_empty()) {
            add(shell.into());
        }
        if let Ok(shells) = std::fs::read_to_string("/etc/shells") {
            for line in shells.lines() {
                let shell = line.split('#').next().unwrap_or_default().trim();
                if std::path::Path::new(shell).is_absolute() {
                    add(shell.into());
                }
            }
        }
    }
    found
}

/// Git Bash / Mintty are window launchers. Embed their companion Bash in our PTY.
#[cfg(windows)]
pub(crate) fn resolve_windows_shell(program: &str) -> Result<std::path::PathBuf> {
    let path = Path::new(program);
    let name = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    if !matches!(name.as_str(), "git-bash" | "mintty") {
        return Ok(path.to_owned());
    }
    let path = if path.components().count() == 1 {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join(program))
            .find(|p| p.is_file())
            .with_context(|| format!("Cannot find shell launcher: {program}"))?
    } else {
        path.to_owned()
    };
    let parent = path.parent().context("Shell launcher has no directory")?;
    let mut candidates = Vec::new();
    if name == "git-bash" {
        candidates.extend([
            parent.join("bin").join("bash.exe"),
            parent.join("usr").join("bin").join("bash.exe"),
        ]);
    } else {
        // Git/MSYS2: <root>/usr/bin/mintty.exe; Cygwin: <root>/bin/mintty.exe.
        if parent
            .file_name()
            .is_some_and(|s| s.eq_ignore_ascii_case("bin"))
            && let Some(usr) = parent.parent()
            && usr
                .file_name()
                .is_some_and(|s| s.eq_ignore_ascii_case("usr"))
            && let Some(root) = usr.parent()
        {
            candidates.push(root.join("bin").join("bash.exe"));
        }
        candidates.push(parent.join("bash.exe"));
    }
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .with_context(|| {
            format!(
                "Cannot find Bash beside {}; choose bash.exe directly",
                path.display()
            )
        })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilesPanelMode {
    #[default]
    Files,
    Git,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FilesSettings {
    pub mode: FilesPanelMode,
    pub visible: bool,
    pub follow_terminal: bool,
    pub width: f32,
}
impl Default for FilesSettings {
    fn default() -> Self {
        Self {
            mode: FilesPanelMode::Files,
            visible: false,
            follow_terminal: true,
            width: 320.,
        }
    }
}
pub fn load_files(path: &Path) -> Result<FilesSettings> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(error) => return Err(error.into()),
    };
    let table: toml::Value = toml::from_str(&text)?;
    let mut settings = table
        .get("files")
        .cloned()
        .map(|v| v.try_into::<FilesSettings>())
        .transpose()
        .context("Invalid files settings")?
        .unwrap_or_default();
    settings.width = if settings.width.is_finite() {
        settings.width.clamp(240., 800.)
    } else {
        320.
    };
    Ok(settings)
}
pub fn save_files(path: &Path, settings: FilesSettings) -> Result<()> {
    let mut table = match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str::<toml::Value>(&text)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            toml::Value::Table(Default::default())
        }
        Err(error) => return Err(error.into()),
    };
    table
        .as_table_mut()
        .context("Expected configuration table")?
        .insert("files".into(), toml::Value::try_from(settings)?);
    let directory = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(directory)?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    use std::io::Write;
    file.write_all(toml::to_string_pretty(&table)?.as_bytes())?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}
