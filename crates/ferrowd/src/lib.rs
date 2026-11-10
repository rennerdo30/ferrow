//! `ferrowd`, the Ferrow background index service (M2b).
//!
//! The daemon holds the index as an immutable [`Snapshot`]
//! behind an `Arc` swap: building a new index never blocks
//! readers, and swapping is one pointer store. Connections are
//! served with bounded workers over `ferrow-ipc`. Accept, signal and clean
//! persistence waits park on events; only dirty state arms a one-shot deadline.
//!
//! Until M2c, system replies require an OS-authenticated privileged caller;
//! development replies require the starting user. Refusals contain no index data.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ferrow_index::{Index, IndexBuilder};
use ferrow_ipc::{
    CallerIdentity, Endpoint, ErrorCode, Listener, Request, Response, serve_cancellable,
};
use ferrow_query::{CancelToken, Query, SearchOptions};
use ferrow_walk::{ScanReport, WalkOptions};

mod admission;
#[cfg(any(unix, test))]
mod definition;
pub mod install;
pub mod live;
#[cfg(windows)]
mod live_windows;
pub mod persistence;
pub mod protection;

/// One immutable index plus how it was produced.
pub struct Snapshot {
    pub index: Arc<Index>,
    /// Live changes on top of `index` (M2d); `None` for a plain index.
    pub live: Option<Arc<live::LiveView>>,
    /// Short mode label, e.g. `walk`, `loaded index file`, `empty`.
    pub mode: String,
    /// Backend report line for `Status` replies.
    pub report: String,
}

impl Snapshot {
    /// Entries a search can return.
    pub fn entries(&self) -> usize {
        self.live
            .as_ref()
            .map_or(self.index.len(), |live| live.entries)
    }

    /// The index holding a hit.
    fn index_of(&self, hit: Hit) -> &Index {
        match (hit.delta, &self.live) {
            (true, Some(live)) => &live.delta,
            _ => &self.index,
        }
    }

    fn metadata_of(&self, hit: Hit) -> ferrow_index::EntryMetadata {
        match (hit.delta, &self.live) {
            (true, Some(live)) => live.delta.metadata(hit.id),
            (false, Some(live)) => live.base_metadata(&self.index, hit.id),
            _ => self.index.metadata(hit.id),
        }
    }

    pub fn empty() -> Snapshot {
        Snapshot {
            index: Arc::new(IndexBuilder::new().freeze()),
            live: None,
            mode: "empty".into(),
            report: "no index loaded".into(),
        }
    }

    /// Build a snapshot by walking `options`.
    pub fn from_scan(options: &WalkOptions) -> (Snapshot, ScanReport) {
        let (index, report) = ferrow_walk::scan(options);
        let snapshot = Snapshot {
            index: Arc::new(index),
            live: None,
            mode: "walk".into(),
            report: report.summary(),
        };
        (snapshot, report)
    }

    /// Load a snapshot from a saved index file.
    pub fn from_file(path: &std::path::Path) -> std::io::Result<Snapshot> {
        let index = Index::load(path).map_err(std::io::Error::other)?;
        Ok(Snapshot {
            report: format!("loaded {} entries from {}", index.len(), path.display()),
            index: Arc::new(index),
            live: None,
            mode: "loaded index file".into(),
        })
    }
}

/// The current snapshot, swap-able without blocking readers:
/// `get` acquires an immutable Arc through ArcSwap's nonblocking reader path.
#[derive(Clone)]
pub struct SnapshotStore {
    inner: Arc<arc_swap::ArcSwap<Snapshot>>,
    generation: Arc<std::sync::atomic::AtomicU64>,
    changes: Arc<std::sync::Mutex<Option<std::sync::mpsc::SyncSender<()>>>>,
    /// Wakes `Watch` requests on publication; `true` once the server stops.
    watchers: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
}

impl SnapshotStore {
    pub fn new(snapshot: Snapshot) -> SnapshotStore {
        SnapshotStore {
            inner: Arc::new(arc_swap::ArcSwap::from_pointee(snapshot)),
            generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            changes: Arc::new(std::sync::Mutex::new(None)),
            watchers: Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new())),
        }
    }

    /// Publication counter: changes whenever a new snapshot is swapped in.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Block until the generation differs from `seen` (no polling). `None`
    /// once the server is stopping.
    pub fn wait_change(&self, seen: u64) -> Option<u64> {
        let (closing, wake) = &*self.watchers;
        let mut stopped = closing.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if *stopped {
                return None;
            }
            let now = self.generation();
            if now != seen {
                return Some(now);
            }
            stopped = wake.wait(stopped).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Release every waiting `Watch` (server shutdown).
    pub fn close_watchers(&self) {
        let (closing, wake) = &*self.watchers;
        *closing.lock().unwrap_or_else(|e| e.into_inner()) = true;
        wake.notify_all();
    }

    pub fn get(&self) -> Arc<Snapshot> {
        self.inner.load_full()
    }

    /// Replace the current snapshot. Readers that already hold
    /// the old `Arc` keep querying it to completion.
    pub fn swap(&self, snapshot: Snapshot) {
        self.publish(snapshot, drop);
    }

    fn publish(&self, snapshot: Snapshot, retire: impl FnOnce(Arc<Snapshot>)) {
        // swap returns the old Arc: reclamation happens after publication,
        // without holding a reader/writer or application publication lock.
        let old = self.inner.swap(Arc::new(snapshot));
        // Increment after publishing the pointer, before notifying persistence.
        // A saver reads the generation before acquiring its snapshot, so it
        // may conservatively save again but cannot certify an older snapshot.
        self.generation.fetch_add(1, Ordering::AcqRel);
        {
            // Take the lock so a watcher between its check and its wait
            // cannot miss this wake.
            let (closing, wake) = &*self.watchers;
            drop(closing.lock().unwrap_or_else(|e| e.into_inner()));
            wake.notify_all();
        }
        if let Some(sender) = self
            .changes
            .lock()
            .expect("change subscriber poisoned")
            .as_ref()
        {
            let _ = sender.try_send(());
        }
        retire(old);
    }

    /// Scan and swap in the result.
    pub fn rescan(&self, options: &WalkOptions) -> ScanReport {
        let (snapshot, report) = Snapshot::from_scan(options);
        self.swap(snapshot);
        report
    }
}

/// Daemon configuration shared by the binary, `ferrow service
/// run`, and the platform service hosts.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub endpoint: Endpoint,
    /// Index file to load at startup (and re-save after a scan).
    pub index_path: Option<PathBuf>,
    pub roots: Vec<PathBuf>,
    pub excludes: Vec<PathBuf>,
    pub follow_symlinks: bool,
}

/// The system service's index file: readable only by the
/// service and administrators. The directory and file policy is validated
/// before system loads and saves, including atomic index replacements.
pub fn default_service_index_path() -> std::io::Result<PathBuf> {
    #[cfg(windows)]
    {
        Ok(
            protection::windows::known_folder(
                &windows_sys::Win32::UI::Shell::FOLDERID_ProgramData,
            )?
            .join("Ferrow")
            .join("index.bin"),
        )
    }
    #[cfg(target_os = "macos")]
    {
        Ok(PathBuf::from("/private/var/lib/ferrow/index.bin"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Ok(PathBuf::from("/var/lib/ferrow/index.bin"))
    }
}

fn prepare_state(config: &DaemonConfig) -> std::io::Result<()> {
    if config.endpoint == Endpoint::default_service() {
        let expected = default_service_index_path()?;
        if config.index_path.as_ref() != Some(&expected) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "system service requires its fixed protected index path",
            ));
        }
        protection::state_directory(expected.parent().unwrap())?;
        if expected.try_exists()? {
            protection::state_file(&expected)?;
        }
    }
    Ok(())
}

/// Initial snapshot for a config: a saved index file wins, then
/// a walk of the configured roots (saved back to `index_path`
/// when set), else an empty index.
pub fn initial_snapshot(config: &DaemonConfig) -> std::io::Result<Snapshot> {
    prepare_state(config)?;
    if let Some(path) = &config.index_path
        && path.exists()
    {
        return Snapshot::from_file(path);
    }
    if !config.roots.is_empty() {
        let options = WalkOptions {
            columns: Default::default(),
            roots: config.roots.clone(),
            excludes: config.excludes.clone(),
            follow_symlinks: config.follow_symlinks,
            ntfs: ferrow_walk::NtfsMode::VolumeRoots,
        };
        let (snapshot, report) = Snapshot::from_scan(&options);
        for sample in &report.error_samples {
            eprintln!("ferrowd: warning: {sample}");
        }
        if let Some(path) = &config.index_path {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            snapshot.index.save(path).map_err(std::io::Error::other)?;
            prepare_state(config)?;
        }
        return Ok(snapshot);
    }
    Ok(Snapshot::empty())
}

/// Start live updates for `options` on `store` (Windows, with raw volume
/// access): a full scan, then change-journal updates. Does nothing when
/// the volumes cannot be read directly. `serve` calls this itself.
pub fn start_live(store: &SnapshotStore, options: WalkOptions) {
    #[cfg(windows)]
    live_windows::start(store.clone(), options);
    #[cfg(not(windows))]
    let _ = (store, options);
}

/// One search hit: an entry of the frozen index or of the live delta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Hit {
    delta: bool,
    id: u32,
}

/// Search the snapshot, merging live changes into the frozen index's
/// order. Returns the hits and the frozen search's status fields.
fn snapshot_search(
    snapshot: &Snapshot,
    query: &Query,
    options: &SearchOptions,
) -> (Vec<Hit>, ferrow_query::SearchResult) {
    let Some(live) = &snapshot.live else {
        let mut result = ferrow_query::search(&snapshot.index, query, options);
        let hits = result
            .hits
            .iter()
            .map(|&id| Hit { delta: false, id })
            .collect();
        result.hits.clear();
        return (hits, result);
    };
    let mut result = ferrow_query::search(
        &snapshot.index,
        query,
        &SearchOptions {
            allowed: Some(live.allowed.clone()),
            ..options.clone()
        },
    );
    // The delta is small: search it in name order, then order its hits
    // the way the frozen search ordered its own.
    let delta = ferrow_query::search(
        &live.delta,
        query,
        &SearchOptions {
            sort: ferrow_query::SortKey::Name,
            descending: false,
            limit: None,
            allowed: Some(live.delta_allowed.clone()),
            cancel: options.cancel.clone(),
            ..SearchOptions::default()
        },
    );
    result.cancelled |= delta.cancelled;
    let key = if result.sort_status == ferrow_query::SortStatus::Ready {
        options.sort
    } else {
        ferrow_query::SortKey::Name
    };
    let descending = options.descending
        && (key != ferrow_query::SortKey::Name || options.sort == ferrow_query::SortKey::Name);
    let compare = |a: Hit, b: Hit| -> std::cmp::Ordering {
        let (ia, ib) = (snapshot.index_of(a), snapshot.index_of(b));
        let by_name = || {
            ia.folded_name(a.id)
                .cmp(ib.folded_name(b.id))
                .then_with(|| ia.name(a.id).cmp(ib.name(b.id)))
                .then(a.delta.cmp(&b.delta))
        };
        if key == ferrow_query::SortKey::Name {
            let o = by_name();
            return if descending { o.reverse() } else { o };
        }
        let value = |h: Hit, i: &Index| i.metadata_value(key, h.id);
        match (value(a, ia), value(b, ib)) {
            (Some(x), Some(y)) => {
                let o = x.cmp(&y);
                (if descending { o.reverse() } else { o }).then_with(by_name)
            }
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => by_name(),
        }
    };
    let mut extra: Vec<Hit> = delta
        .hits
        .iter()
        .map(|&id| Hit { delta: true, id })
        .collect();
    extra.sort_by(|&a, &b| compare(a, b));
    let base: Vec<Hit> = result
        .hits
        .iter()
        .map(|&id| Hit { delta: false, id })
        .collect();
    let mut merged = Vec::with_capacity(base.len() + extra.len());
    let (mut i, mut j) = (0, 0);
    while i < base.len() || j < extra.len() {
        let take_base = j == extra.len()
            || (i < base.len() && compare(base[i], extra[j]) != std::cmp::Ordering::Greater);
        if take_base {
            merged.push(base[i]);
            i += 1;
        } else {
            merged.push(extra[j]);
            j += 1;
        }
    }
    if let Some(limit) = options.limit {
        merged.truncate(limit);
    }
    result.hits.clear();
    (merged, result)
}

fn snapshot_count(snapshot: &Snapshot, query: &Query, cancel: &CancelToken) -> (usize, bool) {
    match &snapshot.live {
        None => ferrow_query::count(&snapshot.index, query, cancel),
        Some(live) => {
            let (a, x) =
                ferrow_query::count_allowed(&snapshot.index, query, cancel, Some(&live.allowed));
            let (b, y) =
                ferrow_query::count_allowed(&live.delta, query, cancel, Some(&live.delta_allowed));
            (a + b, x || y)
        }
    }
}

/// Rows per `Page` request.
const MAX_PAGE_ROWS: usize = 2_000;

fn sort_key(sort: ferrow_ipc::Sort) -> ferrow_query::SortKey {
    match sort {
        ferrow_ipc::Sort::Name => ferrow_query::SortKey::Name,
        ferrow_ipc::Sort::Size => ferrow_query::SortKey::Size,
        ferrow_ipc::Sort::Modified => ferrow_query::SortKey::Modified,
        ferrow_ipc::Sort::Created => ferrow_query::SortKey::Created,
        ferrow_ipc::Sort::Accessed => ferrow_query::SortKey::Accessed,
    }
}

/// Page budgets for streamed `Results` (well under the frame cap).
const MAX_QUERY_RESULTS: usize = 10_000;
const PAGE_MAX_PATHS: usize = 512;
const PAGE_MAX_BYTES: usize = 256 * 1024;

fn bad_request(msg: String) -> Response {
    Response::Error {
        code: ErrorCode::BadRequest,
        msg,
    }
}

/// Answer one request from the current snapshot.
pub fn handle(store: &SnapshotStore, request: Request) -> Vec<Response> {
    handle_cancel(store, request, CancelToken::new())
}

fn handle_cancel(store: &SnapshotStore, request: Request, cancel: CancelToken) -> Vec<Response> {
    let snapshot = store.get();
    match request {
        Request::Query {
            text,
            limit,
            sort,
            flags,
        } => {
            if flags
                & !(ferrow_ipc::protocol::QUERY_DESCENDING | ferrow_ipc::protocol::QUERY_METADATA)
                != 0
            {
                return vec![bad_request("unsupported query flags".into())];
            }
            let metadata = flags & ferrow_ipc::protocol::QUERY_METADATA != 0;
            if sort != ferrow_ipc::Sort::Name && !metadata {
                return vec![bad_request(
                    "non-name sorts require metadata rows with readiness status".into(),
                )];
            }
            let query = match Query::parse(&text) {
                Ok(q) => q,
                Err(e) => return vec![bad_request(e.to_string())],
            };
            let options = SearchOptions {
                sort: match sort {
                    ferrow_ipc::Sort::Name => ferrow_query::SortKey::Name,
                    ferrow_ipc::Sort::Size => ferrow_query::SortKey::Size,
                    ferrow_ipc::Sort::Modified => ferrow_query::SortKey::Modified,
                    ferrow_ipc::Sort::Created => ferrow_query::SortKey::Created,
                    ferrow_ipc::Sort::Accessed => ferrow_query::SortKey::Accessed,
                },
                descending: flags & ferrow_ipc::protocol::QUERY_DESCENDING != 0,
                limit: Some(if limit == 0 {
                    MAX_QUERY_RESULTS + 1
                } else {
                    (limit as usize).min(MAX_QUERY_RESULTS + 1)
                }),
                cancel,
                ..SearchOptions::default()
            };
            let (hits, result) = snapshot_search(&snapshot, &query, &options);
            if let Some(key) = result.sort_unavailable {
                return vec![bad_request(format!(
                    "{key:?} column unavailable; enable the column and rebuild"
                ))];
            }
            if result.sort_status == ferrow_query::SortStatus::Failed {
                return vec![bad_request("sort preparation failed".into())];
            }
            if result.cancelled {
                return vec![Response::Error {
                    code: ErrorCode::Cancelled,
                    msg: "query cancelled".into(),
                }];
            }
            if hits.len() > MAX_QUERY_RESULTS {
                return vec![Response::Error {
                    code: ErrorCode::TooLarge,
                    msg: "service result budget exceeded; use --limit".into(),
                }];
            }
            let mut total_bytes = 0usize;
            let mut pages: Vec<Response> = Vec::new();
            let mut page: Vec<Vec<u8>> = Vec::new();
            let mut page_bytes = 0usize;
            let mut buf: Vec<u8> = Vec::new();
            let mut page_start = 0;
            let response = |paths: Vec<Vec<u8>>, ids: &[Hit], more| {
                if metadata {
                    let rows = paths
                        .into_iter()
                        .zip(ids)
                        .map(|(path, &hit)| {
                            let m = snapshot.metadata_of(hit);
                            ferrow_ipc::FileRow {
                                path,
                                size: m.size,
                                modified: m.modified,
                                created: m.created,
                                accessed: m.accessed,
                                attributes: m.attributes,
                                is_dir: snapshot.index_of(hit).flags(hit.id)
                                    & ferrow_index::flags::DIR
                                    != 0,
                            }
                        })
                        .collect();
                    Response::Rows {
                        rows,
                        columns: snapshot.index.columns_present().bits(),
                        sort_pending: result.sort_status == ferrow_query::SortStatus::Pending,
                        more,
                    }
                } else {
                    Response::Results { paths, more }
                }
            };
            for (position, &hit) in hits.iter().enumerate() {
                snapshot.index_of(hit).path_bytes(hit.id, &mut buf);
                total_bytes = total_bytes.saturating_add(buf.len() + if metadata { 41 } else { 4 });
                if total_bytes > ferrow_ipc::MAX_FRAME_BYTES
                    || buf.len() > ferrow_ipc::protocol::MAX_PATH_BYTES
                {
                    return vec![Response::Error {
                        code: ErrorCode::TooLarge,
                        msg: "service path byte budget exceeded; use --limit".into(),
                    }];
                }
                if !page.is_empty()
                    && (page.len() >= PAGE_MAX_PATHS || page_bytes + buf.len() > PAGE_MAX_BYTES)
                {
                    pages.push(response(
                        std::mem::take(&mut page),
                        &hits[page_start..position],
                        true,
                    ));
                    page_start = position;
                    page_bytes = 0;
                }
                page_bytes += buf.len() + if metadata { 41 } else { 4 };
                page.push(buf.clone());
            }
            pages.push(response(page, &hits[page_start..], false));
            pages
        }
        Request::Page {
            text,
            offset,
            limit,
            sort,
            flags,
        } => {
            if flags & !ferrow_ipc::protocol::QUERY_DESCENDING != 0 {
                return vec![bad_request("unsupported page flags".into())];
            }
            if limit as usize > MAX_PAGE_ROWS {
                return vec![bad_request(format!(
                    "at most {MAX_PAGE_ROWS} rows per page"
                ))];
            }
            let query = match Query::parse(&text) {
                Ok(q) => q,
                Err(e) => return vec![bad_request(e.to_string())],
            };
            let options = SearchOptions {
                sort: sort_key(sort),
                descending: flags & ferrow_ipc::protocol::QUERY_DESCENDING != 0,
                limit: Some(offset as usize + limit as usize),
                cancel,
                ..SearchOptions::default()
            };
            let (hits, result) = snapshot_search(&snapshot, &query, &options);
            if let Some(key) = result.sort_unavailable {
                return vec![bad_request(format!(
                    "{key:?} column unavailable; enable the column and rebuild"
                ))];
            }
            if result.cancelled {
                return vec![Response::Error {
                    code: ErrorCode::Cancelled,
                    msg: "query cancelled".into(),
                }];
            }
            let mut rows = Vec::new();
            let mut bytes = 32usize;
            let mut buf = Vec::new();
            for &hit in hits.iter().skip(offset as usize) {
                snapshot.index_of(hit).path_bytes(hit.id, &mut buf);
                bytes += buf.len() + 41;
                if bytes > ferrow_ipc::MAX_FRAME_BYTES
                    || buf.len() > ferrow_ipc::protocol::MAX_PATH_BYTES
                {
                    break;
                }
                let m = snapshot.metadata_of(hit);
                rows.push(ferrow_ipc::FileRow {
                    path: buf.clone(),
                    size: m.size,
                    modified: m.modified,
                    created: m.created,
                    accessed: m.accessed,
                    attributes: m.attributes,
                    is_dir: snapshot.index_of(hit).flags(hit.id) & ferrow_index::flags::DIR != 0,
                });
            }
            vec![Response::Rows {
                rows,
                columns: snapshot.index.columns_present().bits(),
                sort_pending: result.sort_status == ferrow_query::SortStatus::Pending,
                more: false,
            }]
        }
        Request::Watch { generation } => {
            let now = store.generation();
            if generation == u64::MAX || generation != now {
                return vec![Response::Changed { generation: now }];
            }
            match store.wait_change(generation) {
                Some(generation) => vec![Response::Changed { generation }],
                None => vec![Response::Error {
                    code: ErrorCode::Cancelled,
                    msg: "service stopping".into(),
                }],
            }
        }
        Request::Count { text } => {
            let query = match Query::parse(&text) {
                Ok(q) => q,
                Err(e) => return vec![bad_request(e.to_string())],
            };
            let (n, cancelled) = snapshot_count(&snapshot, &query, &cancel);
            if cancelled {
                return vec![Response::Error {
                    code: ErrorCode::Cancelled,
                    msg: "count cancelled".into(),
                }];
            }
            vec![Response::Count { n: n as u64 }]
        }
        Request::Status => vec![Response::Status {
            entries: snapshot.entries() as u64,
            mode: snapshot.mode.clone(),
            report: snapshot.report.clone(),
        }],
        // The envelope-aware dispatcher owns in-flight cancellation; it is
        // one-way and never receives a separate reply under Cancel's own id.
        Request::Cancel { .. } => vec![],
        Request::Hello { .. } => vec![bad_request("unexpected Hello".into())],
    }
}

/// Interim M2b boundary, removed when M2c provides per-user visibility.
/// The caller must come from the stream after a complete OS-authenticated read.
pub fn handle_authorized(
    store: &SnapshotStore,
    request: Request,
    caller: &CallerIdentity,
    owner: &CallerIdentity,
    system: bool,
) -> Vec<Response> {
    handle_authorized_cancel(store, request, caller, owner, system, CancelToken::new())
}

fn handle_authorized_cancel(
    store: &SnapshotStore,
    request: Request,
    caller: &CallerIdentity,
    owner: &CallerIdentity,
    system: bool,
    cancel: CancelToken,
) -> Vec<Response> {
    let allowed = if system {
        caller.is_admin_account()
    } else {
        #[cfg(windows)]
        {
            caller.access_token().is_some()
                && caller.user_sid.is_some()
                && caller.user_sid == owner.user_sid
        }
        #[cfg(unix)]
        {
            caller.uid.is_some() && caller.uid == owner.uid
        }
    };
    if !allowed {
        return vec![Response::Error { code: ErrorCode::Unauthorized,
            msg: "per-user visibility not yet available; the system service answers SYSTEM, accounts in the Administrators group (not sandboxed) or root; development access requires its owner".into() }];
    }
    handle_cancel(store, request, cancel)
}

/// Cooperative shutdown with persistent listener cancellation. Registration
/// observes earlier requests, and callbacks signal without connecting or waiting.
#[derive(Clone, Default)]
pub struct Shutdown {
    flag: Arc<AtomicBool>,
    listener: Arc<std::sync::Mutex<Option<std::sync::Weak<Listener>>>>,
}

impl Shutdown {
    pub fn new() -> Shutdown {
        Shutdown::default()
    }

    pub fn is_requested(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    pub fn request(&self, _endpoint: &Endpoint) {
        self.flag.store(true, Ordering::SeqCst);
        if let Some(listener) = self
            .listener
            .lock()
            .expect("shutdown poisoned")
            .as_ref()
            .and_then(|l| l.upgrade())
        {
            let _ = listener.cancel();
        }
    }

    fn register(&self, listener: &Arc<Listener>) -> std::io::Result<()> {
        let mut slot = self
            .listener
            .lock()
            .map_err(|_| std::io::Error::other("shutdown poisoned"))?;
        *slot = Some(Arc::downgrade(listener));
        if self.is_requested() {
            listener.cancel()?;
        }
        Ok(())
    }
}

/// Run the daemon until shutdown is requested: bind, accept,
/// one thread per connection. Blocking.
pub fn serve(
    config: &DaemonConfig,
    store: SnapshotStore,
    shutdown: Shutdown,
) -> std::io::Result<()> {
    prepare_state(config)?;
    let listener = Arc::new(Listener::bind(&config.endpoint)?);
    shutdown.register(&listener)?;
    let owner = CallerIdentity::current()?;
    let system = config.endpoint == Endpoint::default_service();
    let persistence = persistence::Persistence::start(config, &store, &shutdown)?;
    #[cfg(windows)]
    if !config.roots.is_empty() {
        live_windows::start(
            store.clone(),
            WalkOptions {
                columns: Default::default(),
                roots: config.roots.clone(),
                excludes: config.excludes.clone(),
                follow_symlinks: config.follow_symlinks,
                ntfs: ferrow_walk::NtfsMode::VolumeRoots,
            },
        );
    }
    eprintln!("ferrowd: listening on {}", config.endpoint);
    let budget = admission::Admission::new(32);
    let mut workers: Vec<(ferrow_ipc::Stream, std::thread::JoinHandle<()>)> =
        Vec::with_capacity(32);
    let result = (|| {
        while !shutdown.is_requested() {
            let (stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(_) if shutdown.is_requested() => break,
                Err(e) => return Err(e),
            };
            if shutdown.is_requested() {
                break;
            }
            for index in (0..workers.len()).rev() {
                if workers[index].1.is_finished() {
                    let _ = workers.swap_remove(index).1.join();
                }
            }
            if workers.len() >= 32 {
                drop(stream);
                continue;
            }
            let Some(permit) = budget.acquire() else {
                drop(stream);
                continue;
            };
            let control = stream.try_clone()?;
            let store = store.clone();
            let owner = owner.clone();
            let stopping = shutdown.clone();
            let thread = std::thread::Builder::new()
                .name("ipc-connection".into())
                .spawn(move || {
                    let _permit = permit;
                    let result = serve_cancellable(stream, move |context, request| {
                        handle_authorized_cancel(
                            &store,
                            request,
                            &context.caller,
                            &owner,
                            system,
                            CancelToken::from_flag(context.cancellation_flag()),
                        )
                    });
                    if let Err(error) = result
                        && !stopping.is_requested()
                    {
                        eprintln!("ferrowd: connection closed: {error}");
                    }
                })?;
            workers.push((control, thread));
        }
        Ok(())
    })();
    store.close_watchers();
    for (control, _) in &workers {
        let _ = control.shutdown();
    }
    for (_, thread) in workers {
        thread
            .join()
            .map_err(|_| std::io::Error::other("connection worker panicked"))?;
    }
    if let Some(worker) = persistence {
        worker.finish()?;
    }
    result
}

/// Foreground hosts register Unix signals before scanning or starting workers.
pub fn serve_foreground(config: &DaemonConfig) -> std::io::Result<()> {
    let shutdown = Shutdown::new();
    #[cfg(unix)]
    let (signal_handle, signal_worker) = {
        use signal_hook::{
            consts::{SIGINT, SIGTERM},
            iterator::Signals,
        };
        let mut signals = Signals::new([SIGTERM, SIGINT])?;
        let handle = signals.handle();
        let stop = shutdown.clone();
        let endpoint = config.endpoint.clone();
        let thread = std::thread::Builder::new()
            .name("stop-signals".into())
            .spawn(move || {
                if signals.forever().next().is_some() {
                    stop.request(&endpoint);
                }
            })?;
        (handle, thread)
    };
    let result = (|| {
        let store = SnapshotStore::new(initial_snapshot(config)?);
        eprintln!("ferrowd: {}", store.get().report);
        serve(config, store, shutdown)
    })();
    #[cfg(unix)]
    {
        signal_handle.close();
        signal_worker
            .join()
            .map_err(|_| std::io::Error::other("signal worker panicked"))?;
    }
    result
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;

    #[test]
    fn readers_continue_while_old_snapshot_reclamation_is_paused() {
        let store = SnapshotStore::new(Snapshot::empty());
        let writer = store.clone();
        let (paused_tx, paused_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            writer.publish(Snapshot::empty(), |old| {
                paused_tx.send(()).unwrap();
                resume_rx
                    .recv_timeout(std::time::Duration::from_secs(1))
                    .unwrap();
                drop(old);
            })
        });
        paused_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        let (read_tx, read_rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            read_tx.send(store.get()).unwrap();
        });
        let result = read_rx.recv_timeout(std::time::Duration::from_millis(150));
        resume_tx.send(()).unwrap();
        thread.join().unwrap();
        reader.join().unwrap();
        assert!(result.is_ok(), "reader waited for reclamation");
    }

    #[test]
    fn stop_before_listener_registration_cannot_be_lost() {
        let endpoint = Endpoint::named(&format!("fr-stop-{}", std::process::id()));
        let shutdown = Shutdown::new();
        shutdown.request(&endpoint);
        let listener = Arc::new(Listener::bind(&endpoint).unwrap());
        shutdown.register(&listener).unwrap();
        let accept = listener.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            tx.send(accept.accept().is_err()).unwrap();
        });
        let stopped = rx.recv_timeout(std::time::Duration::from_millis(200));
        listener.cancel().unwrap(); // bound cleanup even with the handshake broken
        thread.join().unwrap();
        assert!(stopped.unwrap());
    }
}
