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
