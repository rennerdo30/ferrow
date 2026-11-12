//! File kinds (`System.Kind`) the way Windows assigns them: Explorer's
//! `KindMap` (extension to kinds), with a built-in table where the
//! registry is unavailable (other platforms, tests, a damaged registry).
//!
//! Ferrow uses the same kinds for the rows it hands to Start and for the
//! extension filters behind Start's Documents, Photos, Music and Videos
//! groups, so a row always lands in the group Windows would put it in.

use std::collections::HashMap;
use std::sync::OnceLock;

/// Extension (lowercase, no dot) to its kinds, first kind first.
type KindTable = HashMap<String, Vec<String>>;

const BUILTIN: &[(&str, &[&str])] = &[
    (
        "document",
        &[
            "txt", "csv", "rtf", "pdf", "doc", "docx", "docm", "dot", "dotx", "odt", "ott", "xls",
            "xlsx", "xlsm", "xlsb", "ods", "ppt", "pptx", "pptm", "pps", "ppsx", "odp", "xps",
            "oxps", "epub", "one",
        ],
    ),
    (
        "picture",
        &[
            "jpg", "jpeg", "jpe", "jfif", "png", "gif", "bmp", "dib", "tif", "tiff", "webp",
            "heic", "heif", "avif", "ico", "dng", "cr2", "cr3", "nef", "arw", "orf", "rw2",
        ],
    ),
    (
        "music",
        &[
            "mp3", "wav", "flac", "m4a", "aac", "oga", "opus", "wma", "aif", "aiff", "mid", "midi",
        ],
    ),
    (
        "video",
        &[
            "mp4", "m4v", "mkv", "avi", "mov", "wmv", "webm", "mpg", "mpeg", "3gp",
        ],
    ),
    ("program", &["exe", "msi", "bat", "cmd", "com"]),
    ("link", &["lnk", "url"]),
];

fn builtin() -> KindTable {
    let mut table = KindTable::new();
    for (kind, extensions) in BUILTIN {
        for ext in *extensions {
            table
                .entry((*ext).to_owned())
                .or_default()
                .push((*kind).to_owned());
        }
    }
    table
}

fn table() -> &'static KindTable {
    static TABLE: OnceLock<KindTable> = OnceLock::new();
    TABLE.get_or_init(|| {
        let loaded = load_kind_map();
        if loaded.is_empty() { builtin() } else { loaded }
    })
}

/// `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\KindMap`:
/// values named `.ext` holding `kind[;kind...]`.
#[cfg(windows)]
fn load_kind_map() -> KindTable {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ, RegCloseKey, RegEnumValueW, RegOpenKeyExW,
    };
    use windows::core::{PWSTR, w};

    let mut table = KindTable::new();
    let mut key = HKEY::default();
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Explorer\\KindMap"),
            None,
            KEY_READ,
            &mut key,
        )
    };
    if opened != ERROR_SUCCESS {
        return table;
    }
    let mut name = [0u16; 256];
    let mut data = [0u16; 256];
    for index in 0..4096u32 {
        let mut name_len = name.len() as u32;
        let mut data_len = std::mem::size_of_val(&data) as u32;
        let mut kind = 0u32;
        let status = unsafe {
            RegEnumValueW(
                key,
                index,
                Some(PWSTR(name.as_mut_ptr())),
                &mut name_len,
                None,
                Some(&mut kind),
                Some(data.as_mut_ptr().cast()),
                Some(&mut data_len),
            )
        };
        if status != ERROR_SUCCESS {
            break; // ERROR_NO_MORE_ITEMS, or a value too large to be a kind list
        }
        if kind != REG_SZ.0 {
            continue;
        }
        let ext = String::from_utf16_lossy(&name[..name_len as usize]);
        let Some(ext) = ext.strip_prefix('.') else {
            continue;
        };
        let units = (data_len as usize / 2).min(data.len());
        let value = String::from_utf16_lossy(&data[..units]);
        let kinds: Vec<String> = value
            .trim_end_matches('\0')
            .split(';')
            .map(|k| k.trim().to_ascii_lowercase())
            .filter(|k| !k.is_empty())
            .collect();
        if !ext.is_empty() && !kinds.is_empty() {
            table.insert(ext.to_ascii_lowercase(), kinds);
        }
    }
    unsafe {
        let _ = RegCloseKey(key);
    }
    table
}

#[cfg(not(windows))]
fn load_kind_map() -> KindTable {
    KindTable::new()
}

/// The kinds Windows gives a file with extension `ext` (no dot), first
/// kind first. Empty when Windows assigns none (for example `.md`).
pub fn kinds_of_extension(ext: &str) -> &'static [String] {
    table()
        .get(&ext.to_ascii_lowercase())
        .map_or(&[], Vec::as_slice)
}

/// Every extension Windows files under `kind` (for example `picture`),
/// sorted.
pub fn extensions_of_kind(kind: &str) -> Vec<&'static str> {
    let mut extensions: Vec<&'static str> = table()
        .iter()
        .filter(|(_, kinds)| kinds.iter().any(|k| k == kind))
        .map(|(ext, _)| ext.as_str())
        .collect();
    extensions.sort_unstable();
    extensions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_table_classifies_common_extensions() {
        let table = builtin();
        assert_eq!(table["pdf"], ["document"]);
        assert_eq!(table["png"], ["picture"]);
        assert!(!table.contains_key("md"));
    }

    #[test]
    fn kinds_and_extensions_agree() {
        for kind in ["document", "picture", "music", "video"] {
            let extensions = extensions_of_kind(kind);
            assert!(!extensions.is_empty(), "{kind}");
            for ext in extensions {
                assert!(kinds_of_extension(ext).iter().any(|k| k == kind));
            }
        }
        assert_eq!(
            kinds_of_extension("PNG").first().map(String::as_str),
            Some("picture")
        );
    }
}
