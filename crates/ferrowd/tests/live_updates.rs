//! End to end: the service's live updates show file changes in searches.
//!
//! Needs raw volume access (an elevated GitHub Windows runner). Elsewhere
//! the live updater does not start and the test only checks that.
#![cfg(windows)]

use std::time::{Duration, Instant};

use ferrow_ipc::{Request, Response};
use ferrow_walk::WalkOptions;
use ferrowd::{Snapshot, SnapshotStore};

fn hits(store: &SnapshotStore, text: &str) -> Vec<String> {
    let responses = ferrowd::handle(
        store,
        Request::Query {
            text: text.into(),
            limit: 100,
            sort: ferrow_ipc::Sort::Name,
            flags: 0,
        },
    );
    let mut out = Vec::new();
    for r in responses {
        match r {
            Response::Results { paths, .. } => out.extend(
                paths
                    .into_iter()
                    .map(|p| String::from_utf8_lossy(&p).into_owned()),
            ),
            other => panic!("unexpected {other:?}"),
        }
    }
    out
}

fn wait_until(what: &str, mut ok: impl FnMut() -> bool) -> Duration {
    let started = Instant::now();
    while !ok() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    started.elapsed()
}

#[test]
fn file_changes_appear_in_searches_within_a_second() {
    let ci = std::env::var_os("GITHUB_ACTIONS").is_some();
    let dir = std::env::temp_dir().join(format!("ferrow-live-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub").join("existing-zq.txt"), b"x").unwrap();
    let store = SnapshotStore::new(Snapshot::empty());
    ferrowd::start_live(
        &store,
        WalkOptions {
            roots: vec![dir.clone()],
            fast_ntfs: true,
            ..WalkOptions::default()
        },
    );
    if !ci {
        // Not elevated: nothing starts, the store stays as it was.
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(store.get().mode, "empty");
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    wait_until("the initial scan", || store.get().mode == "live");
    assert_eq!(hits(&store, "existing-zq").len(), 1);

    std::fs::write(dir.join("created-zq.txt"), b"new").unwrap();
    let took = wait_until("a created file", || hits(&store, "created-zq").len() == 1);
    eprintln!("created file visible after {took:?}");
    assert!(took < Duration::from_secs(1), "live change took {took:?}");

    std::fs::rename(
        dir.join("created-zq.txt"),
        dir.join("sub").join("renamed-zq.txt"),
    )
    .unwrap();
    wait_until("a renamed file", || {
        hits(&store, "renamed-zq") == [dir.join("sub").join("renamed-zq.txt").display().to_string()]
            && hits(&store, "created-zq").is_empty()
    });

    std::fs::rename(dir.join("sub"), dir.join("moved-dir-zq")).unwrap();
    wait_until("a renamed folder with its files", || {
        hits(&store, "renamed-zq")
            == [dir
                .join("moved-dir-zq")
                .join("renamed-zq.txt")
                .display()
                .to_string()]
    });

    std::fs::remove_dir_all(dir.join("moved-dir-zq")).unwrap();
    wait_until("deleted files", || hits(&store, "zq").is_empty());
    let status = ferrowd::handle(&store, Request::Status);
    eprintln!("{status:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
