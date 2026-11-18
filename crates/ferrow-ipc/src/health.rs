//! The service's health report (`Request::Health`) and the one-line answer
//! to `Request::Rescan`.
//!
//! The wire body is this structure as JSON text inside one frame (the text
//! cap applies), so a field added later never needs a protocol bump and an
//! older reader ignores what it does not know. State words are plain strings
//! for the same reason; their values are the constants below. Times are Unix
//! milliseconds from the service's clock; `now_ms` is the service's own
//! clock when the report was made, so a client shows ages without depending
//! on its own clock.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::protocol::MAX_TEXT_BYTES;

/// `VolumeHealth::mode`: read through the MFT, kept live by the change journal.
pub const MODE_LIVE: &str = "mft+usn live";
/// `VolumeHealth::mode`: read by a directory walk (no live updates).
pub const MODE_WALK: &str = "walk";
/// `VolumeHealth::mode`: live updates were wanted but are not running;
/// `reason` says why.
pub const MODE_UNAVAILABLE: &str = "live unavailable";
/// `VolumeHealth::journal`: the journal is read normally.
pub const JOURNAL_OK: &str = "ok";
/// `VolumeHealth::journal`: the journal was reset or overrun; a rescan is due.
pub const JOURNAL_RESET: &str = "reset";
/// `VolumeHealth::journal`: the journal reader stopped; `journal_detail` says why.
pub const JOURNAL_ERROR: &str = "reader error";
/// `VolumeHealth::journal`: no journal is used (walked volume).
pub const JOURNAL_NONE: &str = "none";
/// `EventInfo::level`.
pub const LEVEL_INFO: &str = "info";
pub const LEVEL_WARN: &str = "warn";
pub const LEVEL_ERROR: &str = "error";

/// The most volumes and events one report carries.
pub const MAX_VOLUMES: usize = 32;
pub const MAX_EVENTS: usize = 50;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Health {
    pub version: String,
    pub pid: u32,
    /// When the service process started, and the service clock now.
    pub started_ms: u64,
    pub now_ms: u64,
    pub uptime_s: u64,
    /// Windows: working set and private bytes. Unix: resident set size in
    /// `working_set` and `None` for private bytes.
    pub working_set: u64,
    pub private_bytes: Option<u64>,
    /// Windows: the private part of the working set, which Task Manager's
    /// Memory column shows. `None` on other platforms and older services.
    pub private_working_set: Option<u64>,
    /// Bytes of the saved index file the service reads through a memory map
    /// (ADR 0007): the operating system's file cache, shared and
    /// reclaimable, in `working_set` while resident but not private.
    pub index_mapped_bytes: u64,
    /// Set while merging the live changes into the index keeps failing (its
    /// file cannot be written: a full disk): why, and when it is tried
    /// again. The changes stay in memory meanwhile and grow with them.
    pub merge_blocked: Option<String>,
    /// Entries a search can return.
    pub entries: u64,
    /// The snapshot's mode label (`live`, `walk`, `indexing`, `empty`, …).
    pub mode: String,
    /// The service's access mode: `admins`, `everyone` or `listed`.
    pub access: String,
    /// Changes the change journals reported and the service applied.
    pub live_changes: u64,
    /// A scan or rescan is running now.
    pub scanning: bool,
    /// Connections that ended with an error since the service started
    /// (counted, not logged one by one).
    pub connection_errors: u64,
    /// What the live updater did since the service started.
    pub live_stats: LiveStats,
    pub last_save: Option<SaveInfo>,
    pub volumes: Vec<VolumeHealth>,
    /// Volumes left out of `volumes` because there were more than [`MAX_VOLUMES`].
    pub more_volumes: u32,
    /// The newest events, oldest first.
    pub events: Vec<EventInfo>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SaveInfo {
    pub at_ms: u64,
    pub ok: bool,
    /// The failure, when `ok` is false.
    pub error: String,
    pub took_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanInfo {
    pub at_ms: u64,
    /// Why it ran: `startup`, `requested by an administrator`, a journal reset…
    pub reason: String,
    pub took_ms: u64,
}

/// Counters of the live updater, so a rescan storm or a failing optimisation
/// shows up without reading logs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LiveStats {
    /// Folders moved into the index whose contents were read in the
    /// background instead of rescanning every volume: finished reads…
    pub folder_reads: u64,
    /// …the entries they found, the milliseconds they took…
    pub folder_read_entries: u64,
    pub folder_read_ms: u64,
    /// Subfolders that vanished while a folder was read (deleted or moved
    /// away; each is checked, and one that still exists is a fallback).
    pub folder_read_skipped: u64,
    /// …and reads that failed or were too large, each of which asked for a
    /// rescan instead.
    pub folder_read_fallbacks: u64,
    /// Scans started, by why: `startup`, `moved-in folder`, `large folder
    /// move` (rare since 0.1.0-beta.20: a merge in memory answers it), `folders swapped nesting`, `too many live changes`, `too many
    /// replaced entries`, `too many superseded entries`, `journal`,
    /// `administrator`. A merge of the live changes in memory is not a scan
    /// and is not counted here.
    pub scans_by_reason: BTreeMap<String, u64>,
    /// Merges of the live changes into a new index in memory (no disk read)
    /// finished since the service started…
    pub compactions: u64,
    /// …the milliseconds they took in total…
    pub compaction_ms: u64,
    /// …and the last one since the last scan (a scan starts from the disks
    /// and clears it); its `reason` is the size limit that asked for it.
    pub last_compaction: Option<ScanInfo>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RescanInfo {
    pub reason: String,
    /// The earliest time the rescan may start (the rate limit), in service time.
    pub earliest_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VolumeHealth {
    /// The volume or folder root as configured.
    pub root: String,
    pub mode: String,
    /// Why live updates are unavailable, otherwise empty.
    pub reason: String,
    /// Entries read from this volume; `None` for a walked root (not counted per root).
    pub entries: Option<u64>,
    pub journal: String,
    pub journal_detail: String,
    pub last_scan: Option<ScanInfo>,
    pub rescan_pending: Option<RescanInfo>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EventInfo {
    pub at_ms: u64,
    pub level: String,
    pub text: String,
}

impl Health {
    /// Drop the oldest events (then trailing volumes) until the JSON fits
    /// the frame's text cap. Real reports are far smaller.
    pub fn fit(&mut self) {
        if self.volumes.len() > MAX_VOLUMES {
            self.more_volumes = (self.volumes.len() - MAX_VOLUMES) as u32;
            self.volumes.truncate(MAX_VOLUMES);
        }
        self.events
            .drain(..self.events.len().saturating_sub(MAX_EVENTS));
        while self.to_json().len() > MAX_TEXT_BYTES && !self.events.is_empty() {
            self.events.remove(0);
        }
        while self.to_json().len() > MAX_TEXT_BYTES && self.volumes.pop().is_some() {}
    }

    /// What needs attention, most important first; empty when all is well.
    /// The CLI and the app show these before any detail.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(save) = self.last_save.as_ref().filter(|s| !s.ok) {
            out.push(format!("the last index save failed: {}", save.error));
        }
        for v in &self.volumes {
            if v.journal == JOURNAL_ERROR {
                out.push(format!(
                    "{}: live updates stopped ({}); restart the service",
                    v.root, v.journal_detail
                ));
            } else if v.journal == JOURNAL_RESET {
                out.push(format!(
                    "{}: the change journal was reset; a rescan is due",
                    v.root
                ));
            }
            if v.mode == MODE_UNAVAILABLE {
                out.push(format!("{}: no live updates ({})", v.root, v.reason));
            }
        }
        out
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(text: &str) -> Result<Health, serde_json::Error> {
        serde_json::from_str(text)
    }
}

/// `2026-11-17 09:12:03Z` for Unix milliseconds (UTC).
pub fn format_utc(ms: u64) -> String {
    let (y, m, d, hh, mm, ss) = civil(ms / 1000);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}Z")
}

/// `20261117T091203Z`: sortable and safe in a file name.
pub fn compact_utc(ms: u64) -> String {
    let (y, m, d, hh, mm, ss) = civil(ms / 1000);
    format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z")
}

/// Civil date and time of Unix seconds (Hinnant's days-to-date).
fn civil(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + u64::from(m <= 2);
    (y, m, d, rem / 3600, rem % 3600 / 60, rem % 60)
}

/// `3 min 5 s ago` / `in 2 min 0 s` for `at_ms` seen from `now_ms`.
pub fn format_ago(now_ms: u64, at_ms: u64) -> String {
    if at_ms <= now_ms {
        format!("{} ago", format_duration(now_ms - at_ms))
    } else {
        format!("in {}", format_duration(at_ms - now_ms))
    }
}

/// A duration in milliseconds as `850 ms`, `4.2 s`, `3 min 5 s` or `2 h 10 min`.
pub fn format_duration(ms: u64) -> String {
    match ms {
        0..1000 => format!("{ms} ms"),
        1000..60_000 => format!("{:.1} s", ms as f64 / 1000.0),
        60_000..3_600_000 => format!("{} min {} s", ms / 60_000, ms % 60_000 / 1000),
        _ => format!("{} h {} min", ms / 3_600_000, ms % 3_600_000 / 60_000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_format_as_utc() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00Z");
        // 2024-02-29 12:34:56 UTC, a leap day.
        assert_eq!(format_utc(1_709_210_096_000), "2024-02-29 12:34:56Z");
        assert_eq!(compact_utc(1_709_210_096_000), "20240229T123456Z");
        assert_eq!(format_duration(850), "850 ms");
        assert_eq!(format_duration(4_200), "4.2 s");
        assert_eq!(format_duration(185_000), "3 min 5 s");
        assert_eq!(format_duration(7_800_000), "2 h 10 min");
    }

    #[test]
    fn fit_keeps_the_newest_events_inside_the_text_cap() {
        let mut health = Health {
            events: (0..200)
                .map(|n| EventInfo {
                    at_ms: n,
                    level: LEVEL_INFO.into(),
                    text: "x".repeat(2_000),
                })
                .collect(),
            ..Health::default()
        };
        health.fit();
        assert!(health.to_json().len() <= MAX_TEXT_BYTES);
        assert!(health.events.len() <= MAX_EVENTS);
        assert_eq!(health.events.last().unwrap().at_ms, 199, "newest kept");
    }

    #[test]
    fn an_older_or_newer_report_still_reads() {
        let h =
            Health::from_json(r#"{"version":"9","future_field":1,"volumes":[{"root":"C:\\"}]}"#)
                .unwrap();
        assert_eq!(h.version, "9");
        assert_eq!(h.volumes[0].root, "C:\\");
        assert_eq!(h.entries, 0);
    }
}
