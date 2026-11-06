#![cfg(unix)]
use ferrow_ipc::{Endpoint, Listener};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};

#[test]
fn client_refuses_writable_directory_before_sending_hello() {
    let (dir, ep) = fixture("client-auth");
    let listener = std::os::unix::net::UnixListener::bind(dir.join("ipc.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    let error = match ferrow_ipc::Client::connect(&ep) {
        Err(error) => error,
        Ok(_) => panic!("connected through an untrusted directory"),
    };
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    drop(listener);
    std::fs::remove_dir_all(dir).unwrap();
}

fn fixture(tag: &str) -> (std::path::PathBuf, Endpoint) {
    let dir = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("fr-{}-{tag}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let endpoint = Endpoint::from_path(dir.join("ipc.sock"));
    (dir, endpoint)
}

#[test]
fn regular_files_and_live_listeners_are_preserved() {
    let (dir, ep) = fixture("preserve");
    let path = dir.join("ipc.sock");
    std::fs::write(&path, b"sentinel").unwrap();
    assert!(Listener::bind(&ep).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"sentinel");
    std::fs::remove_file(&path).unwrap();
    let first = Listener::bind(&ep).unwrap();
    let ino = std::fs::symlink_metadata(&path).unwrap().ino();
    assert!(Listener::bind(&ep).is_err());
    assert_eq!(std::fs::symlink_metadata(&path).unwrap().ino(), ino);
    drop(first);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn stale_socket_is_reclaimed_but_replacement_survives_drop() {
    let (dir, ep) = fixture("stale");
    let path = dir.join("ipc.sock");
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    let first = Listener::bind(&ep).unwrap();
    std::fs::remove_file(&path).unwrap();
    let replacement = std::os::unix::net::UnixListener::bind(&path).unwrap();
    drop(first);
    assert!(
        std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_socket()
    );
    drop(replacement);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn writable_and_symlinked_parents_are_refused() {
    let (dir, ep) = fixture("parents");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(Listener::bind(&ep).is_err());
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let link = dir.with_extension("link");
    std::os::unix::fs::symlink(&dir, &link).unwrap();
    assert!(Listener::bind(&Endpoint::from_path(link.join("ipc.sock"))).is_err());
    std::fs::remove_file(link).unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}
