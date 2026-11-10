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

pub struct Persistence {
    store: SnapshotStore,
    stop: Arc<AtomicBool>,
    wake: mpsc::SyncSender<()>,
    thread: Option<std::thread::JoinHandle<io::Result<()>>>,
}

fn save(config: &DaemonConfig, snapshot: Arc<Snapshot>) -> io::Result<()> {
    let Some(path) = &config.index_path else {
        return Ok(());
    };
    prepare_state(config)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    snapshot.index.save(path).map_err(io::Error::other)?;
    prepare_state(config)
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
        Self::start_with_saver(config, store, shutdown, move |config, snapshot| {
            if last
                .as_ref()
                .and_then(std::sync::Weak::upgrade)
                .is_some_and(|saved| Arc::ptr_eq(&saved, &snapshot.index))
            {
                return Ok(());
            }
            let index = Arc::downgrade(&snapshot.index);
            save(config, snapshot)?;
            last = Some(index);
            Ok(())
        })
    }

    fn start_with_saver(
        config: &DaemonConfig,
        store: &SnapshotStore,
        shutdown: &Shutdown,
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
                let result: io::Result<()> = (|| {
                    loop {
                        // No timer is armed until a publication or controlled exit.
                        match scheduled_delay(false) {
                            None => events.recv().map_err(io::Error::other)?,
                            Some(delay) => events.recv_timeout(delay).map_err(io::Error::other)?,
                        }
                        if !s.load(Ordering::Acquire) {
                            let deadline = Instant::now() + scheduled_delay(true).unwrap();
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
                    eprintln!("ferrowd: persistence failed: {error}");
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
        let worker =
            Persistence::start_with_saver(&config, &store, &shutdown, move |_, snapshot| {
                if snapshot.report == "A" {
                    paused_tx.send(()).unwrap();
                    resumed.recv_timeout(Duration::from_secs(2)).unwrap();
                }
                observed.lock().unwrap().push(snapshot.report.clone());
                Ok(())
            })
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
    fn clean_idle_host_arms_no_periodic_task() {
        assert_eq!(scheduled_delay(false), None);
        assert!(scheduled_delay(true).is_some());
        assert_eq!(scheduled_delay(false), None);
    }
}
