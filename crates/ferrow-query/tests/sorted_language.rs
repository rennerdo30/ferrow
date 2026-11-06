use ferrow_index::{ColumnSet, EntryMetadata, IndexBuilder, ROOT_PARENT, flags};
use ferrow_query::{CancelToken, Query, SearchOptions, SortKey, search};

#[test]
fn every_language_form_uses_the_requested_order_and_limit() {
    let b = IndexBuilder::with_columns(ColumnSet::ALL);
    let root = b.add_root(b"root", flags::DIR);
    let docs = b.add(root, b"docs", flags::DIR);
    for i in 0..257 {
        let name = format!(
            "{}-{i:03}.{}",
            ["Report 2024", "report draft", "tmp", "a*b", "文書"][i % 5],
            ["log", "txt", "md"][i % 3]
        );
        b.add_with_metadata(
            docs,
            name.as_bytes(),
            0,
            EntryMetadata {
                size: (i % 7 != 0).then_some((i % 13) as u64),
                modified: (i % 11 != 0).then_some(i as i64 - 300),
                created: (i % 5 != 0).then_some((i % 17) as i64 - 20),
                accessed: (i % 3 != 0).then_some((i % 23) as i64 - 30),
                attributes: None,
            },
        );
    }
    let index = b.freeze();
    let parent_query = format!("parent:\"root{}docs\"", std::path::MAIN_SEPARATOR);
    for key in [
        SortKey::Name,
        SortKey::Size,
        SortKey::Modified,
        SortKey::Created,
        SortKey::Accessed,
    ] {
        index.request_sort(key);
        if let Some(ticket) = index.sort_ticket(key) {
            assert_eq!(
                ticket.wait_timeout(std::time::Duration::from_secs(5)),
                ferrow_index::SortStatus::Ready
            );
        }
        for descending in [false, true] {
            for text in [
                "",
                "report",
                "report 2024",
                "!tmp",
                "*.log",
                "report*2024*",
                "<report|tmp> !draft",
                "case:Report",
                "path:docs report",
                "wholeword:report",
                "file:*.txt",
                "folder:docs",
                "regex:Report.*",
                "ext:txt;md",
                &parent_query,
                "depth:2",
                "\"a*b\"",
                "文?*",
            ] {
                let query = Query::parse(text).unwrap();
                let mut expected = search(&index, &query, &SearchOptions::default()).hits;
                assert!(!expected.is_empty(), "fixture must exercise {text}");
                expected.sort_by(|&a, &b| {
                    let name = || {
                        index
                            .folded_name(a)
                            .cmp(index.folded_name(b))
                            .then(index.name(a).cmp(index.name(b)))
                            .then(a.cmp(&b))
                    };
                    if key == SortKey::Name {
                        return if descending { name().reverse() } else { name() };
                    }
                    let value = |id| {
                        let m = index.metadata(id);
                        match key {
                            SortKey::Size => m.size.map(i128::from),
                            SortKey::Modified => m.modified.map(i128::from),
                            SortKey::Created => m.created.map(i128::from),
                            SortKey::Accessed => m.accessed.map(i128::from),
                            _ => unreachable!(),
                        }
                    };
                    match (value(a), value(b)) {
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
                    }
                    .then_with(name)
                });
                for limit in [None, Some(0), Some(1), Some(7)] {
                    let result = search(
                        &index,
                        &query,
                        &SearchOptions {
                            sort: key,
                            descending,
                            limit,
                            ..SearchOptions::default()
                        },
                    );
                    assert_eq!(result.sort_unavailable, None);
                    assert_eq!(
                        result.hits,
                        expected
                            .iter()
                            .copied()
                            .take(limit.unwrap_or(usize::MAX))
                            .collect::<Vec<_>>(),
                        "{key:?} {descending} {text} {limit:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn sorted_general_queries_stop_early_and_cancel_at_a_completed_prefix() {
    rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .build()
        .unwrap()
        .install(sorted_general_queries_stop_early_and_cancel_at_a_completed_prefix_in_pool);
}

fn sorted_general_queries_stop_early_and_cancel_at_a_completed_prefix_in_pool() {
    let b = IndexBuilder::with_columns(ColumnSet::MODIFIED);
    for i in 0..20_000 {
        b.add_with_metadata(
            ROOT_PARENT,
            format!("hit-{i:05}.log").as_bytes(),
            0,
            EntryMetadata {
                modified: Some(i - 30_000),
                ..EntryMetadata::default()
            },
        );
    }
    let index = b.freeze();
    index.request_sort(SortKey::Modified);
    let ticket = index.sort_ticket(SortKey::Modified).unwrap();
    assert_eq!(
        ticket.wait_timeout(std::time::Duration::from_secs(5)),
        ferrow_index::SortStatus::Ready
    );
    for text in ["hit*", "<hit|missing> !tmp", "regex:hit"] {
        let query = Query::parse(text).unwrap();
        let result = search(
            &index,
            &query,
            &SearchOptions {
                sort: SortKey::Modified,
                descending: true,
                limit: Some(10),
                ..SearchOptions::default()
            },
        );
        assert_eq!(result.hits, (19_990..20_000).rev().collect::<Vec<_>>());
        assert!(result.visited < index.len());
        let token = CancelToken::new();
        let owner = token.clone();
        let result = search(
            &index,
            &query,
            &SearchOptions {
                sort: SortKey::Modified,
                descending: true,
                cancel: token,
                on_block: Some(std::sync::Arc::new(move |block| {
                    if block == 0 {
                        owner.cancel();
                    }
                })),
                ..SearchOptions::default()
            },
        );
        assert!(result.cancelled);
        assert!(result.visited < index.len());
        assert!(!result.hits.is_empty());
        assert_eq!(
            result.hits,
            (20_000 - result.hits.len() as u32..20_000)
                .rev()
                .collect::<Vec<_>>()
        );
    }
}
