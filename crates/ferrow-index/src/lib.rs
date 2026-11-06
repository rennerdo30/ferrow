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
use metadata::MetadataColumns;
pub use metadata::{
    ColumnSet, EntryMetadata, MetadataOrder, SortKey, SortStatus, SortTicket, metadata_timestamp,
    timestamp_ticks,
};

pub use persist::{LoadError, LoadTimings, SaveError};

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
    /// Ready metadata order; `None` means the column was not indexed.
    pub fn metadata_order(&self, key: SortKey, descending: bool) -> Option<MetadataOrder<'_>> {
        self.metadata.order(key, descending)
    }
    /// Start preparation once, without waiting, allocating by entry count, or
    /// borrowing the query pool. Pending searches can keep using name order.
    pub fn request_sort(&self, key: SortKey) -> SortStatus {
        self.metadata.request(key, self.sorted.clone())
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

    /// Save to the versioned binary format. See [`persist`].
    pub fn save(&self, path: &std::path::Path) -> Result<(), SaveError> {
        persist::save(self, path)
    }

    /// Load from the versioned binary format. See [`persist`].
    pub fn load(path: &std::path::Path) -> Result<Index, LoadError> {
        persist::load(path)
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

struct RawEntry {
    metadata: EntryMetadata,
    id: u32,
    parent: u32,
    name: Vec<u8>,
    flags: u16,
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
    shards: Vec<Mutex<Vec<RawEntry>>>,
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
            shards: (0..SHARDS).map(|_| Mutex::new(Vec::new())).collect(),
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
        let entry = RawEntry {
            metadata,
            id,
            parent,
            name: name.to_vec(),
            flags,
        };
        self.shards[shard]
            .lock()
            .expect("builder shard poisoned")
            .push(entry);
        id
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
        let mut raw: Vec<RawEntry> = Vec::with_capacity(n);
        for shard in self.shards {
            raw.extend(shard.into_inner().expect("builder shard poisoned"));
        }
        raw.sort_unstable_by_key(|e| e.id);
        debug_assert_eq!(raw.len(), n);

        let mut parent = Vec::with_capacity(n);
        let mut name_off = Vec::with_capacity(n);
        let mut name_len = Vec::with_capacity(n);
        let mut ent_flags = Vec::with_capacity(n);
        let total_name: usize = raw
            .iter()
            .map(|e| e.name.len().min(u16::MAX as usize))
            .sum();
        let mut arena = Vec::with_capacity(total_name);
        for e in &raw {
            let take = e.name.len().min(u16::MAX as usize);
            parent.push(e.parent);
            name_off.push(arena.len() as u32);
            name_len.push(take as u16);
            ent_flags.push(e.flags);
            arena.extend_from_slice(&e.name[..take]);
        }

        // Release builder entries before folding/sorting the frozen columns.
        let t = std::time::Instant::now();
        let metadata = MetadataColumns::build(&raw, self.columns);
        let metadata_collect = t.elapsed();
        drop(raw);
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
}
