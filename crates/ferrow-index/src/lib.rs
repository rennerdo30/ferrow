//! Ferrow's compact in-memory file-name index.
//!
//! The layout is columnar (struct of arrays): per entry we store a parent
//! id, a name offset/length into one shared byte arena, and flags. A second
//! arena holds case-folded names at the same offsets, and one permutation
//! keeps entry ids in name-sort order. Full paths are never stored; they are
//! rebuilt from the parent chain on demand. See
//! `docs/decisions/0001-index-layout.md`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use rayon::prelude::*;

pub mod osname;
pub mod persist;

pub use persist::{LoadError, SaveError};

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
    parent: Vec<u32>,
    name_off: Vec<u32>,
    name_len: Vec<u16>,
    flags: Vec<u16>,
    arena: Vec<u8>,
    folded_arena: Vec<u8>,
    sorted: Vec<u32>,
}

impl Index {
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
    }

    /// Save to the versioned binary format. See [`persist`].
    pub fn save(&self, path: &std::path::Path) -> Result<(), SaveError> {
        persist::save(self, path)
    }

    /// Load from the versioned binary format. See [`persist`].
    pub fn load(path: &std::path::Path) -> Result<Index, LoadError> {
        persist::load(path)
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
            parent: c.parent,
            name_off: c.name_off,
            name_len: c.name_len,
            flags: c.flags,
            arena: c.arena,
            folded_arena: c.folded_arena,
            sorted: c.sorted,
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
    id: u32,
    parent: u32,
    name: Vec<u8>,
    flags: u16,
}

const SHARDS: usize = 16;

/// Thread-safe builder. Entries may be added from many threads in any
/// order; sharding keeps lock contention low. Ids are handed out densely,
/// so parent ids returned by [`IndexBuilder::add`] stay valid after
/// [`IndexBuilder::freeze`].
pub struct IndexBuilder {
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
        IndexBuilder {
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
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let shard = self.next_shard.fetch_add(1, Ordering::Relaxed) % SHARDS;
        let mut flags = flags;
        if std::str::from_utf8(name).is_err() {
            flags |= flags::NAME_NOT_UTF8;
        }
        let entry = RawEntry {
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

        // Folded arena: same offsets and lengths as `arena` (see fold_name).
        let folded_arena: Vec<u8> = {
            let slices: Vec<&[u8]> = (0..n)
                .map(|i| {
                    let off = name_off[i] as usize;
                    &arena[off..off + name_len[i] as usize]
                })
                .collect();
            slices
                .par_iter()
                .flat_map_iter(|name| {
                    let mut folded = Vec::with_capacity(name.len());
                    fold_name(name, &mut folded);
                    folded.into_iter()
                })
                .collect()
        };

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

        Index {
            parent,
            name_off,
            name_len,
            flags: ent_flags,
            arena,
            folded_arena,
            sorted,
        }
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
        let idx = small_index();
        assert!(idx.heap_bytes() > idx.len() * 16);
    }
}
