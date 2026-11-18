use crate::{DaemonConfig, Shutdown, Snapshot, SnapshotStore, prepare_state};
use std::io;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

/// A clean host parks indefinitely. Only a change arms a one-shot coalescing
/// deadline, and saving returns it to an unbounded event wait.
pub fn scheduled_delay(dirty: bool) -> Option<Duration> {
    dirty.then_some(Duration::from_millis(250))
}

/// Least time between two index saves while running. An index save
/// writes the whole file (gigabytes on large machines).
pub const SAVE_GAP: Duration = Duration::from_secs(30 * 60);

pub struct Persistence {
    store: SnapshotStore,
    stop: Arc<AtomicBool>,
    wake: mpsc::SyncSender<()>,
    thread: Option<std::thread::JoinHandle<io::Result<()>>>,
}

/// One writer of the index file at a time: the background saver and the
/// live maintainer (which saves and maps every new index, ADR 0007).
static SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn save(config: &DaemonConfig, snapshot: Arc<Snapshot>) -> io::Result<()> {
    save_index(config, &snapshot.index)
}

fn save_index(config: &DaemonConfig, index: &ferrow_index::Index) -> io::Result<()> {
    let Some(path) = &config.index_path else {
        return Ok(());
    };
    prepare_state(config)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    index.save(path).map_err(io::Error::other)?;
    prepare_state(config)
}

/// Save a new index and map it back from the file (ADR 0007): the result
/// reads the same entries with the same ids, but from the file cache
/// instead of private memory, so the caller drops `index`. `None` without
/// an index file; an error leaves the caller with its heap index.
pub fn save_and_map(
    config: &DaemonConfig,
    index: &ferrow_index::Index,
) -> Option<io::Result<ferrow_index::Index>> {
    let path = config.index_path.as_ref()?;
    let _one = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let started = Instant::now();
    let saved = save_index(config, index);
    let took = started.elapsed();
    crate::health::STATE.save_done(took, saved.as_ref().err().map(|e| e.to_string()));
    if let Err(error) = saved {
        return Some(Err(error));
    }
    crate::eventlog::info(format!("index saved in {:.1} s", took.as_secs_f64()));
    Some(
        ferrow_index::Index::load(path)
            .map_err(io::Error::other)
            .and_then(|mapped| {
                if mapped.len() == index.len() && mapped.is_mapped() {
                    Ok(mapped)
                } else {
                    Err(io::Error::other("the saved index did not map back"))
                }
            }),
    )
}

impl Persistence {
    pub fn start(
        config: &DaemonConfig,
        store: &SnapshotStore,
        shutdown: &Shutdown,
    ) -> io::Result<Option<Self>> {
        // Live updates publish often but keep the same frozen index; only
        // a new frozen index (a scan) is worth writing again.
        let mut last: Option<std::sync::Weak<ferrow_index::Index>> = None;
        let published = store.clone();
        Self::start_with_saver(
            config,
            store,
            shutdown,
            SAVE_GAP,
            move |config, snapshot| {
                if last
                    .as_ref()
                    .and_then(std::sync::Weak::upgrade)
                    .is_some_and(|saved| Arc::ptr_eq(&saved, &snapshot.index))
                {
                    return Ok(());
                }
                let _one = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                // A mapped index is the file already (the maintainer saved
                // it); an index replaced meanwhile is not worth writing, and
                // writing it after the maintainer saved a newer one would
                // leave the older one on disk.
                if snapshot.index.is_mapped()
                    || !Arc::ptr_eq(&published.get().index, &snapshot.index)
                {
                    return Ok(());
                }
                let index = Arc::downgrade(&snapshot.index);
                let started = Instant::now();
                // The maintainer holds an in-memory compaction back while the
                // saver keeps the index it writes alive.
                let result = {
                    let _saving = crate::health::STATE.saving();
                    save(config, snapshot)
                };
                let took = started.elapsed();
                crate::health::STATE.save_done(took, result.as_ref().err().map(|e| e.to_string()));
                // A failure is logged once, by the worker that stops on it.
                if result.is_ok() {
                    crate::eventlog::info(format!("index saved in {:.1} s", took.as_secs_f64()));
                }
                result?;
                last = Some(index);
                Ok(())
            },
        )
    }

    fn start_with_saver(
        config: &DaemonConfig,
        store: &SnapshotStore,
        shutdown: &Shutdown,
        save_gap: Duration,
        mut saver: impl FnMut(&DaemonConfig, Arc<Snapshot>) -> io::Result<()> + Send + 'static,
    ) -> io::Result<Option<Self>> {
        if config.index_path.is_none() {
            return Ok(None);
        }
        let (wake, events) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let config = config.clone();
        let current = store.clone();
        let shutdown = shutdown.clone();
        let mut subscriber = store
            .changes
            .lock()
            .map_err(|_| io::Error::other("change subscriber poisoned"))?;
        if subscriber.is_some() {
            return Err(io::Error::other("store already has a persistence worker"));
        }
        *subscriber = Some(wake.clone());
        let thread = std::thread::Builder::new()
            .name("index-persist".into())
            .spawn(move || {
                let mut last_save: Option<Instant> = None;
                let result: io::Result<()> = (|| {
                    loop {
                        // No timer is armed until a publication or controlled exit.
                        match scheduled_delay(false) {
                            None => events.recv().map_err(io::Error::other)?,
                            Some(delay) => events.recv_timeout(delay).map_err(io::Error::other)?,
                        }
                        if !s.load(Ordering::Acquire) {
                            // Coalesce publications; and save a whole index
                            // at most once per gap (every service start
                            // rescans anyway, so a slightly older file only
                            // costs that rescan's diff, never correctness).
                            let mut deadline = Instant::now() + scheduled_delay(true).unwrap();
                            if let Some(at) = last_save {
                                deadline = deadline.max(at + save_gap);
                            }
                            while !s.load(Ordering::Acquire) {
                                let Some(left) = deadline.checked_duration_since(Instant::now())
                                else {
                                    break;
                                };
                                if events.recv_timeout(left).is_err() {
                                    break;
                                }
                            }
                        }
                        let mut saved_generation = current.generation.load(Ordering::Acquire);
                        saver(&config, current.get())?;
                        last_save = Some(Instant::now());
                        if s.load(Ordering::Acquire) {
                            loop {
                                let latest = current.generation.load(Ordering::Acquire);
                                if latest == saved_generation {
                                    break;
                                }
                                saved_generation = latest;
                                saver(&config, current.get())?;
                            }
                            return Ok(());
                        }
                    }
                })();
                if let Err(error) = &result {
                    crate::eventlog::error(format!("persistence failed: {error}"));
                    shutdown.request(&config.endpoint);
                }
                result
            });
        let thread = match thread {
            Ok(t) => t,
            Err(e) => {
                *subscriber = None;
                return Err(e);
            }
        };
        drop(subscriber);
        Ok(Some(Self {
            store: store.clone(),
            stop,
            wake,
            thread: Some(thread),
        }))
    }

    pub fn finish(self) -> io::Result<()> {
        self.finish_with(|| {})
    }

    fn finish_with(mut self, stopped: impl FnOnce()) -> io::Result<()> {
        self.stop.store(true, Ordering::Release);
        stopped();
        let _ = self.wake.try_send(()); // a queued change also observes stop
        self.store
            .changes
            .lock()
            .map_err(|_| io::Error::other("change subscriber poisoned"))?
            .take();
        self.thread
            .take()
            .unwrap()
            .join()
            .map_err(|_| io::Error::other("persistence worker panicked"))?
    }
}

impl Drop for Persistence {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            self.stop.store(true, Ordering::Release);
            let _ = self.wake.try_send(());
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_index_is_saved_and_read_back_from_its_file() {
        let dir = std::env::temp_dir().join(format!("ferrowd-save-map-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("index.bin");
        let config = DaemonConfig {
            endpoint: ferrow_ipc::Endpoint::named("ferrow-save-map-unused"),
            index_path: Some(path.clone()),
            roots: vec![],
            excludes: vec![],
            follow_symlinks: false,
        };
        let b = ferrow_index::IndexBuilder::new();
        let root = b.add_root(b"C:", ferrow_index::flags::DIR);
        for i in 0..1000 {
            b.add(root, format!("file {i}.txt").as_bytes(), 0);
        }
        let heap = b.freeze();
        let mapped = save_and_map(&config, &heap).unwrap().unwrap();
        assert!(mapped.is_mapped() && !heap.is_mapped());
        assert_eq!(mapped.len(), heap.len());
        assert_eq!(mapped.name(500), heap.name(500));
        assert!(mapped.heap_bytes() < heap.heap_bytes() / 10);
        // A second new index replaces the file while the first is mapped.
        let again = save_and_map(&config, &heap).unwrap().unwrap();
        assert_eq!(again.len(), mapped.len());
        drop((mapped, again));
        // No index file configured: nothing to do.
        let none = DaemonConfig {
            index_path: None,
            ..config
        };
        assert!(save_and_map(&none, &heap).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shutdown_during_save_persists_a_newer_publication() {
        let store = SnapshotStore::new(Snapshot::empty());
        let config = DaemonConfig {
            endpoint: ferrow_ipc::Endpoint::named("ferrow-save-race-unused"),
            index_path: Some("unused-mocked-save".into()),
            roots: vec![],
            excludes: vec![],
            follow_symlinks: false,
        };
        let shutdown = Shutdown::new();
        let saved = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = saved.clone();
        let (paused_tx, paused) = mpsc::channel();
        let (release, resumed) = mpsc::channel();
        let worker = Persistence::start_with_saver(
            &config,
            &store,
            &shutdown,
            Duration::ZERO,
            move |_, snapshot| {
                if snapshot.report == "A" {
                    paused_tx.send(()).unwrap();
                    resumed.recv_timeout(Duration::from_secs(2)).unwrap();
                }
                observed.lock().unwrap().push(snapshot.report.clone());
                Ok(())
            },
        )
        .unwrap()
        .unwrap();
        let mut a = Snapshot::empty();
        a.report = "A".into();
        store.swap(a);
        paused.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut b = Snapshot::empty();
        b.report = "B".into();
        store.swap(b);
        let (stopped_tx, stopped) = mpsc::channel();
        let finisher =
            std::thread::spawn(move || worker.finish_with(|| stopped_tx.send(()).unwrap()));
        stopped.recv_timeout(Duration::from_secs(2)).unwrap();
        release.send(()).unwrap();
        finisher.join().unwrap().unwrap();
        assert_eq!(
            *saved.lock().unwrap(),
            ["A", "B"],
            "controlled shutdown lost a pre-shutdown publication"
        );
    }
    #[test]
    fn saves_wait_for_the_gap_but_shutdown_saves_the_latest() {
        let store = SnapshotStore::new(Snapshot::empty());
        let config = DaemonConfig {
            endpoint: ferrow_ipc::Endpoint::named("ferrow-save-gap-unused"),
            index_path: Some("unused-mocked-save".into()),
            roots: vec![],
            excludes: vec![],
            follow_symlinks: false,
        };
        let shutdown = Shutdown::new();
        let (saved_tx, saved) = mpsc::channel();
        let worker = Persistence::start_with_saver(
            &config,
            &store,
            &shutdown,
            Duration::from_secs(60),
            move |_, snapshot| {
                saved_tx.send(snapshot.report.clone()).unwrap();
                Ok(())
            },
        )
        .unwrap()
        .unwrap();
        let mut a = Snapshot::empty();
        a.report = "A".into();
        store.swap(a);
        assert_eq!(saved.recv_timeout(Duration::from_secs(5)).unwrap(), "A");
        let mut b = Snapshot::empty();
        b.report = "B".into();
        store.swap(b);
        assert!(
            saved.recv_timeout(Duration::from_millis(600)).is_err(),
            "a second save within the gap"
        );
        worker.finish().unwrap();
        assert_eq!(saved.try_recv().unwrap(), "B", "shutdown saves the latest");
    }

    #[test]
    fn clean_idle_host_arms_no_periodic_task() {
        assert_eq!(scheduled_delay(false), None);
        assert!(scheduled_delay(true).is_some());
        assert_eq!(scheduled_delay(false), None);
    }
}
