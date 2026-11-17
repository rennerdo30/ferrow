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

use std::cell::Cell;
use std::fmt;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
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

/// The saved file's length, which is also the size of the one buffer
/// [`save`] fills before writing (memory accounting).
pub(crate) fn serialized_len(index: &Index) -> Result<usize, SaveError> {
    let c = index.columns();
    let n32 = u32::try_from(c.parent.len()).map_err(|_| SaveError::TooLarge("entry count"))?;
    let arena32 = u32::try_from(c.arena.len()).map_err(|_| SaveError::TooLarge("name arena"))?;
    let metadata_len = u32::try_from(index.metadata.serialized_len())
        .map_err(|_| SaveError::TooLarge("metadata columns"))?;
    usize::try_from(expected_len(n32, arena32, metadata_len, HEADER_LEN))
        .map_err(|_| SaveError::TooLarge("metadata columns"))
}

pub(crate) fn save(index: &Index, path: &Path) -> Result<(), SaveError> {
    let c = index.columns();
    let n = c.parent.len();
    let n32 = u32::try_from(n).map_err(|_| SaveError::TooLarge("entry count"))?;
    let arena32 = u32::try_from(c.arena.len()).map_err(|_| SaveError::TooLarge("name arena"))?;

    let metadata_len = u32::try_from(index.metadata.serialized_len())
        .map_err(|_| SaveError::TooLarge("metadata columns"))?;
    let payload_len = serialized_len(index)? - HEADER_LEN;
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
    /// Work spent on the optional metadata payload; zero for a core-only
    /// file. In [`MetadataLoad::Overlapped`] mode this ran on the metadata
    /// thread, beside the core sections, and is in none of the parts
    /// above. In the serial modes these are disjoint subspans of `read`,
    /// `checksum` and `decode`: do not add the parents again.
    pub metadata_read: Duration,
    pub metadata_checksum: Duration,
    pub metadata_decode: Duration,
    /// Time the loading thread waited for the metadata thread after
    /// the last core section (a named part of the total).
    pub metadata_wait: Duration,
    /// Which metadata loading path ran, for the active-path log line.
    pub metadata_load: MetadataLoad,
    /// Core read throughput measured over the first bytes of the core
    /// sections, in MB/s (10^6 bytes); 0 when no probe ran.
    pub core_read_mb_s: f64,
}

/// How the optional metadata payload was loaded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MetadataLoad {
    /// No optional column in the file.
    #[default]
    Absent,
    /// Read, checksummed and decoded on a second thread while the loading
    /// thread streamed the core sections.
    Overlapped,
    /// Serial: the payload, or the core sections before it, are too small
    /// for a thread to pay off.
    SerialSmall,
    /// Serial: the source is not a path (a reader or bytes).
    SerialReader,
    /// Serial: the first core bytes read slower than 500 MB/s (a cold disk
    /// or a network volume), where a second stream on another region of
    /// the file can add seeks for no gain; this is the previous serial path.
    SerialSlowVolume,
    /// Serial: the second file handle could not be opened or is a file of
    /// a different size.
    SerialReopenFailed,
    /// Serial: the second handle opened a file whose header differs from
    /// the one being loaded (the index was replaced during the load).
    SerialReopenMismatch,
    /// Serial: the operating system refused to start the metadata thread.
    SerialSpawnFailed,
}

impl MetadataLoad {
    /// Stable label for the active-path log line.
    pub fn label(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Overlapped => "overlapped-thread",
            Self::SerialSmall => "serial-small",
            Self::SerialReader => "serial-reader",
            Self::SerialSlowVolume => "serial-slow-volume",
            Self::SerialReopenFailed => "serial-reopen-failed",
            Self::SerialReopenMismatch => "serial-reopen-mismatch",
            Self::SerialSpawnFailed => "serial-spawn-failed",
        }
    }
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
                + self.derived
                + self.metadata_wait,
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
    let (index, mut timings) = load_from(&mut file, file_len, &Overlap::new(Some(path)))?;
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
///
/// `overlap` names the path `reader` reads, when there is one, and the
/// thresholds for reading a large metadata payload through a second handle
/// on a second thread (see [`stream_sections`]).
fn load_from<R: std::io::Read>(
    reader: &mut R,
    file_len: u64,
    overlap: &Overlap,
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
        let index = stream_sections(reader, &data, &header, expected, overlap, &mut timings)?;
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

/// Called once, with the bytes and read time so far, when the core
/// sections have been read past the probe threshold.
type Probe<'a> = (u64, &'a mut dyn FnMut(u64, Duration));

fn cancelled() -> LoadError {
    LoadError::Io(std::io::Error::new(
        std::io::ErrorKind::Interrupted,
        "index load cancelled",
    ))
}

/// Sequential section reader for [`stream_sections`]: every byte goes
/// through the CRC exactly once, straight after it is read.
struct Stream<'a, R> {
    reader: &'a mut R,
    crc: crc32fast::Hasher,
    chunk: Vec<u8>,
    timings: &'a mut LoadTimings,
    /// Checked before every chunk: set when the other side has failed.
    stop: Option<&'a AtomicBool>,
    /// Bytes read so far, for the probe.
    bytes_read: u64,
    probe: Option<Probe<'a>>,
}

impl<R: std::io::Read> Stream<'_, R> {
    fn check_stop(&self) -> Result<(), LoadError> {
        match self.stop {
            Some(stop) if stop.load(AtomicOrdering::Relaxed) => Err(cancelled()),
            _ => Ok(()),
        }
    }

    /// Account `len` freshly read bytes and run the probe once it is due.
    fn note_read(&mut self, len: usize) {
        self.bytes_read += len as u64;
        if self
            .probe
            .as_ref()
            .is_some_and(|(limit, _)| self.bytes_read >= *limit)
            && let Some((_, probe)) = self.probe.take()
        {
            probe(self.bytes_read, self.timings.read);
        }
    }

    /// Read exactly `len` (at most one chunk) bytes into `self.chunk`.
    fn fill(&mut self, len: usize) -> Result<(), LoadError> {
        self.check_stop()?;
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
        self.note_read(len);
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
            self.check_stop()?;
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
            self.note_read(want);
        }
        Ok(out)
    }
}

/// When and how the metadata payload may be read on a second thread.
struct Overlap<'a> {
    /// The path the loading reader reads, if it is a file.
    source: Option<&'a Path>,
    /// Smallest metadata payload worth a thread.
    min_payload: usize,
    /// Core bytes to read, timed, before deciding whether to start it.
    probe_bytes: u64,
    /// Slowest core read, in bytes per second, that still starts it.
    min_bytes_per_sec: f64,
    /// Test seams: injected throughput, a refused thread spawn, and a hook
    /// run after the second handle is open.
    #[cfg(test)]
    throughput: Option<f64>,
    #[cfg(test)]
    fail_spawn: bool,
    #[cfg(test)]
    after_reopen: Option<&'a dyn Fn()>,
}

/// A metadata payload at least this large is read on a second thread.
/// Below it (about 100k entries with the default columns) a thread costs
/// more than the few milliseconds it hides.
const OVERLAP_MIN_BYTES: usize = 1 << 20;
/// Core bytes read and timed on the loading thread before the second
/// thread is started.
const PROBE_BYTES: u64 = 4 << 20;
/// A page cache or an NVMe drive reads the core sections well above this;
/// a cold hard disk or a network volume well below it.
const MIN_PROBE_BYTES_PER_SEC: f64 = 500e6;

impl<'a> Overlap<'a> {
    fn new(source: Option<&'a Path>) -> Self {
        Self {
            source,
            min_payload: OVERLAP_MIN_BYTES,
            probe_bytes: PROBE_BYTES,
            min_bytes_per_sec: MIN_PROBE_BYTES_PER_SEC,
            #[cfg(test)]
            throughput: None,
            #[cfg(test)]
            fail_spawn: false,
            #[cfg(test)]
            after_reopen: None,
        }
    }

    fn spawn_refused(&self) -> bool {
        #[cfg(test)]
        return self.fail_spawn;
        #[cfg(not(test))]
        false
    }

    fn after_reopen(&self) {
        #[cfg(test)]
        if let Some(hook) = self.after_reopen {
            hook();
        }
    }
}

/// Core read throughput in bytes per second; a zero read time is as fast
/// as it gets.
fn bytes_per_sec(bytes: u64, read: Duration) -> f64 {
    if read.is_zero() {
        f64::INFINITY
    } else {
        bytes as f64 / read.as_secs_f64()
    }
}

/// What the metadata thread hands back: the CRC state of the payload bytes
/// (so the loading thread can combine it into the whole-file checksum) and
/// the decoded columns. A decode error is kept apart from the read result
/// because a checksum mismatch must be reported ahead of it, as in the
/// serial path.
struct MetadataRead {
    crc: crc32fast::Hasher,
    columns: Result<MetadataColumns, LoadError>,
    timings: LoadTimings,
}

/// Read, checksum and decode the metadata payload from `file`, already
/// positioned at its first byte. Runs on the metadata thread; `stop` is
/// set when the loading thread has failed, and is checked per chunk.
fn read_metadata(
    mut file: std::fs::File,
    len: usize,
    n: usize,
    set: ColumnSet,
    stop: &AtomicBool,
) -> Result<MetadataRead, LoadError> {
    let mut timings = LoadTimings::default();
    let mut s = Stream {
        reader: &mut file,
        crc: crc32fast::Hasher::new(),
        chunk: Vec::with_capacity(STREAM_CHUNK),
        timings: &mut timings,
        stop: Some(stop),
        bytes_read: 0,
        probe: None,
    };
    let bytes = s.bytes(len)?;
    s.check_stop()?;
    let crc = s.crc;
    timings.metadata_read = timings.read;
    timings.metadata_checksum = timings.checksum;
    let t = Instant::now();
    let mut at = 0;
    let columns = MetadataColumns::read(&bytes, &mut at, n, set).and_then(|columns| {
        if at == bytes.len() {
            Ok(columns)
        } else {
            Err(LoadError::Corrupt("unexpected metadata bytes"))
        }
    });
    drop(bytes);
    timings.metadata_decode = t.elapsed();
    Ok(MetadataRead {
        crc,
        columns,
        timings,
    })
}

/// A second handle on `path` positioned at `offset`. It must be a file of
/// `file_len` bytes that starts with the same header as the one being
/// loaded: the header holds the whole-file checksum, so a newly saved index
/// of the same size (an atomic replace between the two opens) is refused
/// and the load stays serial instead of failing its checksum.
fn reopen_at(
    path: &Path,
    offset: u64,
    file_len: u64,
    head: &[u8],
) -> Result<std::fs::File, MetadataLoad> {
    use std::io::{Read, Seek};
    let failed = |_: std::io::Error| MetadataLoad::SerialReopenFailed;
    let mut file = std::fs::File::open(path).map_err(failed)?;
    if file.metadata().map_err(failed)?.len() != file_len {
        return Err(MetadataLoad::SerialReopenFailed);
    }
    let mut again = vec![0u8; head.len()];
    file.read_exact(&mut again).map_err(failed)?;
    if again != head {
        return Err(MetadataLoad::SerialReopenMismatch);
    }
    file.seek(std::io::SeekFrom::Start(offset))
        .map_err(failed)?;
    Ok(file)
}

/// Load a v3 index whose 32-byte header is already in `head` and whose
/// validated total length is `expected`: each section is read directly into
/// the column that keeps it, with no whole-file staging buffer (which would
/// cost a second page-fault and zeroing pass over the file size, a copy per
/// column and a large free). The checksum is verified before any structural
/// validation, as in [`from_bytes`]; validation is shared with it.
///
/// The metadata payload is the last section and is independent of the core
/// sections, so when it is large and the source is a file it can be read,
/// checksummed and decoded on a second thread while this thread streams the
/// core sections (a single serial stage). The thread is started only after
/// the first core bytes were read and timed here at a rate that says the
/// file is in the page cache or on a fast drive; a slow volume stays on the
/// serial path, where one sequential stream is best. The CRC states are
/// combined, so the whole-file checksum is still verified before any
/// validation, and the returned index is complete: nothing is pending for a
/// later query. A failure on this thread stops the other one, and its error
/// is the one reported.
fn stream_sections<R: std::io::Read>(
    reader: &mut R,
    head: &[u8],
    header: &Header,
    expected: u64,
    overlap: &Overlap,
    timings: &mut LoadTimings,
) -> Result<Index, LoadError> {
    let n = header.entry_count as usize;
    let arena_len = header.arena_len as usize;
    let metadata_len = header.metadata_len as usize;
    let metadata_at = expected - u64::from(header.metadata_len);
    debug_assert_eq!(
        metadata_at,
        HEADER_LEN as u64 + n as u64 * 16 + arena_len as u64 * 2
    );
    // Provisional mode: replaced when the probe decides. If the core ends
    // before the probe fires, the payload stays serial and small.
    let mode = Cell::new(match overlap.source {
        _ if metadata_len == 0 => MetadataLoad::Absent,
        None => MetadataLoad::SerialReader,
        Some(_) => MetadataLoad::SerialSmall,
    });
    let probing = overlap.source.is_some()
        && metadata_len >= overlap.min_payload
        && metadata_at > overlap.probe_bytes;
    let stop = AtomicBool::new(false);
    let core_read_mb_s = Cell::new(0.0);
    let parts = std::thread::scope(|scope| {
        let built = (|| -> Result<Parts, LoadError> {
            let worker = Cell::new(None);
            let mut decide = |bytes: u64, read: Duration| {
                let Some(path) = overlap.source else { return };
                let rate = bytes_per_sec(bytes, read);
                #[cfg(test)]
                let rate = overlap.throughput.unwrap_or(rate);
                core_read_mb_s.set(rate / 1e6);
                if rate < overlap.min_bytes_per_sec {
                    mode.set(MetadataLoad::SerialSlowVolume);
                    return;
                }
                let file = match reopen_at(path, metadata_at, expected, head) {
                    Ok(file) => file,
                    Err(refused) => {
                        mode.set(refused);
                        return;
                    }
                };
                overlap.after_reopen();
                let (set, stop) = (header.columns, &stop);
                let spawned = if overlap.spawn_refused() {
                    None
                } else {
                    std::thread::Builder::new()
                        .name("ferrow-metadata-load".into())
                        .spawn_scoped(scope, move || {
                            read_metadata(file, metadata_len, n, set, stop)
                        })
                        .ok()
                };
                mode.set(if spawned.is_some() {
                    MetadataLoad::Overlapped
                } else {
                    MetadataLoad::SerialSpawnFailed
                });
                worker.set(spawned);
            };
            let mut crc = crc32fast::Hasher::new();
            crc.update(&head[CHECKSUM_START..HEADER_LEN]);
            let mut s = Stream {
                reader,
                crc,
                chunk: Vec::with_capacity(STREAM_CHUNK),
                timings,
                stop: None,
                bytes_read: 0,
                probe: probing.then_some((overlap.probe_bytes, &mut decide as _)),
            };
            let parent = s.values(n, u32::from_le_bytes)?;
            let name_off = s.values(n, u32::from_le_bytes)?;
            let name_len = s.values(n, u16::from_le_bytes)?;
            let flags = s.values(n, u16::from_le_bytes)?;
            let arena = s.bytes(arena_len)?;
            let folded_arena = s.bytes(arena_len)?;
            let sorted = s.values(n, u32::from_le_bytes)?;
            let worker = worker.take();
            let serial = if worker.is_none() {
                let (read, checksum) = (s.timings.read, s.timings.checksum);
                let bytes = s.bytes(metadata_len)?;
                s.timings.metadata_read = s.timings.read - read;
                s.timings.metadata_checksum = s.timings.checksum - checksum;
                Some(bytes)
            } else {
                None
            };
            let mut crc = s.crc;
            drop(s.chunk);
            timings.metadata_load = mode.get();
            timings.core_read_mb_s = core_read_mb_s.get();
            let metadata = match worker {
                Some(worker) => {
                    let t = Instant::now();
                    let done = worker.join().map_err(|_| {
                        LoadError::Io(std::io::Error::other("metadata reader panicked"))
                    })??;
                    timings.metadata_wait = t.elapsed();
                    crc.combine(&done.crc);
                    timings.metadata_read = done.timings.metadata_read;
                    timings.metadata_checksum = done.timings.metadata_checksum;
                    timings.metadata_decode = done.timings.metadata_decode;
                    if crc.finalize() != header.checksum {
                        return Err(LoadError::ChecksumMismatch);
                    }
                    done.columns?
                }
                None => {
                    if crc.finalize() != header.checksum {
                        return Err(LoadError::ChecksumMismatch);
                    }
                    let bytes = serial.unwrap_or_default();
                    let t = Instant::now();
                    let mut at = 0;
                    let metadata = MetadataColumns::read(&bytes, &mut at, n, header.columns)?;
                    if at != bytes.len() {
                        return Err(LoadError::Corrupt("unexpected metadata bytes"));
                    }
                    drop(bytes);
                    timings.metadata_decode = t.elapsed();
                    timings.decode += timings.metadata_decode;
                    metadata
                }
            };
            Ok(Parts {
                parent,
                name_off,
                name_len,
                flags,
                arena,
                folded_arena,
                sorted,
                metadata,
            })
        })();
        // The scope joins a running metadata thread on exit; tell it to
        // stop first so a failed load does not wait for its payload.
        if built.is_err() {
            stop.store(true, AtomicOrdering::Relaxed);
        }
        built
    })?;
    validate(parts, timings)
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
    timings.metadata_load = if header.metadata_len == 0 {
        MetadataLoad::Absent
    } else {
        MetadataLoad::SerialReader
    };
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
    // Parallel in chunks: each chunk's length sum (u64: at most n * 65535,
    // no overflow), a serial prefix over the few chunks, then each chunk
    // checks its offsets from its own start. A packed layout ending at
    // the arena length also keeps every name inside the arena.
    const STRUCT_CHUNK: usize = 1 << 16;
    let sums: Vec<u64> = name_len
        .par_chunks(STRUCT_CHUNK)
        .map(|lens| lens.iter().map(|&l| l as u64).sum())
        .collect();
    let mut starts = Vec::with_capacity(sums.len());
    let mut running: u64 = 0;
    for sum in &sums {
        starts.push(running);
        running += sum;
    }
    if running != arena_len as u64 {
        return Err(LoadError::Corrupt("name layout is not packed"));
    }
    let packed = name_off
        .par_chunks(STRUCT_CHUNK)
        .zip(name_len.par_chunks(STRUCT_CHUNK))
        .zip(starts.par_iter())
        .all(|((offs, lens), &start)| {
            let mut at = start;
            let mut ok = true;
            for (&off, &len) in offs.iter().zip(lens) {
                ok &= off as u64 == at;
                at += len as u64;
            }
            ok
        });
    if !packed {
        return Err(LoadError::Corrupt("name layout is not packed"));
    }

    // Validate so later lookups can never panic on a crafted file.
    let n32 = n as u64;
    if !parent
        .par_chunks(STRUCT_CHUNK)
        .all(|c| c.iter().all(|&p| p == ROOT_PARENT || (p as u64) < n32))
    {
        return Err(LoadError::Corrupt("parent id out of range"));
    }
    if !sorted
        .par_chunks(STRUCT_CHUNK)
        .all(|c| c.iter().all(|&id| (id as u64) < n32))
    {
        return Err(LoadError::Corrupt("sort order id out of range"));
    }
    // Reject parent cycles, which would loop forever in path rebuild:
    // every chain must reach a root within n steps. The writer gives
    // every parent a smaller id than its children, and then no cycle is
    // possible; that is one parallel pass. Any other layout gets the full
    // walk with a visiting mark (O(n * depth) would be too slow).
    let parents_first = parent
        .par_chunks(STRUCT_CHUNK)
        .enumerate()
        .all(|(c, chunk)| {
            let base = c * STRUCT_CHUNK;
            chunk
                .iter()
                .enumerate()
                .all(|(i, &p)| p == ROOT_PARENT || (p as usize) < base + i)
        });
    let mut state = vec![0u8; if parents_first { 0 } else { n }]; // 0 = unseen, 1 = on current walk, 2 = done
    let mut walk: Vec<usize> = Vec::new(); // reused: no per-entry allocation
    for start in 0..if parents_first { 0 } else { n } {
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

    // Sort-order uniqueness: one bit per id, set in parallel; an id whose
    // bit was already set is a duplicate. With n in-range ids and no
    // duplicate, the order is a permutation.
    let seen: Vec<std::sync::atomic::AtomicU64> = (0..n.div_ceil(64))
        .map(|_| std::sync::atomic::AtomicU64::new(0))
        .collect();
    let unique = sorted.par_chunks(STRUCT_CHUNK).all(|c| {
        c.iter().all(|&id| {
            let bit = 1u64 << (id % 64);
            seen[id as usize / 64].fetch_or(bit, std::sync::atomic::Ordering::Relaxed) & bit == 0
        })
    });
    drop(seen);
    if !unique {
        return Err(LoadError::Corrupt("duplicate id in sort order"));
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

/// What a streaming check of a saved index found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verified {
    pub entries: u32,
    pub bytes: u64,
}

/// The read size [`verify_file`] uses: the check holds this much memory
/// whatever the size of the file.
pub const VERIFY_BUFFER: usize = 1 << 20;

/// Check a saved index without decoding it: the header (magic, version,
/// counts) and the file length against it, then the whole-file CRC-32 computed
/// over reads of at most `buffer` bytes. Memory use is the buffer, not the index;
/// it fails the same way [`Index::load`] does for a bad magic, a short or long
/// file and a checksum mismatch. It does not validate the offsets inside the
/// sections (a file written by Ferrow with a matching checksum has valid ones).
pub(crate) fn verify_reader<R: std::io::Read>(
    reader: &mut R,
    file_len: u64,
    buffer: usize,
) -> Result<Verified, LoadError> {
    let head_len = usize::try_from(file_len.min(HEADER_LEN as u64)).unwrap_or(HEADER_LEN);
    let mut head = vec![0u8; head_len];
    reader.read_exact(&mut head).map_err(LoadError::Io)?;
    let header = parse_header(&head)?;
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
    // The checksum covers everything after its own field, in both versions.
    let mut crc = crc32fast::Hasher::new();
    crc.update(&head[CHECKSUM_START.min(head.len())..]);
    let mut chunk = vec![0u8; buffer.max(4096)];
    let mut left = expected - head_len as u64;
    while left > 0 {
        let want = usize::try_from(left.min(chunk.len() as u64)).unwrap_or(chunk.len());
        let got = match reader.read(&mut chunk[..want]) {
            Ok(0) => return Err(LoadError::Truncated),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(LoadError::Io(e)),
        };
        crc.update(&chunk[..got]);
        left -= got as u64;
    }
    if crc.finalize() != header.checksum {
        return Err(LoadError::ChecksumMismatch);
    }
    Ok(Verified {
        entries: header.entry_count,
        bytes: expected,
    })
}

pub(crate) fn verify_file(path: &Path) -> Result<Verified, LoadError> {
    let mut file = std::fs::File::open(path).map_err(LoadError::Io)?;
    let file_len = file.metadata().map_err(LoadError::Io)?.len();
    verify_reader(&mut file, file_len, VERIFY_BUFFER)
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
    fn verify_passes_a_good_file_and_names_every_kind_of_damage() {
        let bytes = sample_bytes();
        let check = |data: &[u8]| verify_reader(&mut &data[..], data.len() as u64, 4096);
        let ok = check(&bytes).unwrap();
        assert_eq!((ok.entries, ok.bytes), (3, bytes.len() as u64));
        // Same verdicts as the real loader.
        let mut flipped = bytes.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 0x55;
        assert!(matches!(check(&flipped), Err(LoadError::ChecksumMismatch)));
        assert!(matches!(
            check(&bytes[..bytes.len() - 5]),
            Err(LoadError::Truncated)
        ));
        let mut longer = bytes.clone();
        longer.extend_from_slice(&[0; 16]);
        assert!(matches!(check(&longer), Err(LoadError::Corrupt(_))));
        let mut magic = bytes.clone();
        magic[0] = b'X';
        assert!(matches!(check(&magic), Err(LoadError::WrongMagic)));
        let mut version = bytes.clone();
        version[8..12].copy_from_slice(&99u32.to_le_bytes());
        assert!(matches!(
            check(&version),
            Err(LoadError::UnsupportedVersion(99))
        ));
        assert!(matches!(check(&bytes[..4]), Err(LoadError::Truncated)));
        for data in [&flipped[..], &bytes[..bytes.len() - 5], &magic[..]] {
            assert!(from_bytes(data).is_err(), "the loader refuses it too");
        }
    }

    #[test]
    fn verify_reads_a_large_file_in_bounded_pieces_and_reports_io_errors() {
        // A multi-megabyte index checked through a 64 KiB buffer: no single
        // read may exceed the cap, however large the file is.
        let b = IndexBuilder::new();
        let root = b.add_root(b"root", flags::DIR);
        for n in 0..40_000u32 {
            b.add(
                root,
                format!("a-rather-long-file-name-{n:08}.txt").as_bytes(),
                0,
            );
        }
        let path = std::env::temp_dir().join(format!("ferrow-verify-big-{}", std::process::id()));
        b.freeze().save(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(
            bytes.len() > 3 << 20,
            "the fixture is {} bytes",
            bytes.len()
        );
        struct Spy<'a> {
            data: &'a [u8],
            biggest: usize,
            total: usize,
        }
        impl std::io::Read for Spy<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.biggest = self.biggest.max(buf.len());
                let n = self.data.read(buf)?;
                self.total += n;
                Ok(n)
            }
        }
        let cap = 64 * 1024;
        let mut spy = Spy {
            data: &bytes,
            biggest: 0,
            total: 0,
        };
        let ok = verify_reader(&mut spy, bytes.len() as u64, cap).unwrap();
        assert_eq!(ok.entries, 40_001);
        assert_eq!(spy.total, bytes.len(), "the whole file was checked");
        assert!(spy.biggest <= cap, "a read asked for {} bytes", spy.biggest);
        // A read error is I/O, not damage.
        struct Broken;
        impl std::io::Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("disk gone"))
            }
        }
        assert!(matches!(
            verify_reader(&mut Broken, 1 << 20, cap),
            Err(LoadError::Io(_))
        ));
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
        let result = load_from(&mut reader, bytes.len() as u64, &Overlap::new(None));
        assert!(matches!(result, Err(LoadError::Corrupt(_))), "{result:?}");
        assert_eq!(reader.read, HEADER_LEN, "payload was read before rejection");

        // Same for a length mismatch against a plausible header.
        let bytes = sample_bytes();
        let mut reader = Counting {
            inner: &bytes,
            read: 0,
        };
        let result = load_from(&mut reader, bytes.len() as u64 + 4096, &Overlap::new(None));
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

    /// A parent with a larger id than its child is not what the writer
    /// produces but is valid: the full walk accepts it. A two-entry cycle
    /// in the same layout is rejected.
    #[test]
    fn later_parent_ids_load_and_longer_cycles_are_rejected() {
        let dir = test_dir("later-parent");
        let path = dir.join("index.bin");
        let b = IndexBuilder::new();
        let root = b.add_root(b"root", flags::DIR);
        b.add(root, b"a", 0);
        b.add(root, b"b", flags::DIR);
        b.freeze().save(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let set_parent = |bytes: &mut Vec<u8>, id: usize, parent: u32| {
            let at = HEADER_LEN + id * 4;
            bytes[at..at + 4].copy_from_slice(&parent.to_le_bytes());
            fix_checksum(bytes);
        };
        let mut later = bytes.clone();
        set_parent(&mut later, 1, 2); // a's parent is b (id 2 > 1)
        let index = from_bytes(&later).expect("valid non-writer layout loads");
        assert_eq!(index.parent(1), 2);
        let mut cycle = later.clone();
        set_parent(&mut cycle, 2, 1); // b's parent is a: 1 -> 2 -> 1
        assert!(matches!(from_bytes(&cycle), Err(LoadError::Corrupt(_))));
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

    /// Default-column index with unknown, packed and overflow values, and a
    /// metadata payload past `OVERLAP_MIN_BYTES`. `bump` changes one
    /// modified value by that many seconds without changing the file size.
    fn default_columns_file(
        tag: &str,
        bump: i64,
    ) -> (std::path::PathBuf, std::path::PathBuf, Index) {
        let dir = test_dir(tag);
        let path = dir.join("index.bin");
        let b = IndexBuilder::with_columns(ColumnSet::DEFAULT);
        let root = b.add_root(b"root", flags::DIR);
        for i in 0..130_000u64 {
            let meta = crate::EntryMetadata {
                size: match i % 1000 {
                    0 => None,
                    1 => Some(u64::MAX),
                    _ => Some(i * 977),
                },
                modified: match i % 997 {
                    0 => None,
                    1 => Some(i64::MIN),
                    _ => Some(
                        1_700_000_000 * 10_000_000
                            + i as i64 * 10_000_000
                            + if i == 5 { bump * 10_000_000 } else { 0 },
                    ),
                },
                ..Default::default()
            };
            let name = format!("file-{:06}", i * 7919 % 130_000);
            b.add_with_metadata(root, name.as_bytes(), 0, meta);
        }
        let index = b.freeze();
        index.save(&path).unwrap();
        (dir, path, index)
    }

    /// Overlap settings for the test files (about 5 MB of core sections):
    /// probe after 1 MiB and report a fast volume, whatever this machine's
    /// disk does.
    fn fast_overlap(path: &Path) -> Overlap<'_> {
        let mut overlap = Overlap::new(Some(path));
        overlap.probe_bytes = 1 << 20;
        overlap.throughput = Some(1e9);
        overlap
    }

    fn load_with(path: &Path, overlap: &Overlap) -> Result<(Index, LoadTimings), LoadError> {
        let mut file = std::fs::File::open(path).unwrap();
        let len = file.metadata().unwrap().len();
        load_from(&mut file, len, overlap)
    }

    fn assert_same_metadata(a: &Index, b: &Index) {
        assert_eq!(a.columns_present(), b.columns_present());
        assert_eq!(a.heap_bytes(), b.heap_bytes());
        for id in (0..b.len() as u32)
            .step_by(7)
            .chain([1, 2, 6, 1000, 129_999])
        {
            assert_eq!(a.metadata(id), b.metadata(id), "entry {id}");
        }
        // Complete on return: no sort was started or is pending.
        assert!(a.metadata_order(crate::SortKey::Modified, false).is_none());
    }

    /// The overlapped metadata load returns exactly what the serial parse
    /// does, complete on return (nothing pending for a query), and reports
    /// which path ran and the throughput it decided on.
    #[test]
    fn overlapped_metadata_load_matches_serial_parse() {
        let (dir, path, saved) = default_columns_file("overlap", 0);
        let bytes = std::fs::read(&path).unwrap();
        let (loaded, t) = load_with(&path, &fast_overlap(&path)).unwrap();
        assert_eq!(t.metadata_load, MetadataLoad::Overlapped);
        assert_eq!(t.core_read_mb_s, 1000.0);
        assert!(t.metadata_read > Duration::ZERO && t.metadata_decode > Duration::ZERO);
        assert!(t.metadata_wait <= t.total && t.unattributed() <= t.total);
        let mut timings = LoadTimings::default();
        let parsed = from_bytes_inner(&bytes, &mut timings).unwrap();
        assert_eq!(timings.metadata_load, MetadataLoad::SerialReader);
        assert_same_metadata(&loaded, &saved);
        assert_same_metadata(&parsed, &saved);
        // The public path, with the real probe, loads the same index
        // whichever mode this machine's read speed picks.
        assert_same_metadata(&Index::load(&path).unwrap(), &saved);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every way the overlapped load can see bad bytes fails closed, with
    /// the same error as the serial parse: a flipped byte in the metadata
    /// payload or in a core section is a checksum mismatch (also when the
    /// flipped metadata would not decode); checksum-valid metadata that
    /// does not decode is `Corrupt`.
    #[test]
    fn overlapped_metadata_load_fails_closed() {
        let (dir, path, _) = default_columns_file("overlap-bad", 0);
        let bytes = std::fs::read(&path).unwrap();
        let n = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let arena_len = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
        let metadata_at = HEADER_LEN + n * 16 + arena_len * 2;
        let load = |bad: &[u8]| {
            std::fs::write(&path, bad).unwrap();
            let result = load_with(&path, &fast_overlap(&path));
            if let Ok((_, t)) = &result {
                assert_eq!(t.metadata_load, MetadataLoad::Overlapped);
            }
            result.map(|(index, _)| index)
        };
        for at in [
            HEADER_LEN + 9,
            metadata_at - 1,
            metadata_at + 100,
            bytes.len() - 1,
        ] {
            let mut bad = bytes.clone();
            bad[at] ^= 0x10;
            assert!(
                matches!(load(&bad), Err(LoadError::ChecksumMismatch)),
                "flip at {at}"
            );
        }
        // Overflow count beyond the entry count: not decodable. With the
        // checksum fixed it is Corrupt; with a stale checksum the
        // mismatch is reported first.
        let mut bad = bytes.clone();
        bad[metadata_at..metadata_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(load(&bad), Err(LoadError::ChecksumMismatch)));
        fix_checksum(&mut bad);
        assert!(matches!(load(&bad), Err(LoadError::Corrupt(_))));
        // The unmodified file still loads after the failures.
        assert!(load(&bytes).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every reason the load stays serial is reported, never overlapped,
    /// and the index is identical and still checked: a reader with no path,
    /// a missing or differently sized file, a same-sized file with another
    /// header (an atomic save replaced the index between the two opens), a
    /// refused thread spawn, a slow volume, and sizes too small to bother.
    #[test]
    fn metadata_load_falls_back_to_serial_and_reports_why() {
        let (dir, path, saved) = default_columns_file("overlap-fallback", 0);
        let bytes = std::fs::read(&path).unwrap();
        let other = dir.join("other.bin");
        std::fs::write(&other, b"not the index").unwrap();
        let missing = dir.join("missing.bin");
        // Same shape and size as `path`, one modified value differs: only
        // the header checksum tells them apart.
        let (newer_dir, newer, newer_index) = default_columns_file("overlap-newer", 3);
        assert_eq!(
            std::fs::metadata(&newer).unwrap().len(),
            bytes.len() as u64,
            "same size"
        );
        assert_ne!(saved.metadata(6), newer_index.metadata(6));
        let mut cases: Vec<(Overlap, MetadataLoad)> = Vec::new();
        cases.push((Overlap::new(None), MetadataLoad::SerialReader));
        for source in [other.as_path(), missing.as_path()] {
            cases.push((fast_overlap(source), MetadataLoad::SerialReopenFailed));
        }
        cases.push((fast_overlap(&newer), MetadataLoad::SerialReopenMismatch));
        let mut refused = fast_overlap(&path);
        refused.fail_spawn = true;
        cases.push((refused, MetadataLoad::SerialSpawnFailed));
        let mut slow = fast_overlap(&path);
        slow.throughput = Some(1e6);
        cases.push((slow, MetadataLoad::SerialSlowVolume));
        let mut short = fast_overlap(&path);
        short.probe_bytes = 1 << 40;
        cases.push((short, MetadataLoad::SerialSmall));
        let mut small = fast_overlap(&path);
        small.min_payload = 1 << 40;
        cases.push((small, MetadataLoad::SerialSmall));
        for (overlap, mode) in &cases {
            let mut reader = std::io::Cursor::new(&bytes);
            let (index, t) = load_from(&mut reader, bytes.len() as u64, overlap).unwrap();
            assert_eq!(t.metadata_load, *mode);
            assert_eq!(t.metadata_wait, Duration::ZERO);
            assert_same_metadata(&index, &saved);
            let mut bad = bytes.clone();
            let last = bad.len() - 1;
            bad[last] ^= 1;
            let mut reader = std::io::Cursor::new(&bad);
            assert!(
                matches!(
                    load_from(&mut reader, bad.len() as u64, overlap),
                    Err(LoadError::ChecksumMismatch)
                ),
                "{mode:?}"
            );
        }
        // A core-only file has no metadata to place.
        tiny_index(b"a.txt").save(&path).unwrap();
        assert_eq!(
            load_with(&path, &fast_overlap(&path))
                .unwrap()
                .1
                .metadata_load,
            MetadataLoad::Absent
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&newer_dir);
    }

    /// The decision to start the thread is a pure function of the bytes
    /// read and the time they took.
    #[test]
    fn probe_decision_follows_measured_throughput() {
        let min = Overlap::new(None).min_bytes_per_sec;
        assert_eq!(min, 500e6);
        let probe = PROBE_BYTES;
        let ms = Duration::from_millis;
        // 4 MiB in 4 ms is about 1 GB/s (page cache, NVMe): start.
        assert!(bytes_per_sec(probe, ms(4)) >= min);
        // 4 MiB in 8.4 ms is the limit; slower is a cold disk or network.
        assert!(bytes_per_sec(probe, ms(8)) >= min);
        assert!(bytes_per_sec(probe, ms(9)) < min);
        assert!(bytes_per_sec(probe, ms(40)) < min);
        assert_eq!(bytes_per_sec(probe, Duration::ZERO), f64::INFINITY);
    }

    /// A reader that fails after `fail_at` bytes.
    struct FailingReader<'a> {
        data: &'a [u8],
        at: usize,
        fail_at: usize,
    }

    impl std::io::Read for FailingReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.at >= self.fail_at {
                return Err(std::io::Error::other("disk gone"));
            }
            let n = buf.len().min(self.fail_at - self.at).min(64 << 10);
            buf[..n].copy_from_slice(&self.data[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }
    }

    /// A core read error after the thread started is the error reported,
    /// ahead of whatever the thread does, and the thread is told to stop.
    #[test]
    fn core_read_error_while_the_thread_runs_is_reported_and_stops_it() {
        let (dir, path, _) = default_columns_file("overlap-core-error", 0);
        let bytes = std::fs::read(&path).unwrap();
        // The thread is running by 1 MiB (probe) and the read fails at 2 MiB.
        for truncate_second_handle in [false, true] {
            let hook = || {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_len(HEADER_LEN as u64 + 100)
                    .unwrap();
            };
            let mut overlap = fast_overlap(&path);
            if truncate_second_handle {
                overlap.after_reopen = Some(&hook);
            }
            let mut reader = FailingReader {
                data: &bytes,
                at: 0,
                fail_at: 2 << 20,
            };
            let started = Instant::now();
            let result = load_from(&mut reader, bytes.len() as u64, &overlap);
            match result {
                Err(LoadError::Io(e)) => assert_eq!(e.to_string(), "disk gone"),
                other => panic!("expected the core read error, got {:?}", other.map(|_| ())),
            }
            assert!(started.elapsed() < Duration::from_secs(30));
            std::fs::write(&path, &bytes).unwrap();
        }
        // The thread side: a set stop flag ends the read at once; a clear
        // one decodes the payload.
        let n = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let arena_len = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
        let at = (HEADER_LEN + n * 16 + arena_len * 2) as u64;
        let open_at = || {
            use std::io::Seek;
            let mut file = std::fs::File::open(&path).unwrap();
            file.seek(std::io::SeekFrom::Start(at)).unwrap();
            file
        };
        let len = bytes.len() - at as usize;
        let stop = AtomicBool::new(true);
        let stopped = read_metadata(open_at(), len, n, ColumnSet::DEFAULT, &stop);
        match stopped {
            Err(LoadError::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::Interrupted),
            other => panic!("expected a cancelled read, got {:?}", other.map(|_| ())),
        }
        let go = AtomicBool::new(false);
        let done = read_metadata(open_at(), len, n, ColumnSet::DEFAULT, &go).unwrap();
        assert!(done.columns.is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The second handle failing mid-read (the file shrank after it was
    /// opened) is an error, never a partial index.
    #[test]
    fn metadata_thread_read_failure_fails_the_load() {
        let (dir, path, _) = default_columns_file("overlap-shrink", 0);
        let bytes = std::fs::read(&path).unwrap();
        let n = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let arena_len = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
        let at = (HEADER_LEN + n * 16 + arena_len * 2) as u64;
        let hook = || {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_len(at + 100)
                .unwrap();
        };
        let mut overlap = fast_overlap(&path);
        overlap.after_reopen = Some(&hook);
        // The loading thread reads an intact copy; only the second handle
        // sees the shrunken file.
        let mut reader = std::io::Cursor::new(&bytes);
        let result = load_from(&mut reader, bytes.len() as u64, &overlap);
        assert!(
            matches!(result, Err(LoadError::Truncated)),
            "{:?}",
            result.map(|_| ())
        );
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
