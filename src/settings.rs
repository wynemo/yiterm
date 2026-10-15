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
    save_values(path, cursor, None)
}

fn save_values(
    path: &Path,
    cursor: CursorSettings,
    theme: Option<crate::theme::ThemeMode>,
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
    save_values(path, cursor, Some(theme))
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
