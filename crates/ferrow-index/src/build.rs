//! Bounded index builds (ADR 0007, step 8): shards that spill to disk, and a
//! freeze that writes the index file in place instead of in the heap.
//!
//! A heap freeze holds the shards (about 50-64 B/entry), then every final
//! column, both name arenas and the sort order (about 110 B/entry more). Here:
//!
//! - **Spilling.** An [`IndexBuilder`] made with [`IndexBuilder::with_spill`]
//!   writes a shard out to an anonymous temporary file beside the target when
//!   the shard holds its share of the budget, then reuses the shard's memory.
//!   While entries are added, the heap holds at most about twice the budget.
//! - **File freeze.** [`IndexBuilder::freeze_to_file_with`] sizes the new
//!   index file exactly, maps it writable, places the shards' columns in it
//!   (reading each spilled run back once for the columns and once for the
//!   names), folds the names and sorts the name order inside the map, writes
//!   the checksum, flushes, renames the file over the destination and maps
//!   it read-only. The dirty pages of a file mapping are the operating
//!   system's file cache, not this process's private memory, so the build's
//!   private memory stays a fixed working set whatever the index size.
//!
//! The file is byte for byte what [`crate::Index::save`] writes for the same
//! entries, so a file-backed build and a heap build are interchangeable.

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use memmap2::MmapMut;
use rayon::prelude::*;

use crate::metadata::{ColumnSet, FileMeta, ShardMeta, payload_len};
use crate::persist::{CHECKSUM_START, HEADER_LEN, MAGIC, VERSION, expected_len, sorted_pad};
use crate::store::{
    self, Plain, as_bytes, as_bytes_mut, cast_mut, create_temp_file, read_at, write_at,
};
use crate::{Index, IndexBuilder, SHARDS, SaveError, Shard};

/// The default private-memory budget of the shards of a spilling builder
/// (32 MiB: a shard spills a run of about 2 MiB). The shards' vectors keep
/// their capacity between runs, so the heap holds up to about twice that
/// (1.6 times in the measured build) while entries are added.
pub const DEFAULT_SPILL_BUDGET: usize = 32 << 20;

/// Where [`IndexBuilder::freeze_to_file_with`] puts the index.
#[derive(Clone, Copy)]
pub struct FileTarget<'a> {
    /// The index file. A temporary file beside it is written first and
    /// renamed over it, so a failed build leaves it untouched; a file that is
    /// memory-mapped (see `Index::load`) is replaced too.
    pub path: &'a Path,
    /// Held while the temporary file replaces `path` and is mapped, for a
    /// caller that allows one writer of the index file at a time.
    pub lock: Option<&'a Mutex<()>>,
}

/// What a file-backed freeze spent, in disjoint parts of `total` (the rest is
/// opening and sizing the file).
#[derive(Debug, Clone, Copy, Default)]
pub struct FileFreezeTimings {
    pub total: Duration,
    /// Placing the shards' columns, names and metadata in the file.
    pub scatter: Duration,
    /// Case-folding the names.
    pub fold: Duration,
    /// Sorting the name order.
    pub sort: Duration,
    /// The checksum of the file.
    pub checksum: Duration,
    /// Writing the dirty pages out.
    pub flush: Duration,
    /// Renaming over the destination and mapping the result.
    pub replace: Duration,
    pub entries: usize,
    pub file_bytes: u64,
    /// Bytes the shards spilled to temporary files before the freeze.
    pub spilled_bytes: u64,
}

/// A file-backed freeze that failed. Nothing was changed at the destination
/// and no temporary file is left; the builder is intact, so the caller may
/// still [`IndexBuilder::freeze_checked`] it into the heap.
pub struct FileFreezeError {
    pub error: SaveError,
    pub builder: Box<IndexBuilder>,
}

impl fmt::Debug for FileFreezeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileFreezeError")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl fmt::Display for FileFreezeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for FileFreezeError {}

fn other(what: &str) -> SaveError {
    SaveError::Io(io::Error::other(what.to_string()))
}

/// A cursor over a file by position, as `Read` and `Write`.
struct At<'a> {
    file: &'a File,
    pos: u64,
}
impl Read for At<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        read_at(self.file, buf, self.pos)?;
        self.pos += buf.len() as u64;
        Ok(buf.len())
    }
}
impl Write for At<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        write_at(self.file, buf, self.pos)?;
        self.pos += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Bytes a builder keeps for one entry, besides its name, in its shard:
/// the id, parent, flags and name length, and the packed metadata.
pub(crate) fn entry_bytes(columns: ColumnSet) -> usize {
    let on = |bit| usize::from(columns.contains(bit));
    12 + 5 * on(ColumnSet::SIZE)
        + 4 * (on(ColumnSet::MODIFIED) + on(ColumnSet::CREATED) + on(ColumnSet::ACCESSED))
        + 4 * on(ColumnSet::ATTRIBUTES)
}

/// One spilled shard's run: the entries of a shard that were written out
/// together. On disk: ids, parents, flags, name lengths (12 bytes per entry),
/// then the names, then the metadata ([`ShardMeta::write_run`]).
struct Run {
    at: u64,
    entries: usize,
    name_bytes: usize,
}

/// One shard's spill file and what it holds.
#[derive(Default)]
struct ShardSpill {
    file: Option<File>,
    end: u64,
    runs: Vec<Run>,
    /// Overflow values of every run, by column: the file's size depends on it.
    overflow: [usize; 5],
}

fn fill<T: Plain + Default>(from: &mut At<'_>, into: &mut Vec<T>, k: usize) -> io::Result<()> {
    into.clear();
    into.resize(k, T::default());
    from.read_exact(as_bytes_mut(into))
}

impl ShardSpill {
    fn file(&self) -> io::Result<&File> {
        self.file
            .as_ref()
            .ok_or_else(|| io::Error::other("spill file missing"))
    }

    /// The ids, parents, flags and name lengths of a run.
    fn read_core(&self, run: &Run, core: &mut Core) -> io::Result<()> {
        let mut from = At {
            file: self.file()?,
            pos: run.at,
        };
        fill(&mut from, &mut core.ids, run.entries)?;
        fill(&mut from, &mut core.parent, run.entries)?;
        fill(&mut from, &mut core.flags, run.entries)?;
        fill(&mut from, &mut core.name_len, run.entries)
    }

    /// The ids, the names and the metadata of a run.
    fn read_payload(
        &self,
        run: &Run,
        columns: ColumnSet,
        ids: &mut Vec<u32>,
        names: &mut Vec<u8>,
    ) -> io::Result<ShardMeta> {
        let file = self.file()?;
        let k = run.entries;
        fill(&mut At { file, pos: run.at }, ids, k)?;
        let mut from = At {
            file,
            pos: run.at + 12 * k as u64,
        };
        fill(&mut from, names, run.name_bytes)?;
        ShardMeta::read_run(columns, k, &mut from)
    }
}

/// Reusable buffers for the core columns of a spilled run.
#[derive(Default)]
struct Core {
    ids: Vec<u32>,
    parent: Vec<u32>,
    flags: Vec<u16>,
    name_len: Vec<u16>,
}

/// The spill state of a builder: one spill file per shard, shards to reuse,
/// and shards that could not be written (they stay in the heap).
pub(crate) struct Spill {
    dir: PathBuf,
    /// Bytes a shard may hold before it is written out.
    pub(crate) limit: usize,
    shards: Vec<Mutex<ShardSpill>>,
    spare: Mutex<Vec<Shard>>,
    parked: Mutex<Vec<Shard>>,
    failure: Mutex<Option<io::Error>>,
    failed: AtomicBool,
    spilled: AtomicU64,
    /// Nanoseconds the writers spent in [`Spill::write`] (all threads).
    spill_nanos: AtomicU64,
}

/// Spare shards kept for reuse (a few: spills are rare and quick).
const SPARE_SHARDS: usize = 8;

impl Spill {
    pub(crate) fn new(dir: &Path, budget: usize) -> Spill {
        Spill {
            dir: dir.to_path_buf(),
            // At least a few entries' worth, so a tiny budget still works.
            limit: (budget / SHARDS).max(256),
            shards: (0..SHARDS).map(|_| Mutex::default()).collect(),
            spare: Mutex::default(),
            parked: Mutex::default(),
            failure: Mutex::default(),
            failed: AtomicBool::new(false),
            spilled: AtomicU64::new(0),
            spill_nanos: AtomicU64::new(0),
        }
    }

    /// Whether a write failed: shards then stay in the heap.
    pub(crate) fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    /// The first write failure, if any.
    pub(crate) fn failure(&self) -> Option<String> {
        self.failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|e| e.to_string())
    }

    pub(crate) fn spilled_bytes(&self) -> u64 {
        self.spilled.load(Ordering::Relaxed)
    }

    pub(crate) fn spill_time(&self) -> Duration {
        Duration::from_nanos(self.spill_nanos.load(Ordering::Relaxed))
    }

    /// An empty shard to put in place of one that is written out.
    pub(crate) fn fresh(&self, columns: ColumnSet) -> Shard {
        self.spare
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop()
            .unwrap_or_else(|| Shard::new(columns))
    }

    /// Write `full` (shard `idx`'s entries) to its spill file and recycle its
    /// memory; on failure the shard is parked in the heap, intact.
    pub(crate) fn write(&self, idx: usize, mut full: Shard) {
        let started = Instant::now();
        let written = self.try_write(idx, &full);
        self.spill_nanos
            .fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        match written {
            Ok(()) => {
                full.clear();
                let mut spare = self.spare.lock().unwrap_or_else(|e| e.into_inner());
                if spare.len() < SPARE_SHARDS {
                    spare.push(full);
                }
            }
            Err(error) => {
                self.failed.store(true, Ordering::Relaxed);
                self.failure
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_or_insert(error);
                self.parked
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(full);
            }
        }
    }

    fn try_write(&self, idx: usize, shard: &Shard) -> io::Result<()> {
        let mut spill = self.shards[idx].lock().unwrap_or_else(|e| e.into_inner());
        if spill.file.is_none() {
            spill.file = Some(create_temp_file(&self.dir)?);
        }
        let at = spill.end;
        let end = {
            let mut out = At {
                file: spill.file()?,
                pos: at,
            };
            out.write_all(as_bytes(&shard.ids))?;
            out.write_all(as_bytes(&shard.parent))?;
            out.write_all(as_bytes(&shard.flags))?;
            out.write_all(as_bytes(&shard.name_len))?;
            out.write_all(&shard.arena)?;
            shard.meta.write_run(&mut out)?;
            out.pos
        };
        spill.end = end;
        spill.runs.push(Run {
            at,
            entries: shard.ids.len(),
            name_bytes: shard.arena.len(),
        });
        for (total, count) in spill.overflow.iter_mut().zip(shard.meta.overflow_counts()) {
            *total += count;
        }
        self.spilled.fetch_add(end - at, Ordering::Relaxed);
        Ok(())
    }

    /// Heap held by spare and parked shards: columns, names and metadata.
    pub(crate) fn heap_parts(&self) -> (usize, usize, usize) {
        let (mut columns, mut names, mut metadata) = (0, 0, 0);
        for list in [&self.spare, &self.parked] {
            for shard in list.lock().unwrap_or_else(|e| e.into_inner()).iter() {
                columns += shard.columns_bytes();
                names += shard.arena.capacity();
                metadata += shard.meta.heap_bytes();
            }
        }
        (columns, names, metadata)
    }

    /// Room to reserve, given a hint of `entries` entries and `name_bytes`
    /// of names: no more than the shards hold before they spill.
    pub(crate) fn cap_hint(
        &self,
        entries: usize,
        name_bytes: usize,
        per_entry: usize,
    ) -> (usize, usize) {
        let average = name_bytes.div_ceil(entries.max(1));
        let room = SHARDS * (self.limit / (per_entry + average).max(1));
        (entries.min(room), name_bytes.min(room * average))
    }

    /// Every spilled run read back into a heap shard (the heap freeze: it
    /// holds everything, as a builder that never spilled would).
    pub(crate) fn read_all(&self, columns: ColumnSet) -> io::Result<Vec<Shard>> {
        let mut out: Vec<Shard> =
            std::mem::take(&mut *self.parked.lock().unwrap_or_else(|e| e.into_inner()));
        for spill in &self.shards {
            let spill = spill.lock().unwrap_or_else(|e| e.into_inner());
            for run in &spill.runs {
                let mut shard = Shard::new(columns);
                let mut core = Core::default();
                spill.read_core(run, &mut core)?;
                shard.ids = core.ids;
                shard.parent = core.parent;
                shard.flags = core.flags;
                shard.name_len = core.name_len;
                let mut ids = Vec::new();
                shard.meta = spill.read_payload(run, columns, &mut ids, &mut shard.arena)?;
                out.push(shard);
            }
        }
        Ok(out)
    }
}

/// Removes a temporary file when dropped, unless disarmed. Declared before
/// the file and its map, so it runs after they are closed.
struct TempFile {
    path: PathBuf,
    armed: bool,
}
impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// The regions of an index file being built, in file order.
struct Regions<'a> {
    header: &'a mut [u8],
    parent: &'a mut [u32],
    name_off: &'a mut [u32],
    name_len: &'a mut [u16],
    flags: &'a mut [u16],
    arena: &'a mut [u8],
    folded: &'a mut [u8],
    pad: &'a mut [u8],
    sorted: &'a mut [u32],
    meta: &'a mut [u8],
}

fn split_regions(bytes: &mut [u8], n: usize, arena: usize, pad: usize, meta: usize) -> Regions<'_> {
    let (header, rest) = bytes.split_at_mut(HEADER_LEN);
    let (parent, rest) = rest.split_at_mut(n * 4);
    let (name_off, rest) = rest.split_at_mut(n * 4);
    let (name_len, rest) = rest.split_at_mut(n * 2);
    let (flags, rest) = rest.split_at_mut(n * 2);
    let (arena_bytes, rest) = rest.split_at_mut(arena);
    let (folded, rest) = rest.split_at_mut(arena);
    let (pad, rest) = rest.split_at_mut(pad);
    let (sorted, rest) = rest.split_at_mut(n * 4);
    debug_assert_eq!(rest.len(), meta);
    Regions {
        header,
        parent: cast_mut(parent),
        name_off: cast_mut(name_off),
        name_len: cast_mut(name_len),
        flags: cast_mut(flags),
        arena: arena_bytes,
        folded,
        pad,
        sorted: cast_mut(sorted),
        meta: rest,
    }
}

/// One run's core columns, in arrival order.
struct CoreRun<'a> {
    ids: &'a [u32],
    parent: &'a [u32],
    flags: &'a [u16],
    name_len: &'a [u16],
}

/// Place one run's core columns at their ids.
fn place_core(
    run: &CoreRun<'_>,
    parent: &mut [u32],
    flags: &mut [u16],
    name_len: &mut [u16],
) -> Result<(), SaveError> {
    let n = parent.len();
    let k = run.ids.len();
    if run.parent.len() != k || run.flags.len() != k || run.name_len.len() != k {
        return Err(other("a builder shard's columns differ in length"));
    }
    for (j, &id) in run.ids.iter().enumerate() {
        let i = id as usize;
        if i >= n {
            return Err(other("a builder entry id is out of range"));
        }
        parent[i] = run.parent[j];
        flags[i] = run.flags[j];
        name_len[i] = run.name_len[j];
    }
    Ok(())
}

/// Place one run's names at their offsets and its metadata at its ids.
fn place_names(
    ids: &[u32],
    names: &[u8],
    meta: &ShardMeta,
    name_off: &[u32],
    name_len: &[u16],
    arena: &mut [u8],
    places: &mut FileMeta<'_>,
) -> Result<(), SaveError> {
    let n = name_off.len();
    let mut at = 0usize;
    for &id in ids {
        let i = id as usize;
        if i >= n {
            return Err(other("a builder entry id is out of range"));
        }
        let (off, len) = (name_off[i] as usize, name_len[i] as usize);
        let (Some(from), Some(to)) = (names.get(at..at + len), arena.get_mut(off..off + len))
        else {
            return Err(other("a builder shard's names are short"));
        };
        to.copy_from_slice(from);
        at += len;
    }
    places.scatter(meta, ids);
    Ok(())
}

/// The IEEE CRC-32 of `data`, in parallel chunks combined into one.
pub(crate) fn crc_parallel(data: &[u8]) -> u32 {
    const CHUNK: usize = 8 << 20;
    if data.len() <= CHUNK {
        return crc32fast::hash(data);
    }
    let parts: Vec<crc32fast::Hasher> = data
        .par_chunks(CHUNK)
        .map(|chunk| {
            let mut h = crc32fast::Hasher::new();
            h.update(chunk);
            h
        })
        .collect();
    let mut parts = parts.into_iter();
    let mut whole = parts.next().unwrap_or_default();
    for part in parts {
        whole.combine(&part);
    }
    whole.finalize()
}

impl IndexBuilder {
    /// A builder whose shards spill to temporary files in `dir` once they hold
    /// `budget` bytes in all (see [`DEFAULT_SPILL_BUDGET`]), so adding entries
    /// takes a fixed amount of private memory whatever their number. Pair it
    /// with [`IndexBuilder::freeze_to_file`]: a heap [`IndexBuilder::freeze`]
    /// reads every spilled run back into memory.
    pub fn with_spill(columns: ColumnSet, dir: &Path, budget: usize) -> IndexBuilder {
        let mut builder = IndexBuilder::with_columns(columns);
        let spill = Spill::new(dir, budget);
        builder.limit = spill.limit;
        builder.spill = Some(spill);
        builder
    }

    /// Bytes the shards have written to temporary files so far (0 for a
    /// builder that does not spill).
    pub fn spilled_bytes(&self) -> u64 {
        self.spill.as_ref().map_or(0, Spill::spilled_bytes)
    }

    /// Time the writers have spent writing shards out so far, summed over
    /// threads (zero for a builder that does not spill).
    pub fn spill_time(&self) -> Duration {
        self.spill
            .as_ref()
            .map_or(Duration::ZERO, Spill::spill_time)
    }

    /// The first failure to write a spill file, if there was one: the shards
    /// then stayed in memory and the build is correct, only larger.
    pub fn spill_failure(&self) -> Option<String> {
        self.spill.as_ref().and_then(Spill::failure)
    }

    /// [`IndexBuilder::freeze`] straight into the index file at `path`: the
    /// same bytes [`crate::Index::save`] writes, and the returned index reads
    /// them through a memory map, so no column is built in the heap. A failure
    /// leaves `path` untouched and no temporary file behind.
    pub fn freeze_to_file(self, path: &Path) -> Result<Index, SaveError> {
        self.freeze_to_file_with(&FileTarget { path, lock: None })
            .map(|(index, _)| index)
            .map_err(|failed| failed.error)
    }

    /// [`IndexBuilder::freeze_to_file`] with its timings and, on failure, the
    /// builder back (see [`FileFreezeError`]).
    pub fn freeze_to_file_with(
        self,
        target: &FileTarget<'_>,
    ) -> Result<(Index, FileFreezeTimings), FileFreezeError> {
        match self.build_file(target) {
            Ok(done) => Ok(done),
            Err(error) => Err(FileFreezeError {
                error,
                builder: Box::new(self),
            }),
        }
    }

    fn build_file(&self, target: &FileTarget<'_>) -> Result<(Index, FileFreezeTimings), SaveError> {
        let started = Instant::now();
        let mut timings = FileFreezeTimings {
            spilled_bytes: self.spilled_bytes(),
            ..FileFreezeTimings::default()
        };
        let guards: Vec<MutexGuard<'_, Shard>> = self
            .shards
            .iter()
            .map(|s| s.lock().expect("builder shard poisoned"))
            .collect();
        let parked = self
            .spill
            .as_ref()
            .map(|s| s.parked.lock().unwrap_or_else(|e| e.into_inner()));
        let mut resident: Vec<&Shard> = guards.iter().map(|g| &**g).collect();
        resident.extend(parked.iter().flat_map(|p| p.iter()));
        let spills: Vec<MutexGuard<'_, ShardSpill>> = self
            .spill
            .iter()
            .flat_map(|s| s.shards.iter())
            .map(|s| s.lock().unwrap_or_else(|e| e.into_inner()))
            .collect();

        let columns = self.columns;
        let n = resident.iter().map(|s| s.ids.len()).sum::<usize>()
            + spills
                .iter()
                .flat_map(|s| s.runs.iter())
                .map(|r| r.entries)
                .sum::<usize>();
        if n != self.len() {
            return Err(other("the builder lost entries"));
        }
        let name_bytes = resident.iter().map(|s| s.arena.len()).sum::<usize>()
            + spills
                .iter()
                .flat_map(|s| s.runs.iter())
                .map(|r| r.name_bytes)
                .sum::<usize>();
        let mut overflows = [0usize; 5];
        for counts in resident
            .iter()
            .map(|s| s.meta.overflow_counts())
            .chain(spills.iter().map(|s| s.overflow))
        {
            for (total, count) in overflows.iter_mut().zip(counts) {
                *total += count;
            }
        }
        let n32 = u32::try_from(n).map_err(|_| SaveError::TooLarge("entry count"))?;
        let arena32 = u32::try_from(name_bytes).map_err(|_| SaveError::TooLarge("name arena"))?;
        let meta_len = payload_len(n, columns, overflows);
        let meta32 =
            u32::try_from(meta_len).map_err(|_| SaveError::TooLarge("metadata columns"))?;
        let pad = sorted_pad(arena32, VERSION) as usize;
        let total = expected_len(n32, arena32, meta32, HEADER_LEN, VERSION);
        usize::try_from(total).map_err(|_| SaveError::TooLarge("metadata columns"))?;

        // Declared first: dropped last, after the map and the file are closed.
        let (file, tmp) = crate::persist::create_temp(target.path, true)?;
        let mut temp = TempFile {
            path: tmp,
            armed: true,
        };
        file.set_len(total)?;
        // SAFETY: the file is private to this build (created exclusively,
        // never opened by anyone else) and not resized while mapped.
        let mut map = unsafe { MmapMut::map_mut(&file)? };

        let placed = Instant::now();
        let mut regions = split_regions(&mut map, n, name_bytes, pad, meta_len);
        // The header, but for the checksum (patched at the end).
        let header = std::mem::take(&mut regions.header);
        header[..8].copy_from_slice(&MAGIC);
        header[8..12].copy_from_slice(&VERSION.to_le_bytes());
        header[12..16].copy_from_slice(&n32.to_le_bytes());
        header[16..20].copy_from_slice(&arena32.to_le_bytes());
        header[24..28].copy_from_slice(&columns.bits().to_le_bytes());
        header[28..32].copy_from_slice(&meta32.to_le_bytes());
        std::mem::take(&mut regions.pad).fill(0);

        // Pass 1: the core columns of every run (a spilled run is read back
        // for its first 12 bytes per entry only).
        for shard in &resident {
            place_core(
                &CoreRun {
                    ids: &shard.ids,
                    parent: &shard.parent,
                    flags: &shard.flags,
                    name_len: &shard.name_len,
                },
                regions.parent,
                regions.flags,
                regions.name_len,
            )?;
        }
        let mut core = Core::default();
        for spill in &spills {
            for run in &spill.runs {
                spill.read_core(run, &mut core)?;
                place_core(
                    &CoreRun {
                        ids: &core.ids,
                        parent: &core.parent,
                        flags: &core.flags,
                        name_len: &core.name_len,
                    },
                    regions.parent,
                    regions.flags,
                    regions.name_len,
                )?;
            }
        }
        // Names are packed in id order: their offsets are running sums.
        let mut running = 0usize;
        for (off, &len) in regions.name_off.iter_mut().zip(regions.name_len.iter()) {
            *off = running as u32;
            running += len as usize;
        }
        if running != name_bytes {
            return Err(other("the builder's name lengths do not add up"));
        }

        // Pass 2: the names and the metadata of every run.
        let mut places = FileMeta::split(regions.meta, n, columns, overflows);
        for shard in &resident {
            place_names(
                &shard.ids,
                &shard.arena,
                &shard.meta,
                regions.name_off,
                regions.name_len,
                regions.arena,
                &mut places,
            )?;
        }
        let (mut ids, mut names) = (Vec::new(), Vec::new());
        for spill in &spills {
            for run in &spill.runs {
                let meta = spill.read_payload(run, columns, &mut ids, &mut names)?;
                place_names(
                    &ids,
                    &names,
                    &meta,
                    regions.name_off,
                    regions.name_len,
                    regions.arena,
                    &mut places,
                )?;
            }
        }
        if !places.finish() {
            return Err(other("the builder's metadata overflows do not add up"));
        }
        drop((ids, names, core));
        timings.scatter = placed.elapsed();

        #[cfg(test)]
        if self.fail_before_replace.load(Ordering::Relaxed) {
            return Err(other("test: build interrupted"));
        }

        let t = Instant::now();
        crate::fold_arena(
            regions.name_off,
            regions.name_len,
            regions.arena,
            regions.folded,
        );
        timings.fold = t.elapsed();
        let t = Instant::now();
        crate::sort_names(
            regions.sorted,
            regions.name_off,
            regions.name_len,
            regions.arena,
            regions.folded,
        );
        timings.sort = t.elapsed();

        let t = Instant::now();
        let sum = crc_parallel(&map[CHECKSUM_START..]);
        map[20..24].copy_from_slice(&sum.to_le_bytes());
        timings.checksum = t.elapsed();
        let t = Instant::now();
        map.flush()?;
        drop(map);
        file.sync_all()?;
        drop(file);
        timings.flush = t.elapsed();

        // The new file replaces the destination while no other writer of it
        // is between its own replace and map.
        let t = Instant::now();
        let mapped = {
            let _one = target
                .lock
                .map(|lock| lock.lock().unwrap_or_else(|e| e.into_inner()));
            std::fs::rename(&temp.path, target.path)?;
            temp.armed = false;
            store::map_file(target.path)?
        };
        if mapped.len() as u64 != total {
            return Err(other("the new index file has an unexpected length"));
        }
        let index = crate::persist::index_from_own_map(&mapped)
            .map_err(|e| SaveError::Io(io::Error::other(e.to_string())))?;
        timings.replace = t.elapsed();
        timings.total = started.elapsed();
        timings.entries = n;
        timings.file_bytes = total;
        Ok((index, timings))
    }

    /// Every shard in memory: the resident ones and every spilled run read
    /// back. For the heap freeze.
    pub(crate) fn drain_shards(&mut self) -> io::Result<Vec<Shard>> {
        let mut out: Vec<Shard> = self
            .shards
            .drain(..)
            .map(|s| s.into_inner().expect("builder shard poisoned"))
            .collect();
        if let Some(spill) = &self.spill {
            out.extend(spill.read_all(self.columns)?);
        }
        Ok(out)
    }
}
