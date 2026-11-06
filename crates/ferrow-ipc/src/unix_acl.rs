//! Fail-closed ACL policy for privileged paths and IPC directories. Inspect
//! opened objects so permission checks refer to the same inode as mode checks.
use std::io;

#[derive(Clone, Copy)]
struct Entry {
    allow: bool,
    root: bool,
    changes: bool,
}

fn refused() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "untrusted extended ACL: protected objects require no entries; ancestors must grant no non-root write, delete or chmod rights",
    )
}

fn policy(entries: &[Entry], ancestor: bool) -> io::Result<()> {
    if !ancestor && !entries.is_empty()
        || entries
            .iter()
            .any(|entry| entry.allow && !entry.root && entry.changes)
    {
        return Err(refused());
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn posix_acl(bytes: &[u8], ancestor: bool, default_acl: bool) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    if bytes.len() < 4 || bytes[..4] != 2u32.to_le_bytes() || !(bytes.len() - 4).is_multiple_of(8) {
        return Err(refused());
    }
    let records: Vec<_> = bytes[4..]
        .as_chunks::<8>()
        .0
        .iter()
        .map(|b| {
            (
                u16::from_le_bytes([b[0], b[1]]),
                u16::from_le_bytes([b[2], b[3]]),
                u32::from_le_bytes(b[4..8].try_into().unwrap()),
            )
        })
        .collect();
    let extended = default_acl || records.iter().any(|(tag, _, _)| matches!(tag, 2 | 8 | 16));
    if !ancestor && extended {
        return Err(refused());
    }
    let mask = records
        .iter()
        .find(|(tag, _, _)| *tag == 16)
        .map_or(7, |(_, perm, _)| *perm);
    let mut entries = Vec::new();
    for (tag, perm, id) in records {
        if perm & !7 != 0 || !matches!(tag, 1 | 2 | 4 | 8 | 16 | 32) {
            return Err(refused());
        }
        entries.push(Entry {
            allow: true,
            root: tag == 1 || tag == 2 && id == 0 || tag == 16,
            changes: perm & if matches!(tag, 2 | 4 | 8) { mask } else { 7 } & 2 != 0,
        });
    }
    if ancestor {
        policy(&entries, true)
    } else {
        Ok(())
    }
}

/// Final objects reject extended ACLs, including inherited and default entries.
/// Ancestors permit read-only grants and root-only changes. Errors refuse use.
#[cfg(unix)]
pub fn validate(file: &std::fs::File, ancestor: bool) -> io::Result<()> {
    platform_validate(file, ancestor)
}

#[cfg(target_os = "linux")]
fn platform_validate(file: &std::fs::File, ancestor: bool) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    for (name, default_acl) in [
        (c"system.posix_acl_access", false),
        (c"system.posix_acl_default", true),
    ] {
        let mut bytes = vec![0u8; 65536];
        // SAFETY: live fd, NUL-terminated attribute name and initialized output buffer.
        let len = unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
            )
        };
        if len < 0 {
            let error = io::Error::last_os_error();
            if matches!(
                error.raw_os_error(),
                Some(libc::ENODATA) | Some(libc::ENOTSUP)
            ) {
                continue;
            }
            return Err(error);
        }
        bytes.truncate(len as usize);
        posix_acl(&bytes, ancestor, default_acl)?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn platform_validate(file: &std::fs::File, ancestor: bool) -> io::Result<()> {
    use std::ffi::c_void;
    use std::os::fd::AsRawFd;
    // Darwin sys/acl.h and membership.h. ACL objects/qualifiers are owned by
    // acl_free; entry pointers remain borrowed from the owning ACL.
    unsafe extern "C" {
        fn acl_get_fd_np(fd: i32, kind: u32) -> *mut c_void;
        fn acl_free(object: *mut c_void) -> i32;
        fn acl_get_entry(acl: *mut c_void, which: i32, entry: *mut *mut c_void) -> i32;
        fn acl_get_tag_type(entry: *mut c_void, tag: *mut u32) -> i32;
        fn acl_get_permset_mask_np(entry: *mut c_void, permissions: *mut u64) -> i32;
        fn acl_get_qualifier(entry: *mut c_void) -> *mut c_void;
        fn mbr_uuid_to_id(uuid: *const u8, id: *mut u32, kind: *mut i32) -> i32;
    }
    struct Acl(*mut c_void);
    impl Drop for Acl {
        fn drop(&mut self) {
            // SAFETY: object was allocated by an ACL API and is freed once.
            unsafe { acl_free(self.0) };
        }
    }
    // SAFETY: live fd; ACL_TYPE_EXTENDED is Darwin's documented 0x100.
    let raw = unsafe { acl_get_fd_np(file.as_raw_fd(), 0x100) };
    if raw.is_null() {
        let error = io::Error::last_os_error();
        // Darwin filesec reports ENOENT when this live fd has no ACL property.
        return if error.raw_os_error() == Some(libc::ENOENT) {
            Ok(())
        } else {
            Err(error)
        };
    }
    let acl = Acl(raw);
    let mut entries = Vec::new();
    let mut which = 0; // ACL_FIRST_ENTRY, followed by ACL_NEXT_ENTRY (-1).
    loop {
        let mut entry = std::ptr::null_mut();
        // SAFETY: live owned ACL and initialized entry output.
        let result = unsafe { acl_get_entry(acl.0, which, &raw mut entry) };
        // Darwin acl_get_entry returns -1/EINVAL at end (unlike POSIX).
        // The ACL and iterator constants above are valid by construction.
        if result == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL) {
            break;
        }
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        which = -1;
        let (mut tag, mut permissions) = (0, 0);
        // SAFETY: borrowed live entry and correctly sized output values.
        if unsafe { acl_get_tag_type(entry, &raw mut tag) } != 0
            // SAFETY: same live entry and u64 permission-mask output.
            || unsafe { acl_get_permset_mask_np(entry, &raw mut permissions) } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if !matches!(tag, 1 | 2) {
            return Err(refused());
        }
        // SAFETY: entry is live; returned qualifier is an owned UUID allocation.
        let qualifier = unsafe { acl_get_qualifier(entry) };
        if qualifier.is_null() {
            return Err(io::Error::last_os_error());
        }
        let qualifier = Acl(qualifier);
        let (mut id, mut kind) = (u32::MAX, -1);
        // SAFETY: qualifier is a UUID; both outputs are live exact-size scalars.
        let resolved = unsafe { mbr_uuid_to_id(qualifier.0.cast(), &raw mut id, &raw mut kind) };
        const CHANGES: u64 = (1 << 2)
            | (1 << 4)
            | (1 << 5)
            | (1 << 6)
            | (1 << 8)
            | (1 << 10)
            | (1 << 12)
            | (1 << 13);
        entries.push(Entry {
            allow: tag == 1,
            root: resolved == 0 && kind == 0 && id == 0,
            changes: permissions & CHANGES != 0,
        });
    }
    policy(&entries, ancestor)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn platform_validate(_: &std::fs::File, _: bool) -> io::Result<()> {
    Err(refused())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn native_acl_is_read_from_the_opened_object() {
        let path = std::env::temp_dir().join(format!("ferrow-acl-{}", std::process::id()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        assert!(validate(&file, false).is_ok());
        attach_acl(&file);
        let result = validate(&file, false);
        drop(file);
        std::fs::remove_file(path).unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    #[cfg(target_os = "linux")]
    fn attach_acl(file: &std::fs::File) {
        use std::os::fd::AsRawFd;
        let mut bytes = 2u32.to_le_bytes().to_vec();
        for (tag, permissions, id) in [
            (1u16, 6u16, u32::MAX),
            (2, 4, 12345),
            (4, 0, u32::MAX),
            (16, 4, u32::MAX),
            (32, 0, u32::MAX),
        ] {
            bytes.extend(tag.to_le_bytes());
            bytes.extend(permissions.to_le_bytes());
            bytes.extend(id.to_le_bytes());
        }
        // SAFETY: live file, attribute name and exact initialized ACL byte buffer.
        assert_eq!(
            unsafe {
                libc::fsetxattr(
                    file.as_raw_fd(),
                    c"system.posix_acl_access".as_ptr(),
                    bytes.as_ptr().cast(),
                    bytes.len(),
                    0,
                )
            },
            0
        );
    }

    #[cfg(target_os = "macos")]
    fn attach_acl(file: &std::fs::File) {
        use std::ffi::c_void;
        use std::os::fd::AsRawFd;
        unsafe extern "C" {
            fn acl_init(count: i32) -> *mut c_void;
            fn acl_create_entry(acl: *mut *mut c_void, entry: *mut *mut c_void) -> i32;
            fn acl_set_tag_type(entry: *mut c_void, tag: u32) -> i32;
            fn acl_set_qualifier(entry: *mut c_void, uuid: *const c_void) -> i32;
            fn acl_set_permset_mask_np(entry: *mut c_void, mask: u64) -> i32;
            fn acl_set_fd_np(fd: i32, acl: *mut c_void, kind: u32) -> i32;
            fn acl_free(object: *mut c_void) -> i32;
            fn mbr_uid_to_uuid(uid: u32, uuid: *mut u8) -> i32;
        }
        let mut uuid = [0u8; 16];
        // SAFETY: credential getter and an exact 16-byte UUID output buffer.
        assert_eq!(
            unsafe { mbr_uid_to_uuid(libc::geteuid(), uuid.as_mut_ptr()) },
            0
        );
        // SAFETY: request one ACL slot; the result is owned and freed below.
        let mut acl = unsafe { acl_init(1) };
        assert!(!acl.is_null());
        let mut entry = std::ptr::null_mut();
        // SAFETY: owned ACL pointer and live entry output; API may resize ACL.
        assert_eq!(unsafe { acl_create_entry(&raw mut acl, &raw mut entry) }, 0);
        // SAFETY: live entry and documented ALLOW tag.
        assert_eq!(unsafe { acl_set_tag_type(entry, 1) }, 0);
        // SAFETY: live entry and exact UUID qualifier.
        assert_eq!(unsafe { acl_set_qualifier(entry, uuid.as_ptr().cast()) }, 0);
        // SAFETY: live entry and documented READ_DATA bit.
        assert_eq!(unsafe { acl_set_permset_mask_np(entry, 1 << 1) }, 0);
        // SAFETY: owned ACL, live descriptor and ACL_TYPE_EXTENDED.
        let result = unsafe { acl_set_fd_np(file.as_raw_fd(), acl, 0x100) };
        // SAFETY: free the owned ACL exactly once after the setter copied it.
        unsafe { acl_free(acl) };
        assert_eq!(result, 0);
    }
    #[test]
    fn protected_objects_reject_read_write_and_inherited_acl_entries() {
        assert!(policy(&[], false).is_ok());
        for entry in [
            Entry {
                allow: true,
                root: false,
                changes: false,
            },
            Entry {
                allow: true,
                root: false,
                changes: true,
            },
            Entry {
                allow: false,
                root: true,
                changes: true,
            },
        ] {
            assert!(
                policy(&[entry], false).is_err(),
                "protected object accepted an ACL entry"
            );
        }
    }
    #[test]
    fn ancestors_reject_nonroot_changes_but_allow_read_and_deny_entries() {
        for entry in [
            Entry {
                allow: true,
                root: true,
                changes: true,
            },
            Entry {
                allow: true,
                root: false,
                changes: false,
            },
            Entry {
                allow: false,
                root: false,
                changes: true,
            },
        ] {
            assert!(policy(&[entry], true).is_ok());
        }
        assert!(
            policy(
                &[Entry {
                    allow: true,
                    root: false,
                    changes: true
                }],
                true
            )
            .is_err(),
            "ancestor accepted write/delete/chmod grant"
        );
    }
    #[test]
    fn linux_constructed_acl_rejects_named_reads_on_final_and_writes_on_ancestors() {
        let mut bytes = 2u32.to_le_bytes().to_vec();
        for (tag, permissions, id) in [
            (1u16, 7u16, u32::MAX),
            (2, 4, 1000),
            (4, 0, u32::MAX),
            (16, 4, u32::MAX),
            (32, 0, u32::MAX),
        ] {
            bytes.extend(tag.to_le_bytes());
            bytes.extend(permissions.to_le_bytes());
            bytes.extend(id.to_le_bytes());
        }
        assert!(posix_acl(&bytes, false, false).is_err());
        assert!(posix_acl(&bytes, true, false).is_ok());
        bytes[14] = 6;
        bytes[30] = 6; // named user's permissions and mask.
        assert!(posix_acl(&bytes, true, false).is_err());
        assert!(posix_acl(&[2, 0, 0], true, false).is_err());
        assert!(posix_acl(&bytes, false, true).is_err());
    }
}
