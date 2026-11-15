//! Windows side of live updates: one change-journal reader thread per
//! NTFS volume and the maintainer thread that rescans and applies batches.
//!
//! Idle cost is zero: readers sleep inside `FSCTL_READ_USN_JOURNAL` until
//! the volume changes, and the maintainer sleeps on a channel. A batch is
//! collected for [`BATCH`] after the first change, then applied and
//! published, so a change shows up in results well within a second.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use ferrow_walk::{VolumeMap, WalkOptions, ntfs};

use crate::live::{Change, HARD_DELTA, Live, RecordSource, RecordState, RescanPolicy};
use crate::{Snapshot, SnapshotStore};

/// How long changes are collected after the first one of a batch.
const BATCH: Duration = Duration::from_millis(200);
/// `USN_REASON_FILE_CREATE`.
const REASON_CREATE: u32 = 0x100;

enum Message {
    Records {
        device: String,
        records: Vec<(u64, u32)>,
    },
    /// The journal was deleted, recreated or overrun: rescan.
    Reset { device: String, reason: String },
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
        eprintln!("ferrowd: live updates unavailable: {error}");
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
    let mut readers: HashMap<String, ()> = HashMap::new();
    let mut reason = "startup".to_string();
    let pool = background_pool();
    // With a saved index loaded, even the startup scan is background work:
    // that index answers searches meanwhile.
    let mut background = !store.get().index.is_empty();
    loop {
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
            eprintln!("ferrowd: scan failed: {fatal}");
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
        eprintln!("ferrowd: {summary}");
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
                Err(e) => eprintln!("ferrowd: journal reader for {}: {e}", map.device),
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
            eprintln!("ferrowd: cannot reopen every volume; live updates stopped");
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
                Ok(changes) => changes,
                Err(reset) => {
                    rescan.request(reset);
                    continue;
                }
            };
            if changes.is_empty() {
                continue; // woke for a pending rescan
            }
            let outcome = live.apply(&changes, &volumes);
            if let Some(why) = outcome.rescan {
                rescan.request(why);
            }
            if outcome.changed == 0 {
                continue;
            }
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
) -> Option<Result<Vec<Change>, String>> {
    let first = match wake_at {
        None => rx.recv().ok()?,
        Some(at) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
            Ok(message) => message,
            Err(RecvTimeoutError::Timeout) => return Some(Ok(Vec::new())),
            Err(RecvTimeoutError::Disconnected) => return None,
        },
    };
    let deadline = Instant::now() + BATCH;
    let mut changes: HashMap<(usize, u64), bool> = HashMap::new();
    let mut take = |message: Message| -> Result<(), String> {
        match message {
            Message::Reset { device, reason } => Err(format!("{device}: {reason}")),
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
    if let Err(reset) = take(first) {
        return Some(Err(reset));
    }
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(message) => {
                if let Err(reset) = take(message) {
                    return Some(Err(reset));
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
    Some(Ok(out))
}

fn read_journal(device: String, journal: (u64, i64), tx: Sender<Message>) {
    let volume = match ntfs::open_volume(&device) {
        Ok(v) => v,
        Err(e) => {
            let _ = tx.send(Message::Reset {
                device,
                reason: format!("cannot open the volume: {e}"),
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
                    });
                    return;
                }
                // Deleted, recreated or overrun: continue from the journal's
                // current end and let the maintainer rescan.
                let Some((new_id, _, end)) = ntfs::query_journal(&volume) else {
                    let _ = tx.send(Message::Reset {
                        device,
                        reason: format!("change journal unavailable: {error}"),
                    });
                    return;
                };
                id = new_id;
                next = end;
                if tx
                    .send(Message::Reset {
                        device: device.clone(),
                        reason: format!("change journal reset: {error}"),
                    })
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}
