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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NameRecord {
    /// The file's base record number.
    pub record: u64,
    pub parent_record: u64,
    pub parent_sequence: u16,
    /// Stored name bytes (WTF-8) in the owning arena.
    pub name_offset: u32,
    pub name_len: u16,
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

/// The whole volume's MFT contents, indexed for subtree emission.
#[derive(Debug, Default)]
pub struct Volume {
    /// Indexed by record number.
    pub infos: Vec<RecordInfo>,
    pub names: Vec<NameRecord>,
    pub arena: Vec<u8>,
    /// `names` indices grouped by parent record: the children of record
    /// `r` are `order[first[r]..first[r + 1]]`.
    order: Vec<u32>,
    first: Vec<u32>,
    pub serial: u64,
    /// Change journal id and the position after the read, when the
    /// volume has a journal (the starting point for live updates).
    pub journal: Option<(u64, i64)>,
    /// Bytes per MFT record.
    pub record_size: usize,
}

impl Volume {
    pub fn from_parsed(parsed: Parsed, records: u64, serial: u64) -> Volume {
        let records = records as usize;
        let mut infos = vec![RecordInfo::default(); records];
        for (n, info) in parsed.infos {
            if let Some(slot) = infos.get_mut(n as usize) {
                *slot = info;
            }
        }
        for patch in parsed.patches {
            match patch {
                Patch::Size { record, size } => {
                    if let Some(info) = infos.get_mut(record as usize) {
                        info.size = size;
                    }
                }
                Patch::Reparse { record, tag } => {
                    if let Some(info) = infos.get_mut(record as usize) {
                        info.reparse_tag = tag;
                    }
                }
            }
        }
        let names = parsed.names;
        let mut first = vec![0u32; records + 1];
        for n in &names {
            if (n.parent_record as usize) < records {
                first[n.parent_record as usize + 1] += 1;
            }
        }
        for i in 1..first.len() {
            first[i] += first[i - 1];
        }
        let mut fill = first.clone();
        let mut order = vec![0u32; first[records] as usize];
        for (i, n) in names.iter().enumerate() {
            let p = n.parent_record as usize;
            if p < records {
                order[fill[p] as usize] = i as u32;
                fill[p] += 1;
            }
        }
        Volume {
            infos,
            names: names.into_iter().collect(),
            arena: parsed.arena,
            order,
            first,
            serial,
            journal: None,
            record_size: 1024,
        }
    }

    pub fn info(&self, record: u64) -> Option<&RecordInfo> {
        self.infos.get(record as usize).filter(|i| i.in_use)
    }

    /// The current names directly inside directory `record`: entries
    /// whose stored parent reference still matches the directory's
    /// sequence (a stale reference belongs to a deleted directory).
    pub fn children(&self, record: u64) -> impl Iterator<Item = (&NameRecord, &[u8])> + '_ {
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
            if n.parent_sequence != sequence
                || n.record == record
                || n.record < FIRST_USER_RECORD
                || self.info(n.record).is_none()
            {
                return None;
            }
            let off = n.name_offset as usize;
            Some((n, &self.arena[off..off + n.name_len as usize]))
        })
    }
}

#[cfg(windows)]
pub use platform::{
    can_read_volume, fetch_record, fetch_state, open_volume, query_journal, read_journal,
    read_volume, wait_journal,
};

#[cfg(windows)]
mod platform {
    use super::{
        Parsed, Patch, UsnRecord, Volume, apply_fixup, mft_runs, parse_record, parse_usn_records,
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
    /// Returns the device path (the per-volume cache key) and the data.
    pub fn read_volume(path: &Path) -> Result<(String, Volume), String> {
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
            return Err(format!("volume data: {}", std::io::Error::last_os_error()));
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
        let journal_before = query_journal(&volume);
        let read_started = std::time::SystemTime::now();
        let records = valid / record_size as u64;
        let serial = data.VolumeSerialNumber as u64;

        // Reader thread: sequential reads; this thread parses in parallel.
        let parsed = std::thread::scope(|scope| -> Result<Parsed, String> {
            let (tx, rx) = std::sync::mpsc::sync_channel::<(u64, Aligned)>(2);
            let volume = &volume;
            let reader = scope.spawn(move || -> Result<(), String> {
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
            let mut all = Parsed::default();
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
                all.append(part);
            }
            reader
                .join()
                .map_err(|_| "MFT reader panicked".to_string())??;
            Ok(all)
        })?;
        let mut parsed = parsed;
        let journal = match journal_before {
            Some(j) => {
                let next = refresh_recent(&volume, j, read_started, record_size, &mut parsed)?;
                Some((j.0, next))
            }
            None => None,
        };
        let mut out = Volume::from_parsed(parsed, records, serial);
        out.journal = journal;
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
    ) -> Result<i64, String> {
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
                return Err(format!(
                    "read change journal: {}",
                    std::io::Error::last_os_error()
                ));
            }
            // SAFETY: the buffer holds `returned` initialized bytes.
            let bytes =
                unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), returned as usize) };
            let Some(after) = bytes
                .get(0..8)
                .map(|b| i64::from_le_bytes(b.try_into().unwrap()))
            else {
                return Err("short change journal read".into());
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
        if ok == 0 || returned < 12 {
            return None;
        }
        // SAFETY: `returned` bytes were written.
        let bytes =
            unsafe { std::slice::from_raw_parts(out.as_ptr().cast::<u8>(), returned as usize) };
        let got = u64::from_le_bytes(bytes[0..8].try_into().ok()?) & 0x0000_FFFF_FFFF_FFFF;
        // An unused record yields the nearest lower used one instead.
        if got != record {
            return None;
        }
        let len = u32::from_le_bytes(bytes[8..12].try_into().ok()?) as usize;
        let rec = bytes.get(12..12 + len)?.to_vec();
        Some(rec)
    }

    /// Records written in the last two minutes may not have reached the
    /// disk when the raw read ran; re-read them through the file system.
    pub(super) fn refresh_recent(
        volume: &std::fs::File,
        journal: (u64, i64, i64),
        read_started: std::time::SystemTime,
        record_size: usize,
        parsed: &mut Parsed,
    ) -> Result<i64, String> {
        const WINDOW_BYTES: i64 = 64 << 20;
        let (id, first, before) = journal;
        let since = filetime_now(read_started) - 120 * 10_000_000;
        let mut changed = std::collections::BTreeSet::new();
        let next = read_journal(volume, id, first.max(before - WINDOW_BYTES), |r| {
            if r.timestamp >= since {
                changed.insert(r.record);
            }
        })?;
        if changed.is_empty() {
            return Ok(next);
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
        parsed.infos.retain(|(n, _)| !changed.contains(n));
        parsed.patches.retain(|p| match p {
            Patch::Size { record, .. } | Patch::Reparse { record, .. } => {
                still_used.contains(record) || !changed.contains(record)
            }
        });
        // Names of deleted files go; names of refreshed files are replaced
        // (names kept in extension records survive a base-only refresh).
        parsed.names.retain(|n| {
            !changed.contains(&n.record)
                || (still_used.contains(&n.record) && !refreshed_names.contains(&n.record))
        });
        parsed.append(fresh);
        Ok(next)
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
}
