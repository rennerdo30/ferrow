//! File-backed zero-idle-CPU query bridge and standalone index cache.
//!
//! When `ferrow-gui` is running, it already holds the user's `Arc<Index>` in
//! memory (which can be several gigabytes for full-drive indexes). A background
//! responder thread in `ferrow-gui` blocks in the kernel on
//! `ReadDirectoryChangesW` (`0%` idle CPU) watching `<data_dir>/winsearch` for
//! `req-<id>.q` files written by `SearchHost.exe`, runs `ferrow_query::search`
//! on the in-memory `Arc<Index>`, and atomically renames `resp-<id>.tmp` to
//! `resp-<id>.r`.
//!
//! When `ferrow-gui` is not running and `<data_dir>/index.bin` is `<= 256 MiB`,
//! the attachment DLL loads and caches `index.bin` directly by modification time.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use ferrow_index::{Index, flags, osname};
use ferrow_query::{Query, SearchOptions, search};

/// Maximum `index.bin` size loaded directly inside `SearchHost.exe` when
/// `ferrow-gui` is not running (256 MiB). Larger indexes are served over IPC
/// by `ferrow-gui` so `SearchHost.exe` never allocates multi-gigabyte heaps.
const MAX_INPROC_INDEX_BYTES: u64 = 256 * 1024 * 1024;

/// Maximum time `SearchHost.exe` waits for an IPC reply from `ferrow-gui`.
const IPC_TIMEOUT: Duration = Duration::from_millis(250);

type CachedIndex = (PathBuf, SystemTime, Arc<Index>);

static REQ_COUNTER: AtomicU64 = AtomicU64::new(1);
static RESPONDER_STARTED: AtomicBool = AtomicBool::new(false);
static ACTIVE_INDEX: OnceLock<Mutex<Option<Arc<Index>>>> = OnceLock::new();
static CACHED_DISK_INDEX: OnceLock<Mutex<Option<CachedIndex>>> = OnceLock::new();

/// One matched file or folder returned to Windows Search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHitRow {
    pub file_name: String,
    pub full_path: String,
    pub is_dir: bool,
    pub modified_ticks: Option<i64>,
    pub created_ticks: Option<i64>,
    pub accessed_ticks: Option<i64>,
}

fn active_index_slot() -> &'static Mutex<Option<Arc<Index>>> {
    ACTIVE_INDEX.get_or_init(|| Mutex::new(None))
}

fn cached_disk_index_slot() -> &'static Mutex<Option<CachedIndex>> {
    CACHED_DISK_INDEX.get_or_init(|| Mutex::new(None))
}

/// Share the currently loaded `Arc<Index>` from `ferrow-gui` with the Windows
/// Search responder thread.
pub fn set_active_index(index: Option<Arc<Index>>) {
    if let Ok(mut guard) = active_index_slot().lock() {
        *guard = index;
    }
}

/// Subdirectory inside `%LOCALAPPDATA%\ferrow` used for control and IPC files.
pub fn winsearch_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("winsearch")
}

/// Path to the `enabled` marker file that dynamically gates query routing.
pub fn enabled_marker_path(data_dir: &Path) -> PathBuf {
    winsearch_dir(data_dir).join("enabled")
}

/// True when the Windows Search integration toggle is currently enabled.
pub fn is_enabled(data_dir: &Path) -> bool {
    enabled_marker_path(data_dir).is_file()
}

/// Execute `query_str` against `index` and materialize up to `limit` rows.
#[allow(
    clippy::needless_update,
    reason = "SearchOptions has extra fields when ferrow-query test-hooks is enabled"
)]
pub fn hits_from_index(index: &Index, query_str: &str, limit: usize) -> Option<Vec<SearchHitRow>> {
    let query = Query::parse(query_str).ok()?;
    let options = SearchOptions {
        limit: Some(limit.clamp(1, 100)),
        ..Default::default()
    };
    let res = search(index, &query, &options);
    let mut rows = Vec::with_capacity(res.hits.len());
    let mut path_buf = Vec::new();
    for id in res.hits {
        let file_name = osname::bytes_to_os(index.name(id))
            .to_string_lossy()
            .into_owned();
        index.path_bytes(id, &mut path_buf);
        let full_path = osname::path_from_bytes(&path_buf)
            .to_string_lossy()
            .into_owned();
        let is_dir = (index.flags(id) & flags::DIR) != 0;
        let meta = index.metadata(id);
        rows.push(SearchHitRow {
            file_name,
            full_path,
            is_dir,
            modified_ticks: meta.modified,
            created_ticks: meta.created,
            accessed_ticks: meta.accessed,
        });
    }
    Some(rows)
}

/// Query Ferrow for `query_str` using:
/// 1. In-process `ACTIVE_INDEX` if present in this process,
/// 2. The `ferrow-gui` responder over `<data_dir>/winsearch` if active,
/// 3. Direct cached load of `<data_dir>/index.bin` if `<= 256 MiB`.
pub fn query_hits(data_dir: &Path, query_str: &str, limit: usize) -> Option<Vec<SearchHitRow>> {
    if !is_enabled(data_dir) {
        return None;
    }

    if let Ok(guard) = active_index_slot().lock()
        && let Some(index) = guard.clone()
    {
        return hits_from_index(&index, query_str, limit);
    }

    let ws_dir = winsearch_dir(data_dir);
    if ws_dir.join("server.pid").is_file()
        && let Some(rows) = query_via_ipc(&ws_dir, query_str, limit)
    {
        return Some(rows);
    }

    query_via_disk_index(data_dir, query_str, limit)
}

fn query_via_ipc(ws_dir: &Path, query_str: &str, limit: usize) -> Option<Vec<SearchHitRow>> {
    let pid = std::process::id();
    let seq = REQ_COUNTER.fetch_add(1, Ordering::Relaxed);
    let req_id = format!("{pid}-{seq}");
    let tmp_path = ws_dir.join(format!("req-{req_id}.tmp"));
    let req_path = ws_dir.join(format!("req-{req_id}.q"));
    let resp_path = ws_dir.join(format!("resp-{req_id}.r"));

    let payload = format!("{limit}\n{query_str}\n");
    {
        let mut f = std::fs::File::create(&tmp_path).ok()?;
        f.write_all(payload.as_bytes()).ok()?;
    }
    if std::fs::rename(&tmp_path, &req_path).is_err() {
        let _ = std::fs::remove_file(&tmp_path);
        return None;
    }

    let deadline = Instant::now() + IPC_TIMEOUT;
    loop {
        if resp_path.is_file() {
            let content = std::fs::read_to_string(&resp_path).ok();
            let _ = std::fs::remove_file(&resp_path);
            let _ = std::fs::remove_file(&req_path);
            return content.map(|c| deserialize_rows(&c));
        }
        if Instant::now() >= deadline {
            let _ = std::fs::remove_file(&req_path);
            let _ = std::fs::remove_file(&resp_path);
            let _ = std::fs::remove_file(ws_dir.join("server.pid"));
            return None;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn query_via_disk_index(
    data_dir: &Path,
    query_str: &str,
    limit: usize,
) -> Option<Vec<SearchHitRow>> {
    let index_path = data_dir.join("index.bin");
    let meta = std::fs::metadata(&index_path).ok()?;
    if meta.len() > MAX_INPROC_INDEX_BYTES {
        return None;
    }
    let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);

    let index = {
        let mut guard = cached_disk_index_slot().lock().ok()?;
        if let Some((cached_path, cached_mtime, cached_idx)) = guard.as_ref()
            && cached_path == &index_path
            && *cached_mtime == mtime
        {
            cached_idx.clone()
        } else {
            let loaded = Arc::new(Index::load(&index_path).ok()?);
            *guard = Some((index_path, mtime, loaded.clone()));
            loaded
        }
    };

    hits_from_index(&index, query_str, limit)
}

/// Start the background IPC responder thread in `ferrow-gui` if not already running.
pub fn ensure_responder_started(data_dir: &Path) {
    let ws_dir = winsearch_dir(data_dir);
    let _ = std::fs::create_dir_all(&ws_dir);
    let _ = std::fs::write(ws_dir.join("server.pid"), std::process::id().to_string());
    if RESPONDER_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }

    let _ = std::thread::Builder::new()
        .name("ferrow-winsearch-ipc".into())
        .spawn(move || responder_loop(&ws_dir));
}

fn responder_loop(ws_dir: &Path) {
    process_pending_requests(ws_dir);
    #[cfg(windows)]
    watch_directory_windows(ws_dir);
    #[cfg(not(windows))]
    loop {
        std::thread::sleep(Duration::from_millis(50));
        process_pending_requests(ws_dir);
    }
}

#[cfg(windows)]
fn watch_directory_windows(ws_dir: &Path) {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_FILE_NAME,
        FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING, ReadDirectoryChangesW,
    };
    use windows::core::PCWSTR;

    let wide: Vec<u16> = ws_dir.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: `wide` is a valid NUL-terminated UTF-16 path; handle is owned by this thread.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_LIST_DIRECTORY.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    };
    let Ok(handle) = handle else {
        return;
    };

    let mut buffer = [0u64; 512];
    loop {
        let mut bytes_returned = 0u32;
        // SAFETY: synchronous ReadDirectoryChangesW into stack-aligned buffer.
        let ok = unsafe {
            ReadDirectoryChangesW(
                handle,
                buffer.as_mut_ptr().cast(),
                std::mem::size_of_val(&buffer) as u32,
                false,
                FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE,
                Some(&mut bytes_returned),
                None,
                None,
            )
        };
        if ok.is_err() {
            break;
        }
        process_pending_requests(ws_dir);
    }
    // SAFETY: closing owned directory handle on loop exit.
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(handle);
    }
}

/// Process all currently queued `req-*.q` files in `ws_dir`.
pub fn process_pending_requests(ws_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(ws_dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Some(req_id) = name.strip_prefix("req-").and_then(|s| s.strip_suffix(".q")) {
            handle_one_request(ws_dir, &path, req_id);
        } else if (name.ends_with(".tmp") || name.ends_with(".r"))
            && let Ok(meta) = entry.metadata()
            && let Ok(modified) = meta.modified()
            && now.duration_since(modified).unwrap_or_default() > Duration::from_secs(10)
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn handle_one_request(ws_dir: &Path, req_path: &Path, req_id: &str) {
    let Ok(text) = std::fs::read_to_string(req_path) else {
        return;
    };
    let _ = std::fs::remove_file(req_path);
    let mut lines = text.lines();
    let limit = lines
        .next()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(30)
        .clamp(1, 100);
    let Some(query_str) = lines.next() else {
        return;
    };

    let index_opt = active_index_slot().lock().ok().and_then(|g| g.clone());
    let rows = match index_opt {
        Some(index) => hits_from_index(&index, query_str, limit).unwrap_or_default(),
        None => Vec::new(),
    };

    let serialized = serialize_rows(&rows);
    let tmp_resp = ws_dir.join(format!("resp-{req_id}.tmp"));
    let final_resp = ws_dir.join(format!("resp-{req_id}.r"));
    if let Ok(mut f) = std::fs::File::create(&tmp_resp) {
        if f.write_all(serialized.as_bytes()).is_ok() {
            drop(f);
            let _ = std::fs::rename(&tmp_resp, &final_resp);
        } else {
            drop(f);
            let _ = std::fs::remove_file(&tmp_resp);
        }
    }
}

pub fn serialize_rows(rows: &[SearchHitRow]) -> String {
    let mut out = String::new();
    for r in rows {
        let dir_flag = u8::from(r.is_dir);
        let mod_str = r.modified_ticks.map(|v| v.to_string()).unwrap_or_default();
        let cre_str = r.created_ticks.map(|v| v.to_string()).unwrap_or_default();
        let acc_str = r.accessed_ticks.map(|v| v.to_string()).unwrap_or_default();
        let clean_name: String = r
            .file_name
            .chars()
            .filter(|&c| c != '\t' && c != '\n' && c != '\r')
            .collect();
        let clean_path: String = r
            .full_path
            .chars()
            .filter(|&c| c != '\t' && c != '\n' && c != '\r')
            .collect();
        out.push_str(&format!(
            "{dir_flag}\t{mod_str}\t{cre_str}\t{acc_str}\t{clean_name}\t{clean_path}\n"
        ));
    }
    out
}

pub fn deserialize_rows(text: &str) -> Vec<SearchHitRow> {
    let mut rows = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(6, '\t');
        let Some(dir_part) = parts.next() else {
            continue;
        };
        let Some(mod_part) = parts.next() else {
            continue;
        };
        let Some(cre_part) = parts.next() else {
            continue;
        };
        let Some(acc_part) = parts.next() else {
            continue;
        };
        let Some(name_part) = parts.next() else {
            continue;
        };
        let Some(path_part) = parts.next() else {
            continue;
        };
        rows.push(SearchHitRow {
            file_name: name_part.to_owned(),
            full_path: path_part.to_owned(),
            is_dir: dir_part == "1",
            modified_ticks: mod_part.parse::<i64>().ok(),
            created_ticks: cre_part.parse::<i64>().ok(),
            accessed_ticks: acc_part.parse::<i64>().ok(),
        });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrow_index::{ColumnSet, EntryMetadata, IndexBuilder};

    #[test]
    fn round_trips_serialized_rows() {
        let rows = vec![
            SearchHitRow {
                file_name: "report-2026.txt".into(),
                full_path: r"C:\Users\Alice\Documents\report-2026.txt".into(),
                is_dir: false,
                modified_ticks: Some(17_000_000_000_000_000),
                created_ticks: Some(16_000_000_000_000_000),
                accessed_ticks: None,
            },
            SearchHitRow {
                file_name: "Projects".into(),
                full_path: r"C:\Users\Alice\Projects".into(),
                is_dir: true,
                modified_ticks: None,
                created_ticks: None,
                accessed_ticks: None,
            },
        ];
        let text = serialize_rows(&rows);
        assert_eq!(deserialize_rows(&text), rows);
    }

    #[test]
    fn queries_hits_from_index_with_metadata() {
        let builder = IndexBuilder::with_columns(ColumnSet::MODIFIED);
        let root = builder.add_root(br"C:\Work", flags::DIR);
        builder.add_with_metadata(
            root,
            b"ferrow-native-query-proof.txt",
            0,
            EntryMetadata {
                modified: Some(17_123_456_000_000_000),
                ..Default::default()
            },
        );
        builder.add(root, b"other.txt", 0);
        let index = builder.freeze();

        let hits = hits_from_index(&index, "ferrow-native-query-proof", 30).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file_name, "ferrow-native-query-proof.txt");
        assert!(hits[0].full_path.ends_with("ferrow-native-query-proof.txt"));
        assert!(!hits[0].is_dir);
        assert_eq!(hits[0].modified_ticks, Some(17_123_456_000_000_000));
    }
}
