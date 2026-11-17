//! Enumerate one directory subtree of an NTFS volume by file reference
//! number, for the live updater (a folder moved into an indexed location).
//!
//! The directory is opened by its reference (`OpenFileById`, so a rename or
//! move meanwhile does not matter) and listed with
//! `GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)`, which returns
//! every child's reference number, attributes, times and size in one call
//! per 64 KiB buffer. Each entry becomes a [`RecordInfo`], so flags and
//! metadata are exactly what the MFT scan and the journal path produce
//! (name-surrogate reparse points stay leaves, hidden stays hidden).
//!
//! The walk is bounded (entries, time) and cancellable, runs on one thread
//! and, on a thread marked with [`crate::priority::enter_background`], asks
//! for low disk priority on every directory handle it opens.
//!
//! The walk itself ([`walk_by`]) and the buffer parser are plain code,
//! unit-tested on every platform with a mocked lister; only the handle calls
//! are Windows.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::ntfs::RecordInfo;

/// One entry found below the walked directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtreeEntry {
    /// The entry's MFT record number (48 bits, no sequence).
    pub record: u64,
    /// Record number of the directory it was listed in.
    pub parent: u64,
    /// Name bytes (WTF-8), as the index stores them.
    pub name: Vec<u8>,
    pub info: RecordInfo,
}

/// What a finished walk found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Subtree {
    /// Parents come before their children. A folder moved between two of the
    /// walk's listings appears once per listing (its children only once).
    pub entries: Vec<SubtreeEntry>,
    /// References of folders that could not be opened because they no longer
    /// exist as far as the open could tell. The caller checks each one: a
    /// folder that does exist makes the walk incomplete.
    pub skipped: Vec<u64>,
}

impl From<Vec<SubtreeEntry>> for Subtree {
    fn from(entries: Vec<SubtreeEntry>) -> Subtree {
        Subtree {
            entries,
            skipped: Vec::new(),
        }
    }
}

/// Why a subtree walk produced nothing usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalkError {
    /// The directory could not be opened and looks deleted (or its reference
    /// is stale). The caller confirms that before trusting it.
    Gone,
    /// More entries than [`Limits::max_entries`].
    OverBudget,
    /// Longer than [`Limits::max_time`].
    TimedOut,
    /// The caller cancelled it.
    Cancelled,
    /// Anything else, for the log.
    Io(String),
}

impl std::fmt::Display for WalkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalkError::Gone => f.write_str("the folder could not be opened"),
            WalkError::OverBudget => f.write_str("more entries than the walk budget"),
            WalkError::TimedOut => f.write_str("the walk took longer than its time budget"),
            WalkError::Cancelled => f.write_str("the walk was cancelled"),
            WalkError::Io(e) => f.write_str(e),
        }
    }
}

/// What a walk may cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_entries: usize,
    pub max_time: Duration,
}

/// A running walk's limits, start time and cancel flag.
pub struct Budget<'a> {
    pub limits: Limits,
    cancel: &'a AtomicBool,
    started: Instant,
}

impl<'a> Budget<'a> {
    pub fn new(limits: Limits, cancel: &'a AtomicBool) -> Budget<'a> {
        Budget {
            limits,
            cancel,
            started: Instant::now(),
        }
    }

    /// Cancelled or out of time.
    pub fn check(&self) -> Result<(), WalkError> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(WalkError::Cancelled)
        } else if self.started.elapsed() >= self.limits.max_time {
            Err(WalkError::TimedOut)
        } else {
            Ok(())
        }
    }
}

/// One child as the directory listing reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEntry {
    /// 64-bit file reference: record number and, in the top 16 bits, the
    /// record's sequence number.
    pub reference: u64,
    pub attributes: u32,
    pub created: i64,
    pub accessed: i64,
    pub modified: i64,
    pub size: u64,
    pub reparse_tag: u32,
    pub name: Vec<u8>,
}

impl RawEntry {
    pub fn record(&self) -> u64 {
        self.reference & RECORD_MASK
    }

    /// The entry as the MFT scan would have described it.
    pub fn info(&self) -> RecordInfo {
        RecordInfo {
            in_use: true,
            is_dir: self.attributes & ATTR_DIRECTORY != 0,
            sequence: (self.reference >> 48) as u16,
            attributes: self.attributes,
            created: self.created,
            modified: self.modified,
            accessed: self.accessed,
            size: self.size,
            reparse_tag: if self.attributes & ATTR_REPARSE != 0 {
                self.reparse_tag
            } else {
                0
            },
        }
    }
}

const RECORD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
const ATTR_DIRECTORY: u32 = 0x10;
const ATTR_REPARSE: u32 = 0x400;
/// Offset of `FileName` in `FILE_ID_EXTD_DIR_INFO`.
const NAME_AT: usize = 88;

/// Parse one buffer of `FILE_ID_EXTD_DIR_INFO` records, skipping `.` and
/// `..`. `None` when the buffer is malformed (a record or name runs past
/// its end).
pub fn parse_dir_buffer(buf: &[u8]) -> Option<Vec<RawEntry>> {
    let u32_at = |o: usize| Some(u32::from_le_bytes(buf.get(o..o + 4)?.try_into().ok()?));
    let i64_at = |o: usize| Some(i64::from_le_bytes(buf.get(o..o + 8)?.try_into().ok()?));
    let mut out = Vec::new();
    let mut at = 0usize;
    loop {
        let next = u32_at(at)? as usize;
        let name_len = u32_at(at + 60)? as usize;
        let units = buf.get(at + NAME_AT..at + NAME_AT + name_len)?;
        let wide: Vec<u16> = units
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        if wide != [0x2e] && wide != [0x2e, 0x2e] {
            let mut name = Vec::with_capacity(name_len / 2);
            ferrow_index::osname::encode_wtf8(&wide, &mut name);
            out.push(RawEntry {
                reference: u64::from_le_bytes(buf.get(at + 72..at + 80)?.try_into().ok()?),
                attributes: u32_at(at + 56)?,
                created: i64_at(at + 8)?,
                accessed: i64_at(at + 16)?,
                modified: i64_at(at + 24)?,
                size: i64_at(at + 40)?.max(0) as u64,
                reparse_tag: u32_at(at + 68)?,
                name,
            });
        }
        if next == 0 {
            return Some(out);
        }
        at = at.checked_add(next)?;
    }
}

/// Walk the subtree below the directory with reference `root`, breadth
/// first, calling `list(reference, budget, out)` to list each directory:
/// it appends the directory's entries to `out` and answers whether the
/// directory could be opened (`false`: gone). It checks the budget between
/// buffers of a big directory itself.
///
/// A directory is entered at most once, so one that moved between two
/// listings (and so shows up under both parents) does not list its children
/// twice; it is still reported under both of its names, in listing order.
/// Records in `excluded` are left out with everything below them.
pub fn walk_by(
    root: u64,
    excluded: &HashSet<u64>,
    budget: &Budget<'_>,
    mut list: impl FnMut(u64, &Budget<'_>, &mut Vec<RawEntry>) -> Result<bool, WalkError>,
) -> Result<Subtree, WalkError> {
    let mut out = Subtree::default();
    let mut entered: HashSet<u64> = HashSet::from([root & RECORD_MASK]);
    let mut queue: VecDeque<u64> = VecDeque::from([root]);
    let mut first = true;
    while let Some(reference) = queue.pop_front() {
        budget.check()?;
        let mut raw = Vec::new();
        if !list(reference, budget, &mut raw)? {
            if first {
                return Err(WalkError::Gone);
            }
            out.skipped.push(reference);
            continue;
        }
        first = false;
        let parent = reference & RECORD_MASK;
        for entry in raw {
            let record = entry.record();
            if excluded.contains(&record) {
                continue;
            }
            let info = entry.info();
            if info.is_descended_dir() && entered.insert(record) {
                queue.push_back(entry.reference);
            }
            out.entries.push(SubtreeEntry {
                record,
                parent,
                name: entry.name,
                info,
            });
            if out.entries.len() > budget.limits.max_entries {
                return Err(WalkError::OverBudget);
            }
        }
    }
    Ok(out)
}

#[cfg(windows)]
pub use platform::{file_reference, walk_subtree};

#[cfg(windows)]
mod platform {
    use super::{Budget, Limits, RawEntry, Subtree, WalkError, parse_dir_buffer, walk_by};
    use std::collections::HashSet;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_DESCRIPTOR, FILE_ID_DESCRIPTOR_0, FILE_LIST_DIRECTORY,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdType,
        GetFileInformationByHandleEx, OpenFileById, SYNCHRONIZE,
    };

    // FILE_INFO_BY_HANDLE_CLASS values (winbase.h).
    const FILE_ID_EXTD_DIRECTORY_INFO: i32 = 19;
    const FILE_ID_EXTD_DIRECTORY_RESTART_INFO: i32 = 20;
    const ERROR_NO_MORE_FILES: i32 = 18;
    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const ERROR_PATH_NOT_FOUND: i32 = 3;
    /// What opening a deleted folder's reference returns on NTFS.
    const ERROR_INVALID_PARAMETER: i32 = 87;
    /// Bytes per listing call.
    const BUFFER: usize = 64 * 1024;

    fn open_by_reference(hint: &std::fs::File, reference: u64) -> std::io::Result<std::fs::File> {
        let descriptor = FILE_ID_DESCRIPTOR {
            dwSize: size_of::<FILE_ID_DESCRIPTOR>() as u32,
            Type: FileIdType,
            Anonymous: FILE_ID_DESCRIPTOR_0 {
                FileId: reference as i64,
            },
        };
        // SAFETY: `hint` is a live handle on the volume; the descriptor and
        // its size are valid for the call; the returned handle is owned by
        // the `File` built from it (checked against INVALID_HANDLE_VALUE).
        unsafe {
            let handle = OpenFileById(
                hint.as_raw_handle(),
                &descriptor,
                FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                FILE_FLAG_BACKUP_SEMANTICS,
            );
            if handle == INVALID_HANDLE_VALUE {
                return Err(std::io::Error::last_os_error());
            }
            Ok(std::fs::File::from_raw_handle(handle))
        }
    }

    /// An open failure that means "no such folder (any more)". The caller
    /// confirms it against the file system before trusting it.
    fn vanished(e: &std::io::Error) -> bool {
        matches!(
            e.raw_os_error(),
            Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND | ERROR_INVALID_PARAMETER)
        )
    }

    /// The 64-bit file reference (record number and sequence) of `path`.
    /// `None` when the volume uses 128-bit ids (ReFS).
    pub fn file_reference(path: &Path) -> std::io::Result<Option<u64>> {
        use windows_sys::Win32::Storage::FileSystem::{FILE_ID_INFO, FileIdInfo};
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        // SAFETY: zeroed POD output buffer; live handle; size matches.
        let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                (&raw mut info).cast(),
                size_of::<FILE_ID_INFO>() as u32,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let id = info.FileId.Identifier;
        if id[8..].iter().any(|&b| b != 0) {
            return Ok(None);
        }
        Ok(Some(u64::from_le_bytes(id[..8].try_into().unwrap())))
    }

    /// List one directory by reference into `out`; `false` when it cannot be
    /// opened because it looks gone.
    fn list_directory(
        hint: &std::fs::File,
        reference: u64,
        budget: &Budget<'_>,
        out: &mut Vec<RawEntry>,
        buf: &mut [u64],
    ) -> Result<bool, WalkError> {
        let dir = match open_by_reference(hint, reference) {
            Ok(dir) => dir,
            Err(e) if vanished(&e) => return Ok(false),
            Err(e) => return Err(WalkError::Io(format!("open folder: {e}"))),
        };
        if crate::priority::is_background() {
            crate::ntfs::low_io_priority(&dir);
        }
        let mut class = FILE_ID_EXTD_DIRECTORY_RESTART_INFO;
        loop {
            budget.check()?;
            // SAFETY: live handle; the buffer is writable for BUFFER bytes.
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    dir.as_raw_handle(),
                    class,
                    buf.as_mut_ptr().cast(),
                    BUFFER as u32,
                )
            };
            class = FILE_ID_EXTD_DIRECTORY_INFO;
            if ok == 0 {
                let e = std::io::Error::last_os_error();
                // The end of the listing; a folder deleted meanwhile ends it
                // too (the journal reports the deletion).
                if e.raw_os_error() == Some(ERROR_NO_MORE_FILES) || vanished(&e) {
                    return Ok(true);
                }
                return Err(WalkError::Io(format!("list folder: {e}")));
            }
            // SAFETY: the buffer is `BUFFER` initialized bytes.
            let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), BUFFER) };
            let entries = parse_dir_buffer(bytes)
                .ok_or_else(|| WalkError::Io("malformed folder listing".into()))?;
            out.extend(entries);
            // A huge folder must not be held in memory before the budget
            // is looked at.
            if out.len() > budget.limits.max_entries {
                return Err(WalkError::OverBudget);
            }
        }
    }

    /// List every entry below the directory with file reference `root`
    /// (record number plus sequence), parents before their children.
    /// `hint` is any handle on the volume (the volume handle itself).
    /// Records in `excluded` are left out with everything below them.
    pub fn walk_subtree(
        hint: &std::fs::File,
        root: u64,
        excluded: &HashSet<u64>,
        limits: Limits,
        cancel: &AtomicBool,
    ) -> Result<Subtree, WalkError> {
        let budget = Budget::new(limits, cancel);
        let mut buf = vec![0u64; BUFFER / 8];
        walk_by(root, excluded, &budget, |reference, budget, out| {
            list_directory(hint, reference, budget, out, &mut buf)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// One `FILE_ID_EXTD_DIR_INFO` record.
    fn record(next: u32, reference: u64, attrs: u32, size: i64, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut r = vec![0u8; NAME_AT + units.len() * 2];
        r[0..4].copy_from_slice(&next.to_le_bytes());
        r[8..16].copy_from_slice(&11i64.to_le_bytes());
        r[16..24].copy_from_slice(&22i64.to_le_bytes());
        r[24..32].copy_from_slice(&33i64.to_le_bytes());
        r[40..48].copy_from_slice(&size.to_le_bytes());
        r[56..60].copy_from_slice(&attrs.to_le_bytes());
        r[60..64].copy_from_slice(&((units.len() * 2) as u32).to_le_bytes());
        r[68..72].copy_from_slice(&0xA000_000Cu32.to_le_bytes());
        r[72..80].copy_from_slice(&reference.to_le_bytes());
        for (i, u) in units.iter().enumerate() {
            r[NAME_AT + i * 2..NAME_AT + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        r
    }

    /// Records laid out the way the file system does: each padded to 8
    /// bytes, `NextEntryOffset` the padded length, 0 on the last.
    fn buffer(entries: &[(u64, u32, i64, &str)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, (reference, attrs, size, name)) in entries.iter().enumerate() {
            let mut r = record(0, *reference, *attrs, *size, name);
            r.resize(r.len().next_multiple_of(8), 0);
            if i + 1 < entries.len() {
                let len = r.len() as u32;
                r[0..4].copy_from_slice(&len.to_le_bytes());
            }
            out.extend(r);
        }
        out.resize(out.len() + 64, 0); // the unused tail of a real buffer
        out
    }

    #[test]
    fn a_listing_becomes_entries_and_skips_dot_entries() {
        let seq = 7u64 << 48;
        let buf = buffer(&[
            (seq | 5, 0x10, 0, "."),
            (seq | 5, 0x10, 0, ".."),
            (seq | 100, 0x10, 0, "sub"),
            (seq | 101, 0x20 | 0x2, 1234, "Ünï.txt"),
            (seq | 102, 0x10 | 0x400, 0, "junction"),
        ]);
        let entries = parse_dir_buffer(&buf).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.clone()).collect();
        assert_eq!(
            names,
            [b"sub".to_vec(), "Ünï.txt".into(), b"junction".to_vec()]
        );
        assert_eq!(entries[0].record(), 100);
        let dir = entries[0].info();
        assert!(dir.is_descended_dir() && dir.sequence == 7);
        let file = entries[1].info();
        assert_eq!((file.size, file.is_dir), (1234, false));
        assert_ne!(file.entry_flags() & ferrow_index::flags::HIDDEN, 0);
        assert_eq!((file.created, file.accessed, file.modified), (11, 22, 33));
        assert_eq!(file.reparse_tag, 0, "a tag only counts on a reparse point");
        // A junction (name-surrogate reparse point) is a leaf, like the MFT scan.
        let junction = entries[2].info();
        assert!(!junction.is_descended_dir());
        assert_ne!(junction.entry_flags() & ferrow_index::flags::SYMLINK, 0);
    }

    #[test]
    fn a_truncated_listing_is_refused() {
        let buf = buffer(&[(100, 0x10, 0, "sub")]);
        assert!(parse_dir_buffer(&buf[..60]).is_none());
        // A name length that runs past the buffer.
        let mut bad = buf.clone();
        bad[60..64].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse_dir_buffer(&bad).is_none());
    }

    fn raw(reference: u64, attributes: u32, name: &str) -> RawEntry {
        RawEntry {
            reference,
            attributes,
            created: 0,
            accessed: 0,
            modified: 0,
            size: 1,
            reparse_tag: 0,
            name: name.as_bytes().to_vec(),
        }
    }

    const DIR: u32 = 0x10;
    const FILE: u32 = 0x20;
    const LIMITS: Limits = Limits {
        max_entries: 1000,
        max_time: Duration::from_secs(30),
    };

    /// A mocked file system: directory reference → its listing. A directory
    /// that is not in the map cannot be opened.
    fn walk(
        fs: &HashMap<u64, Vec<RawEntry>>,
        root: u64,
        excluded: &HashSet<u64>,
        limits: Limits,
        cancel: &AtomicBool,
    ) -> (Result<Subtree, WalkError>, Vec<u64>) {
        let budget = Budget::new(limits, cancel);
        let mut listed = Vec::new();
        let result = walk_by(root, excluded, &budget, |reference, _, out| {
            listed.push(reference);
            match fs.get(&reference) {
                Some(entries) => {
                    out.extend(entries.iter().cloned());
                    Ok(true)
                }
                None => Ok(false),
            }
        });
        (result, listed)
    }

    /// 1 {a(100){f(110)}, b(101){g(111)}, h(102)}
    fn tree() -> HashMap<u64, Vec<RawEntry>> {
        HashMap::from([
            (
                1,
                vec![raw(100, DIR, "a"), raw(101, DIR, "b"), raw(102, FILE, "h")],
            ),
            (100, vec![raw(110, FILE, "f")]),
            (101, vec![raw(111, FILE, "g")]),
        ])
    }

    fn names(s: &Subtree) -> Vec<String> {
        s.entries
            .iter()
            .map(|e| String::from_utf8(e.name.clone()).unwrap())
            .collect()
    }

    #[test]
    fn the_walk_lists_parents_first_and_leaves_excluded_records_out() {
        let none = AtomicBool::new(false);
        let (r, _) = walk(&tree(), 1, &HashSet::new(), LIMITS, &none);
        let s = r.unwrap();
        assert_eq!(names(&s), ["a", "b", "h", "f", "g"]);
        assert!(s.skipped.is_empty());
        assert_eq!(s.entries[3].parent, 100);
        // An excluded folder goes with everything below it.
        let (r, listed) = walk(&tree(), 1, &HashSet::from([100]), LIMITS, &none);
        assert_eq!(names(&r.unwrap()), ["b", "h", "g"]);
        assert!(!listed.contains(&100));
    }

    #[test]
    fn a_folder_that_moved_between_two_listings_is_entered_once() {
        // b lists a again (a moved from the root into b after the root was
        // listed): a's children must not be listed twice.
        let mut fs = tree();
        fs.get_mut(&101).unwrap().push(raw(100, DIR, "a-moved"));
        let none = AtomicBool::new(false);
        let (r, listed) = walk(&fs, 1, &HashSet::new(), LIMITS, &none);
        let s = r.unwrap();
        assert_eq!(
            listed.iter().filter(|&&d| d == 100).count(),
            1,
            "{listed:?}"
        );
        assert_eq!(names(&s).iter().filter(|n| *n == "f").count(), 1);
        // The folder itself is reported under both names, in listing order.
        let a: Vec<_> = s.entries.iter().filter(|e| e.record == 100).collect();
        assert_eq!(a.len(), 2);
        assert_eq!((a[0].parent, a[1].parent), (1, 101));
        // The walked folder itself is never entered again either.
        let mut fs = tree();
        fs.get_mut(&100).unwrap().push(raw(1, DIR, "loop"));
        let (r, listed) = walk(&fs, 1, &HashSet::new(), LIMITS, &none);
        r.unwrap();
        assert_eq!(listed.iter().filter(|&&d| d == 1).count(), 1);
    }

    #[test]
    fn budget_time_and_cancel_stop_the_walk() {
        let none = AtomicBool::new(false);
        let small = Limits {
            max_entries: 3,
            ..LIMITS
        };
        assert_eq!(
            walk(&tree(), 1, &HashSet::new(), small, &none)
                .0
                .unwrap_err(),
            WalkError::OverBudget
        );
        let late = Limits {
            max_time: Duration::ZERO,
            ..LIMITS
        };
        assert_eq!(
            walk(&tree(), 1, &HashSet::new(), late, &none)
                .0
                .unwrap_err(),
            WalkError::TimedOut
        );
        let cancelled = AtomicBool::new(true);
        assert_eq!(
            walk(&tree(), 1, &HashSet::new(), LIMITS, &cancelled)
                .0
                .unwrap_err(),
            WalkError::Cancelled
        );
    }

    #[test]
    fn an_unopenable_root_is_gone_and_an_unopenable_subfolder_is_reported() {
        let none = AtomicBool::new(false);
        let (r, _) = walk(&HashMap::new(), 1, &HashSet::new(), LIMITS, &none);
        assert_eq!(r.unwrap_err(), WalkError::Gone);
        // b cannot be opened: the rest is listed and b is handed back.
        let mut fs = tree();
        fs.remove(&101);
        let s = walk(&fs, 1, &HashSet::new(), LIMITS, &none).0.unwrap();
        assert_eq!(names(&s), ["a", "b", "h", "f"]);
        assert_eq!(s.skipped, [101]);
    }

    #[cfg(windows)]
    mod windows {
        use super::super::*;
        use std::collections::HashSet;
        use std::os::windows::fs::OpenOptionsExt;
        use std::sync::atomic::AtomicBool;

        #[test]
        fn the_struct_layout_matches_the_parser() {
            use windows_sys::Win32::Storage::FileSystem::FILE_ID_EXTD_DIR_INFO;
            assert_eq!(
                std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileName),
                NAME_AT
            );
            assert_eq!(
                std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileAttributes),
                56
            );
            assert_eq!(
                std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileNameLength),
                60
            );
            assert_eq!(
                std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, ReparsePointTag),
                68
            );
            assert_eq!(std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileId), 72);
            assert_eq!(std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, EndOfFile), 40);
        }

        /// root/{a.txt, sub/{b.txt, deep/c.txt}, skip/d.txt}; the hint is a
        /// handle on root (a volume handle needs administrator rights).
        fn tree(name: &str) -> Option<(std::path::PathBuf, std::fs::File, u64)> {
            let root =
                std::env::temp_dir().join(format!("ferrow-subtree-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("sub/deep")).unwrap();
            std::fs::create_dir_all(root.join("skip")).unwrap();
            std::fs::write(root.join("a.txt"), b"12345").unwrap();
            std::fs::write(root.join("sub/b.txt"), b"1").unwrap();
            std::fs::write(root.join("sub/deep/c.txt"), b"").unwrap();
            std::fs::write(root.join("skip/d.txt"), b"1").unwrap();
            let Some(reference) = file_reference(&root).unwrap() else {
                eprintln!("skipped: the temp folder uses 128-bit file ids");
                return None;
            };
            let hint = std::fs::OpenOptions::new()
                .access_mode(0x80) // FILE_READ_ATTRIBUTES
                .share_mode(7)
                .custom_flags(0x0200_0000) // FILE_FLAG_BACKUP_SEMANTICS
                .open(&root)
                .unwrap();
            Some((root, hint, reference))
        }

        #[test]
        fn walks_a_real_tree_by_reference() {
            let Some((root, hint, reference)) = tree("walk") else {
                return;
            };
            let none = HashSet::new();
            let cancel = AtomicBool::new(false);
            let entries = walk_subtree(&hint, reference, &none, super::LIMITS, &cancel)
                .unwrap()
                .entries;
            let name = |e: &SubtreeEntry| String::from_utf8(e.name.clone()).unwrap();
            let mut names: Vec<_> = entries.iter().map(name).collect();
            names.sort();
            assert_eq!(
                names,
                ["a.txt", "b.txt", "c.txt", "d.txt", "deep", "skip", "sub"]
            );
            let by = |n: &str| entries.iter().find(|e| name(e) == n).unwrap();
            // The references are the real ones, so the journal's records match.
            let sub = file_reference(&root.join("sub")).unwrap().unwrap();
            assert_eq!(by("sub").record, sub & RECORD_MASK);
            assert_eq!(by("b.txt").parent, sub & RECORD_MASK);
            assert_eq!(by("a.txt").parent, reference & RECORD_MASK);
            assert_eq!(by("a.txt").info.size, 5);
            assert!(by("sub").info.is_descended_dir());
            // Parents come before their children.
            let at = |n: &str| entries.iter().position(|e| name(e) == n).unwrap();
            assert!(at("sub") < at("b.txt") && at("deep") < at("c.txt"));

            // An excluded record is left out with everything below it.
            let skip = by("skip").record;
            let excluded = HashSet::from([skip]);
            let entries = walk_subtree(&hint, reference, &excluded, super::LIMITS, &cancel)
                .unwrap()
                .entries;
            let names: Vec<_> = entries.iter().map(name).collect();
            assert!(
                !names.iter().any(|n| n == "skip" || n == "d.txt"),
                "{names:?}"
            );
            assert_eq!(names.len(), 5);
            drop(hint);
            let _ = std::fs::remove_dir_all(root);
        }

        #[test]
        fn budget_cancel_and_a_vanished_folder_are_reported() {
            let Some((root, hint, reference)) = tree("limits") else {
                return;
            };
            let none = HashSet::new();
            let cancel = AtomicBool::new(false);
            let small = Limits {
                max_entries: 3,
                ..super::LIMITS
            };
            assert_eq!(
                walk_subtree(&hint, reference, &none, small, &cancel).unwrap_err(),
                WalkError::OverBudget
            );
            let cancelled = AtomicBool::new(true);
            assert_eq!(
                walk_subtree(&hint, reference, &none, super::LIMITS, &cancelled).unwrap_err(),
                WalkError::Cancelled
            );
            // A deleted folder: its reference no longer opens.
            let sub = file_reference(&root.join("sub")).unwrap().unwrap();
            std::fs::remove_dir_all(root.join("sub")).unwrap();
            assert_eq!(
                walk_subtree(&hint, sub, &none, super::LIMITS, &cancel).unwrap_err(),
                WalkError::Gone
            );
            drop(hint);
            let _ = std::fs::remove_dir_all(root);
        }
    }
}
