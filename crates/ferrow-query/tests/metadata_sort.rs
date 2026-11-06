use ferrow_index::{ColumnSet, EntryMetadata, IndexBuilder, ROOT_PARENT};
use ferrow_query::{CancelToken, Query, SearchOptions, SortKey, search};

#[test]
fn every_sort_matches_independent_oracle_with_name_ties_and_missing_values() {
    let builder = IndexBuilder::with_columns(ColumnSet::ALL);
    let mut rows = Vec::new();
    for i in 0..2049 {
        let name = format!("{}{:04}", if i % 2 == 0 { "a" } else { "Z" }, 2049 - i);
        let metadata = EntryMetadata {
            size: (i % 7 != 0).then_some((i % 13) as u64),
            modified: (i % 11 != 0).then_some((i % 19) as i64 - 9),
            created: (i % 5 != 0).then_some((i % 17) as i64 - 8),
            accessed: (i % 3 != 0).then_some((i % 23) as i64 - 11),
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
    let b = IndexBuilder::with_columns(ColumnSet::MODIFIED);
    for i in 0..20_000 {
        b.add_with_metadata(
            ROOT_PARENT,
            b"hit",
            0,
            EntryMetadata {
                modified: Some(i),
                ..EntryMetadata::default()
            },
        );
    }
    let index = b.freeze();
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
    assert_eq!(result.hits, (19_990..20_000).rev().collect::<Vec<_>>());
    assert!(result.visited < index.len());
}
