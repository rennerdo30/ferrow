//! Service definitions select the privileged executable, so they use the same
//! protected-object policy as the executable itself.
use std::io;

fn metadata(owner: u32, expected: u32, mode: u32, links: u64, regular: bool) -> io::Result<()> {
    if owner != expected || mode & 0o7777 != 0o644 || links != 1 || !regular {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "service definition must be a root-owned, single-link regular file with mode 0644",
        ));
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn write(path: &std::path::Path, contents: &str) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing definition directory"))?;
    crate::protection::unix::directory(parent, 0o755)?;
    write_owned(path, contents, 0)
}

#[cfg(unix)]
fn verify(file: &std::fs::File, expected: u32) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let m = file.metadata()?;
    metadata(m.uid(), expected, m.mode(), m.nlink(), m.is_file())?;
    ferrow_ipc::unix_acl::validate(file, false)
}

#[cfg(unix)]
fn write_owned(path: &std::path::Path, contents: &str, expected: u32) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing definition directory"))?;
    match std::fs::symlink_metadata(path) {
        Ok(_) => {
            let old = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(path)?;
            verify(&old, expected)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let temporary = parent.join(format!(".ferrow-definition-{}.new", std::process::id()));
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .read(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temporary)?;
    let result = (|| {
        // fchmod sets the final mode independently of the caller's umask.
        output.set_permissions(std::fs::Permissions::from_mode(0o644))?;
        verify(&output, expected)?; // refuse inherited ACLs before writing bytes
        output.write_all(contents.as_bytes())?;
        output.sync_all()?;
        verify(&output, expected)?;
        std::fs::rename(&temporary, path)?;
        let published = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        verify(&published, expected)?;
        std::fs::File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registration_requires_exact_owner_mode_and_single_regular_file() {
        assert!(metadata(0, 0, 0o100644, 1, true).is_ok());
        for (owner, mode, links, regular) in [
            (1000, 0o644, 1, true),
            (0, 0o666, 1, true),
            (0, 0o600, 1, true),
            (0, 0o644, 2, true),
            (0, 0o644, 1, false),
        ] {
            assert!(
                metadata(owner, 0, mode, links, regular).is_err(),
                "accepted an unsafe service definition"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn definition_helper_protects_output_and_preserves_hostile_objects() {
        if std::env::var_os("FERROW_DEFINITION_UMASK_CHILD").is_none() {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "definition::tests::definition_helper_protects_output_and_preserves_hostile_objects", "--nocapture"])
                .env("FERROW_DEFINITION_UMASK_CHILD", "1").spawn().unwrap();
            eprintln!("umask fixture PID {}", child.id());
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success());
                    return;
                }
                if std::time::Instant::now() >= deadline {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("umask fixture timed out");
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        // SAFETY: only this dedicated one-test subprocess changes its own umask.
        // It exits after this fixture; the parent test process is unaffected.
        unsafe { libc::umask(0) };
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let directory =
            std::env::temp_dir().join(format!("ferrow-definition-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let target = directory.join("service");
        // SAFETY: credential getter has no preconditions.
        let owner = unsafe { libc::geteuid() };
        write_owned(&target, "first", owner).unwrap();
        let m = std::fs::metadata(&target).unwrap();
        assert_eq!(m.uid(), owner);
        assert_eq!(m.mode() & 0o7777, 0o644);
        write_owned(&target, "second", owner).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(write_owned(&target, "overwrite", owner).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "second");
        std::fs::remove_file(&target).unwrap();
        let sentinel = directory.join("sentinel");
        std::fs::write(&sentinel, "retained").unwrap();
        std::os::unix::fs::symlink(&sentinel, &target).unwrap();
        assert!(write_owned(&target, "overwrite", owner).is_err());
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "retained");
        std::fs::remove_dir_all(directory).unwrap();
    }
}
