//! `ferrowd`, the Ferrow background index service (M2b).
//!
//! The daemon holds the index as an immutable [`Snapshot`]
//! behind an `Arc` swap: building a new index never blocks
//! readers, and swapping is one pointer store. Connections are
//! served one thread each over `ferrow-ipc`; the accept loop is
//! the only parked thread, so an idle daemon burns 0 % CPU
//! (no polling timers anywhere).
//!
//! Note on privacy: until M2c lands, the service returns every
//! indexed entry to any local caller the transport admits. The
//! per-user visibility filter is specified work, not done here.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use ferrow_index::{Index, IndexBuilder};
use ferrow_ipc::{Endpoint, ErrorCode, Listener, Request, Response, Stream, serve_stream};
use ferrow_query::{CancelToken, Query, SearchOptions};
use ferrow_walk::{ScanReport, WalkOptions};

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
/// `get` clones one `Arc` under a short read lock.
#[derive(Clone)]
pub struct SnapshotStore {
    inner: Arc<RwLock<Arc<Snapshot>>>,
}

impl SnapshotStore {
    pub fn new(snapshot: Snapshot) -> SnapshotStore {
        SnapshotStore {
            inner: Arc::new(RwLock::new(Arc::new(snapshot))),
        }
    }

    pub fn get(&self) -> Arc<Snapshot> {
        self.inner.read().expect("snapshot lock poisoned").clone()
    }

    /// Replace the current snapshot. Readers that already hold
    /// the old `Arc` keep querying it to completion.
    pub fn swap(&self, snapshot: Snapshot) {
        *self.inner.write().expect("snapshot lock poisoned") = Arc::new(snapshot);
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
pub fn default_service_index_path() -> PathBuf {
    if cfg!(windows) {
        let base = std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
        base.join("ferrow").join("index.bin")
    } else {
        PathBuf::from("/var/lib/ferrow/index.bin")
    }
}

/// Initial snapshot for a config: a saved index file wins, then
/// a walk of the configured roots (saved back to `index_path`
/// when set), else an empty index.
pub fn initial_snapshot(config: &DaemonConfig) -> Snapshot {
    if let Some(path) = &config.index_path
        && path.exists()
    {
        match Snapshot::from_file(path) {
            Ok(snapshot) => return snapshot,
            Err(e) => eprintln!("ferrowd: loading {}: {e}; starting empty", path.display()),
        }
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
            if let Err(e) = snapshot.index.save(path) {
                eprintln!("ferrowd: saving {}: {e}", path.display());
            }
        }
        return snapshot;
    }
    Snapshot::empty()
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

/// Cooperative shutdown for the accept loop. `request` also
/// wakes the parked `accept` with a throwaway connection, so
/// stopping never needs a polling timer.
#[derive(Clone, Default)]
pub struct Shutdown {
    flag: Arc<AtomicBool>,
}

impl Shutdown {
    pub fn new() -> Shutdown {
        Shutdown::default()
    }

    pub fn is_requested(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    pub fn request(&self, endpoint: &Endpoint) {
        self.flag.store(true, Ordering::SeqCst);
        let _ = Stream::connect(endpoint);
    }
}

/// Run the daemon until shutdown is requested: bind, accept,
/// one thread per connection. Blocking.
pub fn serve(
    config: &DaemonConfig,
    store: SnapshotStore,
    shutdown: Shutdown,
) -> std::io::Result<()> {
    let listener = Listener::bind(&config.endpoint)?;
    eprintln!("ferrowd: listening on {}", config.endpoint);
    while !shutdown.is_requested() {
        let (stream, _caller) = listener.accept()?;
        if shutdown.is_requested() {
            drop(stream);
            break;
        }
        let store = store.clone();
        std::thread::spawn(move || {
            let _ = serve_stream(stream, move |request| handle(&store, request));
        });
    }
    Ok(())
}
