//! Protected service installation paths; never register a build/output path.
use std::io;
use std::path::{Path, PathBuf};

fn refused(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

pub fn binary_path() -> io::Result<PathBuf> {
    #[cfg(windows)]
    {
        Ok(
            windows::known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_ProgramFiles)?
                .join("Ferrow/ferrowd.exe"),
        )
    }
    #[cfg(unix)]
    {
        Ok(PathBuf::from("/usr/local/libexec/ferrow/ferrowd"))
    }
}

pub fn install_binary(source: &Path) -> io::Result<PathBuf> {
    let target = binary_path()?;
    let parent = target
        .parent()
        .ok_or_else(|| refused("missing installation directory"))?;
    #[cfg(windows)]
    {
        windows::validate_path(parent.parent().unwrap(), windows::Role::Ancestor)?;
        windows::create_directory(parent, "O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)")?;
        windows::validate_path(parent, windows::Role::Directory)?;
    }
    #[cfg(unix)]
    unix::directory(parent, 0o755)?;
    if target.try_exists()? {
        validate_binary(&target)?;
    }
    let temporary = parent.join(format!("ferrowd-{}.new", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o755);
    }
    let mut output = options.open(&temporary)?;
    let result = (|| {
        std::io::copy(&mut std::fs::File::open(source)?, &mut output)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            output.set_permissions(std::fs::Permissions::from_mode(0o755))?;
        }
        output.sync_all()?;
        validate_binary(&temporary)?;
        std::fs::rename(&temporary, &target)?;
        validate_binary(&target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result?;
    Ok(target)
}

fn validate_binary(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        windows::validate_path(path, windows::Role::Binary)
    }
    #[cfg(unix)]
    {
        unix::binary(path)
    }
}

#[cfg(unix)]
pub(crate) mod unix {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

    pub fn metadata(path: &Path, directory: bool) -> io::Result<std::fs::Metadata> {
        let meta = std::fs::symlink_metadata(path)?;
        if meta.file_type().is_symlink()
            || meta.uid() != 0
            || meta.mode() & 0o022 != 0
            || directory && !meta.is_dir()
            || !directory && (!meta.is_file() || meta.nlink() != 1)
        {
            return Err(refused(
                "service path must be root-owned, protected, and link-free",
            ));
        }
        Ok(meta)
    }

    pub fn directory(path: &Path, mode: u32) -> io::Result<()> {
        // SAFETY: credential getter has no preconditions.
        if unsafe { libc::geteuid() } != 0 || !path.is_absolute() {
            return Err(refused("protected service directories require root"));
        }
        let mut current = PathBuf::new();
        for component in path.components() {
            if !matches!(
                component,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            ) {
                return Err(refused("noncanonical service path"));
            }
            current.push(component);
            if !current.try_exists()? {
                match std::fs::DirBuilder::new().mode(0o700).create(&current) {
                    Ok(()) => {
                        let handle = std::fs::OpenOptions::new()
                            .read(true)
                            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
                            .open(&current)?;
                        handle.set_permissions(std::fs::Permissions::from_mode(
                            if current == path { mode } else { 0o755 },
                        ))?;
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e),
                }
            }
            let meta = metadata(&current, true)?;
            if current == path && meta.mode() & 0o777 != mode {
                return Err(refused("incorrect service directory mode"));
            }
        }
        Ok(())
    }

    pub fn binary(path: &Path) -> io::Result<()> {
        let meta = metadata(path, false)?;
        if meta.mode() & 0o777 != 0o755 {
            return Err(refused("service binary must be 0755"));
        }
        Ok(())
    }
}

#[cfg(windows)]
pub(crate) mod windows {
    use super::*;
    use std::os::windows::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    };
    use windows_sys::Win32::{
        Foundation::LocalFree, Security::Authorization::*, Security::*, Storage::FileSystem::*,
    };

    const TRUSTED_INSTALLER: &str =
        "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";
    #[derive(Clone, Copy)]
    pub enum Role {
        Ancestor,
        Directory,
        Binary,
    }
    pub struct Descriptor(pub PSECURITY_DESCRIPTOR);
    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: this wrapper owns an API-allocated descriptor exactly once.
            unsafe { LocalFree(self.0) };
        }
    }
    pub fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }
    impl Descriptor {
        pub fn sddl(text: &str) -> io::Result<Self> {
            let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
            let mut sd = std::ptr::null_mut();
            // SAFETY: terminated input and live output; returned ownership is captured.
            if unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    text.as_ptr(),
                    1,
                    &raw mut sd,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(sd))
        }
        pub fn validate(&self, role: Role) -> io::Result<()> {
            let mut owner = std::ptr::null_mut();
            let mut defaulted = 0;
            let mut present = 0;
            let mut acl = std::ptr::null_mut();
            // SAFETY: live valid descriptor from the OS or SDDL converter; outputs live.
            if unsafe { GetSecurityDescriptorOwner(self.0, &raw mut owner, &raw mut defaulted) }
                == 0
                || !trusted(&sid_text(owner)?)
            {
                return Err(refused("service path has an untrusted owner"));
            }
            // SAFETY: same valid descriptor and live output pointers.
            if unsafe {
                GetSecurityDescriptorDacl(
                    self.0,
                    &raw mut present,
                    &raw mut acl,
                    &raw mut defaulted,
                )
            } == 0
                || present == 0
                || acl.is_null()
            {
                return Err(refused("service path has no restrictive DACL"));
            }
            let forbidden = match role {
                Role::Ancestor => 0x000d0040,
                Role::Directory => 0x000d0156,
                Role::Binary => 0x000d0116,
            };
            // SAFETY: API-owned ACL is initialized and remains live for enumeration.
            let count = unsafe { (*acl).AceCount };
            for index in 0..count {
                let mut ace = std::ptr::null_mut();
                // SAFETY: bounded index into live ACL; API supplies the ACE pointer.
                if unsafe { GetAce(acl, u32::from(index), &raw mut ace) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: OS-validated ACE storage; unaligned value reads avoid Rust references.
                let header = unsafe { std::ptr::read_unaligned(ace.cast::<ACE_HEADER>()) };
                if (header.AceFlags & INHERIT_ONLY_ACE as u8 != 0
                    && !matches!(role, Role::Directory))
                    || header.AceType == 1
                {
                    continue;
                }
                if header.AceType != 0 {
                    return Err(refused("unsupported service path ACE"));
                }
                // SAFETY: basic ACCESS_ALLOWED_ACE established above; SID stays in descriptor.
                let allowed = unsafe { std::ptr::read_unaligned(ace.cast::<ACCESS_ALLOWED_ACE>()) };
                let sid = unsafe {
                    ace.cast::<u8>()
                        .add(std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart))
                        .cast()
                };
                let trustee = sid_text(sid)?;
                let mut mask = allowed.Mask;
                let mapping = GENERIC_MAPPING {
                    GenericRead: FILE_GENERIC_READ,
                    GenericWrite: FILE_GENERIC_WRITE,
                    GenericExecute: FILE_GENERIC_EXECUTE,
                    GenericAll: FILE_ALL_ACCESS,
                };
                // SAFETY: initialized mask/mapping, used only to expand generic rights.
                unsafe { MapGenericMask(&raw mut mask, &mapping) };
                if mask & forbidden != 0 && !trusted(&trustee) && trustee != "S-1-3-4" {
                    return Err(refused(
                        "non-admin can modify or replace protected service path",
                    ));
                }
            }
            Ok(())
        }
    }
    fn trusted(sid: &str) -> bool {
        matches!(sid, "S-1-5-18" | "S-1-5-32-544") || sid == TRUSTED_INSTALLER
    }
    fn sid_text(sid: PSID) -> io::Result<String> {
        let mut text = std::ptr::null_mut();
        // SAFETY: caller supplies an OS-owned SID, retained through conversion.
        if sid.is_null() || unsafe { ConvertSidToStringSidW(sid, &raw mut text) } == 0 {
            return Err(refused("cannot query service path SID"));
        }
        let mut len = 0;
        // SAFETY: conversion returns a terminated allocated string; free exactly once.
        let result = unsafe {
            while *text.add(len) != 0 {
                len += 1;
            }
            let result = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            LocalFree(text.cast());
            result
        };
        Ok(result)
    }
    pub fn validate_path(path: &Path, final_role: Role) -> io::Result<()> {
        if !path.is_absolute() {
            return Err(refused("service path must be absolute"));
        }
        let parts: Vec<_> = path.ancestors().collect();
        for (index, part) in parts.iter().enumerate().rev() {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
                .open(part)?;
            if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(refused("reparse point in service path"));
            }
            let mut sd = std::ptr::null_mut();
            // SAFETY: live owned file/directory handle, output owns returned descriptor.
            let result = unsafe {
                GetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &raw mut sd,
                )
            };
            if result != 0 {
                return Err(io::Error::from_raw_os_error(result as i32));
            }
            Descriptor(sd).validate(if index == 0 {
                final_role
            } else {
                Role::Ancestor
            })?;
        }
        Ok(())
    }
    pub fn create_directory(path: &Path, sddl: &str) -> io::Result<()> {
        if path.try_exists()? {
            return Ok(());
        }
        let sd = Descriptor::sddl(sddl)?;
        let attrs = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: 0,
        };
        let name = wide(path);
        // SAFETY: path and initialized attributes live through creation; descriptor copied.
        if unsafe { CreateDirectoryW(name.as_ptr(), &attrs) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    pub fn known_folder(id: &windows_sys::core::GUID) -> io::Result<PathBuf> {
        use windows_sys::Win32::{System::Com::CoTaskMemFree, UI::Shell::SHGetKnownFolderPath};
        let mut text = std::ptr::null_mut();
        // SAFETY: valid known-folder identifier; API allocates terminated output.
        let code = unsafe { SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &raw mut text) };
        if code < 0 {
            return Err(io::Error::other(format!("known folder error {code:#x}")));
        }
        let mut len = 0;
        // SAFETY: allocated terminated output remains live until CoTaskMemFree.
        let result = unsafe {
            while *text.add(len) != 0 {
                len += 1;
            }
            use std::os::windows::ffi::OsStringExt;
            let result = PathBuf::from(std::ffi::OsString::from_wide(std::slice::from_raw_parts(
                text, len,
            )));
            CoTaskMemFree(text.cast());
            result
        };
        Ok(result)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn ancestor_create_only_is_safe_but_replacement_and_control_are_not() {
            for right in ["0x4", "GR"] {
                Descriptor::sddl(&format!("O:BAD:P(A;;FA;;;BA)(A;;{right};;;AU)"))
                    .unwrap()
                    .validate(Role::Ancestor)
                    .unwrap();
            }
            for right in ["0x10000", "0x40000", "0x80000", "0x40", "GA"] {
                assert!(
                    Descriptor::sddl(&format!("O:BAD:P(A;;FA;;;BA)(A;;{right};;;AU)"))
                        .unwrap()
                        .validate(Role::Ancestor)
                        .is_err(),
                    "{right}"
                );
            }
        }

        #[test]
        fn directory_and_binary_reject_nonadmin_write_rights_and_untrusted_owners() {
            for role in [Role::Directory, Role::Binary] {
                for right in ["0x2", "0x4", "0x10000", "0x40000", "0x80000", "GW", "GA"] {
                    assert!(
                        Descriptor::sddl(&format!("O:SYD:P(A;;FA;;;SY)(A;;{right};;;BU)"))
                            .unwrap()
                            .validate(role)
                            .is_err(),
                        "{right}"
                    );
                }
                assert!(
                    Descriptor::sddl("O:BUD:P(A;;FA;;;SY)")
                        .unwrap()
                        .validate(role)
                        .is_err()
                );
                Descriptor::sddl("O:SYD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;GR;;;BU)")
                    .unwrap()
                    .validate(role)
                    .unwrap();
            }
            assert!(
                Descriptor::sddl("O:SYD:P(A;OICIIO;GW;;;AU)")
                    .unwrap()
                    .validate(Role::Directory)
                    .is_err()
            );
        }
    }
}
