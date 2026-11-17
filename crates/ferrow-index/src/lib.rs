//! Ferrow's compact in-memory file-name index.
//!
//! The layout is columnar (struct of arrays): per entry we store a parent
//! id, a name offset/length into one shared byte arena, and flags. A second
//! arena holds case-folded names at the same offsets, and one permutation
//! keeps entry ids in name-sort order. Full paths are never stored; they are
//! rebuilt from the parent chain on demand. See
//! `docs/decisions/0001-index-layout.md`.

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rayon::prelude::*;

mod metadata;
pub mod osname;
pub mod persist;
pub use metadata::{
    ColumnSet, EntryMetadata, MetadataOrder, SortKey, SortStatus, SortTicket, metadata_timestamp,
    timestamp_ticks,
};
use metadata::{MetadataColumns, ShardMeta};

pub use persist::{LoadError, LoadTimings, MetadataLoad, SaveError, VERIFY_BUFFER, Verified};

/// Entry flag bits stored in the `flags` column.
pub mod flags {
    /// Entry is a directory.
    pub const DIR: u16 = 1 << 0;
    /// Entry is hidden (dot-file on Unix, hidden attribute on Windows).
    pub const HIDDEN: u16 = 1 << 1;
    /// Entry is a symbolic link.
    pub const SYMLINK: u16 = 1 << 2;
    /// The raw name bytes are not valid UTF-8 (WTF-8 or raw Unix bytes).
    pub const NAME_NOT_UTF8: u16 = 1 << 3;
}

/// Parent id of root entries.
pub const ROOT_PARENT: u32 = u32::MAX;

/// Case-fold a name for case-insensitive search.
///
/// The fold is Unicode simple case folding (`CaseFolding.txt`, status C
/// and S, via the `casefold` crate), restricted to mappings that keep
/// the UTF-8 byte length so the folded arena can share offsets with the
/// original arena. That covers the full same-length equivalence classes,
/// including `Σ`/`σ`/`ς`, the micro sign `µ`/`μ`, and Cherokee, where
/// the fold target is the uppercase letter. Mappings that would change
/// the byte length (for example `K` U+212A → `k`, `ſ` → `s`) and all
/// full (multi-char) folds such as `ß` → `ss` are a known M1 limitation
/// documented in ADR 0001; those characters are left unchanged. Invalid
/// UTF-8 (raw Unix bytes, WTF-8) gets ASCII-only folding. The query side
/// folds with this same function, so matching stays consistent.
pub fn fold_name(name: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(name.len());
    if name.is_ascii() {
        // Simple case folding over ASCII is exactly A-Z -> a-z.
        out.extend(name.iter().map(u8::to_ascii_lowercase));
        return;
    }
    if let Ok(s) = std::str::from_utf8(name) {
        let mut buf = [0u8; 4];
        for ch in s.chars() {
            let folded = casefold::simple_fold_char(ch);
            let use_char = if folded.len_utf8() == ch.len_utf8() {
                folded
            } else {
                ch
            };
            out.extend_from_slice(use_char.encode_utf8(&mut buf).as_bytes());
        }
    } else {
        out.extend(name.iter().map(u8::to_ascii_lowercase));
    }
    debug_assert_eq!(out.len(), name.len());
}

/// Immutable, query-ready index. Share it as `Arc<Index>`.
#[derive(Debug)]
pub struct Index {
    metadata: MetadataColumns,
    parent: Vec<u32>,
    name_off: Vec<u32>,
    name_len: Vec<u16>,
    flags: Vec<u16>,
    arena: Vec<u8>,
    folded_arena: Vec<u8>,
    sorted: Arc<Vec<u32>>,
}

impl Index {
    pub fn columns_present(&self) -> ColumnSet {
        self.metadata.present()
    }
    pub fn metadata(&self, id: u32) -> EntryMetadata {
        self.metadata.get(id)
    }
    /// One metadata value without decoding the other columns (size in
    /// bytes or a time in ticks); `None` when absent or not indexed.
    pub fn metadata_value(&self, key: SortKey, id: u32) -> Option<i128> {
        self.metadata.value(key, id)
    }
    /// Attribute bits of one entry; `None` when absent or not indexed.
    pub fn attributes(&self, id: u32) -> Option<u32> {
        self.metadata.attributes(id)
    }
    /// Ready metadata order; `None` means the column was not indexed.
    pub fn metadata_order(&self, key: SortKey, descending: bool) -> Option<MetadataOrder<'_>> {
        self.metadata.order(key, descending)
    }
    /// Start preparation once, without waiting, allocating by entry count, or
    /// borrowing the query pool. Pending searches can keep using name order.
    pub fn request_sort(&self, key: SortKey) -> SortStatus {
        self.metadata.request(key, self.sorted.clone(), None)
    }
    /// [`Index::request_sort`], with `start` run first on the preparing
    /// thread (not at all when the order is ready or already being prepared).
    /// For a background warm-up that must not compete with searches: the
    /// caller passes a function that lowers the thread's priority.
    pub fn request_sort_starting(
        &self,
        key: SortKey,
        start: impl FnOnce() + Send + 'static,
    ) -> SortStatus {
        self.metadata
            .request_starting(key, self.sorted.clone(), start)
    }
    /// Share preparation until every requesting token is cancelled. An
    /// uncancelled request continues warming the cache after search returns.
    /// Explicit `request_sort` calls keep it alive until ready or retirement.
    pub fn request_sort_cancellable(
        &self,
        key: SortKey,
        cancel: Arc<std::sync::atomic::AtomicBool>,
    ) -> SortStatus {
        self.metadata
            .request(key, self.sorted.clone(), Some(cancel))
    }
    #[cfg(feature = "test-hooks")]
    pub fn request_sort_with_hook(
        &self,
        key: SortKey,
        hook: Arc<dyn Fn() + Send + Sync>,
    ) -> SortStatus {
        self.metadata
            .request_with_hook(key, self.sorted.clone(), hook)
    }
    pub fn sort_ticket(&self, key: SortKey) -> Option<SortTicket> {
        self.metadata.ticket(key)
    }
    #[cfg(feature = "bench-internals")]
    pub fn sort_value_for_benchmark(&self, key: SortKey, id: u32) -> Option<i128> {
        self.metadata.value(key, id)
    }
    pub fn metadata_heap_bytes(&self) -> usize {
        self.metadata.heap_bytes()
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.parent.len()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    /// Parent id of an entry, or [`ROOT_PARENT`] for roots.
    pub fn parent(&self, id: u32) -> u32 {
        self.parent[id as usize]
    }

    /// Flag bits of an entry (see [`flags`]).
    pub fn flags(&self, id: u32) -> u16 {
        self.flags[id as usize]
    }

    /// Raw name bytes of an entry (a single path component, or the full
    /// root path for roots).
    pub fn name(&self, id: u32) -> &[u8] {
        let i = id as usize;
        let off = self.name_off[i] as usize;
        &self.arena[off..off + self.name_len[i] as usize]
    }

    /// Case-folded name bytes of an entry, for case-insensitive matching.
    pub fn folded_name(&self, id: u32) -> &[u8] {
        let i = id as usize;
        let off = self.name_off[i] as usize;
        &self.folded_arena[off..off + self.name_len[i] as usize]
    }

    /// Entry ids in name-sort order (folded name, then raw name, then id).
    pub fn sorted(&self) -> &[u32] {
        &self.sorted
    }

    /// Rebuild the full path of an entry into `buf` as raw bytes, joined
    /// with the platform separator. Returns the byte range written length.
    pub fn path_bytes(&self, id: u32, buf: &mut Vec<u8>) {
        buf.clear();
        // Collect the chain root-first by walking up, then reversing.
        let mut chain: Vec<u32> = Vec::new();
        let mut cur = id;
        loop {
            chain.push(cur);
            let p = self.parent(cur);
            if p == ROOT_PARENT {
                break;
            }
            cur = p;
        }
        let sep = std::path::MAIN_SEPARATOR as u8;
        for (i, &part) in chain.iter().rev().enumerate() {
            if i > 0 && !buf.ends_with(&[sep]) {
                buf.push(sep);
            }
            buf.extend_from_slice(self.name(part));
        }
    }

    /// Rebuild the full path of an entry as a [`std::path::PathBuf`].
    pub fn path(&self, id: u32) -> std::path::PathBuf {
        let mut buf = Vec::new();
        self.path_bytes(id, &mut buf);
        osname::path_from_bytes(&buf)
    }

    /// Bytes held on the heap by all columns and arenas.
    pub fn heap_bytes(&self) -> usize {
        self.parent.capacity() * 4
            + self.name_off.capacity() * 4
            + self.name_len.capacity() * 2
            + self.flags.capacity() * 2
            + self.arena.capacity()
            + self.folded_arena.capacity()
            + self.sorted.capacity() * 4
            + self.metadata.heap_bytes()
            + std::mem::size_of::<Vec<u32>>()
            + 2 * std::mem::size_of::<usize>()
    }

    /// [`Index::heap_bytes`] split into named parts (capacity, not length):
    /// the columns, both name arenas, the name order and, per present
    /// metadata column, its values, validity bitmap, overflows, cached sort
    /// order (zero until prepared) and fixed objects. The parts add up to
    /// `heap_bytes()` exactly. Memory accounting only; allocates a short list.
    pub fn heap_parts(&self) -> Vec<(String, usize)> {
        let mut out: Vec<(String, usize)> = vec![
            ("parent".into(), self.parent.capacity() * 4),
            ("name_off".into(), self.name_off.capacity() * 4),
            ("name_len".into(), self.name_len.capacity() * 2),
            ("flags".into(), self.flags.capacity() * 2),
            ("name_arena".into(), self.arena.capacity()),
            ("folded_arena".into(), self.folded_arena.capacity()),
            ("name_order".into(), self.sorted.capacity() * 4),
        ];
        let mut columns = Vec::new();
        self.metadata.heap_parts(&mut columns);
        out.extend(
            columns
                .into_iter()
                .map(|(name, bytes)| (format!("metadata.{name}"), bytes)),
        );
        out.push((
            "objects".into(),
            std::mem::size_of::<Vec<u32>>() + 2 * std::mem::size_of::<usize>(),
        ));
        out
    }

    /// Bytes of the saved file, which is also the size of the single buffer a
    /// save fills in memory before it writes (memory accounting).
    pub fn serialized_len(&self) -> Result<usize, SaveError> {
        persist::serialized_len(self)
    }

    /// Save to the versioned binary format. See [`persist`].
    pub fn save(&self, path: &std::path::Path) -> Result<(), SaveError> {
        persist::save(self, path)
    }

    /// Load from the versioned binary format. See [`persist`].
    pub fn load(path: &std::path::Path) -> Result<Index, LoadError> {
        persist::load(path)
    }

    /// Check a saved index without decoding it: header, length and the
    /// whole-file checksum over bounded reads (memory use is
    /// [`VERIFY_BUFFER`], not the index). See [`persist::Verified`].
    pub fn verify_file(path: &std::path::Path) -> Result<Verified, LoadError> {
        persist::verify_file(path)
    }

    /// Load with an additive timing breakdown (read, checksum,
    /// decode, structure validation, derived-data validation). See
    /// [`LoadTimings`].
    pub fn load_with_timings(path: &std::path::Path) -> Result<(Index, LoadTimings), LoadError> {
        persist::load_with_timings(path)
    }

    /// Column access for [`persist`] and tests.
    pub(crate) fn columns(&self) -> PersistColumns<'_> {
        PersistColumns {
            parent: &self.parent,
            name_off: &self.name_off,
            name_len: &self.name_len,
            flags: &self.flags,
            arena: &self.arena,
            folded_arena: &self.folded_arena,
            sorted: &self.sorted,
        }
    }

    pub(crate) fn from_columns(c: OwnedColumns) -> Index {
        Index {
            metadata: MetadataColumns::default(),
            parent: c.parent,
            name_off: c.name_off,
            name_len: c.name_len,
            flags: c.flags,
            arena: c.arena,
            folded_arena: c.folded_arena,
            sorted: Arc::new(c.sorted),
        }
    }
}

pub(crate) struct PersistColumns<'a> {
    pub parent: &'a [u32],
    pub name_off: &'a [u32],
    pub name_len: &'a [u16],
    pub flags: &'a [u16],
    pub arena: &'a [u8],
    pub folded_arena: &'a [u8],
    pub sorted: &'a [u32],
}

pub(crate) struct OwnedColumns {
    pub parent: Vec<u32>,
    pub name_off: Vec<u32>,
    pub name_len: Vec<u16>,
    pub flags: Vec<u16>,
    pub arena: Vec<u8>,
    pub folded_arena: Vec<u8>,
    pub sorted: Vec<u32>,
}

/// What one builder shard has collected, column by column in arrival order:
/// no record or allocation per entry. Names are appended to one byte arena
/// (lengths in `name_len`), metadata is packed like the final columns.
struct Shard {
    /// The entry id of each arrival (dense ids, handed out in any order).
    ids: Vec<u32>,
    parent: Vec<u32>,
    flags: Vec<u16>,
    name_len: Vec<u16>,
    arena: Vec<u8>,
    meta: ShardMeta,
}

impl Shard {
    fn new(columns: ColumnSet) -> Shard {
        Shard {
            ids: Vec::new(),
            parent: Vec::new(),
            flags: Vec::new(),
            name_len: Vec::new(),
            arena: Vec::new(),
            meta: ShardMeta::new(columns),
        }
    }

    fn reserve(&mut self, entries: usize, name_bytes: usize) {
        self.ids.reserve_exact(entries);
        self.parent.reserve_exact(entries);
        self.flags.reserve_exact(entries);
        self.name_len.reserve_exact(entries);
        self.arena.reserve_exact(name_bytes);
        self.meta.reserve(entries);
    }

    fn columns_bytes(&self) -> usize {
        self.ids.capacity() * 4
            + self.parent.capacity() * 4
            + self.flags.capacity() * 2
            + self.name_len.capacity() * 2
    }
}

const SHARDS: usize = 16;

/// Freeze costs; metadata collection and sort are disjoint parts of total.
#[derive(Debug, Clone, Copy)]
pub struct FreezeTimings {
    pub total: std::time::Duration,
    pub metadata_collect: std::time::Duration,
    pub metadata_sort: std::time::Duration,
}

/// Thread-safe builder. Entries may be added from many threads in any
/// order; sharding keeps lock contention low. Ids are handed out densely,
/// so parent ids returned by [`IndexBuilder::add`] stay valid after
/// [`IndexBuilder::freeze`].
pub struct IndexBuilder {
    columns: ColumnSet,
    shards: Vec<Mutex<Shard>>,
    next_id: AtomicU32,
    next_shard: AtomicUsize,
}

impl Default for IndexBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl IndexBuilder {
    pub fn new() -> IndexBuilder {
        Self::with_columns(ColumnSet::NONE)
    }

    pub fn with_columns(columns: ColumnSet) -> IndexBuilder {
        IndexBuilder {
            columns,
            shards: (0..SHARDS)
                .map(|_| Mutex::new(Shard::new(columns)))
                .collect(),
            next_id: AtomicU32::new(0),
            next_shard: AtomicUsize::new(0),
        }
    }

    /// Add a root entry whose name is the root path. Returns its id.
    pub fn add_root(&self, name: &[u8], flags: u16) -> u32 {
        self.add(ROOT_PARENT, name, flags)
    }

    /// Add an entry under `parent`. Returns its id.
    ///
    /// Names longer than `u16::MAX` bytes are truncated to fit the column
    /// (real file systems cap components far below that).
    pub fn add(&self, parent: u32, name: &[u8], flags: u16) -> u32 {
        self.add_with_metadata(parent, name, flags, EntryMetadata::default())
    }

    pub fn add_with_metadata(
        &self,
        parent: u32,
        name: &[u8],
        flags: u16,
        metadata: EntryMetadata,
    ) -> u32 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let shard = self.next_shard.fetch_add(1, Ordering::Relaxed) % SHARDS;
        let mut flags = flags;
        if std::str::from_utf8(name).is_err() {
            flags |= flags::NAME_NOT_UTF8;
        }
        let take = name.len().min(u16::MAX as usize);
        let mut s = self.shards[shard].lock().expect("builder shard poisoned");
        s.ids.push(id);
        s.parent.push(parent);
        s.flags.push(flags);
        s.name_len.push(take as u16);
        s.arena.extend_from_slice(&name[..take]);
        s.meta.push(&metadata);
        id
    }

    /// Make room for `entries` more entries with `name_bytes` of names in
    /// total, spread over the shards, so the build does not grow its columns
    /// by doubling (which would hold up to twice what it needs). A hint: the
    /// builder still takes more. Entries go to the shards round-robin, so
    /// names are shared out evenly up to a small margin.
    pub fn reserve(&self, entries: usize, name_bytes: usize) {
        let per_entries = entries.div_ceil(SHARDS);
        let per_bytes = name_bytes.div_ceil(SHARDS);
        for shard in &self.shards {
            shard
                .lock()
                .expect("builder shard poisoned")
                .reserve(per_entries, per_bytes + per_bytes / 32 + 4096);
        }
    }

    /// Heap the builder holds, by capacity, as named parts (memory
    /// accounting; locks every shard briefly, so call it between phases).
    pub fn memory_parts(&self) -> Vec<(String, usize)> {
        let (mut columns, mut names, mut metadata) = (0, 0, 0);
        for shard in &self.shards {
            let shard = shard.lock().expect("builder shard poisoned");
            columns += shard.columns_bytes();
            names += shard.arena.capacity();
            metadata += shard.meta.heap_bytes();
        }
        vec![
            ("builder.columns".into(), columns),
            ("builder.names".into(), names),
            ("builder.metadata".into(), metadata),
        ]
    }

    /// Number of entries added so far.
    pub fn len(&self) -> usize {
        self.next_id.load(Ordering::Relaxed) as usize
    }

    /// Whether nothing was added yet.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Freeze into an immutable [`Index`]: builds the arenas (folding in
    /// parallel) and the name-sort permutation (parallel sort).
    pub fn freeze(self) -> Index {
        self.freeze_with_timings().0
    }

    pub fn freeze_with_timings(self) -> (Index, FreezeTimings) {
        let started = std::time::Instant::now();
        let n = self.len();
        let mut shards: Vec<Shard> = self
            .shards
            .into_iter()
            .map(|s| s.into_inner().expect("builder shard poisoned"))
            .collect();
        debug_assert_eq!(shards.iter().map(|s| s.ids.len()).sum::<usize>(), n);

        // Ids are dense, so every column is placed by id. Shards hold their
        // entries in arrival order, which is nearly id order: the writes run
        // as sixteen mostly sequential streams.
        let mut parent = vec![0u32; n];
        let mut ent_flags = vec![0u16; n];
        let mut name_len = vec![0u16; n];
        for s in &shards {
            for (k, &id) in s.ids.iter().enumerate() {
                let i = id as usize;
                parent[i] = s.parent[k];
                ent_flags[i] = s.flags[k];
                name_len[i] = s.name_len[k];
            }
        }
        let mut name_off = Vec::with_capacity(n);
        let mut total_name = 0usize;
        for &len in &name_len {
            name_off.push(total_name as u32);
            total_name += len as usize;
        }
        // Names and metadata move shard by shard; each shard is freed as soon
        // as it is placed, so the builder's memory drains while the final
        // columns fill.
        let mut arena = vec![0u8; total_name];
        let mut fill = MetadataColumns::fill(n, self.columns);
        let mut metadata_collect = std::time::Duration::ZERO;
        for s in shards.drain(..) {
            let mut at = 0;
            for (k, &id) in s.ids.iter().enumerate() {
                let len = s.name_len[k] as usize;
                let off = name_off[id as usize] as usize;
                arena[off..off + len].copy_from_slice(&s.arena[at..at + len]);
                at += len;
            }
            let t = std::time::Instant::now();
            fill.scatter(&s.meta, &s.ids);
            metadata_collect += t.elapsed();
        }
        let metadata = fill.finish();
        // Allocate the final arena once and split it only at entry boundaries.
        // Each worker reuses scratch; no per-name Vec or slice table survives.
        let mut folded_arena = vec![0; arena.len()];
        let chunk = n.div_ceil(rayon::current_num_threads() * 4).max(1024);
        let mut rest = folded_arena.as_mut_slice();
        let mut work = Vec::new();
        for start in (0..n).step_by(chunk) {
            let end = (start + chunk).min(n);
            let byte_start = name_off[start] as usize;
            let byte_end = if end == n {
                arena.len()
            } else {
                name_off[end] as usize
            };
            let (out, tail) = rest.split_at_mut(byte_end - byte_start);
            rest = tail;
            work.push((start, end, byte_start, out));
        }
        work.into_par_iter()
            .for_each_init(Vec::new, |scratch, (start, end, byte_start, out)| {
                for i in start..end {
                    let off = name_off[i] as usize;
                    let len = name_len[i] as usize;
                    fold_name(&arena[off..off + len], scratch);
                    out[off - byte_start..off - byte_start + len].copy_from_slice(scratch);
                }
            });

        let mut sorted: Vec<u32> = (0..n as u32).collect();
        let folded_of = |id: u32| -> &[u8] {
            let i = id as usize;
            let off = name_off[i] as usize;
            &folded_arena[off..off + name_len[i] as usize]
        };
        let raw_of = |id: u32| -> &[u8] {
            let i = id as usize;
            let off = name_off[i] as usize;
            &arena[off..off + name_len[i] as usize]
        };
        sorted.par_sort_unstable_by(|&a, &b| {
            folded_of(a)
                .cmp(folded_of(b))
                .then_with(|| raw_of(a).cmp(raw_of(b)))
                .then_with(|| a.cmp(&b))
        });

        let metadata_sort = std::time::Duration::ZERO;
        let index = Index {
            metadata,
            parent,
            name_off,
            name_len,
            flags: ent_flags,
            arena,
            folded_arena,
            sorted: Arc::new(sorted),
        };
        (
            index,
            FreezeTimings {
                total: started.elapsed(),
                metadata_collect,
                metadata_sort,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_index() -> Index {
        let b = IndexBuilder::new();
        let root = b.add_root(b"root", flags::DIR);
        let docs = b.add(root, b"Docs", flags::DIR);
        b.add(docs, b"Report 2024.txt", 0);
        b.add(docs, b"apple.log", 0);
        b.add(root, "Zebra.md".as_bytes(), 0);
        b.add(root, "Ärger.txt".as_bytes(), 0);
        b.freeze()
    }

    /// One entry as the scan hands it to the builder.
    #[derive(Clone)]
    struct Spec {
        parent: u32,
        name: Vec<u8>,
        flags: u16,
        meta: EntryMetadata,
    }

    /// The columns the format defines for entries in id order, computed with
    /// no builder code (the layout of ADR 0001: names packed in id order,
    /// folded arena at the same offsets, sort by folded, raw, id).
    struct Reference {
        parent: Vec<u32>,
        name_off: Vec<u32>,
        name_len: Vec<u16>,
        flags: Vec<u16>,
        arena: Vec<u8>,
        folded: Vec<u8>,
        sorted: Vec<u32>,
    }

    fn reference(specs: &[Spec]) -> Reference {
        let mut r = Reference {
            parent: vec![],
            name_off: vec![],
            name_len: vec![],
            flags: vec![],
            arena: vec![],
            folded: vec![],
            sorted: vec![],
        };
        let mut scratch = Vec::new();
        for s in specs {
            let take = s.name.len().min(u16::MAX as usize);
            r.parent.push(s.parent);
            r.name_off.push(r.arena.len() as u32);
            r.name_len.push(take as u16);
            let bad = std::str::from_utf8(&s.name).is_err();
            r.flags
                .push(s.flags | if bad { flags::NAME_NOT_UTF8 } else { 0 });
            r.arena.extend_from_slice(&s.name[..take]);
            fold_name(&s.name[..take], &mut scratch);
            r.folded.extend_from_slice(&scratch);
        }
        let name = |i: u32| {
            let off = r.name_off[i as usize] as usize;
            off..off + r.name_len[i as usize] as usize
        };
        let mut sorted: Vec<u32> = (0..specs.len() as u32).collect();
        sorted.sort_by(|&a, &b| {
            r.folded[name(a)]
                .cmp(&r.folded[name(b)])
                .then_with(|| r.arena[name(a)].cmp(&r.arena[name(b)]))
                .then(a.cmp(&b))
        });
        r.sorted = sorted;
        r
    }

    /// What a packed column keeps of a time: whole seconds when they fit the
    /// packed range, else the exact value (an overflow).
    fn kept_time(ticks: Option<i64>) -> Option<i64> {
        ticks.map(|t| {
            let seconds = t.div_euclid(10_000_000);
            if (0..=u32::MAX as i64).contains(&seconds) {
                seconds * 10_000_000
            } else {
                t
            }
        })
    }

    /// Check an index against the specs (entry i of `specs` has id i).
    fn assert_matches(idx: &Index, specs: &[Spec], set: ColumnSet) {
        let r = reference(specs);
        let c = idx.columns();
        assert_eq!(c.parent, r.parent.as_slice(), "parent");
        assert_eq!(c.name_off, r.name_off.as_slice(), "name_off");
        assert_eq!(c.name_len, r.name_len.as_slice(), "name_len");
        assert_eq!(c.flags, r.flags.as_slice(), "flags");
        assert_eq!(c.arena, r.arena.as_slice(), "arena");
        assert_eq!(c.folded_arena, r.folded.as_slice(), "folded arena");
        assert_eq!(c.sorted, r.sorted.as_slice(), "sorted");
        assert_eq!(idx.columns_present().bits(), set.bits());
        for (i, s) in specs.iter().enumerate() {
            let got = idx.metadata(i as u32);
            let has = |bit| set.contains(bit);
            let want = EntryMetadata {
                size: s.meta.size.filter(|_| has(ColumnSet::SIZE)),
                modified: kept_time(s.meta.modified).filter(|_| has(ColumnSet::MODIFIED)),
                created: kept_time(s.meta.created).filter(|_| has(ColumnSet::CREATED)),
                accessed: kept_time(s.meta.accessed).filter(|_| has(ColumnSet::ACCESSED)),
                attributes: s.meta.attributes.filter(|_| has(ColumnSet::ATTRIBUTES)),
            };
            assert_eq!(got, want, "metadata of entry {i}");
        }
    }

    /// Entries of every awkward kind, from a fixed generator: a deep chain,
    /// non-ASCII and invalid UTF-8 names, an over-long name, empty names,
    /// duplicates, and metadata that overflows the packed widths.
    fn awkward_specs(n: usize) -> Vec<Spec> {
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut specs = vec![Spec {
            parent: ROOT_PARENT,
            name: b"C:".to_vec(),
            flags: flags::DIR,
            meta: EntryMetadata::default(),
        }];
        let words: [&[u8]; 8] = [
            b"report",
            b"Report",
            "Ärger".as_bytes(),
            "写真".as_bytes(),
            "Σίσυφος".as_bytes(),
            b"\xFF\xFEraw",
            b"",
            b"a",
        ];
        for i in 1..n {
            // The first 300 entries chain: each is the previous one's child.
            let parent = if i < 300 {
                i as u32 - 1
            } else {
                (next() % i as u64) as u32
            };
            let mut name = words[(next() % 8) as usize].to_vec();
            name.extend_from_slice(format!("-{}", next() % 50).as_bytes());
            if i == 777 {
                name = vec![b'L'; 70_000];
            }
            let big = next() % 7 == 0;
            specs.push(Spec {
                parent,
                name,
                flags: if next() % 5 == 0 { flags::DIR } else { 0 }
                    | ((next() % 3 == 0) as u16 * 2),
                meta: EntryMetadata {
                    size: (next() % 4 != 0).then(|| {
                        if big {
                            u64::MAX - i as u64
                        } else {
                            next() % (1 << 33)
                        }
                    }),
                    modified: (next() % 4 != 0).then(|| {
                        if big {
                            -(next() as i64 >> 20)
                        } else {
                            (next() % 4_000_000_000) as i64 * 10_000_000 + 123
                        }
                    }),
                    created: (next() % 3 == 0).then(|| (next() % 1_000_000) as i64 * 10_000_000),
                    accessed: (next() % 3 == 0).then(|| i64::MAX - (next() % 99) as i64),
                    attributes: (next() % 2 == 0).then(|| next() as u32),
                },
            });
        }
        specs
    }

    fn build(specs: &[Spec], set: ColumnSet, hint: Option<(usize, usize)>) -> Index {
        let b = IndexBuilder::with_columns(set);
        if let Some((entries, bytes)) = hint {
            b.reserve(entries, bytes);
        }
        for (i, s) in specs.iter().enumerate() {
            let id = b.add_with_metadata(s.parent, &s.name, s.flags, s.meta);
            assert_eq!(id as usize, i, "ids are handed out in order");
        }
        b.freeze()
    }

    #[test]
    fn built_index_equals_the_format_reference_for_every_column_set() {
        let specs = awkward_specs(4_000);
        let total: usize = specs.iter().map(|s| s.name.len().min(65_535)).sum();
        for set in [
            ColumnSet::NONE,
            ColumnSet::DEFAULT,
            ColumnSet::ALL,
            ColumnSet::ATTRIBUTES,
        ] {
            assert_matches(&build(&specs, set, None), &specs, set);
            // A hint that is too small, exact or far too large changes
            // nothing but capacity.
            for hint in [(10, 10), (specs.len(), total), (100_000, 10_000_000)] {
                assert_matches(&build(&specs, set, Some(hint)), &specs, set);
            }
        }
    }

    #[test]
    fn built_index_survives_a_save_and_load_unchanged() {
        let specs = awkward_specs(3_000);
        let idx = build(&specs, ColumnSet::ALL, None);
        let dir = std::env::temp_dir().join(format!("ferrow-build-eq-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("index.bin");
        idx.save(&path).unwrap();
        let loaded = Index::load(&path).unwrap();
        assert_matches(&loaded, &specs, ColumnSet::ALL);
        let again = dir.join("again.bin");
        loaded.save(&again).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            std::fs::read(&again).unwrap(),
            "the saved file is byte-identical after a round trip"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn entries_added_from_many_threads_land_at_their_ids() {
        let b = IndexBuilder::with_columns(ColumnSet::ALL);
        let root = b.add_root(b"r", flags::DIR);
        let mut all: Vec<(u32, Spec)> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..6)
                .map(|t| {
                    let b = &b;
                    s.spawn(move || {
                        (0..700)
                            .map(|i| {
                                let spec = Spec {
                                    parent: root,
                                    name: format!("t{t}-{i}-Ä").into_bytes(),
                                    flags: 0,
                                    meta: EntryMetadata {
                                        size: Some((t * 1000 + i) as u64),
                                        modified: Some(((t * 1000 + i) as i64) * 10_000_000),
                                        created: None,
                                        accessed: (i % 2 == 0).then_some(i64::MAX),
                                        attributes: Some(i as u32),
                                    },
                                };
                                let id = b.add_with_metadata(
                                    spec.parent,
                                    &spec.name,
                                    spec.flags,
                                    spec.meta,
                                );
                                (id, spec)
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect()
        });
        let idx = b.freeze();
        all.sort_by_key(|(id, _)| *id);
        let mut specs = vec![Spec {
            parent: ROOT_PARENT,
            name: b"r".to_vec(),
            flags: flags::DIR,
            meta: EntryMetadata::default(),
        }];
        for (i, (id, spec)) in all.into_iter().enumerate() {
            assert_eq!(id as usize, i + 1, "ids are dense");
            specs.push(spec);
        }
        assert_matches(&idx, &specs, ColumnSet::ALL);
    }

    #[test]
    fn empty_and_single_entry_builders_freeze() {
        let idx = IndexBuilder::with_columns(ColumnSet::ALL).freeze();
        assert!(idx.is_empty());
        let one = [Spec {
            parent: ROOT_PARENT,
            name: b"x".to_vec(),
            flags: flags::DIR,
            meta: EntryMetadata::default(),
        }];
        assert_matches(&build(&one, ColumnSet::ALL, None), &one, ColumnSet::ALL);
    }

    #[test]
    fn builder_memory_parts_cover_its_columns_names_and_metadata() {
        let specs = awkward_specs(2_000);
        let b = IndexBuilder::with_columns(ColumnSet::DEFAULT);
        b.reserve(specs.len(), 40_000);
        for s in &specs {
            b.add_with_metadata(s.parent, &s.name, s.flags, s.meta);
        }
        let parts: std::collections::HashMap<_, _> = b.memory_parts().into_iter().collect();
        let names: usize = specs.iter().map(|s| s.name.len().min(65_535)).sum();
        assert!(parts["builder.names"] >= names);
        assert!(parts["builder.columns"] >= specs.len() * 12);
        assert!(parts["builder.metadata"] >= specs.len() * 9);
    }

    #[test]
    fn path_rebuild() {
        let idx = small_index();
        let sep = std::path::MAIN_SEPARATOR;
        let mut buf = Vec::new();
        // ids: 0 root, 1 Docs, 2 Report..., in insertion order.
        idx.path_bytes(2, &mut buf);
        assert_eq!(
            String::from_utf8(buf.clone()).unwrap(),
            format!("root{sep}Docs{sep}Report 2024.txt")
        );
        idx.path_bytes(0, &mut buf);
        assert_eq!(String::from_utf8(buf).unwrap(), "root");
    }

    #[test]
    fn root_with_trailing_separator_is_not_doubled() {
        let b = IndexBuilder::new();
        let root_name = format!("root{}", std::path::MAIN_SEPARATOR);
        let root = b.add_root(root_name.as_bytes(), flags::DIR);
        let child = b.add(root, b"a.txt", 0);
        let idx = b.freeze();
        let mut buf = Vec::new();
        idx.path_bytes(child, &mut buf);
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s, format!("root{}a.txt", std::path::MAIN_SEPARATOR));
    }

    #[test]
    fn sort_order_is_folded_name_order() {
        let idx = small_index();
        let names: Vec<String> = idx
            .sorted()
            .iter()
            .map(|&id| String::from_utf8(idx.folded_name(id).to_vec()).unwrap())
            .collect();
        let mut expect = names.clone();
        expect.sort();
        assert_eq!(names, expect);
        // Case-insensitive: "apple.log" sorts before "Docs" (folded "docs").
        assert_eq!(names[0], "apple.log");
        // Non-ASCII folded name participates by its UTF-8 bytes.
        assert!(names.contains(&"ärger.txt".to_string()));
    }

    #[test]
    fn fold_preserves_len_and_folds_unicode() {
        let mut out = Vec::new();
        fold_name("ÄÖÜ ABC ΣΣ".as_bytes(), &mut out);
        assert_eq!(out.len(), "ÄÖÜ ABC ΣΣ".len());
        assert_eq!(String::from_utf8(out.clone()).unwrap(), "äöü abc σσ");
        // Non-UTF8 bytes: ASCII folds, other bytes pass through.
        fold_name(&[b'A', 0xFF, b'Z'], &mut out);
        assert_eq!(out, vec![b'a', 0xFF, b'z']);
    }

    #[test]
    fn parallel_adds_from_many_threads() {
        let b = IndexBuilder::new();
        let root = b.add_root(b"r", flags::DIR);
        std::thread::scope(|s| {
            for t in 0..4 {
                let b = &b;
                s.spawn(move || {
                    for i in 0..250 {
                        b.add(root, format!("f{t}-{i}.txt").as_bytes(), 0);
                    }
                });
            }
        });
        let idx = b.freeze();
        assert_eq!(idx.len(), 1001);
        assert!(
            idx.sorted()
                .windows(2)
                .all(|w| { idx.folded_name(w[0]) <= idx.folded_name(w[1]) })
        );
    }

    /// Independent oracle: expected folds written out by hand from
    /// Unicode CaseFolding.txt (status C and S), restricted to mappings
    /// that keep the UTF-8 byte length. This test deliberately does not
    /// derive its expectations from `fold_name` itself.
    #[test]
    fn fold_matches_unicode_simple_case_folding_same_length() {
        let cases: &[(char, char)] = &[
            ('A', 'a'),
            ('Ä', 'ä'),
            ('Σ', 'σ'), // GREEK CAPITAL SIGMA
            ('ς', 'σ'), // GREEK SMALL FINAL SIGMA folds to sigma
            ('σ', 'σ'),
            ('µ', 'μ'), // MICRO SIGN folds to GREEK SMALL MU
            ('Μ', 'μ'),
            ('\u{0345}', 'ι'), // COMBINING GREEK YPOGEGRAMMENI -> iota
            ('Ж', 'ж'),
            ('ǅ', 'ǆ'), // titlecase digraph -> lowercase digraph
            ('ꭰ', 'Ꭰ'), // CHEROKEE SMALL LETTER A -> CHEROKEE LETTER A
            ('Ꭰ', 'Ꭰ'),
            ('ᾈ', 'ᾀ'), // GREEK CAPITAL ... YPOGEGRAMMENI
            ('ß', 'ß'), // sharp s: simple fold is itself (ss is full folding)
            ('İ', 'İ'), // no simple fold; length-changing full fold is out of scope
        ];
        let mut out = Vec::new();
        for &(input, expected) in cases {
            assert_eq!(
                input.len_utf8(),
                expected.len_utf8(),
                "oracle pair U+{:04X} -> U+{:04X} must be same-length",
                input as u32,
                expected as u32
            );
            fold_name(input.to_string().as_bytes(), &mut out);
            assert_eq!(
                String::from_utf8(out.clone()).unwrap(),
                expected.to_string(),
                "fold of U+{:04X}",
                input as u32
            );
        }
        // Length-changing simple folds stay unfolded in M1 (documented
        // limitation): KELVIN SIGN -> k and LONG S -> s would shrink.
        for input in ['\u{212A}', 'ſ'] {
            fold_name(input.to_string().as_bytes(), &mut out);
            assert_eq!(
                String::from_utf8(out.clone()).unwrap(),
                input.to_string(),
                "length-changing fold of U+{:04X} must be left alone",
                input as u32
            );
        }
    }

    #[test]
    fn heap_bytes_counts_all_columns() {
        let mut idx = small_index();
        // Reserve extra capacity in every column and both arenas so
        // capacity and len differ everywhere: a heap_bytes() that used
        // len() instead of capacity() (or dropped a column) cannot
        // accidentally produce the same total.
        idx.parent.reserve(37);
        idx.name_off.reserve(41);
        idx.name_len.reserve(43);
        idx.flags.reserve(47);
        idx.arena.reserve(53);
        idx.folded_arena.reserve(59);
        Arc::get_mut(&mut idx.sorted).unwrap().reserve(61);
        assert!(idx.parent.capacity() > idx.parent.len());
        assert!(idx.name_off.capacity() > idx.name_off.len());
        assert!(idx.name_len.capacity() > idx.name_len.len());
        assert!(idx.flags.capacity() > idx.flags.len());
        assert!(idx.arena.capacity() > idx.arena.len());
        assert!(idx.folded_arena.capacity() > idx.folded_arena.len());
        assert!(idx.sorted.capacity() > idx.sorted.len());
        // Exact per-column capacity accounting: 4+4+2+2 bytes of fixed
        // columns plus 4 bytes of sort order per entry, plus both
        // arenas.
        let expected = idx.parent.capacity() * 4
            + idx.name_off.capacity() * 4
            + idx.name_len.capacity() * 2
            + idx.flags.capacity() * 2
            + idx.arena.capacity()
            + idx.folded_arena.capacity()
            + idx.sorted.capacity() * 4
            + std::mem::size_of::<Vec<u32>>()
            + 2 * std::mem::size_of::<usize>();
        assert_eq!(idx.heap_bytes(), expected);
        assert_eq!(idx.arena.len(), idx.folded_arena.len());
        let live_floor = idx.len() * (4 + 4 + 2 + 2 + 4) + idx.arena.len() * 2;
        assert!(idx.heap_bytes() > live_floor);
    }

    #[test]
    fn heap_parts_add_up_to_heap_bytes_with_every_column_and_a_ready_order() {
        let b = IndexBuilder::with_columns(ColumnSet::ALL);
        let root = b.add_root(b"root", flags::DIR);
        for i in 0..40u64 {
            b.add_with_metadata(
                root,
                format!("file{i}.txt").as_bytes(),
                0,
                EntryMetadata {
                    size: Some(i * 1000),
                    modified: Some(i as i64 * 10_000_000),
                    created: Some(i as i64 * 10_000_000),
                    accessed: None,
                    attributes: Some(0x20),
                },
            );
        }
        let idx = b.freeze();
        let before = idx.heap_parts();
        let total = |parts: &[(String, usize)]| parts.iter().map(|(_, b)| b).sum::<usize>();
        assert_eq!(total(&before), idx.heap_bytes());
        for name in [
            "parent",
            "name_arena",
            "metadata.size.values",
            "metadata.modified.validity",
            "metadata.attributes.values",
        ] {
            assert!(before.iter().any(|(n, _)| n == name), "{name} missing");
        }
        let order = |parts: &[(String, usize)]| {
            parts
                .iter()
                .find(|(n, _)| n == "metadata.size.order")
                .unwrap()
                .1
        };
        assert_eq!(order(&before), 0, "a sort order is zero until prepared");
        idx.request_sort(SortKey::Size);
        let ticket = idx.sort_ticket(SortKey::Size).unwrap();
        assert_eq!(
            ticket.wait_timeout(std::time::Duration::from_secs(30)),
            SortStatus::Ready
        );
        let after = idx.heap_parts();
        assert!(order(&after) >= idx.len() * 3, "a ready order is counted");
        assert_eq!(total(&after), idx.heap_bytes());
    }
}
