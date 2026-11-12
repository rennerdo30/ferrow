//! File-backed zero-idle-CPU query bridge and standalone index cache.
//!
//! When `ferrow-gui` is running, a background responder thread blocks in the
//! kernel on `ReadDirectoryChangesW` (`0%` idle CPU) watching
//! `<data_dir>/winsearch` for `req-<id>.q` files written by `SearchHost.exe`.
//! It answers from the same source the app itself searches: the in-memory
//! `Arc<Index>` when the app uses its own index, or the `ferrowd` service
//! over `ferrow-ipc` when the service answers (the normal installed setup,
//! whose live index can be several gigabytes and belongs to the service).
//! Replies are atomically renamed `resp-<id>.tmp` to `resp-<id>.r`.
//!
//! When `ferrow-gui` is not running, the app's own index is the source and
//! `<data_dir>/index.bin` is `<= 256 MiB`, the attachment DLL searches a
//! copy preloaded off the query path at attach (deadline-bounded). In
//! service mode the
//! DLL never serves that stale local file; `SearchHost.exe` falls back to
//! native Windows Search instead (the service index under
//! `C:\ProgramData` is SYSTEM-protected and unreadable from the shell).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use ferrow_index::{Index, flags, osname};
use ferrow_ipc::{Client, Endpoint, FileRow, Sort, Stream};
use ferrow_query::{CancelToken, Query, SearchOptions, search};

/// Maximum `index.bin` size loaded directly inside `SearchHost.exe` when
/// `ferrow-gui` is not running (256 MiB). Larger indexes are served over IPC
/// by `ferrow-gui` so `SearchHost.exe` never allocates multi-gigabyte heaps.
const MAX_INPROC_INDEX_BYTES: u64 = 256 * 1024 * 1024;

/// Maximum time `SearchHost.exe` waits for an IPC reply from `ferrow-gui`.
const IPC_TIMEOUT: Duration = Duration::from_millis(500);

/// Budget for (re)connecting the responder to the `ferrowd` service. Runs
/// on the responder thread only; `SearchHost.exe` has already fallen back
/// to native results after [`IPC_TIMEOUT`], so this never stalls the shell.
const SERVICE_CONNECT_BUDGET: Duration = Duration::from_millis(800);
/// Budget for one service page query on an established connection, kept
/// inside [`IPC_TIMEOUT`] so the reply can still reach `SearchHost.exe`.
/// A rare term scans the whole service index (about 150 ms for 35 million
/// names), so this must stay well above that.
const SERVICE_QUERY_BUDGET: Duration = Duration::from_millis(450);
/// Budget for one search over the preloaded on-disk index inside
/// `SearchHost.exe`. Loading never happens on the query path (see
/// [`preload_disk_index`]); this caps the search itself.
const DISK_SEARCH_BUDGET: Duration = Duration::from_millis(200);
/// After a bridge timeout the DLL skips IPC for this long instead of
/// deleting the responder marker: a slow first service connect must
/// not disable the bridge until the next app sync.
/// Kept short: Start sends one query per keystroke, and the query for the
/// finished word must not land inside the cooldown of an earlier one.
const IPC_COOLDOWN: Duration = Duration::from_secs(1);
/// After a failed service connect or query, wait this long before the
/// responder tries the service again (native fallback meanwhile).
const SERVICE_RETRY_BACKOFF: Duration = Duration::from_millis(500);

type CachedIndex = (PathBuf, SystemTime, Arc<Index>);

static REQ_COUNTER: AtomicU64 = AtomicU64::new(1);
static RESPONDER_STARTED: AtomicBool = AtomicBool::new(false);
static DISK_LOAD_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
/// Millisecond timestamp (against [`millis_now`]) until which the DLL
/// skips bridge attempts after a timeout. The responder marker file is
/// never deleted by the shell side.
static IPC_COOLDOWN_UNTIL_MS: AtomicU64 = AtomicU64::new(0);

fn millis_now() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}
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

/// Which index answers Start menu searches, mirroring the app's own source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexSource {
    /// The app's own in-memory / on-disk index.
    Local,
    /// The `ferrowd` service index, queried over `ferrow-ipc`.
    Service,
}

static ACTIVE_SOURCE: AtomicU8 = AtomicU8::new(0);

fn source_code(source: IndexSource) -> u8 {
    match source {
        IndexSource::Local => 1,
        IndexSource::Service => 2,
    }
}

/// The source currently answering in this process, if one was published.
pub fn source() -> Option<IndexSource> {
    match ACTIVE_SOURCE.load(Ordering::Acquire) {
        1 => Some(IndexSource::Local),
        2 => Some(IndexSource::Service),
        _ => None,
    }
}

/// Publish the answering source: in memory for the responder and Settings
/// status, and as the `source` marker the DLL in `SearchHost.exe` reads
/// before considering the on-disk local index.
pub fn set_source(data_dir: &Path, source: IndexSource) {
    ACTIVE_SOURCE.store(source_code(source), Ordering::Release);
    let text = match source {
        IndexSource::Local => b"local".as_slice(),
        IndexSource::Service => b"service".as_slice(),
    };
    let _ = std::fs::write(source_marker_path(data_dir), text);
}

/// Path to the `source` marker file inside the winsearch folder.
pub fn source_marker_path(data_dir: &Path) -> PathBuf {
    winsearch_dir(data_dir).join("source")
}

fn source_from_marker(data_dir: &Path) -> Option<IndexSource> {
    match std::fs::read_to_string(source_marker_path(data_dir))
        .ok()?
        .trim()
    {
        "local" => Some(IndexSource::Local),
        "service" => Some(IndexSource::Service),
        _ => None,
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
pub fn hits_from_index(index: &Index, query_str: &str, limit: usize) -> Option<Vec<SearchHitRow>> {
    hits_from_index_inner(index, query_str, limit, CancelToken::new())
}

/// Like [`hits_from_index`], but the scan is cancelled after `budget`
/// and a cancelled (partial) scan returns `None` so the shell falls
/// back to native results instead of showing an arbitrary prefix.
fn hits_from_index_deadline(
    index: &Index,
    query_str: &str,
    limit: usize,
    budget: Duration,
) -> Option<Vec<SearchHitRow>> {
    let token = CancelToken::new();
    let watchdog = token.clone();
    let _ = std::thread::Builder::new()
        .name("ferrow-winsearch-deadline".into())
        .spawn(move || {
            std::thread::sleep(budget);
            watchdog.cancel();
        });
    let rows = hits_from_index_inner(index, query_str, limit, token.clone())?;
    if token.is_cancelled() {
        None
    } else {
        Some(rows)
    }
}

#[allow(
    clippy::needless_update,
    reason = "SearchOptions has extra fields when ferrow-query test-hooks is enabled"
)]
fn hits_from_index_inner(
    index: &Index,
    query_str: &str,
    limit: usize,
    cancel: CancelToken,
) -> Option<Vec<SearchHitRow>> {
    let query = Query::parse(query_str).ok()?;
    let options = SearchOptions {
        limit: Some(limit.clamp(1, 100)),
        cancel,
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

/// Convert `ferrowd` service rows (full-path bytes with metadata and
/// folder flags, protocol 4) into Windows Search hit rows. Date ticks
/// pass through unchanged: both sides use signed Unix-epoch 100 ns ticks.
pub fn rows_from_service(rows: Vec<FileRow>) -> Vec<SearchHitRow> {
    rows.into_iter()
        .map(|row| {
            let full_path = osname::path_from_bytes(&row.path)
                .to_string_lossy()
                .into_owned();
            let split = row
                .path
                .iter()
                .rposition(|&b| b == b'\\' || b == b'/')
                .filter(|&at| at + 1 < row.path.len());
            let file_name = match split {
                Some(at) => osname::bytes_to_os(&row.path[at + 1..])
                    .to_string_lossy()
                    .into_owned(),
                None => full_path.clone(),
            };
            SearchHitRow {
                file_name,
                full_path,
                is_dir: row.is_dir,
                modified_ticks: row.modified,
                created_ticks: row.created,
                accessed_ticks: row.accessed,
            }
        })
        .collect()
}

/// The responder's reusable connection to the `ferrowd` service. Owned by
/// the responder thread; every operation is deadline-bound and any error
/// drops the connection (a timed-out stream may be mid-frame), with a
/// short backoff before reconnecting.
#[derive(Default)]
pub struct ServiceLink {
    client: Option<Client<Stream>>,
    retry_at: Option<Instant>,
}

impl ServiceLink {
    fn query(&mut self, query_str: &str, limit: usize) -> Option<Vec<SearchHitRow>> {
        if self.client.is_none() {
            if self.retry_at.is_some_and(|at| Instant::now() < at) {
                return None;
            }
            let deadline = Instant::now() + SERVICE_CONNECT_BUDGET;
            match Client::connect_deadline(&Endpoint::default_service(), deadline) {
                Ok(mut client) => {
                    client.clear_deadline();
                    self.client = Some(client);
                    self.retry_at = None;
                }
                Err(_) => {
                    self.retry_at = Some(Instant::now() + SERVICE_RETRY_BACKOFF);
                    return None;
                }
            }
        }
        let client = self.client.as_mut()?;
        let _ = client.set_deadline(Instant::now() + SERVICE_QUERY_BUDGET);
        // Newest first: when there are more matches than fit, Start gets
        // the files the user most likely means. An index without dates
        // (for example during its first scan) answers in name order.
        let page = match client.page(query_str, 0, limit as u32, Sort::Modified, true) {
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                client.page(query_str, 0, limit as u32, Sort::Name, false)
            }
            other => other,
        };
        match page {
            Ok((rows, _, _)) => {
                client.clear_deadline();
                Some(rows_from_service(rows))
            }
            Err(_) => {
                self.client = None;
                self.retry_at = Some(Instant::now() + SERVICE_RETRY_BACKOFF);
                None
            }
        }
    }
}

/// One Start search, as the shell asked for it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StartQuery {
    /// Every match: the Ferrow query for the shell's predicate.
    pub query: String,
    /// The best matches first: names starting with the first term.
    pub prefix_query: Option<String>,
    /// The first term, for ranking.
    pub rank_term: Option<String>,
    pub limit: usize,
}

impl StartQuery {
    pub fn from_sql(parsed: &crate::sql::ParsedSql) -> Self {
        StartQuery {
            query: parsed.query.clone(),
            prefix_query: parsed.prefix_query.clone(),
            rank_term: parsed.rank_term.clone(),
            limit: parsed.limit,
        }
    }
}

/// Matches gathered per Start query before ranking keeps the best `limit`.
const START_POOL: usize = 100;
/// Skip the second (all-matches) search when the prefix search already
/// took this long, so the reply still beats [`IPC_TIMEOUT`].
const SECOND_SEARCH_CUTOFF: Duration = Duration::from_millis(150);

/// Answer `start` with up to `limit` rows ranked like Windows Search:
/// names that start with the first term are asked for first, the rest of
/// the matches fill up, and [`rank_rows`] orders the pool. `fetch(query,
/// n)` runs one Ferrow search; `None` from the all-matches search is a
/// failure (native fallback).
fn ranked_hits(
    start: &StartQuery,
    mut fetch: impl FnMut(&str, usize) -> Option<Vec<SearchHitRow>>,
) -> Option<Vec<SearchHitRow>> {
    let started = Instant::now();
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
    let mut pool: Vec<SearchHitRow> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut add = |rows: Vec<SearchHitRow>, pool: &mut Vec<SearchHitRow>| {
        for row in rows {
            if !is_other_users_profile(&row.full_path, home.as_deref())
                && seen.insert(row.full_path.to_lowercase())
            {
                pool.push(row);
            }
        }
    };
    let mut prefix_ok = false;
    if let Some(prefix) = &start.prefix_query
        && let Some(rows) = fetch(prefix, START_POOL)
    {
        prefix_ok = true;
        add(rows, &mut pool);
    }
    if pool.len() < start.limit && !(prefix_ok && started.elapsed() > SECOND_SEARCH_CUTOFF) {
        match fetch(&start.query, START_POOL) {
            Some(rows) => add(rows, &mut pool),
            None if !prefix_ok => return None,
            None => {}
        }
    }
    rank_rows(&mut pool, start.rank_term.as_deref());
    pool.truncate(start.limit);
    Some(pool)
}

/// True for a path inside another account's profile folder (for example
/// `C:\Users\other\...` when this user's profile is `C:\Users\me`).
fn is_other_users_profile(path: &str, home: Option<&Path>) -> bool {
    let Some(home) = home else { return false };
    let home = home.to_string_lossy().trim_end_matches('\\').to_lowercase();
    let Some((profiles, _)) = home.rsplit_once('\\') else {
        return false;
    };
    let lower = path.to_lowercase();
    let profiles = format!("{profiles}\\");
    let home = format!("{home}\\");
    let Some(rest) = lower.strip_prefix(&profiles) else {
        return false;
    };
    !(lower.starts_with(&home) || rest.starts_with("public\\") || !rest.contains('\\'))
}

/// How well a file name matches like Windows ranks it: 3 whole name (with
/// or without extension), 2 name starts with the term, 1 a word in the
/// name starts with it, 0 elsewhere in the name.
fn name_tier(file_name: &str, term: &str) -> u8 {
    let name = file_name.to_lowercase();
    let term = term.to_lowercase();
    let stem = name
        .rsplit_once('.')
        .map_or(name.as_str(), |(stem, _)| stem);
    if name == term || stem == term {
        3
    } else if name.starts_with(&term) {
        2
    } else if name.match_indices(&term).any(|(at, _)| {
        name[..at]
            .chars()
            .next_back()
            .is_some_and(|c| !c.is_alphanumeric())
    }) {
        1
    } else {
        0
    }
}

/// Folders whose files are rarely what a Start search is for.
fn is_noise_path(path: &str) -> bool {
    const NOISE: [&str; 11] = [
        "\\appdata\\",
        "\\node_modules\\",
        "\\.git\\",
        "\\.cache\\",
        "\\_cache\\",
        "\\cache\\",
        "\\temp\\",
        "\\tmp\\",
        "\\_trash\\",
        "\\$recycle.bin\\",
        "\\windows\\winsxs\\",
    ];
    let lower = path.to_lowercase();
    NOISE.iter().any(|n| lower.contains(n))
}

/// Order like Windows Search ranks Start results: better name match first,
/// then outside noise folders, then most recently modified.
fn rank_rows(rows: &mut [SearchHitRow], term: Option<&str>) {
    rows.sort_by_key(|row| {
        (
            std::cmp::Reverse(term.map_or(0, |t| name_tier(&row.file_name, t))),
            is_noise_path(&row.full_path),
            std::cmp::Reverse(row.modified_ticks.unwrap_or(i64::MIN)),
        )
    });
}

/// Query Ferrow for `start` using:
/// 1. In-process `ACTIVE_INDEX` if present in this process,
/// 2. The `ferrow-gui` responder over `<data_dir>/winsearch` if active
///    (which itself answers from the local index or the `ferrowd` service),
/// 3. The preloaded `<data_dir>/index.bin` cache if ready (loading
///    happens off the query path in [`preload_disk_index`]).
pub fn query_hits(data_dir: &Path, start: &StartQuery) -> Option<Vec<SearchHitRow>> {
    if !is_enabled(data_dir) {
        return None;
    }

    if let Ok(guard) = active_index_slot().lock()
        && let Some(index) = guard.clone()
    {
        return ranked_hits(start, |q, n| hits_from_index(&index, q, n));
    }

    let ws_dir = winsearch_dir(data_dir);
    if ws_dir.join("server.pid").is_file()
        && let Some(rows) = query_via_ipc(&ws_dir, start)
    {
        return Some(rows);
    }

    query_via_disk_index(data_dir, start)
}

fn query_via_ipc(ws_dir: &Path, start: &StartQuery) -> Option<Vec<SearchHitRow>> {
    if millis_now() < IPC_COOLDOWN_UNTIL_MS.load(Ordering::Acquire) {
        return None;
    }
    let pid = std::process::id();
    let seq = REQ_COUNTER.fetch_add(1, Ordering::Relaxed);
    let req_id = format!("{pid}-{seq}");
    let tmp_path = ws_dir.join(format!("req-{req_id}.tmp"));
    let req_path = ws_dir.join(format!("req-{req_id}.q"));
    let resp_path = ws_dir.join(format!("resp-{req_id}.r"));

    let payload = format!(
        "{}\n{}\n{}\n{}\n",
        start.limit,
        start.query,
        start.prefix_query.as_deref().unwrap_or(""),
        start.rank_term.as_deref().unwrap_or("")
    );
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
            IPC_COOLDOWN_UNTIL_MS.store(0, Ordering::Release);
            return content.map(|c| deserialize_rows(&c));
        }
        if Instant::now() >= deadline {
            let _ = std::fs::remove_file(&req_path);
            let _ = std::fs::remove_file(&resp_path);
            // Keep server.pid: it is the responder's liveness marker,
            // owned by ferrow-gui. Back off briefly instead so one
            // slow connect cannot disable the bridge for the session.
            IPC_COOLDOWN_UNTIL_MS.store(
                millis_now() + IPC_COOLDOWN.as_millis() as u64,
                Ordering::Release,
            );
            return None;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The closed-app local fallback, bounded for the shell query thread:
/// it only ever searches an already-loaded cache (with a deadline) and
/// kicks off background loading otherwise, returning `None` so this
/// query falls back to native results instead of blocking on disk I/O.
fn query_via_disk_index(data_dir: &Path, start: &StartQuery) -> Option<Vec<SearchHitRow>> {
    // In service mode the local index.bin is stale by definition (the
    // service owns the live index): prefer native Windows Search.
    if source_from_marker(data_dir) == Some(IndexSource::Service) {
        return None;
    }
    let index_path = data_dir.join("index.bin");
    let meta = std::fs::metadata(&index_path).ok()?;
    if meta.len() > MAX_INPROC_INDEX_BYTES {
        return None;
    }
    let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);

    let cached = {
        let guard = cached_disk_index_slot().lock().ok()?;
        match guard.as_ref() {
            Some((cached_path, _, cached_idx)) if cached_path == &index_path => {
                Some(cached_idx.clone())
            }
            _ => None,
        }
    };
    let Some(index) = cached else {
        ensure_disk_index_loading(data_dir);
        return None;
    };
    // Serve the loaded copy now; refresh it in the background if the
    // file changed since it was loaded.
    let fresh = {
        let guard = cached_disk_index_slot().lock().ok()?;
        matches!(guard.as_ref(), Some((p, t, _)) if p == &index_path && *t == mtime)
    };
    if !fresh {
        ensure_disk_index_loading(data_dir);
    }
    // One budget for the whole ranked answer, split over its searches.
    let deadline = Instant::now() + DISK_SEARCH_BUDGET;
    ranked_hits(start, |q, n| {
        let left = deadline.checked_duration_since(Instant::now())?;
        hits_from_index_deadline(&index, q, n, left)
    })
}

/// Load `<data_dir>/index.bin` into the process cache. Blocking; run
/// only off the shell query path (preload thread, tests).
fn load_disk_index_blocking(data_dir: &Path) -> bool {
    if source_from_marker(data_dir) == Some(IndexSource::Service) {
        return false;
    }
    let index_path = data_dir.join("index.bin");
    let Ok(meta) = std::fs::metadata(&index_path) else {
        return false;
    };
    if meta.len() > MAX_INPROC_INDEX_BYTES {
        return false;
    }
    let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    {
        let Ok(guard) = cached_disk_index_slot().lock() else {
            return false;
        };
        if matches!(guard.as_ref(), Some((p, t, _)) if p == &index_path && *t == mtime) {
            return true;
        }
    }
    let Ok(loaded) = Index::load(&index_path) else {
        return false;
    };
    if let Ok(mut guard) = cached_disk_index_slot().lock() {
        *guard = Some((index_path, mtime, Arc::new(loaded)));
        true
    } else {
        false
    }
}

fn ensure_disk_index_loading(data_dir: &Path) {
    if DISK_LOAD_IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return;
    }
    let dir_owned = data_dir.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("ferrow-winsearch-preload".into())
        .spawn(move || {
            load_disk_index_blocking(&dir_owned);
            DISK_LOAD_IN_FLIGHT.store(false, Ordering::Release);
        });
    if spawned.is_err() {
        DISK_LOAD_IN_FLIGHT.store(false, Ordering::Release);
    }
}

/// Warm the closed-app disk cache off the query path (called when the
/// DLL attaches). No-op in service mode or over the size cap.
pub fn preload_disk_index(data_dir: &Path) {
    if source_from_marker(data_dir) != Some(IndexSource::Service) {
        ensure_disk_index_loading(data_dir);
    }
}

static DIAG_LOG: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Opt-in responder diagnostics: when `FERROW_WINSEARCH_DIAG` names a
/// file, the responder appends one line per request with the request id,
/// limit, hit count and answering source. Query text is never logged.
fn diag_log_path() -> Option<&'static Path> {
    DIAG_LOG
        .get_or_init(|| {
            std::env::var_os("FERROW_WINSEARCH_DIAG")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        })
        .as_deref()
}

fn diag_line(line: String) {
    if let Some(path) = diag_log_path()
        && let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
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
    let mut link = ServiceLink::default();
    process_pending_requests(ws_dir, &mut link);
    #[cfg(windows)]
    watch_directory_windows(ws_dir, &mut link);
    #[cfg(not(windows))]
    loop {
        std::thread::sleep(Duration::from_millis(50));
        process_pending_requests(ws_dir, &mut link);
    }
}

#[cfg(windows)]
fn watch_directory_windows(ws_dir: &Path, link: &mut ServiceLink) {
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
        process_pending_requests(ws_dir, link);
    }
    // SAFETY: closing owned directory handle on loop exit.
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(handle);
    }
}

/// Process all currently queued `req-*.q` files in `ws_dir`.
pub fn process_pending_requests(ws_dir: &Path, link: &mut ServiceLink) {
    let Ok(entries) = std::fs::read_dir(ws_dir) else {
        return;
    };
    let now = SystemTime::now();
    let mut requests: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Some(req_id) = name.strip_prefix("req-").and_then(|s| s.strip_suffix(".q")) {
            requests.push((req_id.to_owned(), path.clone()));
        } else if (name.ends_with(".tmp") || name.ends_with(".r"))
            && let Ok(meta) = entry.metadata()
            && let Ok(modified) = meta.modified()
            && now.duration_since(modified).unwrap_or_default() > Duration::from_secs(10)
        {
            let _ = std::fs::remove_file(&path);
        }
    }

    // Start sends one query per keystroke. Only the newest query of each
    // shell process is worth a search; older ones get an empty reply at
    // once (native results for that keystroke), so a slow search never
    // queues the finished word behind queries nobody looks at anymore.
    let newest = newest_request_per_process(&requests);
    for (req_id, path) in &requests {
        if newest.contains(req_id) {
            handle_one_request(ws_dir, path, req_id, link);
        } else {
            reply_superseded(ws_dir, path, req_id);
        }
    }
}

/// Request ids `<pid>-<seq>`: the highest `seq` per `pid`. Ids that do not
/// parse are always answered.
fn newest_request_per_process(requests: &[(String, PathBuf)]) -> Vec<String> {
    let mut newest: std::collections::HashMap<&str, (u64, &str)> = Default::default();
    let mut unparsed = Vec::new();
    for (req_id, _) in requests {
        match req_id
            .rsplit_once('-')
            .and_then(|(pid, seq)| Some((pid, seq.parse::<u64>().ok()?)))
        {
            Some((pid, seq)) => {
                let slot = newest.entry(pid).or_insert((seq, req_id));
                if seq > slot.0 {
                    *slot = (seq, req_id);
                }
            }
            None => unparsed.push(req_id.clone()),
        }
    }
    unparsed.extend(newest.into_values().map(|(_, id)| id.to_owned()));
    unparsed
}

fn reply_superseded(ws_dir: &Path, req_path: &Path, req_id: &str) {
    if std::fs::remove_file(req_path).is_err() {
        return; // the shell already gave up on it
    }
    diag_line(format!("resp id={req_id} superseded"));
    write_reply(ws_dir, req_id, &serialize_rows(&[]));
}

fn handle_one_request(ws_dir: &Path, req_path: &Path, req_id: &str, link: &mut ServiceLink) {
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
    let mut optional = || {
        lines
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let start = StartQuery {
        query: query_str.to_owned(),
        prefix_query: optional(),
        rank_term: optional(),
        limit,
    };

    // Answer from the app's own source: its in-memory index when one is
    // loaded, otherwise the ferrowd service. An empty reply makes the DLL
    // fall back to native Windows Search for this query.
    let index_opt = active_index_slot().lock().ok().and_then(|g| g.clone());
    let (rows, answered_by) = match index_opt {
        Some(index) => (
            ranked_hits(&start, |q, n| hits_from_index(&index, q, n)).unwrap_or_default(),
            "local",
        ),
        None => match ranked_hits(&start, |q, n| link.query(q, n)) {
            Some(rows) => (rows, "service"),
            None => (Vec::new(), "service-error"),
        },
    };
    diag_line(format!(
        "resp id={req_id} limit={limit} hits={} source={answered_by}",
        rows.len()
    ));

    write_reply(ws_dir, req_id, &serialize_rows(&rows));
}

fn write_reply(ws_dir: &Path, req_id: &str, serialized: &str) {
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

    fn plain(query: &str, limit: usize) -> StartQuery {
        StartQuery {
            query: query.to_owned(),
            limit,
            ..Default::default()
        }
    }

    fn row(path: &str, modified: i64) -> SearchHitRow {
        SearchHitRow {
            file_name: path.rsplit('\\').next().unwrap().to_owned(),
            full_path: path.to_owned(),
            is_dir: false,
            modified_ticks: Some(modified),
            created_ticks: None,
            accessed_ticks: None,
        }
    }

    #[test]
    fn start_hits_rank_like_windows_search() {
        assert_eq!(name_tier("README.md", "readme"), 3);
        assert_eq!(name_tier("readme-old.md", "readme"), 2);
        assert_eq!(name_tier("00-README.md", "readme"), 1);
        assert_eq!(name_tier("ThreadMessage.ts", "readme"), 0);

        let mut rows = vec![
            row(r"E:\src\069_ThreadMessageSearch.ts", 9),
            row(r"E:\docs\00-README.md", 1),
            row(r"C:\Users\me\AppData\Local\x\README.md", 8),
            row(r"E:\docs\README.md", 2),
            row(r"E:\docs\readme-old.md", 7),
        ];
        rank_rows(&mut rows, Some("readme"));
        let order: Vec<&str> = rows.iter().map(|r| r.full_path.as_str()).collect();
        assert_eq!(
            order,
            [
                r"E:\docs\README.md",
                r"C:\Users\me\AppData\Local\x\README.md",
                r"E:\docs\readme-old.md",
                r"E:\docs\00-README.md",
                r"E:\src\069_ThreadMessageSearch.ts",
            ]
        );
    }

    #[test]
    fn start_hits_skip_other_profiles_and_fill_up_from_all_matches() {
        let home = Path::new(r"C:\Users\me");
        assert!(is_other_users_profile(r"C:\Users\other\a.txt", Some(home)));
        assert!(!is_other_users_profile(r"C:\Users\me\a.txt", Some(home)));
        assert!(!is_other_users_profile(
            r"C:\Users\Public\a.txt",
            Some(home)
        ));
        assert!(!is_other_users_profile(r"E:\a.txt", Some(home)));

        let start = StartQuery {
            query: "readme".into(),
            prefix_query: Some("readme*".into()),
            rank_term: Some("readme".into()),
            limit: 3,
        };
        let mut asked = Vec::new();
        let rows = ranked_hits(&start, |q, _| {
            asked.push(q.to_owned());
            Some(match q {
                "readme*" => vec![row(r"E:\a\README.md", 1)],
                _ => vec![row(r"E:\a\README.md", 1), row(r"E:\b\00-readme.txt", 5)],
            })
        })
        .unwrap();
        assert_eq!(asked, ["readme*", "readme"]);
        assert_eq!(rows.len(), 2, "duplicates merged");
        assert_eq!(rows[0].full_path, r"E:\a\README.md");
    }

    #[test]
    fn only_the_newest_request_per_process_is_searched() {
        let requests: Vec<(String, PathBuf)> = ["10-2", "10-10", "10-9", "7-1", "odd"]
            .iter()
            .map(|id| (id.to_string(), PathBuf::new()))
            .collect();
        let mut newest = newest_request_per_process(&requests);
        newest.sort();
        assert_eq!(newest, ["10-10", "7-1", "odd"]);
    }

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

    #[test]
    fn converts_service_rows_with_names_flags_and_dates() {
        let rows = rows_from_service(vec![
            FileRow {
                path: br"C:\Work\report-2026.txt".to_vec(),
                modified: Some(17_123_456_000_000_000),
                created: Some(16_000_000_000_000_000),
                is_dir: false,
                ..Default::default()
            },
            FileRow {
                path: br"E:\Projects".to_vec(),
                is_dir: true,
                ..Default::default()
            },
            FileRow {
                path: br"C:\root-file.log".to_vec(),
                ..Default::default()
            },
        ]);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].file_name, "report-2026.txt");
        assert_eq!(rows[0].full_path, r"C:\Work\report-2026.txt");
        assert!(!rows[0].is_dir);
        assert_eq!(rows[0].modified_ticks, Some(17_123_456_000_000_000));
        assert_eq!(rows[0].created_ticks, Some(16_000_000_000_000_000));
        assert_eq!(rows[0].accessed_ticks, None);
        assert_eq!(rows[1].file_name, "Projects");
        assert!(rows[1].is_dir);
        assert_eq!(rows[2].file_name, "root-file.log");
        assert_eq!(rows[2].full_path, r"C:\root-file.log");
    }

    #[test]
    fn disk_fallback_never_serves_local_index_in_service_mode() {
        let temp_dir =
            std::env::temp_dir().join(format!("ferrow-winsearch-disk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(winsearch_dir(&temp_dir)).unwrap();

        let builder = IndexBuilder::new();
        let root = builder.add_root(br"C:\Work", flags::DIR);
        builder.add(root, b"ferrow-service-mode-guard.txt", 0);
        let index = builder.freeze();
        index.save(&temp_dir.join("index.bin")).unwrap();

        // Service mode: the stale local file must not be served.
        set_source(&temp_dir, IndexSource::Service);
        assert!(query_via_disk_index(&temp_dir, &plain("ferrow-service-mode-guard", 30)).is_none());

        // Local mode: a cold cache never loads synchronously on the
        // query path; after the off-path preload, the file answers.
        set_source(&temp_dir, IndexSource::Local);
        assert!(query_via_disk_index(&temp_dir, &plain("ferrow-service-mode-guard", 30)).is_none());
        assert!(load_disk_index_blocking(&temp_dir));
        let hits =
            query_via_disk_index(&temp_dir, &plain("ferrow-service-mode-guard", 30)).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file_name, "ferrow-service-mode-guard.txt");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn ipc_timeout_keeps_responder_marker_and_recovers() {
        let temp_dir =
            std::env::temp_dir().join(format!("ferrow-winsearch-timeout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        let ws_dir = winsearch_dir(&temp_dir);
        std::fs::create_dir_all(&ws_dir).unwrap();
        std::fs::write(ws_dir.join("server.pid"), b"99999").unwrap();
        IPC_COOLDOWN_UNTIL_MS.store(0, Ordering::Release);

        // No responder answers: the query times out, the marker the
        // responder owns survives, and a retry after the cooldown
        // works once a responder reply path exists again (here: the
        // cooldown is the only state that changed).
        assert!(query_via_ipc(&ws_dir, &plain("anything", 5)).is_none());
        assert!(
            ws_dir.join("server.pid").is_file(),
            "shell side must never delete the responder marker"
        );
        assert!(IPC_COOLDOWN_UNTIL_MS.load(Ordering::Acquire) > millis_now());
        IPC_COOLDOWN_UNTIL_MS.store(0, Ordering::Release);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
