//! Background search engine.
//!
//! The UI thread never touches the index on the interactive path:
//! loading, scanning and searching all run on one worker thread. Each
//! new query cancels the one in flight, and results arrive by message,
//! so a slow disk or a huge result set can never freeze the window.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use ferrow_index::Index;
use ferrow_query::{CancelToken, Query, SearchOptions, SortKey, SortStatus};

/// Hits shown before the full result list is ready.
const FIRST_PAGE: usize = 200;
/// Rows fetched from the Ferrow service per request.
const REMOTE_PAGE: usize = 500;
/// Loaded service rows kept per query; scrolling far away drops the rest.
const REMOTE_KEEP: usize = 20_000;
/// How often the app checks for a Ferrow service it is not connected to.
const SERVICE_PROBE_INTERVAL: Duration = Duration::from_secs(3);

/// The current result set.
#[derive(Clone)]
pub enum Hits {
    /// The empty query: every entry, in name order (no copy).
    All,
    /// Entry ids in name order.
    Some(Arc<Vec<u32>>),
    /// Results from the Ferrow service: the rows loaded so far, held in
    /// a small index of their own.
    Remote(Arc<RemoteView>),
}

/// The loaded part of a service result list.
pub struct RemoteView {
    pub generation: u64,
    /// Rows in the whole result list.
    pub total: usize,
    /// The loaded rows: each folder is a root, each row an entry below it.
    pub index: Arc<Index>,
    ids: std::collections::HashMap<usize, u32>,
}

impl Hits {
    pub fn len(&self, index: &Index) -> usize {
        match self {
            Hits::All => index.len(),
            Hits::Some(ids) => ids.len(),
            Hits::Remote(view) => view.total,
        }
    }

    pub fn get(&self, index: &Index, row: usize) -> Option<u32> {
        match self {
            Hits::All => index.sorted().get(row).copied(),
            Hits::Some(ids) => ids.get(row).copied(),
            Hits::Remote(view) => view.ids.get(&row).copied(),
        }
    }

    /// The index the hit ids refer to, when it is not the app's own.
    pub fn own_index(&self) -> Option<Arc<Index>> {
        match self {
            Hits::Remote(view) => Some(view.index.clone()),
            _ => None,
        }
    }

    /// For service results: the generation whose missing rows to fetch.
    pub fn remote_generation(&self) -> Option<u64> {
        match self {
            Hits::Remote(view) => Some(view.generation),
            _ => None,
        }
    }
}

pub enum Event {
    /// An index is ready to search.
    Loaded {
        index: Arc<Index>,
        elapsed: Duration,
        source: String,
    },
    /// No saved index exists yet.
    NoIndex { path: Option<PathBuf> },
    /// Loading or scanning failed.
    Failed(String),
    /// A scan started (the UI shows progress text).
    Scanning(String),
    /// Results for query generation `generation`. `complete` is false
    /// for the fast first page that precedes the full list.
    Results {
        generation: u64,
        hits: Hits,
        complete: bool,
        elapsed: Duration,
        /// The requested sort order is still being prepared in the
        /// background; these hits are in name order for now.
        sort_pending: bool,
        /// The index has no data for the requested sort column.
        sort_unavailable: bool,
    },
    /// A background sort order became ready: search again.
    SortReady,
    /// The Ferrow service answers this app: searches go to it.
    Remote {
        /// Entries in the service's index.
        entries: u64,
        report: String,
    },
    /// The service's index changed (live update): search again.
    IndexChanged,
    /// The query text does not parse.
    QueryError {
        generation: u64,
        message: String,
        /// Byte range of the problem in the query text.
        span: std::ops::Range<usize>,
    },
}

enum Command {
    Search {
        generation: u64,
        text: String,
        cancel: Arc<AtomicBool>,
        sort: SortKey,
        descending: bool,
    },
    Scan(ferrow_walk::WalkOptions),
    /// Load the service rows around `offset` for query `generation`.
    Rows {
        generation: u64,
        offset: usize,
    },
    /// Pass an event from a background thread to the UI (with a wake).
    Forward(Event),
    /// A background scan finished; swap in its index.
    ScanDone {
        index: Box<Index>,
        elapsed: Duration,
        source: String,
    },
    /// The Ferrow service answers now (it started after the app or came
    /// back after a restart): switch to it.
    ServiceUp,
}

/// Handle owned by the UI.
pub struct Engine {
    commands: Sender<Command>,
    events: Receiver<Event>,
    generation: u64,
    in_flight: Option<Arc<AtomicBool>>,
    /// Service pages already asked for: `(generation, offset)`.
    requested: std::collections::HashSet<(u64, usize)>,
}

impl Engine {
    /// Start the worker. `wake` is called after every event so the UI
    /// repaints only when there is something new (no idle polling).
    pub fn start(wake: impl Fn() + Send + 'static) -> Engine {
        let (commands, command_rx) = mpsc::channel();
        let (event_tx, events) = mpsc::channel();
        std::thread::Builder::new()
            .name("ferrow-engine".into())
            .spawn({
                let commands = commands.clone();
                move || {
                    Worker::new(event_tx, wake)
                        .with_commands(commands)
                        .run(command_rx)
                }
            })
            .expect("spawn engine thread");
        Engine {
            commands,
            events,
            generation: 0,
            in_flight: None,
            requested: std::collections::HashSet::new(),
        }
    }

    /// Start a search for `text`; the previous one is cancelled.
    /// Returns the generation that its results will carry.
    pub fn search(&mut self, text: &str, sort: SortKey, descending: bool) -> u64 {
        if let Some(old) = self.in_flight.take() {
            old.store(true, Ordering::Relaxed);
        }
        self.generation += 1;
        self.requested.clear();
        let cancel = Arc::new(AtomicBool::new(false));
        self.in_flight = Some(cancel.clone());
        let _ = self.commands.send(Command::Search {
            generation: self.generation,
            text: text.to_owned(),
            cancel,
            sort,
            descending,
        });
        self.generation
    }

    /// Load the service rows around `row` of the results of `generation`
    /// (no-op for local results or a superseded query).
    pub fn fetch_rows(&mut self, generation: u64, row: usize) {
        let offset = row / REMOTE_PAGE * REMOTE_PAGE;
        if self.requested.insert((generation, offset)) {
            let _ = self.commands.send(Command::Rows { generation, offset });
        }
    }

    /// Rebuild the index in the background. Search keeps working on the
    /// current index until the new one is ready.
    pub fn scan(&mut self, options: ferrow_walk::WalkOptions) {
        let _ = self.commands.send(Command::Scan(options));
    }

    pub fn poll(&self) -> Option<Event> {
        self.events.try_recv().ok()
    }
}

struct Worker<W: Fn()> {
    events: Sender<Event>,
    wake: W,
    index: Option<Arc<Index>>,
    /// Set while the Ferrow service answers searches.
    remote: Option<Remote>,
    /// Lets scan threads hand their result back to this worker.
    commands: Option<Sender<Command>>,
}

impl<W: Fn()> Worker<W> {
    fn new(events: Sender<Event>, wake: W) -> Self {
        Worker {
            events,
            wake,
            index: None,
            remote: None,
            commands: None,
        }
    }

    fn with_commands(mut self, commands: Sender<Command>) -> Self {
        self.commands = Some(commands);
        self
    }

    fn send(&self, event: Event) {
        let _ = self.events.send(event);
        (self.wake)();
    }

    fn run(mut self, commands: Receiver<Command>) {
        if !self.connect_service() {
            self.load_default();
            self.probe_service();
        }
        while let Ok(mut command) = commands.recv() {
            // Only the newest search matters; skip superseded ones.
            while let Ok(next) = commands.try_recv() {
                match next {
                    Command::Search { .. } if matches!(command, Command::Search { .. }) => {
                        command = next;
                    }
                    _ => {
                        self.handle(command);
                        command = next;
                    }
                }
            }
            self.handle(command);
        }
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Search {
                generation,
                text,
                cancel,
                sort,
                descending,
            } => {
                if self.remote.is_some() {
                    self.remote_search(generation, &text, sort, descending);
                } else {
                    self.search(generation, &text, cancel, sort, descending);
                }
            }
            Command::Scan(options) => self.scan(options),
            Command::Rows { generation, offset } => self.remote_rows(generation, offset),
            Command::ScanDone {
                index,
                elapsed,
                source,
            } => {
                // A local rebuild that finishes after the switch to the
                // service is stale.
                if self.remote.is_none() {
                    self.set_index(*index, elapsed, source);
                }
            }
            Command::Forward(event) => self.send(event),
            Command::ServiceUp => {
                if self.remote.is_none() && !self.connect_service() {
                    self.probe_service();
                }
            }
        }
    }

    /// Watch in the background for the Ferrow service to answer, for
    /// example while it still loads its index after boot or an update,
    /// and then switch to it (`Command::ServiceUp`).
    fn probe_service(&self) {
        let Some(commands) = self.commands.clone() else {
            return;
        };
        let _ = std::thread::Builder::new()
            .name("ferrow-service-probe".into())
            .spawn(move || {
                let endpoint = ferrow_ipc::Endpoint::default_service();
                loop {
                    std::thread::sleep(SERVICE_PROBE_INTERVAL);
                    let deadline = Instant::now() + Duration::from_millis(800);
                    let answers = ferrow_ipc::Client::connect_deadline(&endpoint, deadline)
                        .and_then(|mut client| client.status())
                        .is_ok();
                    if answers {
                        let _ = commands.send(Command::ServiceUp);
                        return;
                    }
                }
            });
    }

    fn load_default(&mut self) {
        let Some(path) = default_index_path() else {
            self.send(Event::NoIndex { path: None });
            return;
        };
        if !path.exists() {
            self.send(Event::NoIndex { path: Some(path) });
            return;
        }
        let started = Instant::now();
        match Index::load(&path) {
            Ok(index) => self.set_index(index, started.elapsed(), path.display().to_string()),
            Err(e) => self.send(Event::Failed(format!(
                "Could not load {}: {e}",
                path.display()
            ))),
        }
    }

    fn set_index(&mut self, index: Index, elapsed: Duration, source: String) {
        let index = Arc::new(index);
        self.index = Some(index.clone());
        self.send(Event::Loaded {
            index,
            elapsed,
            source,
        });
    }

    fn scan(&mut self, options: ferrow_walk::WalkOptions) {
        let source = options
            .roots
            .iter()
            .map(|r| r.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        self.send(Event::Scanning(format!("Indexing {source}…")));
        let Some(commands) = self.commands.clone() else {
            return;
        };
        // The walk runs on its own thread, so searches keep being answered
        // from the current index while it runs (never freeze).
        let spawned = std::thread::Builder::new()
            .name("ferrow-scan".into())
            .spawn(move || {
                let started = Instant::now();
                let (index, report) = ferrow_walk::scan(&options);
                if let Some(fatal) = report.fatal_error {
                    let _ = commands.send(Command::Forward(Event::Failed(fatal)));
                    return;
                }
                if let Some(path) = default_index_path() {
                    if let Some(dir) = path.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    if let Err(e) = index.save(&path) {
                        let _ = commands.send(Command::Forward(Event::Failed(format!(
                            "Indexed, but saving failed: {e}"
                        ))));
                    }
                }
                let _ = commands.send(Command::ScanDone {
                    index: Box::new(index),
                    elapsed: started.elapsed(),
                    source,
                });
            });
        if let Err(e) = spawned {
            self.send(Event::Failed(format!("Could not start indexing: {e}")));
        }
    }

    /// Use the Ferrow service when it runs and answers this user: its
    /// index covers every drive and stays current by itself. Starts a
    /// watcher that reports its live changes. Returns false to use the
    /// app's own index instead.
    fn connect_service(&mut self) -> bool {
        let endpoint = ferrow_ipc::Endpoint::default_service();
        let connect = || {
            let deadline = Instant::now() + Duration::from_millis(800);
            let mut client = ferrow_ipc::Client::connect_deadline(&endpoint, deadline)?;
            client.clear_deadline();
            std::io::Result::Ok(client)
        };
        let Ok(mut client) = connect() else {
            return false;
        };
        let Ok((entries, _mode, report)) = client.status() else {
            return false;
        };
        self.remote = Some(Remote {
            client,
            current: None,
        });
        self.send(Event::Remote { entries, report });
        if let (Some(commands), Ok(mut watcher)) = (self.commands.clone(), connect()) {
            let _ = std::thread::Builder::new()
                .name("ferrow-watch".into())
                .spawn(move || {
                    let Ok(mut seen) = watcher.watch(u64::MAX) else {
                        return;
                    };
                    // Sleeps until the service publishes a change.
                    while let Ok(now) = watcher.watch(seen) {
                        seen = now;
                        if let Ok((entries, _, report)) = watcher.status() {
                            let _ =
                                commands.send(Command::Forward(Event::Remote { entries, report }));
                        }
                        if commands
                            .send(Command::Forward(Event::IndexChanged))
                            .is_err()
                        {
                            return;
                        }
                    }
                });
        }
        true
    }

    /// The service stopped answering: continue with the app's own index.
    fn service_lost(&mut self, error: std::io::Error) {
        self.remote = None;
        self.send(Event::Failed(format!(
            "The Ferrow service stopped answering ({error}); using this app's own index."
        )));
        self.load_default();
        self.probe_service();
    }

    fn remote_search(&mut self, generation: u64, text: &str, sort: SortKey, descending: bool) {
        let started = Instant::now();
        if let Err(e) = Query::parse(text) {
            self.send(Event::QueryError {
                generation,
                message: e.to_string(),
                span: e.span(),
            });
            return;
        }
        let Some(remote) = self.remote.as_mut() else {
            return;
        };
        let client = &mut remote.client;
        let result = client.count(text).and_then(|total| {
            let (rows, columns, pending) =
                client.page(text, 0, REMOTE_PAGE as u32, remote_sort(sort), descending)?;
            Ok((total, rows, columns, pending))
        });
        let (total, rows, columns, pending) = match result {
            Ok(r) => r,
            Err(e)
                if e.kind() == std::io::ErrorKind::InvalidData
                    && e.to_string().contains("column unavailable") =>
            {
                // The service's index has no data for this sort column
                // (for example while its first scan runs): say so like a
                // local index does, never as a search-text error.
                let empty = RemoteQuery {
                    generation,
                    text: text.to_owned(),
                    sort,
                    descending,
                    total: 0,
                    columns: 0,
                    rows: std::collections::BTreeMap::new(),
                };
                self.send(Event::Results {
                    generation,
                    hits: empty.hits(),
                    complete: true,
                    elapsed: started.elapsed(),
                    sort_pending: false,
                    sort_unavailable: true,
                });
                return;
            }
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                // The service refused the query: show its reason, keep the
                // connection.
                self.send(Event::QueryError {
                    generation,
                    message: e.to_string(),
                    span: 0..0,
                });
                return;
            }
            Err(e) => return self.service_lost(e),
        };
        let mut query = RemoteQuery {
            generation,
            text: text.to_owned(),
            sort,
            descending,
            total: total as usize,
            columns,
            rows: std::collections::BTreeMap::new(),
        };
        query.insert(0, rows);
        let hits = query.hits();
        if let Some(remote) = self.remote.as_mut() {
            remote.current = Some(query);
        }
        self.send(Event::Results {
            generation,
            hits,
            complete: true,
            elapsed: started.elapsed(),
            sort_pending: pending,
            sort_unavailable: false,
        });
    }

    fn remote_rows(&mut self, generation: u64, offset: usize) {
        let Some(remote) = self.remote.as_mut() else {
            return;
        };
        let Some(query) = remote
            .current
            .as_mut()
            .filter(|q| q.generation == generation)
        else {
            return;
        };
        let started = Instant::now();
        let page = remote.client.page(
            &query.text,
            offset as u32,
            REMOTE_PAGE as u32,
            remote_sort(query.sort),
            query.descending,
        );
        let (rows, _, pending) = match page {
            Ok(page) => page,
            Err(e) => return self.service_lost(e),
        };
        query.insert(offset, rows);
        let hits = query.hits();
        self.send(Event::Results {
            generation,
            hits,
            complete: true,
            elapsed: started.elapsed(),
            sort_pending: pending,
            sort_unavailable: false,
        });
    }

    #[allow(
        clippy::needless_update,
        reason = "SearchOptions gains fields under ferrow-query's test-hooks feature"
    )]
    fn search(
        &self,
        generation: u64,
        text: &str,
        cancel: Arc<AtomicBool>,
        sort: SortKey,
        descending: bool,
    ) {
        let Some(index) = &self.index else { return };
        let started = Instant::now();
        let query = match Query::parse(text) {
            Ok(q) => q,
            Err(e) => {
                self.send(Event::QueryError {
                    generation,
                    message: e.to_string(),
                    span: e.span(),
                });
                return;
            }
        };
        let plain_order = sort == SortKey::Name && !descending;
        if query.terms.is_empty() && query.expression.is_none() && plain_order {
            self.send(Event::Results {
                generation,
                hits: Hits::All,
                complete: true,
                elapsed: started.elapsed(),
                sort_pending: false,
                sort_unavailable: false,
            });
            return;
        }
        let token = CancelToken::from_flag(cancel);
        let options = |limit| SearchOptions {
            limit,
            cancel: token.clone(),
            sort,
            descending,
            // Other fields exist with the workspace's test-hooks feature.
            ..Default::default()
        };
        // First page: stops early, so typing feels instant even when
        // the full list takes longer.
        let first = ferrow_query::search(index, &query, &options(Some(FIRST_PAGE)));
        if first.cancelled {
            return;
        }
        let pending = first.sort_status == SortStatus::Pending;
        if pending {
            self.notify_when_sorted(index, sort);
        }
        let unavailable = first.sort_unavailable.is_some();
        let first_is_all = first.hits.len() < FIRST_PAGE;
        self.send(Event::Results {
            generation,
            hits: Hits::Some(Arc::new(first.hits)),
            complete: first_is_all,
            elapsed: started.elapsed(),
            sort_pending: pending,
            sort_unavailable: unavailable,
        });
        if first_is_all {
            return;
        }
        let full = ferrow_query::search(index, &query, &options(None));
        if full.cancelled {
            return;
        }
        self.send(Event::Results {
            generation,
            hits: Hits::Some(Arc::new(full.hits)),
            complete: true,
            elapsed: started.elapsed(),
            sort_pending: full.sort_status == SortStatus::Pending,
            sort_unavailable: full.sort_unavailable.is_some(),
        });
    }

    /// Ask for a wake-up when a background sort order is ready. A small
    /// helper thread waits on the index's sort ticket, so neither the UI
    /// nor this worker ever blocks. It gives up if the index is replaced.
    fn notify_when_sorted(&self, index: &Arc<Index>, sort: SortKey) {
        let Some(commands) = self.commands.clone() else {
            return;
        };
        let _ = index.request_sort(sort);
        let Some(ticket) = index.sort_ticket(sort) else {
            return;
        };
        let weak = Arc::downgrade(index);
        let _ = std::thread::Builder::new()
            .name("ferrow-sort-wait".into())
            .spawn(move || {
                loop {
                    match ticket.wait_timeout(Duration::from_secs(1)) {
                        SortStatus::Pending => {
                            // Stop waiting for a retired index.
                            if weak.strong_count() == 0 {
                                return;
                            }
                        }
                        SortStatus::Ready => {
                            let _ = commands.send(Command::Forward(Event::SortReady));
                            return;
                        }
                        _ => return,
                    }
                }
            });
    }
}

struct Remote {
    client: ferrow_ipc::Client<ferrow_ipc::Stream>,
    current: Option<RemoteQuery>,
}

struct RemoteQuery {
    generation: u64,
    text: String,
    sort: SortKey,
    descending: bool,
    total: usize,
    columns: u32,
    rows: std::collections::BTreeMap<usize, ferrow_ipc::FileRow>,
}

impl RemoteQuery {
    fn insert(&mut self, offset: usize, rows: Vec<ferrow_ipc::FileRow>) {
        for (i, row) in rows.into_iter().enumerate() {
            self.rows.insert(offset + i, row);
        }
        // Keep memory bounded: drop the rows farthest from this page.
        while self.rows.len() > REMOTE_KEEP {
            let first = *self.rows.keys().next().expect("non-empty");
            let last = *self.rows.keys().next_back().expect("non-empty");
            let drop = if offset.abs_diff(first) > offset.abs_diff(last) {
                first
            } else {
                last
            };
            self.rows.remove(&drop);
        }
    }

    /// The loaded rows as hits over a small index of their own.
    fn hits(&self) -> Hits {
        let columns = ferrow_index::ColumnSet::from_bits(self.columns).unwrap_or_default();
        let builder = ferrow_index::IndexBuilder::with_columns(columns);
        let mut folders: std::collections::HashMap<&[u8], u32> = std::collections::HashMap::new();
        let mut ids = std::collections::HashMap::with_capacity(self.rows.len());
        for (&row, file) in &self.rows {
            let metadata = ferrow_index::EntryMetadata {
                size: file.size,
                modified: file.modified,
                created: file.created,
                accessed: file.accessed,
                attributes: file.attributes,
            };
            let flags = if file.is_dir {
                ferrow_index::flags::DIR
            } else {
                0
            };
            let split = file
                .path
                .iter()
                .rposition(|&b| b == b'\\' || b == b'/')
                .filter(|&at| at + 1 < file.path.len());
            let id = match split {
                Some(at) => {
                    // `C:\a.txt` and `/a.txt` live in the drive root itself:
                    // keep its separator.
                    let folder_end = if file.path[..at].iter().any(|&b| b == b'\\' || b == b'/') {
                        at
                    } else {
                        at + 1
                    };
                    let folder = &file.path[..folder_end];
                    let parent = *folders
                        .entry(folder)
                        .or_insert_with(|| builder.add_root(folder, ferrow_index::flags::DIR));
                    builder.add_with_metadata(parent, &file.path[at + 1..], flags, metadata)
                }
                None => builder.add_with_metadata(
                    ferrow_index::ROOT_PARENT,
                    &file.path,
                    flags,
                    metadata,
                ),
            };
            ids.insert(row, id);
        }
        Hits::Remote(Arc::new(RemoteView {
            generation: self.generation,
            total: self.total,
            index: Arc::new(builder.freeze()),
            ids,
        }))
    }
}

fn remote_sort(sort: SortKey) -> ferrow_ipc::Sort {
    match sort {
        SortKey::Name => ferrow_ipc::Sort::Name,
        SortKey::Size => ferrow_ipc::Sort::Size,
        SortKey::Modified => ferrow_ipc::Sort::Modified,
        SortKey::Created => ferrow_ipc::Sort::Created,
        SortKey::Accessed => ferrow_ipc::Sort::Accessed,
    }
}

/// Same location as the CLI's default index.
pub fn default_index_path() -> Option<PathBuf> {
    let base = |name: &str| {
        let p = PathBuf::from(std::env::var_os(name)?);
        (p.is_absolute()).then_some(p)
    };
    let dir = if cfg!(windows) {
        base("LOCALAPPDATA")?.join("ferrow")
    } else if cfg!(target_os = "macos") {
        base("HOME")?
            .join("Library")
            .join("Application Support")
            .join("ferrow")
    } else if let Some(xdg) = base("XDG_DATA_HOME") {
        xdg.join("ferrow")
    } else {
        base("HOME")?.join(".local").join("share").join("ferrow")
    };
    Some(dir.join("index.bin"))
}

pub fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrow_index::{IndexBuilder, flags};

    fn sample() -> Arc<Index> {
        let b = IndexBuilder::new();
        let root = b.add_root(b"/r", flags::DIR);
        for name in ["alpha.txt", "beta.rs", "Alphabet", "gamma"] {
            b.add(root, name.as_bytes(), 0);
        }
        Arc::new(b.freeze())
    }

    fn worker() -> (Worker<impl Fn()>, Receiver<Event>) {
        let (tx, rx) = mpsc::channel();
        let mut w = Worker::new(tx, || {});
        w.index = Some(sample());
        (w, rx)
    }

    fn final_hits(rx: &Receiver<Event>) -> Option<Hits> {
        let mut last = None;
        while let Ok(e) = rx.try_recv() {
            if let Event::Results {
                hits,
                complete: true,
                ..
            } = e
            {
                last = Some(hits);
            }
        }
        last
    }

    // Service paths are Windows paths.
    #[cfg(windows)]
    #[test]
    fn service_rows_become_folder_and_name_entries() {
        let row = |path: &str, is_dir| ferrow_ipc::FileRow {
            path: path.as_bytes().to_vec(),
            size: Some(5),
            is_dir,
            ..Default::default()
        };
        let mut q = RemoteQuery {
            generation: 3,
            text: String::new(),
            sort: SortKey::Name,
            descending: false,
            total: 10,
            columns: ferrow_index::ColumnSet::SIZE.bits(),
            rows: std::collections::BTreeMap::new(),
        };
        q.insert(
            4,
            vec![
                row(r"C:\a.txt", false),
                row(r"C:\Users\me\Docs", true),
                row(r"C:\", true),
            ],
        );
        let hits = q.hits();
        let index = hits.own_index().unwrap();
        assert_eq!(hits.len(&index), 10);
        assert_eq!(hits.remote_generation(), Some(3));
        assert!(hits.get(&index, 0).is_none(), "not loaded yet");
        let path = |row| {
            let id = hits.get(&index, row).unwrap();
            (
                index.path(id).to_string_lossy().into_owned(),
                index.flags(id) & flags::DIR != 0,
            )
        };
        let sep = std::path::MAIN_SEPARATOR;
        // Folder roots keep the service's spelling; the app joins the name
        // with its own separator.
        assert_eq!(path(4).0, r"C:\a.txt");
        assert_eq!(path(5), (format!(r"C:\Users\me{sep}Docs"), true));
        assert_eq!(path(6), (r"C:\".to_string(), true));
        let id = hits.get(&index, 4).unwrap();
        assert_eq!(index.name(id), b"a.txt");
        assert_eq!(index.metadata(id).size, Some(5));
    }

    #[test]
    fn empty_query_is_all_without_copy() {
        let (w, rx) = worker();
        w.search(
            1,
            "  ",
            Arc::new(AtomicBool::new(false)),
            SortKey::Name,
            false,
        );
        assert!(matches!(final_hits(&rx), Some(Hits::All)));
    }

    #[test]
    fn query_hits_in_name_order() {
        let (w, rx) = worker();
        w.search(
            1,
            "alpha",
            Arc::new(AtomicBool::new(false)),
            SortKey::Name,
            false,
        );
        let index = w.index.clone().unwrap();
        let Some(Hits::Some(ids)) = final_hits(&rx) else {
            panic!("no hits")
        };
        let names: Vec<_> = ids.iter().map(|&id| index.name(id).to_vec()).collect();
        assert_eq!(names, vec![b"alpha.txt".to_vec(), b"Alphabet".to_vec()]);
    }

    #[test]
    fn cancelled_search_sends_nothing() {
        let (w, rx) = worker();
        w.search(
            1,
            "a",
            Arc::new(AtomicBool::new(true)),
            SortKey::Name,
            false,
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn bad_query_reports_error() {
        let (w, rx) = worker();
        w.search(
            7,
            "\"open",
            Arc::new(AtomicBool::new(false)),
            SortKey::Name,
            false,
        );
        match rx.try_recv() {
            Ok(Event::QueryError { generation: 7, .. }) => {}
            _ => panic!("expected a query error"),
        }
    }
}
