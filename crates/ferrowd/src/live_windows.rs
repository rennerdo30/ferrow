//! Windows side of live updates: one change-journal reader thread per
//! NTFS volume and the maintainer thread that rescans and applies batches.
//!
//! Idle cost is zero: readers sleep inside `FSCTL_READ_USN_JOURNAL` until
//! the volume changes, and the maintainer sleeps on a channel. A batch is
//! collected for [`BATCH`] after the first change, then applied and
//! published, so a change shows up in results well within a second.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ferrow_ipc::health::{JOURNAL_NONE, JOURNAL_OK, MODE_LIVE, MODE_UNAVAILABLE};
use ferrow_walk::{VolumeMap, WalkOptions, ntfs};

use crate::health::{STATE, VolumeState};
use crate::live::{Change, HARD_DELTA, Live, RecordSource, RecordState, RescanPolicy};
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
}

/// What `next_batch` found.
enum Batch {
    Changes(Vec<Change>),
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

/// How each configured root is read and, for volumes read through their MFT,
/// what the scan found (device, entries, journal).
fn describe_volumes(
    options: &WalkOptions,
    base: &ferrow_index::Index,
    maps: &[VolumeMap],
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
                let why = if ntfs::can_read_volume(root) {
                    "the volume is not NTFS or its MFT could not be read; walked instead"
                } else {
                    "reading the MFT needs raw volume access (administrator or SYSTEM)"
                };
                return VolumeState::walked(text, why);
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
                journal_detail: String::new(),
            }
        })
        .collect()
}

struct Volumes {
    handles: Vec<(std::fs::File, usize)>,
}

impl RecordSource for Volumes {
    fn fetch(&self, volume: usize, record: u64) -> Option<RecordState> {
        let (file, size) = self.handles.get(volume)?;
        ntfs::fetch_state(file, record, *size).map(|(info, names)| RecordState { info, names })
    }
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

pub fn start(store: SnapshotStore, options: WalkOptions) {
    // Without raw volume access (not elevated) there is nothing live to
    // keep; the initial walk stands.
    if !will_run(&options) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("index-live".into())
        .spawn(move || maintain(store, options));
    if let Err(error) = spawned {
        eventlog::error(format!("live updates unavailable: {error}"));
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

fn maintain(store: SnapshotStore, options: WalkOptions) {
    let (tx, rx) = channel::<Message>();
    *CONTROL.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx.clone());
    let mut readers: HashMap<String, ()> = HashMap::new();
    let mut reason = "startup".to_string();
    let pool = background_pool();
    // With a saved index loaded, even the startup scan is background work:
    // that index answers searches meanwhile.
    let mut background = !store.get().index.is_empty();
    loop {
        STATE.scan_started();
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
        let base = Arc::new(index);
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
        STATE.scan_finished(&reason, took, describe_volumes(&options, &base, &maps));
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
        // The record maps (16 bytes per entry) are only needed to build
        // the overlay; do not keep them for the whole live session.
        drop(maps);
        let mut applied_total = 0usize;
        let mut rescan = RescanPolicy::after_scan(Instant::now(), took);
        reason = loop {
            let urgent = live.delta_len() > HARD_DELTA;
            if let Some(why) = rescan.due(Instant::now(), urgent) {
                break why;
            }
            let Some(batch) = next_batch(&rx, &device_index, rescan.wake_at()) else {
                return;
            };
            let changes = match batch {
                Batch::Changes(changes) => changes,
                Batch::Reset {
                    device,
                    reason,
                    fatal,
                } => {
                    eventlog::warn(format!("journal of {device}: {reason}"));
                    STATE.journal_problem(&device, fatal, &reason);
                    if rescan.request(format!("{device}: {reason}")) {
                        STATE.set_pending(rescan.pending().map(|why| (why, rescan.earliest())));
                    }
                    continue;
                }
                Batch::RescanNow => {
                    rescan.force(Instant::now(), "requested by an administrator".into());
                    STATE.set_pending(rescan.pending().map(|why| (why, rescan.earliest())));
                    continue;
                }
            };
            if changes.is_empty() {
                continue; // woke for a pending rescan
            }
            let outcome = live.apply(&changes, &volumes);
            if let Some(why) = outcome.rescan {
                // The reason text changes with every batch (it carries the
                // delta size): only the start of an episode is an event.
                let wanted = format!("rescan wanted: {why}");
                if rescan.request(why) {
                    eventlog::info(wanted);
                    STATE.set_pending(rescan.pending().map(|why| (why, rescan.earliest())));
                }
            }
            if outcome.changed == 0 {
                continue;
            }
            STATE.add_live_changes(outcome.changed);
            applied_total += outcome.changed;
            let mut report = format!("{summary}; {applied_total} live changes");
            if let Some(why) = rescan.pending() {
                report.push_str(&format!("; rescan pending ({why})"));
            }
            store.swap(Snapshot {
                index: base.clone(),
                live: Some(Arc::new(live.view())),
                mode: "live".into(),
                report,
            });
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
            Err(RecvTimeoutError::Timeout) => return Some(Batch::Changes(Vec::new())),
            Err(RecvTimeoutError::Disconnected) => return None,
        },
    };
    let deadline = Instant::now() + BATCH;
    let mut changes: HashMap<(usize, u64), bool> = HashMap::new();
    let mut take = |message: Message| -> Result<(), Batch> {
        match message {
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
    Some(Batch::Changes(out))
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
