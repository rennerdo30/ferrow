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
use crate::saved;
use crate::{Snapshot, SnapshotStore, eventlog};

/// How long changes are collected after the first one of a batch.
const BATCH: Duration = Duration::from_millis(200);
/// `USN_REASON_FILE_CREATE`.
const REASON_CREATE: u32 = 0x100;

enum Message {
    Records {
        device: String,
        records: Vec<(u64, u32)>,
        /// The journal id and the position the reader continues from: once
        /// these records are applied, the index holds every change before it.
        position: (u64, i64),
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
        /// Per volume, the journal position the batch's changes reach.
        positions: Vec<(usize, (u64, i64))>,
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

pub fn start(
    store: SnapshotStore,
    options: WalkOptions,
    persist: Option<Persist>,
    tables: Option<std::path::PathBuf>,
) {
    // Without raw volume access (not elevated) there is nothing live to
    // keep; the initial walk stands.
    if !will_run(&options) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("index-live".into())
        .spawn(move || maintain(store, options, persist, tables));
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

/// The index file: where a build writes and a heap index is saved and
/// mapped back (ADR 0007; see `persistence::IndexFile`).
pub type Persist = crate::persistence::IndexFile;

/// The index file a build may write into: `file`, once its directory is
/// ready (a failure is logged and the build is made in memory: correct, only
/// larger).
fn ready(file: Option<&Persist>) -> Option<(&Persist, crate::persistence::Building)> {
    let file = file?;
    match file.prepare() {
        Ok(building) => Some((file, building)),
        Err(error) => {
            eventlog::warn(format!(
                "the index directory is not ready ({error}); the new index is built in memory"
            ));
            None
        }
    }
}

/// A scan, built straight into the index file when there is one (ADR 0007,
/// step 8): its private memory is then a fixed working set, not a builder
/// and a second copy of every column. The index it returns reads the file.
fn scan_into(
    options: &WalkOptions,
    file: Option<&Persist>,
) -> (ferrow_index::Index, ferrow_walk::ScanReport, Vec<VolumeMap>) {
    match ready(file) {
        Some((file, _building)) => {
            let out = ferrow_walk::scan_with_maps_to_file(options, &file.target());
            file.built(&out.1.index_file);
            out
        }
        None => ferrow_walk::scan_with_maps(options),
    }
}

/// The same index read from its saved file instead of the heap, when
/// `persist` can save and map it; `None` otherwise (a failure is logged and
/// the caller keeps its heap index: correct, only larger).
/// Run `work` with this thread in background mode (lower CPU, disk and
/// memory priority); a thread already in it stays so.
fn background_io<T>(work: impl FnOnce() -> T) -> T {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
        THREAD_MODE_BACKGROUND_END,
    };
    // SAFETY: the pseudo handle of the current thread; the call fails
    // harmlessly when the thread is in background mode already.
    let began = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) } != 0;
    let out = work();
    if began {
        // SAFETY: as above; only ends the mode this call began.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_END) };
    }
    out
}

fn map_saved(
    index: &ferrow_index::Index,
    persist: Option<&Persist>,
) -> Option<ferrow_index::Index> {
    // Built into the file already: nothing to save.
    if index.is_mapped() {
        return None;
    }
    // Background work: the thread's background mode lowers its CPU, disk
    // and memory priority, so writing gigabytes does not compete with
    // searches (which may read the mapped file meanwhile).
    let saved = background_io(|| persist.and_then(|file| file.save_and_map(index)));
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

/// What the saved live tables must have been made with (see
/// [`crate::saved::fingerprint`]).
fn options_fingerprint(options: &WalkOptions) -> u64 {
    let roots: Vec<String> = options
        .roots
        .iter()
        .map(|r| root_key(&r.display().to_string()))
        .collect();
    let excludes: Vec<String> = options
        .excludes
        .iter()
        .map(|r| root_key(&r.display().to_string()))
        .collect();
    let mut parts: Vec<Vec<u8>> = vec![
        b"roots".to_vec(),
        roots.join("\n").into_bytes(),
        b"excludes".to_vec(),
        excludes.join("\n").into_bytes(),
        options.columns.bits().to_le_bytes().to_vec(),
        vec![u8::from(options.follow_symlinks)],
        format!("{:?}", options.ntfs).into_bytes(),
    ];
    parts.push(env!("CARGO_PKG_VERSION").as_bytes().to_vec());
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    saved::fingerprint(&refs)
}

/// How long a scan whose index could not be built waits before it runs
/// again.
const BUILD_RETRY: Duration = Duration::from_secs(5 * 60);

/// The most change journal (bytes of USN records) a start replays instead of
/// scanning: about a million changed files.
const MAX_REPLAY_BYTES: i64 = 256 << 20;

/// How far (bytes of change journal) the saved state may lag behind what is
/// applied before its tables alone are saved again (nothing changed: the
/// index file stays); otherwise a session leaves the saved position behind
/// until a start must scan (MAX_REPLAY_BYTES, or the journal wrapping past
/// it). A quarter of what a start replays, and never more than a quarter of
/// the journal itself.
const TABLES_LAG_BYTES: i64 = MAX_REPLAY_BYTES / 4;
/// …and before a merge brings the saved index up to date when something
/// changed. A merge writes the whole index file (gigabytes on a large PC),
/// so it waits for three quarters of what a start replays, or half the
/// journal.
const MERGE_LAG_BYTES: i64 = MAX_REPLAY_BYTES / 4 * 3;
/// …or how long the saved state may lag at all.
const TABLES_REFRESH: Duration = Duration::from_secs(12 * 60 * 60);
/// After an attempt to bring the saved state up to date (it may have
/// failed: a full disk), the next waits at least this long, doubled after
/// each attempt that did not save the tables, up to TABLES_REFRESH.
const REFRESH_RETRY: Duration = Duration::from_secs(15 * 60);

/// How long to wait before the next attempt after `failures` attempts in a
/// row that did not save the tables (0: the last one did).
fn refresh_retry(failures: u32) -> Duration {
    (REFRESH_RETRY * 2u32.pow(failures.saturating_sub(1).min(6))).min(TABLES_REFRESH)
}

/// The smallest journal size the lag limits scale down to: a journal that
/// reports less (just created) is treated as this large.
const MIN_JOURNAL_SPAN: i64 = 4 << 20;
/// Why such a merge runs.
const REASON_REFRESH: &str = "bring the saved index up to date";

/// Whether a start may replay a volume's journal from `saved` (journal id,
/// position) given the journal as it is now (`id`, first and next USN), or
/// why it must scan.
fn replay_verdict(saved: (u64, i64), now: (u64, i64, i64)) -> Result<(), &'static str> {
    let (saved_id, at) = saved;
    let (id, first, next) = now;
    if id != saved_id {
        return Err("was replaced");
    }
    if at < first || at > next {
        return Err("no longer holds every change since the index was saved");
    }
    // Replaying more than this would take longer than the scan it saves
    // (one record read per changed file).
    if next - at > MAX_REPLAY_BYTES {
        return Err("has too many changes since the index was saved");
    }
    Ok(())
}

/// Per volume, how far the applied journal position is ahead of the saved
/// one, in journal bytes (a replaced journal, or saved tables that are not
/// current, count as far ahead).
fn saved_lags(applied: &[(u64, i64)], saved: &[(u64, i64)]) -> Vec<i64> {
    applied
        .iter()
        .zip(saved)
        .map(|(&(id, at), &(saved_id, saved_at))| {
            if id == saved_id {
                at.saturating_sub(saved_at).max(0)
            } else {
                i64::MAX
            }
        })
        .collect()
}

/// Whether the saved state should be brought up to date now, by a merge
/// (`merge`: something changed) or by saving its tables. Per volume, `lags`
/// and `spans` (the journal's configured maximum size, 0 when unknown): due
/// when a volume lags by its limit (see TABLES_LAG_BYTES and
/// MERGE_LAG_BYTES; at most a quarter or half of a small journal, which
/// would otherwise wrap past the saved position first), or by anything
/// for TABLES_REFRESH.
fn refresh_due(lags: &[i64], spans: &[i64], since_saved: Duration, merge: bool) -> bool {
    let (limit, share) = if merge {
        (MERGE_LAG_BYTES, 2)
    } else {
        (TABLES_LAG_BYTES, 4)
    };
    lags.iter().zip(spans).any(|(&lag, &span)| {
        let limit = if span > 0 {
            limit.min(span / share)
        } else {
            limit
        };
        lag > 0 && (lag >= limit || since_saved >= TABLES_REFRESH)
    })
}

/// Whether `device` names a volume (`\\?\Volume{…}`, one optional trailing
/// separator) and nothing else.
fn is_volume_device(device: &str) -> bool {
    device
        .strip_prefix(r"\\?\Volume{")
        .map(|rest| rest.strip_suffix('\\').unwrap_or(rest))
        .and_then(|rest| rest.strip_suffix('}'))
        .is_some_and(|guid| {
            !guid.is_empty() && guid.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
        })
}

/// The saved index and its live tables, when they can replace the startup
/// scan: the loaded index is the mapped `index.bin` the tables were saved
/// for, with the same roots and options, and every volume's change journal
/// still holds every change since (same journal, saved position inside it).
/// Otherwise `None`, with the reason in the log.
fn try_restore(
    store: &SnapshotStore,
    tables: &std::path::Path,
    fingerprint: u64,
) -> Option<(Arc<ferrow_index::Index>, saved::Saved)> {
    let base = store.get().index.clone();
    let checksum = base.file_checksum()?;
    let refuse = |why: String| {
        eventlog::info(format!("startup scan: {why}"));
    };
    let saved = match saved::read(tables, checksum, base.len(), fingerprint) {
        Ok(saved) => saved,
        Err(why) => {
            refuse(why.to_string());
            return None;
        }
    };
    for v in &saved.volumes {
        // Only a volume device is opened (the name comes from a file).
        if !is_volume_device(&v.device) {
            refuse("the saved live tables name an unexpected device".into());
            return None;
        }
        let Ok(file) = ntfs::open_volume(&v.device) else {
            refuse(format!("{} cannot be opened", v.device));
            return None;
        };
        if ntfs::volume_serial(&file) != Some(v.serial) {
            refuse(format!("{} is another volume now", v.device));
            return None;
        }
        let Some((id, first, next)) = ntfs::query_journal(&file) else {
            refuse(format!("{} has no change journal now", v.device));
            return None;
        };
        if let Err(why) = replay_verdict(v.journal, (id, first, next)) {
            refuse(format!("the change journal of {} {why}", v.device));
            return None;
        }
    }
    Some((base, saved))
}

/// Health's volume rows for an index restored from its saved tables: each
/// configured root, live on the volume that holds it.
fn restored_volumes(
    options: &WalkOptions,
    base: &ferrow_index::Index,
    saved: &saved::Saved,
) -> Vec<VolumeState> {
    let mut buf = Vec::new();
    let found: Vec<(Vec<String>, &saved::SavedVolume)> = saved
        .volumes
        .iter()
        .map(|v| {
            let roots = v
                .roots
                .iter()
                .filter_map(|&record| {
                    let id = *v.record_base.get(record as usize)?;
                    ((id as usize) < base.len()).then(|| {
                        base.path_bytes(id, &mut buf);
                        root_key(&String::from_utf8_lossy(&buf))
                    })
                })
                .collect();
            (roots, v)
        })
        .collect();
    options
        .roots
        .iter()
        .map(|root| {
            let text = root.display().to_string();
            let key = root_key(&text);
            match found.iter().find(|(roots, _)| roots.contains(&key)) {
                Some((_, v)) => VolumeState {
                    root: text,
                    device: Some(v.device.clone()),
                    mode: MODE_LIVE,
                    reason: String::new(),
                    entries: Some(v.entries),
                    journal: JOURNAL_OK,
                    journal_detail: String::new(),
                },
                None => VolumeState::walked(text, ""),
            }
        })
        .collect()
}

/// Save `live`'s frozen tables beside its index and read them back mapped:
/// `None` (with a warning) when its index is not the mapped file or the
/// write fails, and then the next start scans.
fn save_tables(
    path: &std::path::Path,
    live: &Live,
    fingerprint: u64,
    facts: &[saved::VolumeFacts],
) -> Option<saved::Saved> {
    let checksum = live.base().file_checksum()?;
    let started = Instant::now();
    // Background mode, like the index save.
    let written = background_io(|| {
        saved::write(path, &live.saved_ref(checksum, fingerprint, facts))
            .map_err(|e| e.to_string())
            .and_then(|()| {
                saved::read(path, checksum, live.base().len(), fingerprint)
                    .map_err(|e| e.to_string())
            })
    });
    match written {
        Ok(saved) => {
            eventlog::info(format!(
                "live tables saved in {:.1} s (the next start replays the change journal instead of scanning)",
                started.elapsed().as_secs_f64()
            ));
            Some(saved)
        }
        Err(error) => {
            eventlog::warn(format!(
                "could not save the live tables ({error}); the next start scans"
            ));
            None
        }
    }
}

fn maintain(
    store: SnapshotStore,
    options: WalkOptions,
    persist: Option<Persist>,
    tables: Option<std::path::PathBuf>,
) {
    let (tx, rx) = channel::<Message>();
    *CONTROL.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx.clone());
    let mut readers: HashMap<String, ()> = HashMap::new();
    let mut reason = "startup".to_string();
    let mut generation = 0u64;
    let pool = background_pool();
    // With a saved index loaded, even the startup scan is background work:
    // that index answers searches meanwhile.
    let mut background = !store.get().index.is_empty();
    let fingerprint = options_fingerprint(&options);
    // Only the first session may come from the saved files; a rescan reads
    // the drives.
    let mut first = true;
    loop {
        let started = Instant::now();
        let restored = if std::mem::take(&mut first) {
            tables
                .as_deref()
                .and_then(|path| try_restore(&store, path, fingerprint))
        } else {
            None
        };
        if let Some((base, saved)) = restored {
            let facts: Vec<saved::VolumeFacts> = saved
                .volumes
                .iter()
                .map(|v| saved::VolumeFacts {
                    device: v.device.clone(),
                    serial: v.serial,
                    journal: Some(v.journal),
                    record_size: v.record_size,
                })
                .collect();
            let volumes = restored_volumes(&options, &base, &saved);
            // Each root must still be on the volume its tables are for
            // (a drive letter can move to another volume).
            if let Some(moved) = volumes.iter().find(|v| {
                v.device.as_deref().is_none_or(|device| {
                    ntfs::volume_device(std::path::Path::new(&v.root))
                        .ok()
                        .as_deref()
                        != Some(device)
                })
            }) {
                eventlog::info(format!(
                    "startup scan: {} is not on the volume its saved tables are for",
                    moved.root
                ));
                drop((base, saved));
                continue;
            }
            let live = Live::restore(base.clone(), saved);
            let took = started.elapsed();
            let summary = format!(
                "loaded the saved index ({} entries) and replays the change journal since it was saved; no scan ({:.1} s)",
                base.len(),
                took.as_secs_f64()
            );
            eventlog::info(&summary);
            STATE.scan_finished("restart: no scan", took, volumes);
            reason = match run_session(
                &store,
                &options,
                &tx,
                &rx,
                &mut readers,
                &mut generation,
                &pool,
                persist.as_ref(),
                tables.as_deref(),
                fingerprint,
                Session {
                    base,
                    live,
                    facts,
                    can_save_tables: true,
                    tables_current: true,
                    summary,
                    took,
                },
            ) {
                Some(why) => why,
                None => return,
            };
            continue;
        }
        STATE.scan_started();
        STATE.count_scan(&reason);
        let scan = || scan_into(&options, persist.as_ref());
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
            // A build that could not read its spilled shards back (a failing
            // disk, a file taken away) may work later: try again then
            // instead of ending live updates. Other fatal errors (an exclude
            // that cannot be resolved) stop them, as before.
            if fatal.starts_with(ferrow_walk::BUILD_FAILED) {
                eventlog::warn(format!(
                    "scan failed: {fatal}; trying again in {} min",
                    BUILD_RETRY.as_secs() / 60
                ));
                STATE.scan_failed();
                // Wait, dropping the journals' records (the next scan reads
                // everything again); an administrator's rescan ends the wait.
                let until = Instant::now() + BUILD_RETRY;
                while let Some(left) = until.checked_duration_since(Instant::now()) {
                    match rx.recv_timeout(left) {
                        Ok(Message::RescanNow) => {
                            reason = REASON_ADMIN.into();
                            break;
                        }
                        // A reader that stopped for good is reported now:
                        // the next session does not start it again.
                        Ok(Message::Reset {
                            device,
                            reason: why,
                            fatal: true,
                        }) => {
                            eventlog::warn(format!("journal of {device}: {why}"));
                            STATE.journal_problem(&device, true, &why);
                        }
                        Ok(_) => {}
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
                continue;
            }
            eventlog::error(format!("scan failed: {fatal}"));
            STATE.scan_failed();
            STATE.set_tables_mapped(0);
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
        // Every root read through its MFT, with a journal that could be
        // re-read right after: what saved tables need to stand for a scan.
        let every_root_live = volumes
            .iter()
            .all(|v| v.mode == MODE_LIVE && v.journal_detail.is_empty());
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
            STATE.set_tables_mapped(0);
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
        // The saved tables can replace the next startup scan only when every
        // root is a volume read through its MFT, with a change journal.
        let can_save_tables = maps.len() == live_volumes.len() && every_root_live;
        let facts: Vec<saved::VolumeFacts> = maps
            .iter()
            .map(|m| saved::VolumeFacts {
                device: m.device.clone(),
                serial: m.serial,
                journal: m.journal,
                record_size: m.record_size as u32,
            })
            .collect();
        let mut live = Live::new(base.clone(), &maps);
        // The record maps (16 bytes per entry) are only needed to build
        // the overlay; do not keep them for the whole live session.
        drop(maps);
        let mut tables_current = false;
        if can_save_tables
            && let Some(path) = tables.as_deref()
            && let Some(saved) = save_tables(path, &live, fingerprint, &facts)
        {
            live.replace_tables(saved);
            tables_current = true;
        }
        reason = match run_session(
            &store,
            &options,
            &tx,
            &rx,
            &mut readers,
            &mut generation,
            &pool,
            persist.as_ref(),
            tables.as_deref(),
            fingerprint,
            Session {
                base,
                live,
                facts,
                can_save_tables,
                tables_current,
                summary,
                took,
            },
        ) {
            Some(why) => why,
            None => return,
        };
    }
}

/// A frozen index and the live layer over it, from a scan or from the saved
/// files, ready to follow the change journals.
struct Session {
    base: Arc<ferrow_index::Index>,
    live: Live,
    /// Per volume, in the live layer's volume order.
    facts: Vec<saved::VolumeFacts>,
    /// Whether the live tables may be saved for the next start.
    can_save_tables: bool,
    /// Whether `live.bin` holds this session's tables now.
    tables_current: bool,
    summary: String,
    took: Duration,
}

/// Follow the change journals over `session` until a rescan is due: its
/// reason, or `None` when live updates stopped.
#[allow(clippy::too_many_arguments)]
fn run_session(
    store: &SnapshotStore,
    options: &WalkOptions,
    tx: &Sender<Message>,
    rx: &Receiver<Message>,
    readers: &mut HashMap<String, ()>,
    generation: &mut u64,
    pool: &Option<rayon::ThreadPool>,
    persist: Option<&Persist>,
    tables: Option<&std::path::Path>,
    fingerprint: u64,
    session: Session,
) -> Option<String> {
    let _ = options;
    let Session {
        base,
        mut live,
        facts,
        can_save_tables,
        tables_current,
        summary,
        took,
    } = session;
    store.swap(Snapshot {
        index: base.clone(),
        live: None,
        mode: "live".into(),
        report: summary.clone(),
    });
    for fact in &facts {
        if readers.contains_key(&fact.device) {
            continue;
        }
        let device = fact.device.clone();
        // A volume without a change journal is read once and not followed.
        let Some(journal) = fact.journal else {
            continue;
        };
        let tx = tx.clone();
        let spawned = std::thread::Builder::new()
            .name("usn-reader".into())
            .spawn(move || read_journal(device, journal, tx));
        match spawned {
            Ok(_) => {
                readers.insert(fact.device.clone(), ());
            }
            Err(e) => eventlog::error(format!("journal reader for {}: {e}", fact.device)),
        }
    }
    let volumes = Volumes {
        handles: facts
            .iter()
            .filter_map(|f| {
                ntfs::open_volume(&f.device)
                    .ok()
                    .map(|file| (file, f.record_size as usize))
            })
            .collect(),
    };
    if volumes.handles.len() != facts.len() {
        eventlog::error("cannot reopen every volume; live updates stopped");
        STATE.set_tables_mapped(0);
        return None;
    }
    let device_index: HashMap<String, usize> = facts
        .iter()
        .enumerate()
        .map(|(i, f)| (f.device.clone(), i))
        .collect();
    // Where each journal's changes are applied up to: what saved tables
    // may claim.
    let mut applied: Vec<(u64, i64)> = facts.iter().map(|f| f.journal.unwrap_or((0, 0))).collect();
    // What the saved tables claim (positions) and when they were saved;
    // tables that could not be saved claim nothing (an impossible journal),
    // so bringing them up to date is due at once.
    let mut saved_at = if tables_current {
        applied.clone()
    } else {
        vec![(u64::MAX, 0); applied.len()]
    };
    let mut tables_saved = Instant::now();
    let mut scratch_in_heap_seen = ferrow_index::store::scratch_in_heap();
    // Attempts in a row that did not save the tables, and when the next may
    // run: tables that could not be saved at the start wait one retry gap.
    let mut refresh_failures: u32 = u32::from(!tables_current);
    let mut next_refresh = Instant::now() + refresh_retry(refresh_failures);
    let refresh_possible = can_save_tables && tables.is_some() && persist.is_some();
    // Each journal's size, which bounds how far the saved position may lag.
    // Each journal's configured maximum size (what it keeps before dropping
    // its oldest records), not what it holds now: a journal created a moment
    // ago is small but will grow.
    let spans: Vec<i64> = volumes
        .handles
        .iter()
        .map(|(file, _)| {
            ntfs::journal_capacity(file).map_or(0, |max| {
                i64::try_from(max).unwrap_or(i64::MAX).max(MIN_JOURNAL_SPAN)
            })
        })
        .collect();
    let devices: Vec<String> = facts.iter().map(|f| f.device.clone()).collect();
    let (tx, rx) = (tx.clone(), rx);
    let pool = pool.as_ref();
    let generation = {
        *generation += 1;
        *generation
    };
    {
        let mut walker = Walker::start(generation, devices.clone(), tx.clone());
        eventlog::info(format!(
            "folders moved into the index are read in the background ({}), not rescanned",
            walk_limits_text(base.len())
        ));
        STATE.set_tables_mapped(live.mapped_bytes());
        log_memory("live updates start", Some(&live), &base, store);
        let mut applied_total = 0usize;
        let mut rescan = Maintenance::after_scan(Instant::now(), took);
        let mut hard_delta = live.limits().hard_delta;
        let mut base = base;
        let mut gate = PublishGate::new();
        let reason = loop {
            // A batch keeps one reason; a disk reason in the same batch as a
            // large move must not leave its hidden entries without the
            // compaction that shows them (a request is kept only once).
            if live.adoptions_pending() {
                rescan.request(Reason::compact(REASON_LARGE_MOVE));
            }
            // Temporary buffers (sort orders) that should have been files
            // beside the index but went to the heap: memory is then not
            // what it should be. Said once per new occurrence.
            let in_heap = ferrow_index::store::scratch_in_heap();
            if in_heap > scratch_in_heap_seen {
                scratch_in_heap_seen = in_heap;
                eventlog::warn(
                    "a sort order could not be kept in a file beside the index (disk full or read-only?); it is in memory",
                );
            }
            // Keep the saved index and its journal positions from falling
            // far behind. With changes, a merge brings both up to date;
            // without any, only the tables are saved again (the index is
            // unchanged).
            let lags = saved_lags(&applied, &saved_at);
            let lagging = lags.iter().any(|&lag| lag > 0);
            let changed = live.has_changes();
            if refresh_possible
                && !rescan.rescan_waiting()
                && Instant::now() >= next_refresh
                && refresh_due(&lags, &spans, tables_saved.elapsed(), changed)
            {
                // One attempt per retry gap, whatever its outcome (a merge
                // reports its own below).
                next_refresh = Instant::now() + refresh_retry(refresh_failures + 1);
                if changed {
                    rescan.request(Reason::compact(REASON_REFRESH));
                } else if let Some(path) = tables {
                    let facts: Vec<saved::VolumeFacts> = facts
                        .iter()
                        .zip(&applied)
                        .map(|(f, &journal)| saved::VolumeFacts {
                            journal: Some(journal),
                            ..f.clone()
                        })
                        .collect();
                    if let Some(saved) = save_tables(path, &live, fingerprint, &facts) {
                        live.replace_tables(saved);
                        saved_at = applied.clone();
                        tables_saved = Instant::now();
                        refresh_failures = 0;
                        next_refresh = Instant::now() + refresh_retry(0);
                    } else {
                        refresh_failures += 1;
                        next_refresh = Instant::now() + refresh_retry(refresh_failures);
                    }
                }
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
                    log_memory("before rescan", Some(&live), &base, store);
                    STATE.set_tables_mapped(0);
                    break why;
                }
                Some(Due::Compact(why)) => {
                    log_memory("before compaction", Some(&live), &base, store);
                    let save_now = can_save_tables && !rescan.rescan_waiting();
                    let mut tables_written = false;
                    // A merge declines only while folders are read; otherwise
                    // a missing result is a failure (the index file could
                    // not be built: a full disk), which waits its gap.
                    let merge_started = Instant::now();
                    let reading = live.walks_outstanding();
                    let report = format!("{summary}; {applied_total} live changes");
                    // Single writer: this thread, on the low-priority pool.
                    // Journal changes queue in the channel meanwhile, and
                    // searches read the snapshot published before.
                    let took = compact_and_publish(
                        &mut live,
                        store,
                        report,
                        |live| {
                            // The merged index is built in the index file and
                            // read from it: no builder, no second copy of the
                            // columns in the heap.
                            // Too little disk for the new file: keep the index as
                            // it is (a merge in memory would take gigabytes)
                            // and try again after the merge's gap.
                            if let Some(file) = persist
                                && let Err(why) = file.has_room()
                            {
                                eventlog::warn(format!("no merge in memory now: {why}"));
                                return None;
                            }
                            let build = |live: &mut Live| match ready(persist) {
                                Some((file, _building)) => {
                                    let built = live.compact_into(Some(&file.target()));
                                    // Declined (folder reads): the build ends
                                    // without a file.
                                    let report = built
                                        .as_ref()
                                        .map(|(_, report)| report.clone())
                                        .unwrap_or_default();
                                    file.built(&report);
                                    built.map(|(new, _)| new)
                                }
                                None => live.compact(),
                            };
                            let mut new = match &pool {
                                Some(pool) => pool.install(|| build(live)),
                                None => build(live),
                            }?;
                            // Built in memory after all (no index file, or it
                            // could not be built): saved and mapped now.
                            let in_file = match map_saved(new.base(), persist) {
                                _ if new.base().is_mapped() => true,
                                Some(mapped) => {
                                    let replaced = new.replace_base(Arc::new(mapped));
                                    if !replaced {
                                        eventlog::warn(
                                            "the saved index does not match the merged one; it stays in memory",
                                        );
                                    }
                                    replaced
                                }
                                None => false,
                            };
                            if in_file
                                && save_now
                                && let Some(path) = tables
                            {
                                // Its live tables too, with the journal
                                // positions every merged change is up to,
                                // unless the index is known to miss
                                // changes (a rescan waits): then the next
                                // start scans.
                                let facts: Vec<saved::VolumeFacts> = facts
                                    .iter()
                                    .zip(&applied)
                                    .map(|(f, &journal)| saved::VolumeFacts {
                                        journal: Some(journal),
                                        ..f.clone()
                                    })
                                    .collect();
                                if let Some(saved) = save_tables(path, &new, fingerprint, &facts) {
                                    new.replace_tables(saved);
                                    tables_written = true;
                                }
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
                        // The merged index replaced the file: tables saved
                        // for the old one no longer match it.
                        if tables_written {
                            saved_at = applied.clone();
                            tables_saved = Instant::now();
                            refresh_failures = 0;
                            next_refresh = Instant::now() + refresh_retry(0);
                        } else {
                            // Not saved: retried after a gap that grows
                            // with each miss (each retry may be a merge
                            // that rewrites the index file).
                            saved_at = vec![(u64::MAX, 0); applied.len()];
                            refresh_failures += 1;
                            next_refresh = Instant::now() + refresh_retry(refresh_failures);
                        }
                        STATE.set_tables_mapped(live.mapped_bytes());
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
                        log_memory("after compaction", Some(&live), &base, store);
                    } else if !reading {
                        rescan.compacted(Instant::now(), merge_started.elapsed());
                        refresh_failures += 1;
                        next_refresh = Instant::now() + refresh_retry(refresh_failures);
                        eventlog::warn(format!(
                            "the merge in memory ({why}) failed; the index stays as it is and the merge is tried again later"
                        ));
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
                // Only a deadline ahead: once due, the refresh was asked for
                // (a merge then wakes the maintainer itself).
                (refresh_possible && lagging)
                    .then(|| {
                        // A limit already met waits only for the retry gap.
                        if refresh_due(&lags, &spans, Duration::ZERO, changed) {
                            next_refresh
                        } else {
                            (tables_saved + TABLES_REFRESH).max(next_refresh)
                        }
                    })
                    .filter(|&at| at > Instant::now()),
            ]
            .into_iter()
            .flatten()
            .min();
            let Some(batch) = next_batch(rx, &device_index, wake) else {
                STATE.set_tables_mapped(0);
                return None;
            };
            let (changes, events, positions) = match batch {
                Batch::Changes {
                    changes,
                    events,
                    positions,
                } => (changes, events, positions),
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
            // Applied: the index now holds every change before these.
            for (volume, position) in positions {
                applied[volume] = position;
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
        Some(reason)
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
                    positions: Vec::new(),
                });
            }
            Err(RecvTimeoutError::Disconnected) => return None,
        },
    };
    let deadline = Instant::now() + BATCH;
    let mut changes: HashMap<(usize, u64), bool> = HashMap::new();
    let mut events: Vec<WalkEvent> = Vec::new();
    let mut positions: HashMap<usize, (u64, i64)> = HashMap::new();
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
            Message::Records {
                device,
                records,
                position,
            } => {
                if let Some(&volume) = device_index.get(&device) {
                    for (record, reason) in records {
                        *changes.entry((volume, record)).or_default() |=
                            reason & REASON_CREATE != 0;
                    }
                    // Messages of one reader arrive in order: the last wins.
                    positions.insert(volume, position);
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
        positions: positions.into_iter().collect(),
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
                        position: (id, next),
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
    fn a_start_replays_only_a_journal_that_holds_every_change_and_not_too_many() {
        let mb = 1i64 << 20;
        assert_eq!(
            replay_verdict((7, 100 * mb), (7, 10 * mb, 150 * mb)),
            Ok(())
        );
        assert!(
            replay_verdict((7, 100 * mb), (8, 10 * mb, 150 * mb)).is_err(),
            "replaced"
        );
        assert!(
            replay_verdict((7, 5 * mb), (7, 10 * mb, 150 * mb)).is_err(),
            "wrapped past"
        );
        assert!(
            replay_verdict((7, 200 * mb), (7, 10 * mb, 150 * mb)).is_err(),
            "ahead of it"
        );
        assert!(
            replay_verdict((7, 0), (7, 0, 300 * mb)).is_err(),
            "too many changes"
        );
        assert_eq!(
            replay_verdict((7, 0), (7, 0, 256 * mb)),
            Ok(()),
            "up to the limit"
        );
    }

    #[test]
    fn failed_refreshes_wait_longer_each_time_up_to_twelve_hours() {
        let min = Duration::from_secs(60);
        assert_eq!(refresh_retry(0), 15 * min);
        assert_eq!(refresh_retry(1), 15 * min);
        assert_eq!(refresh_retry(2), 30 * min);
        assert_eq!(refresh_retry(4), 120 * min);
        assert_eq!(refresh_retry(6), 480 * min);
        assert_eq!(refresh_retry(7), TABLES_REFRESH);
        assert_eq!(refresh_retry(u32::MAX), TABLES_REFRESH);
    }

    #[test]
    fn the_saved_state_is_refreshed_when_it_lags_far_or_long() {
        let mb = 1i64 << 20;
        let hour = Duration::from_secs(3600);
        assert_eq!(
            saved_lags(&[(1, 90 * mb), (2, 5)], &[(1, 10 * mb), (2, 5)]),
            [80 * mb, 0]
        );
        assert_eq!(
            saved_lags(&[(3, 1)], &[(1, 1)]),
            [i64::MAX],
            "a replaced journal"
        );
        assert_eq!(saved_lags(&[(1, 1)], &[(1, 9)]), [0]);
        let unknown = [0];
        // Tables alone: from 64 MiB; a merge: from 192 MiB.
        assert!(refresh_due(&[64 * mb], &unknown, Duration::ZERO, false));
        assert!(!refresh_due(&[63 * mb], &unknown, 11 * hour, false));
        assert!(!refresh_due(&[100 * mb], &unknown, Duration::ZERO, true));
        assert!(refresh_due(&[192 * mb], &unknown, Duration::ZERO, true));
        // Anything, after 12 hours; nothing, never.
        assert!(refresh_due(&[1], &unknown, 12 * hour, true));
        assert!(!refresh_due(&[0], &unknown, 100 * hour, false));
        // A small journal (32 MiB): a quarter or half of it.
        assert!(refresh_due(&[8 * mb], &[32 * mb], Duration::ZERO, false));
        assert!(refresh_due(&[16 * mb], &[32 * mb], Duration::ZERO, true));
        assert!(!refresh_due(&[15 * mb], &[32 * mb], Duration::ZERO, true));
        // Tables not current: due at once.
        assert!(refresh_due(&[i64::MAX], &unknown, Duration::ZERO, true));
    }

    #[test]
    fn only_a_volume_device_is_opened() {
        assert!(is_volume_device(r"\\?\Volume{0123-abcd}"));
        assert!(is_volume_device(r"\\?\Volume{0123-abcd}\"));
        assert!(!is_volume_device(r"\\?\Volume{}"));
        assert!(!is_volume_device(r"\\?\Volume{x}}"));
        assert!(!is_volume_device(r"\\?\Volume{x}\path\file}"));
        assert!(!is_volume_device(r"C:\Windows"));
        assert!(!is_volume_device(r"\\?\Volume{x}/y}"));
    }

    #[test]
    fn a_batch_reports_how_far_each_journal_was_read() {
        let (tx, rx) = channel::<Message>();
        let devices: HashMap<String, usize> = [("A".to_string(), 0), ("B".to_string(), 1)]
            .into_iter()
            .collect();
        for (device, record, at) in [("A", 5, 100), ("B", 7, 50), ("A", 6, 180), ("X", 1, 9)] {
            tx.send(Message::Records {
                device: device.into(),
                records: vec![(record, REASON_CREATE)],
                position: (1, at),
            })
            .unwrap();
        }
        let Some(Batch::Changes {
            changes,
            mut positions,
            ..
        }) = next_batch(&rx, &devices, None)
        else {
            panic!("a batch of changes");
        };
        assert_eq!(changes.len(), 3, "the unknown device is ignored");
        positions.sort();
        assert_eq!(
            positions,
            [(0, (1, 180)), (1, (1, 50))],
            "the last position per journal"
        );
        // A reset ends the batch without positions: what it collected is
        // not claimed as applied.
        tx.send(Message::Records {
            device: "A".into(),
            records: vec![(8, 0)],
            position: (1, 300),
        })
        .unwrap();
        tx.send(Message::Reset {
            device: "A".into(),
            reason: "journal reset".into(),
            fatal: false,
        })
        .unwrap();
        assert!(matches!(
            next_batch(&rx, &devices, None),
            Some(Batch::Reset { .. })
        ));
    }

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
