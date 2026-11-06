//! Endpoint ownership. A private directory plus a lifetime flock serializes
//! cooperating instances; inode checks keep old drops away from replacements.
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path};

fn refused(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

pub(super) fn directory(path: &Path, system: bool) -> io::Result<()> {
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    if !path.is_absolute() || system && uid != 0 {
        return Err(refused(
            "system endpoint requires root and an absolute path",
        ));
    }
    let mut current = std::path::PathBuf::new();
    for part in path.components() {
        if !matches!(part, Component::RootDir | Component::Normal(_)) {
            return Err(refused("endpoint ancestors must be canonical"));
        }
        current.push(part);
        let created = match std::fs::symlink_metadata(&current) {
            Ok(_) => false,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                match std::fs::DirBuilder::new().mode(0o700).create(&current) {
                    Ok(()) => true,
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => false,
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        };
        let meta = std::fs::symlink_metadata(&current)?;
        let sticky_temp = current != path && meta.uid() == 0 && meta.mode() & 0o1000 != 0;
        if !meta.is_dir()
            || meta.file_type().is_symlink()
            || !(meta.uid() == 0 || !system && meta.uid() == uid)
            || meta.mode() & 0o022 != 0 && !sticky_temp
        {
            return Err(refused("untrusted endpoint directory or ancestor"));
        }
        if current == path {
            if meta.uid() != uid {
                return Err(refused("endpoint directory has another owner"));
            }
            let mode = if system { 0o755 } else { 0o700 };
            let dir = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&current)?;
            if created {
                dir.set_permissions(std::fs::Permissions::from_mode(mode))?;
            }
            if dir.metadata()?.mode() & 0o777 != mode {
                return Err(refused("incorrect endpoint directory mode"));
            }
        }
    }
    Ok(())
}

pub(super) fn lease(path: &Path) -> io::Result<File> {
    // The containing directory has already been validated and cannot be
    // renamed/replaced by other users. Never unlink the lock inode.
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path.with_extension("lock"))?;
    let meta = lock.metadata()?;
    // SAFETY: credential getter has no preconditions.
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
        || meta.nlink() != 1
    {
        return Err(refused("untrusted endpoint lease"));
    }
    // SAFETY: valid owned fd; nonblocking advisory lock persists until close.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "endpoint already owned",
        ));
    }
    Ok(lock)
}

pub(super) fn identity(path: &Path) -> io::Result<(u64, u64)> {
    let meta = std::fs::symlink_metadata(path)?;
    // SAFETY: credential getter has no preconditions.
    if !meta.file_type().is_socket() || meta.uid() != unsafe { libc::geteuid() } {
        return Err(refused("endpoint is not an owned socket"));
    }
    Ok((meta.dev(), meta.ino()))
}

pub(super) fn reclaim(path: &Path) -> io::Result<()> {
    let old = match identity(path) {
        Ok(id) => id,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    match std::os::unix::net::UnixStream::connect(path) {
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {}
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "endpoint is live or cannot be proved stale",
            ));
        }
    }
    if identity(path)? != old {
        return Err(refused("endpoint changed during stale recovery"));
    }
    std::fs::remove_file(path)
}
