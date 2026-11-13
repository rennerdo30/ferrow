//! Versioned binary persistence for [`crate::Index`].
//!
//! Layout (all integers little-endian):
//!
//! | Field | Type | Notes |
//! |---|---|---|
//! | magic | `[u8; 8]` | `FERROWID` |
//! | version | `u32` | writes 3; reads core v1 and v3 |
//! | entry_count | `u32` | |
//! | arena_len | `u32` | both arenas have this length |
//! | checksum | `u32` | CRC-32 (IEEE) over everything after this field |
//! | columns | `u32` | v3 presence mask |
//! | metadata_len | `u32` | v3 exact packed metadata length |
//! | parent | `u32` × n | |
//! | name_off | `u32` × n | |
//! | name_len | `u16` × n | |
//! | flags | `u16` × n | |
//! | arena | `u8` × arena_len | |
//! | folded_arena | `u8` × arena_len | |
//! | sorted | `u32` × n | |
//!
//! Loading reads the header, checks the file length against it, then
//! reads each section sequentially straight into its column (v3; v1 files
//! go through one buffer), checks the checksum, validates every offset and
//! never panics on bad input.

use std::fmt;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::metadata::MetadataColumns;
use crate::{ColumnSet, Index, OwnedColumns, ROOT_PARENT, fold_name};

pub const MAGIC: [u8; 8] = *b"FERROWID";
pub const VERSION: u32 = 3;
const HEADER_LEN: usize = 32;
const CHECKSUM_START: usize = 24;
/// Sane ceiling for one index file (16 GiB). A header claiming more is
/// rejected before any payload is read or allocated.
const MAX_INDEX_BYTES: u64 = 16 * 1024 * 1024 * 1024;

struct Header {
    header_len: usize,
    metadata_len: u32,
    columns: ColumnSet,
    entry_count: u32,
    arena_len: u32,
    checksum: u32,
}

/// Exact total file length for the given counts, in `u64` so the math is
/// overflow-free on 32-bit targets too.
fn expected_len(n: u32, arena: u32, metadata: u32, header: usize) -> u64 {
    header as u64 + n as u64 * 16 + arena as u64 * 2 + metadata as u64
}

fn parse_header(data: &[u8]) -> Result<Header, LoadError> {
    if data.len() < 8 {
        return Err(LoadError::Truncated);
    }
    if data[..8] != MAGIC {
        return Err(LoadError::WrongMagic);
    }
    if data.len() < 12 {
        return Err(LoadError::Truncated);
    }
    let at = |i: usize| u32::from_le_bytes(data[i..i + 4].try_into().expect("bounded header"));
    let version = at(8);
    if version != 1 && version != VERSION {
        return Err(LoadError::UnsupportedVersion(version));
    }
    let header_len = if version == 1 { 24 } else { HEADER_LEN };
    if data.len() < header_len {
        return Err(LoadError::Truncated);
    }
    let columns = if version == 1 {
        ColumnSet::NONE
    } else {
        ColumnSet::from_bits(at(24)).ok_or(LoadError::Corrupt("unknown metadata columns"))?
    };
    let metadata_len = if version == 1 { 0 } else { at(28) };
    let header = Header {
        header_len,
        metadata_len,
        columns,
        entry_count: at(12),
        arena_len: at(16),
        checksum: at(20),
    };
    let n = header.entry_count as u64;
    let count = columns.bits().count_ones() as u64;
    let minimum =
        count * (n * 4 + n.div_ceil(8) + 4) + u64::from(columns.contains(ColumnSet::SIZE)) * n;
    if (metadata_len as u64) < minimum || metadata_len as u64 > minimum + count * n * 12 {
        return Err(LoadError::Corrupt("invalid metadata payload length"));
    }
    if expected_len(
        header.entry_count,
        header.arena_len,
        metadata_len,
        header_len,
    ) > MAX_INDEX_BYTES
    {
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
                    "unsupported index version {v} (this build reads 1 and {VERSION}); rebuild the index"
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

    let metadata_len = u32::try_from(index.metadata.serialized_len())
        .map_err(|_| SaveError::TooLarge("metadata columns"))?;
    let payload_len = usize::try_from(expected_len(n32, arena32, metadata_len, HEADER_LEN))
        .map_err(|_| SaveError::TooLarge("metadata columns"))?
        - HEADER_LEN;
    let mut buf = Vec::with_capacity(HEADER_LEN + payload_len);
    buf.extend_from_slice(&MAGIC);
    buf.extend_from_slice(&VERSION.to_le_bytes());
    buf.extend_from_slice(&n32.to_le_bytes());
    buf.extend_from_slice(&arena32.to_le_bytes());
    let checksum_at = buf.len();
    buf.extend_from_slice(&0u32.to_le_bytes()); // patched below
    buf.extend_from_slice(&index.columns_present().bits().to_le_bytes());
    buf.extend_from_slice(&metadata_len.to_le_bytes());

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

    index.metadata.write(&mut buf);
    let sum = crc32(&buf[CHECKSUM_START..]);
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
    /// Reserving and initializing the payload buffer, excluding I/O.
    pub buffer: Duration,
    /// Releasing the payload buffer after decoding and validation.
    pub cleanup: Duration,
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
    /// Disjoint subspans of `derived`; do not add the parent again.
    pub derived_fold: Duration,
    pub derived_sort: Duration,
}

impl LoadTimings {
    /// Total time not covered by the named parts.
    pub fn unattributed(&self) -> Duration {
        self.total.saturating_sub(
            self.read
                + self.buffer
                + self.cleanup
                + self.checksum
                + self.decode
                + self.structure
                + self.derived,
        )
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
    let expected = expected_len(
        header.entry_count,
        header.arena_len,
        header.metadata_len,
        header.header_len,
    );
    if file_len < expected {
        return Err(LoadError::Truncated);
    }
    if file_len > expected {
        return Err(LoadError::Corrupt("unexpected trailing bytes"));
    }
    let total = usize::try_from(expected).map_err(|_| LoadError::Corrupt("size overflow"))?;
    if header.header_len == HEADER_LEN && head_len == HEADER_LEN {
        // v3: read every section straight into its final column.
        let index = stream_sections(reader, &data, &header, &mut timings)?;
        timings.total = started.elapsed();
        return Ok((index, timings));
    }
    let t = Instant::now();
    data.try_reserve_exact(total - data.len())
        .map_err(|_| LoadError::Corrupt("index too large to allocate"))?;
    data.resize(total, 0);
    timings.buffer += t.elapsed();
    let t = Instant::now();
    reader
        .read_exact(&mut data[head_len..])
        .map_err(LoadError::Io)?;
    timings.read += t.elapsed();
    let index = from_bytes_inner(&data, &mut timings)?;
    let t = Instant::now();
    drop(data);
    timings.cleanup += t.elapsed();
    timings.total = started.elapsed();
    Ok((index, timings))
}

/// Bytes per streamed read: big enough for few syscalls, small enough
/// that the CRC and the little-endian decode see each chunk in cache.
const STREAM_CHUNK: usize = 1 << 20;

/// Sequential section reader for [`stream_sections`]: every byte goes
/// through the CRC exactly once, straight after it is read.
struct Stream<'a, R> {
    reader: &'a mut R,
    crc: crc32fast::Hasher,
    chunk: Vec<u8>,
    timings: &'a mut LoadTimings,
}

impl<R: std::io::Read> Stream<'_, R> {
    /// Read exactly `len` (at most one chunk) bytes into `self.chunk`.
    fn fill(&mut self, len: usize) -> Result<(), LoadError> {
        self.chunk.resize(len, 0);
        let t = Instant::now();
        self.reader
            .read_exact(&mut self.chunk)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::UnexpectedEof => LoadError::Truncated,
                _ => LoadError::Io(e),
            })?;
        self.timings.read += t.elapsed();
        let t = Instant::now();
        self.crc.update(&self.chunk);
        self.timings.checksum += t.elapsed();
        Ok(())
    }

    fn reserve<T>(&mut self, n: usize) -> Result<Vec<T>, LoadError> {
        let t = Instant::now();
        let mut out = Vec::new();
        out.try_reserve_exact(n)
            .map_err(|_| LoadError::Corrupt("index too large to allocate"))?;
        self.timings.buffer += t.elapsed();
        Ok(out)
    }

    /// `n` little-endian values of `W` bytes each, decoded by `decode`.
    fn values<T, const W: usize>(
        &mut self,
        n: usize,
        decode: fn([u8; W]) -> T,
    ) -> Result<Vec<T>, LoadError> {
        let mut out = self.reserve(n)?;
        let mut left = n
            .checked_mul(W)
            .ok_or(LoadError::Corrupt("size overflow"))?;
        while left > 0 {
            // STREAM_CHUNK is a multiple of every width.
            let len = left.min(STREAM_CHUNK);
            self.fill(len)?;
            let t = Instant::now();
            out.extend(self.chunk.as_chunks::<W>().0.iter().map(|c| decode(*c)));
            self.timings.decode += t.elapsed();
            left -= len;
        }
        Ok(out)
    }

    /// `len` raw bytes, read directly into the returned vector.
    fn bytes(&mut self, len: usize) -> Result<Vec<u8>, LoadError> {
        use std::io::Read;
        let mut out: Vec<u8> = self.reserve(len)?;
        while out.len() < len {
            let start = out.len();
            let want = (len - start).min(STREAM_CHUNK);
            let t = Instant::now();
            let got = (&mut *self.reader)
                .take(want as u64)
                .read_to_end(&mut out)
                .map_err(LoadError::Io)?;
            self.timings.read += t.elapsed();
            if got != want {
                return Err(LoadError::Truncated);
            }
            let t = Instant::now();
            self.crc.update(&out[start..]);
            self.timings.checksum += t.elapsed();
        }
        Ok(out)
    }
}

/// Load a v3 index whose 32-byte header is already in `head`: each
/// section is read directly into the column that keeps it, with no
/// whole-file staging buffer (which would cost a second page-fault and
/// zeroing pass over the file size, a copy per column and a large free).
/// The checksum is verified before any structural validation, as in
/// [`from_bytes`]; validation is shared with it.
fn stream_sections<R: std::io::Read>(
    reader: &mut R,
    head: &[u8],
    header: &Header,
    timings: &mut LoadTimings,
) -> Result<Index, LoadError> {
    let n = header.entry_count as usize;
    let arena_len = header.arena_len as usize;
    let mut crc = crc32fast::Hasher::new();
    crc.update(&head[CHECKSUM_START..HEADER_LEN]);
    let mut s = Stream {
        reader,
        crc,
        chunk: Vec::with_capacity(STREAM_CHUNK),
        timings,
    };
    let parent = s.values(n, u32::from_le_bytes)?;
    let name_off = s.values(n, u32::from_le_bytes)?;
    let name_len = s.values(n, u16::from_le_bytes)?;
    let flags = s.values(n, u16::from_le_bytes)?;
    let arena = s.bytes(arena_len)?;
    let folded_arena = s.bytes(arena_len)?;
    let sorted = s.values(n, u32::from_le_bytes)?;
    let metadata_bytes = s.bytes(header.metadata_len as usize)?;
    let sum = s.crc.finalize();
    drop(s.chunk);
    if sum != header.checksum {
        return Err(LoadError::ChecksumMismatch);
    }
    let t = Instant::now();
    let mut at = 0;
    let metadata = MetadataColumns::read(&metadata_bytes, &mut at, n, header.columns)?;
    if at != metadata_bytes.len() {
        return Err(LoadError::Corrupt("unexpected metadata bytes"));
    }
    drop(metadata_bytes);
    timings.decode += t.elapsed();
    validate(
        Parts {
            parent,
            name_off,
            name_len,
            flags,
            arena,
            folded_arena,
            sorted,
            metadata,
        },
        timings,
    )
}

/// Decoded sections, not yet validated.
struct Parts {
    parent: Vec<u32>,
    name_off: Vec<u32>,
    name_len: Vec<u16>,
    flags: Vec<u16>,
    arena: Vec<u8>,
    folded_arena: Vec<u8>,
    sorted: Vec<u32>,
    metadata: MetadataColumns,
}

/// Parse an index from bytes (also used by tests and fuzz-style checks).
pub fn from_bytes(data: &[u8]) -> Result<Index, LoadError> {
    from_bytes_inner(data, &mut LoadTimings::default())
}

fn from_bytes_inner(data: &[u8], timings: &mut LoadTimings) -> Result<Index, LoadError> {
    let header = parse_header(data)?;
    let n = header.entry_count as usize;
    let arena_len = header.arena_len as usize;
    let expected = expected_len(
        header.entry_count,
        header.arena_len,
        header.metadata_len,
        header.header_len,
    );
    if (data.len() as u64) < expected {
        return Err(LoadError::Truncated);
    }
    if (data.len() as u64) > expected {
        return Err(LoadError::Corrupt("unexpected trailing bytes"));
    }
    let t = Instant::now();
    let sum_ok = crc32(&data[CHECKSUM_START..]) == header.checksum;
    timings.checksum += t.elapsed();
    if !sum_ok {
        return Err(LoadError::ChecksumMismatch);
    }

    let t = Instant::now();
    let mut at = header.header_len;
    let parent = read_u32s(data, &mut at, n)?;
    let name_off = read_u32s(data, &mut at, n)?;
    let name_len = read_u16s(data, &mut at, n)?;
    let ent_flags = read_u16s(data, &mut at, n)?;
    let arena = take(data, &mut at, arena_len)?.to_vec();
    let folded_arena = take(data, &mut at, arena_len)?.to_vec();
    let sorted = read_u32s(data, &mut at, n)?;
    let metadata = MetadataColumns::read(data, &mut at, n, header.columns)?;
    if at != data.len() {
        return Err(LoadError::Corrupt("unexpected metadata bytes"));
    }
    timings.decode += t.elapsed();
    validate(
        Parts {
            parent,
            name_off,
            name_len,
            flags: ent_flags,
            arena,
            folded_arena,
            sorted,
            metadata,
        },
        timings,
    )
}

/// Structural and derived-data validation shared by both load paths;
/// builds the index only from parts that pass every check.
fn validate(parts: Parts, timings: &mut LoadTimings) -> Result<Index, LoadError> {
    let Parts {
        parent,
        name_off,
        name_len,
        flags: ent_flags,
        arena,
        folded_arena,
        sorted,
        metadata,
    } = parts;
    let n = parent.len();
    let arena_len = arena.len();
    if name_off.len() != n
        || name_len.len() != n
        || ent_flags.len() != n
        || sorted.len() != n
        || folded_arena.len() != arena_len
    {
        return Err(LoadError::Corrupt("section lengths disagree"));
    }
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
    // `fold_name` maps every ASCII byte to its lowercase in place, in every
    // name (ASCII, UTF-8 and invalid UTF-8 alike). So one branchless pass
    // over both whole arenas (the compiler vectorizes it) checks all ASCII
    // positions, and only names with a non-ASCII byte need the full fold.
    const ARENA_CHUNK: usize = 1 << 16;
    arena
        .par_chunks(ARENA_CHUNK)
        .zip(folded_arena.par_chunks(ARENA_CHUNK))
        .try_for_each(|(raw, folded)| -> Result<(), LoadError> {
            let bad = raw.iter().zip(folded).fold(0u8, |acc, (&a, &b)| {
                let lower = a | (u8::from(a.wrapping_sub(b'A') < 26) << 5);
                let ascii = u8::from(a < 0x80).wrapping_neg();
                acc | ((lower ^ b) & ascii)
            });
            if bad != 0 {
                return Err(LoadError::Corrupt("folded arena inconsistent with names"));
            }
            Ok(())
        })?;
    chunk_ranges(n)
        .into_par_iter()
        .try_for_each(|(start, end)| -> Result<(), LoadError> {
            let mut folded_buf: Vec<u8> = Vec::new();
            for i in start..end {
                let raw = name_at(&arena, &name_off, &name_len, i);
                if raw.is_ascii() {
                    continue; // fully checked above
                }
                fold_name(raw, &mut folded_buf);
                if folded_buf != name_at(&folded_arena, &name_off, &name_len, i) {
                    return Err(LoadError::Corrupt("folded arena inconsistent with names"));
                }
            }
            Ok(())
        })?;
    timings.derived_fold = derived_started.elapsed();
    // Construct each key once, carrying it across adjacent comparisons.
    // Both arenas share the validated offset/length, including tie breakers.
    let key = |id: u32| {
        let off = name_off[id as usize] as usize;
        let end = off + name_len[id as usize] as usize;
        (&folded_arena[off..end], &arena[off..end], id)
    };
    chunk_ranges(n.saturating_sub(1))
        .into_par_iter()
        .try_for_each(|(start, end)| -> Result<(), LoadError> {
            // Sorted order visits names at random arena positions, and
            // each step's load chain (id → offset → name bytes) would stall
            // on the previous one. Per batch, first gather the offsets, then
            // touch every name's first byte (independent loads the CPU can
            // overlap), then compare with the data already in cache.
            const BATCH: usize = 64;
            let ids = &sorted[start..=end];
            let mut spans = [(0u32, 0u16); BATCH];
            let mut previous = key(ids[0]);
            for batch in ids[1..].chunks(BATCH) {
                for (span, &id) in spans.iter_mut().zip(batch) {
                    *span = (name_off[id as usize], name_len[id as usize]);
                }
                let mut touch = 0u8;
                for &(off, len) in &spans[..batch.len()] {
                    if len > 0 {
                        touch ^= folded_arena[off as usize];
                    }
                }
                std::hint::black_box(touch);
                for (&(off, len), &id) in spans.iter().zip(batch) {
                    let off = off as usize;
                    let end = off + len as usize;
                    let next = (&folded_arena[off..end], &arena[off..end], id);
                    if previous >= next {
                        return Err(LoadError::Corrupt("sort order is not sorted"));
                    }
                    previous = next;
                }
            }
            Ok(())
        })?;
    timings.derived += derived_started.elapsed();
    timings.derived_sort = timings.derived - timings.derived_fold;

    let mut index = Index::from_columns(OwnedColumns {
        parent,
        name_off,
        name_len,
        flags: ent_flags,
        arena,
        folded_arena,
        sorted,
    });
    index.metadata = metadata;
    Ok(index)
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
        let sum = crc32(&bytes[CHECKSUM_START..]);
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
        for v in [VERSION, n, a, 0, 0, 0] {
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
        let sum = crc32(&bytes[CHECKSUM_START..]);
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

    /// The streamed file load (several chunks per section) equals the
    /// in-memory parse, and rejects any flipped byte and any
    /// checksum-valid structural corruption, like `from_bytes`.
    #[test]
    fn streamed_load_matches_from_bytes_and_rejects_corruption() {
        let dir = test_dir("stream");
        let path = dir.join("index.bin");
        let b = IndexBuilder::new();
        let root = b.add_root(b"root", flags::DIR);
        for i in 0..300_000u32 {
            b.add(
                root,
                format!("file-{:06}.Txt", i * 7919 % 300_000).as_bytes(),
                0,
            );
        }
        b.freeze().save(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            bytes.len() > 4 * STREAM_CHUNK,
            "sections span several chunks"
        );
        let streamed = Index::load(&path).unwrap();
        let parsed = from_bytes(&bytes).unwrap();
        assert_eq!(streamed.len(), parsed.len());
        assert_eq!(streamed.sorted(), parsed.sorted());
        for id in [0u32, 1, 150_000, 300_000] {
            assert_eq!(streamed.name(id), parsed.name(id));
            assert_eq!(streamed.folded_name(id), parsed.folded_name(id));
            assert_eq!(streamed.parent(id), parsed.parent(id));
        }
        // A flipped byte in each section (parent, name_len, arena, folded
        // arena, sorted, last byte) fails the streamed checksum.
        let n = 300_001;
        let arena_len = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
        for at in [
            HEADER_LEN + 5,
            HEADER_LEN + n * 8 + 3,
            HEADER_LEN + n * 12 + arena_len / 2,
            HEADER_LEN + n * 12 + arena_len + arena_len / 2,
            HEADER_LEN + n * 12 + 2 * arena_len + 4 * (n / 2),
            bytes.len() - 1,
        ] {
            let mut bad = bytes.clone();
            bad[at] ^= 0x40;
            std::fs::write(&path, &bad).unwrap();
            assert!(
                matches!(Index::load(&path), Err(LoadError::ChecksumMismatch)),
                "flip at {at}"
            );
        }
        // Checksum-valid corruption still fails validation when streamed.
        let mut bad = three_entry_bytes();
        let at = bad.len() - 12;
        for (i, id) in [2u32, 0, 1].iter().enumerate() {
            bad[at + i * 4..at + i * 4 + 4].copy_from_slice(&id.to_le_bytes());
        }
        fix_checksum(&mut bad);
        std::fs::write(&path, &bad).unwrap();
        assert!(matches!(Index::load(&path), Err(LoadError::Corrupt(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The batched sort check finds a swapped pair anywhere: inside a
    /// batch, across a batch boundary and at a chunk's last entry.
    #[test]
    fn rejects_swapped_sort_ids_in_and_across_batches() {
        let dir = test_dir("sort-batches");
        let path = dir.join("index.bin");
        let b = IndexBuilder::new();
        let root = b.add_root(b"root", flags::DIR);
        for i in 0..5000 {
            b.add(root, format!("name-{i:05}").as_bytes(), 0);
        }
        b.freeze().save(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(from_bytes(&bytes).is_ok());
        let n = 5001;
        let arena_len = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
        let sorted_at = HEADER_LEN + n * 12 + 2 * arena_len;
        // Sorted positions p and p + 1; the batches start at position 1.
        for p in [1usize, 30, 64, 65, 128, 4000, n - 2] {
            let mut bad = bytes.clone();
            let (a, b) = (sorted_at + p * 4, sorted_at + (p + 1) * 4);
            let first: [u8; 4] = bad[a..a + 4].try_into().unwrap();
            bad.copy_within(b..b + 4, a);
            bad[b..b + 4].copy_from_slice(&first);
            fix_checksum(&mut bad);
            assert!(
                matches!(from_bytes(&bad), Err(LoadError::Corrupt(_))),
                "swap at sorted position {p}"
            );
        }
    }

    /// Every single-byte change of the folded arena is rejected, whether it
    /// hits an ASCII byte (whole-arena pass), an ASCII byte inside a
    /// non-ASCII name, a multi-byte character or invalid UTF-8.
    #[test]
    fn any_folded_byte_change_is_rejected() {
        let dir = test_dir("fold-flip");
        let path = dir.join("index.bin");
        let b = IndexBuilder::new();
        let root = b.add_root(b"Root", flags::DIR);
        for name in [
            "plain.TXT".as_bytes(),
            "Ärger-ΣΟΦΙΑ.Log".as_bytes(),
            b"raw\xffAB\xfe.bin",
            "Kelvin\u{212A}.k".as_bytes(),
        ] {
            b.add(root, name, 0);
        }
        b.freeze().save(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(from_bytes(&bytes).is_ok());
        let n = 5;
        let arena_len = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
        let folded_at = HEADER_LEN + n * 12 + arena_len;
        for at in folded_at..folded_at + arena_len {
            for change in [0x01u8, 0x20, 0x80] {
                let mut bad = bytes.clone();
                bad[at] ^= change;
                fix_checksum(&mut bad);
                assert!(
                    matches!(from_bytes(&bad), Err(LoadError::Corrupt(_))),
                    "folded byte {} ^ {change:#x} accepted",
                    at - folded_at
                );
            }
        }
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
        let parts = t.read + t.buffer + t.cleanup + t.checksum + t.decode + t.structure + t.derived;
        assert_eq!(t.derived, t.derived_fold + t.derived_sort);
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
