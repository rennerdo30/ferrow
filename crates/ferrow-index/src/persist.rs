//! Versioned binary persistence for [`crate::Index`].
//!
//! Layout (all integers little-endian):
//!
//! | Field | Type | Notes |
//! |---|---|---|
//! | magic | `[u8; 8]` | `FERROWID` |
//! | version | `u32` | currently 1 |
//! | entry_count | `u32` | |
//! | arena_len | `u32` | both arenas have this length |
//! | checksum | `u32` | CRC-32 (IEEE) over everything after this field |
//! | parent | `u32` × n | |
//! | name_off | `u32` × n | |
//! | name_len | `u16` × n | |
//! | flags | `u16` × n | |
//! | arena | `u8` × arena_len | |
//! | folded_arena | `u8` × arena_len | |
//! | sorted | `u32` × n | |
//!
//! Loading reads the file in one sequential read, checks magic, version
//! and checksum, validates every offset, and never panics on bad input.

use std::fmt;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::{Index, OwnedColumns, ROOT_PARENT, fold_name};

pub const MAGIC: [u8; 8] = *b"FERROWID";
pub const VERSION: u32 = 1;
const HEADER_LEN: usize = 8 + 4 + 4 + 4 + 4;
/// Sane ceiling for one index file (16 GiB). A header claiming more is
/// rejected before any payload is read or allocated.
const MAX_INDEX_BYTES: u64 = 16 * 1024 * 1024 * 1024;

struct Header {
    entry_count: u32,
    arena_len: u32,
    checksum: u32,
}

/// Exact total file length for the given counts, in `u64` so the math is
/// overflow-free on 32-bit targets too.
fn expected_len(entry_count: u32, arena_len: u32) -> u64 {
    HEADER_LEN as u64 + entry_count as u64 * 16 + arena_len as u64 * 2
}

fn parse_header(data: &[u8]) -> Result<Header, LoadError> {
    if data.len() < HEADER_LEN {
        // Too short for a header: bad magic if the prefix disagrees, else truncated.
        if data.len() >= MAGIC.len() && data[..MAGIC.len()] != MAGIC {
            return Err(LoadError::WrongMagic);
        }
        return Err(LoadError::Truncated);
    }
    if data[..8] != MAGIC {
        return Err(LoadError::WrongMagic);
    }
    let u32at =
        |at: usize| u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]);
    let version = u32at(8);
    if version != VERSION {
        return Err(LoadError::UnsupportedVersion(version));
    }
    let header = Header {
        entry_count: u32at(12),
        arena_len: u32at(16),
        checksum: u32at(20),
    };
    if expected_len(header.entry_count, header.arena_len) > MAX_INDEX_BYTES {
        return Err(LoadError::Corrupt("index size exceeds limit"));
    }
    Ok(header)
}

#[derive(Debug)]
pub enum SaveError {
    Io(std::io::Error),
    TooLarge(&'static str),
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SaveError::Io(e) => write!(f, "writing index: {e}"),
            SaveError::TooLarge(what) => write!(f, "index too large to save: {what}"),
        }
    }
}
impl std::error::Error for SaveError {}

#[derive(Debug)]
pub enum LoadError {
    Io(std::io::Error),
    WrongMagic,
    UnsupportedVersion(u32),
    Truncated,
    ChecksumMismatch,
    Corrupt(&'static str),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Io(e) => write!(f, "reading index: {e}"),
            LoadError::WrongMagic => write!(f, "not a Ferrow index file (bad magic)"),
            LoadError::UnsupportedVersion(v) => {
                write!(
                    f,
                    "unsupported index version {v} (this build reads {VERSION})"
                )
            }
            LoadError::Truncated => write!(f, "index file is truncated"),
            LoadError::ChecksumMismatch => write!(f, "index file checksum mismatch (corrupt)"),
            LoadError::Corrupt(what) => write!(f, "index file is corrupt: {what}"),
        }
    }
}
impl std::error::Error for LoadError {}

// IEEE CRC-32, unchanged on disk. crc32fast (MIT OR Apache-2.0)
// selects hardware acceleration when available, with a portable fallback.
fn crc32(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}

pub(crate) fn save(index: &Index, path: &Path) -> Result<(), SaveError> {
    let c = index.columns();
    let n = c.parent.len();
    let n32 = u32::try_from(n).map_err(|_| SaveError::TooLarge("entry count"))?;
    let arena32 = u32::try_from(c.arena.len()).map_err(|_| SaveError::TooLarge("name arena"))?;

    let payload_len = n * (4 + 4 + 2 + 2 + 4) + c.arena.len() * 2;
    let mut buf = Vec::with_capacity(HEADER_LEN + payload_len);
    buf.extend_from_slice(&MAGIC);
    buf.extend_from_slice(&VERSION.to_le_bytes());
    buf.extend_from_slice(&n32.to_le_bytes());
    buf.extend_from_slice(&arena32.to_le_bytes());
    let checksum_at = buf.len();
    buf.extend_from_slice(&0u32.to_le_bytes()); // patched below

    for v in c.parent {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    for v in c.name_off {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    for v in c.name_len {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    for v in c.flags {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    buf.extend_from_slice(c.arena);
    buf.extend_from_slice(c.folded_arena);
    for v in c.sorted {
        buf.extend_from_slice(&v.to_le_bytes());
    }

    let sum = crc32(&buf[HEADER_LEN..]);
    buf[checksum_at..checksum_at + 4].copy_from_slice(&sum.to_le_bytes());

    write_atomically(path, &buf).map_err(SaveError::Io)
}

static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Write `bytes` to `path` atomically: a unique temp file is created
/// exclusively beside the destination, written and synced, then renamed
/// over the destination. The temp file is removed on any error. On Unix
/// the temp file is mode 0600, so the replaced index stays private.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => std::path::PathBuf::from("."),
    };
    let pid = std::process::id();
    for _ in 0..100 {
        let seq = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        // Bounded, destination-independent basename: embedding the
        // destination name would push near-limit valid names over the
        // filesystem component limit.
        let tmp = parent.join(format!(".ferrow-{pid}-{seq}-{nonce:016x}.tmp"));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = match opts.open(&tmp) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let written = (|| -> std::io::Result<()> {
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&tmp, path)
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not create a unique temporary file for the index",
    ))
}

fn take<'a>(data: &'a [u8], at: &mut usize, len: usize) -> Result<&'a [u8], LoadError> {
    let end = at
        .checked_add(len)
        .ok_or(LoadError::Corrupt("size overflow"))?;
    if end > data.len() {
        return Err(LoadError::Truncated);
    }
    let s = &data[*at..end];
    *at = end;
    Ok(s)
}

fn read_u32s(data: &[u8], at: &mut usize, n: usize) -> Result<Vec<u32>, LoadError> {
    let bytes = take(data, at, n.saturating_mul(4))?;
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect())
}

fn read_u16s(data: &[u8], at: &mut usize, n: usize) -> Result<Vec<u16>, LoadError> {
    let bytes = take(data, at, n.saturating_mul(2))?;
    Ok(bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect())
}

/// Additive breakdown of one index load: disjoint measured spans of
/// [`LoadTimings::total`]. [`LoadTimings::unattributed`] is whatever
/// time the named parts do not cover, so the parts always add up.
#[derive(Debug, Clone, Copy, Default)]
pub struct LoadTimings {
    /// Whole load, wall clock.
    pub total: Duration,
    /// Reading the header and payload from the source.
    pub read: Duration,
    /// CRC-32 over the payload.
    pub checksum: Duration,
    /// Decoding columns and arenas out of the payload bytes.
    pub decode: Duration,
    /// Structural validation: packed name layout, id ranges, parent
    /// cycles, sort-order uniqueness.
    pub structure: Duration,
    /// Derived-data validation: folded-arena consistency and
    /// adjacent sort-order checks (parallel, in chunks).
    pub derived: Duration,
}

impl LoadTimings {
    /// Total time not covered by the named parts.
    pub fn unattributed(&self) -> Duration {
        self.total
            .saturating_sub(self.read + self.checksum + self.decode + self.structure + self.derived)
    }
}

pub(crate) fn load(path: &Path) -> Result<Index, LoadError> {
    Ok(load_with_timings(path)?.0)
}

pub(crate) fn load_with_timings(path: &Path) -> Result<(Index, LoadTimings), LoadError> {
    let started = Instant::now();
    let mut file = std::fs::File::open(path).map_err(LoadError::Io)?;
    let file_len = file.metadata().map_err(LoadError::Io)?.len();
    let (index, mut timings) = load_from(&mut file, file_len)?;
    drop(file);
    // Include open/metadata, temporary payload disposal and file close in
    // the API wall interval; the named parts remain disjoint subsets.
    timings.total = started.elapsed();
    Ok((index, timings))
}

/// Read an index from `reader`, where `file_len` is the total source
/// length. Only the bounded header is read before the expected total
/// length is validated against `file_len`, so a hostile header never
/// triggers a payload read or a large allocation.
fn load_from<R: std::io::Read>(
    reader: &mut R,
    file_len: u64,
) -> Result<(Index, LoadTimings), LoadError> {
    let started = Instant::now();
    let mut timings = LoadTimings::default();
    // Read up to the header first; a shorter source is handled by
    // parse_header (wrong magic vs truncated).
    let head_len = usize::try_from(file_len.min(HEADER_LEN as u64)).unwrap_or(HEADER_LEN);
    let mut data = vec![0u8; head_len];
    let t = Instant::now();
    reader.read_exact(&mut data).map_err(LoadError::Io)?;
    timings.read += t.elapsed();
    let header = parse_header(&data)?;
    let expected = expected_len(header.entry_count, header.arena_len);
    if file_len < expected {
        return Err(LoadError::Truncated);
    }
    if file_len > expected {
        return Err(LoadError::Corrupt("unexpected trailing bytes"));
    }
    let total = usize::try_from(expected).map_err(|_| LoadError::Corrupt("size overflow"))?;
    data.try_reserve_exact(total - data.len())
        .map_err(|_| LoadError::Corrupt("index too large to allocate"))?;
    data.resize(total, 0);
    let t = Instant::now();
    reader
        .read_exact(&mut data[head_len..])
        .map_err(LoadError::Io)?;
    timings.read += t.elapsed();
    let index = from_bytes_inner(&data, &mut timings)?;
    drop(data);
    timings.total = started.elapsed();
    Ok((index, timings))
}

/// Parse an index from bytes (also used by tests and fuzz-style checks).
pub fn from_bytes(data: &[u8]) -> Result<Index, LoadError> {
    from_bytes_inner(data, &mut LoadTimings::default())
}

fn from_bytes_inner(data: &[u8], timings: &mut LoadTimings) -> Result<Index, LoadError> {
    let header = parse_header(data)?;
    let n = header.entry_count as usize;
    let arena_len = header.arena_len as usize;
    let expected = expected_len(header.entry_count, header.arena_len);
    if (data.len() as u64) < expected {
        return Err(LoadError::Truncated);
    }
    if (data.len() as u64) > expected {
        return Err(LoadError::Corrupt("unexpected trailing bytes"));
    }
    let t = Instant::now();
    let sum_ok = crc32(&data[HEADER_LEN..]) == header.checksum;
    timings.checksum += t.elapsed();
    if !sum_ok {
        return Err(LoadError::ChecksumMismatch);
    }

    let t = Instant::now();
    let mut at = HEADER_LEN;
    let parent = read_u32s(data, &mut at, n)?;
    let name_off = read_u32s(data, &mut at, n)?;
    let name_len = read_u16s(data, &mut at, n)?;
    let ent_flags = read_u16s(data, &mut at, n)?;
    let arena = take(data, &mut at, arena_len)?.to_vec();
    let folded_arena = take(data, &mut at, arena_len)?.to_vec();
    let sorted = read_u32s(data, &mut at, n)?;
    timings.decode += t.elapsed();
    let structure_started = Instant::now();

    // Require the canonical packed name layout the writer produces
    // (freeze appends names in entry-id order): names contiguous,
    // each offset equal to the running sum of previous lengths, total
    // exactly the arena length. Checked with u64 arithmetic before any
    // content validation, so shared/overlapping ranges cannot make
    // folding and sort-key work grow with the sum of referenced name
    // lengths instead of the stored arena size.
    let mut running: u64 = 0;
    for i in 0..n {
        if name_off[i] as u64 != running {
            return Err(LoadError::Corrupt("name layout is not packed"));
        }
        running = running
            .checked_add(name_len[i] as u64)
            .ok_or(LoadError::Corrupt("size overflow"))?;
    }
    if running != arena_len as u64 {
        return Err(LoadError::Corrupt("name layout is not packed"));
    }

    // Validate so later lookups can never panic on a crafted file.
    for i in 0..n {
        if parent[i] != ROOT_PARENT && parent[i] as usize >= n {
            return Err(LoadError::Corrupt("parent id out of range"));
        }
        // u64 math: overflow-free even on 32-bit targets.
        let end = name_off[i] as u64 + name_len[i] as u64;
        if end > arena_len as u64 {
            return Err(LoadError::Corrupt("name offset out of range"));
        }
        if sorted[i] as usize >= n {
            return Err(LoadError::Corrupt("sort order id out of range"));
        }
    }
    // Reject parent cycles, which would loop forever in path rebuild:
    // every chain must reach a root within n steps. O(n * depth) worst
    // case is too slow for big files, so walk with a visiting mark.
    let mut state = vec![0u8; n]; // 0 = unseen, 1 = on current walk, 2 = done
    let mut walk: Vec<usize> = Vec::new(); // reused: no per-entry allocation
    for start in 0..n {
        if state[start] != 0 {
            continue;
        }
        let mut cur = start;
        walk.clear();
        loop {
            if state[cur] == 1 {
                return Err(LoadError::Corrupt("parent cycle"));
            }
            if state[cur] == 2 || parent[cur] == ROOT_PARENT {
                if state[cur] == 0 {
                    state[cur] = 2;
                }
                break;
            }
            state[cur] = 1;
            walk.push(cur);
            cur = parent[cur] as usize;
        }
        for &id in &walk {
            state[id] = 2;
        }
    }

    // Sort-order uniqueness is structural and stays serial (a single
    // pass over a bool per entry); the packed/range/cycle checks above
    // plus this one make up the structure span.
    let mut seen = vec![false; n];
    for &id in &sorted {
        if seen[id as usize] {
            return Err(LoadError::Corrupt("duplicate id in sort order"));
        }
        seen[id as usize] = true;
    }
    timings.structure += structure_started.elapsed();

    // Validate the derived data instead of trusting it (ADR 0001:
    // validation chosen over rebuild — under the packed layout above,
    // folding plus adjacent sort-key comparisons cost O(n + arena
    // bytes), while rebuilding would pay a full parallel sort). Both
    // checks run in parallel over entry-id-aligned chunks, which also
    // keeps each chunk's arena access sequential for the fold check.
    let derived_started = Instant::now();
    fn name_at<'a>(arena: &'a [u8], name_off: &[u32], name_len: &[u16], i: usize) -> &'a [u8] {
        let off = name_off[i] as usize;
        &arena[off..off + name_len[i] as usize]
    }
    // Chunked ranges over `0..count`, sized for a few chunks per
    // thread with a floor so small inputs stay in one chunk.
    fn chunk_ranges(count: usize) -> Vec<(usize, usize)> {
        if count == 0 {
            return Vec::new();
        }
        let threads = rayon::current_num_threads().max(1);
        let chunk = count.div_ceil(threads * 4).max(1024);
        (0..count.div_ceil(chunk))
            .map(|c| (c * chunk, ((c + 1) * chunk).min(count)))
            .collect()
    }
    chunk_ranges(n)
        .into_par_iter()
        .try_for_each(|(start, end)| -> Result<(), LoadError> {
            let mut folded_buf: Vec<u8> = Vec::new();
            for i in start..end {
                fold_name(name_at(&arena, &name_off, &name_len, i), &mut folded_buf);
                if folded_buf != name_at(&folded_arena, &name_off, &name_len, i) {
                    return Err(LoadError::Corrupt("folded arena inconsistent with names"));
                }
            }
            Ok(())
        })?;
    let key_cmp = |a: u32, b: u32| {
        name_at(&folded_arena, &name_off, &name_len, a as usize)
            .cmp(name_at(&folded_arena, &name_off, &name_len, b as usize))
            .then_with(|| {
                name_at(&arena, &name_off, &name_len, a as usize)
                    .cmp(name_at(&arena, &name_off, &name_len, b as usize))
            })
            .then_with(|| a.cmp(&b))
    };
    chunk_ranges(n.saturating_sub(1))
        .into_par_iter()
        .try_for_each(|(start, end)| -> Result<(), LoadError> {
            for i in start..end {
                if key_cmp(sorted[i], sorted[i + 1]) != std::cmp::Ordering::Less {
                    return Err(LoadError::Corrupt("sort order is not sorted"));
                }
            }
            Ok(())
        })?;
    timings.derived += derived_started.elapsed();

    Ok(Index::from_columns(OwnedColumns {
        parent,
        name_off,
        name_len,
        flags: ent_flags,
        arena,
        folded_arena,
        sorted,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IndexBuilder, flags};

    fn sample_bytes() -> Vec<u8> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let b = IndexBuilder::new();
        let root = b.add_root(b"root", flags::DIR);
        b.add(root, b"hello.txt", 0);
        b.add(root, "Ärger.log".as_bytes(), 0);
        let dir = std::env::temp_dir().join(format!(
            "ferrow-persist-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let idx = b.freeze();
        idx.save(&dir).unwrap();
        let bytes = std::fs::read(&dir).unwrap();
        let _ = std::fs::remove_file(&dir);
        bytes
    }

    #[test]
    fn round_trip() {
        let bytes = sample_bytes();
        let idx = from_bytes(&bytes).unwrap();
        assert_eq!(idx.len(), 3);
        assert_eq!(idx.name(1), b"hello.txt");
        assert_eq!(idx.parent(1), 0);
        let mut buf = Vec::new();
        idx.path_bytes(2, &mut buf);
        assert!(buf.ends_with("Ärger.log".as_bytes()));
    }

    #[test]
    fn rejects_unexpected_trailing_payload() {
        let mut bytes = sample_bytes();
        bytes.extend_from_slice(&[0u8; 4096]);
        assert!(
            matches!(from_bytes(&bytes), Err(LoadError::Corrupt(_))),
            "trailing bytes must be rejected, got {:?}",
            from_bytes(&bytes).map(|i| i.len())
        );
    }

    #[test]
    fn rejects_oversized_counts_before_allocating() {
        // Header claims the maximum counts; parsing must refuse on size
        // grounds without trying to size buffers from them.
        let mut bytes = sample_bytes();
        bytes[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(
            matches!(from_bytes(&bytes), Err(LoadError::Corrupt(_))),
            "oversized counts must be rejected as corrupt, got {:?}",
            from_bytes(&bytes).map(|i| i.len())
        );
    }

    #[test]
    fn load_rejects_oversized_metadata_before_payload_read() {
        // Injected counting reader: the header claims huge counts and the
        // declared source length is small. Loading must stop after the
        // header without reading (let alone allocating) the payload.
        struct Counting<'a> {
            inner: &'a [u8],
            read: usize,
        }
        impl std::io::Read for Counting<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = self.inner.read(buf)?;
                self.read += n;
                Ok(n)
            }
        }
        let mut bytes = sample_bytes();
        bytes[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut reader = Counting {
            inner: &bytes,
            read: 0,
        };
        let result = load_from(&mut reader, bytes.len() as u64);
        assert!(matches!(result, Err(LoadError::Corrupt(_))), "{result:?}");
        assert_eq!(reader.read, HEADER_LEN, "payload was read before rejection");

        // Same for a length mismatch against a plausible header.
        let bytes = sample_bytes();
        let mut reader = Counting {
            inner: &bytes,
            read: 0,
        };
        let result = load_from(&mut reader, bytes.len() as u64 + 4096);
        assert!(matches!(result, Err(LoadError::Corrupt(_))), "{result:?}");
        assert_eq!(reader.read, HEADER_LEN, "payload was read before rejection");
    }

    fn three_entry_bytes() -> Vec<u8> {
        let dir = test_dir("three");
        let path = dir.join("index.bin");
        let b = IndexBuilder::new();
        let root = b.add_root(b"root", flags::DIR);
        b.add(root, b"apple", 0);
        b.add(root, b"zebra", 0);
        b.freeze().save(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        bytes
    }

    fn fix_checksum(bytes: &mut [u8]) {
        let sum = crc32(&bytes[HEADER_LEN..]);
        bytes[20..24].copy_from_slice(&sum.to_le_bytes());
    }

    #[test]
    fn rejects_duplicate_sort_ids_with_valid_checksum() {
        let mut bytes = three_entry_bytes();
        let at = bytes.len() - 12;
        for off in [at, at + 4, at + 8] {
            bytes[off..off + 4].copy_from_slice(&1u32.to_le_bytes());
        }
        fix_checksum(&mut bytes);
        assert!(
            matches!(from_bytes(&bytes), Err(LoadError::Corrupt(_))),
            "duplicate sort ids must be rejected"
        );
    }

    #[test]
    fn rejects_unsorted_permutation_with_valid_checksum() {
        let mut bytes = three_entry_bytes();
        let at = bytes.len() - 12;
        // Valid permutation, wrong order (zebra, root, apple).
        for (i, id) in [2u32, 0, 1].iter().enumerate() {
            bytes[at + i * 4..at + i * 4 + 4].copy_from_slice(&id.to_le_bytes());
        }
        fix_checksum(&mut bytes);
        assert!(
            matches!(from_bytes(&bytes), Err(LoadError::Corrupt(_))),
            "unsorted permutation must be rejected"
        );
    }

    #[test]
    fn rejects_inconsistent_folded_arena_with_valid_checksum() {
        let mut bytes = three_entry_bytes();
        let arena_len = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
        let folded_at = HEADER_LEN + 3 * 12 + arena_len;
        bytes[folded_at..folded_at + arena_len].fill(b'x');
        fix_checksum(&mut bytes);
        assert!(
            matches!(from_bytes(&bytes), Err(LoadError::Corrupt(_))),
            "inconsistent folded arena must be rejected"
        );
    }

    #[test]
    fn rejects_overlapping_name_ranges_with_valid_checksum() {
        // Three entries all referencing the same 255-byte name range.
        // The writer only ever produces the canonical packed layout
        // (names contiguous in entry-id order), so this must be
        // rejected before any folding work happens.
        let n = 3u32;
        let a = 255u32;
        let mut b = Vec::new();
        b.extend_from_slice(b"FERROWID");
        for v in [1, n, a, 0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        for _ in 0..n {
            b.extend_from_slice(&u32::MAX.to_le_bytes());
        }
        for _ in 0..n {
            b.extend_from_slice(&0u32.to_le_bytes());
        }
        for _ in 0..n {
            b.extend_from_slice(&(a as u16).to_le_bytes());
        }
        for _ in 0..n {
            b.extend_from_slice(&0u16.to_le_bytes());
        }
        b.extend(std::iter::repeat_n(b'a', a as usize * 2));
        for id in 0..n {
            b.extend_from_slice(&id.to_le_bytes());
        }
        fix_checksum(&mut b);
        assert!(
            matches!(from_bytes(&b), Err(LoadError::Corrupt(_))),
            "overlapping name ranges must be rejected, got {:?}",
            from_bytes(&b).map(|i| i.len())
        );
    }

    #[test]
    fn rejects_wrong_magic() {
        let mut bytes = sample_bytes();
        bytes[0] = b'X';
        assert!(matches!(from_bytes(&bytes), Err(LoadError::WrongMagic)));
    }

    #[test]
    fn rejects_wrong_version() {
        let mut bytes = sample_bytes();
        bytes[8..12].copy_from_slice(&999u32.to_le_bytes());
        assert!(matches!(
            from_bytes(&bytes),
            Err(LoadError::UnsupportedVersion(999))
        ));
    }

    #[test]
    fn rejects_truncated() {
        let bytes = sample_bytes();
        assert!(matches!(
            from_bytes(&bytes[..bytes.len() - 1]),
            Err(LoadError::Truncated)
        ));
        assert!(matches!(from_bytes(&[]), Err(LoadError::Truncated)));
        assert!(matches!(from_bytes(b"FER"), Err(LoadError::Truncated)));
    }

    #[test]
    fn rejects_corrupt_payload() {
        let mut bytes = sample_bytes();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        assert!(matches!(
            from_bytes(&bytes),
            Err(LoadError::ChecksumMismatch)
        ));
    }

    #[test]
    fn rejects_parent_cycle_even_with_valid_checksum() {
        let dir = std::env::temp_dir().join(format!("ferrow-cycle-test-{}", std::process::id()));
        let b = IndexBuilder::new();
        let root = b.add_root(b"root", flags::DIR);
        b.add(root, b"a", 0);
        let idx = b.freeze();
        idx.save(&dir).unwrap();
        let mut bytes = std::fs::read(&dir).unwrap();
        let _ = std::fs::remove_file(&dir);
        // Point entry 1's parent at itself (parent column starts at HEADER_LEN).
        bytes[HEADER_LEN + 4..HEADER_LEN + 8].copy_from_slice(&1u32.to_le_bytes());
        let sum = crc32(&bytes[HEADER_LEN..]);
        bytes[20..24].copy_from_slice(&sum.to_le_bytes());
        assert!(matches!(from_bytes(&bytes), Err(LoadError::Corrupt(_))));
    }

    fn test_dir(tag: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1000);
        let dir = std::env::temp_dir().join(format!(
            "ferrow-persist-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tiny_index(name: &[u8]) -> Index {
        let b = IndexBuilder::new();
        let root = b.add_root(b"root", flags::DIR);
        b.add(root, name, 0);
        b.freeze()
    }

    #[test]
    fn save_preserves_unrelated_sibling_tmp() {
        let dir = test_dir("sibling-tmp");
        let path = dir.join("index.bin");
        let sibling = path.with_extension("tmp");
        std::fs::write(&sibling, b"unrelated owner data").unwrap();
        tiny_index(b"a.txt").save(&path).unwrap();
        assert_eq!(
            std::fs::read(&sibling).ok(),
            Some(b"unrelated owner data".to_vec()),
            "save destroyed an unrelated sibling index.tmp"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_to_tmp_named_destination_is_atomic() {
        let dir = test_dir("tmp-dest");
        let path = dir.join("index.tmp");
        std::fs::write(&path, b"existing target").unwrap();
        tiny_index(b"a.txt").save(&path).unwrap();
        let loaded = Index::load(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.name(1), b"a.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_saves_do_not_collide_or_corrupt() {
        let dir = test_dir("concurrent");
        let path = dir.join("index.bin");
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let name = format!("file-{i}.txt");
                    tiny_index(name.as_bytes()).save(&path)
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap().unwrap();
        }
        let loaded = Index::load(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        let names: Vec<String> = (0..8).map(|i| format!("file-{i}.txt")).collect();
        let final_name = String::from_utf8(loaded.name(1).to_vec()).unwrap();
        assert!(names.contains(&final_name), "unexpected name {final_name}");
        // No leftover temp files beside the destination.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path() != path)
            .collect();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_save_leaves_destination_intact_and_no_temp() {
        let dir = test_dir("fault");
        // Destination is a non-empty directory: rename over it must fail on
        // every platform, which exercises the error/cleanup path.
        let dest = dir.join("index.bin");
        std::fs::create_dir_all(dest.join("child")).unwrap();
        std::fs::write(dest.join("child").join("keep.txt"), b"keep").unwrap();
        let result = tiny_index(b"a.txt").save(&dest);
        assert!(result.is_err(), "expected save to fail, got {result:?}");
        assert_eq!(
            std::fs::read(dest.join("child").join("keep.txt")).unwrap(),
            b"keep"
        );
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path() != dest)
            .collect();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn saved_index_is_private_under_permissive_umask() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: umask is process-wide; tests in this binary that create
        // files do not depend on permission bits except this one.
        let old = unsafe { libc_umask(0) };
        let dir = test_dir("private");
        let path = dir.join("index.bin");
        let result = tiny_index(b"a.txt").save(&path);
        // SAFETY: restoring the previous umask value obtained above.
        unsafe { libc_umask(old) };
        result.unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "index mode was {mode:o}");
        // Replacement keeps the private mode.
        // SAFETY: as above.
        let old = unsafe { libc_umask(0) };
        let result = tiny_index(b"b.txt").save(&path);
        // SAFETY: restoring the previous umask value obtained above.
        unsafe { libc_umask(old) };
        result.unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "replaced index mode was {mode:o}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    unsafe fn libc_umask(mask: u32) -> u32 {
        // Minimal umask(2) declaration to avoid a libc dependency.
        unsafe extern "C" {
            fn umask(mask: u32) -> u32;
        }
        // SAFETY: umask always succeeds and takes the mask by value.
        unsafe { umask(mask) }
    }

    #[test]
    fn save_accepts_near_limit_basenames() {
        // 230 ASCII chars and a multibyte name near the 255-byte Unix
        // component limit (120 * 2 bytes + 4). Both are valid
        // destinations; the temp name must not push them over the limit.
        for (tag, base) in [
            ("long-ascii", format!("{}.bin", "a".repeat(226))),
            ("long-multibyte", format!("{}.bin", "ä".repeat(120))),
        ] {
            let dir = test_dir(tag);
            let dest = dir.join(&base);
            std::fs::write(&dest, b"existing target").unwrap();
            let sibling = dir.join("keep.txt");
            std::fs::write(&sibling, b"keep").unwrap();
            tiny_index(b"a.txt")
                .save(&dest)
                .unwrap_or_else(|e| panic!("valid existing destination {base:?} refused: {e}"));
            let loaded = Index::load(&dest).unwrap();
            assert_eq!(loaded.len(), 2);
            assert_eq!(loaded.name(1), b"a.txt");
            assert_eq!(std::fs::read(&sibling).unwrap(), b"keep");
            let leftovers: Vec<_> = std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.path() != dest && e.path() != sibling)
                .collect();
            assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn load_with_timings_reports_additive_parts() {
        let dir = test_dir("timings");
        let path = dir.join("index.bin");
        tiny_index(b"a.txt").save(&path).unwrap();
        let (loaded, t) = Index::load_with_timings(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        // The parts are disjoint measured spans of the total, so they
        // must add up to at most the total (the rest is unattributed).
        let parts = t.read + t.checksum + t.decode + t.structure + t.derived;
        assert!(
            parts <= t.total,
            "parts {parts:?} exceed total {:?}",
            t.total
        );
        assert_eq!(t.unattributed(), t.total - parts);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_corrupt_folded_name_past_first_chunk() {
        // Derived-data validation runs in parallel chunks; a mismatch
        // in a late chunk must still be rejected.
        let dir = test_dir("late-chunk");
        let path = dir.join("index.bin");
        let b = IndexBuilder::new();
        let root = b.add_root(b"root", flags::DIR);
        for i in 0..3000 {
            b.add(root, format!("file-{i:04}.txt").as_bytes(), 0);
        }
        b.freeze().save(&path).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let n = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let arena_len = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
        let folded_last = HEADER_LEN + n * 12 + arena_len * 2 - 1;
        bytes[folded_last] ^= 1;
        fix_checksum(&mut bytes);
        assert!(
            matches!(from_bytes(&bytes), Err(LoadError::Corrupt(_))),
            "corrupt folded name in a late chunk must be rejected"
        );
    }

    #[test]
    fn crc32_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
        assert_eq!(crc32(b""), 0);
        // Independent bit-at-a-time IEEE oracle: exercise hardware-sized
        // inputs, tails and unaligned slices without timing or load loops.
        let data: Vec<u8> = (0..513).map(|i| (i * 37) as u8).collect();
        for bytes in [&data[..], &data[1..], &data[3..510], &data[..17]] {
            let mut expected = !0u32;
            for &b in bytes {
                expected ^= u32::from(b);
                for _ in 0..8 {
                    expected = (expected >> 1) ^ (0xEDB88320 & 0u32.wrapping_sub(expected & 1));
                }
            }
            assert_eq!(crc32(bytes), !expected);
        }
    }
}
