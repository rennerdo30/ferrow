//! Fast NTFS backend: reads the volume's master file table (`$MFT`)
//! directly instead of listing every directory.
//!
//! One sequential read of the MFT yields every file's name, parent,
//! size, times and attributes, which is how a full index of millions of
//! files takes seconds instead of minutes. Opening a volume for raw
//! reads needs administrator rights (the system service has them); when
//! the volume cannot be opened, is not NTFS or anything else fails, the
//! caller falls back to the directory walk for that root.
//!
//! The record parser is plain byte parsing and is unit-tested on every
//! platform with synthetic records; only the volume access is Windows.

use std::path::{Path, PathBuf};

use ferrow_index::store::{Appender, Col, Plain, Scratch, count_heap_fallback};
use ferrow_index::{ColumnSet, EntryMetadata};

/// Records below this number are NTFS metadata files (`$MFT`,
/// `$LogFile`, …, `$Extend`); directory listings never show them.
pub const FIRST_USER_RECORD: u64 = 16;
/// The volume root directory's record number.
pub const ROOT_RECORD: u64 = 5;

const ATTR_STANDARD_INFORMATION: u32 = 0x10;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_REPARSE_POINT: u32 = 0xC0;
const ATTR_END: u32 = 0xFFFF_FFFF;

const RECORD_IN_USE: u16 = 0x1;
const RECORD_IS_DIRECTORY: u16 = 0x2;

const NAMESPACE_DOS: u8 = 2;

/// FILETIME of 1970-01-01 (100 ns ticks since 1601).
const FILETIME_UNIX_EPOCH: i64 = 116_444_736_000_000_000;

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
/// NTFS-internal bits in `$STANDARD_INFORMATION` that Win32 never reports.
const NTFS_INTERNAL_ATTRIBUTES: u32 = 0x1000_0000 | 0x2000_0000;

/// What one base record says about its file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecordInfo {
    pub in_use: bool,
    pub is_dir: bool,
    pub sequence: u16,
    /// Win32 file attributes as `FindFirstFile` reports them.
    pub attributes: u32,
    /// FILETIME ticks (since 1601); 0 when absent.
    pub created: i64,
    pub modified: i64,
    pub accessed: i64,
    /// Size of the unnamed data stream (0 for directories).
    pub size: u64,
    /// Reparse tag, 0 when the file is not a reparse point.
    pub reparse_tag: u32,
}

impl RecordInfo {
    /// Name surrogates (symlinks, junctions, mount points) are aliases:
    /// indexed as leaves, never descended, like the directory walk does.
    pub fn is_name_surrogate(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 && self.reparse_tag & 0x2000_0000 != 0
    }

    /// Index flags for an entry with this record, as the walk sets them.
    pub fn entry_flags(&self) -> u16 {
        use ferrow_index::flags;
        let surrogate = self.is_name_surrogate();
        let mut out = 0;
        if self.is_dir && !surrogate {
            out |= flags::DIR;
        }
        if surrogate {
            out |= flags::SYMLINK;
        }
        if self.attributes & 0x2 != 0 {
            out |= flags::HIDDEN;
        }
        out
    }

    /// A directory the index descends into (not an alias).
    pub fn is_descended_dir(&self) -> bool {
        self.is_dir && !self.is_name_surrogate()
    }

    pub fn metadata(&self, set: ColumnSet) -> EntryMetadata {
        let mut out = EntryMetadata::default();
        if set.contains(ColumnSet::SIZE) {
            out.size = Some(if self.is_dir { 0 } else { self.size });
        }
        if set.contains(ColumnSet::MODIFIED) {
            out.modified = filetime_to_metadata(self.modified);
        }
        if set.contains(ColumnSet::CREATED) {
            out.created = filetime_to_metadata(self.created);
        }
        if set.contains(ColumnSet::ACCESSED) {
            out.accessed = filetime_to_metadata(self.accessed);
        }
        if set.contains(ColumnSet::ATTRIBUTES) {
            out.attributes = Some(self.attributes);
        }
        out
    }
}

fn filetime_to_metadata(ticks: i64) -> Option<i64> {
    if ticks <= 0 {
        return None;
    }
    let unix = ticks.checked_sub(FILETIME_UNIX_EPOCH)?;
    let time = if unix >= 0 {
        std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_nanos(unix as u64 * 100))?
    } else {
        std::time::UNIX_EPOCH
            .checked_sub(std::time::Duration::from_nanos(unix.unsigned_abs() * 100))?
    };
    ferrow_index::metadata_timestamp(time)
}

/// One name of a file (hard links give a file several).
///
/// Laid out without padding (24 bytes) so a table of names can live in a
/// memory-mapped temporary file (see [`VolumeBuilder::with_spill`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct NameRecord {
    /// The file's base record number.
    pub record: u64,
    pub parent_record: u64,
    /// Stored name bytes (WTF-8) in the owning arena.
    pub name_offset: u32,
    pub parent_sequence: u16,
    pub name_len: u16,
}

// SAFETY: `repr(C)`, integers only, and no padding (8 + 8 + 4 + 2 + 2 bytes,
// checked below): every bit pattern is a valid value.
unsafe impl Plain for NameRecord {}
const _: () = assert!(size_of::<NameRecord>() == 24);

/// A record's [`RecordInfo`] as a table row: integers only, no padding, so
/// the per-record table can live in a memory-mapped temporary file. All zero
/// is a record that is not in use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
struct RecordSlot {
    created: i64,
    modified: i64,
    accessed: i64,
    size: u64,
    attributes: u32,
    reparse_tag: u32,
    sequence: u16,
    flags: u16,
    reserved: u32,
}

// SAFETY: `repr(C)`, integers only, and no padding (4 * 8 + 4 + 4 + 2 + 2 + 4
// bytes, checked below): every bit pattern is a valid value.
unsafe impl Plain for RecordSlot {}
const _: () = assert!(size_of::<RecordSlot>() == 48);

const SLOT_IN_USE: u16 = 1;
const SLOT_IS_DIR: u16 = 2;

impl RecordSlot {
    fn new(info: &RecordInfo) -> RecordSlot {
        RecordSlot {
            created: info.created,
            modified: info.modified,
            accessed: info.accessed,
            size: info.size,
            attributes: info.attributes,
            reparse_tag: info.reparse_tag,
            sequence: info.sequence,
            flags: if info.in_use { SLOT_IN_USE } else { 0 }
                | if info.is_dir { SLOT_IS_DIR } else { 0 },
            reserved: 0,
        }
    }

    fn info(&self) -> RecordInfo {
        RecordInfo {
            in_use: self.flags & SLOT_IN_USE != 0,
            is_dir: self.flags & SLOT_IS_DIR != 0,
            sequence: self.sequence,
            attributes: self.attributes,
            created: self.created,
            modified: self.modified,
            accessed: self.accessed,
            size: self.size,
            reparse_tag: self.reparse_tag,
        }
    }
}

/// A value that belongs to a base record but was found in one of its
/// extension records (files with many attributes or hard links).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Patch {
    Size { record: u64, size: u64 },
    Reparse { record: u64, tag: u32 },
}

/// Parse output for a run of records.
#[derive(Debug, Default)]
pub struct Parsed {
    pub infos: Vec<(u64, RecordInfo)>,
    pub names: Vec<NameRecord>,
    pub arena: Vec<u8>,
    pub patches: Vec<Patch>,
}

impl Parsed {
    pub fn append(&mut self, mut other: Parsed) {
        let base = self.arena.len() as u32;
        self.arena.append(&mut other.arena);
        self.names.extend(other.names.into_iter().map(|mut n| {
            n.name_offset += base;
            n
        }));
        self.infos.append(&mut other.infos);
        self.patches.append(&mut other.patches);
    }

    /// Heap held by the parse output, by capacity (memory accounting).
    pub fn heap_parts(&self) -> [(&'static str, usize); 4] {
        [
            (
                "parsed.infos",
                self.infos.capacity() * size_of::<(u64, RecordInfo)>(),
            ),
            (
                "parsed.names",
                self.names.capacity() * size_of::<NameRecord>(),
            ),
            ("parsed.arena", self.arena.capacity()),
            (
                "parsed.patches",
                self.patches.capacity() * size_of::<Patch>(),
            ),
        ]
    }
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// Undo the update-sequence protection: NTFS stores a check value in
/// the last two bytes of every 512-byte stride and keeps the real
/// bytes in the update-sequence array. Returns false for a torn or
/// corrupt record, which is then skipped.
pub fn apply_fixup(record: &mut [u8]) -> bool {
    if record.get(0..4) != Some(b"FILE") {
        return false;
    }
    let (Some(usa_offset), Some(usa_count)) = (u16_at(record, 4), u16_at(record, 6)) else {
        return false;
    };
    let usa_offset = usa_offset as usize;
    let usa_count = usa_count as usize;
    if usa_count < 2 || usa_offset + usa_count * 2 > record.len() {
        return false;
    }
    if (usa_count - 1) * 512 > record.len() {
        return false;
    }
    let check = [record[usa_offset], record[usa_offset + 1]];
    for i in 1..usa_count {
        let end = i * 512;
        if record[end - 2..end] != check {
            return false;
        }
        let fix = [record[usa_offset + 2 * i], record[usa_offset + 2 * i + 1]];
        record[end - 2..end].copy_from_slice(&fix);
    }
    true
}

/// Parse one record (after [`apply_fixup`]) numbered `number` into `out`.
pub fn parse_record(record: &[u8], number: u64, out: &mut Parsed) {
    let Some(flags) = u16_at(record, 22) else {
        return;
    };
    if flags & RECORD_IN_USE == 0 {
        return;
    }
    let sequence = u16_at(record, 16).unwrap_or(0);
    let base_ref = u64_at(record, 32).unwrap_or(0) & 0x0000_FFFF_FFFF_FFFF;
    let is_extension = base_ref != 0;
    let owner = if is_extension { base_ref } else { number };
    let mut info = RecordInfo {
        in_use: true,
        is_dir: flags & RECORD_IS_DIRECTORY != 0,
        sequence,
        ..RecordInfo::default()
    };
    let mut si_attributes = None;
    let used = (u32_at(record, 24).unwrap_or(0) as usize).min(record.len());
    let mut at = u16_at(record, 20).unwrap_or(0) as usize;
    let mut wide = Vec::new();
    while at + 16 <= used {
        let Some(kind) = u32_at(record, at) else {
            break;
        };
        if kind == ATTR_END {
            break;
        }
        let Some(len) = u32_at(record, at + 4) else {
            break;
        };
        let len = len as usize;
        if len < 16 || at + len > used {
            break;
        }
        let attr = &record[at..at + len];
        let non_resident = attr[8] != 0;
        let name_len = attr[9];
        let value = if non_resident {
            None
        } else {
            let vlen = u32_at(attr, 16).unwrap_or(0) as usize;
            let voff = u16_at(attr, 20).unwrap_or(0) as usize;
            attr.get(voff..voff + vlen)
        };
        match kind {
            ATTR_STANDARD_INFORMATION if !is_extension => {
                if let Some(v) = value
                    && v.len() >= 36
                {
                    info.created = u64_at(v, 0).unwrap_or(0) as i64;
                    info.modified = u64_at(v, 8).unwrap_or(0) as i64;
                    info.accessed = u64_at(v, 24).unwrap_or(0) as i64;
                    si_attributes = u32_at(v, 32);
                }
            }
            ATTR_FILE_NAME => {
                if let Some(v) = value
                    && v.len() >= 66
                {
                    let parent = u64_at(v, 0).unwrap_or(0);
                    let chars = v[64] as usize;
                    let namespace = v[65];
                    if namespace != NAMESPACE_DOS
                        && let Some(raw) = v.get(66..66 + chars * 2)
                    {
                        wide.clear();
                        wide.extend(
                            raw.as_chunks::<2>()
                                .0
                                .iter()
                                .map(|c| u16::from_le_bytes(*c)),
                        );
                        let start = out.arena.len();
                        ferrow_index::osname::encode_wtf8(&wide, &mut out.arena);
                        let stored = (out.arena.len() - start).min(u16::MAX as usize);
                        out.arena.truncate(start + stored);
                        out.names.push(NameRecord {
                            record: owner,
                            parent_record: parent & 0x0000_FFFF_FFFF_FFFF,
                            parent_sequence: (parent >> 48) as u16,
                            name_offset: start as u32,
                            name_len: stored as u16,
                        });
                    }
                }
            }
            ATTR_DATA if name_len == 0 => {
                let size = if non_resident {
                    // Only the first extent's header carries the size.
                    (u64_at(attr, 16) == Some(0))
                        .then(|| u64_at(attr, 48))
                        .flatten()
                } else {
                    value.map(|v| v.len() as u64)
                };
                if let Some(size) = size {
                    if is_extension {
                        out.patches.push(Patch::Size {
                            record: owner,
                            size,
                        });
                    } else {
                        info.size = size;
                    }
                }
            }
            ATTR_REPARSE_POINT => {
                if let Some(tag) = value.and_then(|v| u32_at(v, 0)) {
                    if is_extension {
                        out.patches.push(Patch::Reparse { record: owner, tag });
                    } else {
                        info.reparse_tag = tag;
                    }
                }
            }
            _ => {}
        }
        at += len;
    }
    if is_extension {
        return;
    }
    let mut attributes = si_attributes.unwrap_or(0) & !NTFS_INTERNAL_ATTRIBUTES;
    if info.is_dir {
        attributes |= FILE_ATTRIBUTE_DIRECTORY;
    }
    if attributes == 0 {
        attributes = FILE_ATTRIBUTE_NORMAL;
    }
    info.attributes = attributes;
    out.infos.push((number, info));
}

/// Decode a non-resident attribute's run list into
/// `(starting cluster, cluster count)`; sparse runs have no cluster.
pub fn decode_runs(runs: &[u8]) -> Option<Vec<(Option<u64>, u64)>> {
    let mut out = Vec::new();
    let mut at = 0;
    let mut lcn: i64 = 0;
    while let Some(&header) = runs.get(at) {
        if header == 0 {
            return Some(out);
        }
        let len_bytes = (header & 0x0F) as usize;
        let off_bytes = (header >> 4) as usize;
        if len_bytes == 0 || len_bytes > 8 || off_bytes > 8 {
            return None;
        }
        let len_raw = runs.get(at + 1..at + 1 + len_bytes)?;
        let mut length = 0u64;
        for (i, b) in len_raw.iter().enumerate() {
            length |= (*b as u64) << (8 * i);
        }
        let off_raw = runs.get(at + 1 + len_bytes..at + 1 + len_bytes + off_bytes)?;
        if off_bytes == 0 {
            out.push((None, length));
        } else {
            let mut delta = 0i64;
            for (i, b) in off_raw.iter().enumerate() {
                delta |= (*b as i64) << (8 * i);
            }
            // Sign-extend from the highest stored byte.
            let shift = 64 - 8 * off_bytes as u32;
            delta = (delta << shift) >> shift;
            lcn = lcn.checked_add(delta)?;
            if lcn < 0 {
                return None;
            }
            out.push((Some(lcn as u64), length));
        }
        at += 1 + len_bytes + off_bytes;
    }
    None
}

/// The run list of record 0's unnamed `$DATA` attribute: where the MFT
/// itself lives on disk.
pub fn mft_runs(record0: &[u8]) -> Option<Vec<(Option<u64>, u64)>> {
    let used = (u32_at(record0, 24)? as usize).min(record0.len());
    let mut at = u16_at(record0, 20)? as usize;
    while at + 16 <= used {
        let kind = u32_at(record0, at)?;
        if kind == ATTR_END {
            break;
        }
        let len = u32_at(record0, at + 4)? as usize;
        if len < 16 || at + len > used {
            break;
        }
        let attr = &record0[at..at + len];
        if kind == ATTR_DATA && attr[8] != 0 && attr[9] == 0 {
            let runs_at = u16_at(attr, 32)? as usize;
            return decode_runs(attr.get(runs_at..)?);
        }
        at += len;
    }
    None
}

/// `(parent record, stored name bytes)` for each of a file's names.
pub type Names = Vec<(u64, Vec<u8>)>;

/// One change journal record (version 2 layout).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsnRecord {
    /// The changed file's record number.
    pub record: u64,
    pub usn: i64,
    /// FILETIME ticks.
    pub timestamp: i64,
    pub reason: u32,
}

/// Parse the records of one `FSCTL_READ_USN_JOURNAL` result (after its
/// leading next-USN field). Unknown versions and damaged lengths stop
/// the parse.
pub fn parse_usn_records(mut bytes: &[u8]) -> Vec<UsnRecord> {
    let mut out = Vec::new();
    while bytes.len() >= 60 {
        let Some(len) = u32_at(bytes, 0) else { break };
        let len = len as usize;
        if len < 60 || len > bytes.len() {
            break;
        }
        if u16_at(bytes, 4) == Some(2) {
            out.push(UsnRecord {
                record: u64_at(bytes, 8).unwrap_or(0) & 0x0000_FFFF_FFFF_FFFF,
                usn: u64_at(bytes, 24).unwrap_or(0) as i64,
                timestamp: u64_at(bytes, 32).unwrap_or(0) as i64,
                reason: u32_at(bytes, 40).unwrap_or(0),
            });
        }
        bytes = &bytes[len..];
    }
    out
}

/// Bytes of temporary files a read of one volume keeps per MFT record at its
/// highest: the record table (48), the names (24) and their text (about 20),
/// the grouping (8) and the record map the scan appends (16). **An estimate**
/// from a synthetic stream (118 to 121 B per record at 1 M entries), rounded
/// up; a volume whose names are far longer or with many hard links needs more.
pub const FILE_BYTES_PER_RECORD: u64 = 120;

/// Bytes of temporary files a read of a volume of `records` MFT records is
/// expected to need (see [`FILE_BYTES_PER_RECORD`]).
pub fn files_estimate(records: u64) -> u64 {
    records.saturating_mul(FILE_BYTES_PER_RECORD)
}

/// What a read with temporary files may do when it cannot have them. The
/// numbers are the caller's to set so that a test can say "a volume this
/// large with this much room" without a volume or a full disk.
#[derive(Clone, Copy, Debug)]
pub struct SpillPolicy {
    /// A volume with more MFT records than this is never read into the heap
    /// instead of files (the heap would take gigabytes: the reason files
    /// exist): its read fails with [`crate::BUILD_FAILED`] and the service
    /// tries again later. A smaller one falls back to the heap, counted
    /// ([`ferrow_index::store::scratch_in_heap`]) and noted.
    pub heap_fallback_records: u64,
    /// Bytes free where the files go, when known; unknown means no check
    /// (the first table that cannot be made then decides).
    pub free_bytes: Option<u64>,
}

impl SpillPolicy {
    /// The policy of a real read: the shared fallback limit and the disk's
    /// free space beside `dir`.
    pub fn for_dir(dir: &Path) -> SpillPolicy {
        SpillPolicy {
            heap_fallback_records: crate::HEAP_FALLBACK_ENTRIES as u64,
            free_bytes: crate::free_bytes(dir),
        }
    }
}

/// The failure of a read that must not fall back to the heap.
fn build_failed(why: impl std::fmt::Display) -> String {
    format!("{}: {why}", crate::BUILD_FAILED)
}

/// Where a builder's tables go: the directory and whether the volume is too
/// large for the heap to stand in for a file.
struct Spill {
    dir: PathBuf,
    records: u64,
    large: bool,
}

impl Spill {
    /// A zeroed table of `len` values in a file. If the file cannot be made:
    /// a large volume fails the read, a small one gets the heap (counted,
    /// `note` says why).
    fn table<T: Plain>(&self, len: usize, note: &mut Option<String>) -> Result<Scratch<T>, String> {
        match Scratch::try_zeroed(len, &self.dir) {
            Ok(table) => Ok(table),
            Err(error) if self.large => Err(self.refused("a table", &error)),
            Err(error) => {
                count_heap_fallback();
                note.get_or_insert_with(|| {
                    format!(
                        "a scan table could not be put in a file ({error}); it is kept in memory"
                    )
                });
                Ok(Scratch::zeroed(len, None))
            }
        }
    }

    /// A names or arena appender: a failed file may continue in the heap only
    /// for a small volume.
    fn appender<T: Plain>(&self) -> Result<Appender<T>, String> {
        if self.large {
            Appender::new(&self.dir).map_err(|e| self.refused("the names", &e))
        } else {
            Ok(Appender::or_heap(&self.dir))
        }
    }

    fn refused(&self, what: &str, error: &dyn std::fmt::Display) -> String {
        build_failed(format!(
            "{what} for a volume of {} MFT records could not be put in a file beside the index ({error}); not read into memory instead",
            self.records
        ))
    }
}

/// Names forgotten by [`VolumeBuilder::forget`] in a builder whose names are
/// in files: a name is dead when its index is below the cut-off made at that
/// call and its record was forgotten then. (A builder that holds its names in
/// the heap removes them instead.)
///
/// The grouping asks this for every name of the volume, twice, so the answer
/// for the usual name must cost a few instructions, not a hash. Kept as a
/// bitmap over the record numbers (one bit per record: 3 MB for 24 million
/// records, built only when something was forgotten; the names arrive about in
/// record order, so the bits are read nearly sequentially) in front of the
/// forgotten records sorted with the highest cut-off each was forgotten at,
/// which is searched only for a record whose bit is set. Records beyond the
/// bitmap's cap (far above any real MFT) skip it and are searched directly.
struct Forgotten {
    /// Bit `r` is set when record `r` was forgotten (records below
    /// `bits.len() * 64`).
    bits: Vec<u64>,
    /// `(record, cut-off)`, sorted by record, one per record.
    dead: Vec<(u64, usize)>,
}

/// The most record numbers the bitmap covers: 2^27 records, a 16 MB bitmap.
const FORGOTTEN_BITMAP_RECORDS: u64 = 1 << 27;

impl Forgotten {
    fn new() -> Forgotten {
        Forgotten {
            bits: Vec::new(),
            dead: Vec::new(),
        }
    }

    /// Forget `records`' names below index `cut`.
    fn add(&mut self, cut: usize, records: impl IntoIterator<Item = u64>) {
        self.dead.extend(records.into_iter().map(|r| (r, cut)));
        self.dead.sort_unstable();
        // One entry per record, with the highest cut-off (sorted ascending).
        self.dead.dedup_by(|later, kept| {
            let same = later.0 == kept.0;
            if same {
                kept.1 = kept.1.max(later.1);
            }
            same
        });
        let covered = self
            .dead
            .iter()
            .map(|d| d.0)
            .filter(|&r| r < FORGOTTEN_BITMAP_RECORDS)
            .max()
            .map_or(0, |max| max as usize / 64 + 1);
        self.bits = vec![0; covered];
        for &(record, _) in self.dead.iter().filter(|d| d.0 < FORGOTTEN_BITMAP_RECORDS) {
            self.bits[record as usize / 64] |= 1 << (record % 64);
        }
    }

    fn is_empty(&self) -> bool {
        self.dead.is_empty()
    }

    /// Whether name number `index`, of file `record`, was forgotten.
    #[inline]
    fn kills(&self, record: u64, index: usize) -> bool {
        let candidate = if record < FORGOTTEN_BITMAP_RECORDS {
            self.bits
                .get(record as usize / 64)
                .is_some_and(|word| word >> (record % 64) & 1 == 1)
        } else {
            !self.dead.is_empty()
        };
        candidate
            && self
                .dead
                .binary_search_by_key(&record, |d| d.0)
                .is_ok_and(|at| index < self.dead[at].1)
    }
}

/// The whole volume's MFT contents, indexed for subtree emission.
///
/// Its tables are columns that are either in the heap or read from temporary
/// files ([`VolumeBuilder::with_spill`]); everything reads them as slices.
#[derive(Debug, Default)]
pub struct Volume {
    /// Indexed by record number.
    infos: Col<RecordSlot>,
    names: Col<NameRecord>,
    arena: Col<u8>,
    /// `names` indices grouped by parent record: the children of record
    /// `r` are `order[first[r]..first[r + 1]]`.
    order: Col<u32>,
    first: Col<u32>,
    pub serial: u64,
    /// Change journal id and the position after the read, when the
    /// volume has a journal (the starting point for live updates).
    pub journal: Option<(u64, i64)>,
    /// Set when the MFT was read but the change journal could not be
    /// re-checked afterwards: the volume still goes live, from the position
    /// before the read, and this says what may be stale.
    pub warning: Option<String>,
    /// Bytes per MFT record.
    pub record_size: usize,
    /// Why the volume was read in memory although temporary files were wanted
    /// (a file could not be made or failed), when it was.
    pub spill_note: Option<String>,
}

/// Where a [`VolumeBuilder`] keeps the names it was given.
enum NameSink {
    /// One exactly sized pair of vectors per chunk, joined by `finish` (growing
    /// one vector chunk by chunk would hold up to twice the names at every
    /// doubling).
    Chunks(Vec<(Vec<NameRecord>, Vec<u8>)>),
    /// Appended to temporary files as each chunk arrives, with their arena
    /// offsets already shifted to the whole volume's arena.
    Files {
        names: Appender<NameRecord>,
        arena: Appender<u8>,
        /// The chunk's names being shifted (a reused buffer).
        shifted: Vec<NameRecord>,
        /// A chunk's names did not fit the 32-bit arena offsets.
        overflow: bool,
    },
}

/// Takes the parse chunks of a volume one by one into the layout [`Volume`]
/// needs, so the whole parse never sits beside the finished volume. The
/// per-record table is allocated once for all `records` and filled as each
/// chunk arrives.
///
/// [`VolumeBuilder::new`] keeps everything in the heap (a chunk's names stay
/// in their own exactly sized vectors until [`VolumeBuilder::finish`] joins
/// them). [`VolumeBuilder::with_spill`] keeps the per-record table, the names
/// and the grouping tables in anonymous temporary files instead (ADR 0007):
/// a full scan's private memory is then a fixed working set, whatever the
/// number of records, and the tables are the same bytes either way.
pub struct VolumeBuilder {
    infos: Scratch<RecordSlot>,
    names: NameSink,
    patches: Vec<Patch>,
    forgotten: Forgotten,
    /// Where the tables go (also those `finish` makes: the grouping by
    /// parent); `None`: the heap.
    spill: Option<Spill>,
    note: Option<String>,
}

impl VolumeBuilder {
    /// A builder for a volume of `records` MFT records (used or free), all in
    /// the heap.
    pub fn new(records: u64) -> VolumeBuilder {
        VolumeBuilder {
            infos: Scratch::zeroed(records as usize, None),
            names: NameSink::Chunks(Vec::new()),
            patches: Vec::new(),
            forgotten: Forgotten::new(),
            spill: None,
            note: None,
        }
    }

    /// A builder whose tables live in anonymous temporary files in `dir`,
    /// given the room `policy` says is there.
    ///
    /// Never silently large in the heap: when the disk has less room than
    /// [`files_estimate`] says the read needs, or a file cannot be made, a
    /// volume above `policy.heap_fallback_records` gets an error that starts
    /// with [`crate::BUILD_FAILED`] (the service backs off and retries), and a
    /// smaller one is built in the heap, counted
    /// ([`ferrow_index::store::scratch_in_heap`]) and described by
    /// [`Self::note`]: the volume is the same, only larger.
    pub fn with_spill(
        records: u64,
        dir: &Path,
        policy: &SpillPolicy,
    ) -> Result<VolumeBuilder, String> {
        let large = records > policy.heap_fallback_records;
        let need = files_estimate(records);
        if let Some(free) = policy.free_bytes.filter(|&free| free < need) {
            let why = format!(
                "{} MB free beside the index, a read of {records} MFT records needs about {} MB of temporary files",
                free >> 20,
                need >> 20
            );
            if large {
                return Err(build_failed(format!(
                    "{why}; a volume this large is not read into memory instead"
                )));
            }
            count_heap_fallback();
            let mut builder = VolumeBuilder::new(records);
            builder.note = Some(format!("{why}; the volume is read in memory"));
            return Ok(builder);
        }
        let spill = Spill {
            dir: dir.to_path_buf(),
            records,
            large,
        };
        let mut note = None;
        let infos = spill.table(records as usize, &mut note)?;
        let names = NameSink::Files {
            names: spill.appender()?,
            arena: spill.appender()?,
            shifted: Vec::new(),
            overflow: false,
        };
        Ok(VolumeBuilder {
            infos,
            names,
            patches: Vec::new(),
            forgotten: Forgotten::new(),
            spill: Some(spill),
            note,
        })
    }

    /// Why a table is in the heap although files were wanted, if one is.
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref().or(match &self.names {
            NameSink::Files { names, arena, .. } => names.fallback().or(arena.fallback()),
            NameSink::Chunks(_) => None,
        })
    }

    /// Why the volume cannot be finished, once that is certain: a names file
    /// failed on a volume too large for the heap, or the names outgrew the
    /// 32-bit arena offsets. A read stops at the first `Some` instead of
    /// reading the rest of the volume to fail at the end.
    pub fn failure(&self) -> Option<String> {
        match &self.names {
            NameSink::Files {
                names,
                arena,
                overflow,
                ..
            } => {
                if *overflow {
                    return Some("the names of the volume are larger than 4 GiB".into());
                }
                names
                    .failure()
                    .or(arena.failure())
                    .map(|why| format!("a names file failed ({why})"))
            }
            NameSink::Chunks(_) => None,
        }
    }

    /// Take one parsed chunk; later values for a record replace earlier ones.
    pub fn push(&mut self, chunk: Parsed) {
        for (n, info) in &chunk.infos {
            if let Some(slot) = self.infos.get_mut(*n as usize) {
                *slot = RecordSlot::new(info);
            }
        }
        self.patches.extend(chunk.patches);
        match &mut self.names {
            NameSink::Chunks(chunks) => {
                let (mut names, mut arena) = (chunk.names, chunk.arena);
                names.shrink_to_fit();
                arena.shrink_to_fit();
                chunks.push((names, arena));
            }
            NameSink::Files {
                names,
                arena,
                shifted,
                overflow,
            } => {
                let base = arena.len();
                if *overflow || base + chunk.arena.len() > u32::MAX as usize {
                    *overflow = true;
                    return;
                }
                shifted.clear();
                shifted.extend(chunk.names.iter().map(|n| NameRecord {
                    name_offset: n.name_offset + base as u32,
                    ..*n
                }));
                names.extend_from_slice(shifted);
                arena.extend_from_slice(&chunk.arena);
            }
        }
    }

    /// Forget what was parsed for the records in `changed`, so a fresh parse
    /// of them can be pushed (the journal refresh after a long read). A
    /// changed record's name or patch stays only where the matching `keep_*`
    /// says so.
    pub fn forget(
        &mut self,
        changed: &std::collections::BTreeSet<u64>,
        keep_names: impl Fn(u64) -> bool,
        keep_patches: impl Fn(u64) -> bool,
    ) {
        for &n in changed {
            if let Some(slot) = self.infos.get_mut(n as usize) {
                *slot = RecordSlot::default();
            }
        }
        self.patches.retain(|p| match p {
            Patch::Size { record, .. } | Patch::Reparse { record, .. } => keep_patches(*record),
        });
        match &mut self.names {
            NameSink::Chunks(chunks) => {
                for (names, _) in chunks {
                    names.retain(|n| keep_names(n.record));
                }
            }
            NameSink::Files { names, .. } => {
                // Only a changed record's names can go; the names pushed
                // after this call are not affected.
                let cut = names.len();
                self.forgotten.add(
                    cut,
                    changed
                        .iter()
                        .copied()
                        .filter(|&record| !keep_names(record)),
                );
            }
        }
    }

    /// Heap held while the chunks are collected, by capacity (memory
    /// accounting).
    pub fn heap_parts(&self) -> [(&'static str, usize); 4] {
        let (names, arena) = match &self.names {
            NameSink::Chunks(chunks) => (
                chunks
                    .iter()
                    .map(|c| c.0.capacity() * size_of::<NameRecord>())
                    .sum(),
                chunks.iter().map(|c| c.1.capacity()).sum(),
            ),
            NameSink::Files {
                names,
                arena,
                shifted,
                ..
            } => (
                names.heap_bytes() + shifted.capacity() * size_of::<NameRecord>(),
                arena.heap_bytes(),
            ),
        };
        [
            ("parsed.infos", self.infos.heap_bytes()),
            ("parsed.names", names),
            ("parsed.arena", arena),
            (
                "parsed.patches",
                self.patches.capacity() * size_of::<Patch>(),
            ),
        ]
    }

    /// Bytes of temporary files the builder holds (its tables, written or
    /// waiting to be).
    pub fn file_bytes(&self) -> usize {
        self.infos.file_bytes()
            + match &self.names {
                NameSink::Chunks(_) => 0,
                NameSink::Files { names, arena, .. } => names.file_bytes() + arena.file_bytes(),
            }
    }

    /// Join the chunks into the volume. Names and arena are made one after the
    /// other and every chunk's part is freed as soon as it is copied, so the
    /// copy never holds more than one extra copy of the larger of the two.
    ///
    /// # Panics
    /// When the volume cannot be made (see [`VolumeBuilder::finish_checked`]):
    /// only possible for a builder with files or names beyond the 32-bit
    /// arena offsets.
    pub fn finish(self, serial: u64) -> Volume {
        self.finish_checked(serial)
            .unwrap_or_else(|error| panic!("cannot finish the volume: {error}"))
    }

    /// [`VolumeBuilder::finish`], reporting a failure: names that do not fit
    /// the 32-bit arena offsets, or a table file that failed and could not be
    /// read back.
    pub fn finish_checked(mut self, serial: u64) -> Result<Volume, String> {
        for patch in &self.patches {
            match *patch {
                Patch::Size { record, size } => {
                    if let Some(slot) = self.infos.get_mut(record as usize) {
                        slot.size = size;
                    }
                }
                Patch::Reparse { record, tag } => {
                    if let Some(slot) = self.infos.get_mut(record as usize) {
                        slot.reparse_tag = tag;
                    }
                }
            }
        }
        let (names, arena): (Col<NameRecord>, Col<u8>) =
            match std::mem::replace(&mut self.names, NameSink::Chunks(Vec::new())) {
                NameSink::Chunks(mut chunks) => {
                    let mut bases = Vec::with_capacity(chunks.len());
                    let mut arena_len = 0usize;
                    for (_, arena) in &chunks {
                        bases.push(arena_len as u32);
                        arena_len += arena.len();
                    }
                    if arena_len > u32::MAX as usize {
                        return Err("the names of the volume are larger than 4 GiB".into());
                    }
                    let mut names = Vec::with_capacity(chunks.iter().map(|c| c.0.len()).sum());
                    for ((chunk, _), base) in chunks.iter_mut().zip(&bases) {
                        names.extend(chunk.drain(..).map(|mut n| {
                            n.name_offset += base;
                            n
                        }));
                        *chunk = Vec::new();
                    }
                    let mut arena = Vec::with_capacity(arena_len);
                    for (_, part) in &mut chunks {
                        arena.extend_from_slice(part);
                        *part = Vec::new();
                    }
                    (names.into(), arena.into())
                }
                NameSink::Files {
                    names,
                    arena,
                    overflow,
                    ..
                } => {
                    if overflow {
                        return Err("the names of the volume are larger than 4 GiB".into());
                    }
                    let fell_back = names.fallback().or(arena.fallback()).map(str::to_string);
                    let names = names
                        .finish()
                        .map_err(|e| format!("the names table: {e}"))?;
                    let arena = arena
                        .finish()
                        .map_err(|e| format!("the names arena: {e}"))?;
                    if let Some(why) = fell_back {
                        self.note.get_or_insert(format!(
                            "a names file failed ({why}); its names are kept in memory"
                        ));
                    }
                    (names, arena)
                }
            };
        let infos = std::mem::take(&mut self.infos).freeze();
        let mut volume = Volume::group(
            infos,
            names,
            arena,
            serial,
            &self.forgotten,
            self.spill.as_ref(),
            &mut self.note,
        )?;
        volume.spill_note = self.note;
        Ok(volume)
    }
}

impl Volume {
    pub fn from_parsed(parsed: Parsed, records: u64, serial: u64) -> Volume {
        let mut builder = VolumeBuilder::new(records);
        builder.push(parsed);
        builder.finish(serial)
    }

    /// The volume from its tables: groups the names by parent record (in
    /// `spill`'s temporary files when there is one). Names of a record that
    /// `forgotten` lists, and names of a parent beyond the table, are left out
    /// of the grouping.
    fn group(
        infos: Col<RecordSlot>,
        names: Col<NameRecord>,
        arena: Col<u8>,
        serial: u64,
        forgotten: &Forgotten,
        spill: Option<&Spill>,
        note: &mut Option<String>,
    ) -> Result<Volume, String> {
        let records = infos.len();
        let any_forgotten = !forgotten.is_empty();
        let counted = |i: usize, n: &NameRecord| {
            (n.parent_record as usize) < records && !(any_forgotten && forgotten.kills(n.record, i))
        };
        let table = |len: usize, note: &mut Option<String>| match spill {
            Some(spill) => spill.table::<u32>(len, note),
            None => Ok(Scratch::zeroed(len, None)),
        };
        let mut first = table(records + 1, note)?;
        for (i, n) in names.iter().enumerate() {
            if counted(i, n) {
                first[n.parent_record as usize + 1] += 1;
            }
        }
        for i in 1..first.len() {
            first[i] += first[i - 1];
        }
        let total = first[records] as usize;
        let mut order = table(total, note)?;
        // Fill by advancing each parent's start, then shift the starts back
        // (no second table of `records` entries).
        for (i, n) in names.iter().enumerate() {
            if counted(i, n) {
                let p = n.parent_record as usize;
                order[first[p] as usize] = i as u32;
                first[p] += 1;
            }
        }
        for i in (1..=records).rev() {
            first[i] = first[i - 1];
        }
        first[0] = 0;
        Ok(Volume {
            infos,
            names,
            arena,
            order: order.freeze(),
            first: first.freeze(),
            serial,
            journal: None,
            warning: None,
            record_size: 1024,
            spill_note: None,
        })
    }

    /// MFT records the volume has a slot for (used or free).
    pub fn records(&self) -> usize {
        self.infos.len()
    }

    /// Names held (a file with several names has several; names a journal
    /// refresh forgot may still be counted).
    pub fn name_count(&self) -> usize {
        self.names.len()
    }

    /// Bytes of name text held.
    pub fn arena_len(&self) -> usize {
        self.arena.len()
    }

    /// Heap held while the volume is in memory, by capacity (memory
    /// accounting): the per-record table, the names, their arena and the
    /// parent grouping. A table read from a temporary file holds none.
    pub fn heap_parts(&self) -> [(&'static str, usize); 5] {
        [
            ("volume.infos", self.infos.heap_bytes()),
            ("volume.names", self.names.heap_bytes()),
            ("volume.arena", self.arena.heap_bytes()),
            ("volume.order", self.order.heap_bytes()),
            ("volume.first", self.first.heap_bytes()),
        ]
    }

    /// Bytes of the volume's tables that are read from temporary files (file
    /// cache, not private memory).
    pub fn mapped_bytes(&self) -> usize {
        self.infos.mapped_bytes()
            + self.names.mapped_bytes()
            + self.arena.mapped_bytes()
            + self.order.mapped_bytes()
            + self.first.mapped_bytes()
    }

    pub fn info(&self, record: u64) -> Option<RecordInfo> {
        self.infos
            .get(record as usize)
            .filter(|slot| slot.flags & SLOT_IN_USE != 0)
            .map(RecordSlot::info)
    }

    /// The current names directly inside directory `record`: entries
    /// whose stored parent reference still matches the directory's
    /// sequence (a stale reference belongs to a deleted directory).
    pub fn children(&self, record: u64) -> impl Iterator<Item = (&NameRecord, &[u8])> + '_ {
        self.children_with_info(record)
            .map(|(name, bytes, _)| (name, bytes))
    }

    /// [`Volume::children`] with each child's record (read once, as the scan
    /// needs it right away).
    pub fn children_with_info(
        &self,
        record: u64,
    ) -> impl Iterator<Item = (&NameRecord, &[u8], RecordInfo)> + '_ {
        let (start, end, sequence) = match self.info(record) {
            Some(info) if (record as usize) + 1 < self.first.len() => (
                self.first[record as usize] as usize,
                self.first[record as usize + 1] as usize,
                info.sequence,
            ),
            _ => (0, 0, 0),
        };
        self.order[start..end].iter().filter_map(move |&i| {
            let n = &self.names[i as usize];
            if n.parent_sequence != sequence || n.record == record || n.record < FIRST_USER_RECORD {
                return None;
            }
            let info = self.info(n.record)?;
            let off = n.name_offset as usize;
            Some((n, &self.arena[off..off + n.name_len as usize], info))
        })
    }
}

/// `ERROR_INVALID_PARAMETER`: a journal read at a position that is not the
/// start of a record.
#[cfg_attr(not(windows), allow(dead_code))]
const ERROR_INVALID_PARAMETER: i32 = 87;
/// `ERROR_JOURNAL_ENTRY_DELETED`: the requested USN was purged.
#[cfg_attr(not(windows), allow(dead_code))]
const ERROR_JOURNAL_ENTRY_DELETED: i32 = 1181;
/// The change journal's page size: a record never crosses it, so a
/// page-aligned position is the start of a record.
#[cfg_attr(not(windows), allow(dead_code))]
const USN_PAGE: i64 = 4096;
/// How far before the MFT read the journal is re-read, to catch changes
/// that had not reached the disk when the raw read ran.
#[cfg_attr(not(windows), allow(dead_code))]
const RECENT_WINDOW_BYTES: i64 = 64 << 20;
/// A refused start is retried from the journal's first record only when
/// the whole journal is at most this long: a journal of many GiB would be a
/// long read at SYSTEM priority for two minutes of changes.
#[cfg_attr(not(windows), allow(dead_code))]
const MAX_FULL_REREAD_BYTES: i64 = 256 << 20;

/// Where the re-read of the journal after an MFT read starts: the position
/// `RECENT_WINDOW_BYTES` before `before` (the journal's next position when
/// the read began), rounded down to a page so it is a record boundary, and
/// never before `first` (the oldest record the journal still holds, itself a
/// record start). A journal longer than the window used to start in the
/// middle of a record, which `FSCTL_READ_USN_JOURNAL` refuses.
#[cfg_attr(not(windows), allow(dead_code))]
fn recent_window_start(first: i64, before: i64) -> i64 {
    let windowed = before.saturating_sub(RECENT_WINDOW_BYTES).max(0);
    first.max(windowed - windowed % USN_PAGE)
}

/// The journal's position after the re-read, and what to tell the user when
/// the re-read itself failed.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
struct Reread {
    next: i64,
    warning: Option<String>,
}

/// Re-read the journal for the changes just before an MFT read.
/// `read(start)` feeds the records from `start` to the caller and returns
/// the next position; `query()` is the journal's current
/// `(id, first usn, next usn)`.
///
/// - A start that the journal refuses as a bad parameter is retried once
///   from `first`, when the journal is short enough to read whole.
/// - A purged start continues from the journal's new beginning (up to three
///   times); a purge that cannot be followed is an error, as is a journal
///   that was replaced.
/// - Any other failure keeps the MFT read when the same journal still holds
///   every record since `before`: the position stays at `before`, so the
///   live reader replays every change since the read began, and a warning
///   says that changes just before the read may be missing. A journal that
///   is gone, was replaced or was purged past `before` is an error.
#[cfg_attr(not(windows), allow(dead_code))]
fn reread_journal(
    journal: (u64, i64, i64),
    mut read: impl FnMut(i64) -> std::io::Result<i64>,
    mut query: impl FnMut() -> Option<(u64, i64, i64)>,
) -> Result<Reread, String> {
    let (id, first, before) = journal;
    let mut start = recent_window_start(first, before);
    let mut purged = 0;
    let mut from_first = start <= first;
    loop {
        let e = match read(start) {
            Ok(next) => {
                return Ok(Reread {
                    next,
                    warning: None,
                });
            }
            Err(e) => e,
        };
        match e.raw_os_error() {
            // A busy volume with a small journal purges its oldest records
            // while the MFT is read. Continue from the journal's current
            // start: the purged records are its oldest ones. Giving up here
            // used to send the whole volume to the directory walk (minutes,
            // no live updates).
            Some(ERROR_JOURNAL_ENTRY_DELETED) => match query() {
                Some((now_id, now_first, _)) if now_id == id && now_first > start && purged < 3 => {
                    purged += 1;
                    start = now_first;
                    from_first = true;
                }
                _ => return Err(format!("read change journal: {e}")),
            },
            Some(ERROR_INVALID_PARAMETER)
                if !from_first && before.saturating_sub(first) <= MAX_FULL_REREAD_BYTES =>
            {
                from_first = true;
                start = first;
            }
            _ => {
                return match query() {
                    Some((now_id, now_first, _)) if now_id == id && now_first <= before => {
                        Ok(Reread {
                            next: before,
                            warning: Some(format!(
                                "could not re-check the change journal after reading the MFT ({e}); files changed just before the scan may show their old state until the next scan"
                            )),
                        })
                    }
                    Some((now_id, _, _)) if now_id != id => Err(format!(
                        "read change journal: {e}; the journal was replaced during the scan"
                    )),
                    Some(_) => Err(format!(
                        "read change journal: {e}; changes since the scan began were already purged"
                    )),
                    None => Err(format!(
                        "read change journal: {e}; the change journal is no longer available"
                    )),
                };
            }
        }
    }
}

#[cfg(windows)]
pub(crate) use platform::low_io_priority;
#[cfg(windows)]
pub use platform::{
    can_read_volume, fetch_record, fetch_state, journal_capacity, open_volume, query_journal,
    read_journal, read_volume, read_volume_in, record_is_free, volume_device, volume_serial,
    wait_journal,
};

#[cfg(windows)]
mod platform {
    use super::{
        Parsed, UsnRecord, Volume, VolumeBuilder, apply_fixup, mft_runs, parse_record,
        parse_usn_records, reread_journal,
    };
    use rayon::prelude::*;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::os::windows::fs::{FileExt, OpenOptionsExt};
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    /// Bytes read per request; a multiple of every record and cluster size.
    const CHUNK: usize = 16 << 20;
    /// `FILE_FLAG_NO_BUFFERING`: raw MFT reads bypass the file cache, so a
    /// scan never evicts other programs' cached data.
    const NO_BUFFERING: u32 = 0x2000_0000;
    /// Unbuffered reads need sector-aligned memory; 4 KiB covers every disk.
    const ALIGN: usize = 4096;
    /// Ask the disk stack to serve this handle's reads at low priority
    /// (background rescans). Best effort: on failure reads stay normal.
    pub(crate) fn low_io_priority(volume: &std::fs::File) {
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_IO_PRIORITY_HINT_INFO, FileIoPriorityHintInfo, IoPriorityHintLow,
            SetFileInformationByHandle,
        };
        let hint = FILE_IO_PRIORITY_HINT_INFO {
            PriorityHint: IoPriorityHintLow,
        };
        // SAFETY: live handle; the input pointer and size describe `hint`.
        unsafe {
            SetFileInformationByHandle(
                volume.as_raw_handle(),
                FileIoPriorityHintInfo,
                (&raw const hint).cast(),
                size_of::<FILE_IO_PRIORITY_HINT_INFO>() as u32,
            );
        }
    }

    /// A zeroed, 4 KiB-aligned byte buffer for unbuffered reads.
    struct Aligned {
        ptr: std::ptr::NonNull<u8>,
        len: usize,
    }

    // SAFETY: the buffer is uniquely owned; moving it between threads is fine.
    unsafe impl Send for Aligned {}

    impl Aligned {
        fn layout(len: usize) -> std::alloc::Layout {
            std::alloc::Layout::from_size_align(len.max(ALIGN).next_multiple_of(ALIGN), ALIGN)
                .expect("valid read buffer layout")
        }
        fn new(len: usize) -> Aligned {
            let layout = Self::layout(len);
            // SAFETY: non-zero size, valid alignment.
            let raw = unsafe { std::alloc::alloc_zeroed(layout) };
            let ptr = std::ptr::NonNull::new(raw)
                .unwrap_or_else(|| std::alloc::handle_alloc_error(layout));
            Aligned { ptr, len }
        }
        fn as_mut_slice(&mut self) -> &mut [u8] {
            // SAFETY: `len` initialized (zeroed) bytes owned by this buffer.
            unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
        }
    }

    impl Drop for Aligned {
        fn drop(&mut self) {
            // SAFETY: allocated in `new` with this exact layout.
            unsafe { std::alloc::dealloc(self.ptr.as_ptr(), Self::layout(self.len)) }
        }
    }

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// The `\\?\Volume{…}` device path of the NTFS volume holding `path`.
    pub fn volume_device(path: &Path) -> Result<String, String> {
        ntfs_volume_device(path)
    }

    /// The serial number of an open NTFS volume (what a scan records).
    pub fn volume_serial(volume: &std::fs::File) -> Option<u64> {
        use windows_sys::Win32::System::IO::DeviceIoControl;
        use windows_sys::Win32::System::Ioctl::{
            FSCTL_GET_NTFS_VOLUME_DATA, NTFS_VOLUME_DATA_BUFFER,
        };
        // SAFETY: zeroed POD output buffer.
        let mut data: NTFS_VOLUME_DATA_BUFFER = unsafe { std::mem::zeroed() };
        let mut returned = 0u32;
        // SAFETY: live volume handle; output pointer and size describe `data`.
        let ok = unsafe {
            DeviceIoControl(
                volume.as_raw_handle(),
                FSCTL_GET_NTFS_VOLUME_DATA,
                std::ptr::null(),
                0,
                (&raw mut data).cast(),
                size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
                &raw mut returned,
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(data.VolumeSerialNumber as u64)
    }

    /// The `\\?\Volume{…}` device path of the volume holding `path`,
    /// if it is NTFS.
    fn ntfs_volume_device(path: &Path) -> Result<String, String> {
        use windows_sys::Win32::Storage::FileSystem::{
            GetVolumeInformationW, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
        };
        let path_w = wide(path.as_os_str());
        let mut mount = [0u16; 1024];
        // SAFETY: NUL-terminated input; output buffer and its length match.
        if unsafe { GetVolumePathNameW(path_w.as_ptr(), mount.as_mut_ptr(), mount.len() as u32) }
            == 0
        {
            return Err(format!(
                "volume of {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
        let mut fs_name = [0u16; 64];
        // SAFETY: `mount` is NUL-terminated; optional outputs are null and
        // the file-system name buffer length matches.
        let ok = unsafe {
            GetVolumeInformationW(
                mount.as_ptr(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs_name.as_mut_ptr(),
                fs_name.len() as u32,
            )
        };
        if ok == 0 {
            return Err(format!(
                "volume information: {}",
                std::io::Error::last_os_error()
            ));
        }
        let fs_len = fs_name.iter().position(|&c| c == 0).unwrap_or(0);
        let fs = String::from_utf16_lossy(&fs_name[..fs_len]);
        if fs != "NTFS" {
            return Err(format!("{fs} volume"));
        }
        let mut guid = [0u16; 128];
        // SAFETY: `mount` is NUL-terminated and ends with a backslash;
        // output buffer and its length match.
        if unsafe {
            GetVolumeNameForVolumeMountPointW(mount.as_ptr(), guid.as_mut_ptr(), guid.len() as u32)
        } == 0
        {
            return Err(format!("volume name: {}", std::io::Error::last_os_error()));
        }
        let len = guid.iter().position(|&c| c == 0).unwrap_or(0);
        let name = std::ffi::OsString::from_wide(&guid[..len]);
        let mut device = name.to_string_lossy().into_owned();
        // A volume is opened without the trailing backslash.
        while device.ends_with('\\') {
            device.pop();
        }
        Ok(device)
    }

    /// Read and parse the whole MFT of the NTFS volume holding `path`.
    /// Returns the device path (the per-volume cache key) and the data. The
    /// parsed volume is held in the heap (see [`read_volume_in`]).
    pub fn read_volume(path: &Path) -> Result<(String, Volume), String> {
        read_volume_in(path, None)
    }

    /// Why a read failed, and whether the temporary files it used were the
    /// cause (then the read may be tried again in the heap).
    struct ReadFailure {
        message: String,
        spill: bool,
    }

    impl ReadFailure {
        /// A failure of the read's temporary files (or of finishing what they
        /// hold). A volume too large for the heap fails for good, as a build
        /// failure the service retries later (see [`super::build_failed`]); a
        /// small one may be read again in the heap. With no files in use it is
        /// an ordinary failure.
        fn files(message: String, files_used: bool, large: bool) -> ReadFailure {
            match (files_used, large) {
                (false, _) => message.into(),
                (true, true) => ReadFailure {
                    message: if message.starts_with(crate::BUILD_FAILED) {
                        message
                    } else {
                        super::build_failed(message)
                    },
                    spill: false,
                },
                (true, false) => ReadFailure {
                    message,
                    spill: true,
                },
            }
        }
    }

    impl From<String> for ReadFailure {
        fn from(message: String) -> ReadFailure {
            ReadFailure {
                message,
                spill: false,
            }
        }
    }

    impl From<&str> for ReadFailure {
        fn from(message: &str) -> ReadFailure {
            message.to_string().into()
        }
    }

    /// [`read_volume`], keeping the parsed volume's tables in anonymous
    /// temporary files in `spill` when there is a directory (ADR 0007, step
    /// 9): the record table, the names and the grouping by parent are
    /// written through memory maps, so the read's private memory is a fixed
    /// working set (the read buffers and one parsed chunk), not 100 bytes or
    /// more per entry.
    ///
    /// The disk is checked first: a volume whose temporary files would not fit
    /// beside the index, or whose file cannot be made or fails during the
    /// read, is **not** read into the heap when it has more than
    /// [`crate::HEAP_FALLBACK_ENTRIES`] records; the read stops at once with an
    /// error that starts with [`crate::BUILD_FAILED`], which the scan turns
    /// into a fatal error the service retries after a back-off. A smaller
    /// volume is read in the heap instead (counted in
    /// [`ferrow_index::store::scratch_in_heap`], `spill_note` says why). Either
    /// way a returned volume is complete.
    pub fn read_volume_in(path: &Path, spill: Option<&Path>) -> Result<(String, Volume), String> {
        match read_volume_once(path, spill) {
            Err(failed) if failed.spill && spill.is_some() => {
                ferrow_index::store::count_heap_fallback();
                let (device, mut volume) = read_volume_once(path, None).map_err(|f| f.message)?;
                volume.spill_note = Some(format!(
                    "the read with temporary files failed ({}); the volume was read again in memory",
                    failed.message
                ));
                Ok((device, volume))
            }
            other => other.map_err(|f| f.message),
        }
    }

    fn read_volume_once(
        path: &Path,
        spill: Option<&Path>,
    ) -> Result<(String, Volume), ReadFailure> {
        use windows_sys::Win32::System::IO::DeviceIoControl;
        use windows_sys::Win32::System::Ioctl::{
            FSCTL_GET_NTFS_VOLUME_DATA, NTFS_VOLUME_DATA_BUFFER,
        };
        let device = ntfs_volume_device(path)?;
        let volume = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(7) // read | write | delete: never block other programs
            .custom_flags(NO_BUFFERING)
            .open(&device)
            .map_err(|e| format!("open {device}: {e}"))?;
        // SAFETY: zeroed POD output buffer.
        let mut data: NTFS_VOLUME_DATA_BUFFER = unsafe { std::mem::zeroed() };
        let mut returned = 0u32;
        // SAFETY: live volume handle; output pointer and size describe `data`.
        let ok = unsafe {
            DeviceIoControl(
                volume.as_raw_handle(),
                FSCTL_GET_NTFS_VOLUME_DATA,
                std::ptr::null(),
                0,
                (&raw mut data).cast(),
                size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
                &raw mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(format!("volume data: {}", std::io::Error::last_os_error()).into());
        }
        let cluster = data.BytesPerCluster as u64;
        let record_size = data.BytesPerFileRecordSegment as usize;
        let valid = data.MftValidDataLength as u64;
        if cluster == 0
            || record_size < 512
            || !record_size.is_multiple_of(512)
            || !CHUNK.is_multiple_of(record_size)
            || !(CHUNK as u64).is_multiple_of(cluster)
            || !cluster.is_multiple_of(512)
        {
            return Err("unexpected NTFS geometry".into());
        }
        let mut first = Aligned::new(record_size.max(cluster as usize).next_multiple_of(ALIGN));
        volume
            .seek_read(first.as_mut_slice(), data.MftStartLcn as u64 * cluster)
            .map_err(|e| format!("read MFT record 0: {e}"))?;
        let mut record0 = first.as_mut_slice()[..record_size].to_vec();
        drop(first);
        if !apply_fixup(&mut record0) {
            return Err("MFT record 0 is damaged".into());
        }
        let runs = mft_runs(&record0).ok_or("cannot decode the MFT's location")?;
        let background = crate::priority::is_background();
        if background {
            low_io_priority(&volume);
        }
        let journal_before = query_journal(&volume);
        let read_started = std::time::SystemTime::now();
        let records = valid / record_size as u64;
        let serial = data.VolumeSerialNumber as u64;

        let large = records > crate::HEAP_FALLBACK_ENTRIES as u64;
        // Made before any of the MFT is read: the room check and the files'
        // creation fail a large volume here, not after minutes of reading.
        let all = match spill {
            Some(dir) => {
                VolumeBuilder::with_spill(records, dir, &super::SpillPolicy::for_dir(dir))?
            }
            None => VolumeBuilder::new(records),
        };

        // Reader thread: sequential reads; this thread parses in parallel.
        let mut builder = std::thread::scope(|scope| -> Result<VolumeBuilder, ReadFailure> {
            let (tx, rx) = std::sync::mpsc::sync_channel::<(u64, Aligned)>(2);
            let volume = &volume;
            let reader = scope.spawn(move || -> Result<(), String> {
                if background {
                    crate::priority::enter_background();
                }
                let mut mft_offset = 0u64;
                for (lcn, clusters) in runs {
                    let run_bytes = clusters * cluster;
                    let mut done = 0u64;
                    while done < run_bytes && mft_offset + done < valid {
                        // Whole clusters keep every read sector-aligned.
                        let want = (run_bytes - done)
                            .min(CHUNK as u64)
                            .min((valid - mft_offset - done).next_multiple_of(cluster))
                            as usize;
                        let first_record = (mft_offset + done) / record_size as u64;
                        let mut buf = Aligned::new(want);
                        if let Some(lcn) = lcn {
                            let disk = lcn * cluster + done;
                            volume
                                .seek_read(buf.as_mut_slice(), disk)
                                .map_err(|e| format!("read MFT at {disk}: {e}"))?;
                        }
                        if tx.send((first_record, buf)).is_err() {
                            return Ok(());
                        }
                        done += want as u64;
                    }
                    mft_offset += run_bytes;
                    if mft_offset >= valid {
                        break;
                    }
                }
                Ok(())
            });
            let mut all = all;
            for (first_record, mut buf) in rx {
                let part = buf
                    .as_mut_slice()
                    .par_chunks_mut(record_size)
                    .enumerate()
                    .fold(Parsed::default, |mut acc, (i, rec)| {
                        let number = first_record + i as u64;
                        if number < records && apply_fixup(rec) {
                            parse_record(rec, number, &mut acc);
                        }
                        acc
                    })
                    .reduce(Parsed::default, |mut a, b| {
                        a.append(b);
                        a
                    });
                all.push(part);
                // A names file that failed on a volume too large for the heap:
                // stop reading (dropping the channel ends the reader).
                if let Some(why) = all.failure() {
                    return Err(ReadFailure::files(why, spill.is_some(), large));
                }
            }
            reader
                .join()
                .map_err(|_| "MFT reader panicked".to_string())??;
            Ok(all)
        })?;
        let mut warning = None;
        let journal = match journal_before {
            Some(j) => {
                let (next, note) =
                    refresh_recent(&volume, j, read_started, record_size, &mut builder)?;
                warning = note;
                Some((j.0, next))
            }
            None => None,
        };
        let mut out = builder
            .finish_checked(serial)
            .map_err(|message| ReadFailure::files(message, spill.is_some(), large))?;
        out.journal = journal;
        out.warning = warning;
        out.record_size = record_size;
        Ok((device, out))
    }

    /// The volume's change journal position: `(journal id, first usn, next usn)`.
    pub fn query_journal(volume: &std::fs::File) -> Option<(u64, i64, i64)> {
        use windows_sys::Win32::System::IO::DeviceIoControl;
        use windows_sys::Win32::System::Ioctl::{FSCTL_QUERY_USN_JOURNAL, USN_JOURNAL_DATA_V0};
        // SAFETY: zeroed POD output buffer.
        let mut data: USN_JOURNAL_DATA_V0 = unsafe { std::mem::zeroed() };
        let mut returned = 0u32;
        // SAFETY: live volume handle; output pointer and size describe `data`.
        let ok = unsafe {
            DeviceIoControl(
                volume.as_raw_handle(),
                FSCTL_QUERY_USN_JOURNAL,
                std::ptr::null(),
                0,
                (&raw mut data).cast(),
                size_of::<USN_JOURNAL_DATA_V0>() as u32,
                &raw mut returned,
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some((data.UsnJournalID, data.FirstUsn, data.NextUsn))
    }

    /// The most a volume's change journal keeps (bytes of records) before
    /// Windows drops its oldest records: its configured maximum size.
    pub fn journal_capacity(volume: &std::fs::File) -> Option<u64> {
        use windows_sys::Win32::System::IO::DeviceIoControl;
        use windows_sys::Win32::System::Ioctl::{FSCTL_QUERY_USN_JOURNAL, USN_JOURNAL_DATA_V0};
        // SAFETY: zeroed POD output buffer.
        let mut data: USN_JOURNAL_DATA_V0 = unsafe { std::mem::zeroed() };
        let mut returned = 0u32;
        // SAFETY: live volume handle; output pointer and size describe `data`.
        let ok = unsafe {
            DeviceIoControl(
                volume.as_raw_handle(),
                FSCTL_QUERY_USN_JOURNAL,
                std::ptr::null(),
                0,
                (&raw mut data).cast(),
                size_of::<USN_JOURNAL_DATA_V0>() as u32,
                &raw mut returned,
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(data.MaximumSize)
    }

    /// Whether this process may read the NTFS volume holding `path`
    /// directly (administrator or SYSTEM): a cheap probe, no MFT read.
    pub fn can_read_volume(path: &Path) -> bool {
        ntfs_volume_device(path).is_ok_and(|device| open_volume(&device).is_ok())
    }

    /// Open a volume device (`\\?\Volume{…}`) for journal and record reads.
    pub fn open_volume(device: &str) -> std::io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(7)
            .open(device)
    }

    /// Block until the journal has records after `start` (no polling: the
    /// thread sleeps in the kernel), then return them and the next
    /// position. Use a handle of its own: a blocked call holds the handle.
    pub fn wait_journal(
        volume: &std::fs::File,
        journal: u64,
        start: i64,
    ) -> std::io::Result<(i64, Vec<UsnRecord>)> {
        use windows_sys::Win32::System::IO::DeviceIoControl;
        use windows_sys::Win32::System::Ioctl::{FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V1};
        let mut buf = vec![0u64; 64 * 1024 / 8];
        let input = READ_USN_JOURNAL_DATA_V1 {
            StartUsn: start,
            ReasonMask: 0xFFFF_FFFF,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: 1,
            UsnJournalID: journal,
            MinMajorVersion: 2,
            MaxMajorVersion: 2,
        };
        let mut returned = 0u32;
        // SAFETY: live volume handle; buffers live for the call, sizes match.
        let ok = unsafe {
            DeviceIoControl(
                volume.as_raw_handle(),
                FSCTL_READ_USN_JOURNAL,
                (&raw const input).cast(),
                size_of::<READ_USN_JOURNAL_DATA_V1>() as u32,
                buf.as_mut_ptr().cast(),
                (buf.len() * 8) as u32,
                &raw mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: the buffer holds `returned` initialized bytes.
        let bytes =
            unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), returned as usize) };
        let Some(next) = bytes
            .get(0..8)
            .map(|b| i64::from_le_bytes(b.try_into().unwrap()))
        else {
            return Err(std::io::Error::other("short change journal read"));
        };
        Ok((next, parse_usn_records(&bytes[8..])))
    }

    /// A record's current info and non-DOS names, read through the file
    /// system; `None` when it is not in use (deleted).
    pub fn fetch_state(
        volume: &std::fs::File,
        record: u64,
        size: usize,
    ) -> Option<(super::RecordInfo, super::Names)> {
        let mut rec = fetch_record(volume, record, size)?;
        let mut copy = rec.clone();
        if apply_fixup(&mut copy) {
            rec = copy;
        }
        if rec.get(0..4) != Some(b"FILE") {
            return None;
        }
        let mut parsed = Parsed::default();
        parse_record(&rec, record, &mut parsed);
        let (_, info) = parsed.infos.first().copied()?;
        let names = parsed
            .names
            .iter()
            .map(|n| {
                let off = n.name_offset as usize;
                (
                    n.parent_record,
                    parsed.arena[off..off + n.name_len as usize].to_vec(),
                )
            })
            .collect();
        Some((info, names))
    }

    /// Read journal records from `start` up to the journal's end without
    /// waiting. Returns the records and the next position to read.
    pub fn read_journal(
        volume: &std::fs::File,
        journal: u64,
        start: i64,
        mut each: impl FnMut(UsnRecord),
    ) -> std::io::Result<i64> {
        use windows_sys::Win32::System::IO::DeviceIoControl;
        use windows_sys::Win32::System::Ioctl::{FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V1};
        let mut next = start;
        let mut buf = vec![0u64; 64 * 1024 / 8];
        loop {
            let input = READ_USN_JOURNAL_DATA_V1 {
                StartUsn: next,
                ReasonMask: 0xFFFF_FFFF,
                ReturnOnlyOnClose: 0,
                Timeout: 0,
                BytesToWaitFor: 0,
                UsnJournalID: journal,
                MinMajorVersion: 2,
                MaxMajorVersion: 2,
            };
            let mut returned = 0u32;
            // SAFETY: live volume handle; input and output buffers are live
            // for the call and their sizes match.
            let ok = unsafe {
                DeviceIoControl(
                    volume.as_raw_handle(),
                    FSCTL_READ_USN_JOURNAL,
                    (&raw const input).cast(),
                    size_of::<READ_USN_JOURNAL_DATA_V1>() as u32,
                    buf.as_mut_ptr().cast(),
                    (buf.len() * 8) as u32,
                    &raw mut returned,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: the buffer holds `returned` initialized bytes.
            let bytes =
                unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), returned as usize) };
            let Some(after) = bytes
                .get(0..8)
                .map(|b| i64::from_le_bytes(b.try_into().unwrap()))
            else {
                return Err(std::io::Error::other("short change journal read"));
            };
            let records = parse_usn_records(&bytes[8..]);
            let empty = records.is_empty();
            for r in records {
                each(r);
            }
            if empty || after == next {
                return Ok(after);
            }
            next = after;
        }
    }

    /// Re-read one record through the file system (always current, unlike
    /// raw volume reads that can lag the cache by a few seconds).
    pub fn fetch_record(volume: &std::fs::File, record: u64, size: usize) -> Option<Vec<u8>> {
        read_record(volume, record, size).ok().flatten()
    }

    /// Whether the file system answers that `record` is not in use. A failed
    /// read is not an answer.
    pub fn record_is_free(volume: &std::fs::File, record: u64, size: usize) -> bool {
        matches!(read_record(volume, record, size), Ok(None))
    }

    /// Read one record: `Ok(None)` when it is not in use (the file system
    /// answers with the nearest lower used record instead), `Err` when the
    /// read itself failed.
    fn read_record(
        volume: &std::fs::File,
        record: u64,
        size: usize,
    ) -> std::io::Result<Option<Vec<u8>>> {
        use windows_sys::Win32::System::IO::DeviceIoControl;
        use windows_sys::Win32::System::Ioctl::{
            FSCTL_GET_NTFS_FILE_RECORD, NTFS_FILE_RECORD_INPUT_BUFFER,
        };
        let input = NTFS_FILE_RECORD_INPUT_BUFFER {
            FileReferenceNumber: record as i64,
        };
        let mut out = vec![0u64; (size + 16).div_ceil(8)];
        let mut returned = 0u32;
        // SAFETY: live volume handle; buffers live for the call, sizes match.
        let ok = unsafe {
            DeviceIoControl(
                volume.as_raw_handle(),
                FSCTL_GET_NTFS_FILE_RECORD,
                (&raw const input).cast(),
                size_of::<NTFS_FILE_RECORD_INPUT_BUFFER>() as u32,
                out.as_mut_ptr().cast(),
                (out.len() * 8) as u32,
                &raw mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if returned < 12 {
            return Err(std::io::Error::other("short file record answer"));
        }
        // SAFETY: `returned` bytes were written.
        let bytes =
            unsafe { std::slice::from_raw_parts(out.as_ptr().cast::<u8>(), returned as usize) };
        let short = || std::io::Error::other("short file record answer");
        let got = u64::from_le_bytes(bytes[0..8].try_into().map_err(|_| short())?)
            & 0x0000_FFFF_FFFF_FFFF;
        // An unused record yields the nearest lower used one instead.
        if got != record {
            return Ok(None);
        }
        let len = u32::from_le_bytes(bytes[8..12].try_into().map_err(|_| short())?) as usize;
        let rec = bytes.get(12..12 + len).ok_or_else(short)?.to_vec();
        Ok(Some(rec))
    }

    /// Records written in the last two minutes may not have reached the
    /// disk when the raw read ran; re-read them through the file system.
    /// Returns the journal position to continue from and, when the re-read
    /// of the journal failed but the MFT read was kept, a warning.
    pub(super) fn refresh_recent(
        volume: &std::fs::File,
        journal: (u64, i64, i64),
        read_started: std::time::SystemTime,
        record_size: usize,
        builder: &mut VolumeBuilder,
    ) -> Result<(i64, Option<String>), String> {
        let id = journal.0;
        let since = filetime_now(read_started) - 120 * 10_000_000;
        let mut changed = std::collections::BTreeSet::new();
        let reread = reread_journal(
            journal,
            |start| {
                read_journal(volume, id, start, |r| {
                    if r.timestamp >= since {
                        changed.insert(r.record);
                    }
                })
            },
            || query_journal(volume),
        )?;
        let (next, warning) = (reread.next, reread.warning);
        if changed.is_empty() {
            return Ok((next, warning));
        }
        let mut fresh = Parsed::default();
        for &record in &changed {
            if let Some(mut rec) = fetch_record(volume, record, record_size) {
                // The file system may return the record with its update
                // sequence already applied; only undo it when present.
                let mut copy = rec.clone();
                if apply_fixup(&mut copy) {
                    rec = copy;
                }
                if rec.get(0..4) == Some(b"FILE") {
                    parse_record(&rec, record, &mut fresh);
                }
            }
        }
        let refreshed_names: std::collections::HashSet<u64> =
            fresh.names.iter().map(|n| n.record).collect();
        let still_used: std::collections::HashSet<u64> =
            fresh.infos.iter().map(|(n, _)| *n).collect();
        // Names of deleted files go; names of refreshed files are replaced
        // (names kept in extension records survive a base-only refresh).
        builder.forget(
            &changed,
            |record| {
                !changed.contains(&record)
                    || (still_used.contains(&record) && !refreshed_names.contains(&record))
            },
            |record| still_used.contains(&record) || !changed.contains(&record),
        );
        builder.push(fresh);
        Ok((next, warning))
    }

    fn filetime_now(time: std::time::SystemTime) -> i64 {
        match time.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => super::FILETIME_UNIX_EPOCH + (d.as_nanos() / 100) as i64,
            Err(_) => super::FILETIME_UNIX_EPOCH,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a 1 KiB record with fixups applied the way NTFS writes it.
    pub fn record(number_flags: u16, sequence: u16, base: u64, attrs: &[Vec<u8>]) -> Vec<u8> {
        let mut r = vec![0u8; 1024];
        r[0..4].copy_from_slice(b"FILE");
        r[4..6].copy_from_slice(&48u16.to_le_bytes()); // usa offset
        r[6..8].copy_from_slice(&3u16.to_le_bytes()); // 1 + 2 strides
        r[16..18].copy_from_slice(&sequence.to_le_bytes());
        r[20..22].copy_from_slice(&56u16.to_le_bytes()); // first attribute
        r[22..24].copy_from_slice(&number_flags.to_le_bytes());
        r[32..40].copy_from_slice(&base.to_le_bytes());
        let mut at = 56;
        for a in attrs {
            r[at..at + a.len()].copy_from_slice(a);
            at += a.len();
        }
        r[at..at + 4].copy_from_slice(&ATTR_END.to_le_bytes());
        at += 8;
        r[24..28].copy_from_slice(&(at as u32).to_le_bytes());
        r[28..32].copy_from_slice(&1024u32.to_le_bytes());
        // Protect: move each stride's last two bytes into the array.
        let check = [0x34, 0x12];
        r[48..50].copy_from_slice(&check);
        for i in 1..3 {
            let end = i * 512;
            let (a, b) = (r[end - 2], r[end - 1]);
            r[48 + 2 * i] = a;
            r[48 + 2 * i + 1] = b;
            r[end - 2..end].copy_from_slice(&check);
        }
        r
    }

    fn resident(kind: u32, name_len: u8, value: &[u8]) -> Vec<u8> {
        let len = (24 + value.len()).div_ceil(8) * 8;
        let mut a = vec![0u8; len];
        a[0..4].copy_from_slice(&kind.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[9] = name_len;
        a[16..20].copy_from_slice(&(value.len() as u32).to_le_bytes());
        a[20..22].copy_from_slice(&24u16.to_le_bytes());
        a[24..24 + value.len()].copy_from_slice(value);
        a
    }

    pub fn standard_info(created: i64, modified: i64, accessed: i64, attrs: u32) -> Vec<u8> {
        let mut v = vec![0u8; 48];
        v[0..8].copy_from_slice(&created.to_le_bytes());
        v[8..16].copy_from_slice(&modified.to_le_bytes());
        v[24..32].copy_from_slice(&accessed.to_le_bytes());
        v[32..36].copy_from_slice(&attrs.to_le_bytes());
        resident(ATTR_STANDARD_INFORMATION, 0, &v)
    }

    pub fn file_name(parent: u64, parent_seq: u16, name: &str, namespace: u8) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut v = vec![0u8; 66 + units.len() * 2];
        v[0..8].copy_from_slice(&(parent | (parent_seq as u64) << 48).to_le_bytes());
        v[64] = units.len() as u8;
        v[65] = namespace;
        for (i, u) in units.iter().enumerate() {
            v[66 + 2 * i..68 + 2 * i].copy_from_slice(&u.to_le_bytes());
        }
        resident(ATTR_FILE_NAME, 0, &v)
    }

    pub fn data_resident(len: usize) -> Vec<u8> {
        resident(ATTR_DATA, 0, &vec![7u8; len])
    }

    pub fn data_nonresident(lowest_vcn: u64, size: u64, runs: &[u8]) -> Vec<u8> {
        let len = (64 + runs.len()).div_ceil(8) * 8;
        let mut a = vec![0u8; len];
        a[0..4].copy_from_slice(&ATTR_DATA.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[8] = 1;
        a[16..24].copy_from_slice(&lowest_vcn.to_le_bytes());
        a[32..34].copy_from_slice(&64u16.to_le_bytes());
        a[48..56].copy_from_slice(&size.to_le_bytes());
        a[64..64 + runs.len()].copy_from_slice(runs);
        a
    }

    pub fn reparse(tag: u32) -> Vec<u8> {
        let mut v = vec![0u8; 8];
        v[0..4].copy_from_slice(&tag.to_le_bytes());
        resident(ATTR_REPARSE_POINT, 0, &v)
    }

    fn parse(mut r: Vec<u8>, number: u64) -> Parsed {
        assert!(apply_fixup(&mut r));
        let mut out = Parsed::default();
        parse_record(&r, number, &mut out);
        out
    }

    const T: i64 = FILETIME_UNIX_EPOCH + 10_000_000 * 1_700_000_000;

    #[test]
    fn fixup_restores_stride_ends_and_rejects_torn_records() {
        let mut r = record(1, 1, 0, &[]);
        r[511] = 0xAA; // tear: the check value no longer matches
        assert!(!apply_fixup(&mut r));
        let mut ok = record(1, 1, 0, &[]);
        assert!(apply_fixup(&mut ok));
        assert_eq!(&ok[510..512], &[0, 0]);
        let mut bad = ok.clone();
        bad[0] = b'B';
        assert!(!apply_fixup(&mut bad));
    }

    #[test]
    fn file_record_yields_name_size_times_and_attributes() {
        let r = record(
            RECORD_IN_USE,
            3,
            0,
            &[
                standard_info(T, T + 10_000_000, T + 20_000_000, 0x20 | 0x2),
                file_name(40, 2, "Report.TXT", 1),
                file_name(40, 2, "REPORT~1.TXT", NAMESPACE_DOS),
                data_resident(123),
            ],
        );
        let p = parse(r, 77);
        assert_eq!(p.names.len(), 1, "DOS short names are not indexed");
        let n = p.names[0];
        assert_eq!((n.record, n.parent_record, n.parent_sequence), (77, 40, 2));
        assert_eq!(&p.arena[..n.name_len as usize], b"Report.TXT");
        let (number, info) = p.infos[0];
        assert_eq!(number, 77);
        assert_eq!(info.sequence, 3);
        assert!(!info.is_dir);
        assert_eq!(info.size, 123);
        assert_eq!(info.attributes, 0x22);
        let md = info.metadata(ColumnSet::ALL);
        assert_eq!(md.size, Some(123));
        assert_eq!(md.modified, Some(1_700_000_001 * 10_000_000));
        assert_eq!(md.created, Some(1_700_000_000 * 10_000_000));
    }

    #[test]
    fn directory_and_nonresident_and_reparse_records() {
        let dir = parse(
            record(
                RECORD_IN_USE | RECORD_IS_DIRECTORY,
                1,
                0,
                &[
                    standard_info(T, T, T, 0x1000_0000),
                    file_name(5, 5, "Docs", 3),
                ],
            ),
            20,
        );
        let info = dir.infos[0].1;
        assert!(info.is_dir);
        assert_eq!(
            info.attributes, FILE_ATTRIBUTE_DIRECTORY,
            "internal bits dropped"
        );
        assert_eq!(info.metadata(ColumnSet::SIZE).size, Some(0));

        let big = parse(
            record(
                1,
                1,
                0,
                &[
                    file_name(20, 1, "big.bin", 0),
                    data_nonresident(0, 5 << 30, &[0x11, 4, 9, 0]),
                ],
            ),
            21,
        );
        assert_eq!(big.infos[0].1.size, 5 << 30);
        assert_eq!(big.infos[0].1.attributes, FILE_ATTRIBUTE_NORMAL);

        let link = parse(
            record(
                RECORD_IN_USE | RECORD_IS_DIRECTORY,
                1,
                0,
                &[
                    standard_info(T, T, T, FILE_ATTRIBUTE_REPARSE_POINT),
                    file_name(20, 1, "jx", 1),
                    reparse(0xA000_0003),
                ],
            ),
            22,
        );
        assert!(link.infos[0].1.is_name_surrogate());
    }

    #[test]
    fn unused_records_and_extension_records() {
        assert!(
            parse(record(0, 1, 0, &[file_name(5, 5, "gone", 1)]), 30)
                .names
                .is_empty()
        );
        let ext = parse(
            record(
                1,
                1,
                31,
                &[
                    file_name(20, 1, "second link", 1),
                    data_nonresident(0, 999, &[0x11, 1, 2, 0]),
                ],
            ),
            45,
        );
        assert!(ext.infos.is_empty(), "extension records carry no own entry");
        assert_eq!(ext.names[0].record, 31, "names belong to the base record");
        assert_eq!(
            ext.patches,
            vec![Patch::Size {
                record: 31,
                size: 999
            }]
        );
        let later_extent = parse(
            record(1, 1, 31, &[data_nonresident(8, 1, &[0x11, 1, 2, 0])]),
            46,
        );
        assert!(
            later_extent.patches.is_empty(),
            "only the first extent has the size"
        );
    }

    #[test]
    fn run_lists_decode_relative_signed_offsets_and_sparse_runs() {
        // 0x21: 1 length byte, 2 offset bytes.
        let runs = [
            0x21, 0x10, 0x00, 0x01, 0x21, 0x08, 0x00, 0xFF, 0x01, 0x04, 0x00,
        ];
        assert_eq!(
            decode_runs(&runs),
            Some(vec![
                (Some(0x100), 0x10),
                (Some(0x100 - 0x100), 0x08),
                (None, 4)
            ])
        );
        assert_eq!(decode_runs(&[0x21, 1]), None, "truncated");
        assert_eq!(decode_runs(&[0x11, 1, 0x80, 0]), None, "negative cluster");
        let r0 = {
            let mut r = record(1, 1, 0, &[data_nonresident(0, 4096, &[0x11, 4, 9, 0])]);
            assert!(apply_fixup(&mut r));
            r
        };
        assert_eq!(mft_runs(&r0), Some(vec![(Some(9), 4)]));
    }

    #[test]
    fn usn_records_parse_version_2_and_stop_on_damage() {
        let mut one = vec![0u8; 64];
        one[0..4].copy_from_slice(&64u32.to_le_bytes());
        one[4..6].copy_from_slice(&2u16.to_le_bytes());
        one[8..16].copy_from_slice(&(77u64 | 9 << 48).to_le_bytes());
        one[24..32].copy_from_slice(&4096u64.to_le_bytes());
        one[32..40].copy_from_slice(&123u64.to_le_bytes());
        one[40..44].copy_from_slice(&0x100u32.to_le_bytes());
        let mut v3 = one.clone();
        v3[4..6].copy_from_slice(&3u16.to_le_bytes());
        let mut bytes = one.clone();
        bytes.extend_from_slice(&v3);
        bytes.extend_from_slice(&one);
        let records = parse_usn_records(&bytes);
        assert_eq!(records.len(), 2, "other versions are skipped");
        assert_eq!(
            records[0],
            UsnRecord {
                record: 77,
                usn: 4096,
                timestamp: 123,
                reason: 0x100
            }
        );
        let mut bad = one.clone();
        bad[0..4].copy_from_slice(&9999u32.to_le_bytes());
        assert!(parse_usn_records(&bad).is_empty());
    }

    #[test]
    fn volume_children_skip_metafiles_stale_parents_and_self_links() {
        let mut parsed = Parsed::default();
        let recs = [
            (
                5,
                record(
                    3,
                    5,
                    0,
                    &[standard_info(T, T, T, 0), file_name(5, 5, ".", 3)],
                ),
            ),
            (11, record(3, 11, 0, &[file_name(5, 5, "$Extend", 3)])),
            (20, record(3, 1, 0, &[file_name(5, 5, "Docs", 3)])),
            (
                21,
                record(
                    1,
                    1,
                    0,
                    &[
                        file_name(20, 1, "a.txt", 3),
                        file_name(5, 5, "a-link.txt", 1),
                    ],
                ),
            ),
            (22, record(1, 1, 0, &[file_name(20, 9, "stale.txt", 3)])),
        ];
        for (n, mut r) in recs {
            assert!(apply_fixup(&mut r));
            parse_record(&r, n, &mut parsed);
        }
        let v = Volume::from_parsed(parsed, 32, 0xABCD);
        let names = |r: u64| {
            v.children(r)
                .map(|(_, n)| String::from_utf8(n.to_vec()).unwrap())
                .collect::<Vec<_>>()
        };
        let mut root = names(ROOT_RECORD);
        root.sort();
        assert_eq!(root, ["Docs", "a-link.txt"], "no '.', no metafiles");
        assert_eq!(names(20), ["a.txt"], "stale parent references are dropped");
        assert!(names(99).is_empty());
    }

    /// Records 5 (root), 20 (Docs), 21..=60 (files, every fifth also linked in
    /// the root), as parse chunks of `per` names each, with an extension-record
    /// size patch for 22 and a reparse patch for 23.
    fn chunks(per: usize) -> Vec<Parsed> {
        let info = |is_dir: bool, size: u64| RecordInfo {
            in_use: true,
            is_dir,
            sequence: 1,
            attributes: if is_dir { 0x10 } else { 0x20 },
            size,
            ..RecordInfo::default()
        };
        type Row = (u64, RecordInfo, Vec<(u64, String)>);
        let mut rows: Vec<Row> = vec![
            (5, info(true, 0), vec![]),
            (20, info(true, 0), vec![(5, "Docs".into())]),
        ];
        for r in 21..=60u64 {
            let mut names = vec![(20, format!("fä{r}.txt"))];
            if r % 5 == 0 {
                names.push((5, format!("link{r}")));
            }
            rows.push((r, info(false, r), names));
        }
        let mut out = vec![Parsed::default()];
        for (record, info, names) in rows {
            if out.last().unwrap().names.len() >= per {
                out.push(Parsed::default());
            }
            let part = out.last_mut().unwrap();
            part.infos.push((record, info));
            for (parent, name) in names {
                let name_offset = part.arena.len() as u32;
                part.arena.extend_from_slice(name.as_bytes());
                part.names.push(NameRecord {
                    record,
                    parent_record: parent,
                    parent_sequence: 1,
                    name_offset,
                    name_len: name.len() as u16,
                });
            }
        }
        out[0].patches.push(Patch::Size {
            record: 22,
            size: 9999,
        });
        out.last_mut().unwrap().patches.push(Patch::Reparse {
            record: 23,
            tag: 0xA000_000C,
        });
        out
    }

    fn built(per: usize) -> Volume {
        let mut b = VolumeBuilder::new(64);
        for chunk in chunks(per) {
            b.push(chunk);
        }
        b.finish(7)
    }

    type Kids = Vec<(u64, Vec<u8>)>;

    fn listing(v: &Volume) -> Vec<(u64, Kids)> {
        (0..64)
            .map(|r| {
                let mut kids: Vec<_> = v.children(r).map(|(n, b)| (n.record, b.to_vec())).collect();
                kids.sort();
                (r, kids)
            })
            .collect()
    }

    #[test]
    fn a_volume_built_from_chunks_is_the_same_whatever_the_chunking() {
        let whole = built(usize::MAX);
        assert!(chunks(usize::MAX).len() == 1 && chunks(3).len() > 10);
        for per in [1, 3, 7, 50] {
            let v = built(per);
            assert_eq!(v.infos[..], whole.infos[..], "records, chunks of {per}");
            assert_eq!(listing(&v), listing(&whole), "children, chunks of {per}");
            assert_eq!(v.names.len(), whole.names.len());
            assert_eq!(v.arena.len(), whole.arena.len());
        }
        // What the chunks said: sizes, a patched size and reparse tag, links.
        assert_eq!(whole.info(21).unwrap().size, 21);
        assert_eq!(
            whole.info(22).unwrap().size,
            9999,
            "patch from an extension record"
        );
        assert_eq!(whole.info(23).unwrap().reparse_tag, 0xA000_000C);
        assert_eq!(whole.children(20).count(), 40);
        assert_eq!(whole.children(5).count(), 1 + 8, "Docs and eight links");
        assert!(whole.children(5).any(|(_, n)| n == b"link25"));
        assert!(whole.children(20).any(|(_, n)| n == "fä33.txt".as_bytes()));
        assert_eq!((whole.serial, whole.record_size), (7, 1024));
    }

    #[test]
    fn forgetting_a_record_lets_a_fresh_parse_replace_it() {
        let mut b = VolumeBuilder::new(64);
        for chunk in chunks(5) {
            b.push(chunk);
        }
        // Record 30 is deleted, 31 renamed, 25 (two names) is rewritten with one.
        let changed: std::collections::BTreeSet<u64> = [25, 30, 31].into();
        let mut fresh = Parsed::default();
        for (record, name) in [(31u64, "renamed.txt"), (25, "only.txt")] {
            let info = RecordInfo {
                in_use: true,
                attributes: 0x20,
                sequence: 1,
                ..RecordInfo::default()
            };
            fresh.infos.push((record, info));
            fresh.names.push(NameRecord {
                record,
                parent_record: 20,
                parent_sequence: 1,
                name_offset: fresh.arena.len() as u32,
                name_len: name.len() as u16,
            });
            fresh.arena.extend_from_slice(name.as_bytes());
        }
        let used: std::collections::HashSet<u64> = [31, 25].into();
        let renamed: std::collections::HashSet<u64> = [31, 25].into();
        b.forget(
            &changed,
            |r| !changed.contains(&r) || (used.contains(&r) && !renamed.contains(&r)),
            |r| used.contains(&r) || !changed.contains(&r),
        );
        b.push(fresh);
        let v = b.finish(1);
        let names = |dir| {
            let mut n: Vec<String> = v
                .children(dir)
                .map(|(_, n)| String::from_utf8(n.to_vec()).unwrap())
                .collect();
            n.sort();
            n
        };
        assert!(!names(20).iter().any(|n| n.contains("30")), "deleted");
        assert!(names(20).contains(&"renamed.txt".to_string()));
        assert!(!names(20).iter().any(|n| n.contains("31")), "old name gone");
        assert!(!names(5).contains(&"link25".to_string()), "old link gone");
        assert!(v.info(30).is_none() && v.info(31).is_some());
    }

    const MIB: i64 = 1 << 20;

    #[test]
    fn the_recent_window_starts_on_a_page_never_before_the_journal() {
        // A journal shorter than the window is read from its first record.
        assert_eq!(recent_window_start(0, 30 * MIB), 0);
        assert_eq!(recent_window_start(4096, 64 * MIB + 4096), 4096);
        // A longer journal starts a window back, rounded down to a page,
        // even when the next position is in the middle of a page.
        let before = 9_000 * MIB + 1234;
        let start = recent_window_start(100 * MIB + 8, before);
        assert_eq!(start % USN_PAGE, 0);
        assert!(start <= before - RECENT_WINDOW_BYTES);
        assert!(start > before - RECENT_WINDOW_BYTES - USN_PAGE);
        // A page-aligned position is kept as it is.
        assert_eq!(
            recent_window_start(0, 200 * MIB),
            200 * MIB - RECENT_WINDOW_BYTES
        );
        // The journal's first record wins when it lies after the aligned
        // position, aligned or not.
        let first = before - 10 * MIB + 77;
        assert_eq!(recent_window_start(first, before), first);
        // Nothing underflows near the start of a big counter.
        assert_eq!(recent_window_start(0, 5), 0);
        assert_eq!(recent_window_start(0, i64::MIN / 2), 0);
    }

    fn os(code: i32) -> std::io::Error {
        std::io::Error::from_raw_os_error(code)
    }

    /// A journal that only accepts record starts: its first record and the
    /// starts of 4 KiB pages (as the real one does).
    fn strict_journal(first: i64, next: i64) -> impl FnMut(i64) -> std::io::Result<i64> {
        move |start| {
            if start == first || (start % USN_PAGE == 0 && (first..next).contains(&start)) {
                Ok(next)
            } else {
                Err(os(ERROR_INVALID_PARAMETER))
            }
        }
    }

    #[test]
    fn a_big_journal_is_re_read_from_an_aligned_window_in_one_try() {
        // 18 GiB journal, next position inside a page: the old unaligned
        // start made the first read fail with error 87.
        let (first, before) = (1_000 * MIB + 16, 18_000 * MIB + 3_333);
        let mut starts = Vec::new();
        let mut read = strict_journal(first, before + 500);
        let out = reread_journal(
            (7, first, before),
            |s| {
                starts.push(s);
                read(s)
            },
            || panic!("no query needed"),
        )
        .unwrap();
        assert_eq!(starts.len(), 1, "{starts:?}");
        assert_eq!(starts[0] % USN_PAGE, 0);
        assert_eq!(
            out,
            Reread {
                next: before + 500,
                warning: None
            }
        );
    }

    #[test]
    fn a_refused_start_is_retried_once_from_the_first_record() {
        let (first, before) = (10 * MIB + 8, 200 * MIB + 5);
        let mut starts = Vec::new();
        let out = reread_journal(
            (7, first, before),
            |s| {
                starts.push(s);
                if s == first {
                    Ok(before + 64)
                } else {
                    Err(os(ERROR_INVALID_PARAMETER))
                }
            },
            || panic!("no query needed"),
        )
        .unwrap();
        assert_eq!(starts.len(), 2);
        assert_eq!(starts[1], first);
        assert_eq!(out.next, before + 64);
        assert_eq!(out.warning, None);
    }

    #[test]
    fn a_journal_that_cannot_be_re_read_keeps_the_mft_and_replays_from_before() {
        let (first, before) = (10 * MIB + 8, 200 * MIB + 5);
        for code in [ERROR_INVALID_PARAMETER, 5, 1117] {
            let mut calls = 0;
            let out = reread_journal(
                (7, first, before),
                |_| {
                    calls += 1;
                    Err(os(code))
                },
                || Some((7, first, before + 4096)),
            )
            .unwrap();
            // The refused start is retried once, other errors are not.
            assert_eq!(
                calls,
                if code == ERROR_INVALID_PARAMETER {
                    2
                } else {
                    1
                }
            );
            assert_eq!(out.next, before, "replay every change since the read began");
            let warning = out.warning.expect("a warning for the service");
            assert!(
                warning
                    .starts_with("could not re-check the change journal after reading the MFT ("),
                "{warning}"
            );
            assert!(warning.contains(&os(code).to_string()), "{warning}");
            assert!(warning.contains("until the next scan"), "{warning}");
        }
    }

    #[test]
    fn a_short_journal_that_refuses_its_first_record_is_not_retried_in_a_loop() {
        let (first, before) = (0, 20 * MIB);
        let mut calls = 0;
        let out = reread_journal(
            (7, first, before),
            |_| {
                calls += 1;
                Err(os(ERROR_INVALID_PARAMETER))
            },
            || Some((7, first, before)),
        )
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(out.next, before);
        assert!(out.warning.is_some());
    }

    #[test]
    fn a_big_journal_that_refuses_the_aligned_start_is_not_read_whole() {
        // 18 GiB journal: a retry from its first record would read all of it.
        let (first, before) = (1_000 * MIB, 18_000 * MIB + 3_333);
        let mut starts = Vec::new();
        let out = reread_journal(
            (7, first, before),
            |s| {
                starts.push(s);
                Err(os(ERROR_INVALID_PARAMETER))
            },
            || Some((7, first, before)),
        )
        .unwrap();
        assert_eq!(starts.len(), 1, "{starts:?}");
        assert_ne!(starts[0], first);
        assert_eq!(out.next, before);
        assert!(out.warning.is_some());
    }

    #[test]
    fn a_failed_re_read_of_a_gone_replaced_or_purged_journal_is_an_error() {
        let (first, before) = (10 * MIB, 200 * MIB + 5);
        let failing = |_: i64| Err(os(1117));
        let err = reread_journal((7, first, before), failing, || None).unwrap_err();
        assert!(err.contains("no longer available"), "{err}");
        let err = reread_journal((7, first, before), failing, || Some((8, 0, before))).unwrap_err();
        assert!(err.contains("replaced"), "{err}");
        let err = reread_journal((7, first, before), failing, || {
            Some((7, before + 4096, before + 8192))
        })
        .unwrap_err();
        assert!(err.contains("purged"), "{err}");
    }

    #[test]
    fn a_purged_start_continues_from_the_journals_new_beginning() {
        let (first, before) = (0, 500 * MIB + 5);
        let new_first = 480 * MIB;
        let mut starts = Vec::new();
        let out = reread_journal(
            (7, first, before),
            |s| {
                starts.push(s);
                if s < new_first {
                    Err(os(ERROR_JOURNAL_ENTRY_DELETED))
                } else {
                    Ok(before + 1)
                }
            },
            || Some((7, new_first, before)),
        )
        .unwrap();
        assert_eq!(*starts.last().unwrap(), new_first);
        assert_eq!(out.next, before + 1);
        assert_eq!(out.warning, None);
    }

    #[test]
    fn a_purge_that_cannot_be_followed_or_a_replaced_journal_is_an_error() {
        let (first, before) = (0, 500 * MIB + 5);
        let purged = |_: i64| Err(os(ERROR_JOURNAL_ENTRY_DELETED));
        // A different journal id: the position means nothing any more.
        let err = reread_journal((7, first, before), purged, || Some((8, 400 * MIB, before)))
            .unwrap_err();
        assert!(err.starts_with("read change journal: "), "{err}");
        // The journal's start did not move, or it cannot be queried.
        assert!(reread_journal((7, first, before), purged, || Some((7, first, before))).is_err());
        assert!(reread_journal((7, first, before), purged, || None).is_err());
        // A journal purged faster than it is read gives up after three tries.
        let mut now_first = 450 * MIB;
        let mut queries = 0;
        let result = reread_journal((7, first, before), purged, || {
            queries += 1;
            now_first += MIB;
            Some((7, now_first, before))
        });
        assert!(result.is_err());
        assert_eq!(queries, 4);
    }

    /// A pseudo-random volume as parse chunks of `per` records each: folders
    /// and files under random folders (with record-number gaps), some files
    /// with a second name in another folder (a hard link), some names that
    /// arrive in a later chunk than their record (an extension record's), and
    /// size and reparse patches. Returns the volume's record count and the
    /// chunks. Deterministic.
    pub(crate) fn synthetic(n: usize, per: usize) -> (u64, Vec<Parsed>) {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let info = |is_dir: bool, size: u64, modified: i64| RecordInfo {
            in_use: true,
            is_dir,
            sequence: 1 + (size % 3) as u16,
            attributes: if is_dir { 0x10 } else { 0x20 },
            modified,
            size,
            ..RecordInfo::default()
        };
        // (record, sequence) of every folder created so far.
        let mut dirs: Vec<(u64, u16)> = vec![(ROOT_RECORD, 1)];
        let mut files: Vec<u64> = Vec::new();
        let mut out = vec![Parsed::default()];
        let mut record = FIRST_USER_RECORD;
        let add_name = |part: &mut Parsed, record: u64, parent: (u64, u16), name: &str| {
            let name_offset = part.arena.len() as u32;
            part.arena.extend_from_slice(name.as_bytes());
            part.names.push(NameRecord {
                record,
                parent_record: parent.0,
                parent_sequence: parent.1,
                name_offset,
                name_len: name.len() as u16,
            });
        };
        let mut root = info(true, 0, 0);
        root.sequence = 1;
        out[0].infos.push((ROOT_RECORD, root));
        for i in 0..n {
            record += 1 + next() % 3;
            if out.last().unwrap().infos.len() >= per {
                out.push(Parsed::default());
            }
            let part = out.last_mut().unwrap();
            let parent = dirs[(next() % dirs.len() as u64) as usize];
            let is_dir = next() % 7 == 0;
            let size = if is_dir { 0 } else { next() % 5_000_000 };
            let sequence_of = info(is_dir, size, 0).sequence;
            part.infos
                .push((record, info(is_dir, size, 1_000_000 + i as i64)));
            add_name(part, record, parent, &format!("n{i}-é{}", next() % 100));
            if is_dir {
                dirs.push((record, sequence_of));
            } else {
                files.push(record);
                if next() % 11 == 0 {
                    let other = dirs[(next() % dirs.len() as u64) as usize];
                    add_name(part, record, other, &format!("link{i}"));
                }
                if next() % 13 == 0 {
                    part.patches.push(Patch::Size {
                        record,
                        size: size + 1,
                    });
                }
            }
            if next() % 17 == 0 && !files.is_empty() {
                let owner = files[(next() % files.len() as u64) as usize];
                let other = dirs[(next() % dirs.len() as u64) as usize];
                add_name(part, owner, other, &format!("ext{i}"));
            }
            if next() % 29 == 0 && !files.is_empty() {
                let target = files[(next() % files.len() as u64) as usize];
                part.patches.push(Patch::Reparse {
                    record: target,
                    tag: 0xA000_000C,
                });
            }
        }
        (record + 40, out)
    }

    /// A scratch directory that is removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!(
                "ferrow-walk-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        /// Temporary files of a build still in the directory.
        fn leftovers(&self) -> Vec<String> {
            std::fs::read_dir(&self.0)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with(".ferrow-"))
                .collect()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The policy of a read with room to spare and the usual heap limit.
    fn roomy() -> SpillPolicy {
        SpillPolicy {
            heap_fallback_records: crate::HEAP_FALLBACK_ENTRIES as u64,
            free_bytes: None,
        }
    }

    /// A builder with files in `dir`, for a volume small enough for the heap
    /// to stand in.
    fn spilled(records: u64, dir: &Path) -> VolumeBuilder {
        VolumeBuilder::with_spill(records, dir, &roomy()).unwrap()
    }

    fn assert_same_tables(a: &Volume, b: &Volume, what: &str) {
        assert_eq!(a.infos[..], b.infos[..], "records, {what}");
        assert_eq!(a.names[..], b.names[..], "names, {what}");
        assert_eq!(a.arena[..], b.arena[..], "arena, {what}");
        assert_eq!(a.order[..], b.order[..], "grouping, {what}");
        assert_eq!(a.first[..], b.first[..], "starts, {what}");
    }

    #[test]
    fn a_volume_built_in_temporary_files_is_the_volume_built_in_the_heap() {
        let scratch = Scratch::new("volume");
        let (records, chunks) = synthetic(4_000, 97);
        assert!(chunks.len() > 30);
        for per in [97usize, 1_000, usize::MAX] {
            let (_, chunks) = synthetic(4_000, per);
            let mut heap = VolumeBuilder::new(records);
            let mut files = spilled(records, &scratch.0);
            for chunk in chunks {
                let copy = Parsed {
                    infos: chunk.infos.clone(),
                    names: chunk.names.clone(),
                    arena: chunk.arena.clone(),
                    patches: chunk.patches.clone(),
                };
                heap.push(copy);
                files.push(chunk);
            }
            assert!(files.file_bytes() > 0, "the tables went to files");
            let heap = heap.finish(5);
            let files = files.finish(5);
            assert_same_tables(&heap, &files, &format!("chunks of {per}"));
            assert_eq!(listing_all(&heap, records), listing_all(&files, records));
            assert_eq!(files.spill_note, None);
            // Nothing of the volume is in the heap; all of it is in files.
            assert!(files.heap_parts().iter().all(|(_, bytes)| *bytes == 0));
            assert!(files.mapped_bytes() > 0);
            assert_eq!(heap.mapped_bytes(), 0);
            // A wide volume really has folders with several children and links.
            assert!(heap.children(ROOT_RECORD).count() > 10);
        }
        drop(scratch);
    }

    fn listing_all(v: &Volume, records: u64) -> Vec<(u64, Kids)> {
        (0..records)
            .map(|r| {
                // Order matters here: the scan emits in this order.
                let kids: Vec<_> = v.children(r).map(|(n, b)| (n.record, b.to_vec())).collect();
                (r, kids)
            })
            .filter(|(_, kids)| !kids.is_empty())
            .collect()
    }

    #[test]
    fn forgetting_records_in_a_volume_built_in_files_matches_the_heap() {
        let scratch = Scratch::new("forget");
        let (records, chunks) = synthetic(1_500, 40);
        // The journal refresh forgets records after all chunks were read: a
        // deleted one, a renamed one (its extension names go with it) and a
        // rewritten one that keeps its extension-record names.
        let changed: std::collections::BTreeSet<u64> = chunks
            .iter()
            .flat_map(|c| c.infos.iter().map(|(r, _)| *r))
            .step_by(37)
            .collect();
        let renamed: std::collections::HashSet<u64> = changed.iter().copied().step_by(2).collect();
        let used: std::collections::HashSet<u64> = changed.iter().copied().skip(1).collect();
        let fresh = |record: u64| {
            let mut p = Parsed::default();
            p.infos.push((
                record,
                RecordInfo {
                    in_use: true,
                    sequence: 1,
                    attributes: 0x20,
                    size: 7,
                    ..RecordInfo::default()
                },
            ));
            p.names.push(NameRecord {
                record,
                parent_record: ROOT_RECORD,
                parent_sequence: 1,
                name_offset: 0,
                name_len: 5,
            });
            p.arena.extend_from_slice(b"fresh");
            p
        };
        let run = |mut b: VolumeBuilder| {
            for chunk in synthetic(1_500, 40).1 {
                b.push(chunk);
            }
            let refreshed: Vec<u64> = renamed
                .iter()
                .copied()
                .filter(|r| used.contains(r))
                .collect();
            b.forget(
                &changed,
                |r| !changed.contains(&r) || (used.contains(&r) && !renamed.contains(&r)),
                |r| used.contains(&r) || !changed.contains(&r),
            );
            for r in refreshed {
                b.push(fresh(r));
            }
            b.finish(9)
        };
        let heap = run(VolumeBuilder::new(records));
        let files = run(spilled(records, &scratch.0));
        assert_eq!(heap.infos[..], files.infos[..]);
        assert_eq!(
            listing_all(&heap, records),
            listing_all(&files, records),
            "the children of every folder, in order"
        );
        // The refresh did something: the same volume without it lists other
        // children.
        let mut untouched = VolumeBuilder::new(records);
        for chunk in synthetic(1_500, 40).1 {
            untouched.push(chunk);
        }
        let untouched = untouched.finish(9);
        assert_ne!(
            listing_all(&heap, records),
            listing_all(&untouched, records),
            "forgetting changed the volume"
        );
        drop(scratch);
    }

    #[test]
    fn a_builder_that_cannot_make_its_files_holds_the_same_volume_in_the_heap() {
        let scratch = Scratch::new("nofiles");
        let (records, chunks) = synthetic(800, 50);
        let missing = scratch.0.join("no").join("such").join("dir");
        let mut heap = VolumeBuilder::new(records);
        let mut files = spilled(records, &missing);
        let why = files.note().expect("a note says why").to_string();
        assert!(why.contains("kept in memory"), "{why}");
        for chunk in chunks {
            heap.push(Parsed {
                infos: chunk.infos.clone(),
                names: chunk.names.clone(),
                arena: chunk.arena.clone(),
                patches: chunk.patches.clone(),
            });
            files.push(chunk);
        }
        assert_eq!(files.file_bytes(), 0);
        let heap = heap.finish(1);
        let files = files.finish(1);
        assert_same_tables(&heap, &files, "no files");
        assert_eq!(files.mapped_bytes(), 0);
        assert!(files.spill_note.is_some());
        drop(scratch);
    }

    #[test]
    fn temporary_files_are_gone_after_success_failure_and_a_panic() {
        let scratch = Scratch::new("cleanup");
        let (records, chunks) = synthetic(1_000, 64);
        let feed = |b: &mut VolumeBuilder| {
            for c in synthetic(1_000, 64).1 {
                b.push(c);
            }
        };
        // Success: the volume holds the files; dropping it removes them.
        {
            let mut b = spilled(records, &scratch.0);
            feed(&mut b);
            let v = b.finish(1);
            assert!(v.mapped_bytes() > 0);
            drop(v);
        }
        assert_eq!(scratch.leftovers(), Vec::<String>::new(), "after success");
        // Failure: a builder dropped before it was finished.
        {
            let mut b = spilled(records, &scratch.0);
            feed(&mut b);
            assert!(b.file_bytes() > 0);
        }
        assert_eq!(scratch.leftovers(), Vec::<String>::new(), "after a drop");
        // Failure: names that cannot be finished (past the 32-bit offsets).
        {
            let mut b = spilled(records, &scratch.0);
            feed(&mut b);
            if let NameSink::Files { overflow, .. } = &mut b.names {
                *overflow = true;
            }
            let error = b.finish_checked(1).expect_err("an error");
            assert!(error.contains("4 GiB"), "{error}");
        }
        assert_eq!(scratch.leftovers(), Vec::<String>::new(), "after an error");
        // A panic while the builder is alive.
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut b = spilled(records, &scratch.0);
            feed(&mut b);
            assert!(b.file_bytes() > 0);
            panic!("the parse failed");
        }));
        assert!(panicked.is_err());
        assert_eq!(scratch.leftovers(), Vec::<String>::new(), "after a panic");
        let _ = chunks;
        drop(scratch);
    }

    /// Feed the synthetic volume's chunks to a builder.
    fn feed_all(b: &mut VolumeBuilder, count: usize, per: usize) {
        for chunk in synthetic(count, per).1 {
            b.push(chunk);
        }
    }

    #[test]
    fn a_large_volume_with_no_room_for_its_files_fails_with_build_failed_before_reading() {
        let scratch = Scratch::new("noroom");
        let records = 5_000;
        // Large: above the (injected) heap limit. 120 B/record would need
        // 600 kB; 1 kB is free.
        let large = SpillPolicy {
            heap_fallback_records: 1_000,
            free_bytes: Some(1_000),
        };
        let Err(error) = VolumeBuilder::with_spill(records, &scratch.0, &large) else {
            panic!("a large volume without room must not get a builder");
        };
        assert!(error.starts_with(crate::BUILD_FAILED), "{error}");
        assert!(error.contains("not read into memory instead"), "{error}");
        assert!(error.contains("MB free"), "{error}");
        assert_eq!(scratch.leftovers(), Vec::<String>::new(), "no file made");
        // The same volume with room gets its files.
        let roomy = SpillPolicy {
            free_bytes: Some(files_estimate(records) + 1),
            ..large
        };
        assert!(VolumeBuilder::with_spill(records, &scratch.0, &roomy).is_ok());
        // Unknown free space is not a refusal (the first file decides).
        let unknown = SpillPolicy {
            free_bytes: None,
            ..large
        };
        assert!(VolumeBuilder::with_spill(records, &scratch.0, &unknown).is_ok());
        // A small volume with no room is built in the heap, with a note.
        let small = SpillPolicy {
            heap_fallback_records: 10_000,
            free_bytes: Some(1_000),
        };
        let builder = VolumeBuilder::with_spill(records, &scratch.0, &small).unwrap();
        let note = builder.note().expect("a note says why");
        assert!(note.contains("read in memory"), "{note}");
        assert_eq!(builder.file_bytes(), 0);
        drop(scratch);
    }

    #[test]
    fn a_large_volume_whose_files_cannot_be_made_fails_and_a_small_one_uses_the_heap() {
        let scratch = Scratch::new("nodir");
        let missing = scratch.0.join("no").join("such").join("dir");
        let records = 5_000;
        let large = SpillPolicy {
            heap_fallback_records: 1_000,
            free_bytes: None,
        };
        let Err(error) = VolumeBuilder::with_spill(records, &missing, &large) else {
            panic!("no file, no builder for a large volume");
        };
        assert!(error.starts_with(crate::BUILD_FAILED), "{error}");
        let small = SpillPolicy {
            heap_fallback_records: 10_000,
            ..large
        };
        let builder = VolumeBuilder::with_spill(records, &missing, &small).unwrap();
        assert!(builder.note().is_some());
        drop(scratch);
    }

    #[test]
    fn a_names_file_that_fails_stops_a_large_volume_and_a_small_one_goes_on_in_the_heap() {
        let scratch = Scratch::new("failing");
        let (records, _) = synthetic(1_500, 40);
        let fail = |b: &mut VolumeBuilder| {
            let NameSink::Files { names, .. } = &mut b.names else {
                panic!("names in files");
            };
            names.fail(std::io::Error::other("disk full"));
        };
        // Large: the failure is known at once, nothing is read back into the
        // heap, and the volume cannot be finished.
        let large = SpillPolicy {
            heap_fallback_records: 100,
            free_bytes: None,
        };
        let mut b = VolumeBuilder::with_spill(records, &scratch.0, &large).unwrap();
        assert_eq!(b.failure(), None);
        for chunk in synthetic(1_500, 40).1.into_iter().take(10) {
            b.push(chunk);
        }
        fail(&mut b);
        let why = b.failure().expect("the failure is reported at once");
        assert!(why.contains("disk full"), "{why}");
        let NameSink::Files { names, .. } = &b.names else {
            unreachable!()
        };
        assert_eq!(names.heap_bytes(), 0, "not read back into memory");
        assert!(b.finish_checked(1).is_err());
        // Small: the names continue in the heap and the volume is complete.
        let mut heap = VolumeBuilder::new(records);
        feed_all(&mut heap, 1_500, 40);
        let mut b = spilled(records, &scratch.0);
        let mut chunks = synthetic(1_500, 40).1.into_iter();
        for chunk in chunks.by_ref().take(10) {
            b.push(chunk);
        }
        fail(&mut b);
        assert_eq!(b.failure(), None, "a small volume can go on");
        for chunk in chunks {
            b.push(chunk);
        }
        let files = b.finish(1);
        assert_same_tables(&heap.finish(1), &files, "names failed midway");
        assert!(files.spill_note.is_some());
        drop(scratch);
    }

    #[test]
    fn forgotten_names_are_found_by_record_and_cut_off() {
        let mut forgotten = Forgotten::new();
        assert!(forgotten.is_empty() && !forgotten.kills(0, 0) && !forgotten.kills(u64::MAX, 0));
        forgotten.add(10, [50, 20, 30]);
        // A later call: record 20 again with a higher cut-off, and a new one.
        forgotten.add(25, [20, 90]);
        assert!(!forgotten.is_empty());
        // Before the cut-off of its call: dead. At or after it: alive (a
        // fresh name pushed after the forget).
        assert!(forgotten.kills(30, 9) && !forgotten.kills(30, 10));
        assert!(
            forgotten.kills(20, 24) && !forgotten.kills(20, 25),
            "highest cut"
        );
        assert!(forgotten.kills(90, 0) && !forgotten.kills(90, 25));
        // Records between and outside the listed ones are never dead.
        for record in [0, 19, 21, 29, 31, 49, 51, 89, 91, u64::MAX] {
            assert!(!forgotten.kills(record, 0), "{record}");
        }
        // A record number beyond the bitmap is still found (and its neighbours not).
        let far = FORGOTTEN_BITMAP_RECORDS + 7;
        forgotten.add(5, [far]);
        assert!(
            forgotten.kills(far, 4) && !forgotten.kills(far, 5) && !forgotten.kills(far + 1, 0)
        );
        assert!(forgotten.kills(30, 9), "the others are kept");
    }

    #[test]
    fn the_forgotten_lookup_agrees_with_the_set_per_call_rule_it_replaced() {
        // The rule before: a name is dead when some forget call with a cut-off
        // above its index listed its record.
        let mut calls: Vec<(usize, std::collections::HashSet<u64>)> = Vec::new();
        let mut forgotten = Forgotten::new();
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut cut = 0usize;
        for _ in 0..6 {
            cut += (next() % 200) as usize;
            let records: Vec<u64> = (0..(next() % 40)).map(|_| 16 + next() % 300).collect();
            calls.push((cut, records.iter().copied().collect()));
            forgotten.add(cut, records);
        }
        for index in 0..cut + 50 {
            for record in 0..340u64 {
                let old = calls
                    .iter()
                    .any(|(cut, dead)| index < *cut && dead.contains(&record));
                assert_eq!(forgotten.kills(record, index), old, "{record} at {index}");
            }
        }
    }

    /// Prints what the forgotten check costs per name, against the hash-set
    /// rule it replaced: `cargo test --release -p ferrow-walk
    /// forgotten_lookup_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement"]
    fn forgotten_lookup_cost() {
        let names: u64 = 4_000_000;
        let spread = |i: u64| (i.wrapping_mul(2_654_435_761) % 30_000_000) + 16;
        for forgotten_records in [0u64, 20_000, 400_000] {
            let records: Vec<u64> = (0..forgotten_records).map(|i| spread(i * 7)).collect();
            let mut new = Forgotten::new();
            let mut old: Vec<(usize, std::collections::HashSet<u64>)> = Vec::new();
            if forgotten_records > 0 {
                new.add(names as usize, records.iter().copied());
                old.push((names as usize, records.iter().copied().collect()));
            }
            let time = |what: &str, check: &dyn Fn(u64, usize) -> bool| {
                let started = std::time::Instant::now();
                let mut dead = 0u64;
                for i in 0..names {
                    dead += u64::from(check(spread(i), i as usize));
                }
                let ns = started.elapsed().as_secs_f64() * 1e9 / names as f64;
                println!(
                    "forgotten={forgotten_records:>7} {what}: {ns:6.2} ns per name ({dead} dead)"
                );
                dead
            };
            let a = time("bitmap + search", &|r, i| {
                !new.is_empty() && new.kills(r, i)
            });
            let b = time("hash set      ", &|r, i| {
                old.iter().any(|(cut, dead)| i < *cut && dead.contains(&r))
            });
            assert_eq!(a, b);
        }
    }
}
