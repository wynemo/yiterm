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
    save_values(path, cursor, None, None, None)
}

fn save_values(
    path: &Path,
    cursor: CursorSettings,
    theme: Option<crate::theme::ThemeMode>,
    font: Option<&FontSettings>,
    browser: Option<BrowserSettings>,
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
    save_values(path, cursor, Some(theme), None, None)
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
    save_values(path, cursor, Some(theme), Some(font), None)
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
    save_values(path, cursor, Some(theme), Some(font), Some(browser))
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
