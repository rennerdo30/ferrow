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

/// Set once the live maintainer has saved an index: from then on it is the
/// only writer of the file. The background saver only writes before that
/// (an index loaded from an older file version, a walk without live
/// updates): an index it holds may already be older than the one the
/// maintainer saved and is about to publish.
static MAINTAINER_SAVED: AtomicBool = AtomicBool::new(false);

/// Set while the live maintainer builds an index into the file: the
/// background saver writes nothing meanwhile (the build renames its file
/// into place before the new index is published). Cleared when the build
/// ends, whatever its outcome.
static BUILDING: AtomicBool = AtomicBool::new(false);

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
    let _saving = crate::health::STATE.saving();
    let started = Instant::now();
    let saved = (|| -> io::Result<ferrow_index::Index> {
        prepare_state(config)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Mapped back without reading the file again: it was just written.
        let mapped = index.save_and_map(path).map_err(io::Error::other)?;
        prepare_state(config)?;
        Ok(mapped)
    })();
    let took = started.elapsed();
    crate::health::STATE.save_done(took, saved.as_ref().err().map(|e| e.to_string()));
    if saved.is_ok() {
        // Only a save that worked makes the maintainer the file's writer: a
        // failed one leaves its heap index to the background saver (whose
        // check against the published index still applies).
        MAINTAINER_SAVED.store(true, Ordering::Release);
        crate::eventlog::info(format!("index saved in {:.1} s", took.as_secs_f64()));
    }
    Some(saved)
}

/// The index file as the live maintainer builds it (ADR 0007, step 8): a scan
/// or a compaction writes its new index straight into the file instead of
/// building it in the heap and saving it. One writer of the file at a time:
/// the build holds [`SAVE_LOCK`] while the new file replaces the old one.
#[derive(Clone)]
pub struct IndexFile {
    config: Arc<DaemonConfig>,
    path: std::path::PathBuf,
}

/// A build into the index file is running (see [`IndexFile::prepare`]).
#[must_use = "the saver waits only while this lives"]
pub struct Building(());

impl Building {
    fn start() -> Building {
        BUILDING.store(true, Ordering::Release);
        Building(())
    }
}

impl Drop for Building {
    fn drop(&mut self) {
        BUILDING.store(false, Ordering::Release);
    }
}

/// Temporary files a save or build of another process left beside the index
/// (a crash or a power loss mid-way; a file build's is the size of the index)
/// are removed once they are this old.
const STALE_TEMPORARY: Duration = Duration::from_secs(60 * 60);

/// Whether the process that named a temporary file (`.ferrow[-kind]-<pid>-…`)
/// still runs. Unknown counts as running.
fn writer_alive(name: &str) -> bool {
    let Some(pid) = name
        .strip_prefix(".ferrow-")
        .map(|rest| rest.trim_start_matches(|c: char| c.is_ascii_alphabetic() || c == '-'))
        .and_then(|rest| rest.split('-').next())
        .and_then(|pid| pid.parse::<u32>().ok())
    else {
        return true;
    };
    process_alive(pid)
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    const STILL_ACTIVE: u32 = 259;
    // SAFETY: plain query of another process; the handle is closed below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        // Gone, or not ours to ask: only a missing process is certain.
        return std::io::Error::last_os_error().raw_os_error() != Some(87);
    }
    let mut code = 0u32;
    // SAFETY: a live handle and a writable code.
    let ok = unsafe { GetExitCodeProcess(handle, &mut code) } != 0;
    // SAFETY: the handle opened above.
    unsafe { CloseHandle(handle) };
    !ok || code == STILL_ACTIVE
}

#[cfg(not(windows))]
fn process_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Remove `.ferrow-<pid>-….tmp` files in `dir` of another process (this
/// one's may be in use) that nobody wrote for [`STALE_TEMPORARY`].
fn remove_stale_temporaries(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let own = format!("-{}-", std::process::id());
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let ours = name
            .strip_prefix(".ferrow-")
            .is_some_and(|rest| rest.ends_with(".tmp"));
        let this_process = name.starts_with(&format!(".ferrow{own}"))
            || name.starts_with(&format!(".ferrow-live{own}"))
            || name.starts_with(&format!(".ferrow-spill{own}"));
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|at| at.elapsed().is_ok_and(|age| age >= STALE_TEMPORARY));
        // A file whose writer still runs may be in use (a mapped build
        // writes without touching the file's time).
        if ours && !this_process && old && !writer_alive(name) {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => crate::eventlog::info(format!(
                    "removed a temporary file left by an interrupted save: {name}"
                )),
                Err(error) => crate::eventlog::warn(format!(
                    "could not remove a temporary file left by an interrupted save ({name}): {error}"
                )),
            }
        }
    }
}

impl IndexFile {
    /// `None` without an index path (a service that does not save).
    pub fn new(config: &DaemonConfig) -> Option<IndexFile> {
        Some(IndexFile {
            path: config.index_path.clone()?,
            config: Arc::new(config.clone()),
        })
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Make the state directory ready for a build that writes beside the index
    /// (its temporary file and the spilled shards): created, and protected as
    /// a save protects it.
    ///
    /// The background saver writes nothing until the returned guard is
    /// dropped (the build renames its file into place before the new index
    /// is published): keep it for the whole build, `built()` included. An
    /// error, or a panic in the build, drops it too.
    pub fn prepare(&self) -> io::Result<Building> {
        let building = Building::start();
        prepare_state(&self.config)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
            remove_stale_temporaries(parent);
        }
        Ok(building)
    }

    /// Where a build puts its index, replacing the file under [`SAVE_LOCK`].
    pub fn target(&self) -> ferrow_index::FileTarget<'_> {
        ferrow_index::FileTarget {
            path: &self.path,
            lock: Some(&SAVE_LOCK),
        }
    }

    /// Report a finished build: health and the event log, as a save reports,
    /// and the protection of the new file. `report` is what the build
    /// returned; a failed file build or spill is logged, not hidden.
    pub fn built(&self, report: &ferrow_walk::IndexFileReport) {
        // A build that replaced the file makes the maintainer its writer; one
        // that fell back to memory leaves its index to `save_and_map`, and to
        // the background saver if that fails too.
        if report.timings.is_some() {
            MAINTAINER_SAVED.store(true, Ordering::Release);
        }
        if let Some(t) = &report.timings {
            // What a save would have taken: the checksum, the write and the
            // replacement. The parts before are the build itself.
            let wrote = t.checksum + t.flush + t.replace;
            crate::health::STATE.save_done(wrote, None);
            if let Err(error) = prepare_state(&self.config) {
                crate::eventlog::warn(format!("could not protect the new index file: {error}"));
            }
            crate::eventlog::info(format!("index saved in {:.1} s", wrote.as_secs_f64()));
            crate::eventlog::info(format!(
                "index built into its file in {:.1} s ({:.1} s placing entries, {:.1} s folding names, {:.1} s sorting; {:.0} MB spilled while building)",
                t.total.as_secs_f64(),
                t.scatter.as_secs_f64(),
                t.fold.as_secs_f64(),
                t.sort.as_secs_f64(),
                t.spilled_bytes as f64 / 1e6,
            ));
        }
        if let Some(error) = &report.error {
            crate::health::STATE.save_done(Duration::ZERO, Some(error.clone()));
            crate::eventlog::warn(format!(
                "could not build the new index in its file ({error}); it was built in memory"
            ));
        }
        if let Some(error) = &report.spill_failure {
            crate::eventlog::warn(format!(
                "could not spill the build to disk ({error}); it stayed in memory"
            ));
        }
    }

    /// Save a heap index and map it back from the file (the fallback when a
    /// build could not go straight into the file); see [`save_and_map`].
    pub fn save_and_map(
        &self,
        index: &ferrow_index::Index,
    ) -> Option<io::Result<ferrow_index::Index>> {
        save_and_map(&self.config, index)
    }
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
                    || MAINTAINER_SAVED.load(Ordering::Acquire)
                    || BUILDING.load(Ordering::Acquire)
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

    /// Tests that start a build hold this: the state they check is global.
    static BUILDING_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn the_saver_waits_exactly_while_a_build_runs() {
        let _one = BUILDING_TEST.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ferrowd-building-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = |path: std::path::PathBuf| DaemonConfig {
            endpoint: ferrow_ipc::Endpoint::named("ferrow-building-unused"),
            index_path: Some(path),
            roots: vec![],
            excludes: vec![],
            follow_symlinks: false,
        };
        let good = IndexFile::new(&config(dir.join("index.bin"))).unwrap();
        {
            let _building = good.prepare().unwrap();
            assert!(BUILDING.load(Ordering::Acquire));
        }
        assert!(!BUILDING.load(Ordering::Acquire), "held only by the guard");
        // A directory that cannot be made (its parent is a file): the error
        // does not leave the saver held back.
        std::fs::write(dir.join("a-file"), b"x").unwrap();
        let bad =
            IndexFile::new(&config(dir.join("a-file").join("sub").join("index.bin"))).unwrap();
        assert!(bad.prepare().is_err());
        assert!(!BUILDING.load(Ordering::Acquire));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_writer_that_still_runs_keeps_its_temporary_file() {
        assert!(writer_alive(&format!(
            ".ferrow-{}-1-2.tmp",
            std::process::id()
        )));
        assert!(!writer_alive(".ferrow-4294967-1-2.tmp"));
        assert!(!writer_alive(".ferrow-spill-4294967-3.tmp"));
        assert!(writer_alive(".ferrow-odd.tmp"), "unknown counts as running");
    }

    #[test]
    fn stale_temporary_files_of_other_processes_are_removed() {
        let dir = std::env::temp_dir().join(format!("ferrowd-stale-tmp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(2 * 3600);
        let make = |name: &str, aged: bool| {
            let file = std::fs::File::create(dir.join(name)).unwrap();
            if aged {
                file.set_modified(old).unwrap();
            }
        };
        let own = std::process::id();
        make(".ferrow-4294967-2-3.tmp", true); // a process gone, old: removed
        make(".ferrow-live-4294967-abc.tmp", true); // the same for live tables
        make(".ferrow-spill-4294967-9.tmp", true); // and spilled shards
        make(".ferrow-4294967-2-4.tmp", false); // recent: may be in use
        make(&format!(".ferrow-{own}-3-1.tmp"), true); // this process, old
        make(&format!(".ferrow-{own}-2-5.tmp"), true); // this process
        make("index.bin", true); // not a temporary file
        remove_stale_temporaries(&dir);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        let mut want = vec![
            ".ferrow-4294967-2-4.tmp".to_string(),
            format!(".ferrow-{own}-3-1.tmp"),
            format!(".ferrow-{own}-2-5.tmp"),
            "index.bin".to_string(),
        ];
        want.sort();
        assert_eq!(left, want);
        let _ = std::fs::remove_dir_all(&dir);
    }

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
    fn a_build_into_the_index_file_replaces_it_and_leaves_the_save_lock_free() {
        let dir = std::env::temp_dir().join(format!("ferrowd-build-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("state").join("index.bin");
        let config = DaemonConfig {
            endpoint: ferrow_ipc::Endpoint::named("ferrow-build-file-unused"),
            index_path: Some(path.clone()),
            roots: vec![],
            excludes: vec![],
            follow_symlinks: false,
        };
        let file = IndexFile::new(&config).unwrap();
        assert!(
            IndexFile::new(&DaemonConfig {
                index_path: None,
                ..config.clone()
            })
            .is_none()
        );
        // The state directory does not exist yet: preparing it creates it.
        let _building = {
            let _one = BUILDING_TEST.lock().unwrap_or_else(|e| e.into_inner());
            file.prepare().unwrap()
        };
        drop(_building);

        let build = |entries: usize| {
            let b = ferrow_index::IndexBuilder::with_spill(
                ferrow_index::ColumnSet::DEFAULT,
                path.parent().unwrap(),
                16 * 1024,
            );
            let root = b.add_root(b"C:", ferrow_index::flags::DIR);
            for i in 0..entries {
                b.add(root, format!("file {i}.txt").as_bytes(), 0);
            }
            b.freeze_to_file_with(&file.target()).unwrap()
        };
        let (first, timings) = build(3_000);
        assert!(first.is_mapped());
        assert!(timings.spilled_bytes > 0, "the build spilled");
        file.built(&ferrow_walk::IndexFileReport {
            timings: Some(timings),
            ..Default::default()
        });
        assert!(SAVE_LOCK.try_lock().is_ok(), "the lock is released");

        // A second build replaces the file while the first is mapped.
        let (second, _) = build(4_000);
        assert_eq!(first.len(), 3_001);
        assert_eq!(second.len(), 4_001);
        assert_eq!(ferrow_index::Index::load(&path).unwrap().len(), 4_001);
        drop((first, second));
        let mut left: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["index.bin"], "no temporary or spill file is left");
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
