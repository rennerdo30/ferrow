use ferrow_index::{ColumnSet, EntryMetadata, IndexBuilder, ROOT_PARENT};
use ferrow_query::{CancelToken, Query, SearchOptions, SortKey, search};

#[test]
fn already_cancelled_query_does_not_start_preparation() {
    let b = IndexBuilder::with_columns(ColumnSet::MODIFIED);
    for i in 0..4 {
        b.add_with_metadata(
            ROOT_PARENT,
            format!("file-{i}").as_bytes(),
            0,
            EntryMetadata {
                modified: Some(i * 10_000_000),
                ..Default::default()
            },
        );
    }
    let index = b.freeze();
    let ticket = index.sort_ticket(SortKey::Modified).unwrap();
    assert_eq!(ticket.status(), ferrow_query::SortStatus::Unprepared);
    let result = search(
        &index,
        &Query::default(),
        &SearchOptions {
            sort: SortKey::Modified,
            cancel: CancelToken::cancelled(),
            ..Default::default()
        },
    );
    assert!(result.cancelled);
    assert_eq!(result.visited, 0);
    assert!(result.hits.is_empty());
    assert_eq!(
        ticket.wait_timeout(std::time::Duration::from_secs(2)),
        ferrow_query::SortStatus::Unprepared
    );
    let options = SearchOptions {
        sort: SortKey::Modified,
        ..Default::default()
    };
    search(&index, &Query::default(), &options);
    assert_eq!(
        ticket.wait_timeout(std::time::Duration::from_secs(2)),
        ferrow_query::SortStatus::Ready
    );
}

#[test]
fn retired_index_cancels_preparation_but_a_live_owner_keeps_it_valid() {
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;
    for keep_owner in [false, true] {
        let b = IndexBuilder::with_columns(ColumnSet::MODIFIED);
        for i in 0..4 {
            b.add_with_metadata(
                ROOT_PARENT,
                format!("file-{i}").as_bytes(),
                0,
                EntryMetadata {
                    modified: Some(i * 10_000_000),
                    ..Default::default()
                },
            );
        }
        let index = Arc::new(b.freeze());
        let weak = Arc::downgrade(&index);
        let live = keep_owner.then(|| index.clone());
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let receiver = Mutex::new(release_rx);
        index.request_sort_with_hook(
            SortKey::Modified,
            Arc::new(move || {
                started_tx.send(()).unwrap();
                receiver
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap();
            }),
        );
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let ticket = index.sort_ticket(SortKey::Modified).unwrap();
        drop(index);
        assert_eq!(weak.upgrade().is_some(), keep_owner);
        release_tx.send(()).unwrap();
        let status = ticket.wait_timeout(Duration::from_secs(2));
        assert_eq!(
            status,
            if keep_owner {
                ferrow_query::SortStatus::Ready
            } else {
                ferrow_query::SortStatus::Unprepared
            }
        );
        if let Some(index) = live {
            assert!(index.metadata_order(SortKey::Modified, false).is_some());
        }
    }
}

#[test]
fn pending_sort_returns_names_and_cancellation_while_worker_is_paused() {
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;
    let b = IndexBuilder::with_columns(ColumnSet::MODIFIED);
    for i in 0..2000 {
        b.add_with_metadata(
            ROOT_PARENT,
            format!("file-{i:04}").as_bytes(),
            0,
            EntryMetadata {
                modified: Some(i * 10_000_000),
                ..Default::default()
            },
        );
    }
    let index = b.freeze();
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let receiver = Mutex::new(release_rx);
    index.request_sort_with_hook(
        SortKey::Modified,
        Arc::new(move || {
            started_tx.send(()).unwrap();
            receiver
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
        }),
    );
    started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let result = search(
        &index,
        &Query::default(),
        &SearchOptions {
            sort: SortKey::Modified,
            descending: true,
            limit: Some(10),
            ..Default::default()
        },
    );
    assert_eq!(result.sort_status, ferrow_query::SortStatus::Pending);
    assert_eq!(result.hits, index.sorted()[..10]);
    let result = search(
        &index,
        &Query::default(),
        &SearchOptions {
            sort: SortKey::Modified,
            cancel: CancelToken::cancelled(),
            ..Default::default()
        },
    );
    assert!(result.cancelled);
    assert!(result.hits.is_empty());
    let fresh = IndexBuilder::with_columns(ColumnSet::MODIFIED).freeze();
    assert_eq!(
        fresh.sort_ticket(SortKey::Modified).unwrap().status(),
        ferrow_query::SortStatus::Unprepared
    );
    assert!(fresh.metadata_order(SortKey::Modified, false).is_none());
    release_tx.send(()).unwrap();
    assert_eq!(
        index
            .sort_ticket(SortKey::Modified)
            .unwrap()
            .wait_timeout(Duration::from_secs(2)),
        ferrow_query::SortStatus::Ready
    );
    let result = search(
        &index,
        &Query::default(),
        &SearchOptions {
            sort: SortKey::Modified,
            descending: true,
            limit: Some(10),
            ..Default::default()
        },
    );
    assert_eq!(result.sort_status, ferrow_query::SortStatus::Ready);
    assert_eq!(result.hits, (1990..2000).rev().collect::<Vec<_>>());
}

#[test]
fn every_sort_matches_independent_oracle_with_name_ties_and_missing_values() {
    let builder = IndexBuilder::with_columns(ColumnSet::ALL);
    let mut rows = Vec::new();
    for i in 0..2049 {
        let name = format!("{}{:04}", if i % 2 == 0 { "a" } else { "Z" }, 2049 - i);
        let metadata = EntryMetadata {
            size: (i % 7 != 0).then_some((i % 13) as u64),
            modified: (i % 11 != 0).then_some(((i % 19) as i64 - 9) * 10_000_000),
            created: (i % 5 != 0).then_some(((i % 17) as i64 - 8) * 10_000_000),
            accessed: (i % 3 != 0).then_some(((i % 23) as i64 - 11) * 10_000_000),
            attributes: None,
        };
        let id = builder.add_with_metadata(ROOT_PARENT, name.as_bytes(), 0, metadata);
        rows.push((id, name, metadata));
    }
    let index = builder.freeze();
    for key in [
        SortKey::Name,
        SortKey::Size,
        SortKey::Modified,
        SortKey::Created,
        SortKey::Accessed,
    ] {
        if key != SortKey::Name {
            index.request_sort(key);
            assert_eq!(
                index
                    .sort_ticket(key)
                    .unwrap()
                    .wait_timeout(std::time::Duration::from_secs(2)),
                ferrow_query::SortStatus::Ready
            );
        }
        for descending in [false, true] {
            for needle in ["", "a", "19", "zz-no-hit"] {
                let mut expected: Vec<_> = rows
                    .iter()
                    .filter(|r| r.1.to_ascii_lowercase().contains(needle))
                    .collect();
                expected.sort_by(|a, b| {
                    let by_name =
                        a.1.to_ascii_lowercase()
                            .cmp(&b.1.to_ascii_lowercase())
                            .then(a.1.cmp(&b.1))
                            .then(a.0.cmp(&b.0));
                    if key == SortKey::Name {
                        return if descending {
                            by_name.reverse()
                        } else {
                            by_name
                        };
                    }
                    let value = |m: EntryMetadata| match key {
                        SortKey::Size => m.size.map(i128::from),
                        SortKey::Modified => m.modified.map(i128::from),
                        SortKey::Created => m.created.map(i128::from),
                        SortKey::Accessed => m.accessed.map(i128::from),
                        _ => unreachable!(),
                    };
                    let primary = match (value(a.2), value(b.2)) {
                        (Some(a), Some(b)) => {
                            if descending {
                                b.cmp(&a)
                            } else {
                                a.cmp(&b)
                            }
                        }
                        (Some(_), None) => std::cmp::Ordering::Less,
                        (None, Some(_)) => std::cmp::Ordering::Greater,
                        _ => std::cmp::Ordering::Equal,
                    };
                    primary.then(by_name)
                });
                for limit in [None, Some(0), Some(1), Some(37), Some(3000)] {
                    let result = search(
                        &index,
                        &Query::parse(needle).unwrap(),
                        &SearchOptions {
                            sort: key,
                            descending,
                            limit,
                            ..SearchOptions::default()
                        },
                    );
                    assert_eq!(result.sort_unavailable, None);
                    assert!(!result.cancelled);
                    assert_eq!(
                        result.hits,
                        expected
                            .iter()
                            .take(limit.unwrap_or(usize::MAX))
                            .map(|r| r.0)
                            .collect::<Vec<_>>(),
                        "{key:?} {descending} {needle} {limit:?}"
                    );
                }
                let result = search(
                    &index,
                    &Query::default(),
                    &SearchOptions {
                        sort: key,
                        descending,
                        cancel: CancelToken::cancelled(),
                        ..SearchOptions::default()
                    },
                );
                assert!(result.cancelled);
                assert!(result.hits.is_empty());
                assert_eq!(result.visited, 0);
            }
        }
    }
}

#[test]
fn unavailable_column_is_explicit_and_available_order_stops_early() {
    let absent = IndexBuilder::new().freeze();
    let result = search(
        &absent,
        &Query::default(),
        &SearchOptions {
            sort: SortKey::Created,
            ..SearchOptions::default()
        },
    );
    assert_eq!(result.sort_unavailable, Some(SortKey::Created));
    // The scan admits one block per worker ahead of the completed prefix,
    // so "stops early" needs more blocks than workers (32 threads locally).
    let n = (20_000).max((rayon::current_num_threads() + 2) * ferrow_query::BLOCK) as i64;
    let b = IndexBuilder::with_columns(ColumnSet::MODIFIED);
    for i in 0..n {
        b.add_with_metadata(
            ROOT_PARENT,
            b"hit",
            0,
            EntryMetadata {
                modified: Some(i * 10_000_000),
                ..EntryMetadata::default()
            },
        );
    }
    let index = b.freeze();
    index.request_sort(SortKey::Modified);
    assert_eq!(
        index
            .sort_ticket(SortKey::Modified)
            .unwrap()
            .wait_timeout(std::time::Duration::from_secs(2)),
        ferrow_query::SortStatus::Ready
    );
    let result = search(
        &index,
        &Query::default(),
        &SearchOptions {
            sort: SortKey::Modified,
            descending: true,
            limit: Some(10),
            ..SearchOptions::default()
        },
    );
    assert_eq!(
        result.hits,
        (n as u32 - 10..n as u32).rev().collect::<Vec<_>>()
    );
    assert!(result.visited < index.len());
}

#[test]
fn descending_equal_and_unknown_groups_cross_chunk_boundaries() {
    let b = IndexBuilder::with_columns(ColumnSet::MODIFIED);
    let mut expected = Vec::new();
    for i in 0..20_001 {
        let name = format!("item-{:05}", 20_001 - i);
        let value = if i % 3 == 0 {
            None
        } else {
            Some((i % 2) * 10_000_000)
        };
        let id = b.add_with_metadata(
            ROOT_PARENT,
            name.as_bytes(),
            0,
            EntryMetadata {
                modified: value,
                ..Default::default()
            },
        );
        expected.push((id, name, value));
    }
    expected.sort_by(|a, b| {
        match (a.2, b.2) {
            (Some(a), Some(b)) => b.cmp(&a),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            _ => std::cmp::Ordering::Equal,
        }
        .then(a.1.cmp(&b.1))
    });
    let index = b.freeze();
    index.request_sort(SortKey::Modified);
    assert_eq!(
        index
            .sort_ticket(SortKey::Modified)
            .unwrap()
            .wait_timeout(std::time::Duration::from_secs(2)),
        ferrow_query::SortStatus::Ready
    );
    for limit in [None, Some(4200), Some(17_000)] {
        let result = search(
            &index,
            &Query::default(),
            &SearchOptions {
                sort: SortKey::Modified,
                descending: true,
                limit,
                ..Default::default()
            },
        );
        assert_eq!(
            result.hits,
            expected
                .iter()
                .take(limit.unwrap_or(usize::MAX))
                .map(|r| r.0)
                .collect::<Vec<_>>()
        );
    }
}
