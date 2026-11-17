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
//! - a renamed or moved folder moves its whole subtree into the delta.
//!
//! When the delta grows large, the owner rescans the volume (seconds with
//! the MFT reader) and starts over from a fresh frozen index.
//!
//! The logic here is platform-independent; the journal readers and record
//! fetching live in `live_windows`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crate::memory::{Breakdown, Kind, Part, table_bytes, vec_bytes};
use ferrow_index::{ColumnSet, EntryMetadata, Index, IndexBuilder, ROOT_PARENT, flags};
use ferrow_walk::VolumeMap;
use ferrow_walk::ntfs::RecordInfo;

/// Rescan once the overlay holds this many live delta entries…
const MAX_DELTA: usize = 50_000;
/// …or once this share of frozen entries is hidden (1/10)…
const MAX_REMOVED_DIVISOR: usize = 10;
/// …or a single folder move would copy more than this many entries.
const MAX_MOVE: usize = 100_000;

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
    excluded: HashSet<u64>,
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

/// A frozen index's heap by part, named `<prefix>.<part>`; a cached sort
/// order is lazy (built when first sorted by), everything else steady.
pub fn index_parts(index: &Index, prefix: &str) -> Vec<Part> {
    index
        .heap_parts()
        .into_iter()
        .map(|(name, bytes)| {
            let kind = if name.ends_with(".order") {
                Kind::Lazy
            } else {
                Kind::Steady
            };
            Part::new(format!("{prefix}.{name}"), bytes, kind)
        })
        .collect()
}

impl LiveView {
    /// What the published view holds, by capacity (memory accounting).
    pub fn memory_parts(&self) -> Vec<Part> {
        let mut parts = vec![
            Part::new("view.allowed", vec_bytes(&self.allowed), Kind::Steady),
            Part::new(
                "view.delta_allowed",
                vec_bytes(&self.delta_allowed),
                Kind::Steady,
            ),
            Part::new(
                "view.overrides",
                table_bytes(self.overrides.capacity(), size_of::<(u32, EntryMetadata)>()),
                Kind::Steady,
            ),
        ];
        parts.extend(index_parts(&self.delta, "view.delta_index"));
        parts
    }

    /// Metadata of a frozen entry, with any live override applied.
    pub fn base_metadata(&self, base: &Index, id: u32) -> EntryMetadata {
        self.overrides
            .get(&id)
            .copied()
            .unwrap_or_else(|| base.metadata(id))
    }
}

/// Result of applying one batch.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// Records whose entries changed.
    pub changed: usize,
    /// The overlay is too large or a change cannot be applied
    /// incrementally (for example a folder moved in from outside the
    /// indexed roots): rescan the volumes.
    pub rescan: Option<String>,
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
                excluded: map.excluded.iter().copied().collect(),
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

    /// What the live layer holds beyond the frozen index, by capacity
    /// (memory accounting; allocator overhead per allocation is not here).
    pub fn memory_parts(&self) -> Vec<Part> {
        let mut record_base = 0;
        let mut overlay = 0;
        let mut sets = 0;
        for v in &self.volumes {
            record_base += vec_bytes(&v.record_base);
            overlay += table_bytes(v.overlay.capacity(), size_of::<(u64, Vec<Loc>)>())
                + v.overlay.values().map(vec_bytes).sum::<usize>();
            sets += table_bytes(v.excluded.capacity(), size_of::<u64>())
                + table_bytes(v.roots.capacity(), size_of::<u64>());
        }
        let delta_names: usize = self.delta.iter().map(|e| e.name.capacity()).sum();
        let delta_children =
            table_bytes(self.delta_children.capacity(), size_of::<(Loc, Vec<u32>)>())
                + self.delta_children.values().map(vec_bytes).sum::<usize>();
        let base_children = self
            .base_children
            .get()
            .map_or(0, |(first, ids)| vec_bytes(first) + vec_bytes(ids));
        vec![
            Part::new(
                "live.base_record",
                vec_bytes(&self.base_record),
                Kind::Steady,
            ),
            Part::new("live.record_base", record_base, Kind::Steady),
            Part::new("live.overlay", overlay, Kind::Steady),
            Part::new("live.excluded_roots", sets, Kind::Steady),
            Part::new("live.removed", vec_bytes(&self.removed), Kind::Steady),
            Part::new(
                "live.delta",
                vec_bytes(&self.delta) + delta_names,
                Kind::Steady,
            ),
            Part::new("live.delta_children", delta_children, Kind::Steady),
            Part::new(
                "live.overrides",
                table_bytes(self.overrides.capacity(), size_of::<(u32, EntryMetadata)>()),
                Kind::Steady,
            ),
            // Built by the first folder rename or delete, then kept.
            Part::new("live.base_children", base_children, Kind::Lazy),
        ]
    }

    /// The whole retained picture of a live session: the frozen index's parts
    /// (a metadata sort order is lazy: built on first use), this layer's, and
    /// the published view's. `baseline` is the process's private bytes before
    /// any index existed (see [`Breakdown::baseline`]).
    pub fn breakdown(&self, view: Option<&LiveView>, baseline: u64) -> Breakdown {
        let mut parts: Vec<Part> = index_parts(&self.base, "index");
        parts.extend(self.memory_parts());
        if let Some(view) = view {
            parts.extend(view.memory_parts());
        }
        Breakdown::new(self.base.len(), parts, baseline)
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
                    return Err("a large folder moved".into());
                }
            }
            (true, _) => self.remove_subtree(old[0]),
            (false, Some(_)) if descended && !created => {
                self.set_locations(volume, record, new_locs);
                return Err("a folder moved into an indexed location".into());
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

    fn current(base: &Fake) -> Fake {
        let mut f = Fake(base.0.clone());
        f.set(5, DIR_INFO, &[(5, ".")]);
        f
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

    #[test]
    fn folder_moved_in_from_outside_asks_for_a_rescan() {
        let (base, map) = fixture();
        let mut live = Live::new(base, &[map]);
        let mut f = current(&initial());
        f.set(60, DIR_INFO, &[(5, "FromElsewhere")]);
        let r = live.apply(&[change(60, false)], &f);
        assert!(r.rescan.is_some());
    }

    #[test]
    fn memory_accounting_counts_each_structure_by_capacity_and_adds_up_with_the_index() {
        let (base, map) = fixture();
        let mut live = Live::new(base.clone(), &[map]);
        let part = |parts: &[Part], name: &str| {
            parts
                .iter()
                .find(|p| p.name == name)
                .unwrap_or_else(|| panic!("{name} missing"))
                .bytes
        };
        let parts = live.memory_parts();
        assert_eq!(part(&parts, "live.base_record"), 4 * 8, "8 bytes per entry");
        assert_eq!(part(&parts, "live.removed"), 8, "one bitset word");
        assert_eq!(part(&parts, "live.base_children"), 0, "lazy: not built yet");
        assert!(
            part(&parts, "live.record_base") >= 21 * 4,
            "4 bytes per MFT record"
        );

        // Deleting a folder is the first thing that needs the frozen children
        // index: (entries + 1) offsets and one id per non-root entry.
        let mut f = initial();
        f.0.remove(&20);
        f.set(70, file_info(1), &[(5, "a-new-name.txt")]);
        live.apply(&[change(20, false), change(70, true)], &f);
        let parts = live.memory_parts();
        assert_eq!(part(&parts, "live.base_children"), (5 + 3) * 4);
        assert!(part(&parts, "live.delta") > size_of::<DeltaEntry>() as u64);
        assert!(part(&parts, "live.delta_children") > 0);

        // Every part is counted once: the breakdown is the frozen index's
        // heap, the live layer's parts and the view's parts, nothing else.
        let view = live.view();
        let all = live.breakdown(Some(&view), 0);
        let expected = base.heap_bytes()
            + live
                .memory_parts()
                .iter()
                .map(|p| p.bytes as usize)
                .sum::<usize>()
            + view
                .memory_parts()
                .iter()
                .map(|p| p.bytes as usize)
                .sum::<usize>();
        assert_eq!(all.accounted() as usize, expected);
        assert_eq!(all.entries, 4);
        assert!(part(&all.parts, "view.allowed") >= 8);
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
