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
use ferrow_query::{CancelToken, Query, SearchOptions};

/// Hits shown before the full result list is ready.
const FIRST_PAGE: usize = 200;

/// The current result set.
#[derive(Clone)]
pub enum Hits {
    /// The empty query: every entry, in name order (no copy).
    All,
    /// Entry ids in name order.
    Some(Arc<Vec<u32>>),
}

impl Hits {
    pub fn len(&self, index: &Index) -> usize {
        match self {
            Hits::All => index.len(),
            Hits::Some(ids) => ids.len(),
        }
    }

    pub fn get(&self, index: &Index, row: usize) -> Option<u32> {
        match self {
            Hits::All => index.sorted().get(row).copied(),
            Hits::Some(ids) => ids.get(row).copied(),
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
    },
    /// The query text does not parse.
    QueryError { generation: u64, message: String },
}

enum Command {
    Search {
        generation: u64,
        text: String,
        cancel: Arc<AtomicBool>,
    },
    Scan(ferrow_walk::WalkOptions),
    /// Pass an event from a background thread to the UI (with a wake).
    Forward(Event),
    /// A background scan finished; swap in its index.
    ScanDone {
        index: Box<Index>,
        elapsed: Duration,
        source: String,
    },
}

/// Handle owned by the UI.
pub struct Engine {
    commands: Sender<Command>,
    events: Receiver<Event>,
    generation: u64,
    in_flight: Option<Arc<AtomicBool>>,
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
        }
    }

    /// Start a search for `text`; the previous one is cancelled.
    /// Returns the generation that its results will carry.
    pub fn search(&mut self, text: &str) -> u64 {
        if let Some(old) = self.in_flight.take() {
            old.store(true, Ordering::Relaxed);
        }
        self.generation += 1;
        let cancel = Arc::new(AtomicBool::new(false));
        self.in_flight = Some(cancel.clone());
        let _ = self.commands.send(Command::Search {
            generation: self.generation,
            text: text.to_owned(),
            cancel,
        });
        self.generation
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
    /// Lets scan threads hand their result back to this worker.
    commands: Option<Sender<Command>>,
}

impl<W: Fn()> Worker<W> {
    fn new(events: Sender<Event>, wake: W) -> Self {
        Worker {
            events,
            wake,
            index: None,
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
        self.load_default();
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
            } => self.search(generation, &text, cancel),
            Command::Scan(options) => self.scan(options),
            Command::ScanDone {
                index,
                elapsed,
                source,
            } => self.set_index(*index, elapsed, source),
            Command::Forward(event) => self.send(event),
        }
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

    #[allow(
        clippy::needless_update,
        reason = "SearchOptions gains fields under ferrow-query's test-hooks feature"
    )]
    fn search(&self, generation: u64, text: &str, cancel: Arc<AtomicBool>) {
        let Some(index) = &self.index else { return };
        let started = Instant::now();
        let query = match Query::parse(text) {
            Ok(q) => q,
            Err(e) => {
                self.send(Event::QueryError {
                    generation,
                    message: e.to_string(),
                });
                return;
            }
        };
        if query.terms.is_empty() {
            self.send(Event::Results {
                generation,
                hits: Hits::All,
                complete: true,
                elapsed: started.elapsed(),
            });
            return;
        }
        let token = CancelToken::from_flag(cancel);
        // First page: stops early, so typing feels instant even when
        // the full list takes longer.
        let first = ferrow_query::search(
            index,
            &query,
            &SearchOptions {
                limit: Some(FIRST_PAGE),
                cancel: token.clone(),
                // Other fields exist with the workspace's test-hooks feature.
                ..Default::default()
            },
        );
        if first.cancelled {
            return;
        }
        let first_is_all = first.hits.len() < FIRST_PAGE;
        self.send(Event::Results {
            generation,
            hits: Hits::Some(Arc::new(first.hits)),
            complete: first_is_all,
            elapsed: started.elapsed(),
        });
        if first_is_all {
            return;
        }
        let full = ferrow_query::search(
            index,
            &query,
            &SearchOptions {
                limit: None,
                cancel: token,
                ..Default::default()
            },
        );
        if full.cancelled {
            return;
        }
        self.send(Event::Results {
            generation,
            hits: Hits::Some(Arc::new(full.hits)),
            complete: true,
            elapsed: started.elapsed(),
        });
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

    #[test]
    fn empty_query_is_all_without_copy() {
        let (w, rx) = worker();
        w.search(1, "  ", Arc::new(AtomicBool::new(false)));
        assert!(matches!(final_hits(&rx), Some(Hits::All)));
    }

    #[test]
    fn query_hits_in_name_order() {
        let (w, rx) = worker();
        w.search(1, "alpha", Arc::new(AtomicBool::new(false)));
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
        w.search(1, "a", Arc::new(AtomicBool::new(true)));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn bad_query_reports_error() {
        let (w, rx) = worker();
        w.search(7, "\"open", Arc::new(AtomicBool::new(false)));
        match rx.try_recv() {
            Ok(Event::QueryError { generation: 7, .. }) => {}
            _ => panic!("expected a query error"),
        }
    }
}
