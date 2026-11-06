//! Daemon tests over a per-test unprivileged endpoint: query,
//! count, status, snapshot swap visibility, clean shutdown.

use ferrow_ipc::{Client, Endpoint};
use ferrow_walk::WalkOptions;
use ferrowd::{DaemonConfig, Shutdown, Snapshot, SnapshotStore, serve};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> TestDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ferrowd-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TestDir(dir)
    }
    fn file(&self, rel: &str) {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn connect(endpoint: &Endpoint) -> Client<ferrow_ipc::Stream> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match Client::connect(endpoint) {
            Ok(client) => return client,
            Err(e) => {
                assert!(std::time::Instant::now() < deadline, "connect: {e}");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
}

#[test]
fn daemon_answers_and_snapshot_swap_is_visible() {
    let dir = TestDir::new("serve");
    dir.file("alpha-report.txt");
    dir.file("sub/beta-report.txt");
    let options = WalkOptions {
        roots: vec![dir.0.clone()],
        ..WalkOptions::default()
    };
    let (snapshot, _) = Snapshot::from_scan(&options);
    let entries_before = snapshot.index.len() as u64;
    let store = SnapshotStore::new(snapshot);

    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let endpoint = Endpoint::named(&format!(
        "ferrowd-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let config = DaemonConfig {
        endpoint: endpoint.clone(),
        index_path: None,
        roots: vec![],
        excludes: vec![],
        follow_symlinks: false,
    };
    let shutdown = Shutdown::new();
    let server_store = store.clone();
    let server_shutdown = shutdown.clone();
    let server = std::thread::spawn(move || serve(&config, server_store, server_shutdown));

    let mut client = connect(&endpoint);
    assert_eq!(client.count("report").unwrap(), 2);
    let hits = client.query("alpha", 0).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(String::from_utf8_lossy(&hits[0]).ends_with("alpha-report.txt"));
    let (entries, mode, _) = client.status().unwrap();
    assert_eq!(entries, entries_before);
    assert_eq!(mode, "walk");

    // Swap in a fresh snapshot: new queries see the new file,
    // without reconnecting.
    dir.file("gamma-report.txt");
    store.rescan(&options);
    assert_eq!(client.count("report").unwrap(), 3);

    drop(client);
    shutdown.request(&endpoint);
    server.join().unwrap().unwrap();
}
