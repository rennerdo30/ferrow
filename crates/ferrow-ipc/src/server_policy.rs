//! Endpoint authentication decisions, separate from credential acquisition.
use std::io;

pub(super) fn server_uid(expected: u32, actual: Option<u32>) -> io::Result<()> {
    if actual != Some(expected) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("untrusted IPC server uid {actual:?}; expected uid {expected}"),
        ));
    }
    Ok(())
}

pub(super) fn directory(expected: u32, owner: u32, mode: u32, is_dir: bool) -> io::Result<()> {
    if owner != expected || mode & 0o022 != 0 || !is_dir {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "untrusted IPC socket directory: expected owner uid {expected} and no other writers"
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_service_and_same_user_development_are_authenticated() {
        assert!(server_uid(0, Some(0)).is_ok());
        assert!(server_uid(1000, Some(1000)).is_ok());
        for actual in [None, Some(1001)] {
            assert!(
                server_uid(1000, actual).is_err(),
                "accepted a forged development server"
            );
        }
        assert!(
            server_uid(0, Some(1000)).is_err(),
            "accepted a non-root system server"
        );
    }

    #[test]
    fn socket_directory_requires_the_expected_owner_and_no_other_writers() {
        assert!(directory(0, 0, 0o755, true).is_ok());
        assert!(directory(1000, 1000, 0o700, true).is_ok());
        for (owner, mode, is_dir) in [
            (1001, 0o755, true),
            (1000, 0o720, true),
            (1000, 0o702, true),
            (1000, 0o700, false),
        ] {
            assert!(
                directory(1000, owner, mode, is_dir).is_err(),
                "accepted an untrusted socket directory"
            );
        }
    }
}
