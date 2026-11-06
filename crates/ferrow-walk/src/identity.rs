//! Filesystem identity of a file or directory, independent of the
//! spelling used to reach it.
//!
//! Two paths that the filesystem resolves to the same object — normal
//! vs verbatim (`\\?\`) spelling, differently cased components, a
//! symlinked or junctioned alias — yield the same [`FileId`]. Exclude
//! matching compares identities, never path strings (ADR 0001).

use std::path::Path;

/// Inspect the reparse point itself: the name-surrogate bit also covers
/// junctions and mount points, independently of Rust's symlink classification.
#[cfg(windows)]
pub fn is_name_surrogate(path: &Path) -> std::io::Result<bool> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FileAttributeTagInfo, GetFileInformationByHandleEx,
    };
    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    // SAFETY: zeroed FILE_ATTRIBUTE_TAG_INFO is a valid output buffer.
    let mut info: FILE_ATTRIBUTE_TAG_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: `file` owns the live handle throughout the query. The output
    // pointer and size describe `info` exactly and it outlives the call.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileAttributeTagInfo,
            (&raw mut info).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(info.ReparseTag & 0x2000_0000 != 0)
}

/// Identity of one filesystem object on one volume.
///
/// `kind` separates the namespaces the platform APIs report, so ids
/// from different queries are never compared across kinds: 0 = Unix
/// `(st_dev, st_ino)`, 1 = Windows 128-bit file id, 2 = Windows
/// fallback 64-bit file index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId {
    kind: u8,
    volume: u64,
    id: u128,
}

/// Identity of the object at `path`, following symlinks and junctions
/// to their target.
#[cfg(unix)]
pub fn file_id(path: &Path) -> std::io::Result<FileId> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(path)?;
    Ok(FileId {
        kind: 0,
        volume: md.dev(),
        id: md.ino() as u128,
    })
}

/// Identity of the object at `path`, following symlinks and junctions
/// to their target.
#[cfg(windows)]
pub fn file_id(path: &Path) -> std::io::Result<FileId> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    // encode_wide preserves lone surrogates, so WTF-8 names open fine.
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is NUL-terminated and outlives the call. Minimal
    // access (attributes only), full sharing, no security attributes
    // or template file. FILE_FLAG_BACKUP_SEMANTICS allows opening
    // directories.
    let handle: HANDLE = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `handle` was just opened successfully and is closed
    // exactly once below.
    let result = unsafe { file_id_from_handle(handle) };
    // SAFETY: closing the handle opened above, exactly once.
    unsafe {
        CloseHandle(handle);
    }
    result
}

#[cfg(windows)]
unsafe fn file_id_from_handle(
    handle: windows_sys::Win32::Foundation::HANDLE,
) -> std::io::Result<FileId> {
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ID_INFO, FileIdInfo, GetFileInformationByHandle,
        GetFileInformationByHandleEx,
    };

    // SAFETY: zeroed FILE_ID_INFO is a valid output buffer; `handle`
    // is a live file handle owned by the caller.
    let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: the buffer pointer and size describe `info` exactly and
    // it outlives the call.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            (&raw mut info).cast::<std::ffi::c_void>(),
            u32::try_from(size_of::<FILE_ID_INFO>()).unwrap_or(u32::MAX),
        )
    };
    if ok != 0 {
        return Ok(FileId {
            kind: 1,
            volume: info.VolumeSerialNumber,
            id: u128::from_ne_bytes(info.FileId.Identifier),
        });
    }
    // Fall back for filesystems without 128-bit file ids.
    // SAFETY: zeroed BY_HANDLE_FILE_INFORMATION is a valid output
    // buffer; `handle` is a live file handle owned by the caller.
    let mut basic: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `basic` outlives the call and is only written through
    // the pointer.
    let ok = unsafe { GetFileInformationByHandle(handle, &raw mut basic) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(FileId {
        kind: 2,
        volume: basic.dwVolumeSerialNumber as u64,
        id: ((basic.nFileIndexHigh as u128) << 32) | basic.nFileIndexLow as u128,
    })
}

/// Identity of the object at `path`.
#[cfg(not(any(unix, windows)))]
pub fn file_id(_path: &Path) -> std::io::Result<FileId> {
    Err(std::io::ErrorKind::Unsupported.into())
}
