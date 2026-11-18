//! Where the service's memory goes, from a synthetic MFT-style build (FR-37).
//!
//! Hosted-only measurement, never a local workload: it builds the structures
//! the service holds for a 5 M entry NTFS volume without admin rights or a
//! disk, then prints every component by capacity, the process's own counters
//! and the unattributed remainder as `FERROW-MEMORY*` lines.
//!
//! The record stream is made from `ferrow-fixture`'s tree: each entry becomes
//! an MFT record (numbers with gaps for free records, a share of files with a
//! second hard-link name), generated chunk by chunk so the source adds almost
//! nothing to the measured peaks, and goes through the service's own path, in
//! order:
//!
//! 1. `Parsed` chunks pushed into `ntfs::VolumeBuilder` like the parallel MFT
//!    parse does,
//! 2. `VolumeBuilder::finish` (the volume),
//! 3. `ferrow_walk::emit_parsed_volume` (the scan's emission, which also fills
//!    the `VolumeMap`), then `IndexBuilder::freeze`, each with its own peak,
//! 4. `live::Live::new` and `drop(maps)`, as `live_windows::maintain` does,
//! 5. live changes through `Live::apply` in batches, with `Live::view()` after
//!    each batch (only the last view is kept),
//! 6. both default metadata sort orders, built on first use,
//! 7. an index save (streamed through a 1 MiB buffer), then the saved file
//!    mapped back in place of the heap index, as the service does after every
//!    scan and compaction (ADR 0007): the `scan1.mapped` phase is the steady
//!    state the service keeps,
//! 8. a second scan while the old index and its published view are still held
//!    (a rescan; the old `Live` is dropped first, as `maintain` does),
//! 9. the same scan as the service makes it since ADR 0007 steps 8 and 9
//!    (`bounded`): the volume builder keeps its tables in temporary files
//!    (`VolumeBuilder::with_spill`), the builder's shards spill to temporary
//!    files past a 32 MiB budget, the record map is appended to a file, the
//!    freeze writes the index file in place (`freeze_to_file`) and the live
//!    layer's tables are built in files (`Live::new_in`, then the children
//!    table `saved_ref` needs). Its `bounded.parse`, `bounded.from_parsed`,
//!    `bounded.emit`, `bounded.freeze`, `bounded.live_new` and
//!    `bounded.children` peaks sit beside the heap build's `scan2.*`; what the
//!    files hold is printed as `FERROW-MEMORY-FILES` (file cache, not private
//!    bytes). It is checked against the heap build's index entry for entry
//!    and must leave no temporary file behind. A `forgotten` round then times
//!    the join of the same volume with nothing, `FERROW_MEMORY_FORGET`
//!    (default 20000) and a tenth of the records forgotten, as after a
//!    journal refresh (`FERROW-MEMORY-TIMING round=forgotten`),
//! 10. the merge of the live changes into a new index (`compact`), in the heap
//!     (`Live::compact`) and into the index file (`Live::compact_into`), each
//!     with its peak,
//! 11. the steady state and the restart (`steady_and_restart`): the heap
//!     builds are dropped and the private bytes left are printed as
//!     `FERROW-MEMORY-STEADY phase=mapped`; the live tables are saved, all is
//!     dropped, and the index and tables are mapped and the live layer
//!     restored as at the service's start (`restart.load` peak,
//!     `FERROW-MEMORY-STEADY phase=restarted`).
//!
//! `scripts/memory-gate.py` reads this output in CI (the Windows job, 5 M
//! entries) and fails when a phase is over its limit: ADR 0007, step 7.
//!
//! Assumptions are printed as `FERROW-MEMORY-CONFIG`: free-record share,
//! hard-link share, change mix and count. Change them with the environment
//! variables below and the output follows.
//!
//! `FERROW_BENCH_ENTRIES` (default 5000000), `FERROW_MEMORY_CHANGES`
//! (default 20000), `FERROW_MEMORY_SPILL_BUDGET` (bytes the bounded build's
//! shards hold before they spill, default 32 MiB; a small value makes a small
//! run spill), `FERROW_MEMORY_SUMMARY` (a file to also append the Markdown
//! tables to; CI passes `$GITHUB_STEP_SUMMARY`; they are printed either way).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Instant;

use ferrow_fixture::Rng;
use ferrow_index::{
    ColumnSet, DEFAULT_SPILL_BUDGET, FileTarget, Index, IndexBuilder, SortKey, SortStatus, flags,
};
use ferrow_walk::ntfs::{self, NameRecord, Parsed, RecordInfo, Volume, VolumeBuilder};
use ferrowd::live::{Change, Live, LiveView, RecordSource, RecordState, index_parts};
use ferrowd::memory::{self, Breakdown, Kind, Part};

const SEED: u64 = 2026;
/// One in this many files gets a second (hard-link) name.
const HARD_LINK_ONE_IN: u64 = 100;
/// Free MFT records between used ones: this many in 100 steps skip a record.
const FREE_RECORD_PER_100: u64 = 12;
/// FILETIME ticks of 1970-01-01.
const FILETIME_UNIX_EPOCH: i64 = 116_444_736_000_000_000;
/// Records per parse chunk (a 16 MiB read of 1 KiB records).
const CHUNK_RECORDS: usize = 16_384;
const FOLDER_OPS: usize = 5;

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

fn dir_info(modified: i64) -> RecordInfo {
    RecordInfo {
        is_dir: true,
        attributes: 0x10,
        size: 0,
        ..file_info(0, modified)
    }
}

fn filetime(unix_ticks: i64) -> i64 {
    unix_ticks + FILETIME_UNIX_EPOCH
}

/// What the record stream turned out to hold.
struct Synth {
    /// MFT records, used or free.
    records: u64,
    entries: usize,
    links: usize,
}

/// A record stream shaped like an MFT read: record numbers ascend with gaps,
/// names and infos arrive in chunks of a 16 MiB read each. The stream is
/// generated chunk by chunk and keeps only its table of directories (about
/// 2 B/entry), so a phase's memory peak is the consumer's, not the source's.
/// `start` makes the consumer once the volume's record count is known;
/// `on_chunk` gets every chunk as the parallel parse would hand it over.
fn synth_stream<T>(
    n: usize,
    start: impl FnOnce(u64) -> T,
    mut on_chunk: impl FnMut(&mut T, Parsed),
) -> (Synth, T) {
    // MFT records, used or free: the gaps come from their own stream, run once
    // for the total (the volume's slot count) and once more as the entries
    // arrive. The hard-link draws continue the first run's stream.
    let mut link_rng = Rng::new(SEED ^ 0xF4EE);
    let mut next = 16u64;
    for _ in 1..n {
        while link_rng.chance(FREE_RECORD_PER_100, 100) {
            next += 1;
        }
        next += 1;
    }
    let records = next + next / 50;
    let mut sink = start(records);
    let mut gap_rng = Rng::new(SEED ^ 0xF4EE);
    let mut next = 16u64;
    // Directories by id (ascending), with their record numbers.
    let mut dirs: Vec<(u32, u64)> = vec![(0, ntfs::ROOT_RECORD)];
    let record_of_dir = |dirs: &[(u32, u64)], id: u32| {
        dirs[dirs
            .binary_search_by_key(&id, |d| d.0)
            .expect("parents are dirs")]
        .1
    };
    let mut part = Parsed::default();
    let mut links = 0usize;
    part.infos.push((ntfs::ROOT_RECORD, dir_info(0)));
    let mut id = 0u32;
    ferrow_fixture::generate_with(n, SEED, |parent, name, entry_flags, meta| {
        let this = id;
        id += 1;
        if this == 0 {
            return 0;
        }
        while gap_rng.chance(FREE_RECORD_PER_100, 100) {
            next += 1;
        }
        let record = next;
        next += 1;
        let modified = filetime(meta.modified.unwrap_or(0));
        let is_dir = entry_flags & flags::DIR != 0;
        part.infos.push((
            record,
            if is_dir {
                dir_info(modified)
            } else {
                file_info(meta.size.unwrap_or(0), modified)
            },
        ));
        let add_name = |part: &mut Parsed, parent_record: u64| {
            let start = part.arena.len() as u32;
            part.arena.extend_from_slice(name);
            part.names.push(NameRecord {
                record,
                parent_record,
                parent_sequence: 1,
                name_offset: start,
                name_len: name.len() as u16,
            });
        };
        add_name(&mut part, record_of_dir(&dirs, parent));
        if is_dir {
            dirs.push((this, record));
        } else if link_rng.chance(1, HARD_LINK_ONE_IN) {
            let other = dirs[link_rng.below(dirs.len())].1;
            add_name(&mut part, other);
            links += 1;
        }
        if part.infos.len() >= CHUNK_RECORDS {
            on_chunk(&mut sink, std::mem::take(&mut part));
        }
        this
    });
    on_chunk(&mut sink, part);
    (
        Synth {
            records,
            entries: n + links,
            links,
        },
        sink,
    )
}

/// The records a batch of changes will fetch (`None`: deleted).
#[derive(Default)]
struct Fake(HashMap<u64, RecordState>);

impl RecordSource for Fake {
    fn fetch(&self, _volume: usize, record: u64) -> Option<RecordState> {
        self.0.get(&record).cloned()
    }
}

struct Plan {
    changes: Vec<Change>,
    fake: Fake,
    counts: [usize; 7],
}

const KINDS: [&str; 7] = [
    "modify_file",
    "new_file",
    "delete_file",
    "rename_file",
    "folder_metadata",
    "rename_folder",
    "delete_folder",
];

/// Twenty thousand (by default) changes in the proportions of a busy desktop,
/// plus a few folder renames and deletes (the first one builds the frozen
/// children index). Needs the entry-to-record pairs of the scan.
fn plan_changes(base: &Index, entries: &ferrow_walk::Pairs, records: u64, count: usize) -> Plan {
    let mut rng = Rng::new(SEED ^ 0xC4A4);
    let n = base.len() as u32;
    let is_dir = |id: u32| base.flags(id) & flags::DIR != 0;
    let pick_dir = |rng: &mut Rng| loop {
        let id = rng.below(n as usize) as u32;
        if is_dir(id) {
            return id;
        }
    };
    let pick_file = |rng: &mut Rng| loop {
        let id = 1 + rng.below(n as usize - 1) as u32;
        if !is_dir(id) {
            return id;
        }
    };
    // Folders with few direct children that lie late in the tree, so a rename
    // or delete copies or hides a small subtree.
    let mut folder_ops = Vec::new();
    {
        let mut children = vec![0u32; n as usize];
        for id in 0..n {
            let p = base.parent(id);
            if p != ferrow_index::ROOT_PARENT {
                children[p as usize] += 1;
            }
        }
        for id in (n / 10 * 9..n).rev() {
            if is_dir(id) && (1..=20).contains(&children[id as usize]) {
                folder_ops.push(id);
                if folder_ops.len() == 2 * FOLDER_OPS {
                    break;
                }
            }
        }
    }
    struct Pick {
        kind: usize,
        id: u32,
        target: u32,
    }
    let mut picks: Vec<Pick> = Vec::with_capacity(count + 2 * FOLDER_OPS);
    for _ in 0..count {
        let r = rng.below(100);
        let kind = match r {
            0..=39 => 0,
            40..=59 => 1,
            60..=79 => 2,
            80..=94 => 3,
            _ => 4,
        };
        let (id, target) = match kind {
            1 => (0, pick_dir(&mut rng)),
            4 => (pick_dir(&mut rng).max(1), 0),
            _ => (pick_file(&mut rng), 0),
        };
        picks.push(Pick { kind, id, target });
    }
    for (i, &id) in folder_ops.iter().enumerate() {
        picks.push(Pick {
            kind: if i < FOLDER_OPS { 5 } else { 6 },
            id,
            target: 0,
        });
    }
    // Records of every id involved: one pass over the scan's pairs.
    let mut need: HashMap<u32, u64> = HashMap::new();
    for p in &picks {
        let id = if p.kind == 1 { p.target } else { p.id };
        need.insert(id, 0);
        let parent = base.parent(id);
        if parent != ferrow_index::ROOT_PARENT {
            need.insert(parent, 0);
        }
    }
    for (record, id) in entries.iter() {
        if let Some(slot) = need.get_mut(&id) {
            *slot = record;
        }
    }
    let rec = |id: u32| need[&id];
    let parent_rec = |id: u32| {
        let p = base.parent(id);
        if p == ferrow_index::ROOT_PARENT {
            ntfs::ROOT_RECORD
        } else {
            rec(p)
        }
    };
    let mut plan = Plan {
        changes: Vec::with_capacity(picks.len()),
        fake: Fake::default(),
        counts: [0; 7],
    };
    let fresh = filetime(1_900_000_000 * 10_000_000);
    let mut next_new = records + 1_000;
    for (i, p) in picks.iter().enumerate() {
        plan.counts[p.kind] += 1;
        let (record, created, state) = match p.kind {
            0 => {
                let record = rec(p.id);
                let old = base.metadata(p.id);
                (
                    record,
                    false,
                    Some(RecordState {
                        info: file_info(old.size.unwrap_or(0) + 4096, fresh + i as i64),
                        names: vec![(parent_rec(p.id), base.name(p.id).to_vec())],
                    }),
                )
            }
            1 => {
                next_new += 1;
                (
                    next_new,
                    true,
                    Some(RecordState {
                        info: file_info(1234 + i as u64, fresh),
                        names: vec![(rec(p.target), format!("new-file-{i}.dat").into_bytes())],
                    }),
                )
            }
            2 | 6 => (rec(p.id), false, None),
            3 => (
                rec(p.id),
                false,
                Some(RecordState {
                    info: file_info(base.metadata(p.id).size.unwrap_or(0), fresh),
                    names: vec![(parent_rec(p.id), format!("renamed-{i}.txt").into_bytes())],
                }),
            ),
            4 => (
                rec(p.id),
                false,
                Some(RecordState {
                    info: dir_info(fresh + i as i64),
                    names: vec![(parent_rec(p.id), base.name(p.id).to_vec())],
                }),
            ),
            _ => (
                rec(p.id),
                false,
                Some(RecordState {
                    info: dir_info(0),
                    names: vec![(parent_rec(p.id), format!("renamed-dir-{i}").into_bytes())],
                }),
            ),
        };
        if let Some(s) = state {
            plan.fake.0.insert(record, s);
        }
        plan.changes.push(Change {
            volume: 0,
            record,
            created,
        });
    }
    plan
}

/// Prints and remembers one measured moment.
struct Report {
    entries: u64,
    baseline: u64,
    table: Vec<(String, Breakdown)>,
    /// `(phase, bytes before, peak, note)`.
    peaks: Vec<(String, u64, u64, String)>,
}

impl Report {
    fn show(&mut self, phase: &str, parts: Vec<Part>) {
        let mut b = Breakdown::new_with_heap(0, parts, self.baseline);
        b.entries = self.entries;
        for line in b.lines(phase) {
            println!("{line}");
        }
        self.table.push((phase.to_string(), b));
    }

    /// The highest value the process reached during a phase (Linux: since the
    /// phase's `reset_peak`, high-water resident minus file-backed pages;
    /// Windows: the 1 ms sampled maximum of private bytes, a lower bound that
    /// also leaves out the real scan's 16 MiB read buffers).
    fn peak(&mut self, phase: &str, before: u64, peak: Option<u64>, note: &str) {
        let peak = peak.unwrap_or(0);
        println!(
            "FERROW-MEMORY-PEAK phase={phase} before_bytes={before} peak_bytes={peak} growth_bytes={} growth_per_entry={:.3} note={note}",
            peak.saturating_sub(before),
            peak.saturating_sub(before) as f64 / self.entries as f64,
        );
        self.peaks
            .push((phase.to_string(), before, peak, note.to_string()));
    }
}

fn transient(parts: &[(&'static str, usize)]) -> Vec<Part> {
    parts
        .iter()
        .map(|&(name, bytes)| Part::new(name, bytes, Kind::Transient))
        .collect()
}

/// What a finished scan leaves in the service's hands.
struct Held {
    live: Live,
    view: Arc<LiveView>,
}

/// What the snapshot store keeps while a rescan runs: the old index and the
/// last published view. `maintain` drops its `Live` (record maps, delta,
/// overrides, children index) at the end of the loop iteration, before the next
/// `scan_with_maps`, so the rescan does not hold it.
struct Retained {
    index: Arc<Index>,
    view: Arc<LiveView>,
}

/// One scan and live session built like `live_windows::maintain`: scan, build
/// `Live`, drop the maps, then apply changes. `held` is the previous snapshot
/// that stays in the store during the scan (the rescan case).
fn session(
    round: &str,
    n: usize,
    changes: usize,
    report: &mut Report,
    held: Option<&Retained>,
) -> Held {
    // Parse: chunks appended to the whole, as the parallel MFT parse does. The
    // source streams, so this phase's peak is the parse structures' own.
    let _ = memory::release_free_heap();
    let before = memory::process().bytes();
    memory::reset_peak();
    let parse_started = Instant::now();
    let (synth, parsed) = synth_stream(n, VolumeBuilder::new, |b, chunk| b.push(chunk));
    println!(
        "FERROW-MEMORY-TIMING round={round} step=parse ms={:.1}",
        parse_started.elapsed().as_secs_f64() * 1000.0
    );
    report.peak(
        &format!("{round}.parse"),
        before,
        memory::process().peak,
        "parse chunks pushed into the volume builder; peak since this phase began",
    );
    println!(
        "FERROW-MEMORY-CONFIG round={round} entries={} index_entries={} mft_records={} hard_link_names={} change_count={changes}",
        n, synth.entries, synth.records, synth.links,
    );
    let held_parts = |parts: &mut Vec<Part>| {
        if let Some(h) = held {
            parts.extend(
                index_parts(&h.index, "previous.index")
                    .into_iter()
                    .chain(h.view.memory_parts()),
            );
        }
    };
    report.entries = synth.entries as u64;
    let mut parts = transient(&parsed.heap_parts());
    held_parts(&mut parts);
    report.show(&format!("{round}.parsed"), parts);

    // Parse chunks -> Volume (both alive while the volume is joined).
    let before = memory::process().bytes();
    memory::reset_peak();
    let started = Instant::now();
    let records = synth.records;
    let volume: Volume = parsed.finish(0xFE77);
    report.peak(
        &format!("{round}.from_parsed"),
        before,
        memory::process().peak,
        "the parse chunks and the volume coexist while the volume is joined; peak since this phase began",
    );
    println!(
        "FERROW-MEMORY-TIMING round={round} step=from_parsed ms={:.1}",
        started.elapsed().as_secs_f64() * 1000.0
    );
    let mut parts = transient(&volume.heap_parts());
    held_parts(&mut parts);
    report.show(&format!("{round}.volume"), parts);

    // Emission, then freeze. As in the scan, the parsed volume is dropped once
    // it is emitted and before the freeze: the builder holds its records beside
    // the volume during emission, then beside the frozen columns.
    let builder = IndexBuilder::with_columns(ColumnSet::DEFAULT);
    let before = memory::process().bytes();
    memory::reset_peak();
    let started = Instant::now();
    let map = ferrow_walk::emit_parsed_volume(
        &volume,
        ntfs::ROOT_RECORD,
        b"C:",
        &builder,
        ColumnSet::DEFAULT,
    );
    report.peak(
        &format!("{round}.emit"),
        before,
        memory::process().peak,
        "volume, builder and record map while the subtree is emitted; peak since this phase began",
    );
    println!(
        "FERROW-MEMORY-TIMING round={round} step=emit ms={:.1}",
        started.elapsed().as_secs_f64() * 1000.0
    );
    let mut parts = transient(&volume.heap_parts());
    parts.extend(
        builder
            .memory_parts()
            .into_iter()
            .map(|(name, bytes)| Part::new(name, bytes, Kind::Transient)),
    );
    parts.push(Part::new(
        "scan.volume_map",
        map.heap_bytes(),
        Kind::Transient,
    ));
    held_parts(&mut parts);
    report.show(&format!("{round}.emitted"), parts);
    drop(volume);
    let before = memory::process().bytes();
    memory::reset_peak();
    let started = Instant::now();
    let index = builder.freeze();
    report.peak(
        &format!("{round}.freeze"),
        before,
        memory::process().peak,
        "builder records beside the frozen columns; peak since this phase began",
    );
    println!(
        "FERROW-MEMORY-TIMING round={round} step=freeze ms={:.1}",
        started.elapsed().as_secs_f64() * 1000.0
    );
    assert_eq!(index.len(), synth.entries, "every name became an entry");
    let mut parts = index_parts(&index, "index");
    parts.push(Part::new(
        "scan.volume_map",
        map.heap_bytes(),
        Kind::Transient,
    ));
    held_parts(&mut parts);
    report.show(&format!("{round}.scanned"), parts);

    // The service plans its changes from nothing; the benchmark needs the
    // pairs, which `Live::new` consumes, so plan first from a borrow.
    let plan = plan_changes(&index, &map.entries, records, changes);
    let counts: Vec<String> = KINDS
        .iter()
        .zip(plan.counts)
        .map(|(k, c)| format!("{k}={c}"))
        .collect();
    println!(
        "FERROW-MEMORY-CONFIG round={round} change_mix {}",
        counts.join(" ")
    );

    let base = Arc::new(index);
    let mut live = Live::new(base.clone(), std::slice::from_ref(&map));
    drop(map);
    let mut parts = live.breakdown(None, 0).parts;
    held_parts(&mut parts);
    report.show(&format!("{round}.live_new"), parts);

    // Apply in batches, publishing a view after each (the last one is kept).
    let started = Instant::now();
    let mut view = Arc::new(live.view());
    let mut applied = 0usize;
    let mut rescan_asked = None;
    for chunk in plan.changes.chunks(500) {
        let out = live.apply(chunk, &plan.fake);
        applied += out.changed;
        rescan_asked = rescan_asked.or(out.rescan);
        view = Arc::new(live.view());
    }
    println!(
        "FERROW-MEMORY-TIMING round={round} step=apply_and_views changes={} applied={applied} delta_len={} ms={:.1} rescan_asked={}",
        plan.changes.len(),
        live.delta_len(),
        started.elapsed().as_secs_f64() * 1000.0,
        rescan_asked.map_or_else(|| "no".into(), |why| why.to_string())
    );
    let mut parts = live.breakdown(Some(view.as_ref()), 0).parts;
    held_parts(&mut parts);
    report.show(&format!("{round}.changes"), parts);
    Held { live, view }
}

fn prepare_sorts(index: &Index) {
    for key in [SortKey::Size, SortKey::Modified] {
        index.request_sort(key);
    }
    for key in [SortKey::Size, SortKey::Modified] {
        let status = index
            .sort_ticket(key)
            .expect("default columns")
            .wait_timeout(std::time::Duration::from_secs(120));
        assert_eq!(status, SortStatus::Ready, "{key:?}");
    }
}

/// The ranked tables for one phase: by group, then by component (the delta
/// index's many small parts rolled up), then what the process held.
fn markdown(report: &Report, phase: &str) -> String {
    let Some((_, b)) = report.table.iter().find(|(p, _)| p == phase) else {
        return String::new();
    };
    let entries = b.entries as f64;
    let total = b.process.bytes() as f64;
    let mut rows: Vec<(String, f64, Kind)> = Vec::new();
    for p in &b.parts {
        let name = if p.name.starts_with("view.delta_index.") {
            "view.delta_index.* (the live view's small index)".to_string()
        } else {
            p.name.clone()
        };
        match rows.iter_mut().find(|r| r.0 == name) {
            Some(r) => r.1 += p.bytes as f64,
            None => rows.push((name, p.bytes as f64, p.kind)),
        }
    }
    let group_of = |name: &str| name.split('.').next().unwrap_or(name).to_string();
    let mut groups: Vec<(String, f64)> = Vec::new();
    for (name, bytes, _) in &rows {
        let g = group_of(name);
        match groups.iter_mut().find(|r| r.0 == g) {
            Some(r) => r.1 += bytes,
            None => groups.push((g, *bytes)),
        }
    }
    groups.push((
        "process baseline (runtime, binary, thread stacks)".into(),
        b.baseline as f64,
    ));
    groups.push((
        "UNATTRIBUTED (allocator overhead and free memory it holds, plus anything not counted)"
            .into(),
        b.unattributed() as f64,
    ));
    groups.sort_by(|a, b| b.1.total_cmp(&a.1));
    rows.push(("process baseline".into(), b.baseline as f64, Kind::Steady));
    rows.push(("UNATTRIBUTED".into(), b.unattributed() as f64, Kind::Steady));
    rows.sort_by(|a, b| b.1.total_cmp(&a.1));

    let mut out = String::new();
    let _ = writeln!(
        out,
        "### Memory breakdown, phase `{phase}`: {} entries, process {:.1} B/entry ({} private bytes)\n",
        b.entries,
        total / entries,
        b.process.bytes()
    );
    let _ = writeln!(out, "By group:\n");
    let _ = writeln!(out, "| group | bytes | B/entry | % of process |");
    let _ = writeln!(out, "|---|---:|---:|---:|");
    for (name, bytes) in &groups {
        let _ = writeln!(
            out,
            "| {name} | {bytes:.0} | {:.2} | {:.1} |",
            bytes / entries,
            bytes / total * 100.0
        );
    }
    let _ = writeln!(out, "\nBy component, ranked:\n");
    let _ = writeln!(out, "| component | bytes | B/entry | % of process | kind |");
    let _ = writeln!(out, "|---|---:|---:|---:|---|");
    for (name, bytes, kind) in rows.iter().filter(|r| r.1 != 0.0) {
        let _ = writeln!(
            out,
            "| {name} | {bytes:.0} | {:.2} | {:.1} | {} |",
            bytes / entries,
            bytes / total * 100.0,
            kind.as_str()
        );
    }
    if let Some(h) = b.process.heap {
        let _ = writeln!(
            out,
            "\nAllocator counters: {} in use, {} committed, {} free but held ({:.2} B/entry).",
            h.in_use,
            h.committed,
            h.committed.saturating_sub(h.in_use),
            h.committed.saturating_sub(h.in_use) as f64 / entries
        );
    }
    out
}

/// Every phase's process counters and the peaks, in run order.
fn markdown_phases(report: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "### Process memory by phase\n");
    let _ = writeln!(
        out,
        "| phase | private bytes | B/entry | accounted bytes | unattributed B/entry | heap in use | heap committed |"
    );
    let _ = writeln!(out, "|---|---:|---:|---:|---:|---:|---:|");
    for (phase, b) in &report.table {
        let (in_use, committed) = b.process.heap.map_or((0, 0), |h| (h.in_use, h.committed));
        let _ = writeln!(
            out,
            "| {phase} | {} | {:.1} | {} | {:.1} | {in_use} | {committed} |",
            b.process.bytes(),
            b.per_entry(b.process.bytes() as i64),
            b.accounted(),
            b.per_entry(b.unattributed())
        );
    }
    let _ = writeln!(out, "\n### Peaks (transient)\n");
    let _ = writeln!(
        out,
        "| phase | before | peak | growth | growth B/entry | what is alive |"
    );
    let _ = writeln!(out, "|---|---:|---:|---:|---:|---|");
    for (phase, before, peak, note) in &report.peaks {
        let growth = peak.saturating_sub(*before);
        let _ = writeln!(
            out,
            "| {phase} | {before} | {peak} | {growth} | {:.1} | {note} |",
            growth as f64 / report.entries as f64
        );
    }
    out
}

/// The policy of a bench run: room for the files is assumed (the machine
/// running it has it), and the usual limit for the heap.
fn bench_policy() -> ntfs::SpillPolicy {
    ntfs::SpillPolicy {
        heap_fallback_records: ferrow_walk::HEAP_FALLBACK_ENTRIES as u64,
        free_bytes: None,
    }
}

/// The cost of the grouping when a journal refresh forgot records after the
/// read (`VolumeBuilder::forget`): the same stream is finished in temporary
/// files three times, with nothing forgotten, with `FERROW_MEMORY_FORGET`
/// records forgotten (default 20 000, spread over the volume), and with a
/// tenth of the volume forgotten. Every name of the volume is checked against
/// the forgotten list twice (once to count, once to place), so this is the
/// per-name cost of that check. Prints `FERROW-MEMORY-TIMING round=forgotten`
/// lines.
fn forgotten_round(n: usize) {
    let dir = std::env::temp_dir().join(format!("ferrow-memory-forgotten-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("forgotten build directory");
    let wanted = env_usize("FERROW_MEMORY_FORGET", 20_000);
    for forgotten in [0, wanted, n / 10] {
        let (synth, mut parsed) = synth_stream(
            n,
            |records| VolumeBuilder::with_spill(records, &dir, &bench_policy()).expect("room"),
            |b, chunk| b.push(chunk),
        );
        if let Some(step) = synth.entries.checked_div(forgotten) {
            // Records spread over the volume, none of the metadata files.
            let step = step.max(1) as u64;
            let changed: std::collections::BTreeSet<u64> =
                (0..forgotten as u64).map(|i| 16 + i * step).collect();
            let unchanged = |r: u64| !changed.contains(&r);
            parsed.forget(&changed, unchanged, unchanged);
        }
        let started = Instant::now();
        let volume = parsed.finish(0xF026);
        println!(
            "FERROW-MEMORY-TIMING round=forgotten step=from_parsed ms={:.1} forgotten={forgotten} entries={} names={}",
            started.elapsed().as_secs_f64() * 1000.0,
            synth.entries,
            volume.name_count()
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The scan the service makes since ADR 0007 steps 8 and 9: the same record
/// stream as a `session`, but every table is bounded. The parsed volume's
/// per-record table, names and grouping are written to temporary files
/// (`VolumeBuilder::with_spill`), the builder spills its shards past a budget,
/// the record map is appended to a file, the freeze writes the index file in
/// place, and the live layer's tables are built in temporary files too
/// (`Live::new_in`). Its peaks are the ones to compare with the heap build's
/// `parse`, `from_parsed`, `emit` and `freeze`; `reference` is the heap build
/// of the same stream, which the result must equal entry for entry.
///
/// What the files hold is reported as `FERROW-MEMORY-FILES` lines: it is file
/// cache, not private memory, so it is not in the peaks.
fn bounded_round(n: usize, report: &mut Report, reference: &Index) -> Bounded {
    let round = "bounded";
    let dir = std::env::temp_dir().join(format!("ferrow-memory-bounded-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("bounded build directory");
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    let _ = memory::release_free_heap();
    let before = memory::process().bytes();
    memory::reset_peak();
    let started = Instant::now();
    let (synth, parsed) = synth_stream(
        n,
        |records| {
            VolumeBuilder::with_spill(records, &dir, &bench_policy())
                .expect("room beside the bench")
        },
        |b, chunk| b.push(chunk),
    );
    report.peak(
        &format!("{round}.parse"),
        before,
        memory::process().peak,
        "parse chunks pushed into a volume builder whose tables are in temporary files; peak since this phase began",
    );
    println!(
        "FERROW-MEMORY-TIMING round={round} step=parse ms={:.1}",
        started.elapsed().as_secs_f64() * 1000.0
    );
    println!(
        "FERROW-MEMORY-FILES round={round} part=volume_builder bytes={} note=temporary files (file cache, not private bytes)",
        parsed.file_bytes()
    );
    assert_eq!(parsed.note(), None, "every table went to a file");
    report.entries = synth.entries as u64;
    let mut parts = transient(&parsed.heap_parts());
    report.show(&format!("{round}.parsed"), parts);

    let before = memory::process().bytes();
    memory::reset_peak();
    let started = Instant::now();
    let volume: Volume = parsed.finish(0xFE77);
    report.peak(
        &format!("{round}.from_parsed"),
        before,
        memory::process().peak,
        "names, records and the grouping by parent finished in temporary files; peak since this phase began",
    );
    println!(
        "FERROW-MEMORY-TIMING round={round} step=from_parsed ms={:.1}",
        started.elapsed().as_secs_f64() * 1000.0
    );
    println!(
        "FERROW-MEMORY-FILES round={round} part=volume bytes={} note=temporary files (file cache, not private bytes)",
        volume.mapped_bytes()
    );
    assert!(volume.mapped_bytes() > 0 && volume.spill_note.is_none());
    parts = transient(&volume.heap_parts());
    report.show(&format!("{round}.volume"), parts);

    let budget = env_usize("FERROW_MEMORY_SPILL_BUDGET", DEFAULT_SPILL_BUDGET);
    println!("FERROW-MEMORY-CONFIG round={round} spill_budget_bytes={budget}");
    let builder = IndexBuilder::with_spill(ColumnSet::DEFAULT, &dir, budget);
    let before = memory::process().bytes();
    memory::reset_peak();
    let started = Instant::now();
    let map = ferrow_walk::emit_parsed_volume_in(
        &volume,
        ntfs::ROOT_RECORD,
        b"C:",
        &builder,
        ColumnSet::DEFAULT,
        Some(&dir),
    );
    report.peak(
        &format!("{round}.emit"),
        before,
        memory::process().peak,
        "volume in files, spilling builder (default budget) and record map in a file while the subtree is emitted; peak since this phase began",
    );
    println!(
        "FERROW-MEMORY-TIMING round={round} step=emit ms={:.1} spilled_bytes={} spill_write_ms={:.1}",
        started.elapsed().as_secs_f64() * 1000.0,
        builder.spilled_bytes(),
        builder.spill_time().as_secs_f64() * 1000.0
    );
    println!(
        "FERROW-MEMORY-FILES round={round} part=record_map bytes={} note=temporary file (file cache, not private bytes)",
        map.file_bytes()
    );
    assert_eq!(
        map.entries.len(),
        synth.entries,
        "every entry is in the map"
    );
    let mut parts = transient(&volume.heap_parts());
    parts.extend(
        builder
            .memory_parts()
            .into_iter()
            .map(|(name, bytes)| Part::new(name, bytes, Kind::Transient)),
    );
    parts.push(Part::new(
        "scan.volume_map",
        map.heap_bytes(),
        Kind::Transient,
    ));
    report.show(&format!("{round}.emitted"), parts);
    drop(volume);

    let path = dir.join("index.bin");
    let before = memory::process().bytes();
    memory::reset_peak();
    let started = Instant::now();
    let (index, t) = builder
        .freeze_to_file_with(&FileTarget {
            path: &path,
            lock: None,
        })
        .expect("freeze into the index file");
    report.peak(
        &format!("{round}.freeze"),
        before,
        memory::process().peak,
        "spilled shards placed in the index file, names folded, order sorted and file checksummed inside its memory map; peak since this phase began",
    );
    println!(
        "FERROW-MEMORY-TIMING round={round} step=freeze ms={:.1} scatter_ms={:.1} fold_ms={:.1} sort_ms={:.1} checksum_ms={:.1} flush_ms={:.1} replace_ms={:.1} file_bytes={} spilled_bytes={}",
        started.elapsed().as_secs_f64() * 1000.0,
        ms(t.scatter),
        ms(t.fold),
        ms(t.sort),
        ms(t.checksum),
        ms(t.flush),
        ms(t.replace),
        t.file_bytes,
        t.spilled_bytes,
    );
    assert_eq!(index.len(), synth.entries, "every name became an entry");
    assert!(index.is_mapped(), "the result reads its file");
    assert_eq!(index.len(), reference.len());
    assert_eq!(
        index.sorted(),
        reference.sorted(),
        "same name order as the heap build"
    );
    for id in (0..index.len() as u32).step_by((index.len() / 997).max(1)) {
        assert_eq!(index.name(id), reference.name(id));
        assert_eq!(index.parent(id), reference.parent(id));
        assert_eq!(index.metadata(id), reference.metadata(id));
    }
    let mut parts = index_parts(&index, "index");
    parts.push(Part::new(
        "scan.volume_map",
        map.heap_bytes(),
        Kind::Transient,
    ));
    report.show(&format!("{round}.built"), parts);

    // The live layer over the new index, as `maintain` builds it: its tables
    // in temporary files beside the index, the record map consumed and dropped.
    let base = Arc::new(index);
    let before = memory::process().bytes();
    memory::reset_peak();
    let started = Instant::now();
    let live = Live::new_in(base.clone(), std::slice::from_ref(&map), Some(&dir));
    drop(map);
    report.peak(
        &format!("{round}.live_new"),
        before,
        memory::process().peak,
        "the live layer's frozen tables built in temporary files from the record map; peak since this phase began",
    );
    println!(
        "FERROW-MEMORY-TIMING round={round} step=live_new ms={:.1}",
        started.elapsed().as_secs_f64() * 1000.0
    );
    println!(
        "FERROW-MEMORY-FILES round={round} part=live_tables bytes={} note=temporary files (file cache, not private bytes)",
        live.mapped_bytes()
    );
    assert!(live.mapped_bytes() > 0);
    // What saving the tables first needs: the frozen children table.
    let facts = [ferrowd::saved::VolumeFacts {
        device: "synthetic".into(),
        serial: 0xFE77,
        journal: Some((1, 1)),
        record_size: 1024,
    }];
    let before = memory::process().bytes();
    memory::reset_peak();
    let started = Instant::now();
    let checksum = base.file_checksum().expect("a mapped file has a checksum");
    let children = live.saved_ref(checksum, 0, &facts).children_first.len();
    report.peak(
        &format!("{round}.children"),
        before,
        memory::process().peak,
        "the frozen children table, built in temporary files for the saved tables; peak since this phase began",
    );
    println!(
        "FERROW-MEMORY-TIMING round={round} step=children ms={:.1} entries={}",
        started.elapsed().as_secs_f64() * 1000.0,
        children - 1
    );
    let live_parts = live.breakdown(None, 0).parts;
    report.show(&format!("{round}.live"), live_parts);
    Bounded {
        live,
        base,
        dir,
        facts,
    }
}

/// What the bounded build leaves in the service's hands: the live layer over
/// the index file, held until the end so the steady state and the restart
/// can be measured after the heap builds are gone.
struct Bounded {
    live: Live,
    base: Arc<Index>,
    dir: std::path::PathBuf,
    facts: [ferrowd::saved::VolumeFacts; 1],
}

/// The steady state and the restart, which the CI memory gate reads
/// (`scripts/memory-gate.py`).
///
/// `bounded_round` ends with the live layer over its index file, but the heap
/// builds before it are still held, so the process counters there say nothing
/// about the service's own steady state. Here the heap builds are dropped
/// first and the private bytes that remain are measured: the bounded build's
/// result, as the service holds it after a scan (`FERROW-MEMORY-STEADY
/// phase=mapped`).
///
/// Then the restart (ADR 0007 decision 5): the live tables are saved beside
/// the index, everything is dropped, and the index and the tables are mapped
/// again and a live layer is restored from them, as `try_restore` does at
/// start. Its peak (`FERROW-MEMORY-PEAK phase=restart.load`) and the private
/// bytes afterwards (`FERROW-MEMORY-STEADY phase=restarted`) are what a
/// restart costs; the saved tables are read through maps, so both are small.
fn steady_and_restart(held: Bounded, report: &mut Report) {
    let Bounded {
        live,
        base,
        dir,
        facts,
    } = held;
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    let steady = |report: &Report, phase: &str, live: &Live| {
        let _ = memory::release_free_heap();
        let now = memory::process_with_heap();
        let private = now.bytes();
        let grown = private.saturating_sub(report.baseline);
        println!(
            "FERROW-MEMORY-STEADY phase={phase} entries={} private_bytes={private} baseline_bytes={} growth_bytes={grown} growth_per_entry={:.3} mapped_bytes={} note=private bytes after the heap builds are dropped; the mapped files are file cache, not private bytes",
            report.entries,
            report.baseline,
            grown as f64 / report.entries as f64,
            live.mapped_bytes() + live.base().mapped_bytes(),
        );
    };
    report.show("steady.mapped", live.breakdown(None, 0).parts);
    steady(report, "mapped", &live);

    // Save the live tables beside the index.
    let index_path = dir.join("index.bin");
    let tables_path = ferrowd::saved::path_beside(&index_path);
    let checksum = base.file_checksum().expect("a mapped file has a checksum");
    let fingerprint = ferrowd::saved::fingerprint(&[b"memory_breakdown"]);
    let before = memory::process().bytes();
    memory::reset_peak();
    let started = Instant::now();
    ferrowd::saved::write(&tables_path, &live.saved_ref(checksum, fingerprint, &facts))
        .expect("save the live tables");
    println!(
        "FERROW-MEMORY-TIMING round=restart step=save_tables ms={:.1} file_bytes={}",
        ms(started.elapsed()),
        std::fs::metadata(&tables_path).map_or(0, |m| m.len())
    );
    report.peak(
        "restart.save_tables",
        before,
        memory::process().peak,
        "the live tables streamed to live.bin; peak since this phase began",
    );
    drop((live, base));

    // The restart: map both files, restore the live layer.
    let _ = memory::release_free_heap();
    let before = memory::process().bytes();
    memory::reset_peak();
    let started = Instant::now();
    let base = Arc::new(Index::load(&index_path).expect("map the saved index"));
    assert!(base.is_mapped(), "a current file is mapped");
    let saved = ferrowd::saved::read(
        &tables_path,
        base.file_checksum().expect("checksum"),
        base.len(),
        fingerprint,
    )
    .expect("the saved tables belong to the index");
    let live = Live::restore(base, saved);
    println!(
        "FERROW-MEMORY-TIMING round=restart step=load ms={:.1} entries={}",
        ms(started.elapsed()),
        live.base().len()
    );
    report.peak(
        "restart.load",
        before,
        memory::process().peak,
        "index and live tables mapped, checksums read through the maps, live layer restored; peak since this phase began",
    );
    report.show("restarted.live", live.breakdown(None, 0).parts);
    steady(report, "restarted", &live);
    drop(live);

    let left: Vec<_> = std::fs::read_dir(&dir)
        .expect("bounded build directory")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".ferrow-"))
        .collect();
    assert!(left.is_empty(), "temporary files left behind: {left:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The merge of the live changes into a new index, in the heap
/// (`Live::compact`) and straight into the index file (`Live::compact_into`,
/// what the service does since ADR 0007 step 8), each with its own peak. The
/// live layer is the one `scan2` left, with its changes applied.
fn compaction_round(live: &mut Live, report: &mut Report) {
    let dir = std::env::temp_dir().join(format!("ferrow-memory-compaction-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("compaction directory");
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;

    let _ = memory::release_free_heap();
    let before = memory::process().bytes();
    memory::reset_peak();
    let started = Instant::now();
    let heap = live.compact().expect("no folder read is outstanding");
    let took = started.elapsed();
    report.peak(
        "compact.heap",
        before,
        memory::process().peak,
        "Live::compact in the heap: builder, then every column beside it, plus the new tables; peak since this phase began",
    );
    println!(
        "FERROW-MEMORY-TIMING round=compact step=heap ms={:.1} entries={}",
        ms(took),
        heap.base().len()
    );
    let entries = heap.base().len();
    drop(heap);

    let path = dir.join("index.bin");
    let _ = memory::release_free_heap();
    let before = memory::process().bytes();
    memory::reset_peak();
    let started = Instant::now();
    let (filed, built) = live
        .compact_into(Some(&FileTarget {
            path: &path,
            lock: None,
        }))
        .expect("no folder read is outstanding");
    let took = started.elapsed();
    report.peak(
        "compact.file",
        before,
        memory::process().peak,
        "Live::compact_into the index file: spilling builder, file freeze in a map, plus the new tables; peak since this phase began",
    );
    let t = built.timings.expect("built into the file");
    println!(
        "FERROW-MEMORY-TIMING round=compact step=file ms={:.1} freeze_ms={:.1} scatter_ms={:.1} fold_ms={:.1} sort_ms={:.1} checksum_ms={:.1} flush_ms={:.1} replace_ms={:.1} spilled_bytes={} entries={}",
        ms(took),
        ms(t.total),
        ms(t.scatter),
        ms(t.fold),
        ms(t.sort),
        ms(t.checksum),
        ms(t.flush),
        ms(t.replace),
        t.spilled_bytes,
        filed.base().len(),
    );
    assert_eq!(filed.base().len(), entries, "the same entries either way");
    assert!(filed.base().is_mapped(), "the new index reads its file");
    drop(filed);
    let _ = std::fs::remove_dir_all(&dir);
}

fn main() {
    let n = env_usize("FERROW_BENCH_ENTRIES", 5_000_000);
    let changes = env_usize("FERROW_MEMORY_CHANGES", 20_000);
    assert!(n >= 1_000, "needs at least 1000 entries");
    // Start the worker threads first so their stacks are in the baseline.
    rayon::broadcast(|_| {});
    let baseline = memory::process().bytes();
    println!(
        "FERROW-MEMORY-ACTIVE example=memory_breakdown entries={n} changes={changes} seed={SEED} allocator={} counters={} hard_link_one_in={HARD_LINK_ONE_IN} free_record_per_100={FREE_RECORD_PER_100} folder_ops={}",
        if cfg!(all(target_os = "linux", target_env = "gnu")) {
            "glibc-malloc"
        } else if cfg!(windows) {
            "windows-process-heap"
        } else {
            "system"
        },
        if memory::process().private.is_some() {
            "private-bytes"
        } else {
            "resident-only"
        },
        2 * FOLDER_OPS,
    );
    let mut report = Report {
        entries: n as u64,
        baseline,
        table: Vec::new(),
        peaks: Vec::new(),
    };
    println!(
        "FERROW-MEMORY-PROCESS phase=start private_bytes={baseline} baseline_bytes={baseline}"
    );

    // Round 1: the first scan and a live session on top of it.
    let mut one = session("scan1", n, changes, &mut report, None);

    // Sort orders are built by the first sorted query; both defaults here.
    let before = memory::process().bytes();
    memory::reset_peak();
    let started = Instant::now();
    prepare_sorts(one.live.base());
    println!(
        "FERROW-MEMORY-TIMING round=scan1 step=sort_orders ms={:.1}",
        started.elapsed().as_secs_f64() * 1000.0
    );
    report.peak(
        "scan1.sort_orders",
        before,
        memory::process().peak,
        "sort scratch and the finished orders; peak since this phase began",
    );
    let steady = |held: &Held| held.live.breakdown(Some(held.view.as_ref()), 0).parts;
    report.show("scan1.steady", steady(&one));

    // What the allocator would give back (glibc only): the free memory it
    // still holds from the build's transients.
    let held_before_trim = memory::process();
    if memory::release_free_heap() {
        report.show("scan1.steady_trimmed", steady(&one));
        let after = memory::process();
        println!(
            "FERROW-MEMORY-TRIM reclaimed_bytes={} reclaimed_per_entry={:.3}",
            held_before_trim.bytes().saturating_sub(after.bytes()),
            held_before_trim.bytes().saturating_sub(after.bytes()) as f64 / report.entries as f64
        );
    } else {
        println!("FERROW-MEMORY-TRIM reclaimed_bytes=na note=no allocator trim on this platform");
    }

    // Save: streamed to the file through a small buffer.
    let path = std::env::temp_dir().join(format!("ferrow-memory-{}.bin", std::process::id()));
    let file_len = one.live.base().serialized_len().expect("size");
    let before = memory::process().bytes();
    memory::reset_peak();
    let started = Instant::now();
    one.live.base().save(&path).expect("save");
    println!(
        "FERROW-MEMORY-TIMING round=scan1 step=save ms={:.1} file_bytes={file_len}",
        started.elapsed().as_secs_f64() * 1000.0
    );
    report.peak(
        "scan1.save",
        before,
        memory::process().peak,
        "streamed: growth should be about the 1 MiB write buffer, not the file",
    );

    // Map the saved file back in place of the heap index, as the service
    // does after every scan and compaction (ADR 0007).
    let started = Instant::now();
    let mapped = Arc::new(ferrow_index::Index::load(&path).expect("map the saved index"));
    assert!(mapped.is_mapped(), "a current file is mapped");
    println!(
        "FERROW-MEMORY-TIMING round=scan1 step=map ms={:.1} mapped_bytes={}",
        started.elapsed().as_secs_f64() * 1000.0,
        mapped.mapped_bytes()
    );
    one.live.replace_base(mapped);
    report.show("scan1.mapped", steady(&one));
    println!(
        "FERROW-MEMORY component=index.mapped bytes={} per_entry={:.3} kind=steady note=file cache, not private bytes",
        one.live.base().mapped_bytes(),
        one.live.base().mapped_bytes() as f64 / report.entries as f64
    );

    // Round 2: a rescan while the old snapshot (index and view) is still in the
    // store. The old `Live` is dropped first, as `maintain` does.
    let Held { live, view } = one;
    let old = Retained {
        index: live.base().clone(),
        view,
    };
    drop(live);
    let before = memory::process();
    let mut two = session("scan2", n, changes, &mut report, Some(&old));
    let during = memory::process();
    println!(
        "FERROW-MEMORY-PEAK phase=rescan_old_and_new_held before_bytes={} now_bytes={} growth_bytes={} growth_per_entry={:.3} note=old index and view stay in the store while the scan builds the next; the old live layer is already dropped",
        before.bytes(),
        during.bytes(),
        during.bytes().saturating_sub(before.bytes()),
        during.bytes().saturating_sub(before.bytes()) as f64 / report.entries as f64
    );
    drop(old);
    report.show("scan2.after_swap", steady(&two));
    if memory::release_free_heap() {
        report.show("scan2.after_swap_trimmed", steady(&two));
    }

    // The bounded build, against the heap build of the same stream.
    let bounded = bounded_round(n, &mut report, two.live.base());
    forgotten_round(n);
    compaction_round(&mut two.live, &mut report);
    // The heap builds are gone: what is left is the service's steady state,
    // then its restart from the saved files (the CI memory gate reads both).
    drop(two);
    steady_and_restart(bounded, &mut report);

    // The result: the table to read.
    println!("FERROW-MEMORY-DONE");
    let mut summary = String::from("## Ferrow service memory breakdown (synthetic MFT build)\n\n");
    summary.push_str(&markdown_phases(&report));
    summary.push('\n');
    for phase in [
        "scan1.steady",
        "scan1.mapped",
        "scan2.after_swap",
        "bounded.built",
        "bounded.live",
    ] {
        summary.push_str(&markdown(&report, phase));
        summary.push('\n');
    }
    if let Ok(path) = std::env::var("FERROW_MEMORY_SUMMARY")
        && !path.is_empty()
    {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .expect("summary file");
        file.write_all(summary.as_bytes()).expect("summary write");
    }
    println!("{summary}");
    let _ = std::fs::remove_file(&path);
}
