//! What a large live delta costs (FR-42: the scaled rescan limit, the cheaper
//! delta, the duty-gated view; research in `docs/notes/delta-limit-research.md`).
//!
//! Builds a synthetic frozen base (default 1,000,000 entries, a fixture tree
//! with MFT-style record numbers `id + 16`), then grows one `live::Live`
//! through `Live::apply` in 500-change batches to each checkpoint and prints,
//! at every checkpoint, `DELTA-*` lines:
//!
//! - `apply`: mean and worst wall time of the last 20 batches before the
//!   checkpoint (`Live::apply` only, no view, no record fetch cost beyond a
//!   hash-map clone);
//! - `view`: wall time of `Live::view()` on the default rayon pool, on a
//!   1-thread pool (CPU-time proxy) and on a pool like the service's
//!   low-priority background pool (where the service builds it);
//! - `publish`: one steady-state batch, `apply(500)` then `view()`, which is
//!   what the maintainer does whenever a view is due;
//! - `storm`: a minute of 200 ms batches on a fake clock through the
//!   maintainer's `PublishGate`: views built and the core share they take,
//!   without and with the duty gate;
//! - `search`: the public `ferrowd::handle` path (the real snapshot search
//!   with the delta merge) for a few queries, median of 7 runs, against the
//!   same base with no live view as the reference;
//! - `memory`: capacity-counted bytes of the live layer and of the published
//!   view per delta entry, plus the process private-bytes growth and the
//!   peak while the view was built;
//! - `compact` (FR-42 part 2): `Live::compact`, which folds the delta into a
//!   new frozen index in memory instead of a disk rescan: wall time on the
//!   service's pool, on one thread (a CPU proxy) and on the default pool, the
//!   private-byte peak per entry, the new live layer's bytes per entry, and
//!   the same searches against the compacted index, and `DELTA-SORT-READY`:
//!   the time until the size and the date sort orders (the ones a compaction
//!   asks the new index to prepare, as the service does) are ready, alone and
//!   both at once.
//!
//! Change mix per batch: 70 % new file (into one of 2000 "build output"
//! folders), 25 % modified existing file (base entry hidden, delta entry
//! added), 5 % deleted existing file.
//!
//! `FERROW_DELTA_BASE` (default 1000000), `FERROW_DELTA_CHECKPOINTS`
//! (default `50000,200000,1000000`, live delta entries; changes are added
//! until the delta holds that many, whatever the rescan limit says),
//! `FERROW_DELTA_PROXIES=1` also times two rebuild-from-memory proxies (the
//! research for an in-memory compaction; a few seconds at 1 M).
//! Measurement only; never a service path.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ferrow_fixture::Rng;
use ferrow_index::{ColumnSet, EntryMetadata, Index, IndexBuilder, SortKey, SortStatus, flags};
use ferrow_ipc::{Request, Response, Sort};
use ferrow_walk::VolumeMap;
use ferrow_walk::ntfs::RecordInfo;
use ferrowd::live::{Change, Live, LiveView, RecordSource, RecordState};
// NEW-API-BEGIN
use ferrowd::live::{DeltaLimits, PublishGate};
// NEW-API-END
use ferrowd::{Snapshot, SnapshotStore, handle, memory};

const BATCH: usize = 500;
const FIRST_RECORD: u64 = 16;
const FILETIME_UNIX_EPOCH: i64 = 116_444_736_000_000_000;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn file_info(size: u64, modified: i64) -> RecordInfo {
    RecordInfo {
        in_use: true,
        is_dir: false,
        sequence: 1,
        attributes: 0x20,
        created: 0,
        modified,
        accessed: 0,
        size,
        reparse_tag: 0,
    }
}

#[derive(Default)]
struct Fake(HashMap<u64, RecordState>);

impl RecordSource for Fake {
    fn fetch(&self, _volume: usize, record: u64) -> Option<RecordState> {
        self.0.get(&record).cloned()
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

/// Generates the next batches of the change stream.
struct Stream {
    rng: Rng,
    /// Base file ids in random order: each is modified or deleted at most once.
    files: Vec<u32>,
    next_file: usize,
    /// Folders new files go to (as records), a build-output-like pool.
    out_dirs: Vec<u64>,
    next_record: u64,
    counter: u64,
}

impl Stream {
    fn new(base: &Index) -> Stream {
        let n = base.len() as u32;
        let mut rng = Rng::new(7);
        let mut files: Vec<u32> = (1..n)
            .filter(|&i| base.flags(i) & flags::DIR == 0)
            .collect();
        for i in (1..files.len()).rev() {
            let j = rng.below(i + 1);
            files.swap(i, j);
        }
        let dirs: Vec<u32> = (0..n)
            .filter(|&i| base.flags(i) & flags::DIR != 0)
            .collect();
        let out_dirs = (0..2000)
            .map(|_| u64::from(dirs[rng.below(dirs.len())]) + FIRST_RECORD)
            .collect();
        Stream {
            rng,
            files,
            next_file: 0,
            out_dirs,
            next_record: u64::from(n) + FIRST_RECORD + 1_000_000,
            counter: 0,
        }
    }

    fn batch(&mut self, base: &Index) -> (Vec<Change>, Fake) {
        let mut changes = Vec::with_capacity(BATCH);
        let mut fake = Fake::default();
        let fresh = FILETIME_UNIX_EPOCH + 1_900_000_000 * 10_000_000;
        for _ in 0..BATCH {
            self.counter += 1;
            let r = self.rng.below(100);
            if r < 70 || self.next_file >= self.files.len() {
                let record = self.next_record;
                self.next_record += 1;
                let dir = self.out_dirs[self.rng.below(self.out_dirs.len())];
                fake.0.insert(
                    record,
                    RecordState {
                        info: file_info(4096 + self.counter, fresh),
                        names: vec![(dir, format!("build-obj-{}.o", self.counter).into_bytes())],
                    },
                );
                changes.push(Change {
                    volume: 0,
                    record,
                    created: true,
                });
            } else {
                let id = self.files[self.next_file];
                self.next_file += 1;
                let record = u64::from(id) + FIRST_RECORD;
                if r < 95 {
                    let parent = u64::from(base.parent(id)) + FIRST_RECORD;
                    fake.0.insert(
                        record,
                        RecordState {
                            info: file_info(base.metadata(id).size.unwrap_or(0) + 1, fresh),
                            names: vec![(parent, base.name(id).to_vec())],
                        },
                    );
                }
                changes.push(Change {
                    volume: 0,
                    record,
                    created: false,
                });
            }
        }
        (changes, fake)
    }
}

fn base_index(n: usize) -> (Arc<Index>, VolumeMap) {
    let builder = IndexBuilder::with_columns(ColumnSet::DEFAULT);
    let mut entries: Vec<(u64, u32)> = Vec::with_capacity(n);
    let mut id = 0u32;
    ferrow_fixture::generate_with(n, 2026, |parent, name, entry_flags, meta: EntryMetadata| {
        let this = builder.add_with_metadata(parent, name, entry_flags, meta);
        debug_assert_eq!(this, id);
        entries.push((u64::from(this) + FIRST_RECORD, this));
        id += 1;
        this
    });
    let map = VolumeMap {
        device: "bench".into(),
        entries: entries.into(),
        roots: vec![(FIRST_RECORD, 0)],
        ..VolumeMap::default()
    };
    (Arc::new(builder.freeze()), map)
}

fn run_query(store: &SnapshotStore, text: &str, limit: u32) -> (Duration, usize) {
    let started = Instant::now();
    let out = handle(
        store,
        Request::Query {
            text: text.into(),
            limit,
            sort: Sort::Name,
            flags: 0,
        },
    );
    let took = started.elapsed();
    let rows = out
        .iter()
        .map(|r| match r {
            Response::Results { paths, .. } => paths.len(),
            Response::Error { msg, .. } => {
                println!("DELTA-NOTE query {text:?} error: {msg}");
                0
            }
            _ => 0,
        })
        .sum();
    (took, rows)
}

fn search_times(store: &SnapshotStore) -> Vec<(String, Duration, usize)> {
    // `report`: a fixture word in many base names, none in the delta.
    // `build-obj`: only delta names (every new file), first 100 asked for.
    // `.o`: matches every new file and some base names.
    // `fixture`: matches nearly nothing; shows the pure scan cost.
    let queries: [(&str, u32); 5] = [
        ("report", 100),
        ("build-obj-1", 100),
        ("build-obj", 100),
        (".o", 100),
        ("zzzqq", 100),
    ];
    queries
        .iter()
        .map(|&(text, limit)| {
            let _ = run_query(store, text, limit);
            let runs: Vec<(Duration, usize)> =
                (0..7).map(|_| run_query(store, text, limit)).collect();
            let rows = runs[0].1;
            (
                format!("{text} limit={limit}"),
                median(runs.into_iter().map(|r| r.0).collect()),
                rows,
            )
        })
        .collect()
}

fn count_time(store: &SnapshotStore, text: &str) -> Duration {
    let one = || {
        let started = Instant::now();
        let _ = handle(store, Request::Count { text: text.into() });
        started.elapsed()
    };
    let _ = one();
    median((0..7).map(|_| one()).collect())
}

fn live_bytes(live: &Live) -> (u64, u64) {
    let mut delta = 0;
    let mut other = 0;
    for p in live.memory_parts() {
        match p.name.as_str() {
            "live.delta" | "live.delta_children" => delta += p.bytes,
            "live.removed" | "live.overlay" | "live.overrides" => other += p.bytes,
            _ => {}
        }
    }
    (delta, other)
}

fn view_bytes(view: &LiveView) -> u64 {
    view.memory_parts().iter().map(|p| p.bytes).sum()
}

// NEW-API-BEGIN
/// Milliseconds until each sort order is ready, from one request of all of
/// them at once, as the service asks (`request_sort_starting` with the
/// background priority); polled every millisecond.
fn sort_ready_ms(index: &Index, keys: &[SortKey]) -> Vec<f64> {
    let t = Instant::now();
    for &key in keys {
        index.request_sort_starting(key, ferrow_walk::priority::enter_background);
    }
    let mut ready: Vec<Option<f64>> = vec![None; keys.len()];
    while ready.iter().any(Option::is_none) {
        for (&key, at) in keys.iter().zip(ready.iter_mut()) {
            if at.is_none() && index.request_sort(key) == SortStatus::Ready {
                *at = Some(ms(t.elapsed()));
            }
        }
        assert!(t.elapsed() < Duration::from_secs(120), "sort never ready");
        std::thread::sleep(Duration::from_millis(1));
    }
    ready.into_iter().flatten().collect()
}

/// What folding the live layer into a new frozen index in memory costs
/// (FR-42, part 2: `Live::compact`, which the service runs instead of a disk
/// rescan when the delta outgrows its limit), at this checkpoint's delta:
///
/// - `wall_service_pool`: on a pool like the service's low-priority one (what
///   the maintainer waits for; journal changes queue meanwhile), `one_thread`
///   (a CPU-time proxy: nothing runs in parallel) and `default_pool`;
/// - `peak_growth_B_per_entry`: the process's private-byte peak during the
///   service-pool run, above what was held before it (the old index, the live
///   layer and the published view stay alive meanwhile), per entry of the new
///   index; a disk rescan peaks at about 161 B/entry (`memory_breakdown`);
/// - `new_live_B_per_entry`: what the new live layer's tables hold;
/// - the same searches against the compacted index as `DELTA-SEARCH` rows,
///   to show a search is no slower afterwards (it merges no delta any more).
fn compaction_row(
    target: usize,
    live: &mut Live,
    base: &Arc<Index>,
    svc: &rayon::ThreadPool,
    one: &rayon::ThreadPool,
    svc_threads: usize,
) {
    let old_len = base.len();
    let delta = live.delta_len();
    let _ = memory::release_free_heap();
    let before = memory::process().bytes();
    memory::reset_peak();
    let t = Instant::now();
    let new = svc
        .install(|| live.compact())
        .expect("no folder is being read");
    let wall_svc = t.elapsed();
    let peak = memory::process().peak.unwrap_or(0);
    let held = memory::process().bytes();
    let entries = new.base().len();
    let new_live: u64 = new.memory_parts().iter().map(|p| p.bytes).sum();
    let new_base = new.base().clone();
    drop(new);
    let t = Instant::now();
    let again = one
        .install(|| live.compact())
        .expect("no folder is being read");
    let wall_one = t.elapsed();
    let single_size = again.base().clone();
    drop(again);
    let t = Instant::now();
    let again = live.compact().expect("no folder is being read");
    let wall_default = t.elapsed();
    let single_modified = again.base().clone();
    drop(again);
    println!(
        "DELTA-COMPACT checkpoint={target} base_entries={old_len} delta_len={delta} new_entries={entries} compact_ms_service_pool_{svc_threads}_threads={:.0} compact_ms_one_thread={:.0} compact_ms_default_pool={:.0} peak_growth_mb={:.0} compact_peak_B_per_entry={:.1} held_growth_mb={:.0} new_live_B_per_entry={:.1}",
        ms(wall_svc),
        ms(wall_one),
        ms(wall_default),
        peak.saturating_sub(before) as f64 / 1e6,
        peak.saturating_sub(before) as f64 / entries as f64,
        held.saturating_sub(before) as f64 / 1e6,
        new_live as f64 / entries as f64,
    );
    // The sort orders the service asks the new index for right after the swap
    // (the ones that were ready before): each on its own thread at the lowest
    // priority, both at once as the service does, then each alone.
    let both = sort_ready_ms(&new_base, &[SortKey::Size, SortKey::Modified]);
    let size = sort_ready_ms(&single_size, &[SortKey::Size]);
    let modified = sort_ready_ms(&single_modified, &[SortKey::Modified]);
    println!(
        "DELTA-SORT-READY checkpoint={target} entries={entries} size_ms_alone={:.0} modified_ms_alone={:.0} size_ms_with_both={:.0} modified_ms_with_both={:.0}",
        size[0], modified[0], both[0], both[1]
    );
    // The searches the service would answer right after the swap, and the
    // same ones against the old base with its delta (printed above).
    let store = SnapshotStore::new(Snapshot {
        index: new_base,
        live: None,
        mode: "live".into(),
        report: String::new(),
    });
    for (q, t, rows) in search_times(&store) {
        println!(
            "DELTA-SEARCH checkpoint={target} after_compaction query=\"{q}\" ms={:.2} rows={rows}",
            ms(t)
        );
    }
    println!(
        "DELTA-COUNT checkpoint={target} after_compaction query=\"report\" ms={:.2}",
        ms(count_time(&store, "report"))
    );
}
// NEW-API-END

fn main() {
    let n = env_usize("FERROW_DELTA_BASE", 1_000_000);
    let checkpoints: Vec<usize> = std::env::var("FERROW_DELTA_CHECKPOINTS")
        .unwrap_or_else(|_| "50000,200000,1000000".into())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let started = Instant::now();
    let (base, map) = base_index(n);
    println!(
        "DELTA-CONFIG base_entries={} build_ms={:.0} rayon_threads={} batch={BATCH}",
        base.len(),
        ms(started.elapsed()),
        rayon::current_num_threads(),
    );
    let mut stream = Stream::new(&base);
    let one = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap();
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    // Like the service's background pool: a quarter of the cores, at most 8,
    // lowest thread priority.
    let svc_threads = (cores / 4).clamp(2, 8);
    let svc = rayon::ThreadPoolBuilder::new()
        .num_threads(svc_threads)
        .start_handler(|_| ferrow_walk::priority::enter_background())
        .build()
        .unwrap();

    // Reference: the base without any live layer.
    let plain = SnapshotStore::new(Snapshot {
        index: base.clone(),
        live: None,
        mode: "scan".into(),
        report: String::new(),
    });
    for (q, t, rows) in search_times(&plain) {
        println!(
            "DELTA-SEARCH delta=none query=\"{q}\" ms={:.2} rows={rows}",
            ms(t)
        );
    }
    println!(
        "DELTA-COUNT delta=none query=\"report\" ms={:.2}",
        ms(count_time(&plain, "report"))
    );
    // NEW-API-BEGIN
    println!(
        "DELTA-LIMITS base={} {:?}",
        base.len(),
        DeltaLimits::for_base(base.len())
    );
    // NEW-API-END

    // Proxy for an in-memory compaction that re-adds every entry to a new
    // builder and freezes it (no merge shortcuts): wall time and memory peak.
    if std::env::var("FERROW_DELTA_PROXIES").is_ok_and(|v| v == "1") {
        let steady: u64 = ferrowd::live::index_parts(&base, "index")
            .iter()
            .filter(|p| !p.name.ends_with(".order"))
            .map(|p| p.bytes)
            .sum();
        println!(
            "DELTA-INDEX steady_bytes={steady} per_entry={:.1}",
            steady as f64 / base.len() as f64
        );
        let _ = memory::release_free_heap();
        let before = memory::process().bytes();
        memory::reset_peak();
        let t = Instant::now();
        let b = IndexBuilder::with_columns(base.columns_present());
        for id in 0..base.len() as u32 {
            b.add_with_metadata(
                base.parent(id),
                base.name(id),
                base.flags(id),
                base.metadata(id),
            );
        }
        let added = t.elapsed();
        let t2 = Instant::now();
        let rebuilt = b.freeze();
        let frozen = t2.elapsed();
        let peak = memory::process().peak.unwrap_or(0);
        println!(
            "DELTA-COMPACT-PROXY entries={} add_ms={:.0} freeze_ms={:.0} peak_growth_mb={:.0} peak_growth_B_per_entry={:.1} (old index still held)",
            rebuilt.len(),
            ms(added),
            ms(frozen),
            peak.saturating_sub(before) as f64 / 1e6,
            peak.saturating_sub(before) as f64 / rebuilt.len() as f64
        );
        let one_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        drop(rebuilt);
        let t = Instant::now();
        one_pool.install(|| {
            let b = IndexBuilder::with_columns(base.columns_present());
            for id in 0..base.len() as u32 {
                b.add_with_metadata(
                    base.parent(id),
                    base.name(id),
                    base.flags(id),
                    base.metadata(id),
                );
            }
            let _ = b.freeze();
        });
        println!(
            "DELTA-COMPACT-PROXY one_thread_total_ms={:.0}",
            ms(t.elapsed())
        );
    }

    // Proxy for a merge-based compaction (no builder, no re-sort): hide 10 %
    // of the base ids, then rank-remap the sorted order and the parent column
    // and copy the kept names, one thread. The delta's own entries would be
    // spliced into the sorted order by binary search and are not included.
    if std::env::var("FERROW_DELTA_PROXIES").is_ok_and(|v| v == "1") {
        let n = base.len();
        let mut alive = vec![u64::MAX; n.div_ceil(64)];
        let mut rng = Rng::new(99);
        for _ in 0..n / 10 {
            let id = 1 + rng.below(n - 1);
            alive[id / 64] &= !(1u64 << (id % 64));
        }
        let before = memory::process().bytes();
        memory::reset_peak();
        let t = Instant::now();
        let mut rank = vec![0u32; n + 1];
        let mut run = 0u32;
        for id in 0..n {
            rank[id] = run;
            run += ((alive[id / 64] >> (id % 64)) & 1) as u32;
        }
        let kept = run as usize;
        let sorted: Vec<u32> = base
            .sorted()
            .iter()
            .filter(|&&id| (alive[id as usize / 64] >> (id % 64)) & 1 == 1)
            .map(|&id| rank[id as usize])
            .collect();
        let mut parent = Vec::with_capacity(kept);
        let mut names: Vec<u8> = Vec::new();
        let mut folded_len = 0usize;
        for id in 0..n as u32 {
            if (alive[id as usize / 64] >> (id % 64)) & 1 == 1 {
                let p = base.parent(id);
                parent.push(if p == ferrow_index::ROOT_PARENT {
                    p
                } else {
                    rank[p as usize]
                });
                names.extend_from_slice(base.name(id));
                folded_len += base.name(id).len();
            }
        }
        let took = t.elapsed();
        let peak = memory::process().peak.unwrap_or(0);
        println!(
            "DELTA-MERGE-PROXY kept={} one_thread_ms={:.0} (rank table, sorted remap, parent remap, name copy; folded arena copy not included, similar to names) peak_growth_mb={:.0} sorted_len={} parent_len={} names={} folded={}",
            kept,
            ms(took),
            peak.saturating_sub(before) as f64 / 1e6,
            sorted.len(),
            parent.len(),
            names.len(),
            folded_len,
        );
    }
    // Dead delta entries: one file written again and again, a batch each.
    {
        let (small, map) = base_index(100_000);
        let mut live = Live::new(small.clone(), std::slice::from_ref(&map));
        let id = 5000u32;
        let record = u64::from(id) + FIRST_RECORD;
        let parent = u64::from(small.parent(id)) + FIRST_RECORD;
        for i in 0..2000u64 {
            let mut fake = Fake::default();
            fake.0.insert(
                record,
                RecordState {
                    info: file_info(
                        10_000 + i,
                        FILETIME_UNIX_EPOCH + 1_900_000_000 * 10_000_000 + i as i64,
                    ),
                    names: vec![(parent, small.name(id).to_vec())],
                },
            );
            let _ = live.apply(
                &[Change {
                    volume: 0,
                    record,
                    created: false,
                }],
                &fake,
            );
        }
        println!(
            "DELTA-REPEAT one file written in 2000 batches: delta_len={} (alive is 1; 1 is right, more are dead entries); view_entries={}",
            live.delta_len(),
            live.view().delta.len()
        );
        // NEW-API-BEGIN
        // A rewritten file keeps one delta entry (FR-42): this is a check,
        // not only a figure, so the CI smoke run fails on a regression.
        assert_eq!(
            live.delta_len(),
            1,
            "a rewritten file left dead delta entries"
        );
        // NEW-API-END
    }
    let _ = memory::release_free_heap();
    let private0 = memory::process().bytes();
    let mut live = Live::new(base.clone(), std::slice::from_ref(&map));
    drop(map);
    let private_live = memory::process().bytes();
    println!(
        "DELTA-MEMORY live_new_private_growth_mb={:.1} (record tables, removed bits; independent of the delta)",
        private_live.saturating_sub(private0) as f64 / 1e6
    );
    let mut batch_times: Vec<Duration> = Vec::new();
    let mut asked: Option<(usize, String)> = None;
    let mut asked_removed: Option<(usize, String)> = None;
    let mut done_batches = 0usize;
    let mut applied_total = 0usize;
    let mut prev_private = memory::process().bytes();

    for &target in &checkpoints {
        while live.delta_len() < target {
            let (changes, fake) = stream.batch(&base);
            let t = Instant::now();
            let out = live.apply(&changes, &fake);
            let took = t.elapsed();
            applied_total += out.changed;
            done_batches += 1;
            batch_times.push(took);
            if let Some(why) = out.rescan {
                if why.ends_with("live changes") {
                    asked.get_or_insert((live.delta_len(), why.to_string()));
                } else {
                    asked_removed.get_or_insert((live.delta_len(), why.to_string()));
                }
            }
        }
        let last: Vec<Duration> = batch_times.iter().rev().take(20).copied().collect();
        let mean = last.iter().sum::<Duration>() / last.len() as u32;
        let worst = last.iter().max().copied().unwrap();
        println!(
            "DELTA-APPLY checkpoint={target} delta_len={} batches={done_batches} changes_applied={applied_total} mean_ms_per_{BATCH}={:.2} worst_ms={:.2}",
            live.delta_len(),
            ms(mean),
            ms(worst)
        );

        // Memory: capacity counted, then process growth and the build peak.
        let before = memory::process().bytes();
        memory::reset_peak();
        let t = Instant::now();
        let view = live.view();
        let view_default = t.elapsed();
        let peak = memory::process().peak.unwrap_or(0);
        let t = Instant::now();
        let v1 = one.install(|| live.view());
        let view_one = t.elapsed();
        let t = Instant::now();
        let v2 = svc.install(|| live.view());
        let view_svc = t.elapsed();
        drop((v1, v2));
        let t = Instant::now();
        let v3 = live.view();
        let view_again = t.elapsed();
        drop(v3);
        println!(
            "DELTA-VIEW checkpoint={target} delta_len={} view_entries={} default_pool_ms={:.1} again_ms={:.1} one_thread_ms={:.1} service_pool_{svc_threads}_threads_ms={:.1} build_peak_growth_mb={:.1}",
            live.delta_len(),
            view.delta.len(),
            ms(view_default),
            ms(view_again),
            ms(view_one),
            ms(view_svc),
            peak.saturating_sub(before) as f64 / 1e6
        );
        let (dl, other) = live_bytes(&live);
        let vb = view_bytes(&view);
        let after = memory::process().bytes();
        println!(
            "DELTA-MEMORY checkpoint={target} live_delta_B_per_entry={:.1} live_other_B={} view_B_per_view_entry={:.1} live+view_B_per_delta_entry={:.1} process_private_growth_since_prev_checkpoint_mb={:.1} private_now_mb={:.1}",
            dl as f64 / live.delta_len() as f64,
            other,
            vb as f64 / view.delta.len() as f64,
            (dl + vb) as f64 / live.delta_len() as f64,
            after.saturating_sub(prev_private) as f64 / 1e6,
            after as f64 / 1e6,
        );
        prev_private = after;

        // Steady state publish: one batch applied, then a new view.
        let mut publish = Vec::new();
        let mut apply_only = Vec::new();
        for _ in 0..5 {
            let (changes, fake) = stream.batch(&base);
            let t = Instant::now();
            let _ = live.apply(&changes, &fake);
            apply_only.push(t.elapsed());
            let _v = live.view();
            publish.push(t.elapsed());
        }
        println!(
            "DELTA-PUBLISH checkpoint={target} apply_ms={:.2} apply_plus_view_ms={:.1} (median of 5; the maintainer does this whenever a view is due)",
            ms(median(apply_only)),
            ms(median(publish))
        );
        // NEW-API-BEGIN
        // A minute of storm on a fake clock: a batch of changes every 200 ms,
        // a view costing what it just cost on the background pool. Ungated, a
        // view follows every batch; gated, the maintainer's duty gate decides.
        // The core share is the view's one-thread CPU over the elapsed time.
        {
            let build = view_svc;
            let t0 = Instant::now();
            let mut gate = PublishGate::new();
            let mut now = t0;
            let mut views = 0u32;
            let mut longest_gap = Duration::ZERO;
            let mut last = t0;
            for _ in 0..300 {
                gate.changed();
                if gate.due(now) {
                    now += build;
                    gate.published(now, build);
                    views += 1;
                    longest_gap = longest_gap.max(now - last);
                    last = now;
                }
                now += Duration::from_millis(200);
            }
            println!(
                "DELTA-STORM checkpoint={target} view_ms={:.1} cpu_one_thread_ms={:.1} views_per_minute_ungated=300 views_per_minute_gated={views} core_share_ungated={:.2} core_share_gated={:.2} longest_gap_between_views_ms={:.0}",
                ms(build),
                ms(view_one),
                ms(view_one) * 300.0 / 60_000.0,
                ms(view_one) * f64::from(views) / 60_000.0,
                ms(longest_gap),
            );
        }
        // NEW-API-END

        let store = SnapshotStore::new(Snapshot {
            index: base.clone(),
            live: Some(Arc::new(view)),
            mode: "live".into(),
            report: String::new(),
        });
        for (q, t, rows) in search_times(&store) {
            println!(
                "DELTA-SEARCH checkpoint={target} query=\"{q}\" ms={:.2} rows={rows}",
                ms(t)
            );
        }
        println!(
            "DELTA-COUNT checkpoint={target} query=\"report\" ms={:.2} query=\".o\" ms={:.2}",
            ms(count_time(&store, "report")),
            ms(count_time(&store, ".o")),
        );
        // NEW-API-BEGIN
        compaction_row(target, &mut live, &base, &svc, &one, svc_threads);
        // NEW-API-END
    }
    println!(
        "DELTA-ASKS first_delta_rescan_ask={:?} first_removed_rescan_ask={:?} base_removed_limit={}",
        asked,
        asked_removed,
        base.len() / 10 + 1000
    );
}
