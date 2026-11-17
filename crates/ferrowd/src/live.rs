//! Live index updates (M2d).
//!
//! The scanned index stays frozen. Changes reported by a volume's change
//! journal are applied on top of it:
//!
//! - a replaced or deleted entry is hidden through an "allowed" bit set
//!   (searches skip it at no extra cost per entry);
//! - new and changed entries go into a small second index (the delta),
//!   rebuilt per batch; its entries hang below "shadow" roots named with
//!   the full path of their frozen parent folder;
//! - a folder whose only change is its own size or dates keeps its entry
//!   and gets a metadata override (its children stay in place);
//! - a renamed or moved folder moves its whole subtree into the delta;
//! - a folder moved in from outside the index (or from an excluded place)
//!   gets an entry at once and its subtree is read by a bounded background
//!   walk ([`WalkJob`]) whose result is merged later (see [`Live::finish_walk`]).
//!
//! When the delta grows large, the owner rescans the volume (seconds with
//! the MFT reader) and starts over from a fresh frozen index.
//!
//! The logic here is platform-independent; the journal readers and record
//! fetching live in `live_windows`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use ferrow_index::{ColumnSet, EntryMetadata, Index, IndexBuilder, ROOT_PARENT, flags};
use ferrow_walk::VolumeMap;
use ferrow_walk::ntfs::RecordInfo;
use ferrow_walk::subtree::{Limits, Subtree, WalkError};

/// Rescan once the overlay holds this many live delta entries…
const MAX_DELTA: usize = 50_000;
/// …or once this share of frozen entries is hidden (1/10)…
const MAX_REMOVED_DIVISOR: usize = 10;
/// …or a single folder move would copy more than this many entries.
const MAX_MOVE: usize = 100_000;

/// A folder moved in from outside the index is read by a background walk of
/// at most this many entries (and never more than the delta has room for:
/// every walked entry becomes a delta entry, and past [`MAX_DELTA`] a
/// rescan is asked for anyway), …
const MIN_WALK_BUDGET: usize = 1_000;
/// …at most this long (a cold hard disk lists on the order of 10,000
/// entries per second, so a full budget fits; a rescan reads 27 million
/// entries in about 74 s on the owner's PC, which is cheaper per entry
/// beyond that), …
pub const MAX_WALK_TIME: Duration = Duration::from_secs(30);
/// A finished walk at least this long is an event in the service log.
pub const NOTABLE_WALK: Duration = Duration::from_secs(1);
/// What the log line at the start of a live session says is on.
pub fn walk_limits_text() -> String {
    format!(
        "up to {MAX_DELTA} entries and {} s each, at most {MAX_WALKS} at once",
        MAX_WALK_TIME.as_secs()
    )
}
/// …with this many folders waiting at once (a package manager moves dozens
/// of sibling folders in one burst; the reader takes them one by one), …
const MAX_WALKS: usize = 64;
/// …at most this many records remembered as changed while walks run (they
/// are looked at again once a result is merged), …
const MAX_TOUCHED: usize = MAX_DELTA;
/// …and a walk the reader has not finished this long after its time budget
/// is given up (a folder on a hung network share or cloud placeholder can
/// block the listing call itself).
pub const WALK_SLACK: Duration = Duration::from_secs(10);
/// A record number is the low 48 bits of a file reference.
const RECORD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// Why a rescan was asked for: the stable start of the reason text, which
/// [`reason_kind`] maps to a counter name.
pub const REASON_MOVED_IN: &str = "a folder moved into an indexed location";
pub const REASON_LARGE_MOVE: &str = "a large folder moved";
pub const REASON_ADMIN: &str = "requested by an administrator";

/// The counter a rescan reason belongs to (reasons carry numbers and
/// device names, so they cannot be counted as written).
pub fn reason_kind(reason: &str) -> &'static str {
    if reason.starts_with(REASON_MOVED_IN) {
        "moved-in folder"
    } else if reason.starts_with(REASON_LARGE_MOVE) {
        "large folder move"
    } else if reason.ends_with(" live changes") {
        "too many live changes"
    } else if reason.ends_with(" replaced entries") {
        "too many replaced entries"
    } else if reason == REASON_ADMIN {
        "administrator"
    } else if reason == "startup" {
        "startup"
    } else {
        "journal"
    }
}

/// A pending rescan waits until a full scan's worth of time has passed
/// this many times over (rescans take at most 1/10 of the time)…
const RESCAN_DUTY: u32 = 9;
/// …but at least this long…
const MIN_RESCAN_GAP: Duration = Duration::from_secs(60);
/// …and at most this long.
const MAX_RESCAN_GAP: Duration = Duration::from_secs(30 * 60);
/// A pending rescan starts at once when the delta, removed entries
/// included, grows past this (bounds its memory and per-batch view cost).
pub const HARD_DELTA: usize = 8 * MAX_DELTA;

/// When a requested rescan may start. A rescan is a full scan: on a busy
/// disk the triggers (a folder moved in, a full delta, a journal reset)
/// can arrive every few seconds, and rescanning on each kept the service
/// scanning, and holding two indexes, nearly all the time. Changes keep
/// being applied while a rescan waits.
#[derive(Debug)]
pub struct RescanPolicy {
    pending: Option<String>,
    earliest: Instant,
}

impl RescanPolicy {
    /// After a scan that took `took` and finished at `now`.
    pub fn after_scan(now: Instant, took: Duration) -> RescanPolicy {
        let gap = (took * RESCAN_DUTY).clamp(MIN_RESCAN_GAP, MAX_RESCAN_GAP);
        RescanPolicy {
            pending: None,
            earliest: now + gap,
        }
    }

    /// Ask for a rescan; the first reason is kept. True when this started a
    /// pending episode (nothing was pending before): only then is it news.
    pub fn request(&mut self, reason: String) -> bool {
        let fresh = self.pending.is_none();
        self.pending.get_or_insert(reason);
        fresh
    }

    /// An administrator asked for a rescan now: it replaces the waiting
    /// reason and is not held back by the rate limit.
    pub fn force(&mut self, now: Instant, reason: String) {
        self.pending = Some(reason);
        self.earliest = now;
    }

    pub fn pending(&self) -> Option<&str> {
        self.pending.as_deref()
    }

    /// When a pending rescan may start.
    pub fn earliest(&self) -> Instant {
        self.earliest
    }

    /// The reason to rescan now: one is pending and its gap has passed, or
    /// `urgent` (the delta outgrew [`HARD_DELTA`]).
    pub fn due(&mut self, now: Instant, urgent: bool) -> Option<String> {
        if self.pending.is_some() && (urgent || now >= self.earliest) {
            self.pending.take()
        } else {
            None
        }
    }

    /// When the maintainer must wake to start a pending rescan.
    pub fn wake_at(&self) -> Option<Instant> {
        self.pending.as_ref().map(|_| self.earliest)
    }
}

/// Where an entry lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Loc {
    Base(u32),
    Delta(u32),
}

#[derive(Debug, Clone)]
struct DeltaEntry {
    parent: Loc,
    name: Vec<u8>,
    flags: u16,
    metadata: EntryMetadata,
    volume: usize,
    record: u64,
    alive: bool,
}

/// The current state of one MFT record, fetched through the file system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordState {
    pub info: RecordInfo,
    /// `(parent record, stored name bytes)` for every non-DOS name.
    pub names: Vec<(u64, Vec<u8>)>,
}

/// One changed record reported by a journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Change {
    pub volume: usize,
    pub record: u64,
    /// The journal reported the file as newly created in this batch.
    pub created: bool,
}

/// Reads records' current state (`None`: deleted or unreadable).
pub trait RecordSource {
    fn fetch(&self, volume: usize, record: u64) -> Option<RecordState>;
}

struct Volume {
    /// Frozen entry of each record (`u32::MAX`: none); hard links and
    /// moved records live in `overlay` instead.
    record_base: Vec<u32>,
    overlay: HashMap<u64, Vec<Loc>>,
    excluded: Arc<HashSet<u64>>,
    /// Configured roots: their entries never move (their own parent may be
    /// outside the index), only their size and dates follow.
    roots: HashSet<u64>,
}

/// What a search needs to see the live state.
#[derive(Debug)]
pub struct LiveView {
    /// Frozen entries still current (bit set = shown).
    pub allowed: Arc<Vec<u64>>,
    /// New and changed entries; shadow roots are hidden via `delta_allowed`.
    pub delta: Arc<Index>,
    pub delta_allowed: Arc<Vec<u64>>,
    /// Fresh metadata of frozen folders whose size or dates changed.
    pub overrides: Arc<HashMap<u32, EntryMetadata>>,
    /// Entries a search can return in total.
    pub entries: usize,
}

impl LiveView {
    /// Metadata of a frozen entry, with any live override applied.
    pub fn base_metadata(&self, base: &Index, id: u32) -> EntryMetadata {
        self.overrides
            .get(&id)
            .copied()
            .unwrap_or_else(|| base.metadata(id))
    }
}

/// One folder to read in the background: it moved into an indexed place
/// from somewhere the index has no entries for (outside the roots, an
/// excluded place, another volume). Hand it to a walker thread, then give
/// the result to [`Live::finish_walk`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkJob {
    pub id: u64,
    pub volume: usize,
    /// The folder's record number and sequence, which name it whatever
    /// happens to its path meanwhile.
    pub record: u64,
    pub sequence: u16,
    pub limits: Limits,
    /// Records of this volume the walk must leave out (the scan's excludes).
    pub excluded: Arc<HashSet<u64>>,
}

/// Result of applying one batch.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// Records whose entries changed.
    pub changed: usize,
    /// Entries a finished walk added.
    pub merged: usize,
    /// The overlay is too large or a change cannot be applied
    /// incrementally (a folder too large to read in the background, a
    /// failed walk): rescan the volumes.
    pub rescan: Option<String>,
    /// Folders to read in the background.
    pub walks: Vec<WalkJob>,
    /// Folder reads given up because the reader never answered: the reader
    /// is stuck and must be replaced.
    pub abandoned: usize,
}

impl Applied {
    /// Fold the result of another step into this one.
    pub fn absorb(&mut self, other: Applied) {
        self.changed += other.changed;
        self.merged += other.merged;
        self.abandoned += other.abandoned;
        self.walks.extend(other.walks);
        if let Some(reason) = other.rescan {
            self.rescan.get_or_insert(reason);
        }
    }
}

/// A folder being read.
struct Walk {
    id: u64,
    volume: usize,
    record: u64,
    sequence: u16,
    /// Position in the shared change log when it was queued: the records
    /// reported after it are the ones to look at again once it is merged.
    since: u64,
    /// When the reader began it (for the watchdog); none while it waits.
    started: Option<Instant>,
    /// The log could not keep every record since `since`.
    overflowed: bool,
    /// Folders that moved in below this one while it was read: they have
    /// an entry already, so their contents are only read if the walk did
    /// not list them (checked when its result is merged).
    covered: Vec<(u64, u16)>,
}

/// When a record was last reported while folders were being read, and when
/// it was last reported as created (positions in the log; 0: never).
#[derive(Clone, Copy)]
struct Touch {
    last: u64,
    created: u64,
}

pub struct Live {
    base: Arc<Index>,
    columns: ColumnSet,
    removed: Vec<u64>,
    removed_count: usize,
    delta: Vec<DeltaEntry>,
    delta_alive: usize,
    /// Delta entries by parent (alive or not; readers filter). Without it
    /// every folder visited by a move or delete scanned the whole delta.
    delta_children: HashMap<Loc, Vec<u32>>,
    overrides: HashMap<u32, EntryMetadata>,
    volumes: Vec<Volume>,
    /// Frozen id → `(volume << 48) | record`, `u64::MAX` when unmapped.
    base_record: Vec<u64>,
    /// Frozen children by parent, built on the first folder move.
    base_children: OnceLock<(Vec<u32>, Vec<u32>)>,
    /// Folders waiting for their background walk.
    walks: Vec<Walk>,
    next_walk: u64,
    /// One log of the records reported while any walk is outstanding.
    log: HashMap<(usize, u64), Touch>,
    log_seq: u64,
}

impl Live {
    pub fn new(base: Arc<Index>, maps: &[VolumeMap]) -> Live {
        let n = base.len();
        let mut base_record = vec![u64::MAX; n];
        let mut volumes = Vec::with_capacity(maps.len());
        for (v, map) in maps.iter().enumerate() {
            let records = map.entries.iter().map(|&(r, _)| r + 1).max().unwrap_or(0);
            let mut record_base = vec![u32::MAX; records as usize];
            let mut overlay: HashMap<u64, Vec<Loc>> = HashMap::new();
            for &(record, id) in &map.entries {
                if (id as usize) < n {
                    base_record[id as usize] = ((v as u64) << 48) | record;
                }
                let slot = &mut record_base[record as usize];
                if *slot == u32::MAX && !overlay.contains_key(&record) {
                    *slot = id;
                } else {
                    // A second name: keep every location in the overlay.
                    let list = overlay.entry(record).or_default();
                    if *slot != u32::MAX {
                        list.push(Loc::Base(*slot));
                        *slot = u32::MAX;
                    }
                    list.push(Loc::Base(id));
                }
            }
            volumes.push(Volume {
                record_base,
                overlay,
                excluded: Arc::new(map.excluded.iter().copied().collect()),
                roots: map.roots.iter().map(|&(r, _)| r).collect(),
            });
        }
        Live {
            columns: base.columns_present(),
            removed: vec![0; n.div_ceil(64)],
            removed_count: 0,
            delta: Vec::new(),
            delta_alive: 0,
            delta_children: HashMap::new(),
            overrides: HashMap::new(),
            volumes,
            base_record,
            base_children: OnceLock::new(),
            walks: Vec::new(),
            next_walk: 0,
            log: HashMap::new(),
            log_seq: 0,
            base,
        }
    }

    pub fn base(&self) -> &Arc<Index> {
        &self.base
    }

    fn is_alive(&self, loc: Loc) -> bool {
        match loc {
            Loc::Base(id) => self.removed[id as usize / 64] >> (id % 64) & 1 == 0,
            Loc::Delta(d) => self.delta[d as usize].alive,
        }
    }

    fn locations(&self, volume: usize, record: u64) -> Vec<Loc> {
        let v = &self.volumes[volume];
        let all = match v.overlay.get(&record) {
            Some(list) => list.clone(),
            None => match v.record_base.get(record as usize) {
                Some(&id) if id != u32::MAX => vec![Loc::Base(id)],
                _ => Vec::new(),
            },
        };
        all.into_iter().filter(|&l| self.is_alive(l)).collect()
    }

    fn set_locations(&mut self, volume: usize, record: u64, locs: Vec<Loc>) {
        let v = &mut self.volumes[volume];
        if let Some(slot) = v.record_base.get_mut(record as usize) {
            *slot = u32::MAX;
        }
        v.overlay.insert(record, locs);
    }

    fn flags_of(&self, loc: Loc) -> u16 {
        match loc {
            Loc::Base(id) => self.base.flags(id),
            Loc::Delta(d) => self.delta[d as usize].flags,
        }
    }

    fn name_of(&self, loc: Loc) -> &[u8] {
        match loc {
            Loc::Base(id) => self.base.name(id),
            Loc::Delta(d) => &self.delta[d as usize].name,
        }
    }

    fn parent_of(&self, loc: Loc) -> Option<Loc> {
        match loc {
            Loc::Base(id) => {
                let p = self.base.parent(id);
                (p != ROOT_PARENT).then_some(Loc::Base(p))
            }
            Loc::Delta(d) => Some(self.delta[d as usize].parent),
        }
    }

    fn metadata_of(&self, loc: Loc) -> EntryMetadata {
        match loc {
            Loc::Base(id) => self
                .overrides
                .get(&id)
                .copied()
                .unwrap_or_else(|| self.base.metadata(id)),
            Loc::Delta(d) => self.delta[d as usize].metadata,
        }
    }

    fn record_of(&self, loc: Loc) -> Option<(usize, u64)> {
        match loc {
            Loc::Base(id) => {
                let packed = *self.base_record.get(id as usize)?;
                (packed != u64::MAX)
                    .then_some(((packed >> 48) as usize, packed & 0x0000_FFFF_FFFF_FFFF))
            }
            Loc::Delta(d) => {
                let e = &self.delta[d as usize];
                Some((e.volume, e.record))
            }
        }
    }

    fn remove(&mut self, loc: Loc) {
        match loc {
            Loc::Base(id) => {
                let word = &mut self.removed[id as usize / 64];
                if *word >> (id % 64) & 1 == 0 {
                    *word |= 1 << (id % 64);
                    self.removed_count += 1;
                }
                self.overrides.remove(&id);
            }
            Loc::Delta(d) => {
                let e = &mut self.delta[d as usize];
                if e.alive {
                    e.alive = false;
                    self.delta_alive -= 1;
                }
            }
        }
    }

    fn add(&mut self, entry: DeltaEntry) -> Loc {
        let d = self.delta.len() as u32;
        self.delta_children.entry(entry.parent).or_default().push(d);
        self.delta.push(entry);
        self.delta_alive += 1;
        Loc::Delta(d)
    }

    /// Delta entries ever added, removed ones included (they stay until
    /// the next rescan): what the delta costs in memory and per view.
    pub fn delta_len(&self) -> usize {
        self.delta.len()
    }

    fn children(&self, loc: Loc) -> Vec<Loc> {
        let mut out = Vec::new();
        if let Loc::Base(id) = loc {
            let (first, ids) = self.base_children.get_or_init(|| {
                let n = self.base.len();
                let mut first = vec![0u32; n + 1];
                for c in 0..n as u32 {
                    let p = self.base.parent(c);
                    if p != ROOT_PARENT {
                        first[p as usize + 1] += 1;
                    }
                }
                for i in 1..first.len() {
                    first[i] += first[i - 1];
                }
                let mut fill = first.clone();
                let mut ids = vec![0u32; first[n] as usize];
                for c in 0..n as u32 {
                    let p = self.base.parent(c);
                    if p != ROOT_PARENT {
                        ids[fill[p as usize] as usize] = c;
                        fill[p as usize] += 1;
                    }
                }
                (first, ids)
            });
            let range = first[id as usize] as usize..first[id as usize + 1] as usize;
            out.extend(ids[range].iter().map(|&c| Loc::Base(c)));
        }
        if let Some(ids) = self.delta_children.get(&loc) {
            out.extend(ids.iter().map(|&d| Loc::Delta(d)));
        }
        out.retain(|&l| self.is_alive(l));
        out
    }

    /// Move the subtree below `from` under `to` (a folder's new entry).
    /// Returns the number of entries copied, or `None` past [`MAX_MOVE`].
    fn move_children(&mut self, from: Loc, to: Loc, budget: &mut usize) -> Option<()> {
        let mut stack = vec![(from, to)];
        while let Some((old_dir, new_dir)) = stack.pop() {
            for child in self.children(old_dir) {
                if *budget == 0 {
                    return None;
                }
                *budget -= 1;
                let record = self.record_of(child);
                let entry = DeltaEntry {
                    parent: new_dir,
                    name: self.name_of(child).to_vec(),
                    flags: self.flags_of(child),
                    metadata: self.metadata_of(child),
                    volume: record.map_or(0, |r| r.0),
                    record: record.map_or(u64::MAX, |r| r.1),
                    alive: true,
                };
                let is_dir = entry.flags & flags::DIR != 0;
                self.remove(child);
                let new = self.add(entry);
                if let Some((v, r)) = record {
                    let locs = self
                        .locations_raw(v, r)
                        .into_iter()
                        .map(|l| if l == child { new } else { l })
                        .collect();
                    self.set_locations(v, r, locs);
                }
                if is_dir {
                    stack.push((child, new));
                }
            }
        }
        Some(())
    }

    fn locations_raw(&self, volume: usize, record: u64) -> Vec<Loc> {
        let v = &self.volumes[volume];
        match v.overlay.get(&record) {
            Some(list) => list.clone(),
            None => match v.record_base.get(record as usize) {
                Some(&id) if id != u32::MAX => vec![Loc::Base(id)],
                _ => Vec::new(),
            },
        }
    }

    fn remove_subtree(&mut self, loc: Loc) {
        let mut stack = vec![loc];
        while let Some(dir) = stack.pop() {
            for child in self.children(dir) {
                if self.flags_of(child) & flags::DIR != 0 {
                    stack.push(child);
                }
                self.remove(child);
            }
        }
    }

    /// Apply one batch of changed records.
    pub fn apply(&mut self, changes: &[Change], source: &impl RecordSource) -> Applied {
        self.remember_for_walks(changes);
        self.apply_changes(changes, source)
    }

    fn apply_changes(&mut self, changes: &[Change], source: &impl RecordSource) -> Applied {
        let mut applied = Applied::default();
        let mut pending: Vec<(Change, Option<RecordState>)> = changes
            .iter()
            .map(|c| (*c, source.fetch(c.volume, c.record)))
            .collect();
        // Parents first: a record whose parent is also new in this batch
        // waits until the parent has an entry.
        loop {
            let waiting: HashSet<(usize, u64)> =
                pending.iter().map(|(c, _)| (c.volume, c.record)).collect();
            let before = pending.len();
            let mut deferred = Vec::new();
            for (change, state) in std::mem::take(&mut pending) {
                let blocked = state.as_ref().is_some_and(|s| {
                    s.names.iter().any(|(parent, _)| {
                        *parent != change.record
                            && waiting.contains(&(change.volume, *parent))
                            && self.locations(change.volume, *parent).is_empty()
                    })
                });
                if blocked {
                    deferred.push((change, state));
                } else if let Err(reason) = self.apply_one(change, state, &mut applied) {
                    applied.rescan.get_or_insert(reason);
                }
            }
            pending = deferred;
            if pending.is_empty() || pending.len() == before {
                break;
            }
        }
        for (change, state) in pending {
            if let Err(reason) = self.apply_one(change, state, &mut applied) {
                applied.rescan.get_or_insert(reason);
            }
        }
        self.check_limits(&mut applied);
        applied
    }

    /// Ask for a rescan when the overlay has outgrown its limits.
    fn check_limits(&self, applied: &mut Applied) {
        if self.delta_alive > MAX_DELTA {
            applied
                .rescan
                .get_or_insert_with(|| format!("{} live changes", self.delta_alive));
        }
        if self.removed_count > self.base.len() / MAX_REMOVED_DIVISOR + 1000 {
            applied
                .rescan
                .get_or_insert_with(|| format!("{} replaced entries", self.removed_count));
        }
    }

    /// While folders are being read, remember which records change: a walk
    /// may have listed each of them before or after the change, so
    /// [`Live::finish_walk`] looks at them again, and a re-read of a record
    /// always shows its current state. One log serves every outstanding
    /// walk; each takes the part reported after it was queued.
    fn remember_for_walks(&mut self, changes: &[Change]) {
        if self.walks.is_empty() {
            return;
        }
        self.log_seq += 1;
        let at = self.log_seq;
        for c in changes {
            let touch = self.log.entry((c.volume, c.record)).or_insert(Touch {
                last: at,
                created: 0,
            });
            touch.last = at;
            if c.created {
                touch.created = at;
            }
        }
        self.trim_log();
    }

    /// Keep the log within [`MAX_TOUCHED`] records: the oldest walks lose
    /// their window first (they fall back to the rescan when they finish),
    /// younger ones keep theirs.
    fn trim_log(&mut self) {
        while self.log.len() > MAX_TOUCHED {
            let Some(oldest) = self
                .walks
                .iter()
                .filter(|w| !w.overflowed)
                .map(|w| w.since)
                .min()
            else {
                self.log.clear();
                return;
            };
            for w in self.walks.iter_mut().filter(|w| w.since == oldest) {
                w.overflowed = true;
            }
            match self
                .walks
                .iter()
                .filter(|w| !w.overflowed)
                .map(|w| w.since)
                .min()
            {
                Some(next) => self.log.retain(|_, t| t.last > next),
                None => self.log.clear(),
            }
        }
    }

    /// Forget what no outstanding walk needs any more.
    fn prune_log(&mut self) {
        match self.walks.iter().map(|w| w.since).min() {
            None => self.log.clear(),
            Some(oldest) => self.log.retain(|_, t| t.last > oldest),
        }
    }

    /// The records reported after position `since`, with whether they were
    /// reported as created after it.
    fn changes_since(&self, since: u64) -> Vec<Change> {
        let mut out: Vec<Change> = self
            .log
            .iter()
            .filter(|(_, t)| t.last > since)
            .map(|(&(volume, record), t)| Change {
                volume,
                record,
                created: t.created > since,
            })
            .collect();
        out.sort_by_key(|c| (c.volume, c.record));
        out
    }

    /// The walk of a folder that `record`'s parent chain runs through: it
    /// lists the record, or (if it listed the parent already) the record is
    /// looked at again and found missing when the walk is merged.
    fn walk_above(&self, volume: usize, record: u64) -> Option<usize> {
        self.locations(volume, record).into_iter().find_map(|loc| {
            let mut at = self.parent_of(loc);
            for _ in 0..4096 {
                let up = at?;
                if let Some((v, r)) = self.record_of(up)
                    && let Some(i) = self
                        .walks
                        .iter()
                        .position(|w| w.volume == v && w.record == r)
                {
                    return Some(i);
                }
                at = self.parent_of(up);
            }
            None
        })
    }

    /// A folder with no entries below it has moved into the index: read it
    /// in the background unless a walk already covers it.
    fn walk_moved_in(
        &mut self,
        volume: usize,
        record: u64,
        sequence: u16,
        applied: &mut Applied,
    ) -> Result<(), String> {
        if self
            .walks
            .iter()
            .any(|w| w.volume == volume && w.record == record)
        {
            return Ok(());
        }
        if let Some(above) = self.walk_above(volume, record) {
            self.walks[above].covered.push((record, sequence));
            return Ok(());
        }
        let budget = MAX_DELTA.saturating_sub(self.delta_alive);
        if self.walks.len() >= MAX_WALKS || budget < MIN_WALK_BUDGET {
            return Err(format!(
                "{REASON_MOVED_IN} (too many changes to read its contents live)"
            ));
        }
        let id = self.next_walk;
        self.next_walk += 1;
        self.walks.push(Walk {
            id,
            volume,
            record,
            sequence,
            since: self.log_seq,
            started: None,
            overflowed: false,
            covered: Vec::new(),
        });
        applied.walks.push(WalkJob {
            id,
            volume,
            record,
            sequence,
            limits: Limits {
                max_entries: budget,
                max_time: MAX_WALK_TIME,
            },
            excluded: self.volumes[volume].excluded.clone(),
        });
        Ok(())
    }

    /// The reader began walk `id` at `now`.
    pub fn walk_started(&mut self, id: u64, now: Instant) {
        if let Some(walk) = self.walks.iter_mut().find(|w| w.id == id) {
            walk.started = Some(now);
        }
    }

    /// When the maintainer must wake to check on a walk the reader started.
    pub fn next_walk_deadline(&self) -> Option<Instant> {
        self.walks
            .iter()
            .filter_map(|w| w.started)
            .map(|started| started + MAX_WALK_TIME + WALK_SLACK)
            .min()
    }

    /// Give up on walks that outlived their budget by more than the slack:
    /// the reader is stuck inside a file-system call. Every outstanding
    /// walk is dropped (the reader's queue goes with it), a rescan is asked
    /// for, and `abandoned` tells the caller to start a new reader.
    pub fn expire_walks(&mut self, now: Instant) -> Applied {
        let mut applied = Applied::default();
        if !self.next_walk_deadline().is_some_and(|at| at <= now) {
            return applied;
        }
        applied.abandoned = self.walks.len();
        applied.rescan = Some(format!(
            "{REASON_MOVED_IN} (reading its contents did not finish in time)"
        ));
        self.walks.clear();
        self.log.clear();
        applied
    }

    /// Merge the result of a [`WalkJob`].
    ///
    /// The walk read the folder at some time between its start and now, and
    /// the journal kept reporting changes meanwhile. Nothing is lost or
    /// doubled because:
    ///
    /// - walked entries are applied like journal records (an entry whose
    ///   record already sits at the same place and name changes nothing),
    ///   parents first, so a record the journal already placed is not added
    ///   twice. A record that has entries already (a hard link also named
    ///   outside the walked folder, or one the journal placed) is applied
    ///   with its complete, current set of names, not just the walked ones;
    ///   a folder listed twice (it moved between two listings) keeps its
    ///   latest name;
    /// - every record the journal reported while the walk ran, and that the
    ///   walk listed or that has no entry, is read again afterwards and
    ///   brought to its current state. A change the walk missed (made after
    ///   it listed the folder, and dropped meanwhile because its parent had
    ///   no entry yet) is applied then; a change the walk saw only in part
    ///   is overwritten by the record's current state;
    /// - a folder moved in again inside the walked one starts a walk of its
    ///   own if the first walk did not list it.
    ///
    /// A failed walk (too large, too slow, an error, a folder that exists
    /// but could not be opened) asks for a rescan; a folder that really is
    /// gone or a result of an older index is not an error.
    pub fn finish_walk(
        &mut self,
        id: u64,
        result: Result<Subtree, WalkError>,
        source: &impl RecordSource,
    ) -> Applied {
        let Some(at) = self.walks.iter().position(|w| w.id == id) else {
            return Applied::default();
        };
        let walk = self.walks.remove(at);
        let applied = self.merge_walk(&walk, result, source);
        self.prune_log();
        applied
    }

    fn merge_walk(
        &mut self,
        walk: &Walk,
        result: Result<Subtree, WalkError>,
        source: &impl RecordSource,
    ) -> Applied {
        let mut applied = Applied::default();
        let volume = walk.volume;
        let exists = |record: u64, sequence: u16| {
            source
                .fetch(volume, record)
                .is_some_and(|s| s.info.is_dir && (sequence == 0 || s.info.sequence == sequence))
        };
        let listing = match result {
            Ok(listing) => listing,
            Err(WalkError::Cancelled) => return applied,
            // Deleted meanwhile: the journal says so, and the records
            // reported since are re-read below. Believed only if the record
            // really is not a folder any more.
            Err(WalkError::Gone) if !exists(walk.record, walk.sequence) => Subtree::default(),
            Err(WalkError::Gone) => {
                applied.rescan = Some(format!(
                    "{REASON_MOVED_IN} (the folder exists but could not be opened)"
                ));
                return applied;
            }
            Err(error) => {
                applied.rescan = Some(format!("{REASON_MOVED_IN} ({error})"));
                return applied;
            }
        };
        if walk.overflowed {
            applied.rescan = Some(format!(
                "{REASON_MOVED_IN} (too many changes while its contents were read)"
            ));
            return applied;
        }
        // A subfolder that could not be opened is deleted or moved away (the
        // journal says so); one that still exists means an incomplete walk.
        if listing
            .skipped
            .iter()
            .any(|&r| exists(r & RECORD_MASK, (r >> 48) as u16))
        {
            applied.rescan = Some(format!(
                "{REASON_MOVED_IN} (a subfolder exists but could not be opened)"
            ));
            return applied;
        }
        // One state per record, applied once every folder it lists is
        // placed: at the position of its last name.
        /// Position of its last name, its info and every `(parent, name)`.
        type Listed = (usize, RecordInfo, Vec<(u64, Vec<u8>)>);
        let mut states: HashMap<u64, Listed> = HashMap::new();
        for (i, e) in listing.entries.into_iter().enumerate() {
            let state = states.entry(e.record).or_insert((i, e.info, Vec::new()));
            state.0 = i;
            state.1 = e.info;
            let name = (e.parent, e.name);
            if e.info.is_dir {
                // A folder has one name: the later listing is the newer.
                state.2.clear();
                state.2.push(name);
            } else if !state.2.contains(&name) {
                state.2.push(name);
            }
        }
        let mut order: Vec<(usize, u64)> = states.iter().map(|(r, s)| (s.0, *r)).collect();
        order.sort_unstable();
        let listed: HashSet<u64> = order.iter().map(|&(_, r)| r).collect();
        for (_, record) in order {
            let Some((_, info, names)) = states.remove(&record) else {
                continue;
            };
            let state = if self.locations(volume, record).is_empty() {
                RecordState { info, names }
            } else {
                match source.fetch(volume, record) {
                    Some(current) => current,
                    None => continue, // deleted since: the journal removes it
                }
            };
            let change = Change {
                volume,
                record,
                created: true,
            };
            if let Err(reason) = self.apply_one(change, Some(state), &mut applied) {
                applied.rescan.get_or_insert(reason);
            }
        }
        applied.merged = std::mem::take(&mut applied.changed);
        let again: Vec<Change> = self
            .changes_since(walk.since)
            .into_iter()
            .filter(|c| {
                (c.volume == volume && (c.record == walk.record || listed.contains(&c.record)))
                    || self.locations(c.volume, c.record).is_empty()
            })
            .collect();
        // Not `apply`: these records were logged when they changed, and
        // logging them again would only fill other walks' windows.
        let rest = self.apply_changes(&again, source);
        applied.changed = rest.changed;
        applied.walks.extend(rest.walks);
        if let Some(reason) = rest.rescan {
            applied.rescan.get_or_insert(reason);
        }
        // Folders that moved in below the walked one after it listed it.
        for &(record, sequence) in &walk.covered {
            let is_folder = self
                .locations(volume, record)
                .iter()
                .any(|&l| self.flags_of(l) & flags::DIR != 0);
            if is_folder
                && !listed.contains(&record)
                && let Err(reason) = self.walk_moved_in(volume, record, sequence, &mut applied)
            {
                applied.rescan.get_or_insert(reason);
            }
        }
        self.check_limits(&mut applied);
        applied
    }

    fn apply_one(
        &mut self,
        change: Change,
        state: Option<RecordState>,
        applied: &mut Applied,
    ) -> Result<(), String> {
        let Change {
            volume,
            record,
            created,
        } = change;
        let old = self.locations(volume, record);
        if self.volumes[volume].roots.contains(&record) {
            // A root's entry stays put; a vanished root keeps its last state
            // until the next rescan.
            if let Some(s) = &state {
                let metadata = s.info.metadata(self.columns);
                for &loc in &old {
                    if self.metadata_of(loc) != metadata
                        && let Loc::Base(id) = loc
                    {
                        self.overrides.insert(id, metadata);
                        applied.changed += 1;
                    }
                }
            }
            return Ok(());
        }
        let excluded = self.volumes[volume].excluded.contains(&record);
        let mut desired: Vec<(Loc, Vec<u8>)> = Vec::new();
        let (new_flags, new_metadata, descended) = match &state {
            Some(s) if !excluded => {
                for (parent, name) in &s.names {
                    if *parent == record {
                        continue;
                    }
                    for p in self.locations(volume, *parent) {
                        if self.flags_of(p) & flags::DIR != 0 {
                            desired.push((p, name.clone()));
                        }
                    }
                }
                (
                    s.info.entry_flags(),
                    s.info.metadata(self.columns),
                    s.info.is_descended_dir(),
                )
            }
            _ => (0, EntryMetadata::default(), false),
        };
        let same_place = old.len() == desired.len()
            && old.iter().zip(&desired).all(|(&loc, (parent, name))| {
                self.parent_of(loc) == Some(*parent)
                    && self.name_of(loc) == name.as_slice()
                    && self.flags_of(loc) == new_flags
            });
        if same_place {
            if old.iter().all(|&l| self.metadata_of(l) == new_metadata) {
                return Ok(());
            }
            if descended {
                // A folder's own size or dates: keep its entry so its
                // children stay attached.
                for &loc in &old {
                    match loc {
                        Loc::Base(id) => {
                            self.overrides.insert(id, new_metadata);
                        }
                        Loc::Delta(d) => self.delta[d as usize].metadata = new_metadata,
                    }
                }
                applied.changed += 1;
                return Ok(());
            }
        }
        applied.changed += 1;
        for &loc in &old {
            self.remove(loc);
        }
        let mut new_locs = Vec::with_capacity(desired.len());
        for (parent, name) in desired {
            new_locs.push(self.add(DeltaEntry {
                parent,
                name,
                flags: new_flags,
                metadata: new_metadata,
                volume,
                record,
                alive: true,
            }));
        }
        let was_dir = old.iter().any(|&l| self.flags_of(l) & flags::DIR != 0);
        match (was_dir, new_locs.first().copied()) {
            (true, Some(to)) if descended => {
                let mut budget = MAX_MOVE;
                if self.move_children(old[0], to, &mut budget).is_none() {
                    self.set_locations(volume, record, new_locs);
                    return Err(REASON_LARGE_MOVE.into());
                }
            }
            (true, _) => self.remove_subtree(old[0]),
            (false, Some(_)) if descended && !created => {
                self.set_locations(volume, record, new_locs);
                let sequence = state.as_ref().map_or(0, |s| s.info.sequence);
                return self.walk_moved_in(volume, record, sequence, applied);
            }
            _ => {}
        }
        self.set_locations(volume, record, new_locs);
        Ok(())
    }

    /// Build the searchable view of the current state.
    pub fn view(&self) -> LiveView {
        let allowed: Vec<u64> = {
            let n = self.base.len();
            let mut words: Vec<u64> = self.removed.iter().map(|w| !w).collect();
            if !n.is_multiple_of(64)
                && let Some(last) = words.last_mut()
            {
                *last &= (1u64 << (n % 64)) - 1;
            }
            words
        };
        let builder = IndexBuilder::with_columns(self.columns);
        let mut shadows: HashMap<u32, u32> = HashMap::new();
        let mut shadow_ids = Vec::new();
        let mut ids: Vec<u32> = vec![u32::MAX; self.delta.len()];
        let mut path = Vec::new();
        for (d, e) in self.delta.iter().enumerate() {
            if !e.alive {
                continue;
            }
            let parent = match e.parent {
                Loc::Base(b) => {
                    if !self.is_alive(Loc::Base(b)) {
                        continue;
                    }
                    *shadows.entry(b).or_insert_with(|| {
                        self.base.path_bytes(b, &mut path);
                        let id = builder.add_root(&path, self.base.flags(b));
                        shadow_ids.push(id);
                        id
                    })
                }
                Loc::Delta(p) => match ids.get(p as usize) {
                    Some(&id) if id != u32::MAX => id,
                    _ => continue,
                },
            };
            ids[d] = builder.add_with_metadata(parent, &e.name, e.flags, e.metadata);
        }
        let delta = builder.freeze();
        let mut delta_allowed = vec![u64::MAX; delta.len().div_ceil(64)];
        for id in &shadow_ids {
            delta_allowed[*id as usize / 64] &= !(1 << (id % 64));
        }
        let shown_delta = delta.len() - shadow_ids.len();
        LiveView {
            entries: self.base.len() - self.removed_count + shown_delta,
            allowed: Arc::new(allowed),
            delta: Arc::new(delta),
            delta_allowed: Arc::new(delta_allowed),
            overrides: Arc::new(self.overrides.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrow_query::{CancelToken, Query, SearchOptions};
    use ferrow_walk::subtree::SubtreeEntry;

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

    fn file_info(size: u64) -> RecordInfo {
        RecordInfo {
            is_dir: false,
            attributes: 0x20,
            size,
            ..DIR_INFO
        }
    }

    /// root(5)/Docs(20)/a.txt(21), root/b.txt(22)
    fn fixture() -> (Arc<Index>, VolumeMap) {
        let b = IndexBuilder::with_columns(ColumnSet::DEFAULT);
        let root = b.add_root(b"C:", flags::DIR);
        let docs = b.add_with_metadata(
            root,
            b"Docs",
            flags::DIR,
            DIR_INFO.metadata(ColumnSet::DEFAULT),
        );
        let a = b.add_with_metadata(docs, b"a.txt", 0, file_info(1).metadata(ColumnSet::DEFAULT));
        let bb = b.add_with_metadata(root, b"b.txt", 0, file_info(2).metadata(ColumnSet::DEFAULT));
        let map = VolumeMap {
            entries: vec![(5, root), (20, docs), (21, a), (22, bb)],
            excluded: vec![30],
            roots: vec![(5, root)],
            ..VolumeMap::default()
        };
        (Arc::new(b.freeze()), map)
    }

    #[derive(Default)]
    struct Fake(HashMap<u64, RecordState>);
    impl Fake {
        fn set(&mut self, record: u64, info: RecordInfo, names: &[(u64, &str)]) {
            self.0.insert(
                record,
                RecordState {
                    info,
                    names: names
                        .iter()
                        .map(|(p, n)| (*p, n.as_bytes().to_vec()))
                        .collect(),
                },
            );
        }
    }
    impl RecordSource for Fake {
        fn fetch(&self, _volume: usize, record: u64) -> Option<RecordState> {
            self.0.get(&record).cloned()
        }
    }

    fn initial() -> Fake {
        let mut f = Fake::default();
        f.set(5, DIR_INFO, &[(5, ".")]);
        f.set(20, DIR_INFO, &[(5, "Docs")]);
        f.set(21, file_info(1), &[(20, "a.txt")]);
        f.set(22, file_info(2), &[(5, "b.txt")]);
        f
    }

    /// Every shown path, as a search over base and delta returns it.
    fn paths(base: &Index, view: &LiveView) -> Vec<String> {
        let q = Query::parse("").unwrap();
        let mut out = Vec::new();
        for (index, allowed) in [(base, &view.allowed), (&*view.delta, &view.delta_allowed)] {
            let r = ferrow_query::search(
                index,
                &q,
                &SearchOptions {
                    allowed: Some(allowed.clone()),
                    ..SearchOptions::default()
                },
            );
            out.extend(
                r.hits
                    .iter()
                    .map(|&id| index.path(id).to_string_lossy().replace('/', "\\")),
            );
            let (n, _) = ferrow_query::count_allowed(index, &q, &CancelToken::new(), Some(allowed));
            assert_eq!(n, r.hits.len());
        }
        out.sort();
        assert_eq!(out.len(), view.entries);
        out
    }

    fn change(record: u64, created: bool) -> Change {
        Change {
            volume: 0,
            record,
            created,
        }
    }

    #[test]
    fn rescans_wait_for_their_share_of_time() {
        let t0 = Instant::now();
        // A 3-minute scan (directory walk): the next waits 27 minutes.
        let mut p = RescanPolicy::after_scan(t0, Duration::from_secs(180));
        assert_eq!(p.wake_at(), None, "nothing pending arms no wake-up");
        assert_eq!(p.due(t0 + Duration::from_secs(3600), false), None);
        p.request("a folder moved into an indexed location".into());
        p.request("50001 live changes".into());
        assert_eq!(p.wake_at(), Some(t0 + Duration::from_secs(27 * 60)));
        assert_eq!(p.due(t0 + Duration::from_secs(26 * 60), false), None);
        assert_eq!(
            p.due(t0 + Duration::from_secs(27 * 60), false).as_deref(),
            Some("a folder moved into an indexed location")
        );
        assert_eq!(p.pending(), None);
        // Fast MFT scans still wait a minute; slow ones at most 30.
        let p = RescanPolicy::after_scan(t0, Duration::from_millis(500));
        assert_eq!(p.earliest, t0 + MIN_RESCAN_GAP);
        let p = RescanPolicy::after_scan(t0, Duration::from_secs(3600));
        assert_eq!(p.earliest, t0 + MAX_RESCAN_GAP);
        // An outgrown delta does not wait.
        let mut p = RescanPolicy::after_scan(t0, Duration::from_secs(10));
        p.request("x".into());
        assert_eq!(p.due(t0, true).as_deref(), Some("x"));
    }

    #[test]
    fn a_pending_rescan_is_news_once_per_episode_not_once_per_batch() {
        let t0 = Instant::now();
        let mut p = RescanPolicy::after_scan(t0, Duration::from_secs(10));
        // Past the threshold every batch asks again with a changing delta size.
        let fresh = (50_001..50_100)
            .filter(|n| p.request(format!("{n} live changes")))
            .count();
        assert_eq!(fresh, 1, "only the first request starts the episode");
        assert_eq!(p.pending(), Some("50001 live changes"), "first reason kept");
        // Once the rescan is taken, the next request starts a new episode.
        assert!(p.due(t0 + MAX_RESCAN_GAP, false).is_some());
        assert!(p.request("50200 live changes".into()));
        assert!(!p.request("50201 live changes".into()));
    }

    #[test]
    fn a_requested_rescan_skips_the_rate_limit_once() {
        let t0 = Instant::now();
        let mut p = RescanPolicy::after_scan(t0, Duration::from_secs(180));
        p.request("50001 live changes".into());
        assert_eq!(
            p.due(t0 + Duration::from_secs(60), false),
            None,
            "rate limited"
        );
        p.force(
            t0 + Duration::from_secs(60),
            "requested by an administrator".into(),
        );
        assert_eq!(
            p.due(t0 + Duration::from_secs(60), false).as_deref(),
            Some("requested by an administrator"),
            "the request replaces the waiting reason and starts now"
        );
        // The next scan is rate limited again.
        let p = RescanPolicy::after_scan(t0 + Duration::from_secs(61), Duration::from_secs(180));
        assert_eq!(p.earliest(), t0 + Duration::from_secs(61 + 27 * 60));
    }

    #[test]
    fn deleting_a_folder_visits_only_its_own_children() {
        // Many unrelated delta entries must not make a subtree delete
        // scan the whole delta per folder (it was quadratic).
        let (base, map) = fixture();
        let mut live = Live::new(base.clone(), &[map]);
        let mut f = initial();
        let mut changes = Vec::new();
        for i in 0..2000u64 {
            let record = 1000 + i;
            f.set(record, file_info(1), &[(20, &format!("n{i}.txt"))]);
            changes.push(change(record, true));
        }
        live.apply(&changes, &f);
        assert_eq!(live.delta_len(), 2000);
        let docs = live.locations(0, 20)[0];
        assert_eq!(live.children(docs).len(), 2001, "a.txt and the new files");
        assert!(
            live.children(Loc::Delta(0)).is_empty(),
            "files have no children"
        );
        // Deleting the folder hides it and everything below it.
        f.0.remove(&20);
        live.apply(&[change(20, false)], &f);
        assert_eq!(paths(&base, &live.view()), ["C:", "C:\\b.txt"]);
    }

    #[test]
    fn unchanged_records_change_nothing() {
        let (base, map) = fixture();
        let mut live = Live::new(base.clone(), &[map]);
        let f = initial();
        let r = live.apply(&[change(21, false), change(20, false)], &f);
        assert_eq!(r, Applied::default());
        assert_eq!(
            paths(&base, &live.view()),
            ["C:", "C:\\Docs", "C:\\Docs\\a.txt", "C:\\b.txt"]
        );
    }

    #[test]
    fn create_modify_delete_and_rename_files() {
        let (base, map) = fixture();
        let mut live = Live::new(base.clone(), &[map]);
        let mut f = initial();
        f.set(40, file_info(9), &[(20, "new.txt")]);
        f.set(21, file_info(100), &[(20, "a.txt")]);
        f.0.remove(&22);
        let r = live.apply(
            &[change(40, true), change(21, false), change(22, false)],
            &f,
        );
        assert_eq!(r.changed, 3);
        assert!(r.rescan.is_none());
        let view = live.view();
        assert_eq!(
            paths(&base, &view),
            ["C:", "C:\\Docs", "C:\\Docs\\a.txt", "C:\\Docs\\new.txt"]
        );
        let q = Query::parse("size:100").unwrap();
        let hit = ferrow_query::search(
            &view.delta,
            &q,
            &SearchOptions {
                allowed: Some(view.delta_allowed.clone()),
                ..SearchOptions::default()
            },
        );
        assert_eq!(hit.hits.len(), 1, "fresh metadata is searchable");

        f.set(40, file_info(9), &[(5, "moved.txt")]);
        live.apply(&[change(40, false)], &f);
        assert_eq!(
            paths(&base, &live.view()),
            ["C:", "C:\\Docs", "C:\\Docs\\a.txt", "C:\\moved.txt"]
        );
    }

    #[test]
    fn folder_metadata_change_keeps_children_in_place() {
        let (base, map) = fixture();
        let mut live = Live::new(base.clone(), &[map]);
        let mut f = initial();
        f.set(
            20,
            RecordInfo {
                modified: 133_000_000_000_000_000,
                ..DIR_INFO
            },
            &[(5, "Docs")],
        );
        let r = live.apply(&[change(20, false)], &f);
        assert_eq!(r.changed, 1);
        let view = live.view();
        assert_eq!(view.overrides.len(), 1);
        assert_eq!(view.delta.len(), 0, "no copies for a folder's own dates");
        assert_eq!(
            paths(&base, &view),
            ["C:", "C:\\Docs", "C:\\Docs\\a.txt", "C:\\b.txt"]
        );
    }

    #[test]
    fn folder_rename_moves_its_subtree_and_later_changes_follow() {
        let (base, map) = fixture();
        let mut live = Live::new(base.clone(), &[map]);
        let mut f = initial();
        f.set(20, DIR_INFO, &[(5, "Papers")]);
        live.apply(&[change(20, false)], &f);
        assert_eq!(
            paths(&base, &live.view()),
            ["C:", "C:\\Papers", "C:\\Papers\\a.txt", "C:\\b.txt"]
        );
        // The moved child is still tracked by its record.
        f.set(21, file_info(5), &[(20, "a2.txt")]);
        f.set(41, file_info(1), &[(20, "fresh.txt")]);
        live.apply(&[change(21, false), change(41, true)], &f);
        assert_eq!(
            paths(&base, &live.view()),
            [
                "C:",
                "C:\\Papers",
                "C:\\Papers\\a2.txt",
                "C:\\Papers\\fresh.txt",
                "C:\\b.txt"
            ]
        );
        // Deleting the folder hides everything below it.
        f.0.remove(&20);
        live.apply(&[change(20, false)], &f);
        assert_eq!(paths(&base, &live.view()), ["C:", "C:\\b.txt"]);
    }

    #[test]
    fn new_folder_and_child_in_one_batch_and_excluded_records() {
        let (base, map) = fixture();
        let mut live = Live::new(base.clone(), &[map]);
        let mut f = initial();
        f.set(51, file_info(1), &[(50, "inside.txt")]);
        f.set(50, DIR_INFO, &[(5, "NewDir")]);
        f.set(30, file_info(1), &[(5, "excluded.txt")]);
        // Child listed before its parent.
        let r = live.apply(&[change(51, true), change(50, true), change(30, true)], &f);
        assert!(r.rescan.is_none(), "{r:?}");
        assert_eq!(
            paths(&base, &live.view()),
            [
                "C:",
                "C:\\Docs",
                "C:\\Docs\\a.txt",
                "C:\\NewDir",
                "C:\\NewDir\\inside.txt",
                "C:\\b.txt"
            ]
        );
    }

    #[test]
    fn root_changes_only_touch_its_dates() {
        let (base, map) = fixture();
        let mut live = Live::new(base.clone(), &[map]);
        let mut f = initial();
        f.set(
            5,
            RecordInfo {
                modified: 133_000_000_000_000_000,
                ..DIR_INFO
            },
            &[(5, ".")],
        );
        let r = live.apply(&[change(5, false)], &f);
        assert_eq!(r.changed, 1);
        assert_eq!(
            paths(&base, &live.view()),
            ["C:", "C:\\Docs", "C:\\Docs\\a.txt", "C:\\b.txt"]
        );
        f.0.remove(&5);
        live.apply(&[change(5, false)], &f);
        assert_eq!(paths(&base, &live.view()).len(), 4);
    }

    /// Every path the file system holds below the root (record 5), listed
    /// the way a fresh scan would: what a correct live index must show.
    fn oracle(f: &Fake) -> Vec<String> {
        let mut out = vec!["C:".to_string()];
        let mut stack = vec![(5u64, "C:".to_string())];
        while let Some((dir, path)) = stack.pop() {
            for (record, state) in &f.0 {
                if *record == dir || *record == 30 {
                    continue; // the directory itself; the excluded record
                }
                for (parent, name) in &state.names {
                    if *parent == dir {
                        let child = format!("{path}\\{}", String::from_utf8_lossy(name));
                        out.push(child.clone());
                        if state.info.is_dir {
                            stack.push((*record, child));
                        }
                    }
                }
            }
        }
        out.sort();
        out
    }

    /// The background walker, mocked: lists the subtree as the file system
    /// is right now (breadth first, so parents come first).
    fn walk_fake(f: &Fake, job: &WalkJob) -> Vec<SubtreeEntry> {
        let mut out = Vec::new();
        let mut queue = std::collections::VecDeque::from([job.record]);
        while let Some(dir) = queue.pop_front() {
            let mut children: Vec<_> =
                f.0.iter()
                    .filter(|(r, _)| **r != dir && !job.excluded.contains(r))
                    .flat_map(|(r, s)| {
                        s.names
                            .iter()
                            .filter(|(p, _)| *p == dir)
                            .map(move |(_, n)| (*r, s.info, n.clone()))
                    })
                    .collect();
            children.sort_by_key(|(r, _, _)| *r);
            for (record, info, name) in children {
                if info.is_descended_dir() {
                    queue.push_back(record);
                }
                out.push(SubtreeEntry {
                    record,
                    parent: dir,
                    name,
                    info,
                });
            }
        }
        out
    }

    /// root/Moved(60){one.txt(61), sub(62){two.txt(63)}} arrived from outside.
    fn moved_in() -> Fake {
        let mut f = initial();
        f.set(60, DIR_INFO, &[(5, "Moved")]);
        f.set(61, file_info(1), &[(60, "one.txt")]);
        f.set(62, DIR_INFO, &[(60, "sub")]);
        f.set(63, file_info(2), &[(62, "two.txt")]);
        f
    }

    /// A live index in which folder 60 has just moved in: the live index,
    /// the base and the walk it asked for.
    fn after_move_in(f: &Fake) -> (Live, Arc<Index>, WalkJob) {
        let (base, map) = fixture();
        let mut live = Live::new(base.clone(), &[map]);
        let r = live.apply(&[change(60, false)], f);
        assert_eq!(r.rescan, None);
        assert_eq!(r.walks.len(), 1, "{r:?}");
        let job = r.walks[0].clone();
        (live, base, job)
    }

    fn shown(live: &Live, base: &Index) -> Vec<String> {
        paths(base, &live.view())
    }

    #[test]
    fn a_folder_moved_in_is_read_in_the_background_instead_of_rescanning() {
        let f = moved_in();
        let (mut live, base, job) = after_move_in(&f);
        assert_eq!((job.volume, job.record), (0, 60));
        assert_eq!(
            job.limits.max_entries,
            MAX_DELTA - 1,
            "the room left after the folder itself"
        );
        assert_eq!(job.limits.max_time, MAX_WALK_TIME);
        assert!(job.excluded.contains(&30), "the walk knows the excludes");
        // The folder shows up at once; its contents follow the walk.
        assert_eq!(
            shown(&live, &base),
            [
                "C:",
                "C:\\Docs",
                "C:\\Docs\\a.txt",
                "C:\\Moved",
                "C:\\b.txt"
            ]
        );
        let done = live.finish_walk(job.id, Ok(walk_fake(&f, &job).into()), &f);
        assert_eq!(done.rescan, None);
        assert!(done.walks.is_empty());
        assert_eq!(done.merged, 3);
        assert_eq!(shown(&live, &base), oracle(&f));
        assert!(
            shown(&live, &base).contains(&"C:\\Moved\\sub\\two.txt".to_string()),
            "searchable through the delta"
        );
        // The walked records are tracked like any other: later changes follow.
        let mut f = f;
        f.set(63, file_info(9), &[(62, "two-b.txt")]);
        live.apply(&[change(63, false)], &f);
        assert_eq!(shown(&live, &base), oracle(&f));
        // Nothing is waiting for a walk any more.
        assert!(live.walks.is_empty());
    }

    #[test]
    fn the_walk_budget_is_the_room_left_in_the_delta() {
        let (base, map) = fixture();
        let mut live = Live::new(base, &[map]);
        let mut f = moved_in();
        let mut changes = Vec::new();
        for i in 0..2000u64 {
            f.set(1000 + i, file_info(1), &[(20, &format!("n{i}.txt"))]);
            changes.push(change(1000 + i, true));
        }
        live.apply(&changes, &f);
        let r = live.apply(&[change(60, false)], &f);
        assert_eq!(r.walks[0].limits.max_entries, MAX_DELTA - 2000 - 1);
        // With the delta nearly full there is no room to read a folder into:
        // ask for the rescan right away (and do not start a walk).
        let (base, map) = fixture();
        let mut live = Live::new(base, &[map]);
        let mut changes = Vec::new();
        for i in 0..(MAX_DELTA - 500) as u64 {
            f.set(5000 + i, file_info(1), &[(20, &format!("m{i}.txt"))]);
            changes.push(change(5000 + i, true));
        }
        let r = live.apply(&changes, &f);
        assert_eq!(r.rescan, None, "still within the limit");
        let r = live.apply(&[change(60, false)], &f);
        assert!(r.walks.is_empty());
        let why = r.rescan.expect("no room to walk a folder");
        assert_eq!(reason_kind(&why), "moved-in folder", "{why}");
    }

    #[test]
    fn changes_during_the_walk_are_neither_lost_nor_doubled() {
        let mut f = moved_in();
        let (mut live, base, job) = after_move_in(&f);
        // The walker lists the folder now…
        let listed = walk_fake(&f, &job);
        // …and the file system changes before its result arrives:
        // a file created directly in the folder (the folder has an entry,
        // so it is applied at once), …
        f.set(64, file_info(1), &[(60, "created-direct.txt")]);
        // …a rename of a listed file, …
        f.set(61, file_info(1), &[(60, "renamed.txt")]);
        let r = live.apply(&[change(64, true), change(61, false)], &f);
        assert_eq!(r.rescan, None);
        // …a file created and one deleted below a folder not read yet (its
        // parent has no entry, so these are dropped for now), …
        f.set(65, file_info(1), &[(62, "created-deep.txt")]);
        f.0.remove(&63);
        live.apply(&[change(65, true), change(63, false)], &f);
        // …a new folder with a file below the unread folder, …
        f.set(66, DIR_INFO, &[(62, "newdir")]);
        f.set(67, file_info(1), &[(66, "in-newdir.txt")]);
        live.apply(&[change(66, true), change(67, true)], &f);
        // …and the unread folder itself renamed.
        f.set(62, DIR_INFO, &[(60, "sub2")]);
        let r = live.apply(&[change(62, false)], &f);
        assert!(r.walks.is_empty(), "inside the folder being read: {r:?}");
        let done = live.finish_walk(job.id, Ok(listed.into()), &f);
        assert_eq!(done.rescan, None);
        assert!(done.walks.is_empty(), "{done:?}");
        assert_eq!(shown(&live, &base), oracle(&f));
        // Exactly one entry per name: the delta holds Moved, one.txt→renamed,
        // created-direct, sub2 and what is below it, no stale copies alive.
        assert_eq!(
            live.delta_alive, 7,
            "Moved, renamed, created-direct, sub2, created-deep, newdir, in-newdir"
        );
    }

    #[test]
    fn a_change_the_journal_placed_before_the_listing_is_not_added_twice() {
        let mut f = moved_in();
        let (mut live, base, job) = after_move_in(&f);
        f.set(64, file_info(1), &[(60, "early.txt")]);
        live.apply(&[change(64, true)], &f);
        let listed = walk_fake(&f, &job); // lists the file the journal placed
        assert!(listed.iter().any(|e| e.record == 64));
        live.finish_walk(job.id, Ok(listed.into()), &f);
        assert_eq!(shown(&live, &base), oracle(&f));
        assert_eq!(
            live.delta_alive, 5,
            "Moved, one.txt, early.txt, sub, two.txt"
        );
    }

    #[test]
    fn the_folder_deleted_or_renamed_during_its_walk() {
        // Deleted: nothing of it shows, no rescan.
        let mut f = moved_in();
        let (mut live, base, job) = after_move_in(&f);
        let listed = walk_fake(&f, &job);
        for r in [60, 61, 62, 63] {
            f.0.remove(&r);
        }
        live.apply(&[change(60, false)], &f);
        let done = live.finish_walk(job.id, Ok(listed.into()), &f);
        assert_eq!(done.rescan, None);
        assert_eq!(shown(&live, &base), oracle(&f));
        assert_eq!(live.delta_alive, 0);
        // The walker found it gone (deleted before it could open it).
        let mut f = moved_in();
        let (mut live, base, job) = after_move_in(&f);
        for r in [60, 61, 62, 63] {
            f.0.remove(&r);
        }
        live.apply(&[change(60, false)], &f);
        let done = live.finish_walk(job.id, Err(WalkError::Gone), &f);
        assert_eq!(done.rescan, None);
        assert_eq!(shown(&live, &base), oracle(&f));
        // Renamed: the contents arrive under the new name.
        let mut f = moved_in();
        let (mut live, base, job) = after_move_in(&f);
        let listed = walk_fake(&f, &job);
        f.set(60, DIR_INFO, &[(5, "Renamed")]);
        live.apply(&[change(60, false)], &f);
        let done = live.finish_walk(job.id, Ok(listed.into()), &f);
        assert_eq!(done.rescan, None);
        assert_eq!(shown(&live, &base), oracle(&f));
        assert!(shown(&live, &base).contains(&"C:\\Renamed\\sub\\two.txt".to_string()));
    }

    #[test]
    fn a_walk_over_budget_or_failed_asks_for_the_rescan() {
        for error in [
            WalkError::OverBudget,
            WalkError::TimedOut,
            WalkError::Io("access denied".into()),
        ] {
            let f = moved_in();
            let (mut live, base, job) = after_move_in(&f);
            let done = live.finish_walk(job.id, Err(error.clone()), &f);
            let why = done.rescan.expect("falls back to the rescan");
            assert_eq!(reason_kind(&why), "moved-in folder", "{why}");
            assert!(why.contains(&error.to_string()), "{why}");
            // The folder keeps its entry; the rescan fills it in.
            assert!(shown(&live, &base).contains(&"C:\\Moved".to_string()));
            assert!(live.walks.is_empty());
        }
        // A cancelled walk (a rescan started) is not a failure.
        let f = moved_in();
        let (mut live, _, job) = after_move_in(&f);
        let done = live.finish_walk(job.id, Err(WalkError::Cancelled), &f);
        assert_eq!(done, Applied::default());
        // A result nobody waits for (an older index) is ignored.
        let done = live.finish_walk(job.id, Ok(Subtree::default()), &f);
        assert_eq!(done, Applied::default());
    }

    #[test]
    fn too_many_changes_during_a_walk_fall_back_to_the_rescan() {
        let f = moved_in();
        let (mut live, _, job) = after_move_in(&f);
        let flood: Vec<Change> = (0..MAX_TOUCHED as u64 + 1)
            .map(|r| change(10_000 + r, false))
            .collect();
        live.apply(&flood, &f);
        let done = live.finish_walk(job.id, Ok(walk_fake(&f, &job).into()), &f);
        let why = done.rescan.expect("too many changes to look at again");
        assert_eq!(reason_kind(&why), "moved-in folder", "{why}");
    }

    #[test]
    fn nested_moved_in_folders_are_covered_by_one_walk() {
        // Both arrive in one batch: the inner one is inside the folder
        // already being read.
        let mut f = moved_in();
        f.set(70, DIR_INFO, &[(60, "inner")]);
        f.set(71, file_info(1), &[(70, "deep.txt")]);
        let (base, map) = fixture();
        let mut live = Live::new(base.clone(), &[map]);
        let r = live.apply(&[change(60, false), change(70, false)], &f);
        assert_eq!(r.rescan, None);
        assert_eq!(r.walks.len(), 1, "one walk covers both: {r:?}");
        let job = r.walks[0].clone();
        let done = live.finish_walk(job.id, Ok(walk_fake(&f, &job).into()), &f);
        assert!(done.walks.is_empty());
        assert_eq!(shown(&live, &base), oracle(&f));

        // The inner folder moves in after the walker listed the outer one:
        // no second walk while the first runs, but the re-read after its
        // merge starts the inner folder's own walk.
        let mut f = moved_in();
        let (mut live, base, job) = after_move_in(&f);
        let listed = walk_fake(&f, &job);
        f.set(70, DIR_INFO, &[(60, "inner")]);
        f.set(71, file_info(1), &[(70, "deep.txt")]);
        let r = live.apply(&[change(70, false)], &f);
        assert!(r.walks.is_empty(), "covered while the outer walk runs");
        let done = live.finish_walk(job.id, Ok(listed.into()), &f);
        assert_eq!(done.rescan, None);
        assert_eq!(done.walks.len(), 1, "the outer walk did not list it");
        assert_eq!(done.walks[0].record, 70);
        let job = done.walks[0].clone();
        let done = live.finish_walk(job.id, Ok(walk_fake(&f, &job).into()), &f);
        assert!(done.walks.is_empty());
        assert_eq!(shown(&live, &base), oracle(&f));

        // The walker listed it (it moved in before the listing): no second walk.
        let mut f = moved_in();
        let (mut live, base, job) = after_move_in(&f);
        f.set(70, DIR_INFO, &[(60, "inner")]);
        f.set(71, file_info(1), &[(70, "deep.txt")]);
        live.apply(&[change(70, false)], &f);
        let done = live.finish_walk(job.id, Ok(walk_fake(&f, &job).into()), &f);
        assert!(done.walks.is_empty(), "{done:?}");
        assert_eq!(shown(&live, &base), oracle(&f));
        // The same folder asked for twice is one walk.
        let mut f = moved_in();
        let (mut live, _, _) = after_move_in(&f);
        f.set(60, DIR_INFO, &[(5, "Moved2")]);
        let r = live.apply(&[change(60, false)], &f);
        assert!(r.walks.is_empty());
    }

    fn entry(record: u64, parent: u64, name: &str, info: RecordInfo) -> SubtreeEntry {
        SubtreeEntry {
            record,
            parent,
            name: name.as_bytes().to_vec(),
            info,
        }
    }

    /// A source that counts how often each record is read.
    struct Counting<'a>(&'a Fake, std::cell::RefCell<HashMap<u64, usize>>);
    impl RecordSource for Counting<'_> {
        fn fetch(&self, volume: usize, record: u64) -> Option<RecordState> {
            *self.1.borrow_mut().entry(record).or_default() += 1;
            self.0.fetch(volume, record)
        }
    }

    #[test]
    fn a_hard_link_named_outside_the_walked_folder_keeps_its_other_name() {
        // b.txt (record 22, in the base at C:\b.txt) also got a name in the
        // moved-in folder; the walk lists only that name.
        let mut f = moved_in();
        f.set(22, file_info(2), &[(5, "b.txt"), (60, "b-link.txt")]);
        let (mut live, base, job) = after_move_in(&f);
        let listed = walk_fake(&f, &job);
        assert!(listed.iter().any(|e| e.record == 22));
        live.finish_walk(job.id, Ok(listed.into()), &f);
        let shown = shown(&live, &base);
        assert!(shown.contains(&"C:\\b.txt".to_string()), "{shown:?}");
        assert_eq!(shown, oracle(&f));
    }

    #[test]
    fn a_folder_listed_twice_keeps_its_latest_name_and_its_children_once() {
        // Moved{A, B}: A moved into B after Moved was listed, so the walk
        // saw A under both parents (its children only once).
        let mut f = initial();
        f.set(60, DIR_INFO, &[(5, "Moved")]);
        f.set(72, DIR_INFO, &[(60, "B")]);
        f.set(70, DIR_INFO, &[(72, "A-moved")]);
        f.set(71, file_info(1), &[(70, "x.txt")]);
        let (mut live, base, job) = after_move_in(&f);
        let listed = vec![
            entry(70, 60, "A", DIR_INFO),
            entry(72, 60, "B", DIR_INFO),
            entry(70, 72, "A-moved", DIR_INFO),
            entry(71, 70, "x.txt", file_info(1)),
            entry(71, 70, "x.txt", file_info(1)), // the same name twice
        ];
        live.finish_walk(job.id, Ok(listed.into()), &f);
        assert_eq!(shown(&live, &base), oracle(&f));
    }

    #[test]
    fn a_walk_the_reader_never_finishes_is_given_up() {
        let f = moved_in();
        let (mut live, base, job) = after_move_in(&f);
        let mut f2 = f;
        f2.set(100, DIR_INFO, &[(5, "Other")]);
        let waiting = live.apply(&[change(100, false)], &f2).walks[0].clone();
        let t0 = Instant::now();
        assert_eq!(live.next_walk_deadline(), None, "none while they wait");
        live.walk_started(job.id, t0);
        assert_eq!(
            live.next_walk_deadline(),
            Some(t0 + MAX_WALK_TIME + WALK_SLACK)
        );
        assert_eq!(live.expire_walks(t0 + MAX_WALK_TIME), Applied::default());
        let r = live.expire_walks(t0 + MAX_WALK_TIME + WALK_SLACK);
        let why = r.rescan.expect("falls back to the rescan");
        assert_eq!(reason_kind(&why), "moved-in folder", "{why}");
        // The stuck reader's queue goes with it: both folders are given up.
        assert_eq!(r.abandoned, 2);
        assert!(live.walks.is_empty());
        assert_eq!(live.next_walk_deadline(), None);
        // A late answer of the stuck read is ignored.
        let late = live.finish_walk(job.id, Ok(walk_fake(&f2, &job).into()), &f2);
        assert_eq!(late, Applied::default());
        let late = live.finish_walk(waiting.id, Ok(Subtree::default()), &f2);
        assert_eq!(late, Applied::default());
        // Nothing waits on the given-up folder any more: it can be read again.
        assert!(shown(&live, &base).contains(&"C:\\Moved".to_string()));
        f2.set(60, DIR_INFO, &[(5, "Moved")]);
        let r = live.apply(&[change(61, false)], &f2);
        assert!(r.rescan.is_none() && r.walks.is_empty());
    }

    #[test]
    fn a_folder_that_could_not_be_opened_is_gone_only_if_it_is_gone() {
        // It still exists: the walk is incomplete, so rescan.
        let f = moved_in();
        let (mut live, _, job) = after_move_in(&f);
        let done = live.finish_walk(job.id, Err(WalkError::Gone), &f);
        let why = done.rescan.expect("it exists");
        assert_eq!(reason_kind(&why), "moved-in folder", "{why}");
        assert!(why.contains("exists"), "{why}");
        // Deleted, or its record reused by another folder (new sequence).
        for reused in [false, true] {
            let mut f = moved_in();
            let (mut live, base, job) = after_move_in(&f);
            for r in [60, 61, 62, 63] {
                f.0.remove(&r);
            }
            if reused {
                let info = RecordInfo {
                    sequence: 2,
                    ..DIR_INFO
                };
                f.set(60, info, &[(777, "elsewhere")]);
            }
            live.apply(&[change(60, false)], &f);
            let done = live.finish_walk(job.id, Err(WalkError::Gone), &f);
            assert_eq!(done.rescan, None, "reused: {reused}");
            assert_eq!(shown(&live, &base), oracle(&f));
        }
        // A subfolder the walk could not open: gone is fine, existing is not.
        let skipped = vec![(1u64 << 48) | 62];
        let f = moved_in();
        let (mut live, _, job) = after_move_in(&f);
        let listing = Subtree {
            entries: walk_fake(&f, &job),
            skipped: skipped.clone(),
        };
        let done = live.finish_walk(job.id, Ok(listing), &f);
        let why = done.rescan.expect("the subfolder exists");
        assert_eq!(reason_kind(&why), "moved-in folder", "{why}");
        let mut f = moved_in();
        let (mut live, base, job) = after_move_in(&f);
        f.0.remove(&62);
        f.0.remove(&63);
        let listing = Subtree {
            entries: walk_fake(&f, &job),
            skipped,
        };
        let done = live.finish_walk(job.id, Ok(listing), &f);
        assert_eq!(done.rescan, None);
        assert_eq!(shown(&live, &base), oracle(&f));
    }

    #[test]
    fn only_records_the_walk_listed_or_without_an_entry_are_read_again() {
        let mut f = moved_in();
        let (mut live, base, job) = after_move_in(&f);
        let listed = walk_fake(&f, &job);
        // a.txt (in the base, not in the walked folder) changes and is
        // applied at once; a record outside the index changes too.
        f.set(21, file_info(9), &[(20, "a.txt")]);
        f.set(99, file_info(1), &[(777, "outside.txt")]);
        let source = Counting(&f, Default::default());
        live.apply(&[change(21, false), change(99, false)], &source);
        live.finish_walk(job.id, Ok(listed.into()), &source);
        let reads = source.1.borrow();
        assert_eq!(reads[&21], 1, "read when it changed, not again");
        assert_eq!(
            reads[&99], 2,
            "no entry: read again in case it belongs inside"
        );
        drop(reads);
        assert_eq!(shown(&live, &base), oracle(&f));
    }

    #[test]
    fn merging_one_walk_does_not_fill_another_walks_window() {
        let mut f = moved_in();
        f.set(100, DIR_INFO, &[(5, "Other")]);
        f.set(101, file_info(1), &[(100, "o.txt")]);
        let (mut live, _, first) = after_move_in(&f);
        // Many changes outside the index while the first walk runs…
        let flood: Vec<Change> = (0..MAX_TOUCHED as u64 - 10)
            .map(|r| change(10_000 + r, false))
            .collect();
        live.apply(&flood, &f);
        let second = live.apply(&[change(100, false)], &f).walks[0].clone();
        // …are read again when it is merged, which must not count against
        // the second walk, which was queued after them.
        live.finish_walk(first.id, Ok(walk_fake(&f, &first).into()), &f);
        assert!(!live.walks[0].overflowed);
        assert!(live.changes_since(live.walks[0].since).is_empty());
        live.apply(&[change(20_000, false)], &f);
        let done = live.finish_walk(second.id, Ok(walk_fake(&f, &second).into()), &f);
        assert_eq!(done.rescan, None);
    }

    #[test]
    fn overlapping_walks_each_read_again_what_changed_after_they_were_queued() {
        let mut f = moved_in();
        f.set(100, DIR_INFO, &[(5, "Other")]);
        f.set(101, file_info(1), &[(100, "o.txt")]);
        let (mut live, base, first) = after_move_in(&f);
        let listed_first = walk_fake(&f, &first);
        f.0.remove(&61); // listed by the first walk
        live.apply(&[change(61, false)], &f);
        let second = live.apply(&[change(100, false)], &f).walks[0].clone();
        let listed_second = walk_fake(&f, &second);
        f.set(102, file_info(1), &[(100, "p.txt")]);
        f.0.remove(&63);
        live.apply(&[change(102, true), change(63, false)], &f);
        // The results arrive in the other order.
        live.finish_walk(second.id, Ok(listed_second.into()), &f);
        live.finish_walk(first.id, Ok(listed_first.into()), &f);
        assert_eq!(shown(&live, &base), oracle(&f));
        assert!(
            live.walks.is_empty() && live.log.is_empty(),
            "nothing is kept"
        );
    }

    #[test]
    fn a_burst_of_folders_is_queued_up_to_a_cap() {
        let (base, map) = fixture();
        let mut live = Live::new(base, &[map]);
        let mut f = initial();
        let mut changes = Vec::new();
        for i in 0..(MAX_WALKS as u64 + 1) {
            f.set(100 + i, DIR_INFO, &[(5, &format!("m{i}"))]);
            changes.push(change(100 + i, false));
        }
        let r = live.apply(&changes, &f);
        assert_eq!(r.walks.len(), MAX_WALKS);
        let why = r.rescan.expect("one folder too many");
        assert_eq!(reason_kind(&why), "moved-in folder", "{why}");
    }

    #[test]
    fn walked_hard_links_and_excluded_records() {
        let mut f = moved_in();
        // One file with a name in two walked folders, and an excluded record.
        f.set(80, file_info(3), &[(60, "link-a.txt"), (62, "link-b.txt")]);
        f.set(30, file_info(1), &[(60, "excluded.txt")]);
        let (mut live, base, job) = after_move_in(&f);
        assert!(!walk_fake(&f, &job).iter().any(|e| e.record == 30));
        // Even if a walker returned the excluded record, it is not shown.
        let mut listed = walk_fake(&f, &job);
        listed.push(SubtreeEntry {
            record: 30,
            parent: 60,
            name: b"excluded.txt".to_vec(),
            info: file_info(1),
        });
        live.finish_walk(job.id, Ok(listed.into()), &f);
        let shown = shown(&live, &base);
        assert!(
            shown.contains(&"C:\\Moved\\link-a.txt".to_string()),
            "{shown:?}"
        );
        assert!(
            shown.contains(&"C:\\Moved\\sub\\link-b.txt".to_string()),
            "{shown:?}"
        );
        assert!(!shown.iter().any(|p| p.contains("excluded")), "{shown:?}");
    }

    #[test]
    fn rescan_reasons_are_counted_by_kind() {
        assert_eq!(reason_kind(REASON_MOVED_IN), "moved-in folder");
        assert_eq!(
            reason_kind("a folder moved into an indexed location (the walk took longer)"),
            "moved-in folder"
        );
        assert_eq!(reason_kind(REASON_LARGE_MOVE), "large folder move");
        assert_eq!(reason_kind("50001 live changes"), "too many live changes");
        assert_eq!(
            reason_kind("12345 replaced entries"),
            "too many replaced entries"
        );
        assert_eq!(reason_kind(REASON_ADMIN), "administrator");
        assert_eq!(reason_kind("startup"), "startup");
        assert_eq!(
            reason_kind("\\\\?\\Volume{1}: change journal reset: x"),
            "journal"
        );
    }

    #[test]
    fn a_folder_moved_within_the_index_is_still_moved_without_a_walk() {
        // Not a moved-in folder: its old place is indexed, so it moves its
        // entries (the earlier behaviour) and reads nothing.
        let (base, map) = fixture();
        let mut live = Live::new(base.clone(), &[map]);
        let mut f = initial();
        f.set(20, DIR_INFO, &[(5, "Papers")]);
        let r = live.apply(&[change(20, false)], &f);
        assert!(r.walks.is_empty() && r.rescan.is_none());
        assert_eq!(shown(&live, &base), oracle(&f));
    }

    /// Prices `Live::view` (run by hand: `cargo test --release -p ferrowd
    /// --lib price_view -- --ignored --nocapture`).
    #[test]
    #[ignore = "measurement, run by hand"]
    fn price_view() {
        use std::time::Instant;
        // The allowed bit set of 27 M base entries: what every view copies.
        let words = 27_000_000usize.div_ceil(64);
        let removed = vec![0u64; words];
        let t = Instant::now();
        let mut sink = 0u64;
        for i in 0..200 {
            let mut copy: Vec<u64> = std::hint::black_box(&removed).iter().map(|w| !w).collect();
            copy[i] = 1;
            sink ^= std::hint::black_box(&copy)[words / 2] ^ copy[i];
        }
        eprintln!(
            "allowed bit set, 27 M entries ({} KB): {:.3} ms per view (sink {sink})",
            words * 8 / 1024,
            t.elapsed().as_secs_f64() * 1000.0 / 200.0
        );
        // The delta index rebuilt per view, for growing deltas.
        for n in [1_000u64, 5_000, 19_000, 50_000] {
            let (base, map) = fixture();
            let mut live = Live::new(base, &[map]);
            let mut f = initial();
            let mut changes = Vec::new();
            for i in 0..n {
                f.set(1000 + i, file_info(i), &[(20, &format!("file-{i:06}.txt"))]);
                changes.push(change(1000 + i, true));
            }
            live.apply(&changes, &f);
            let t = Instant::now();
            let views = 10;
            for _ in 0..views {
                let v = live.view();
                sink ^= v.entries as u64;
            }
            eprintln!(
                "delta of {n} entries: {:.2} ms per view",
                t.elapsed().as_secs_f64() * 1000.0 / views as f64
            );
        }
        assert_ne!(sink, 1);
    }

    #[test]
    fn hard_links_get_one_entry_per_name() {
        let (base, map) = fixture();
        let mut live = Live::new(base.clone(), &[map]);
        let mut f = initial();
        f.set(22, file_info(2), &[(5, "b.txt"), (20, "b-link.txt")]);
        live.apply(&[change(22, false)], &f);
        assert_eq!(
            paths(&base, &live.view()),
            [
                "C:",
                "C:\\Docs",
                "C:\\Docs\\a.txt",
                "C:\\Docs\\b-link.txt",
                "C:\\b.txt"
            ]
        );
        f.set(22, file_info(2), &[(20, "b-link.txt")]);
        live.apply(&[change(22, false)], &f);
        assert_eq!(
            paths(&base, &live.view()),
            ["C:", "C:\\Docs", "C:\\Docs\\a.txt", "C:\\Docs\\b-link.txt"]
        );
    }
}
