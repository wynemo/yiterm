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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cursor_roundtrip_preserves_logging() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(load(&path).unwrap(), CursorSettings::default());
        std::fs::write(&path, "[logging]\nfilter = 'debug'\n").unwrap();
        for shape in [
            CursorShape::Block,
            CursorShape::Underline,
            CursorShape::Beam,
        ] {
            let settings = CursorSettings {
                shape,
                blinking: false,
            };
            save(&path, settings).unwrap();
            assert_eq!(load(&path).unwrap(), settings);
            let value: toml::Value =
                toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(value["logging"]["filter"].as_str(), Some("debug"));
        }
    }
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

#[cfg(test)]
mod appearance_tests {
    use super::*;
    use crate::theme::ThemeMode;

    #[test]
    fn theme_roundtrip_preserves_cursor_and_other_sections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(load_theme(&path).unwrap(), ThemeMode::Dark);
        std::fs::write(
            &path,
            "[logging]\nfilter = 'debug'\n[cursor]\nshape = 'beam'\n",
        )
        .unwrap();
        assert_eq!(load_theme(&path).unwrap(), ThemeMode::Dark);
        let cursor = load(&path).unwrap();
        for theme in [ThemeMode::Light, ThemeMode::Dark] {
            save_all(&path, cursor, theme).unwrap();
            assert_eq!(load_theme(&path).unwrap(), theme);
            assert_eq!(load(&path).unwrap(), cursor);
            let value: toml::Value =
                toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(value["logging"]["filter"].as_str(), Some("debug"));
        }
        std::fs::write(&path, "[appearance]\ntheme = 'unknown'\n").unwrap();
        assert!(load_theme(&path).is_err());
    }
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
#[cfg(test)]
mod font_tests {
    use super::*;
    #[test]
    fn font_roundtrip_and_validation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(load_font(&path).unwrap(), FontSettings::default());
        std::fs::write(&path, "[logging]\nfilter = 'debug'\n").unwrap();
        let font = FontSettings {
            family: "Menlo".into(),
            size: 22.0,
        };
        save_with_font(&path, CursorSettings::default(), Default::default(), &font).unwrap();
        assert_eq!(load_font(&path).unwrap(), font);
        save_all(&path, CursorSettings::default(), Default::default()).unwrap();
        assert_eq!(load_font(&path).unwrap(), font);
        let value: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["logging"]["filter"].as_str(), Some("debug"));
        for size in [0.0, 49.0, f32::NAN, f32::INFINITY] {
            assert!(
                FontSettings {
                    size,
                    ..font.clone()
                }
                .validate()
                .is_err()
            );
        }
        std::fs::write(&path, "[font]\nsize = 100\n").unwrap();
        assert!(load_font(&path).is_err());
    }
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

#[cfg(test)]
mod browser_tests {
    use super::*;

    #[test]
    fn browser_defaults_and_roundtrip_preserve_other_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(load_browser(&path).unwrap().open_links_in_browser);
        std::fs::write(&path, "[logging]\nfilter = 'debug'\n").unwrap();
        assert_eq!(load_browser(&path).unwrap(), BrowserSettings::default());
        for enabled in [false, true] {
            let browser = BrowserSettings {
                open_links_in_browser: enabled,
            };
            save_preferences(
                &path,
                CursorSettings::default(),
                Default::default(),
                &FontSettings::default(),
                browser,
            )
            .unwrap();
            assert_eq!(load_browser(&path).unwrap(), browser);
            save_with_font(
                &path,
                CursorSettings::default(),
                Default::default(),
                &FontSettings::default(),
            )
            .unwrap();
            assert_eq!(load_browser(&path).unwrap(), browser);
            let value: toml::Value =
                toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(value["logging"]["filter"].as_str(), Some("debug"));
        }
        std::fs::write(&path, "[browser]\nopen_links_in_browser = 'invalid'\n").unwrap();
        assert!(load_browser(&path).is_err());
    }
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

#[cfg(test)]
mod shell_tests {
    use super::*;

    #[test]
    fn shell_roundtrip_preserves_preferences_and_rejects_invalid_programs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(load_shell(&path).unwrap().mode, ShellMode::Default);
        std::fs::write(&path, "[logging]\nfilter = 'debug'\n").unwrap();
        let shell = ShellSettings {
            mode: ShellMode::Program,
            program: "C:\\Program Files\\PowerShell\\7\\pwsh.exe".into(),
        };
        save_terminal_preferences(
            &path,
            CursorSettings::default(),
            Default::default(),
            &FontSettings::default(),
            BrowserSettings::default(),
            &shell,
        )
        .unwrap();
        assert_eq!(load_shell(&path).unwrap(), shell);
        assert_eq!(shell.executable(None).unwrap(), shell.program);
        assert_eq!(shell.executable(Some("/bin/sh")).unwrap(), "/bin/sh");
        save_all(&path, CursorSettings::default(), Default::default()).unwrap();
        assert_eq!(load_shell(&path).unwrap(), shell);
        let value: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["logging"]["filter"].as_str(), Some("debug"));
        for program in ["", "   ", "bad\0program"] {
            let invalid = ShellSettings {
                program: program.into(),
                ..shell.clone()
            };
            assert!(
                save_terminal_preferences(
                    &path,
                    CursorSettings::default(),
                    Default::default(),
                    &FontSettings::default(),
                    BrowserSettings::default(),
                    &invalid
                )
                .is_err()
            );
            assert_eq!(load_shell(&path).unwrap(), shell);
        }
        std::fs::write(&path, "[shell]\nmode = 'program'\nprogram = ''\n").unwrap();
        assert!(load_shell(&path).is_err());
        std::fs::write(&path, "[shell]\nmode = 'unknown'\n").unwrap();
        assert!(load_shell(&path).is_err());
    }
}

/// Discover known shells without executing anything from PATH.
pub fn available_shell_programs() -> Vec<String> {
    let paths =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect::<Vec<_>>();
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
                    add(root.join("bin/bash.exe"));
                    add(root.join("usr/bin/mintty.exe"));
                }
            }
        }
    }
    found
}

/// Git Bash / Mintty are window launchers. Embed their companion Bash in our PTY.
/// Kept platform independent so the Windows installation layouts can be tested on macOS.
#[cfg(any(windows, test))]
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
        candidates.extend([parent.join("bin/bash.exe"), parent.join("usr/bin/bash.exe")]);
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
            candidates.push(root.join("bin/bash.exe"));
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

#[cfg(test)]
mod shell_discovery_tests {
    use super::*;

    #[test]
    fn windows_path_discovery_keeps_distinct_installs_and_ignores_non_shells() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("First Install");
        let second = dir.path().join("Second Install");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        for name in [
            "PWSH.EXE",
            "git-bash.exe",
            "mintty.exe",
            "unrelated.exe",
            "bash.cmd",
        ] {
            std::fs::write(first.join(name), "").unwrap();
        }
        std::fs::create_dir(first.join("cmd.exe")).unwrap();
        std::fs::write(second.join("PWSH.EXE"), "").unwrap();
        let paths = vec![
            first.clone(),
            dir.path().join("missing"),
            first.clone(),
            second.clone(),
        ];
        let found = discover_shell_programs(&paths, true);
        assert_eq!(
            found,
            vec![
                first.join("PWSH.EXE"),
                first.join("git-bash.exe"),
                first.join("mintty.exe"),
                second.join("PWSH.EXE")
            ]
        );
    }

    #[test]
    fn git_cmd_on_path_discovers_git_bash_and_mintty() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Git With Spaces");
        for subdir in ["cmd", "bin", "usr/bin"] {
            std::fs::create_dir_all(root.join(subdir)).unwrap();
        }
        for file in [
            "cmd/git.exe",
            "git-bash.exe",
            "bin/bash.exe",
            "usr/bin/mintty.exe",
        ] {
            std::fs::write(root.join(file), "").unwrap();
        }
        let found = discover_shell_programs(
            &[root.join("cmd"), root.join("bin"), root.join("usr/bin")],
            true,
        );
        assert_eq!(
            found,
            vec![
                root.join("git-bash.exe"),
                root.join("bin/bash.exe"),
                root.join("usr/bin/mintty.exe")
            ]
        );
    }

    #[test]
    fn windows_launchers_resolve_to_companion_bash_without_opening_another_window() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Git With Spaces");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::create_dir_all(root.join("usr/bin")).unwrap();
        let git = root.join("git-bash.exe");
        let mintty = root.join("usr/bin/mintty.exe");
        let bash = root.join("bin/bash.exe");
        std::fs::write(&git, "").unwrap();
        std::fs::write(&mintty, "").unwrap();
        assert!(resolve_windows_shell(git.to_str().unwrap()).is_err());
        assert!(resolve_windows_shell(mintty.to_str().unwrap()).is_err());
        std::fs::write(&bash, "").unwrap();
        for launcher in [&git, &mintty] {
            assert_eq!(
                resolve_windows_shell(launcher.to_str().unwrap()).unwrap(),
                bash
            );
        }
        std::fs::remove_file(&bash).unwrap();
        let msys_bash = root.join("usr/bin/bash.exe");
        std::fs::write(&msys_bash, "").unwrap();
        for launcher in [&git, &mintty] {
            assert_eq!(
                resolve_windows_shell(launcher.to_str().unwrap()).unwrap(),
                msys_bash
            );
        }
        let cygwin = dir.path().join("cygwin/bin");
        std::fs::create_dir_all(&cygwin).unwrap();
        std::fs::write(cygwin.join("bash.exe"), "").unwrap();
        assert_eq!(
            resolve_windows_shell(cygwin.join("mintty.exe").to_str().unwrap()).unwrap(),
            cygwin.join("bash.exe")
        );
        assert_eq!(
            resolve_windows_shell("pwsh.exe").unwrap(),
            std::path::PathBuf::from("pwsh.exe")
        );
    }
}

#[cfg(all(test, windows))]
mod windows_shell_pty_tests {
    use super::*;
    use std::{
        io::{Read, Write},
        sync::mpsc,
        time::Duration,
    };

    #[test]
    fn git_bash_and_mintty_run_inside_pty() {
        let shells = available_shell_programs();
        let integration = crate::ssh::Integration::new(&std::env::current_exe().unwrap()).unwrap();
        for name in ["git-bash.exe", "mintty.exe"] {
            let Some(launcher) = shells.iter().find(|path| {
                Path::new(path)
                    .file_name()
                    .is_some_and(|file| file.eq_ignore_ascii_case(name))
            }) else {
                assert!(
                    std::env::var_os("GITHUB_ACTIONS").is_none(),
                    "Windows CI requires Git for Windows ({name})"
                );
                eprintln!("Skipping {name} PTY test: Git for Windows is not installed");
                continue;
            };
            let pair = portable_pty::native_pty_system()
                .openpty(portable_pty::PtySize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .unwrap();
            let command = integration.shell(launcher).unwrap();
            assert_eq!(
                Path::new(&command.get_argv()[0]).file_name().unwrap(),
                "bash.exe"
            );
            let mut child = pair.slave.spawn_command(command).unwrap();
            drop(pair.slave);
            let mut reader = pair.master.try_clone_reader().unwrap();
            let mut writer = pair.master.take_writer().unwrap();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let mut output = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => output.extend_from_slice(&buffer[..n]),
                    }
                    if String::from_utf8_lossy(&output).contains("yiterm-git-shell-ok") {
                        break;
                    }
                }
                let _ = tx.send(output);
            });
            // Split the marker so terminal input echo cannot satisfy the assertion.
            writer
                .write_all(b"printf 'yiterm-%s' 'git-shell-ok'; exit\n")
                .unwrap();
            let output = rx.recv_timeout(Duration::from_secs(20));
            let _ = child.kill();
            let _ = child.wait();
            let output =
                output.unwrap_or_else(|error| panic!("{name} produced no PTY output: {error}"));
            assert!(
                String::from_utf8_lossy(&output).contains("yiterm-git-shell-ok"),
                "{name}: {}",
                String::from_utf8_lossy(&output)
            );
        }
    }
}
