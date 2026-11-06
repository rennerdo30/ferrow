//! User settings, stored as JSON next to the index
//! (`%LOCALAPPDATA%\ferrow\settings.json` on Windows).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeChoice,
    /// Folders to index. Empty means "not chosen yet".
    pub roots: Vec<PathBuf>,
    /// Folders and files left out of the index.
    pub excludes: Vec<PathBuf>,
    pub follow_symlinks: bool,
}

pub fn path() -> Option<PathBuf> {
    Some(crate::engine::default_index_path()?.with_file_name("settings.json"))
}

impl Settings {
    /// Load the settings; a missing or unreadable file gives defaults.
    pub fn load() -> Settings {
        path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Save atomically: write a temporary file, then rename it over the
    /// old one, so a crash never leaves a half-written file.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = path() else {
            return Ok(());
        };
        save_to(self, &path)
    }
}

fn save_to(settings: &Settings, path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(settings).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_defaults() {
        let dir = std::env::temp_dir().join(format!("ferrow-settings-{}", std::process::id()));
        let file = dir.join("settings.json");
        let s = Settings {
            theme: ThemeChoice::Dark,
            roots: vec![PathBuf::from("C:/data")],
            excludes: vec![PathBuf::from("C:/data/tmp")],
            follow_symlinks: true,
        };
        save_to(&s, &file).unwrap();
        let back: Settings = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(back, s);
        // Unknown or missing fields fall back to defaults.
        let partial: Settings = serde_json::from_str(r#"{"theme":"light","future":1}"#).unwrap();
        assert_eq!(partial.theme, ThemeChoice::Light);
        assert!(partial.roots.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
