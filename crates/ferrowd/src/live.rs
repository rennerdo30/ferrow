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
//! When the delta grows large, the owner folds it into a new frozen index in
//! memory ([`Live::compact`]: seconds of one background thread, no disk); a
//! journal that cannot be trusted or a folder that could not be read still
//! asks for a rescan of the volumes (see [`Maintenance`]).
//!
//! The logic here is platform-independent; the journal readers and record
//! fetching live in `live_windows`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crate::memory::{Breakdown, Kind, Part, table_bytes, vec_bytes};
use ferrow_index::{ColumnSet, EntryMetadata, Index, IndexBuilder, ROOT_PARENT, SortKey, flags};
use ferrow_walk::VolumeMap;
use ferrow_walk::ntfs::RecordInfo;
use ferrow_walk::subtree::{Limits, Subtree, WalkError};

/// Ask for a compaction once the overlay holds more live delta entries than
/// [`DeltaLimits::max_delta`] (about 1 % of the index, never less than this)…
pub const MIN_DELTA: usize = 50_000;
/// …and never more than this (a view of this many entries takes about
/// 100 ms to build and about 80 MB to hold).
pub const MAX_DELTA_CAP: usize = 250_000;
/// …or once this share of frozen entries is hidden (1/10)…
const MAX_REMOVED_DIVISOR: usize = 10;
/// A folder move copies frozen entries into the delta while its frozen
/// subtree has at most this many entries (folders and dead entries count);
/// a larger frozen subtree stays in place, hidden, and a compaction
/// re-attaches it. Delta entries below the folder are always copied (the
/// delta limits bound them). See [`Live::move_children`].
const MAX_MOVE: usize = 100_000;

/// The size limits of the live layer. They follow the index: a fixed limit
/// asked a PC with 27 million entries for a full rescan (over a minute of
/// heavy CPU and disk) as often as one with 1 million, though the overlay
/// costs the same per entry on both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeltaLimits {
    /// Rescan when more delta entries than this are alive; also the room a
    /// folder read in the background can fill.
    pub max_delta: usize,
    /// Rescan when more delta entries than this are dead (replaced or
    /// deleted again, kept until the next rescan). Twice `max_delta`: dead
    /// entries cost memory only (a view skips them), and git or a build
    /// that creates and deletes a temporary folder of tens of thousands of
    /// files must not cost a full rescan.
    pub max_dead: usize,
    /// A pending rescan starts at once when the delta, dead entries
    /// included, grows past this (bounds its memory and per-batch view cost:
    /// at most 750,000 entries, a few hundred MB in the worst case).
    pub hard_delta: usize,
    /// Records remembered as changed while walks run.
    pub max_touched: usize,
}

impl DeltaLimits {
    /// The limits for a frozen index of `base_len` entries.
    pub fn for_base(base_len: usize) -> DeltaLimits {
        let max_delta = max_delta(base_len);
        DeltaLimits {
            max_delta,
            max_dead: 2 * max_delta,
            hard_delta: MIN_HARD_DELTA.max(3 * max_delta),
            max_touched: max_delta,
        }
    }
}

/// Live delta entries a frozen index of `base_len` entries may carry before
/// a rescan is asked for: 1 %, between [`MIN_DELTA`] and [`MAX_DELTA_CAP`]
/// (1 million entries: 50,000; 18.6 million: 186,000; 27 million: 250,000).
pub fn max_delta(base_len: usize) -> usize {
    (base_len / 100).clamp(MIN_DELTA, MAX_DELTA_CAP)
}

/// The least [`DeltaLimits::hard_delta`] (8 times the smallest `max_delta`).
const MIN_HARD_DELTA: usize = 8 * MIN_DELTA;

/// A folder moved in from outside the index is read by a background walk of
/// at most this many entries (and never more than the delta has room for:
/// every walked entry becomes a delta entry, and past
/// [`DeltaLimits::max_delta`] a rescan is asked for anyway), …
const MIN_WALK_BUDGET: usize = 1_000;
/// …at most this long (a cold hard disk lists on the order of 10,000
/// entries per second, so 300,000 entries fit; a larger budget can run out
/// of time first, which falls back to a rescan; a rescan reads 27 million
/// entries in about 74 s on the owner's PC, which is cheaper per entry
/// beyond that), …
pub const MAX_WALK_TIME: Duration = Duration::from_secs(30);
/// A finished walk at least this long is an event in the service log.
pub const NOTABLE_WALK: Duration = Duration::from_secs(1);
/// What the log line at the start of a live session says is on.
pub fn walk_limits_text(base_len: usize) -> String {
    format!(
        "up to {} entries and {} s each, at most {MAX_WALKS} at once",
        max_delta(base_len),
        MAX_WALK_TIME.as_secs()
    )
}
/// …with this many folders waiting at once (a package manager moves dozens
/// of sibling folders in one burst; the reader takes them one by one), …
const MAX_WALKS: usize = 64;
/// (At most [`DeltaLimits::max_touched`] records are remembered as changed
/// while walks run; they are looked at again once a result is merged.)
/// A walk the reader has not finished this long after its time budget
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
pub const REASON_NESTING: &str = "a folder moved below a folder that was inside it";
/// The end of the reason text for a delta full of dead entries (`"N …"`).
const REASON_SUPERSEDED_SUFFIX: &str = "superseded entries";

/// The counter a rescan reason belongs to (reasons carry numbers and
/// device names, so they cannot be counted as written).
pub fn reason_kind(reason: &str) -> &'static str {
    if reason.starts_with(REASON_MOVED_IN) {
        "moved-in folder"
    } else if reason.starts_with(REASON_LARGE_MOVE) {
        "large folder move"
    } else if reason.starts_with(REASON_NESTING) {
        "folders swapped nesting"
    } else if reason.ends_with(" live changes") {
        "too many live changes"
    } else if reason.ends_with(" replaced entries") {
        "too many replaced entries"
    } else if reason.ends_with(REASON_SUPERSEDED_SUFFIX) {
        "too many superseded entries"
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
    /// `urgent` (the delta outgrew [`DeltaLimits::hard_delta`]).
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

/// A view build may use this share of the time (1 in 11): the next view is
/// published this many times the last build's duration after the last one.
const PUBLISH_DUTY: u32 = 10;
/// …but never wait longer than this to show a change.
const MAX_PUBLISH_GAP: Duration = Duration::from_secs(5);

/// When the maintainer may publish the next view. Building a view costs time
/// in proportion to the delta (about 0.5 µs of CPU per entry); doing it after
/// every 200 ms batch of a long storm (a build, an install) kept a core busy.
/// Changes that arrive meanwhile are already merged into [`Live`], so waiting
/// loses nothing: the next view shows all of them. The last batch is always
/// published, at the latest when the gap has passed. A search never waits for
/// a build: it reads the snapshot published before.
#[derive(Debug, Default)]
pub struct PublishGate {
    /// When the last view was published (none yet: the first goes at once).
    last: Option<Instant>,
    /// How long that view took to build.
    build: Duration,
    /// Changes were applied since the last view.
    dirty: bool,
}

impl PublishGate {
    pub fn new() -> PublishGate {
        PublishGate::default()
    }

    /// Changes were applied: a view is owed.
    pub fn changed(&mut self) {
        self.dirty = true;
    }

    /// The wait after a publish: [`PUBLISH_DUTY`] times the build time.
    fn gap(&self) -> Duration {
        (self.build * PUBLISH_DUTY).min(MAX_PUBLISH_GAP)
    }

    /// A view is owed and its time has come.
    pub fn due(&self, now: Instant) -> bool {
        self.dirty && self.last.is_none_or(|last| now >= last + self.gap())
    }

    /// When the maintainer must wake to publish an owed view.
    pub fn wake_at(&self) -> Option<Instant> {
        self.dirty
            .then(|| self.last.map(|last| last + self.gap()))
            .flatten()
    }

    /// A view that took `build` to make was published at `now` (the end of
    /// the build); it showed every change so far.
    pub fn published(&mut self, now: Instant, build: Duration) {
        self.last = Some(now);
        self.build = build;
        self.dirty = false;
    }
}

/// What answers a reason for maintenance. Carried with the reason from the
/// place that found it (never read back from its text, which is for people).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasonKind {
    /// The live layer has grown too large: the service folds it in memory
    /// ([`Live::compact`]) instead of reading the disks again.
    Compact,
    /// The index may be wrong (a journal reset, a folder that could not be
    /// read, an administrator's request, startup): only a scan can fix it.
    Disk,
}

/// A reason for maintenance: what it needs, and the text shown for it. It
/// derefs to its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reason {
    kind: ReasonKind,
    text: String,
}

impl Reason {
    /// The live layer is large: a compaction answers it.
    pub fn compact(text: impl Into<String>) -> Reason {
        Reason {
            kind: ReasonKind::Compact,
            text: text.into(),
        }
    }

    /// The disks must be read again.
    pub fn disk(text: impl Into<String>) -> Reason {
        Reason {
            kind: ReasonKind::Disk,
            text: text.into(),
        }
    }

    pub fn kind(&self) -> ReasonKind {
        self.kind
    }
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl std::ops::Deref for Reason {
    type Target = str;
    fn deref(&self) -> &str {
        &self.text
    }
}

/// A compaction may use this share of the time (1 in 6): the next one waits
/// this many times the last one's duration…
const COMPACT_DUTY: u32 = 5;
/// …but at least this long…
const MIN_COMPACT_GAP: Duration = Duration::from_secs(60);
/// …and at most this long.
const MAX_COMPACT_GAP: Duration = Duration::from_secs(10 * 60);
/// How long an overgrown delta (past [`DeltaLimits::hard_delta`]) may wait
/// for folder reads or for the index save to end before the cap is enforced
/// anyway: the longest a folder read can last.
const URGENT_PATIENCE: Duration =
    Duration::from_secs(MAX_WALK_TIME.as_secs() + WALK_SLACK.as_secs());
/// While an index save runs, a held-back compaction looks again this often
/// (the save's end does not wake the maintainer).
const SAVE_POLL: Duration = Duration::from_secs(2);

/// What the maintainer does next about the size of the live layer.
#[derive(Debug, PartialEq, Eq)]
pub enum Due {
    /// Read the disks again (the reason is the first one asked for).
    Rescan(String),
    /// Fold the live layer into a new frozen index in memory.
    Compact(String),
}

/// What is going on while the maintainer asks what is due.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Load {
    /// The delta outgrew [`DeltaLimits::hard_delta`]: no waiting for the gap.
    pub urgent: bool,
    /// It outgrew twice that: no waiting for anything.
    pub severe: bool,
    /// Folders are being read.
    pub walks: bool,
    /// The index is being saved to disk (the saver holds the old index).
    pub saving: bool,
    /// A large folder move left entries hidden until a compaction
    /// ([`Live::adoptions_pending`]): one is wanted as soon as its share of
    /// the time allows, without the minimum gap, and a waiting rescan does
    /// not hold it back.
    pub prompt: bool,
}

impl Load {
    /// The load of a live layer with `delta` entries and the given limit.
    pub fn of(delta: usize, hard_delta: usize, walks: bool, saving: bool) -> Load {
        Load {
            urgent: delta > hard_delta,
            severe: delta > hard_delta.saturating_mul(2),
            walks,
            saving,
            prompt: false,
        }
    }
}

/// When a requested rescan or compaction may start. A reason that only says
/// the live layer is large asks for a compaction (seconds of one low-priority
/// thread, no disk); any other reason asks for a rescan (see [`ReasonKind`]).
/// A pending rescan covers a pending compaction (a fresh index has no live
/// layer), except one that shows entries hidden by a large folder move
/// ([`Load::prompt`]): that one runs first and the rescan still follows.
#[derive(Debug)]
pub struct Maintenance {
    rescan: RescanPolicy,
    compact: Option<String>,
    compact_earliest: Instant,
    /// When the last compaction's share of the time ([`COMPACT_DUTY`]) is
    /// used up: the earliest a [`Load::prompt`] one may start.
    duty_earliest: Instant,
    /// Since when the delta has been past its hard limit (without a break).
    urgent_since: Option<Instant>,
    /// Since when entries have been hidden by a large folder move: they
    /// wait for folder reads and saves no longer than an overgrown delta.
    prompt_since: Option<Instant>,
}

impl Maintenance {
    /// After a scan that took `took` and finished at `now`.
    pub fn after_scan(now: Instant, took: Duration) -> Maintenance {
        Maintenance {
            rescan: RescanPolicy::after_scan(now, took),
            compact: None,
            compact_earliest: now + MIN_COMPACT_GAP,
            duty_earliest: now,
            urgent_since: None,
            prompt_since: None,
        }
    }

    /// Ask for a rescan or a compaction, by the reason's kind; the first of
    /// each is kept. True when this started a pending episode of the kind it
    /// asks for: only then is it news.
    pub fn request(&mut self, reason: Reason) -> bool {
        match reason.kind {
            ReasonKind::Compact => {
                let fresh = self.compact.is_none();
                self.compact.get_or_insert(reason.text);
                fresh
            }
            ReasonKind::Disk => self.rescan.request(reason.text),
        }
    }

    /// An administrator asked for a rescan now.
    pub fn force(&mut self, now: Instant, reason: String) {
        self.rescan.force(now, reason);
    }

    /// The rescan waiting for its time (a waiting compaction is not one).
    pub fn rescan_pending(&self) -> Option<(&str, Instant)> {
        self.rescan
            .pending()
            .map(|why| (why, self.rescan.earliest()))
    }

    /// Whether a rescan is waiting.
    pub fn rescan_waiting(&self) -> bool {
        self.rescan.pending().is_some()
    }

    /// Whether an overgrown delta, or entries hidden by a large folder
    /// move, have waited as long as they may.
    fn overdue(&self, now: Instant, load: Load) -> bool {
        load.severe || self.patience().is_some_and(|end| now >= end)
    }

    /// When the patience of an overgrown delta or of hidden entries ends.
    fn patience(&self) -> Option<Instant> {
        [self.urgent_since, self.prompt_since]
            .into_iter()
            .flatten()
            .min()
            .map(|since| since + URGENT_PATIENCE)
    }

    /// What to do now, if anything. A compaction waits for folder reads (their
    /// results are merged into the delta) and for an index save (which holds
    /// the old index); while the delta is [`Load::urgent`] it does not wait
    /// for its gap. The hard limit stays a cap: an urgent delta that has
    /// waited [`URGENT_PATIENCE`], or a [`Load::severe`] one, is not held
    /// back by a save, and when folders keep being read the volumes are
    /// rescanned, as before compactions existed. Entries hidden by a large
    /// folder move ([`Load::prompt`]) have the same patience.
    pub fn due(&mut self, now: Instant, load: Load) -> Option<Due> {
        self.urgent_since = if load.urgent {
            self.urgent_since.or(Some(now))
        } else {
            None
        };
        self.prompt_since = if load.prompt {
            self.prompt_since.or(Some(now))
        } else {
            None
        };
        if let Some(why) = self.rescan.due(now, load.urgent) {
            return Some(Due::Rescan(why));
        }
        if (self.rescan.pending().is_some() && !load.prompt)
            || self.compact.is_none()
            || !(load.urgent
                || now >= self.compact_earliest
                || (load.prompt && now >= self.duty_earliest))
        {
            return None;
        }
        let overdue = self.overdue(now, load);
        if load.walks {
            return if overdue {
                self.compact.take().map(Due::Rescan)
            } else {
                None
            };
        }
        if load.saving && !overdue {
            return None;
        }
        self.compact.take().map(Due::Compact)
    }

    /// A compaction that took `took` ended at `now`: the next waits for its
    /// share of the time.
    pub fn compacted(&mut self, now: Instant, took: Duration) {
        let gap = (took * COMPACT_DUTY).clamp(MIN_COMPACT_GAP, MAX_COMPACT_GAP);
        self.compact_earliest = now + gap;
        self.duty_earliest = now + (took * COMPACT_DUTY).min(MAX_COMPACT_GAP);
        self.urgent_since = None;
        self.prompt_since = None;
    }

    /// When the maintainer must wake to start what is pending. A compaction
    /// held back by a folder read needs no timer of its own (the read's answer
    /// wakes it), but an overgrown delta wakes it when its patience ends; a
    /// save is looked at again every [`SAVE_POLL`].
    pub fn wake_at(&self, now: Instant, load: Load) -> Option<Instant> {
        let compact = (self.compact.is_some() && (load.prompt || !self.rescan_waiting()))
            .then(|| {
                let patience = self.patience();
                let earliest = if load.prompt {
                    self.compact_earliest.min(self.duty_earliest)
                } else {
                    self.compact_earliest
                };
                match (load.walks, load.saving) {
                    (true, _) => patience,
                    (false, true) => {
                        let looks = earliest.max(now + SAVE_POLL);
                        Some(patience.map_or(looks, |p| p.min(looks)))
                    }
                    (false, false) => Some(earliest),
                }
            })
            .flatten();
        [self.rescan.wake_at(), compact].into_iter().flatten().min()
    }
}

/// Compact `live` and publish the result with one snapshot swap, or do
/// nothing (`None`) when `build` declines (folders are being read). `build`
/// is the compaction itself: the service runs it on its low-priority pool, a
/// test can make it slow. Searches use the snapshot published before for as
/// long as it runs, and never wait for it; the old index is freed when the
/// last search that holds it ends. `report` is the new snapshot's report
/// line. `warm` is called with each metadata sort order that was ready on the
/// old index, right after the swap, so the new index prepares them in the
/// background (see [`Live::compact`]). Returns how long the compaction took
/// (without the warming, which does not block).
pub fn compact_and_publish(
    live: &mut Live,
    store: &crate::SnapshotStore,
    report: String,
    build: impl FnOnce(&mut Live) -> Option<Live>,
    warm: impl Fn(&Index, SortKey),
) -> Option<Duration> {
    let started = Instant::now();
    let ready = ready_sorts(live.base());
    let new = build(live)?;
    let took = started.elapsed();
    // Everything applied so far is in the new index: no delta to show.
    store.swap(crate::Snapshot {
        index: new.base().clone(),
        live: None,
        mode: "live".into(),
        report,
    });
    for key in ready {
        warm(new.base(), key);
    }
    // The old layer's tables go now; its index goes with the old snapshot.
    *live = new;
    Some(took)
}

/// The metadata sort orders `index` has prepared.
pub fn ready_sorts(index: &Index) -> Vec<SortKey> {
    [
        SortKey::Size,
        SortKey::Modified,
        SortKey::Created,
        SortKey::Accessed,
    ]
    .into_iter()
    .filter(|&key| index.metadata_order(key, false).is_some())
    .collect()
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

    /// Whether the file system answered that `record` is not in use, as
    /// opposed to the read failing: asked after `fetch` gave `None`, when a
    /// failed read must not be taken for a deletion. A source that cannot
    /// tell reads `None` as deleted.
    fn certainly_free(&self, _volume: usize, _record: u64) -> bool {
        true
    }
}

/// What a volume's tables are built from.
struct VolumeSeed {
    /// Table size: the highest record number plus one.
    records: u64,
    excluded: Arc<HashSet<u64>>,
    roots: HashSet<u64>,
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
    /// The overlay is too large (a compaction answers it) or a change cannot
    /// be applied incrementally (a folder too large to read in the
    /// background, a failed walk: rescan the volumes).
    pub rescan: Option<Reason>,
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
            self.ask(reason);
        }
    }

    /// Ask for a rescan or a compaction, by the reason's kind. The first
    /// reason is kept, except that one which needs the disks read again
    /// replaces one which a compaction answers: the maintainer routes by it.
    fn ask(&mut self, reason: Reason) {
        match &self.rescan {
            None => self.rescan = Some(reason),
            Some(old) if old.kind == ReasonKind::Compact && reason.kind == ReasonKind::Disk => {
                self.rescan = Some(reason)
            }
            Some(_) => {}
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
    /// When it was queued, and when the reader began it (for the watchdog;
    /// none while it waits).
    queued: Instant,
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
    limits: DeltaLimits,
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
    /// Frozen folders hidden by a move too large to copy, and the new entry
    /// of the folder that took over their children (always a delta entry).
    /// The children stay where they are; the next compaction places them
    /// under the new entry. See [`Live::move_children`].
    adopted: HashMap<u32, Loc>,
    /// The same, by new entry.
    adopters: HashMap<Loc, Vec<u32>>,
    /// Frozen entries below an adopted folder (bit set): kept, but hidden
    /// from searches until the compaction gives them their new path, so a
    /// search never shows a path that no longer exists. Empty while nothing
    /// is adopted.
    hidden: Vec<u64>,
    /// A folder move copies at most this many entries; larger frozen
    /// subtrees are adopted instead ([`MAX_MOVE`]; smaller in tests).
    move_budget: usize,
    /// Folders waiting for their background walk.
    walks: Vec<Walk>,
    next_walk: u64,
    /// One log of the records reported while any walk is outstanding.
    log: HashMap<(usize, u64), Touch>,
    log_seq: u64,
    /// When the reader last showed it is alive (ready, began or finished a
    /// folder): a queued folder only waits for it from then on.
    progress: Instant,
}

impl Live {
    pub fn new(base: Arc<Index>, maps: &[VolumeMap]) -> Live {
        let n = base.len();
        let mut base_record = vec![u64::MAX; n];
        for (v, map) in maps.iter().enumerate() {
            for &(record, id) in &map.entries {
                if (id as usize) < n {
                    base_record[id as usize] = ((v as u64) << 48) | record;
                }
            }
        }
        let seeds = maps
            .iter()
            .map(|map| VolumeSeed {
                records: map.entries.iter().map(|&(r, _)| r + 1).max().unwrap_or(0),
                excluded: Arc::new(map.excluded.iter().copied().collect()),
                roots: map.roots.iter().map(|&(r, _)| r).collect(),
            })
            .collect();
        let entries = maps
            .iter()
            .enumerate()
            .flat_map(|(v, map)| map.entries.iter().map(move |&(record, id)| (v, record, id)));
        let volumes = Live::volumes(seeds, entries);
        Live::assemble(base, base_record, volumes, 0)
    }

    /// The per-volume record tables from `(volume, record, frozen id)`.
    fn volumes(
        seeds: Vec<VolumeSeed>,
        entries: impl Iterator<Item = (usize, u64, u32)>,
    ) -> Vec<Volume> {
        let mut volumes: Vec<Volume> = seeds
            .into_iter()
            .map(|seed| Volume {
                record_base: vec![u32::MAX; seed.records as usize],
                overlay: HashMap::new(),
                excluded: seed.excluded,
                roots: seed.roots,
            })
            .collect();
        for (v, record, id) in entries {
            let volume = &mut volumes[v];
            let slot = &mut volume.record_base[record as usize];
            if *slot == u32::MAX && !volume.overlay.contains_key(&record) {
                *slot = id;
            } else {
                // A second name: keep every location in the overlay.
                let list = volume.overlay.entry(record).or_default();
                if *slot != u32::MAX {
                    list.push(Loc::Base(*slot));
                    *slot = u32::MAX;
                }
                list.push(Loc::Base(id));
            }
        }
        volumes
    }

    /// A live layer over `base` with nothing changed yet. `next_walk`: the id
    /// of the next folder read (ids stay unique across a compaction, so a
    /// late answer of an abandoned reader cannot be taken for a new read).
    fn assemble(
        base: Arc<Index>,
        base_record: Vec<u64>,
        volumes: Vec<Volume>,
        next_walk: u64,
    ) -> Live {
        let n = base.len();
        Live {
            limits: DeltaLimits::for_base(n),
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
            adopted: HashMap::new(),
            adopters: HashMap::new(),
            hidden: Vec::new(),
            move_budget: MAX_MOVE,
            walks: Vec::new(),
            next_walk,
            log: HashMap::new(),
            log_seq: 0,
            progress: Instant::now(),
            base,
        }
    }

    pub fn base(&self) -> &Arc<Index> {
        &self.base
    }

    /// The size limits of this layer (they follow the frozen index).
    pub fn limits(&self) -> DeltaLimits {
        self.limits
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
                if p == ROOT_PARENT {
                    return None;
                }
                // A child of an adopted folder belongs to its new entry.
                Some(self.adopted.get(&p).copied().unwrap_or(Loc::Base(p)))
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
        // `compact` places the delta in one pass because of this.
        debug_assert!(!matches!(entry.parent, Loc::Delta(p) if p >= d));
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
        // Folders waiting for or being read, and the records reported
        // meanwhile (empty, and freed, while no walk is outstanding).
        let walks = vec_bytes(&self.walks)
            + self
                .walks
                .iter()
                .map(|w| vec_bytes(&w.covered))
                .sum::<usize>();
        let change_log = table_bytes(self.log.capacity(), size_of::<((usize, u64), Touch)>());
        let adopted = table_bytes(self.adopted.capacity(), size_of::<(u32, Loc)>())
            + table_bytes(self.adopters.capacity(), size_of::<(Loc, Vec<u32>)>())
            + self.adopters.values().map(vec_bytes).sum::<usize>()
            + vec_bytes(&self.hidden);
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
            Part::new("live.walks", walks, Kind::Steady),
            Part::new("live.change_log", change_log, Kind::Steady),
            // A large folder move, until the next compaction.
            Part::new("live.adopted", adopted, Kind::Steady),
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

    /// Frozen children of a frozen folder (alive or not).
    fn base_children_of(&self, id: u32) -> &[u32] {
        let (first, ids) = self.base_children();
        &ids[first[id as usize] as usize..first[id as usize + 1] as usize]
    }

    /// Whether the frozen subtree below `id` holds more than `limit`
    /// entries (counted without allocating per folder; stops at the limit).
    fn base_subtree_exceeds(&self, id: u32, limit: usize) -> bool {
        let mut count = 0usize;
        let mut stack = vec![id];
        while let Some(dir) = stack.pop() {
            for &c in self.base_children_of(dir) {
                count += 1;
                if count > limit {
                    return true;
                }
                if self.base.flags(c) & flags::DIR != 0 {
                    stack.push(c);
                }
            }
        }
        false
    }

    /// `to` takes over the frozen children of the frozen folder `dir`
    /// (already hidden): they stay in place, hidden from searches, until a
    /// compaction places them under `to`. Costs one bit per frozen entry
    /// once, and a pass over the subtree.
    fn adopt(&mut self, dir: u32, to: Loc) {
        debug_assert!(matches!(to, Loc::Delta(_)));
        self.adopted.insert(dir, to);
        self.adopters.entry(to).or_default().push(dir);
        if self.hidden.is_empty() {
            self.hidden = vec![0; self.base.len().div_ceil(64)];
        }
        let mut hidden = std::mem::take(&mut self.hidden);
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            for &c in self.base_children_of(d) {
                hidden[c as usize / 64] |= 1 << (c % 64);
                if self.base.flags(c) & flags::DIR != 0 {
                    stack.push(c);
                }
            }
        }
        self.hidden = hidden;
    }

    /// Whether a frozen entry is hidden below an adopted folder.
    fn is_hidden(&self, id: u32) -> bool {
        !self.hidden.is_empty() && self.hidden[id as usize / 64] >> (id % 64) & 1 != 0
    }

    /// Whether a large folder move left frozen entries to re-attach: they
    /// are hidden from searches until a compaction, which is then wanted
    /// soon ([`Load::prompt`]).
    pub fn adoptions_pending(&self) -> bool {
        !self.adopted.is_empty()
    }

    fn base_children(&self) -> &(Vec<u32>, Vec<u32>) {
        self.base_children.get_or_init(|| {
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
        })
    }

    /// The live children of a folder: its frozen children, those of the
    /// frozen folders it adopted, and its delta children.
    fn children(&self, loc: Loc) -> Vec<Loc> {
        let mut out = Vec::new();
        if let Loc::Base(id) = loc {
            out.extend(self.base_children_of(id).iter().map(|&c| Loc::Base(c)));
        }
        if let Some(dirs) = self.adopters.get(&loc) {
            for &dir in dirs {
                out.extend(self.base_children_of(dir).iter().map(|&c| Loc::Base(c)));
            }
        }
        if let Some(ids) = self.delta_children.get(&loc) {
            out.extend(ids.iter().map(|&d| Loc::Delta(d)));
        }
        out.retain(|&l| self.is_alive(l));
        out
    }

    /// Move the subtree below `from` under `to` (a folder's new entry).
    ///
    /// Entries are copied into the delta up to `budget`. A frozen folder
    /// whose subtree is larger than what is left is not copied: its new
    /// entry adopts its frozen children instead ([`Live::adopt`]), which
    /// costs a pass over the subtree and no memory per entry, and the
    /// children are placed by the next compaction. Before compactions
    /// existed, such a move asked for a full rescan of every volume (over a
    /// minute of heavy CPU and disk on a PC with 27 million entries), and a
    /// tool that renames a large folder now and then kept the service
    /// rescanning. Folders adopted by an entry that moves again go with it.
    ///
    /// Returns whether anything was adopted (a compaction is then wanted).
    fn move_children(&mut self, from: Loc, to: Loc, budget: &mut usize) -> bool {
        let mut adopted = false;
        // Once a frozen folder's whole subtree fits, so do the folders below
        // it: they are not counted again (only delta entries, which the
        // delta limits bound, come on top).
        let mut fits = false;
        let mut stack = vec![(from, to)];
        while let Some((old_dir, new_dir)) = stack.pop() {
            if let Some(dirs) = self.adopters.remove(&old_dir) {
                for &dir in &dirs {
                    self.adopted.insert(dir, new_dir);
                }
                self.adopters.entry(new_dir).or_default().extend(dirs);
                adopted = true;
            }
            let large = match old_dir {
                Loc::Base(id) if !fits => {
                    let large = self.base_subtree_exceeds(id, *budget);
                    fits = !large;
                    large
                }
                _ => false,
            };
            let children = match old_dir {
                Loc::Base(id) if large => {
                    self.adopt(id, new_dir);
                    adopted = true;
                    // Only the delta children are copied.
                    self.delta_children
                        .get(&old_dir)
                        .map(|ids| {
                            ids.iter()
                                .map(|&d| Loc::Delta(d))
                                .filter(|&l| self.is_alive(l))
                                .collect()
                        })
                        .unwrap_or_default()
                }
                _ => self.children(old_dir),
            };
            for child in children {
                *budget = budget.saturating_sub(1);
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
        adopted
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
        // waits until the parent has an entry, and a folder whose new parent
        // also moves in this batch and is still below it here waits until
        // that parent has moved out (see `apply_one`).
        loop {
            let waiting: HashSet<(usize, u64)> =
                pending.iter().map(|(c, _)| (c.volume, c.record)).collect();
            let before = pending.len();
            let mut deferred = Vec::new();
            for (change, state) in std::mem::take(&mut pending) {
                let blocked = state.as_ref().is_some_and(|s| {
                    s.names.iter().any(|(parent, _)| {
                        if *parent == change.record || !waiting.contains(&(change.volume, *parent))
                        {
                            return false;
                        }
                        let at = self.locations(change.volume, *parent);
                        if at.is_empty() {
                            return true;
                        }
                        let own = self.locations(change.volume, change.record);
                        !own.is_empty() && at.iter().any(|&p| self.at_or_below(p, &own))
                    })
                });
                if blocked {
                    deferred.push((change, state));
                } else if let Err(reason) = self.apply_one(change, state, &mut applied) {
                    applied.ask(Reason::disk(reason));
                }
            }
            pending = deferred;
            if pending.is_empty() || pending.len() == before {
                break;
            }
        }
        for (change, state) in pending {
            if let Err(reason) = self.apply_one(change, state, &mut applied) {
                applied.ask(Reason::disk(reason));
            }
        }
        self.check_limits(&mut applied);
        applied
    }

    /// Ask for a rescan when the overlay has outgrown its limits.
    fn check_limits(&self, applied: &mut Applied) {
        if self.delta_alive > self.limits.max_delta {
            applied.ask(Reason::compact(format!(
                "{} live changes",
                self.delta_alive
            )));
        }
        // Entries replaced or deleted again stay in the delta until the next
        // rescan: few live ones among many dead still cost memory.
        let dead = self.delta.len() - self.delta_alive;
        if dead > self.limits.max_dead {
            applied.ask(Reason::compact(format!(
                "{dead} {REASON_SUPERSEDED_SUFFIX}"
            )));
        }
        if self.removed_count > self.base.len() / MAX_REMOVED_DIVISOR + 1000 {
            applied.ask(Reason::compact(format!(
                "{} replaced entries",
                self.removed_count
            )));
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

    /// Keep the log within [`DeltaLimits::max_touched`] records: the oldest walks lose
    /// their window first (they fall back to the rescan when they finish),
    /// younger ones keep theirs.
    fn trim_log(&mut self) {
        while self.log.len() > self.limits.max_touched {
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
        // A walk that lost its window needs nothing from the log.
        match self
            .walks
            .iter()
            .filter(|w| !w.overflowed)
            .map(|w| w.since)
            .min()
        {
            // Nothing needs the log: free its table (a flood of changes
            // would otherwise keep megabytes for the rest of the session).
            None => self.log = HashMap::new(),
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
    /// Whether `loc` is one of `dirs` or (still, in this index) below one
    /// of them. A chain longer than any real path counts as below.
    fn at_or_below(&self, loc: Loc, dirs: &[Loc]) -> bool {
        let mut at = Some(loc);
        for _ in 0..4096 {
            match at {
                None => return false,
                Some(l) if dirs.contains(&l) => return true,
                Some(l) => at = self.parent_of(l),
            }
        }
        true
    }

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
        let budget = self.limits.max_delta.saturating_sub(self.delta_alive);
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
            queued: Instant::now(),
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

    /// The reader is up and has its volumes open.
    pub fn reader_ready(&mut self, now: Instant) {
        self.progress = now;
    }

    /// The reader began walk `id` at `now`.
    pub fn walk_started(&mut self, id: u64, now: Instant) {
        self.progress = now;
        if let Some(walk) = self.walks.iter_mut().find(|w| w.id == id) {
            walk.started = Some(now);
        }
    }

    /// When the maintainer must wake to check on the reader. A walk the
    /// reader began is due its time budget plus slack after it began. While
    /// none has begun, the queue is not being served: a folder is due that
    /// long after it was queued or the reader last showed life, whichever is
    /// later (the reader can hang opening its volumes before it takes a job).
    pub fn next_walk_deadline(&self) -> Option<Instant> {
        let patience = MAX_WALK_TIME + WALK_SLACK;
        let running = self
            .walks
            .iter()
            .filter_map(|w| w.started)
            .map(|started| started + patience)
            .min();
        running.or_else(|| {
            self.walks
                .iter()
                .map(|w| w.queued.max(self.progress) + patience)
                .min()
        })
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
        applied.rescan = Some(Reason::disk(format!(
            "{REASON_MOVED_IN} (reading its contents did not finish in time)"
        )));
        self.walks = Vec::new();
        self.log = HashMap::new();
        self.progress = now;
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
        self.progress = Instant::now();
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
        // Whether the record may still be that folder: it is, unless the
        // file system said it is free or now something else. A read that
        // failed proves nothing.
        let exists = |record: u64, sequence: u16| match source.fetch(volume, record) {
            Some(s) => s.info.is_dir && (sequence == 0 || s.info.sequence == sequence),
            None => !source.certainly_free(volume, record),
        };
        let listing = match result {
            Ok(listing) => listing,
            Err(WalkError::Cancelled) => return applied,
            // Deleted meanwhile: the journal says so, and the records
            // reported since are re-read below. Believed only if the record
            // really is not a folder any more.
            Err(WalkError::Gone) if !exists(walk.record, walk.sequence) => Subtree::default(),
            Err(WalkError::Gone) => {
                applied.rescan = Some(Reason::disk(format!(
                    "{REASON_MOVED_IN} (the folder exists but could not be opened)"
                )));
                return applied;
            }
            Err(error) => {
                applied.rescan = Some(Reason::disk(format!("{REASON_MOVED_IN} ({error})")));
                return applied;
            }
        };
        if walk.overflowed {
            applied.rescan = Some(Reason::disk(format!(
                "{REASON_MOVED_IN} (too many changes while its contents were read)"
            )));
            return applied;
        }
        // A subfolder that could not be opened is deleted or moved away (the
        // journal says so); one that still exists means an incomplete walk.
        if listing
            .skipped
            .iter()
            .any(|&r| exists(r & RECORD_MASK, (r >> 48) as u16))
        {
            applied.rescan = Some(Reason::disk(format!(
                "{REASON_MOVED_IN} (a subfolder exists but could not be opened)"
            )));
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
                applied.ask(Reason::disk(reason));
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
            applied.ask(reason);
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
                applied.ask(Reason::disk(reason));
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
            // A folder's own size or dates keep its entry so its children
            // stay attached. A file that already lives in the delta is
            // updated in place too: replacing its entry would leave a dead
            // one behind (and cost a view entry) on every write of a busy
            // log or database file.
            if descended || old.iter().all(|l| matches!(l, Loc::Delta(_))) {
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
        // A folder whose new parent this index still has below it: a folder
        // inside it moved out and it moved below that one, and the batch
        // reached it first (or the other move is not reported yet). Placing
        // it there would hang it below itself (lost on the next compaction,
        // or a loop in its path); only a scan sees the real nesting.
        if old.iter().any(|&l| self.flags_of(l) & flags::DIR != 0)
            && desired.iter().any(|(p, _)| self.at_or_below(*p, &old))
        {
            return Err(REASON_NESTING.into());
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
                let mut budget = self.move_budget;
                if self.move_children(old[0], to, &mut budget) {
                    applied.ask(Reason::compact(REASON_LARGE_MOVE));
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

    /// Whether folders are being read in the background. A compaction waits
    /// for them: their results are merged into the delta by id.
    pub fn walks_outstanding(&self) -> bool {
        !self.walks.is_empty()
    }

    /// Fold the live state into a new frozen index and a new live layer over
    /// it: the frozen entries still shown (with their folder overrides) plus
    /// the live delta entries, in that order. Everything a search returns
    /// stays the same (paths and metadata); what changes is that the delta,
    /// the hidden entries, the dead entries and the lookup tables built for
    /// folder moves are gone, and a search no longer merges a delta.
    ///
    /// Entry ids change (a page re-runs its query, so nothing holds an id).
    /// Hard links, record numbers, excluded records and roots carry over;
    /// entries without a record stay in the index without a map entry. The
    /// cached metadata sort orders start empty, as after a scan;
    /// [`compact_and_publish`] asks for the ones that were ready before to be
    /// built again in the background, so a sorted search does not fall back to
    /// name order for longer than a scan would make it.
    ///
    /// `None` while folders are being read: their records are not placed
    /// yet, and a compaction must not drop the state that merges them.
    ///
    /// Memory: the builder (about 87 bytes per entry while it is fed and
    /// frozen) and the new tables come on top of the old index, which stays
    /// until the caller publishes the result and the last search drops the
    /// old snapshot. No disk is read.
    pub fn compact(&mut self) -> Option<Live> {
        if self.walks_outstanding() {
            return None;
        }
        // Lazily built lookup tables are only needed for the next folder
        // move; the new layer builds them again on demand.
        self.base_children = OnceLock::new();
        self.log = HashMap::new();

        let n = self.base.len();
        // Frozen entries that survive: shown, and with a surviving parent. An
        // entry whose parent was hidden without it cannot be placed, and a
        // scan would not list it. Nothing guarantees that a parent has the
        // lower id (a builder takes any id for a parent; a reader that lists
        // records in record order can give a folder a later id than its
        // children), so the pass repeats until nothing more is dropped. It
        // runs once more than needed only when some entry's parent has the
        // higher id; an index whose parents all come first takes one pass.
        let mut keep: Vec<u64> = self.removed.iter().map(|w| !w).collect();
        if !n.is_multiple_of(64)
            && let Some(last) = keep.last_mut()
        {
            *last &= (1u64 << (n % 64)) - 1;
        }
        let bit = |keep: &[u64], id: u32| keep[id as usize / 64] >> (id % 64) & 1 != 0;
        // A frozen entry whose parent was adopted by a delta entry (a large
        // folder move) survives with that entry, which is placed after the
        // frozen ones; which delta entries are placed depends on the frozen
        // ones in turn, so with adoptions the passes repeat until nothing
        // more is dropped.
        let mut placed_delta = if self.adopted.is_empty() {
            Vec::new()
        } else {
            self.placeable_delta(&keep)
        };
        loop {
            let (mut dropped, mut parent_after_child) = (false, false);
            for id in 0..n as u32 {
                if !bit(&keep, id) {
                    continue;
                }
                let parent = self.base.parent(id);
                if parent == ROOT_PARENT {
                    continue;
                }
                parent_after_child |= parent > id;
                if !bit(&keep, parent)
                    && !matches!(self.adopted.get(&parent), Some(Loc::Delta(d)) if placed_delta[*d as usize])
                {
                    keep[id as usize / 64] &= !(1 << (id % 64));
                    dropped = true;
                }
            }
            // With every parent below its child, one pass is exact.
            if !dropped || (!parent_after_child && self.adopted.is_empty()) {
                break;
            }
            if !self.adopted.is_empty() {
                placed_delta = self.placeable_delta(&keep);
            }
        }
        let placed_delta = if self.adopted.is_empty() {
            self.placeable_delta(&keep)
        } else {
            // Unchanged by the last pass, which dropped nothing.
            placed_delta
        };
        let (mut kept_base, mut name_bytes) = (0usize, 0usize);
        for id in 0..n as u32 {
            if bit(&keep, id) {
                kept_base += 1;
                name_bytes += self.base.name(id).len();
            }
        }
        // Surviving entries before each word: a frozen id's new id is that
        // plus the survivors below it in its word.
        let mut before: Vec<u32> = Vec::with_capacity(keep.len());
        let mut running = 0u32;
        for word in &keep {
            before.push(running);
            running += word.count_ones();
        }
        let new_id = |id: u32| -> u32 {
            let word = id as usize / 64;
            before[word] + (keep[word] & ((1u64 << (id % 64)) - 1)).count_ones()
        };

        // The new id of each placed delta entry: after the frozen ones, in
        // delta order (a frozen entry adopted by one refers to it ahead).
        let mut placed: Vec<u32> = vec![u32::MAX; self.delta.len()];
        let mut next_delta = kept_base as u32;
        for (d, ok) in placed_delta.iter().enumerate() {
            if *ok {
                placed[d] = next_delta;
                next_delta += 1;
            }
        }
        drop(placed_delta);

        let builder = IndexBuilder::with_columns(self.columns);
        let delta_names: usize = self
            .delta
            .iter()
            .zip(&placed)
            .filter(|(_, id)| **id != u32::MAX)
            .map(|(e, _)| e.name.len())
            .sum();
        builder.reserve(kept_base + self.delta_alive, name_bytes + delta_names);
        let mut records: Vec<u64> = Vec::with_capacity(kept_base + self.delta_alive);
        let mut next = 0u32;
        for id in 0..n as u32 {
            if !bit(&keep, id) {
                continue;
            }
            let parent = self.base.parent(id);
            let parent = if parent == ROOT_PARENT {
                ROOT_PARENT
            } else if bit(&keep, parent) {
                new_id(parent)
            } else {
                // Kept with a hidden parent: adopted by a placed delta entry.
                // (The passes above keep nothing else; a service must not
                // stop over it, so a release build shows it as a root.)
                match self.adopted.get(&parent) {
                    Some(Loc::Delta(d)) if placed[*d as usize] != u32::MAX => placed[*d as usize],
                    _ => {
                        debug_assert!(false, "kept without a kept or adopting parent");
                        ROOT_PARENT
                    }
                }
            };
            let metadata = if self.overrides.is_empty() {
                self.base.metadata(id)
            } else {
                self.metadata_of(Loc::Base(id))
            };
            let added = builder.add_with_metadata(
                parent,
                self.base.name(id),
                self.base.flags(id),
                metadata,
            );
            debug_assert_eq!(added, next);
            next += 1;
            records.push(self.base_record[id as usize]);
        }
        // The live entries follow the frozen ones, those placeable in
        // `placed` (see [`Live::placeable_delta`]).
        for (d, e) in self.delta.iter().enumerate() {
            if placed[d] == u32::MAX {
                continue;
            }
            let parent = match e.parent {
                Loc::Base(b) => new_id(b),
                Loc::Delta(p) => placed[p as usize],
            };
            let added = builder.add_with_metadata(parent, &e.name, e.flags, e.metadata);
            debug_assert_eq!(added, placed[d]);
            records.push(if e.record == u64::MAX {
                u64::MAX
            } else {
                ((e.volume as u64) << 48) | e.record
            });
        }
        drop(keep);
        drop(before);
        drop(placed);
        let index = Arc::new(builder.freeze());
        debug_assert_eq!(index.len(), records.len());

        // The tables of the new layer, after the builder has been freed.
        let mut seeds: Vec<VolumeSeed> = self
            .volumes
            .iter()
            .map(|v| VolumeSeed {
                records: 0,
                excluded: v.excluded.clone(),
                roots: v.roots.clone(),
            })
            .collect();
        for &packed in records.iter().filter(|&&p| p != u64::MAX) {
            let seed = &mut seeds[(packed >> 48) as usize];
            seed.records = seed.records.max((packed & RECORD_MASK) + 1);
        }
        let entries = records
            .iter()
            .enumerate()
            .filter(|(_, p)| **p != u64::MAX)
            .map(|(id, &p)| ((p >> 48) as usize, p & RECORD_MASK, id as u32));
        let volumes = Live::volumes(seeds, entries);
        Some(Live::assemble(index, records, volumes, self.next_walk))
    }

    /// Which delta entries a compaction places, given the frozen entries it
    /// keeps: alive, with a kept frozen parent or a placed delta parent. A
    /// delta entry's parent is always a lower delta index (`add` takes a
    /// parent that was found among the entries that exist, and asserts it),
    /// so one pass in order is exact.
    fn placeable_delta(&self, keep: &[u64]) -> Vec<bool> {
        let mut ok = vec![false; self.delta.len()];
        for (d, e) in self.delta.iter().enumerate() {
            ok[d] = e.alive
                && match e.parent {
                    Loc::Base(b) => keep[b as usize / 64] >> (b % 64) & 1 != 0,
                    Loc::Delta(p) => ok[p as usize],
                };
        }
        ok
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
            // Entries below an adopted folder wait for the compaction.
            for (w, h) in words.iter_mut().zip(&self.hidden) {
                *w &= !h;
            }
            words
        };
        let base_shown = if self.hidden.is_empty() {
            self.base.len() - self.removed_count
        } else {
            allowed.iter().map(|w| w.count_ones() as usize).sum()
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
                    if !self.is_alive(Loc::Base(b)) || self.is_hidden(b) {
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
            entries: base_shown + shown_delta,
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
    use std::sync::atomic::AtomicBool;

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
            MIN_DELTA - 1,
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
        assert_eq!(r.walks[0].limits.max_entries, MIN_DELTA - 2000 - 1);
        // With the delta nearly full there is no room to read a folder into:
        // ask for the rescan right away (and do not start a walk).
        let (base, map) = fixture();
        let mut live = Live::new(base, &[map]);
        let mut changes = Vec::new();
        for i in 0..(MIN_DELTA - 500) as u64 {
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
        let flood: Vec<Change> = (0..live.limits().max_touched as u64 + 1)
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
        assert!(
            live.next_walk_deadline().is_some(),
            "a queued folder has one too"
        );
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
        let flood: Vec<Change> = (0..live.limits().max_touched as u64 - 10)
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

    /// A source whose reads of a missing record fail instead of saying "free".
    struct Unsure<'a>(&'a Fake);
    impl RecordSource for Unsure<'_> {
        fn fetch(&self, volume: usize, record: u64) -> Option<RecordState> {
            self.0.fetch(volume, record)
        }
        fn certainly_free(&self, _volume: usize, _record: u64) -> bool {
            false
        }
    }

    #[test]
    fn a_reader_that_never_takes_a_job_is_given_up() {
        // The reader hangs before it takes its first job (opening a volume):
        // no walk ever starts, yet the queued folder must not wait forever.
        let f = moved_in();
        let (mut live, _, job) = after_move_in(&f);
        let patience = MAX_WALK_TIME + WALK_SLACK;
        let queued = live.walks[0].queued;
        let due = live
            .next_walk_deadline()
            .expect("a queued folder has a deadline");
        assert_eq!(due, queued.max(live.progress) + patience);
        assert_eq!(live.expire_walks(queued), Applied::default());
        // A reader that shows life (ready) gets its patience from then on.
        let later = queued + Duration::from_secs(20);
        live.reader_ready(later);
        assert_eq!(live.next_walk_deadline(), Some(later + patience));
        assert_eq!(
            live.expire_walks(later + patience - Duration::from_secs(1)),
            Applied::default()
        );
        let r = live.expire_walks(later + patience);
        let why = r.rescan.expect("falls back to the rescan");
        assert_eq!(reason_kind(&why), "moved-in folder", "{why}");
        assert_eq!(r.abandoned, 1);
        assert!(live.walks.is_empty());
        assert_eq!(
            live.finish_walk(job.id, Ok(Subtree::default()), &f),
            Applied::default()
        );
        // A folder behind a running one waits for it: only the running
        // walk's own deadline counts.
        let f = moved_in();
        let (mut live, _, job) = after_move_in(&f);
        let mut f2 = f;
        f2.set(100, DIR_INFO, &[(5, "Other")]);
        live.apply(&[change(100, false)], &f2);
        let t0 = Instant::now() + Duration::from_secs(5);
        live.walk_started(job.id, t0);
        assert_eq!(live.next_walk_deadline(), Some(t0 + patience));
    }

    #[test]
    fn a_failed_read_is_not_a_deletion() {
        // The folder's record cannot be read (not "free"): not confirmed
        // gone, so the walk that could not open it falls back to the rescan.
        let mut f = moved_in();
        let (mut live, _, job) = after_move_in(&f);
        for r in [60, 61, 62, 63] {
            f.0.remove(&r);
        }
        let done = live.finish_walk(job.id, Err(WalkError::Gone), &Unsure(&f));
        let why = done.rescan.expect("not confirmed gone");
        assert_eq!(reason_kind(&why), "moved-in folder", "{why}");
        // The same for a subfolder that could not be opened.
        let mut f = moved_in();
        let (mut live, _, job) = after_move_in(&f);
        f.0.remove(&62);
        f.0.remove(&63);
        let listing = Subtree {
            entries: walk_fake(&f, &job),
            skipped: vec![(1u64 << 48) | 62],
        };
        let done = live.finish_walk(job.id, Ok(listing), &Unsure(&f));
        assert!(done.rescan.is_some());
    }

    #[test]
    fn a_full_log_drops_the_oldest_window_first() {
        let f = moved_in();
        let (mut live, _, first) = after_move_in(&f);
        let mut f = f;
        // 40,000 changes during the first walk…
        let before: Vec<Change> = (0..40_000u64).map(|r| change(10_000 + r, false)).collect();
        live.apply(&before, &f);
        f.set(100, DIR_INFO, &[(5, "Other")]);
        let second = live.apply(&[change(100, false)], &f).walks[0].clone();
        // …and 20,000 more during both: the log (50,000 at most) cannot hold
        // them all.
        let after: Vec<Change> = (0..20_000u64).map(|r| change(100_000 + r, false)).collect();
        live.apply(&after, &f);
        assert!(live.log.len() <= live.limits().max_touched);
        let (a, b) = (&live.walks[0], &live.walks[1]);
        assert!(a.overflowed, "the oldest walk lost its window");
        assert!(!b.overflowed, "the younger one keeps its own");
        assert_eq!(live.changes_since(b.since).len(), 20_000);
        // The younger walk merges first: the older, which lost its window,
        // needs nothing from the log, so nothing is kept for it.
        let done = live.finish_walk(second.id, Ok(walk_fake(&f, &second).into()), &f);
        assert_eq!(done.rescan, None);
        assert!(live.log.is_empty(), "{} records kept", live.log.len());
        // The older one falls back to the rescan when it finishes.
        let done = live.finish_walk(first.id, Ok(walk_fake(&f, &first).into()), &f);
        let why = done.rescan.expect("its window is gone");
        assert_eq!(reason_kind(&why), "moved-in folder", "{why}");
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
    fn memory_accounting_covers_the_walks_and_their_change_log() {
        let part = |parts: &[Part], name: &str| {
            parts
                .iter()
                .find(|p| p.name == name)
                .unwrap_or_else(|| panic!("{name} missing"))
                .bytes
        };
        let f = moved_in();
        let (mut live, _, job) = after_move_in(&f);
        // A folder waits, and changes are logged meanwhile.
        let flood: Vec<Change> = (0..5_000u64).map(|r| change(10_000 + r, false)).collect();
        live.apply(&flood, &f);
        let parts = live.memory_parts();
        assert!(part(&parts, "live.walks") >= size_of::<Walk>() as u64);
        assert_eq!(
            part(&parts, "live.change_log"),
            table_bytes(live.log.capacity(), size_of::<((usize, u64), Touch)>()) as u64
        );
        assert!(part(&parts, "live.change_log") >= 5_000 * size_of::<Touch>() as u64);
        // Covered folders are counted by capacity too.
        live.walks[0].covered.push((70, 1));
        let more = live.memory_parts();
        assert!(part(&more, "live.walks") > part(&parts, "live.walks"));
        // They are in the breakdown's total (nothing is counted outside the
        // named parts).
        let all = live.breakdown(None, 0);
        assert_eq!(
            part(&all.parts, "live.change_log"),
            part(&more, "live.change_log")
        );
        // Once the walk is merged nothing is kept: the table is freed, not
        // just emptied.
        live.finish_walk(job.id, Ok(walk_fake(&f, &job).into()), &f);
        let after = live.memory_parts();
        assert_eq!(part(&after, "live.change_log"), 0);
        assert!(live.walks.is_empty());
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

    #[test]
    fn a_rewritten_file_keeps_one_delta_entry() {
        // A log or database file written again and again: the first write
        // moves its frozen entry into the delta, every later one updates
        // that entry. Replacing it each time left one dead entry per write
        // (2000 batches gave a delta of 2000 with one alive).
        let (base, map) = fixture();
        let mut live = Live::new(base.clone(), &[map]);
        let mut f = initial();
        f.set(40, file_info(9), &[(20, "new.txt")]);
        live.apply(&[change(40, true)], &f);
        for size in 100..2100u64 {
            f.set(21, file_info(size), &[(20, "a.txt")]);
            f.set(40, file_info(size), &[(20, "new.txt")]);
            let r = live.apply(&[change(21, false), change(40, false)], &f);
            assert_eq!(r.changed, 2);
            assert_eq!(r.rescan, None);
        }
        assert_eq!(live.delta_len(), 2, "one entry for each file, no dead ones");
        assert_eq!(live.delta_alive, 2);
        assert_eq!(live.delta_children.values().map(Vec::len).sum::<usize>(), 2);
        let view = live.view();
        assert_eq!(view.delta.len(), 3, "two entries and the shadow root");
        let q = Query::parse("size:2099").unwrap();
        let hit = ferrow_query::search(
            &view.delta,
            &q,
            &SearchOptions {
                allowed: Some(view.delta_allowed.clone()),
                ..SearchOptions::default()
            },
        );
        assert_eq!(hit.hits.len(), 2, "the latest write is what is searched");
        assert_eq!(
            paths(&base, &view),
            [
                "C:",
                "C:\\Docs",
                "C:\\Docs\\a.txt",
                "C:\\Docs\\new.txt",
                "C:\\b.txt"
            ]
        );
        assert_eq!(live.locations(0, 40).len(), 1);
    }

    #[test]
    fn moving_a_file_still_replaces_its_entry() {
        // Only a change of metadata is done in place; a new place is a new
        // entry (and the old one is dead until the next rescan).
        let (base, map) = fixture();
        let mut live = Live::new(base.clone(), &[map]);
        let mut f = initial();
        f.set(40, file_info(9), &[(20, "new.txt")]);
        live.apply(&[change(40, true)], &f);
        f.set(40, file_info(9), &[(5, "moved.txt")]);
        live.apply(&[change(40, false)], &f);
        assert_eq!(live.delta_len(), 2);
        assert_eq!(live.delta_alive, 1);
        assert_eq!(
            paths(&base, &live.view()),
            [
                "C:",
                "C:\\Docs",
                "C:\\Docs\\a.txt",
                "C:\\b.txt",
                "C:\\moved.txt"
            ]
        );
    }

    #[test]
    fn the_delta_limit_follows_the_index() {
        for (base_len, max) in [
            (0, 50_000),
            (1_000_000, 50_000),
            (5_000_000, 50_000),
            (5_100_000, 51_000),
            (18_600_000, 186_000),
            (25_000_000, 250_000),
            (27_000_000, 250_000),
            (usize::MAX / 2, 250_000),
        ] {
            assert_eq!(max_delta(base_len), max, "base of {base_len}");
            let l = DeltaLimits::for_base(base_len);
            assert_eq!(l.max_delta, max);
            assert_eq!(l.max_touched, max, "the change log follows");
            assert_eq!(l.max_dead, 2 * max);
            assert!(l.hard_delta >= 400_000 && l.hard_delta >= 3 * max, "{l:?}");
            assert!(l.hard_delta <= 750_000, "bounded memory: {l:?}");
        }
        assert_eq!(DeltaLimits::for_base(1_000_000).hard_delta, 400_000);
        assert_eq!(DeltaLimits::for_base(27_000_000).hard_delta, 750_000);
        assert!(walk_limits_text(27_000_000).starts_with("up to 250000 entries"));
        assert!(walk_limits_text(1_000_000).starts_with("up to 50000 entries"));
        let (base, map) = fixture();
        assert_eq!(Live::new(base, &[map]).limits(), DeltaLimits::for_base(4));
    }

    #[test]
    fn a_delta_of_mostly_dead_entries_asks_for_a_rescan() {
        // One file renamed back and forth: one live entry, a growing pile of
        // dead ones. The live count never reaches the limit.
        let (base, map) = fixture();
        let mut live = Live::new(base, &[map]);
        let max_dead = live.limits().max_dead;
        let mut f = initial();
        let mut asked = None;
        for i in 0..=(max_dead + 2) {
            let name = if i % 2 == 0 { "ping.txt" } else { "pong.txt" };
            f.set(40, file_info(9), &[(20, name)]);
            if let Some(why) = live.apply(&[change(40, i == 0)], &f).rescan {
                asked = Some((i, why));
                break;
            }
        }
        let (at, why) = asked.expect("dead entries are bounded");
        assert_eq!(live.delta_alive, 1);
        assert_eq!(at, max_dead + 1, "the first change past the bound: {why}");
        assert_eq!(reason_kind(&why), "too many superseded entries", "{why}");
    }

    #[test]
    fn views_are_published_by_duty_not_per_batch() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut gate = PublishGate::new();
        assert!(!gate.due(t0), "nothing changed");
        assert_eq!(gate.wake_at(), None);
        // The first change after start-up is shown at once.
        gate.changed();
        assert!(gate.due(t0));
        // A view that took 70 ms: the next one no earlier than 700 ms later.
        gate.published(t0, ms(70));
        assert!(!gate.due(t0 + ms(5_000)), "no change owed, nothing to show");
        assert_eq!(gate.wake_at(), None);
        gate.changed();
        assert!(!gate.due(t0 + ms(200)), "the next batch is coalesced");
        assert!(!gate.due(t0 + ms(699)));
        assert_eq!(gate.wake_at(), Some(t0 + ms(700)));
        assert!(gate.due(t0 + ms(700)), "the last batch is published");
        // More changes meanwhile change nothing about the time owed.
        gate.changed();
        gate.changed();
        assert_eq!(gate.wake_at(), Some(t0 + ms(700)));
        // It is published late (the maintainer was busy): the clock restarts
        // at the end of that publish, with the new build time.
        gate.published(t0 + ms(900), ms(40));
        gate.changed();
        assert!(!gate.due(t0 + ms(1_299)));
        assert!(gate.due(t0 + ms(1_300)));
        // A cheap build never holds a change back for longer than a batch…
        gate.published(t0 + ms(1_300), ms(2));
        gate.changed();
        assert!(gate.due(t0 + ms(1_320)));
        // …and a very costly one never for more than the cap.
        gate.published(t0 + ms(2_000), Duration::from_secs(3));
        gate.changed();
        assert_eq!(gate.wake_at(), Some(t0 + ms(2_000) + MAX_PUBLISH_GAP));
        assert!(!gate.due(t0 + ms(6_999)));
        assert!(gate.due(t0 + ms(7_000)));
    }

    #[test]
    fn a_storm_publishes_every_change_eventually_within_the_duty() {
        // A fake clock: a batch every 200 ms for a minute, each view costing
        // 70 ms. Views stay within the duty, and the last change is shown.
        let t0 = Instant::now();
        let mut gate = PublishGate::new();
        let build = Duration::from_millis(70);
        let mut now = t0;
        let mut published_at = Vec::new();
        for _ in 0..300 {
            gate.changed();
            if gate.due(now) {
                now += build;
                gate.published(now, build);
                published_at.push(now);
            }
            now += Duration::from_millis(200);
        }
        // The storm stops with a change still owed: it is published at its
        // time, within one gap, and nothing is owed afterwards.
        let at = gate.wake_at().expect("the last batch is still owed");
        assert!(at <= now + build * PUBLISH_DUTY, "bounded wait");
        assert!(gate.due(at));
        gate.published(at + build, build);
        assert_eq!(gate.wake_at(), None);
        assert!(!gate.due(at + Duration::from_secs(60)));
        let busy = build * (published_at.len() as u32 + 1);
        assert!(
            busy.as_secs_f64() / (now - t0).as_secs_f64() <= 0.11,
            "{} builds of {build:?} in {:?}",
            published_at.len(),
            now - t0
        );
        for pair in published_at.windows(2) {
            assert!(pair[1] - pair[0] >= build * PUBLISH_DUTY, "{pair:?}");
        }
    }

    // ---- compaction -----------------------------------------------------

    /// Every entry a search can return from `base` plus `view`: its path and
    /// metadata, sorted.
    fn listing(base: &Index, view: &LiveView) -> Vec<(String, EntryMetadata)> {
        let norm = |p: std::path::PathBuf| p.to_string_lossy().replace('/', "\\");
        let mut out = Vec::new();
        for id in 0..base.len() as u32 {
            if view.allowed[id as usize / 64] >> (id % 64) & 1 != 0 {
                out.push((norm(base.path(id)), view.base_metadata(base, id)));
            }
        }
        for id in 0..view.delta.len() as u32 {
            if view.delta_allowed[id as usize / 64] >> (id % 64) & 1 != 0 {
                out.push((norm(view.delta.path(id)), view.delta.metadata(id)));
            }
        }
        assert_eq!(out.len(), view.entries, "the view counts what it shows");
        out.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| format!("{:?}", a.1).cmp(&format!("{:?}", b.1)))
        });
        out
    }

    /// The scan of a fake file system: the tree below its root (record 5,
    /// named `C:`), every name of a hard-linked record, and the map of
    /// `(record, id)` a scan returns. `unmapped` entries are added without a
    /// map entry (a scan that has no record for them).
    fn scan(f: &Fake, unmapped: &[&str]) -> (Arc<Index>, VolumeMap) {
        let b = IndexBuilder::with_columns(ColumnSet::DEFAULT);
        let root = b.add_with_metadata(
            ROOT_PARENT,
            b"C:",
            flags::DIR,
            f.0[&5].info.metadata(ColumnSet::DEFAULT),
        );
        let mut entries = vec![(5u64, root)];
        let mut at: HashMap<u64, Vec<u32>> = HashMap::from([(5, vec![root])]);
        let mut queue = std::collections::VecDeque::from([5u64]);
        let mut dirs: HashMap<String, u32> = HashMap::new();
        while let Some(dir) = queue.pop_front() {
            let mut kids: Vec<(u64, Vec<u8>)> =
                f.0.iter()
                    .filter(|(r, _)| **r != dir && **r != 30)
                    .flat_map(|(r, s)| {
                        s.names
                            .iter()
                            .filter(|(p, _)| *p == dir)
                            .map(move |(_, n)| (*r, n.clone()))
                    })
                    .collect();
            kids.sort();
            for (record, name) in kids {
                let state = &f.0[&record];
                for parent in at[&dir].clone() {
                    let id = b.add_with_metadata(
                        parent,
                        &name,
                        state.info.entry_flags(),
                        state.info.metadata(ColumnSet::DEFAULT),
                    );
                    entries.push((record, id));
                    if state.info.is_dir {
                        dirs.insert(format!("{dir}/{}", String::from_utf8_lossy(&name)), id);
                        at.entry(record).or_default().push(id);
                    }
                }
                if state.info.is_dir && !queue.contains(&record) {
                    queue.push_back(record);
                }
            }
        }
        // Unmapped files below the first folders (by `dirs` order, stable).
        let mut dir_ids: Vec<u32> = dirs.values().copied().collect();
        dir_ids.sort();
        for (i, name) in unmapped.iter().enumerate() {
            let parent = dir_ids
                .get(i % dir_ids.len().max(1))
                .copied()
                .unwrap_or(root);
            b.add_with_metadata(
                parent,
                name.as_bytes(),
                0,
                file_info(77).metadata(ColumnSet::DEFAULT),
            );
        }
        let map = VolumeMap {
            entries,
            excluded: vec![30],
            roots: vec![(5, root)],
            ..VolumeMap::default()
        };
        (Arc::new(b.freeze()), map)
    }

    /// A fake file system that changes at random: what the journal would
    /// report, and the state a record fetch answers from.
    struct World {
        fs: Fake,
        next: u64,
        rng: u64,
        tick: i64,
        /// A folder was moved or deleted in the batch being built. Two in one
        /// batch can reverse their nesting (each is applied against the
        /// other's old place), which the live layer answers with a rescan.
        folder_op: bool,
    }

    impl World {
        fn new(seed: u64) -> World {
            let mut w = World {
                fs: Fake::default(),
                next: 100,
                rng: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
                tick: 133_000_000_000_000_000,
                folder_op: false,
            };
            w.fs.set(5, DIR_INFO, &[(5, ".")]);
            for _ in 0..40 {
                w.create();
            }
            w
        }

        fn random(&mut self, n: usize) -> usize {
            self.rng ^= self.rng << 13;
            self.rng ^= self.rng >> 7;
            self.rng ^= self.rng << 17;
            (self.rng % n.max(1) as u64) as usize
        }

        fn records(&self, dirs: Option<bool>) -> Vec<u64> {
            let mut out: Vec<u64> = self
                .fs
                .0
                .iter()
                .filter(|(r, s)| **r != 5 && **r != 30 && dirs.is_none_or(|d| s.info.is_dir == d))
                .map(|(r, _)| *r)
                .collect();
            out.sort();
            out
        }

        fn dir(&mut self) -> u64 {
            let mut dirs = self.records(Some(true));
            dirs.push(5);
            dirs[self.random(dirs.len())]
        }

        /// `dir` and every folder below it.
        fn subtree(&self, dir: u64) -> HashSet<u64> {
            let mut out = HashSet::from([dir]);
            loop {
                let more: Vec<u64> = self
                    .fs
                    .0
                    .iter()
                    .filter(|(r, s)| {
                        s.info.is_dir
                            && !out.contains(r)
                            && s.names.iter().any(|(p, _)| out.contains(p))
                    })
                    .map(|(r, _)| *r)
                    .collect();
                if more.is_empty() {
                    return out;
                }
                out.extend(more);
            }
        }

        fn name(&mut self) -> String {
            // Few names: collisions in one folder and the same name in
            // several folders both happen.
            ["a", "B", "c.txt", "Report", "x.log", "ÄÖ"][self.random(6)].to_string()
                + &self.random(5).to_string()
        }

        fn info(&mut self, dir: bool) -> RecordInfo {
            self.tick += 10_000_000 * (1 + self.random(5) as i64);
            let base = if dir {
                DIR_INFO
            } else {
                file_info(self.random(1000) as u64)
            };
            RecordInfo {
                modified: self.tick,
                created: self.tick - 5 * 10_000_000,
                ..base
            }
        }

        fn create(&mut self) -> Change {
            let record = self.next;
            self.next += 1;
            let dir = self.random(4) == 0;
            let parent = self.dir();
            let name = self.name();
            let info = self.info(dir);
            self.fs.set(record, info, &[(parent, &name)]);
            change(record, true)
        }

        /// One random step: the records that changed.
        fn step(&mut self) -> Vec<Change> {
            let files = self.records(Some(false));
            let dirs = self.records(Some(true));
            let pick = |w: &mut World, of: &[u64]| of.get(w.random(of.len())).copied();
            match self.random(14) {
                0..=2 => vec![self.create()],
                3 | 4 => match pick(self, &files) {
                    // A file written: new size and dates, same place.
                    Some(r) => {
                        let names = self.fs.0[&r].names.clone();
                        let info = self.info(false);
                        self.fs.0.insert(r, RecordState { info, names });
                        vec![change(r, false)]
                    }
                    None => vec![],
                },
                5 | 6 => match pick(self, &files) {
                    // Renamed or moved: its first name changes.
                    Some(r) => {
                        let (parent, name) = (self.dir(), self.name());
                        let state = self.fs.0.get_mut(&r).unwrap();
                        state.names[0] = (parent, name.into_bytes());
                        vec![change(r, false)]
                    }
                    None => vec![],
                },
                7 | 8 | 10 if std::mem::replace(&mut self.folder_op, true) => vec![],
                7 | 8 => match pick(self, &dirs) {
                    // A folder renamed or moved, not into itself.
                    Some(r) => {
                        let (parent, name) = (self.dir(), self.name());
                        if self.subtree(r).contains(&parent) {
                            return vec![];
                        }
                        let state = self.fs.0.get_mut(&r).unwrap();
                        state.names = vec![(parent, name.into_bytes())];
                        vec![change(r, false)]
                    }
                    None => vec![],
                },
                9 => match pick(self, &files) {
                    Some(r) => {
                        self.fs.0.remove(&r);
                        vec![change(r, false)]
                    }
                    None => vec![],
                },
                10 => match pick(self, &dirs) {
                    // A folder deleted with everything in it (only the
                    // folder is reported; the rest follows from it).
                    Some(r) => {
                        let gone = self.subtree(r);
                        let mut changed = vec![change(r, false)];
                        let records: Vec<u64> = self.fs.0.keys().copied().collect();
                        for record in records {
                            let state = self.fs.0.get_mut(&record).unwrap();
                            let before = state.names.len();
                            state.names.retain(|(p, _)| !gone.contains(p));
                            if state.names.is_empty() || gone.contains(&record) {
                                self.fs.0.remove(&record);
                            } else if state.names.len() != before {
                                changed.push(change(record, false));
                            }
                        }
                        changed
                    }
                    None => vec![],
                },
                11 => match pick(self, &files) {
                    // A hard link: a second name for a file.
                    Some(r) => {
                        let (parent, name) = (self.dir(), self.name());
                        let state = self.fs.0.get_mut(&r).unwrap();
                        state.names.push((parent, name.into_bytes()));
                        vec![change(r, false)]
                    }
                    None => vec![],
                },
                12 => match pick(self, &files) {
                    // One name of a hard-linked file removed.
                    Some(r) if self.fs.0[&r].names.len() > 1 => {
                        self.fs.0.get_mut(&r).unwrap().names.pop();
                        vec![change(r, false)]
                    }
                    _ => vec![],
                },
                _ => {
                    // A folder's own dates change (the created file's effect
                    // on its parent), a root's too, and an excluded record
                    // appears.
                    let r = if self.random(3) == 0 { 5 } else { self.dir() };
                    let state = self.fs.0.get_mut(&r).unwrap();
                    state.info.modified += 10_000_000 * 3;
                    let mut out = vec![change(r, false)];
                    if self.random(4) == 0 {
                        let parent = self.dir();
                        self.fs.set(30, file_info(1), &[(parent, "excluded.bin")]);
                        out.push(change(30, true));
                    }
                    out
                }
            }
        }
    }

    /// Random batches of steps, applied to every live layer in `lives` the
    /// way the maintainer does: a folder the journal reports without an entry
    /// (it moved while its parent also changed) is read by the walker, which
    /// is mocked here, and merged. None may ask for a rescan.
    fn drive(w: &mut World, lives: &mut [&mut Live], batches: usize) {
        for _ in 0..batches {
            let mut changes: Vec<Change> = Vec::new();
            w.folder_op = false;
            for _ in 0..1 + w.random(6) {
                changes.extend(w.step());
            }
            changes.sort_by_key(|c| (c.volume, c.record));
            changes.dedup_by_key(|c| (c.volume, c.record));
            for live in lives.iter_mut() {
                let mut r = live.apply(&changes, &w.fs);
                while let Some(job) = r.walks.pop() {
                    let listing = walk_fake(&w.fs, &job);
                    r.absorb(live.finish_walk(job.id, Ok(listing.into()), &w.fs));
                }
                assert!(r.rescan.is_none(), "{r:?}");
                assert!(!live.walks_outstanding());
            }
        }
    }

    #[test]
    fn a_compacted_index_shows_exactly_what_the_live_one_showed() {
        for seed in 1..=24u64 {
            let mut w = World::new(seed);
            // Even seeds: a scan that has no record for two of its entries.
            let unmapped: &[&str] = if seed % 2 == 0 {
                &["unmapped-1.txt", "unmapped-2.txt"]
            } else {
                &[]
            };
            let (base, map) = scan(&w.fs, unmapped);
            let mut old = Live::new(base.clone(), &[map]);
            drive(&mut w, &mut [&mut old], 25);
            let shown = listing(&base, &old.view());
            let mut new = old.compact().expect("no folder is being read");
            let nbase = new.base().clone();
            let nview = new.view();
            assert_eq!(listing(&nbase, &nview), shown, "seed {seed}");
            assert_eq!(nview.delta.len(), 0, "nothing left in a delta");
            assert_eq!(nview.entries, nbase.len());
            assert_eq!(new.delta_len(), 0);
            assert_eq!(new.removed_count, 0);
            if unmapped.is_empty() {
                // Nothing a fresh scan would not list: it agrees too.
                let (fresh, _) = scan(&w.fs, &[]);
                let none = LiveView {
                    allowed: Arc::new(vec![u64::MAX; fresh.len().div_ceil(64)]),
                    delta: Arc::new(IndexBuilder::new().freeze()),
                    delta_allowed: Arc::new(Vec::new()),
                    overrides: Arc::new(HashMap::new()),
                    entries: fresh.len(),
                };
                assert_eq!(listing(&fresh, &none), shown, "fresh scan, seed {seed}");
            }
            // The same changes now give the same result on both: the new
            // layer tracks every record (hard links, folders moved, roots).
            for round in 0..4 {
                drive(&mut w, &mut [&mut old, &mut new], 8);
                assert_eq!(
                    listing(&nbase, &new.view()),
                    listing(&base, &old.view()),
                    "seed {seed}, round {round}"
                );
            }
            // And a second compaction of the changed layer loses nothing.
            let shown = listing(&nbase, &new.view());
            let mut third = new.compact().unwrap();
            assert_eq!(
                listing(third.base(), &third.view()),
                shown,
                "seed {seed}, again"
            );
            drive(&mut w, &mut [&mut new, &mut third], 5);
            assert_eq!(
                listing(third.base(), &third.view()),
                listing(&nbase, &new.view()),
                "seed {seed}, after the second"
            );
        }
    }

    #[test]
    fn compaction_keeps_hard_links_folder_dates_and_unmapped_entries() {
        let (base, map) = fixture();
        let mut live = Live::new(base.clone(), &[map]);
        let mut f = initial();
        // A hard link, a folder's dates, a root's dates, a new folder.
        f.set(21, file_info(1), &[(20, "a.txt"), (5, "a-link.txt")]);
        f.set(
            20,
            RecordInfo {
                modified: 133_100_000_000_000_000,
                ..DIR_INFO
            },
            &[(5, "Docs")],
        );
        f.set(
            5,
            RecordInfo {
                modified: 133_200_000_000_000_000,
                ..DIR_INFO
            },
            &[(5, ".")],
        );
        f.set(50, DIR_INFO, &[(5, "NewDir")]);
        f.set(51, file_info(4), &[(50, "inside.txt")]);
        let r = live.apply(
            &[
                change(21, false),
                change(20, false),
                change(5, false),
                change(51, true),
                change(50, true),
            ],
            &f,
        );
        assert!(r.rescan.is_none() && r.walks.is_empty(), "{r:?}");
        let shown = listing(&base, &live.view());
        let mut new = live.compact().unwrap();
        let nbase = new.base().clone();
        assert_eq!(listing(&nbase, &new.view()), shown);
        // Folder dates are folded into the new columns, not kept as overrides.
        assert!(new.overrides.is_empty());
        let docs = (0..nbase.len() as u32)
            .find(|&i| nbase.name(i) == b"Docs")
            .unwrap();
        assert_eq!(
            nbase.metadata(docs),
            RecordInfo {
                modified: 133_100_000_000_000_000,
                ..DIR_INFO
            }
            .metadata(ColumnSet::DEFAULT)
        );
        // Both names of the hard link are entries of the one record.
        let record_21 = new.locations(0, 21);
        assert_eq!(record_21.len(), 2, "{record_21:?}");
        // Removing one name leaves the other, as it would without a compaction.
        f.set(21, file_info(1), &[(20, "a.txt")]);
        new.apply(&[change(21, false)], &f);
        live.apply(&[change(21, false)], &f);
        assert_eq!(listing(&nbase, &new.view()), listing(&base, &live.view()));
        // The root's entry still only follows its dates, and an excluded
        // record is still never indexed.
        assert!(new.volumes[0].roots.contains(&5));
        assert!(new.volumes[0].excluded.contains(&30));
        f.set(30, file_info(1), &[(5, "excluded.txt")]);
        f.set(
            5,
            RecordInfo {
                modified: 133_300_000_000_000_000,
                ..DIR_INFO
            },
            &[(5, ".")],
        );
        let r = new.apply(&[change(30, true), change(5, false)], &f);
        assert_eq!(r.changed, 1, "only the root's dates: {r:?}");
        assert!(
            !listing(&nbase, &new.view())
                .iter()
                .any(|(p, _)| p.contains("excluded")),
            "excluded stays excluded"
        );
    }

    #[test]
    fn compaction_keeps_each_volumes_records_apart() {
        // Two volumes with the same record numbers.
        let b = IndexBuilder::with_columns(ColumnSet::DEFAULT);
        let c = b.add_root(b"C:", flags::DIR);
        let d = b.add_root(b"D:", flags::DIR);
        let md = file_info(1).metadata(ColumnSet::DEFAULT);
        let on_c = b.add_with_metadata(c, b"same.txt", 0, md);
        let on_d = b.add_with_metadata(d, b"same.txt", 0, md);
        let maps = [
            VolumeMap {
                entries: vec![(5, c), (21, on_c)],
                excluded: vec![30],
                roots: vec![(5, c)],
                ..VolumeMap::default()
            },
            VolumeMap {
                entries: vec![(5, d), (21, on_d)],
                excluded: vec![31],
                roots: vec![(5, d)],
                ..VolumeMap::default()
            },
        ];
        struct Two(Vec<Fake>);
        impl RecordSource for Two {
            fn fetch(&self, volume: usize, record: u64) -> Option<RecordState> {
                self.0[volume].0.get(&record).cloned()
            }
        }
        let mut fs = Two(vec![Fake::default(), Fake::default()]);
        for fake in &mut fs.0 {
            fake.set(5, DIR_INFO, &[(5, ".")]);
            fake.set(21, file_info(1), &[(5, "same.txt")]);
        }
        let mut live = Live::new(Arc::new(b.freeze()), &maps);
        // A change on D: only D's file is replaced.
        fs.0[1].set(21, file_info(900), &[(5, "same.txt")]);
        live.apply(
            &[Change {
                volume: 1,
                record: 21,
                created: false,
            }],
            &fs,
        );
        let mut new = live.compact().unwrap();
        let nbase = new.base().clone();
        let shown = listing(&nbase, &new.view());
        let sizes: Vec<(&str, Option<u64>)> =
            shown.iter().map(|(p, m)| (p.as_str(), m.size)).collect();
        assert_eq!(
            sizes,
            [
                ("C:", None),
                ("C:\\same.txt", Some(1)),
                ("D:", None),
                ("D:\\same.txt", Some(900)),
            ]
        );
        assert!(new.volumes[0].excluded.contains(&30) && !new.volumes[0].excluded.contains(&31));
        assert!(new.volumes[1].excluded.contains(&31));
        // Record 21 of volume 0 and of volume 1 are still different entries.
        fs.0[0].set(21, file_info(5), &[(5, "same.txt")]);
        new.apply(
            &[Change {
                volume: 0,
                record: 21,
                created: false,
            }],
            &fs,
        );
        let sizes: Vec<Option<u64>> = listing(&nbase, &new.view())
            .iter()
            .map(|(_, m)| m.size)
            .collect();
        assert_eq!(sizes, [None, Some(5), None, Some(900)]);
    }

    #[test]
    fn compaction_frees_what_a_folder_move_built_and_the_delta() {
        let (base, map) = fixture();
        let mut live = Live::new(base, &[map]);
        let mut f = initial();
        f.set(20, DIR_INFO, &[(5, "Papers")]);
        for i in 0..200u64 {
            f.set(500 + i, file_info(1), &[(20, &format!("n{i}.txt"))]);
        }
        let changes: Vec<Change> = (0..200u64)
            .map(|i| change(500 + i, true))
            .chain([change(20, false)])
            .collect();
        live.apply(&changes, &f);
        let bytes = |live: &Live, name: &str| {
            live.memory_parts()
                .into_iter()
                .find(|p| p.name == name)
                .map_or(0, |p| p.bytes)
        };
        assert!(bytes(&live, "live.base_children") > 0, "built by the move");
        assert!(bytes(&live, "live.delta") > 0);
        let new = live.compact().unwrap();
        assert_eq!(bytes(&live, "live.base_children"), 0, "freed first");
        assert_eq!(bytes(&new, "live.base_children"), 0);
        assert_eq!(bytes(&new, "live.delta"), 0, "no delta any more");
        assert_eq!(bytes(&new, "live.delta_children"), 0);
        assert_eq!(new.base().len(), 4 + 200);
        assert_eq!(new.limits(), DeltaLimits::for_base(204));
    }

    #[test]
    fn compaction_waits_for_folder_reads() {
        let f = moved_in();
        let (mut live, base, job) = after_move_in(&f);
        assert!(live.walks_outstanding());
        assert!(live.compact().is_none(), "a read is outstanding");
        // Nothing is published and nothing changes.
        let store = crate::SnapshotStore::new(crate::Snapshot {
            index: base.clone(),
            live: Some(Arc::new(live.view())),
            mode: "live".into(),
            report: String::new(),
        });
        let built = std::cell::Cell::new(false);
        let took = compact_and_publish(
            &mut live,
            &store,
            "r".into(),
            |live| {
                built.set(true);
                live.compact()
            },
            |_, _| panic!("nothing was compacted"),
        );
        assert!(took.is_none() && built.get());
        assert!(Arc::ptr_eq(&store.get().index, &base), "no swap");
        assert_eq!(shown(&live, &base).len(), 5);
        // Once the read has been merged it folds, including what it read,
        // and the next read still gets a new id.
        let done = live.finish_walk(job.id, Ok(walk_fake(&f, &job).into()), &f);
        assert_eq!(done.rescan, None);
        assert!(!live.walks_outstanding());
        let mut new = live.compact().expect("no read outstanding");
        assert_eq!(
            listing(new.base(), &new.view())
                .iter()
                .map(|(p, _)| p.clone())
                .collect::<Vec<_>>(),
            oracle(&f)
        );
        let mut f = f;
        f.set(70, DIR_INFO, &[(5, "Another")]);
        let r = new.apply(&[change(70, false)], &f);
        assert_eq!(r.walks.len(), 1);
        assert_eq!(r.walks[0].id, job.id + 1, "ids never repeat");
    }

    /// The text of every path a query finds, through the service's handler.
    fn found(store: &crate::SnapshotStore, text: &str) -> Vec<String> {
        let answers = crate::handle(
            store,
            ferrow_ipc::Request::Query {
                text: text.into(),
                limit: 100,
                sort: ferrow_ipc::Sort::Name,
                flags: 0,
            },
        );
        let mut out = Vec::new();
        for answer in answers {
            match answer {
                ferrow_ipc::Response::Results { paths, .. } => out.extend(
                    paths
                        .into_iter()
                        .map(|p| String::from_utf8_lossy(&p).replace('/', "\\")),
                ),
                other => panic!("unexpected {other:?}"),
            }
        }
        out.sort();
        out
    }

    #[test]
    fn a_search_never_waits_for_a_compaction_and_the_old_snapshot_stays_until_the_swap() {
        use std::sync::mpsc::{RecvTimeoutError, channel};
        let (base, map) = fixture();
        let mut live = Live::new(base.clone(), &[map]);
        let mut f = initial();
        f.set(40, file_info(9), &[(20, "fresh.txt")]);
        f.0.remove(&22);
        live.apply(&[change(40, true), change(22, false)], &f);
        let store = crate::SnapshotStore::new(crate::Snapshot {
            index: base.clone(),
            live: Some(Arc::new(live.view())),
            mode: "live".into(),
            report: "before".into(),
        });
        let weak_old = Arc::downgrade(&base);
        // A search that began before the compaction and is still running.
        let in_flight = store.get();
        let (started_tx, started_rx) = channel::<()>();
        let (go_tx, go_rx) = channel::<()>();
        let (answer_tx, answer_rx) = channel();
        std::thread::scope(|scope| {
            // The compaction, with a stage that stays slow until released.
            let (live_ref, store_ref) = (&mut live, &store);
            let compaction = scope.spawn(move || {
                compact_and_publish(
                    live_ref,
                    store_ref,
                    "after".into(),
                    |live| {
                        started_tx.send(()).unwrap();
                        go_rx.recv().unwrap();
                        live.compact()
                    },
                    |_, _| {},
                )
            });
            started_rx.recv().unwrap();
            // Mid-compaction: searches are answered, from the old snapshot
            // (its delta included), and are not held up.
            let searcher = scope.spawn(|| {
                let text = found(&store, "txt");
                let seen = store.get();
                answer_tx
                    .send((text, Arc::ptr_eq(&seen.index, &base), seen.report.clone()))
                    .ok();
            });
            let answered = answer_rx.recv_timeout(Duration::from_secs(20));
            // Release the compaction whatever happened above.
            go_tx.send(()).unwrap();
            match answered {
                Ok((hits, old_index, report)) => {
                    assert_eq!(
                        hits,
                        ["C:\\Docs\\a.txt", "C:\\Docs\\fresh.txt"],
                        "the live state of the old snapshot"
                    );
                    assert!(old_index, "the old index is still the published one");
                    assert_eq!(report, "before");
                }
                Err(RecvTimeoutError::Timeout) => panic!("a search waited for the compaction"),
                Err(e) => panic!("{e:?}"),
            }
            searcher.join().unwrap();
            assert!(compaction.join().unwrap().is_some());
        });
        // After the swap: the new index answers the same, with no delta.
        let now = store.get();
        assert_eq!(now.report, "after");
        assert!(now.live.is_none());
        assert!(!Arc::ptr_eq(&now.index, &base));
        assert_eq!(now.entries(), 4);
        assert_eq!(
            found(&store, "txt"),
            ["C:\\Docs\\a.txt", "C:\\Docs\\fresh.txt"]
        );
        // The search that was running keeps the old snapshot whole.
        assert!(Arc::ptr_eq(&in_flight.index, &base));
        assert_eq!(in_flight.entries(), 4);
        assert_eq!(in_flight.index.len(), 4);
        assert!(weak_old.upgrade().is_some(), "held by the running search");
        // The old index is freed when the last holder lets go.
        drop(in_flight);
        drop(now);
        drop(base);
        assert!(weak_old.upgrade().is_none(), "the old index is freed");
    }

    #[test]
    fn compactions_are_rate_limited_by_their_own_short_gap() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let calm = Load::default();
        let urgent = Load::of(60, 50, false, false);
        let walking = Load {
            walks: true,
            ..calm
        };
        // After a 3-minute scan a rescan waits 27 minutes; a compaction
        // only a minute.
        let mut m = Maintenance::after_scan(t0, s(180));
        assert!(
            m.request(Reason::compact("50001 live changes")),
            "starts an episode"
        );
        assert!(!m.request(Reason::compact("50002 live changes")));
        assert_eq!(m.rescan_pending(), None, "a compaction is not a rescan");
        assert_eq!(m.wake_at(t0, calm), Some(t0 + MIN_COMPACT_GAP));
        assert_eq!(m.due(t0 + s(59), calm), None);
        assert_eq!(
            m.due(t0 + s(60), calm),
            Some(Due::Compact("50001 live changes".into())),
            "the first reason is kept"
        );
        assert_eq!(m.due(t0 + s(61), calm), None, "taken");
        // A compaction that took 30 s: the next waits 5 times that.
        let end = t0 + s(100);
        m.compacted(end, s(30));
        assert!(m.request(Reason::compact("60000 live changes")));
        assert_eq!(m.wake_at(end, calm), Some(end + s(150)));
        assert_eq!(m.due(end + s(149), calm), None);
        assert!(matches!(m.due(end + s(150), calm), Some(Due::Compact(_))));
        // A quick one still waits the minimum, a very slow one at most ten minutes.
        m.compacted(end, Duration::from_millis(300));
        m.request(Reason::compact("x live changes"));
        assert_eq!(m.wake_at(end, calm), Some(end + MIN_COMPACT_GAP));
        m.compacted(end, s(3600));
        assert_eq!(m.wake_at(end, calm), Some(end + MAX_COMPACT_GAP));
        // A delta past its hard limit does not wait for the gap.
        assert!(matches!(m.due(end + s(1), urgent), Some(Due::Compact(_))));
        // Folders being read hold it back without a timer of its own (the
        // read's answer wakes the maintainer)…
        m.due(end + s(2), calm);
        m.request(Reason::compact("y live changes"));
        assert_eq!(m.wake_at(end, walking), None);
        assert_eq!(m.due(end + s(3600), walking), None);
        assert!(matches!(m.due(end + s(3600), calm), Some(Due::Compact(_))));
    }

    #[test]
    fn the_hard_limit_stays_a_cap_while_folder_reads_keep_arriving() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let hard = 100;
        let walking = |delta| Load::of(delta, hard, true, false);
        let mut m = Maintenance::after_scan(t0, s(10));
        m.request(Reason::compact("101 live changes"));
        // Just past the limit with a read outstanding: it waits for the read,
        // and wakes when its patience ends (no timer for the read itself).
        assert_eq!(m.due(t0 + s(100), walking(101)), None);
        let patience = t0 + s(100) + URGENT_PATIENCE;
        assert_eq!(m.wake_at(t0 + s(100), walking(101)), Some(patience));
        assert_eq!(
            m.due(t0 + s(100) + URGENT_PATIENCE - s(1), walking(150)),
            None,
            "still within its patience"
        );
        // Reads kept arriving for as long as a read can last: rescan, as
        // before compactions existed.
        assert_eq!(
            m.due(patience, walking(150)),
            Some(Due::Rescan("101 live changes".into()))
        );
        // Back under the limit, the clock starts again.
        let mut m = Maintenance::after_scan(t0, s(10));
        m.request(Reason::compact("101 live changes"));
        assert_eq!(m.due(t0 + s(100), walking(101)), None);
        assert_eq!(m.due(t0 + s(110), walking(99)), None);
        assert_eq!(m.due(patience + s(5), walking(101)), None, "restarted");
        // Twice the limit does not wait at all.
        assert_eq!(
            m.due(patience + s(5), walking(2 * hard + 1)),
            Some(Due::Rescan("101 live changes".into()))
        );
        // Not urgent and walking: only the walk's answer matters, however long.
        let mut m = Maintenance::after_scan(t0, s(10));
        m.request(Reason::compact("90 live changes"));
        assert_eq!(m.due(t0 + s(3600), walking(90)), None);
        // A burst that queues walks without end on a live layer: the delta
        // never gets past twice the limit unanswered.
        let mut m = Maintenance::after_scan(t0, s(10));
        m.request(Reason::compact("101 live changes"));
        let mut answered_at = None;
        for second in 0..=300u64 {
            let delta = 101 + second as usize * 2;
            if let Some(due) = m.due(t0 + s(100 + second), walking(delta)) {
                answered_at = Some((second, delta, due));
                break;
            }
        }
        let (second, delta, due) = answered_at.expect("answered");
        assert!(matches!(due, Due::Rescan(_)));
        assert!(
            delta <= 2 * hard + 1 && second <= URGENT_PATIENCE.as_secs(),
            "{second} s, {delta} entries"
        );
    }

    #[test]
    fn a_compaction_waits_for_the_index_save_but_not_past_the_cap() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let hard = 100;
        let saving = |delta| Load::of(delta, hard, false, true);
        let mut m = Maintenance::after_scan(t0, s(10));
        m.request(Reason::compact("60 live changes"));
        // Its time has come, but the saver holds the old index: look again
        // soon, not at once and not never.
        assert_eq!(m.due(t0 + s(60), saving(60)), None);
        assert_eq!(
            m.wake_at(t0 + s(60), saving(60)),
            Some(t0 + s(60) + SAVE_POLL)
        );
        assert_eq!(
            m.due(t0 + s(3600), saving(60)),
            None,
            "however long it takes"
        );
        // The save ended.
        assert_eq!(
            m.due(t0 + s(3601), Load::of(60, hard, false, false)),
            Some(Due::Compact("60 live changes".into()))
        );
        // Past the hard limit the save is waited for only so long…
        let mut m = Maintenance::after_scan(t0, s(10));
        m.request(Reason::compact("101 live changes"));
        assert_eq!(m.due(t0 + s(100), saving(101)), None);
        assert_eq!(
            m.wake_at(t0 + s(100), saving(101)),
            Some((t0 + s(100) + SAVE_POLL).min(t0 + s(100) + URGENT_PATIENCE))
        );
        assert_eq!(
            m.due(t0 + s(100) + URGENT_PATIENCE, saving(120)),
            Some(Due::Compact("101 live changes".into()))
        );
        // …and twice the limit does not wait.
        let mut m = Maintenance::after_scan(t0, s(10));
        m.request(Reason::compact("201 live changes"));
        assert_eq!(
            m.due(t0 + s(100), saving(2 * hard + 1)),
            Some(Due::Compact("201 live changes".into()))
        );
    }

    #[test]
    fn what_answers_a_reason_is_carried_with_it_never_read_from_its_text() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let calm = Load::default();
        // A journal reason that happens to end like a size reason is still a
        // rescan, and a size reason with an unexpected text is still a
        // compaction.
        let mut m = Maintenance::after_scan(t0, s(10));
        assert!(m.request(Reason::disk("Volume{1}: 5 live changes")));
        assert_eq!(
            m.rescan_pending().map(|(why, _)| why),
            Some("Volume{1}: 5 live changes")
        );
        let mut m = Maintenance::after_scan(t0, s(10));
        assert!(m.request(Reason::compact("anything at all")));
        assert_eq!(m.rescan_pending(), None);
        assert_eq!(
            m.due(t0 + s(60), calm),
            Some(Due::Compact("anything at all".into()))
        );
        // The reasons the live layer itself raises carry the right kind.
        let (base, map) = fixture();
        let mut live = Live::new(base, &[map]);
        live.limits = DeltaLimits {
            max_delta: 0,
            ..live.limits
        };
        let mut f = initial();
        f.set(40, file_info(9), &[(20, "fresh.txt")]);
        let r = live.apply(&[change(40, true)], &f);
        let why = r.rescan.expect("past the limit");
        assert_eq!((why.kind(), &*why), (ReasonKind::Compact, "1 live changes"));
        let mut r = Applied::default();
        r.ask(Reason::disk(REASON_LARGE_MOVE));
        assert_eq!(r.rescan.as_ref().map(Reason::kind), Some(ReasonKind::Disk));

        // A real rescan covers a compaction, and is shown as pending.
        let mut m = Maintenance::after_scan(t0, s(10));
        m.request(Reason::compact("50001 live changes"));
        assert!(m.request(Reason::disk("Volume{1}: change journal reset")));
        assert_eq!(
            m.rescan_pending().map(|(why, _)| why),
            Some("Volume{1}: change journal reset")
        );
        // A 10 s scan: the rescan waits 9 times that.
        assert_eq!(m.wake_at(t0, calm), Some(t0 + s(90)));
        assert_eq!(m.due(t0 + s(60), calm), None);
        assert_eq!(
            m.due(t0 + s(90), calm),
            Some(Due::Rescan("Volume{1}: change journal reset".into()))
        );
        // An administrator's request is not held back, and replaces any wait.
        let mut m = Maintenance::after_scan(t0, s(10));
        m.request(Reason::compact("50001 live changes"));
        m.force(t0 + s(5), REASON_ADMIN.into());
        assert_eq!(
            m.due(t0 + s(5), calm),
            Some(Due::Rescan(REASON_ADMIN.into()))
        );
        // Within one batch, the reason that needs the disks wins over the
        // size of the delta, whichever came first.
        let mut applied = Applied::default();
        applied.ask(Reason::compact("50001 live changes"));
        applied.ask(Reason::disk(REASON_LARGE_MOVE));
        applied.ask(Reason::compact("60000 replaced entries"));
        assert_eq!(applied.rescan.as_deref(), Some(REASON_LARGE_MOVE));
        assert_eq!(
            applied.rescan.as_ref().map(Reason::kind),
            Some(ReasonKind::Disk)
        );
        let mut first = Applied::default();
        first.ask(Reason::compact("50001 live changes"));
        let mut second = Applied::default();
        second.ask(Reason::disk(REASON_MOVED_IN));
        first.absorb(second);
        assert_eq!(first.rescan.as_deref(), Some(REASON_MOVED_IN));
        let mut kept = Applied::default();
        kept.ask(Reason::compact("1 live changes"));
        kept.ask(Reason::compact("2 replaced entries"));
        assert_eq!(kept.rescan.as_deref(), Some("1 live changes"));
    }

    #[test]
    fn the_new_index_sorts_by_metadata_like_a_fresh_one() {
        use ferrow_index::{SortKey, SortStatus};
        let mut w = World::new(7);
        let (base, map) = scan(&w.fs, &[]);
        let mut live = Live::new(base, &[map]);
        drive(&mut w, &mut [&mut live], 15);
        let new = live.compact().unwrap();
        let index = new.base();
        // The sort orders are lazy: empty after a compaction, built when asked.
        assert!(
            index_parts(index, "index")
                .iter()
                .filter(|p| p.name.ends_with(".order"))
                .all(|p| p.bytes == 0)
        );
        assert_ne!(index.request_sort(SortKey::Size), SortStatus::Unavailable);
        let started = Instant::now();
        while index.request_sort(SortKey::Size) != SortStatus::Ready {
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "sort never ready"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let order = index.metadata_order(SortKey::Size, false).unwrap();
        let sizes: Vec<Option<u64>> = order
            .range(0, order.len())
            .map(|id| index.metadata(id).size)
            .collect();
        assert_eq!(sizes.len(), index.len());
        let mut sorted = sizes.clone();
        sorted.sort();
        assert_eq!(sizes, sorted, "ascending by size");
        // Name order: the sorted permutation is by folded name.
        let names: Vec<Vec<u8>> = index
            .sorted()
            .iter()
            .map(|&id| index.folded_name(id).to_vec())
            .collect();
        let mut by_name = names.clone();
        by_name.sort();
        assert_eq!(names, by_name);
    }

    #[test]
    fn compaction_drops_what_hangs_below_a_hidden_folder_whatever_the_id_order() {
        // Nothing orders ids by depth: here a file has a lower id than its
        // folder, and that folder a lower id than the one that is hidden.
        let b = IndexBuilder::with_columns(ColumnSet::DEFAULT);
        let root = b.add_root(b"C:", flags::DIR); // 0
        let meta = DIR_INFO.metadata(ColumnSet::DEFAULT);
        let leaf = b.add_with_metadata(2, b"leaf.txt", 0, meta); // 1, parent 2
        let mid = b.add_with_metadata(3, b"mid", flags::DIR, meta); // 2, parent 3
        let top = b.add_with_metadata(root, b"top", flags::DIR, meta); // 3
        let keep = b.add_with_metadata(root, b"keep.txt", 0, meta); // 4
        assert_eq!((leaf, mid, top, keep), (1, 2, 3, 4));
        let map = VolumeMap {
            entries: vec![(5, root), (21, leaf), (22, mid), (20, top), (23, keep)],
            excluded: vec![],
            roots: vec![(5, root)],
            ..VolumeMap::default()
        };
        let mut live = Live::new(Arc::new(b.freeze()), &[map]);
        live.remove(Loc::Base(top));
        let new = live.compact().unwrap();
        let index = new.base();
        let names: Vec<String> = (0..index.len() as u32)
            .map(|id| String::from_utf8_lossy(index.name(id)).into_owned())
            .collect();
        assert_eq!(names, ["C:", "keep.txt"], "nothing below `top` survives");
        for id in 0..index.len() as u32 {
            let parent = index.parent(id);
            assert!(
                parent == ROOT_PARENT || (parent as usize) < index.len(),
                "{id}: parent {parent}"
            );
        }
        assert_eq!(index.parent(1), 0);
    }

    #[test]
    fn compaction_asks_the_new_index_for_the_sort_orders_the_old_one_had_ready() {
        use ferrow_index::SortStatus;
        let mut w = World::new(11);
        let (base, map) = scan(&w.fs, &[]);
        let mut live = Live::new(base.clone(), &[map]);
        drive(&mut w, &mut [&mut live], 10);
        let wait = |index: &Index, key: SortKey| {
            let until = Instant::now() + Duration::from_secs(20);
            while index.request_sort(key) != SortStatus::Ready {
                assert!(Instant::now() < until, "{key:?} never ready");
                std::thread::sleep(Duration::from_millis(2));
            }
        };
        // Only the size order was ever used on the old index.
        assert!(ready_sorts(&base).is_empty());
        wait(&base, SortKey::Size);
        assert_eq!(ready_sorts(&base), [SortKey::Size]);
        let store = crate::SnapshotStore::new(crate::Snapshot {
            index: base.clone(),
            live: Some(Arc::new(live.view())),
            mode: "live".into(),
            report: String::new(),
        });
        let warmed = std::cell::RefCell::new(Vec::new());
        let started = std::sync::Arc::new(AtomicBool::new(false));
        let flag = started.clone();
        compact_and_publish(
            &mut live,
            &store,
            "r".into(),
            |live| live.compact(),
            |index, key| {
                let flag = flag.clone();
                warmed.borrow_mut().push(key);
                let status = index.request_sort_starting(key, move || {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst)
                });
                assert_ne!(status, SortStatus::Unavailable);
            },
        )
        .expect("compacted");
        assert_eq!(*warmed.borrow(), [SortKey::Size], "only what was ready");
        // The published index prepares it in the background, and the start
        // hook ran on the preparing thread.
        let new = store.get().index.clone();
        assert!(!Arc::ptr_eq(&new, &base));
        wait(&new, SortKey::Size);
        assert!(started.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            new.metadata_order(SortKey::Modified, false).is_none(),
            "an order nobody used is not built"
        );
    }

    /// root(5)/Docs(20)/{a.txt(21), Sub(23)/{c.txt(24)}}, root/b.txt(22),
    /// with a folder move budget of one entry: any move of Docs is large.
    fn large_move_fixture() -> (Live, Arc<Index>, Fake) {
        let b = IndexBuilder::with_columns(ColumnSet::DEFAULT);
        let dir = DIR_INFO.metadata(ColumnSet::DEFAULT);
        let root = b.add_root(b"C:", flags::DIR);
        let docs = b.add_with_metadata(root, b"Docs", flags::DIR, dir);
        let a = b.add_with_metadata(docs, b"a.txt", 0, file_info(1).metadata(ColumnSet::DEFAULT));
        let bb = b.add_with_metadata(root, b"b.txt", 0, file_info(2).metadata(ColumnSet::DEFAULT));
        let sub = b.add_with_metadata(docs, b"Sub", flags::DIR, dir);
        let c = b.add_with_metadata(sub, b"c.txt", 0, file_info(3).metadata(ColumnSet::DEFAULT));
        let map = VolumeMap {
            entries: vec![(5, root), (20, docs), (21, a), (22, bb), (23, sub), (24, c)],
            excluded: vec![30],
            roots: vec![(5, root)],
            ..VolumeMap::default()
        };
        let base = Arc::new(b.freeze());
        let mut live = Live::new(base.clone(), &[map]);
        live.move_budget = 1;
        let mut f = initial();
        f.set(23, DIR_INFO, &[(20, "Sub")]);
        f.set(24, file_info(3), &[(23, "c.txt")]);
        assert_eq!(shown(&live, &base), oracle(&f));
        (live, base, f)
    }

    /// What the live index shows after a compaction.
    fn shown_compacted(live: &mut Live) -> Vec<String> {
        let new = live.compact().expect("no folder reads");
        assert!(!new.adoptions_pending());
        assert!(new.hidden.is_empty());
        shown(&new, &new.base().clone())
    }

    #[test]
    fn a_large_folder_move_is_merged_in_memory_instead_of_rescanning() {
        let (mut live, base, mut f) = large_move_fixture();
        f.set(20, DIR_INFO, &[(5, "Papers")]);
        let r = live.apply(&[change(20, false)], &f);
        let why = r.rescan.expect("a compaction is asked for");
        assert_eq!(why.kind(), ReasonKind::Compact, "no disk read: {why}");
        assert_eq!(&*why, REASON_LARGE_MOVE);
        assert!(live.adoptions_pending());
        assert_eq!(live.delta_len(), 1, "only the folder itself was copied");
        // Until the compaction the contents are hidden, never shown under
        // the old path.
        assert_eq!(shown(&live, &base), ["C:", r"C:\Papers", r"C:\b.txt"]);
        // Changes inside keep being applied meanwhile.
        f.set(24, file_info(7), &[(23, "c2.txt")]);
        f.set(25, file_info(8), &[(23, "new.txt")]);
        let r = live.apply(&[change(24, false), change(25, true)], &f);
        assert!(r.rescan.is_none(), "{r:?}");
        assert_eq!(shown(&live, &base), ["C:", r"C:\Papers", r"C:\b.txt"]);
        // A folder's own dates follow too.
        f.set(
            23,
            RecordInfo {
                modified: 99,
                ..DIR_INFO
            },
            &[(20, "Sub")],
        );
        live.apply(&[change(23, false)], &f);
        assert_eq!(shown_compacted(&mut live), oracle(&f));
    }

    #[test]
    fn an_adopting_folder_that_moves_again_takes_its_contents_along() {
        let (mut live, _, mut f) = large_move_fixture();
        f.set(20, DIR_INFO, &[(5, "Papers")]);
        live.apply(&[change(20, false)], &f);
        // Renamed again before the compaction, then moved below b's sibling.
        f.set(40, DIR_INFO, &[(5, "Archive")]);
        f.set(20, DIR_INFO, &[(40, "Old papers")]);
        let r = live.apply(&[change(40, true), change(20, false)], &f);
        assert_eq!(
            r.rescan.as_ref().map(|r| r.kind()),
            Some(ReasonKind::Compact)
        );
        assert_eq!(shown_compacted(&mut live), oracle(&f));
    }

    #[test]
    fn a_large_folder_moved_below_a_moved_folder_is_placed() {
        // Docs is moved into a folder that was renamed (a delta entry), and
        // a new file in Docs moves with it.
        let (mut live, _, mut f) = large_move_fixture();
        f.set(26, file_info(5), &[(20, "fresh.txt")]);
        live.apply(&[change(26, true)], &f);
        f.set(40, DIR_INFO, &[(5, "Archive")]);
        live.apply(&[change(40, true)], &f);
        f.set(20, DIR_INFO, &[(40, "Docs")]);
        live.apply(&[change(20, false)], &f);
        assert!(live.adoptions_pending());
        assert!(
            shown(&live, &live.base().clone()).contains(&r"C:\Archive\Docs\fresh.txt".to_string()),
            "delta children are copied"
        );
        assert_eq!(shown_compacted(&mut live), oracle(&f));
    }

    #[test]
    fn deleting_an_adopting_folder_deletes_what_it_adopted() {
        let (mut live, base, mut f) = large_move_fixture();
        f.set(20, DIR_INFO, &[(5, "Papers")]);
        live.apply(&[change(20, false)], &f);
        for r in [20, 21, 23, 24] {
            f.0.remove(&r);
        }
        // The journal may report only the folder.
        live.apply(&[change(20, false)], &f);
        assert_eq!(shown(&live, &base), oracle(&f));
        assert_eq!(shown_compacted(&mut live), oracle(&f));
    }

    #[test]
    fn a_small_folder_move_is_still_copied_at_once() {
        let (mut live, base, mut f) = large_move_fixture();
        live.move_budget = MAX_MOVE;
        f.set(20, DIR_INFO, &[(5, "Papers")]);
        let r = live.apply(&[change(20, false)], &f);
        assert!(r.rescan.is_none(), "{r:?}");
        assert!(!live.adoptions_pending());
        assert_eq!(shown(&live, &base), oracle(&f));
    }

    #[test]
    fn a_large_move_compaction_waits_only_for_its_share_of_time() {
        let s = Duration::from_secs;
        let t0 = Instant::now();
        let mut m = Maintenance::after_scan(t0, s(60));
        let prompt = Load {
            prompt: true,
            ..Load::default()
        };
        m.request(Reason::compact(REASON_LARGE_MOVE));
        assert_eq!(m.due(t0 + s(1), Load::default()), None, "the usual gap");
        assert_eq!(m.wake_at(t0 + s(1), prompt), Some(t0), "no minimum gap");
        assert_eq!(
            m.due(t0 + s(1), prompt),
            Some(Due::Compact(REASON_LARGE_MOVE.into()))
        );
        // A 4 s compaction: the next prompt one waits 5 times that.
        let t1 = t0 + s(5);
        m.compacted(t1, s(4));
        m.request(Reason::compact(REASON_LARGE_MOVE));
        assert_eq!(m.due(t1 + s(19), prompt), None);
        assert_eq!(m.wake_at(t1 + s(19), prompt), Some(t1 + s(20)));
        // A waiting rescan does not hold it back (it may be half an hour off).
        m.request(Reason::disk("Volume{1}: change journal reset"));
        assert!(matches!(m.due(t1 + s(20), prompt), Some(Due::Compact(_))));
        // Folder reads still go first.
        m.request(Reason::compact(REASON_LARGE_MOVE));
        let reading = Load {
            walks: true,
            ..prompt
        };
        assert_eq!(m.due(t1 + s(21), reading), None);
    }

    #[test]
    fn a_folder_moved_below_its_own_subfolder_in_one_batch_waits_for_it() {
        // `mv Docs\Sub Sub; mv Docs Sub\Docs`: Docs has the lower record
        // number, so the batch reaches it while Sub is still inside it.
        let (mut live, _, mut f) = large_move_fixture();
        f.set(23, DIR_INFO, &[(5, "Sub")]);
        f.set(20, DIR_INFO, &[(23, "Docs")]);
        let r = live.apply(&[change(20, false), change(23, false)], &f);
        assert_ne!(
            r.rescan.as_ref().map(|r| r.kind()),
            Some(ReasonKind::Disk),
            "{r:?}"
        );
        assert_eq!(shown_compacted(&mut live), oracle(&f));
    }

    #[test]
    fn a_folder_moved_below_its_own_subfolder_before_that_one_is_reported_rescans() {
        let (mut live, base, mut f) = large_move_fixture();
        f.set(23, DIR_INFO, &[(5, "Sub")]);
        f.set(20, DIR_INFO, &[(23, "Docs")]);
        let before = shown(&live, &base);
        let r = live.apply(&[change(20, false)], &f);
        let why = r.rescan.expect("only a scan sees the nesting");
        assert_eq!((why.kind(), &*why), (ReasonKind::Disk, REASON_NESTING));
        assert!(!live.adoptions_pending());
        assert_eq!(shown(&live, &base), before, "nothing was hung below itself");
    }

    /// [`drive`] for a live layer that adopts: compactions may be asked
    /// for, never a disk read.
    fn drive_adopting(w: &mut World, live: &mut Live, batches: usize, seed: u64) {
        for _ in 0..batches {
            let mut changes: Vec<Change> = Vec::new();
            w.folder_op = false;
            for _ in 0..1 + w.random(6) {
                changes.extend(w.step());
            }
            changes.sort_by_key(|c| (c.volume, c.record));
            changes.dedup_by_key(|c| (c.volume, c.record));
            let mut r = live.apply(&changes, &w.fs);
            while let Some(job) = r.walks.pop() {
                let listing = walk_fake(&w.fs, &job);
                r.absorb(live.finish_walk(job.id, Ok(listing.into()), &w.fs));
            }
            if let Some(why) = &r.rescan {
                assert_eq!(why.kind(), ReasonKind::Compact, "seed {seed}: {why}");
            }
        }
    }

    #[test]
    fn a_compaction_after_large_moves_shows_what_a_scan_shows() {
        let mut adoptions = 0;
        for seed in 1..=32u64 {
            let mut w = World::new(seed);
            let (base, map) = scan(&w.fs, &[]);
            let mut live = Live::new(base, &[map]);
            // Every folder with more than two entries below it is "large".
            live.move_budget = 2;
            for round in 0..3 {
                drive_adopting(&mut w, &mut live, 10, seed);
                adoptions += usize::from(live.adoptions_pending());
                let mut new = live.compact().expect("no folder is being read");
                new.move_budget = 2;
                let (fresh, _) = scan(&w.fs, &[]);
                let none = LiveView {
                    allowed: Arc::new(vec![u64::MAX; fresh.len().div_ceil(64)]),
                    delta: Arc::new(IndexBuilder::new().freeze()),
                    delta_allowed: Arc::new(Vec::new()),
                    overrides: Arc::new(HashMap::new()),
                    entries: fresh.len(),
                };
                assert_eq!(
                    listing(&new.base().clone(), &new.view()),
                    listing(&fresh, &none),
                    "seed {seed}, round {round}"
                );
                live = new;
            }
        }
        assert!(adoptions > 0, "the worlds moved large folders");
    }

    #[test]
    fn hidden_entries_wait_for_folder_reads_and_saves_only_so_long() {
        let s = Duration::from_secs;
        let t0 = Instant::now();
        let mut m = Maintenance::after_scan(t0, s(60));
        m.request(Reason::compact(REASON_LARGE_MOVE));
        let saving = Load {
            prompt: true,
            saving: true,
            ..Load::default()
        };
        assert_eq!(m.due(t0 + s(1), saving), None, "a save holds it back");
        assert_eq!(
            m.wake_at(t0 + s(1), saving),
            Some(t0 + s(1) + SAVE_POLL),
            "looked at again"
        );
        assert!(matches!(
            m.due(t0 + s(1) + URGENT_PATIENCE, saving),
            Some(Due::Compact(_))
        ));
        m.compacted(t0 + s(42), s(1));
        // Folder reads that keep coming: a rescan once patience ends.
        m.request(Reason::compact(REASON_LARGE_MOVE));
        let reading = Load {
            prompt: true,
            walks: true,
            ..Load::default()
        };
        let t1 = t0 + s(100);
        assert_eq!(m.due(t1, reading), None);
        assert_eq!(m.wake_at(t1, reading), Some(t1 + URGENT_PATIENCE));
        assert!(matches!(
            m.due(t1 + URGENT_PATIENCE, reading),
            Some(Due::Rescan(_))
        ));
    }
}
