//! Daemon tests over a per-test unprivileged endpoint: query,
//! count, status, snapshot swap visibility, clean shutdown.

use ferrow_ipc::{Client, Endpoint};
use ferrow_walk::WalkOptions;
use ferrowd::{DaemonConfig, Shutdown, Snapshot, SnapshotStore, serve};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn service_plain_count_and_query_use_the_compact_factory() {
    use ferrow_ipc::{Request, Response};
    use ferrow_query::take_compact_plan_for_test;
    let b = ferrow_index::IndexBuilder::new();
    let root = b.add_root(b"root", ferrow_index::flags::DIR);
    b.add(root, b"Report 2024.txt", 0);
    let store = SnapshotStore::new(Snapshot {
        index: std::sync::Arc::new(b.freeze()),
        live: None,
        mode: "fixture".into(),
        report: "fixture".into(),
    });
    for text in ["report 2024", "!tmp log", "case:Report", "\"Report 2024\""] {
        take_compact_plan_for_test();
        assert!(matches!(
            &ferrowd::handle(&store, Request::Count { text: text.into() })[..],
            [Response::Count { .. }]
        ));
        assert_eq!(take_compact_plan_for_test(), Some(true), "Count: {text}");
        let request = Request::Query {
            text: text.into(),
            limit: 100,
            sort: ferrow_ipc::protocol::Sort::Name,
            flags: 0,
        };
        assert!(matches!(
            &ferrowd::handle(&store, request)[..],
            [Response::Results { .. }]
        ));
        assert_eq!(take_compact_plan_for_test(), Some(true), "Query: {text}");
    }
}

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
    let snapshot = store.get();
    snapshot.index.request_sort(ferrow_index::SortKey::Modified);
    assert_eq!(
        snapshot
            .index
            .sort_ticket(ferrow_index::SortKey::Modified)
            .unwrap()
            .wait_timeout(std::time::Duration::from_secs(2)),
        ferrow_index::SortStatus::Ready
    );
    let (rows, columns) = client
        .query_rows("alpha", 10, ferrow_ipc::Sort::Modified, true)
        .unwrap();
    assert_eq!(columns, ferrow_index::ColumnSet::DEFAULT.bits());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].size, Some(1));
    assert!(rows[0].modified.is_some());
    assert_eq!(rows[0].created, None);
    assert!(
        client
            .query_rows("alpha", 10, ferrow_ipc::Sort::Created, false)
            .is_err()
    );
    assert_eq!(entries, entries_before);
    assert_eq!(mode, "walk");

    // Swap in a fresh snapshot: new queries see the new file,
    // without reconnecting.
    dir.file("gamma-report.txt");
    store.rescan(&options);
    assert_eq!(client.count("report").unwrap(), 3);

    shutdown.request(&endpoint);
    server.join().unwrap().unwrap();
    assert!(
        client.status().is_err(),
        "shutdown left a detached connection alive"
    );
    drop(client);
}

#[test]
fn metadata_sort_keeps_values_aligned_across_pages() {
    let b = ferrow_index::IndexBuilder::with_columns(ferrow_index::ColumnSet::DEFAULT);
    for i in 0..700 {
        b.add_with_metadata(
            ferrow_index::ROOT_PARENT,
            format!("file-{i:04}").as_bytes(),
            0,
            ferrow_index::EntryMetadata {
                size: Some(i),
                modified: Some(i as i64 * 10_000_000),
                ..Default::default()
            },
        );
    }
    let store = SnapshotStore::new(Snapshot {
        index: std::sync::Arc::new(b.freeze()),
        live: None,
        mode: "fixture".into(),
        report: String::new(),
    });
    let snapshot = store.get();
    let refused = ferrowd::handle(
        &store,
        ferrow_ipc::Request::Query {
            text: "file".into(),
            limit: 1,
            sort: ferrow_ipc::Sort::Modified,
            flags: 1,
        },
    );
    assert!(matches!(
        &refused[..],
        [ferrow_ipc::Response::Error {
            code: ferrow_ipc::ErrorCode::BadRequest,
            ..
        }]
    ));
    snapshot.index.request_sort(ferrow_index::SortKey::Modified);
    assert_eq!(
        snapshot
            .index
            .sort_ticket(ferrow_index::SortKey::Modified)
            .unwrap()
            .wait_timeout(std::time::Duration::from_secs(2)),
        ferrow_index::SortStatus::Ready
    );
    let responses = ferrowd::handle(
        &store,
        ferrow_ipc::Request::Query {
            text: "file".into(),
            limit: 700,
            sort: ferrow_ipc::Sort::Modified,
            flags: 3,
        },
    );
    assert!(responses.len() > 1);
    let mut expected = 699;
    for (i, response) in responses.iter().enumerate() {
        let ferrow_ipc::Response::Rows {
            rows,
            columns,
            more,
            sort_pending,
        } = response
        else {
            panic!("{response:?}")
        };
        assert_eq!(*columns, 3);
        assert!(!sort_pending);
        assert_eq!(*more, i + 1 < responses.len());
        for row in rows {
            assert_eq!(row.size, Some(expected));
            assert_eq!(row.modified, Some(expected as i64 * 10_000_000));
            assert_eq!(row.path, format!("file-{expected:04}").as_bytes());
            expected = expected.saturating_sub(1);
        }
    }
    assert_eq!(expected, 0);
}

#[test]
fn unlimited_service_queries_have_an_explicit_overall_result_budget() {
    let builder = ferrow_index::IndexBuilder::new();
    let root = builder.add_root(b"root", ferrow_index::flags::DIR);
    for _ in 0..10_001 {
        builder.add(root, b"hit", 0);
    }
    let store = SnapshotStore::new(Snapshot {
        index: std::sync::Arc::new(builder.freeze()),
        live: None,
        mode: "fixture".into(),
        report: "fixture".into(),
    });
    let request = |limit| ferrow_ipc::Request::Query {
        text: "hit".into(),
        limit,
        sort: ferrow_ipc::protocol::Sort::Name,
        flags: 0,
    };
    assert!(matches!(
        &ferrowd::handle(&store, request(0))[..],
        [ferrow_ipc::Response::Error {
            code: ferrow_ipc::ErrorCode::TooLarge,
            ..
        }]
    ));
    assert!(
        matches!(&ferrowd::handle(&store, request(1))[..], [ferrow_ipc::Response::Results { paths, more: false }] if paths.len() == 1)
    );
}

#[test]
fn unknown_callers_cannot_read_any_snapshot_data() {
    let store = SnapshotStore::new(Snapshot::empty());
    let owner = ferrow_ipc::CallerIdentity::current().unwrap();
    let unknown = ferrow_ipc::CallerIdentity::default();
    for system in [false, true] {
        for request in [
            ferrow_ipc::Request::Status,
            ferrow_ipc::Request::Count { text: "".into() },
            ferrow_ipc::Request::Query {
                text: "".into(),
                limit: 0,
                sort: ferrow_ipc::protocol::Sort::Name,
                flags: 0,
            },
        ] {
            let response = ferrowd::handle_authorized(&store, request, &unknown, &owner, system);
            assert!(matches!(
                &response[..],
                [ferrow_ipc::Response::Error {
                    code: ferrow_ipc::ErrorCode::Unauthorized,
                    ..
                }]
            ));
        }
    }
}

#[test]
fn controlled_shutdown_saves_the_latest_replaced_snapshot() {
    let dir = TestDir::new("persist");
    dir.file("new-after-start.txt");
    let store = SnapshotStore::new(Snapshot::empty());
    let endpoint = Endpoint::named(&format!("fr-persist-{}", std::process::id()));
    let path = dir.0.join("saved.bin");
    let config = DaemonConfig {
        endpoint: endpoint.clone(),
        index_path: Some(path.clone()),
        roots: vec![],
        excludes: vec![],
        follow_symlinks: false,
    };
    let shutdown = Shutdown::new();
    let stop = shutdown.clone();
    let server_store = store.clone();
    let server = std::thread::spawn(move || serve(&config, server_store, stop));
    drop(connect(&endpoint));
    store.rescan(&WalkOptions {
        columns: Default::default(),
        roots: vec![dir.0.clone()],
        ..Default::default()
    });
    shutdown.request(&endpoint);
    server.join().unwrap().unwrap();
    let saved = Snapshot::from_file(&path).unwrap();
    let response = ferrowd::handle(
        &SnapshotStore::new(saved),
        ferrow_ipc::Request::Count {
            text: "new-after-start".into(),
        },
    );
    assert!(matches!(
        &response[..],
        [ferrow_ipc::Response::Count { n: 1 }]
    ));
}
