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

/// What kind of location an index source is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// A whole local drive or volume, e.g. `D:\`.
    Drive,
    #[default]
    Folder,
    /// A mapped network drive, e.g. `Z:\`.
    NetworkDrive,
    /// A network share or folder, e.g. `\\server\share\docs`.
    NetworkFolder,
}

impl SourceKind {
    pub fn label(self) -> &'static str {
        match self {
            SourceKind::Drive => "Drive",
            SourceKind::Folder => "Folder",
            SourceKind::NetworkDrive => "Network drive",
            SourceKind::NetworkFolder => "Network folder",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub kind: SourceKind,
    pub path: PathBuf,
}

/// Automatic rebuilds. Off by default: nothing runs while idle unless the
/// user asks for it (SPEC §3: 0 % idle CPU).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoRebuild {
    #[default]
    Never,
    OnStart,
    Hourly,
    Daily,
}

impl AutoRebuild {
    pub const ALL: [AutoRebuild; 4] = [
        AutoRebuild::Never,
        AutoRebuild::OnStart,
        AutoRebuild::Hourly,
        AutoRebuild::Daily,
    ];

    pub fn label(self) -> &'static str {
        match self {
            AutoRebuild::Never => "Never",
            AutoRebuild::OnStart => "When Ferrow starts",
            AutoRebuild::Hourly => "Every hour",
            AutoRebuild::Daily => "Every day",
        }
    }

    pub fn interval_secs(self) -> Option<u64> {
        match self {
            AutoRebuild::Hourly => Some(3600),
            AutoRebuild::Daily => Some(86_400),
            _ => None,
        }
    }
}

/// A result-list column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnId {
    Name,
    Folder,
    Size,
    Modified,
    Created,
    Accessed,
    Type,
    Attributes,
}

impl ColumnId {
    pub const ALL: [ColumnId; 8] = [
        ColumnId::Name,
        ColumnId::Folder,
        ColumnId::Size,
        ColumnId::Modified,
        ColumnId::Created,
        ColumnId::Accessed,
        ColumnId::Type,
        ColumnId::Attributes,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ColumnId::Name => "Name",
            ColumnId::Folder => "Folder",
            ColumnId::Size => "Size",
            ColumnId::Modified => "Date modified",
            ColumnId::Created => "Date created",
            ColumnId::Accessed => "Date accessed",
            ColumnId::Type => "Type",
            ColumnId::Attributes => "Attributes",
        }
    }

    pub fn default_width(self) -> f32 {
        match self {
            ColumnId::Name => 320.0,
            ColumnId::Folder => 0.0, // flexible: takes the remaining width
            ColumnId::Size => 90.0,
            ColumnId::Modified | ColumnId::Created | ColumnId::Accessed => 150.0,
            ColumnId::Type => 110.0,
            ColumnId::Attributes => 90.0,
        }
    }

    /// The sort key, for sortable columns.
    pub fn sort_key(self) -> Option<SortChoice> {
        match self {
            ColumnId::Name => Some(SortChoice::Name),
            ColumnId::Size => Some(SortChoice::Size),
            ColumnId::Modified => Some(SortChoice::Modified),
            ColumnId::Created => Some(SortChoice::Created),
            ColumnId::Accessed => Some(SortChoice::Accessed),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColumnConfig {
    pub id: ColumnId,
    pub visible: bool,
    pub width: f32,
}

pub fn default_columns() -> Vec<ColumnConfig> {
    ColumnId::ALL
        .iter()
        .map(|&id| ColumnConfig {
            id,
            visible: matches!(
                id,
                ColumnId::Name
                    | ColumnId::Folder
                    | ColumnId::Size
                    | ColumnId::Modified
                    | ColumnId::Type
            ),
            width: id.default_width(),
        })
        .collect()
}

/// Sort order of the result list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortChoice {
    #[default]
    Name,
    Size,
    Modified,
    Created,
    Accessed,
}

impl SortChoice {
    pub fn key(self) -> ferrow_query::SortKey {
        use ferrow_query::SortKey;
        match self {
            SortChoice::Name => SortKey::Name,
            SortChoice::Size => SortKey::Size,
            SortChoice::Modified => SortKey::Modified,
            SortChoice::Created => SortKey::Created,
            SortChoice::Accessed => SortKey::Accessed,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SortChoice::Name => "name",
            SortChoice::Size => "size",
            SortChoice::Modified => "date modified",
            SortChoice::Created => "date created",
            SortChoice::Accessed => "date accessed",
        }
    }
}

/// Windows-only options.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowsSettings {
    /// Index every fixed (internal) drive, including drives added later.
    pub all_fixed_drives: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeChoice,
    /// Locations to index.
    pub sources: Vec<Source>,
    /// Older settings files stored plain folder paths here.
    #[serde(skip_serializing)]
    roots: Vec<PathBuf>,
    /// Folders and files left out of the index.
    pub excludes: Vec<PathBuf>,
    pub follow_symlinks: bool,
    pub auto_rebuild: AutoRebuild,
    /// Unix seconds of the last completed rebuild started by the app.
    pub last_rebuild: Option<u64>,
    pub windows: WindowsSettings,
    /// Result-list columns, in display order.
    pub columns: Vec<ColumnConfig>,
    pub sort: SortChoice,
    pub sort_descending: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            theme: ThemeChoice::default(),
            sources: Vec::new(),
            roots: Vec::new(),
            excludes: Vec::new(),
            follow_symlinks: false,
            auto_rebuild: AutoRebuild::default(),
            last_rebuild: None,
            windows: WindowsSettings::default(),
            columns: default_columns(),
            sort: SortChoice::default(),
            sort_descending: false,
        }
    }
}

pub fn path() -> Option<PathBuf> {
    Some(crate::engine::default_index_path()?.with_file_name("settings.json"))
}

impl Settings {
    /// Load the settings; a missing or unreadable file gives defaults.
    pub fn load() -> Settings {
        let mut s: Settings = path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        s.migrate();
        s
    }

    fn migrate(&mut self) {
        // Keep every known column exactly once, appending new ones.
        let mut seen = Vec::new();
        self.columns.retain(|c| {
            let new = !seen.contains(&c.id);
            seen.push(c.id);
            new
        });
        for d in default_columns() {
            if !self.columns.iter().any(|c| c.id == d.id) {
                self.columns.push(d);
            }
        }
        if let Some(name) = self.columns.iter_mut().find(|c| c.id == ColumnId::Name) {
            name.visible = true;
        }
        for root in std::mem::take(&mut self.roots) {
            if !self.sources.iter().any(|s| s.path == root) {
                let kind = crate::platform::classify_location(&root);
                self.sources.push(Source { kind, path: root });
            }
        }
    }

    /// Save atomically: write a temporary file, then rename it over the
    /// old one, so a crash never leaves a half-written file.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = path() else {
            return Ok(());
        };
        save_to(self, &path)
    }

    /// Add a location unless it is already listed. Returns true if added.
    pub fn add_source(&mut self, path: PathBuf) -> bool {
        if self.sources.iter().any(|s| s.path == path) {
            return false;
        }
        let kind = crate::platform::classify_location(&path);
        self.sources.push(Source { kind, path });
        true
    }

    /// The folders a rebuild walks: every listed location, plus every
    /// fixed drive when that Windows option is on.
    pub fn scan_roots(&self, fixed_drives: &[PathBuf]) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = self.sources.iter().map(|s| s.path.clone()).collect();
        if cfg!(windows) && self.windows.all_fixed_drives {
            for d in fixed_drives {
                if !roots.contains(d) {
                    roots.push(d.clone());
                }
            }
        }
        roots
    }

    /// Metadata the scanner collects: size and modified always (the
    /// defaults), plus any other date or attribute column that is shown.
    pub fn index_columns(&self) -> ferrow_index::ColumnSet {
        use ferrow_index::ColumnSet;
        let mut set = ColumnSet::DEFAULT;
        for c in self.columns.iter().filter(|c| c.visible) {
            set = set.union(match c.id {
                ColumnId::Created => ColumnSet::CREATED,
                ColumnId::Accessed => ColumnSet::ACCESSED,
                ColumnId::Attributes => ColumnSet::ATTRIBUTES,
                _ => ColumnSet::NONE,
            });
        }
        set
    }

    pub fn has_locations(&self) -> bool {
        !self.sources.is_empty() || (cfg!(windows) && self.windows.all_fixed_drives)
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
            sources: vec![Source {
                kind: SourceKind::NetworkFolder,
                path: PathBuf::from(r"\\server\share"),
            }],
            excludes: vec![PathBuf::from("C:/data/tmp")],
            follow_symlinks: true,
            auto_rebuild: AutoRebuild::Daily,
            ..Default::default()
        };
        save_to(&s, &file).unwrap();
        let back: Settings = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(back, s);
        // Unknown or missing fields fall back to defaults.
        let partial: Settings = serde_json::from_str(r#"{"theme":"light","future":1}"#).unwrap();
        assert_eq!(partial.theme, ThemeChoice::Light);
        assert!(partial.sources.is_empty());
        assert_eq!(partial.columns, default_columns());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_roots_migrate_to_sources() {
        let mut s: Settings = serde_json::from_str(r#"{"roots":["/data","/data"]}"#).unwrap();
        s.migrate();
        assert_eq!(s.sources.len(), 1);
        assert_eq!(s.sources[0].path, PathBuf::from("/data"));
    }

    #[test]
    fn fixed_drives_join_the_scan_roots_only_when_enabled() {
        let mut s = Settings::default();
        s.add_source(PathBuf::from("/a"));
        let drives = [PathBuf::from("/d1")];
        assert_eq!(s.scan_roots(&drives), vec![PathBuf::from("/a")]);
        s.windows.all_fixed_drives = true;
        let expected = if cfg!(windows) { 2 } else { 1 };
        assert_eq!(s.scan_roots(&drives).len(), expected);
    }

    #[test]
    fn columns_migrate_and_drive_index_columns() {
        let mut s: Settings = serde_json::from_str(
            r#"{"columns":[{"id":"name","visible":false,"width":200.0},{"id":"created","visible":true,"width":150.0},{"id":"created","visible":false,"width":1.0}]}"#,
        )
        .unwrap();
        s.migrate();
        assert_eq!(s.columns.len(), ColumnId::ALL.len());
        assert!(s.columns[0].visible, "name is always shown");
        assert!(s.index_columns().contains(ferrow_index::ColumnSet::CREATED));
        assert!(
            !s.index_columns()
                .contains(ferrow_index::ColumnSet::ACCESSED)
        );
        assert!(
            Settings::default()
                .index_columns()
                .contains(ferrow_index::ColumnSet::MODIFIED)
        );
    }
}
