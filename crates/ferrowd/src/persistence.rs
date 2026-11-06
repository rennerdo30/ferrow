use crate::{DaemonConfig, Shutdown, SnapshotStore, prepare_state};
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

fn save(config: &DaemonConfig, store: &SnapshotStore) -> io::Result<()> {
    let Some(path) = &config.index_path else {
        return Ok(());
    };
    prepare_state(config)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    store.get().index.save(path).map_err(io::Error::other)?;
    prepare_state(config)
}

impl Persistence {
    pub fn start(
        config: &DaemonConfig,
        store: &SnapshotStore,
        shutdown: &Shutdown,
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
                        save(&config, &current)?;
                        if s.load(Ordering::Acquire) {
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

    pub fn finish(mut self) -> io::Result<()> {
        self.stop.store(true, Ordering::Release);
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
    fn clean_idle_host_arms_no_periodic_task() {
        assert_eq!(scheduled_delay(false), None);
        assert!(scheduled_delay(true).is_some());
        assert_eq!(scheduled_delay(false), None);
    }
}
