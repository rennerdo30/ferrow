//! Windows side of live updates: one change-journal reader thread per
//! NTFS volume and the maintainer thread that rescans and applies batches.
//!
//! Idle cost is zero: readers sleep inside `FSCTL_READ_USN_JOURNAL` until
//! the volume changes, and the maintainer sleeps on a channel. A batch is
//! collected for [`BATCH`] after the first change, then applied and
//! published, so a change shows up in results well within a second.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ferrow_ipc::health::{JOURNAL_NONE, JOURNAL_OK, MODE_LIVE, MODE_UNAVAILABLE};
use ferrow_walk::subtree::{Subtree, WalkError, walk_subtree};
use ferrow_walk::{VolumeMap, WalkOptions, ntfs};

use crate::health::{STATE, VolumeState};
use crate::live::{
    Applied, Change, Due, Live, Load, MAX_WALK_TIME, Maintenance, NOTABLE_WALK, PublishGate,
    REASON_ADMIN, REASON_LARGE_MOVE, Reason, ReasonKind, RecordSource, RecordState, WALK_SLACK,
    WalkJob, compact_and_publish, index_parts, walk_limits_text,
};
use crate::memory::Breakdown;
use crate::{Snapshot, SnapshotStore, eventlog};

/// How long changes are collected after the first one of a batch.
const BATCH: Duration = Duration::from_millis(200);
/// `USN_REASON_FILE_CREATE`.
const REASON_CREATE: u32 = 0x100;

enum Message {
    Records {
        device: String,
        records: Vec<(u64, u32)>,
    },
    /// The journal was deleted, recreated or overrun: rescan. `fatal`: the
    /// reader has stopped and is not restarted until the service restarts.
    Reset {
        device: String,
        reason: String,
        fatal: bool,
    },
    /// An administrator asked for a rescan now.
    RescanNow,
    /// The folder reader finished a folder.
    Walk(WalkEvent),
}

/// What the folder reader reports.
enum WalkEvent {
    /// It opened its volumes and takes jobs.
    Ready {
        generation: u64,
    },
    /// It began a folder (the watchdog's start time).
    Started {
        generation: u64,
        id: u64,
    },
    Done(WalkDone),
}

/// A folder the reader thread finished (or failed to read).
struct WalkDone {
    /// The live session it belongs to: a result of an older one is dropped.
    generation: u64,
    id: u64,
    result: Result<Subtree, WalkError>,
    took: Duration,
}

/// What `next_batch` found.
enum Batch {
    /// Changed records and finished folder reads (both may be empty: the
    /// maintainer woke for a pending rescan).
    Changes {
        changes: Vec<Change>,
        events: Vec<WalkEvent>,
    },
    Reset {
        device: String,
        reason: String,
        fatal: bool,
    },
    RescanNow,
}

/// Where a requested rescan is delivered: the maintainer's channel.
static CONTROL: Mutex<Option<Sender<Message>>> = Mutex::new(None);

/// Ask the maintainer for a rescan of every volume now (bypassing the rate
/// limit once). A scan already running, or one already requested, answers
/// instead of queueing another.
pub fn request_rescan() -> (bool, String) {
    if let Err(why) = STATE.claim_rescan(Instant::now()) {
        return (false, why);
    }
    let sender = CONTROL.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if sender.is_some_and(|tx| tx.send(Message::RescanNow).is_ok()) {
        return (
            true,
            "rescan requested; every volume is scanned again now".into(),
        );
    }
    STATE.release_rescan();
    (
        false,
        "live updates are not running on this service (no raw volume access, or they stopped); restart the service to rescan"
            .into(),
    )
}

/// A root for comparison: case, separators and the trailing separator ignored.
fn root_key(root: &str) -> String {
    root.replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

/// Why a volume root that the fast read did not cover was walked: the error
/// the read gave when there is one, a guess only when there is none.
fn walked_reason(can_read_volume: bool, error: Option<&str>) -> String {
    if !can_read_volume {
        return "reading the MFT needs raw volume access (administrator or SYSTEM)".into();
    }
    match error.map(str::trim).filter(|e| !e.is_empty()) {
        Some(error) => format!("reading the MFT failed ({error}); walked instead"),
        None => "the volume is not NTFS or its MFT could not be read; walked instead".into(),
    }
}

/// How each configured root is read and, for volumes read through their MFT,
/// what the scan found (device, entries, journal).
fn describe_volumes(
    options: &WalkOptions,
    base: &ferrow_index::Index,
    maps: &[VolumeMap],
    read_errors: &[(std::path::PathBuf, String)],
) -> Vec<VolumeState> {
    let mut buf = Vec::new();
    let found: Vec<(Vec<String>, &VolumeMap)> = maps
        .iter()
        .map(|m| {
            let roots = m
                .roots
                .iter()
                .map(|&(_, id)| {
                    base.path_bytes(id, &mut buf);
                    root_key(&String::from_utf8_lossy(&buf))
                })
                .collect();
            (roots, m)
        })
        .collect();
    options
        .roots
        .iter()
        .map(|root| {
            let text = root.display().to_string();
            if options.follow_symlinks {
                return VolumeState::walked(
                    text,
                    "following symbolic links needs a directory walk",
                );
            }
            if !ferrow_walk::is_volume_root(root) {
                return VolumeState::walked(text, "");
            }
            let key = root_key(&text);
            let Some((_, map)) = found.iter().find(|(roots, _)| roots.contains(&key)) else {
                let error = read_errors
                    .iter()
                    .find(|(dir, _)| root_key(&dir.display().to_string()) == key)
                    .map(|(_, error)| error.as_str());
                let why = walked_reason(ntfs::can_read_volume(root), error);
                return VolumeState::walked(text, &why);
            };
            let journal = map.journal.is_some();
            VolumeState {
                root: text,
                device: Some(map.device.clone()),
                mode: if journal { MODE_LIVE } else { MODE_UNAVAILABLE },
                reason: if journal {
                    String::new()
                } else {
                    "this volume has no change journal".into()
                },
                entries: Some(map.entries.len() as u64),
                journal: if journal { JOURNAL_OK } else { JOURNAL_NONE },
                // A journal that could not be re-checked after the MFT read:
                // still live, with what may be stale.
                journal_detail: map.warning.clone().unwrap_or_default(),
            }
        })
        .collect()
}

/// Log where the service's memory is after a scan and before a rescan
/// (FR-37): process counters, bytes per entry by group, the largest
/// components and the unattributed remainder. Computed here only, never per
/// request or batch.
fn log_memory(when: &str, live: Option<&Live>, base: &ferrow_index::Index, store: &SnapshotStore) {
    let breakdown = match live {
        Some(live) => {
            let snapshot = store.get();
            live.breakdown(snapshot.live.as_deref(), 0)
        }
        None => Breakdown::new(base.len(), index_parts(base, "index"), 0),
    };
    for line in breakdown.event_lines(when) {
        eventlog::info(line);
    }
    if base.is_mapped() {
        eventlog::info(format!(
            "memory {when}: the index is read from its saved file: {} MiB mapped (file cache, not private memory)",
            base.mapped_bytes() >> 20
        ));
    }
}

struct Volumes {
    handles: Vec<(std::fs::File, usize)>,
}

impl RecordSource for Volumes {
    fn fetch(&self, volume: usize, record: u64) -> Option<RecordState> {
        let (file, size) = self.handles.get(volume)?;
        ntfs::fetch_state(file, record, *size).map(|(info, names)| RecordState { info, names })
    }

    fn certainly_free(&self, volume: usize, record: u64) -> bool {
        self.handles
            .get(volume)
            .is_some_and(|(file, size)| ntfs::record_is_free(file, record, *size))
    }
}

/// Hand a batch's folder-reader events to the live state: a reader that is
/// ready, or began a folder, shows life (the watchdog's clock), and the
/// events of an older live session are dropped. Returns the finished folders
/// of this session.
fn route_events(
    live: &mut Live,
    generation: u64,
    events: Vec<WalkEvent>,
    now: Instant,
) -> Vec<WalkDone> {
    let mut done = Vec::new();
    for event in events {
        match event {
            WalkEvent::Ready { generation: g } if g == generation => live.reader_ready(now),
            WalkEvent::Started { generation: g, id } if g == generation => {
                live.walk_started(id, now);
            }
            WalkEvent::Done(d) if d.generation == generation => done.push(d),
            _ => {}
        }
    }
    done
}

/// Start the maintainer: a full scan now (the saved index is only a quick
/// start), then live updates for every NTFS volume with a journal.
/// Whether [`start`] would keep `options` live: some root is read through
/// its MFT (raw volume access, administrator or SYSTEM).
pub fn will_run(options: &WalkOptions) -> bool {
    let fast = |root: &std::path::Path| {
        (options.ntfs == ferrow_walk::NtfsMode::AllRoots
            || (options.ntfs == ferrow_walk::NtfsMode::VolumeRoots
                && ferrow_walk::is_volume_root(root)))
            && ntfs::can_read_volume(root)
    };
    !options.follow_symlinks && options.roots.iter().any(|r| fast(r))
}

pub fn start(store: SnapshotStore, options: WalkOptions, persist: Option<Persist>) {
    // Without raw volume access (not elevated) there is nothing live to
    // keep; the initial walk stands.
    if !will_run(&options) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("index-live".into())
        .spawn(move || maintain(store, options, persist));
    if let Err(error) = spawned {
        eventlog::error(format!("live updates unavailable: {error}"));
    }
}

/// Folder readers given up as stuck since the service started: each leaves
/// its thread blocked in a file-system call until the call returns.
static ABANDONED_READERS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Reads the folders that moved into the index, one at a time, on a thread
/// of its own at the rescan threads' priority (lowest CPU priority, low disk
/// priority). It sleeps in `recv` while nothing is queued: no idle cost.
struct Walker {
    jobs: Option<Sender<WalkJob>>,
    cancel: Arc<AtomicBool>,
}

impl Walker {
    fn start(generation: u64, devices: Vec<String>, results: Sender<Message>) -> Walker {
        let (jobs, queue) = channel::<WalkJob>();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let spawned = std::thread::Builder::new()
            .name("index-folder-read".into())
            .spawn(move || read_folders(generation, devices, queue, flag, results));
        match spawned {
            Ok(_) => Walker {
                jobs: Some(jobs),
                cancel,
            },
            Err(e) => {
                eventlog::error(format!(
                    "cannot read moved-in folders in the background: {e}"
                ));
                Walker { jobs: None, cancel }
            }
        }
    }

    /// Queue a folder; false when the reader is gone.
    fn send(&self, job: WalkJob) -> bool {
        self.jobs.as_ref().is_some_and(|tx| tx.send(job).is_ok())
    }
}

impl Drop for Walker {
    /// The live session ended (a rescan starts): stop the running read; the
    /// thread ends when its queue closes.
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

fn read_folders(
    generation: u64,
    devices: Vec<String>,
    queue: Receiver<WalkJob>,
    cancel: Arc<AtomicBool>,
    results: Sender<Message>,
) {
    ferrow_walk::priority::enter_background();
    let handles: Vec<Option<std::fs::File>> =
        devices.iter().map(|d| ntfs::open_volume(d).ok()).collect();
    if results
        .send(Message::Walk(WalkEvent::Ready { generation }))
        .is_err()
    {
        return;
    }
    while let Ok(job) = queue.recv() {
        let started = Instant::now();
        let begun = WalkEvent::Started {
            generation,
            id: job.id,
        };
        if results.send(Message::Walk(begun)).is_err() {
            return;
        }
        let result = match handles.get(job.volume) {
            Some(Some(volume)) => walk_subtree(
                volume,
                (u64::from(job.sequence) << 48) | job.record,
                &job.excluded,
                job.limits,
                &cancel,
            ),
            _ => Err(WalkError::Io("cannot open the volume".into())),
        };
        let done = WalkDone {
            generation,
            id: job.id,
            result,
            took: started.elapsed(),
        };
        if results.send(Message::Walk(WalkEvent::Done(done))).is_err() {
            return;
        }
    }
}

/// Threads for background rescans: a few, never the whole machine. The
/// global pool stays free for searches.
fn background_pool() -> Option<rayon::ThreadPool> {
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    rayon::ThreadPoolBuilder::new()
        .num_threads((cores / 4).clamp(2, 8))
        .thread_name(|i| format!("index-rescan-{i}"))
        .start_handler(|_| ferrow_walk::priority::enter_background())
        .build()
        .ok()
}

/// Saves a new frozen index and maps it back from the file (ADR 0007); see
/// `persistence::save_and_map`. `None` from it: no index file.
pub type Persist =
    Arc<dyn Fn(&ferrow_index::Index) -> Option<std::io::Result<ferrow_index::Index>> + Send + Sync>;

/// The same index read from its saved file instead of the heap, when
/// `persist` can save and map it; `None` otherwise (a failure is logged and
/// the caller keeps its heap index: correct, only larger).
fn map_saved(
    index: &ferrow_index::Index,
    persist: Option<&Persist>,
) -> Option<ferrow_index::Index> {
    // Background work: lowest CPU and disk priority, like the scan, so a
    // save of gigabytes does not compete with searches.
    ferrow_walk::priority::enter_background();
    let saved = persist.and_then(|persist| persist(index));
    ferrow_walk::priority::leave_background();
    match saved? {
        Ok(mapped) => Some(mapped),
        Err(error) => {
            eventlog::warn(format!(
                "could not save and map the new index ({error}); it stays in memory"
            ));
            None
        }
    }
}

fn maintain(store: SnapshotStore, options: WalkOptions, persist: Option<Persist>) {
    let (tx, rx) = channel::<Message>();
    *CONTROL.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx.clone());
    let mut readers: HashMap<String, ()> = HashMap::new();
    let mut reason = "startup".to_string();
    let mut generation = 0u64;
    let pool = background_pool();
    // With a saved index loaded, even the startup scan is background work:
    // that index answers searches meanwhile.
    let mut background = !store.get().index.is_empty();
    loop {
        STATE.scan_started();
        STATE.count_scan(&reason);
        let started = Instant::now();
        let scan = || ferrow_walk::scan_with_maps(&options);
        let (index, report, maps) = match (&pool, background) {
            (Some(pool), true) => {
                // This thread runs the MFT reads; live batches after the
                // scan run at normal priority again.
                ferrow_walk::priority::enter_background();
                let out = pool.install(scan);
                ferrow_walk::priority::leave_background();
                out
            }
            _ => scan(),
        };
        background = true;
        let took = started.elapsed();
        if let Some(fatal) = &report.fatal_error {
            eventlog::error(format!("scan failed: {fatal}"));
            STATE.scan_failed();
            return;
        }
        // Saved and mapped before it is published: the heap copy goes now,
        // and the file is the one the next start loads.
        let base = Arc::new(map_saved(&index, persist.as_ref()).unwrap_or(index));
        let live_volumes: Vec<&VolumeMap> = maps.iter().filter(|m| m.journal.is_some()).collect();
        let summary = format!(
            "{} ({} after {reason}, {:.1} s)",
            report.summary(),
            if live_volumes.is_empty() {
                "no live updates"
            } else {
                "live"
            },
            took.as_secs_f64()
        );
        eventlog::info(&summary);
        let volumes = describe_volumes(&options, &base, &maps, &report.fast_read_errors);
        // A volume that is live but could not re-check its change journal
        // after the MFT read: visible in the event log as well as in health.
        for v in volumes.iter().filter(|v| v.journal == JOURNAL_OK) {
            if !v.journal_detail.is_empty() {
                eventlog::warn(format!("{}: {}", v.root, v.journal_detail));
            }
        }
        STATE.scan_finished(&reason, took, volumes);
        store.swap(Snapshot {
            index: base.clone(),
            live: None,
            mode: if live_volumes.is_empty() {
                "scan"
            } else {
                "live"
            }
            .into(),
            report: summary.clone(),
        });
        if live_volumes.is_empty() {
            log_memory("after scan", None, &base, &store);
            return;
        }
        for map in &live_volumes {
            if readers.contains_key(&map.device) {
                continue;
            }
            let Some(journal) = map.journal else { continue };
            let device = map.device.clone();
            let tx = tx.clone();
            let spawned = std::thread::Builder::new()
                .name("usn-reader".into())
                .spawn(move || read_journal(device, journal, tx));
            match spawned {
                Ok(_) => {
                    readers.insert(map.device.clone(), ());
                }
                Err(e) => eventlog::error(format!("journal reader for {}: {e}", map.device)),
            }
        }
        let volumes = Volumes {
            handles: maps
                .iter()
                .filter_map(|m| {
                    ntfs::open_volume(&m.device)
                        .ok()
                        .map(|f| (f, m.record_size))
                })
                .collect(),
        };
        if volumes.handles.len() != maps.len() {
            eventlog::error("cannot reopen every volume; live updates stopped");
            return;
        }
        let device_index: HashMap<String, usize> = maps
            .iter()
            .enumerate()
            .map(|(i, m)| (m.device.clone(), i))
            .collect();
        let mut live = Live::new(base.clone(), &maps);
        let devices: Vec<String> = maps.iter().map(|m| m.device.clone()).collect();
        // The record maps (16 bytes per entry) are only needed to build
        // the overlay; do not keep them for the whole live session.
        drop(maps);
        generation += 1;
        let mut walker = Walker::start(generation, devices.clone(), tx.clone());
        eventlog::info(format!(
            "folders moved into the index are read in the background ({}), not rescanned",
            walk_limits_text(base.len())
        ));
        log_memory("after scan", Some(&live), &base, &store);
        let mut applied_total = 0usize;
        let mut rescan = Maintenance::after_scan(Instant::now(), took);
        let mut hard_delta = live.limits().hard_delta;
        let mut base = base;
        let mut gate = PublishGate::new();
        reason = loop {
            // A batch keeps one reason; a disk reason in the same batch as a
            // large move must not leave its hidden entries without the
            // compaction that shows them (a request is kept only once).
            if live.adoptions_pending() {
                rescan.request(Reason::compact(REASON_LARGE_MOVE));
            }
            let load = Load {
                prompt: live.adoptions_pending(),
                ..Load::of(
                    live.delta_len(),
                    hard_delta,
                    live.walks_outstanding(),
                    STATE.is_saving(),
                )
            };
            match rescan.due(Instant::now(), load) {
                Some(Due::Rescan(why)) => {
                    // The fullest the live layer gets: what it holds before
                    // the rescan replaces it.
                    log_memory("before rescan", Some(&live), &base, &store);
                    break why;
                }
                Some(Due::Compact(why)) => {
                    log_memory("before compaction", Some(&live), &base, &store);
                    let report = format!("{summary}; {applied_total} live changes");
                    // Single writer: this thread, on the low-priority pool.
                    // Journal changes queue in the channel meanwhile, and
                    // searches read the snapshot published before.
                    let took = compact_and_publish(
                        &mut live,
                        &store,
                        report,
                        |live| {
                            let mut new = match &pool {
                                Some(pool) => pool.install(|| live.compact()),
                                None => live.compact(),
                            }?;
                            // The merged index replaces the file and is read
                            // from it, so its heap copy goes at once.
                            if let Some(mapped) = map_saved(new.base(), persist.as_ref()) {
                                let _ = new.replace_base(Arc::new(mapped));
                            }
                            Some(new)
                        },
                        // The sort orders the old index had ready are built
                        // again by one background thread each, at the lowest
                        // priority, so a sorted search is not left on name
                        // order. Nothing waits for them.
                        |index, key| {
                            index.request_sort_starting(
                                key,
                                ferrow_walk::priority::enter_background,
                            );
                        },
                    );
                    if let Some(took) = took {
                        base = live.base().clone();
                        hard_delta = live.limits().hard_delta;
                        gate = PublishGate::new();
                        rescan.compacted(Instant::now(), took);
                        STATE.compaction_done(&why, took);
                        eventlog::info(format!(
                            "merged the live changes into the index in memory ({why}) in {:.1} s: {} entries, no disk read",
                            took.as_secs_f64(),
                            base.len()
                        ));
                        log_memory("after compaction", Some(&live), &base, &store);
                    }
                    continue;
                }
                None => {}
            }
            // Wake for a pending rescan or compaction, for a view that is
            // owed but held back by the duty gate, and to check on a folder
            // read that may be stuck.
            let wake = [
                rescan.wake_at(Instant::now(), load),
                gate.wake_at(),
                live.next_walk_deadline(),
            ]
            .into_iter()
            .flatten()
            .min();
            let Some(batch) = next_batch(&rx, &device_index, wake) else {
                return;
            };
            let (changes, events) = match batch {
                Batch::Changes { changes, events } => (changes, events),
                Batch::Reset {
                    device,
                    reason,
                    fatal,
                } => {
                    eventlog::warn(format!("journal of {device}: {reason}"));
                    STATE.journal_problem(&device, fatal, &reason);
                    if rescan.request(Reason::disk(format!("{device}: {reason}"))) {
                        STATE.set_pending(rescan.rescan_pending());
                    }
                    continue;
                }
                Batch::RescanNow => {
                    rescan.force(Instant::now(), REASON_ADMIN.into());
                    STATE.set_pending(rescan.rescan_pending());
                    continue;
                }
            };
            // Finished folder reads first: the changes below are then applied
            // to a state that already holds them.
            let mut outcome = Applied::default();
            for done in route_events(&mut live, generation, events, Instant::now()) {
                match &done.result {
                    Ok(listing) => {
                        let skipped = listing.skipped.len();
                        STATE.folder_read(listing.entries.len(), skipped, done.took);
                        if done.took >= NOTABLE_WALK || skipped > 0 {
                            eventlog::info(format!(
                                "read {} entries of a folder moved into the index in {:.1} s{}",
                                listing.entries.len(),
                                done.took.as_secs_f64(),
                                if skipped > 0 {
                                    format!(" ({skipped} subfolders vanished while it was read)")
                                } else {
                                    String::new()
                                }
                            ));
                        }
                    }
                    Err(WalkError::Gone | WalkError::Cancelled) => {}
                    Err(error) => {
                        STATE.folder_read_fell_back();
                        eventlog::warn(format!(
                            "could not read a folder moved into the index ({error}, {:.1} s); every volume will be rescanned",
                            done.took.as_secs_f64()
                        ));
                    }
                }
                outcome.absorb(live.finish_walk(done.id, done.result, &volumes));
            }
            // A read the reader never finished (it is stuck in a file-system
            // call): give up on it and start a reader that is not.
            let expired = live.expire_walks(Instant::now());
            if expired.abandoned > 0 {
                STATE.folder_read_fell_back();
                let readers = ABANDONED_READERS.fetch_add(1, Ordering::Relaxed) + 1;
                eventlog::warn(format!(
                    "the folder reader did not answer within {} s; {} folder reads given up, every volume will be rescanned (folder readers abandoned since the service started: {readers})",
                    (MAX_WALK_TIME + WALK_SLACK).as_secs(),
                    expired.abandoned
                ));
                walker = Walker::start(generation, devices.clone(), tx.clone());
            }
            outcome.absorb(expired);
            if !changes.is_empty() {
                outcome.absorb(live.apply(&changes, &volumes));
            }
            for job in std::mem::take(&mut outcome.walks) {
                let id = job.id;
                if !walker.send(job) {
                    STATE.folder_read_fell_back();
                    let error = WalkError::Io("the folder reader stopped".into());
                    outcome.absorb(live.finish_walk(id, Err(error), &volumes));
                }
            }
            if let Some(why) = outcome.rescan {
                // The reason text changes with every batch (it carries the
                // delta size): only the start of an episode is an event.
                let wanted = match why.kind() {
                    ReasonKind::Compact => format!("merge in memory wanted: {why}"),
                    ReasonKind::Disk => format!("rescan wanted: {why}"),
                };
                if rescan.request(why) {
                    eventlog::info(wanted);
                    STATE.set_pending(rescan.rescan_pending());
                }
            }
            if outcome.changed > 0 || outcome.merged > 0 {
                STATE.add_live_changes(outcome.changed);
                applied_total += outcome.changed;
                gate.changed();
            }
            // The view is built after the batch but not after every batch:
            // the changes are all in `live`, and a storm gets one view per
            // gap, not one per 200 ms. Searches read the snapshot published
            // before and never wait for the build.
            if !gate.due(Instant::now()) {
                continue;
            }
            let mut report = format!("{summary}; {applied_total} live changes");
            if let Some((why, _)) = rescan.rescan_pending() {
                report.push_str(&format!("; rescan pending ({why})"));
            }
            let built = Instant::now();
            // On the low-priority pool the rescans use, so the build never
            // takes the cores a search needs.
            let view = match &pool {
                // Low-priority threads: on a saturated PC the build (and so
                // this thread) can wait; journal batches queue meanwhile and
                // the gate's gap grows with the measured build time.
                Some(pool) => pool.install(|| live.view()),
                None => live.view(),
            };
            let build = built.elapsed();
            store.swap(Snapshot {
                index: base.clone(),
                live: Some(Arc::new(view)),
                mode: "live".into(),
                report,
            });
            gate.published(Instant::now(), build);
        };
    }
}

/// Wait for the next batch (blocking while idle, or until `wake_at`, then
/// an empty batch). `None` when every reader is gone; `Err` when a volume
/// must be rescanned.
fn next_batch(
    rx: &Receiver<Message>,
    device_index: &HashMap<String, usize>,
    wake_at: Option<Instant>,
) -> Option<Batch> {
    let first = match wake_at {
        None => rx.recv().ok()?,
        Some(at) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
            Ok(message) => message,
            Err(RecvTimeoutError::Timeout) => {
                return Some(Batch::Changes {
                    changes: Vec::new(),
                    events: Vec::new(),
                });
            }
            Err(RecvTimeoutError::Disconnected) => return None,
        },
    };
    let deadline = Instant::now() + BATCH;
    let mut changes: HashMap<(usize, u64), bool> = HashMap::new();
    let mut events: Vec<WalkEvent> = Vec::new();
    let mut take = |message: Message| -> Result<(), Batch> {
        match message {
            // Kept with the batch: returning early would drop the changes
            // already collected.
            Message::Walk(event) => {
                events.push(event);
                Ok(())
            }
            Message::Reset {
                device,
                reason,
                fatal,
            } => Err(Batch::Reset {
                device,
                reason,
                fatal,
            }),
            Message::RescanNow => Err(Batch::RescanNow),
            Message::Records { device, records } => {
                if let Some(&volume) = device_index.get(&device) {
                    for (record, reason) in records {
                        *changes.entry((volume, record)).or_default() |=
                            reason & REASON_CREATE != 0;
                    }
                }
                Ok(())
            }
        }
    };
    if let Err(batch) = take(first) {
        return Some(batch);
    }
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(message) => {
                if let Err(batch) = take(message) {
                    return Some(batch);
                }
            }
            Err(RecvTimeoutError::Timeout) => break,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let mut out: Vec<Change> = changes
        .into_iter()
        .map(|((volume, record), created)| Change {
            volume,
            record,
            created,
        })
        .collect();
    out.sort_by_key(|c| (c.volume, c.record));
    Some(Batch::Changes {
        changes: out,
        events,
    })
}

fn read_journal(device: String, journal: (u64, i64), tx: Sender<Message>) {
    let volume = match ntfs::open_volume(&device) {
        Ok(v) => v,
        Err(e) => {
            let _ = tx.send(Message::Reset {
                device,
                reason: format!("cannot open the volume: {e}"),
                fatal: true,
            });
            return;
        }
    };
    let (mut id, mut next) = journal;
    let mut failures = 0;
    loop {
        match ntfs::wait_journal(&volume, id, next) {
            Ok((after, records)) => {
                failures = 0;
                next = after;
                if records.is_empty() {
                    continue;
                }
                let records = records.iter().map(|r| (r.record, r.reason)).collect();
                if tx
                    .send(Message::Records {
                        device: device.clone(),
                        records,
                    })
                    .is_err()
                {
                    return;
                }
            }
            Err(error) => {
                failures += 1;
                if failures > 3 {
                    let _ = tx.send(Message::Reset {
                        device,
                        reason: format!("change journal keeps failing: {error}"),
                        fatal: true,
                    });
                    return;
                }
                // Deleted, recreated or overrun: continue from the journal's
                // current end and let the maintainer rescan.
                let Some((new_id, _, end)) = ntfs::query_journal(&volume) else {
                    let _ = tx.send(Message::Reset {
                        device,
                        reason: format!("change journal unavailable: {error}"),
                        fatal: true,
                    });
                    return;
                };
                id = new_id;
                next = end;
                if tx
                    .send(Message::Reset {
                        device: device.clone(),
                        reason: format!("change journal reset: {error}"),
                        fatal: false,
                    })
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_walked_volume_shows_the_error_the_fast_read_gave() {
        let os87 = "read change journal: Falscher Parameter. (os error 87)";
        assert_eq!(
            walked_reason(true, Some(os87)),
            format!("reading the MFT failed ({os87}); walked instead")
        );
        // The guess is only for no error text at all.
        for none in [None, Some(""), Some("  ")] {
            assert_eq!(
                walked_reason(true, none),
                "the volume is not NTFS or its MFT could not be read; walked instead"
            );
        }
        // Without raw volume access the read never ran, whatever else is known.
        for error in [None, Some(os87)] {
            assert_eq!(
                walked_reason(false, error),
                "reading the MFT needs raw volume access (administrator or SYSTEM)"
            );
        }
    }
    use ferrow_index::{ColumnSet, IndexBuilder, flags};
    use ferrow_walk::ntfs::RecordInfo;

    const DIR_INFO: RecordInfo = RecordInfo {
        in_use: true,
        is_dir: true,
        sequence: 1,
        attributes: 0x10,
        created: 0,
        modified: 0,
        accessed: 0,
        size: 0,
        reparse_tag: 0,
    };

    /// Folder 60 exists below the root (record 5).
    struct Fs;
    impl RecordSource for Fs {
        fn fetch(&self, _volume: usize, record: u64) -> Option<RecordState> {
            (record == 60).then(|| RecordState {
                info: DIR_INFO,
                names: vec![(5, b"Moved".to_vec())],
            })
        }
    }

    /// A live index with folder 60 moved in: its id is the first walk's.
    fn live_with_a_queued_walk() -> (Live, u64) {
        let b = IndexBuilder::with_columns(ColumnSet::DEFAULT);
        let root = b.add_root(b"C:", flags::DIR);
        let map = VolumeMap {
            entries: vec![(5, root)],
            roots: vec![(5, root)],
            ..VolumeMap::default()
        };
        let mut live = Live::new(Arc::new(b.freeze()), &[map]);
        let change = Change {
            volume: 0,
            record: 60,
            created: false,
        };
        let applied = live.apply(&[change], &Fs);
        (live, applied.walks[0].id)
    }

    fn done(generation: u64, id: u64) -> WalkEvent {
        WalkEvent::Done(WalkDone {
            generation,
            id,
            result: Ok(Subtree::default()),
            took: Duration::ZERO,
        })
    }

    #[test]
    fn only_the_current_sessions_reader_events_reach_the_live_state() {
        let (mut live, id) = live_with_a_queued_walk();
        let now = Instant::now() + Duration::from_secs(100);
        let patience = crate::live::MAX_WALK_TIME + crate::live::WALK_SLACK;
        // A reader of an older session (before the last rescan) is ignored:
        // its ready, started and done events change nothing.
        let old = vec![
            WalkEvent::Ready { generation: 1 },
            WalkEvent::Started { generation: 1, id },
            done(1, id),
        ];
        assert!(route_events(&mut live, 2, old, now).is_empty());
        assert!(
            live.next_walk_deadline().is_some_and(|d| d < now),
            "the queued folder still waits on its own clock"
        );
        // The current reader's ready shows life; its start sets the walk's
        // clock; its answer is handed on.
        let ready = vec![WalkEvent::Ready { generation: 2 }];
        assert!(route_events(&mut live, 2, ready, now).is_empty());
        assert_eq!(live.next_walk_deadline(), Some(now + patience));
        let later = now + Duration::from_secs(1);
        let started = vec![WalkEvent::Started { generation: 2, id }];
        assert!(route_events(&mut live, 2, started, later).is_empty());
        assert_eq!(live.next_walk_deadline(), Some(later + patience));
        let answers = route_events(&mut live, 2, vec![done(1, id), done(2, id)], later);
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].generation, 2);
    }
}
