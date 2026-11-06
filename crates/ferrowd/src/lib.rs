//! `ferrowd`, the Ferrow background index service (M2b).
//!
//! The daemon holds the index as an immutable [`Snapshot`]
//! behind an `Arc` swap: building a new index never blocks
//! readers, and swapping is one pointer store. Connections are
//! served one thread each over `ferrow-ipc`; the accept loop is
//! the only parked thread, so an idle daemon burns 0 % CPU
//! (no polling timers anywhere).
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

pub mod install;
pub mod protection;

/// One immutable index plus how it was produced.
pub struct Snapshot {
    pub index: Arc<Index>,
    /// Short mode label, e.g. `walk`, `loaded index file`, `empty`.
    pub mode: String,
    /// Backend report line for `Status` replies.
    pub report: String,
}

impl Snapshot {
    pub fn empty() -> Snapshot {
        Snapshot {
            index: Arc::new(IndexBuilder::new().freeze()),
            mode: "empty".into(),
            report: "no index loaded".into(),
        }
    }

    /// Build a snapshot by walking `options`.
    pub fn from_scan(options: &WalkOptions) -> (Snapshot, ScanReport) {
        let (index, report) = ferrow_walk::scan(options);
        let snapshot = Snapshot {
            index: Arc::new(index),
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
            mode: "loaded index file".into(),
        })
    }
}

/// The current snapshot, swap-able without blocking readers:
/// `get` acquires an immutable Arc through ArcSwap's nonblocking reader path.
#[derive(Clone)]
pub struct SnapshotStore {
    inner: Arc<arc_swap::ArcSwap<Snapshot>>,
}

impl SnapshotStore {
    pub fn new(snapshot: Snapshot) -> SnapshotStore {
        SnapshotStore {
            inner: Arc::new(arc_swap::ArcSwap::from_pointee(snapshot)),
        }
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
/// service (enforced by the directory ACL / mode at install
/// time; the file itself is written 0600 on Unix by
/// `Index::save`'s exclusive temp creation? see `ferrow-index`).
pub fn default_service_index_path() -> std::io::Result<PathBuf> {
    #[cfg(windows)]
    {
        Ok(
            protection::windows::known_folder(
                &windows_sys::Win32::UI::Shell::FOLDERID_ProgramData,
            )?
            .join("Ferrow/index.bin"),
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
            roots: config.roots.clone(),
            excludes: config.excludes.clone(),
            follow_symlinks: config.follow_symlinks,
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

/// Page budgets for streamed `Results` (well under the frame cap).
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
    let snapshot = store.get();
    match request {
        Request::Query { text, limit, .. } => {
            let query = match Query::parse(&text) {
                Ok(q) => q,
                Err(e) => return vec![bad_request(e.to_string())],
            };
            let options = SearchOptions {
                limit: if limit == 0 {
                    None
                } else {
                    Some(limit as usize)
                },
                cancel: CancelToken::new(),
                ..SearchOptions::default()
            };
            let result = ferrow_query::search(&snapshot.index, &query, &options);
            if result.cancelled {
                return vec![Response::Error {
                    code: ErrorCode::Cancelled,
                    msg: "query cancelled".into(),
                }];
            }
            let mut pages: Vec<Response> = Vec::new();
            let mut page: Vec<Vec<u8>> = Vec::new();
            let mut page_bytes = 0usize;
            let mut buf: Vec<u8> = Vec::new();
            for &id in &result.hits {
                snapshot.index.path_bytes(id, &mut buf);
                if !page.is_empty()
                    && (page.len() >= PAGE_MAX_PATHS || page_bytes + buf.len() > PAGE_MAX_BYTES)
                {
                    pages.push(Response::Results {
                        paths: std::mem::take(&mut page),
                        more: true,
                    });
                    page_bytes = 0;
                }
                page_bytes += buf.len();
                page.push(buf.clone());
            }
            pages.push(Response::Results {
                paths: page,
                more: false,
            });
            pages
        }
        Request::Count { text } => {
            let query = match Query::parse(&text) {
                Ok(q) => q,
                Err(e) => return vec![bad_request(e.to_string())],
            };
            let (n, cancelled) = ferrow_query::count(&snapshot.index, &query, &CancelToken::new());
            if cancelled {
                return vec![Response::Error {
                    code: ErrorCode::Cancelled,
                    msg: "count cancelled".into(),
                }];
            }
            vec![Response::Count { n: n as u64 }]
        }
        Request::Status => vec![Response::Status {
            entries: snapshot.index.len() as u64,
            mode: snapshot.mode.clone(),
            report: snapshot.report.clone(),
        }],
        Request::Cancel { .. } => vec![Response::Error {
            code: ErrorCode::Cancelled,
            msg: "cancellation is not wired to in-flight work in protocol v1".into(),
        }],
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
    let allowed = if system {
        caller.is_system_administrator()
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
            msg: "per-user visibility not yet available; system access requires SYSTEM/Administrators or root; development access requires its owner".into() }];
    }
    handle(store, request)
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
    eprintln!("ferrowd: listening on {}", config.endpoint);
    while !shutdown.is_requested() {
        let (stream, _caller) = match listener.accept() {
            Ok(pair) => pair,
            Err(_) if shutdown.is_requested() => break,
            Err(e) => return Err(e),
        };
        if shutdown.is_requested() {
            drop(stream);
            break;
        }
        let store = store.clone();
        let owner = owner.clone();
        std::thread::spawn(move || {
            let _ = serve_cancellable(stream, move |context, request| {
                handle_authorized(&store, request, &context.caller, &owner, system)
            });
        });
    }
    Ok(())
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
