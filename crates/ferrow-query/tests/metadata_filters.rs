//! `size:`, `dm:`/`dc:`/`da:` and `attrib:` end to end, against a naive
//! oracle over the same entries.
use ferrow_index::{ColumnSet, EntryMetadata, Index, IndexBuilder, ROOT_PARENT, flags};
use ferrow_query::{CancelToken, Query, SearchOptions, SortKey, count, search};

const SECOND: i64 = 10_000_000;

fn now() -> i64 {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    d.as_secs() as i64 * SECOND
}

/// Files with sizes 0, 1 KiB, … and modification times from now back to
/// 40 days ago; one folder; one file with no metadata at all.
fn fixture(columns: ColumnSet) -> (Index, Vec<(String, EntryMetadata, bool)>) {
    let b = IndexBuilder::with_columns(columns);
    let mut entries = Vec::new();
    let now = now();
    for i in 0..40i64 {
        let meta = EntryMetadata {
            size: Some((i as u64) * 1024 * 100),
            modified: Some(now - i * 86_400 * SECOND - 60 * SECOND),
            created: Some(now - 400 * 86_400 * SECOND),
            accessed: None,
            attributes: Some(if i % 3 == 0 { 0x2 | 0x20 } else { 0x20 }),
        };
        let name = format!("file-{i:02}.log");
        b.add_with_metadata(ROOT_PARENT, name.as_bytes(), 0, meta);
        entries.push((name, meta, false));
    }
    let dir = EntryMetadata {
        size: Some(0),
        modified: Some(now),
        ..Default::default()
    };
    b.add_with_metadata(ROOT_PARENT, b"folder", flags::DIR, dir);
    entries.push(("folder".into(), dir, true));
    b.add(ROOT_PARENT, b"bare.log", 0);
    entries.push(("bare.log".into(), EntryMetadata::default(), false));
    (b.freeze(), entries)
}

fn names(index: &Index, query: &str) -> Vec<String> {
    let q = Query::parse(query).unwrap_or_else(|e| panic!("{query}: {e:?}"));
    let mut out: Vec<String> = search(index, &q, &SearchOptions::default())
        .hits
        .iter()
        .map(|&id| String::from_utf8_lossy(index.name(id)).into_owned())
        .collect();
    out.sort();
    out
}

fn expect(
    entries: &[(String, EntryMetadata, bool)],
    keep: impl Fn(&EntryMetadata, bool) -> bool,
) -> Vec<String> {
    let mut out: Vec<String> = entries
        .iter()
        .filter(|(_, m, dir)| keep(m, *dir))
        .map(|(n, _, _)| n.clone())
        .collect();
    out.sort();
    out
}

#[test]
fn size_filters_match_the_oracle() {
    let (index, entries) = fixture(ColumnSet::ALL);
    let kib = 1024u64;
    let file = |m: &EntryMetadata, dir: bool| !dir && m.size.is_some();
    assert_eq!(
        names(&index, "size:>1mb"),
        expect(&entries, |m, d| file(m, d) && m.size.unwrap() > 1024 * kib)
    );
    assert_eq!(
        names(&index, "size:<=200kb"),
        expect(&entries, |m, d| file(m, d) && m.size.unwrap() <= 200 * kib)
    );
    assert_eq!(
        names(&index, "size:500kb..1mb"),
        expect(&entries, |m, d| file(m, d)
            && (500 * kib..=1024 * kib).contains(&m.size.unwrap()))
    );
    assert_eq!(names(&index, "size:300kb"), vec!["file-03.log"]);
    // Folders never match a size filter; entries without a size neither.
    assert!(!names(&index, "size:0").contains(&"folder".to_string()));
    assert!(!names(&index, "size:<=1tb").contains(&"bare.log".to_string()));
}

#[test]
fn date_filters_match_the_oracle() {
    let (index, entries) = fixture(ColumnSet::ALL);
    let now = now();
    let modified_after =
        |t: i64| move |m: &EntryMetadata, _: bool| m.modified.is_some_and(|v| v >= t);
    assert_eq!(
        names(&index, "dm:last3days"),
        expect(&entries, modified_after(now - 3 * 86_400 * SECOND))
    );
    assert_eq!(
        names(&index, "dm:last10minutes"),
        expect(&entries, modified_after(now - 600 * SECOND))
    );
    // Combined with a name pattern and sorted output stays consistent.
    let recent_logs = names(&index, "*.log dm:last2days");
    assert!(recent_logs.iter().all(|n| n.ends_with(".log")));
    assert!(recent_logs.contains(&"file-00.log".to_string()));
    assert!(!recent_logs.contains(&"file-05.log".to_string()));
    // Created is a year ago for every file.
    assert!(names(&index, "dc:last30days").is_empty());
    assert_eq!(names(&index, "dc:<today").len(), 40);
    // Accessed was never recorded: nothing matches.
    assert!(names(&index, "da:<today").is_empty());
}

#[test]
fn attribute_filters_and_negation() {
    let (index, entries) = fixture(ColumnSet::ALL);
    let hidden = expect(&entries, |m, _| m.attributes.is_some_and(|a| a & 2 != 0));
    assert_eq!(names(&index, "attrib:H"), hidden);
    assert_eq!(names(&index, "attrib:HA"), hidden);
    let visible_logs = names(&index, "*.log !attrib:H");
    assert!(visible_logs.iter().all(|n| !hidden.contains(n)));
    assert!(
        visible_logs.contains(&"bare.log".to_string()),
        "no attributes is not hidden"
    );
}

#[test]
fn missing_columns_never_match_and_are_reported() {
    let (index, _) = fixture(ColumnSet::DEFAULT); // size + modified only
    assert!(names(&index, "attrib:A").is_empty());
    assert!(names(&index, "dc:<today").is_empty());
    assert!(!names(&index, "size:>1mb").is_empty());
    let q = Query::parse("dc:today attrib:H size:>1").unwrap();
    let needed = q.required_columns();
    assert!(
        needed.contains(
            ColumnSet::CREATED
                .union(ColumnSet::ATTRIBUTES)
                .union(ColumnSet::SIZE)
        )
    );
    assert!(!index.columns_present().contains(needed));
}

#[test]
fn filter_values_parse_with_comparisons_and_groups() {
    for ok in [
        "size:>10mb",
        "size:>=1.5gb",
        "<size:<1kb | size:>1gb> *.iso",
        "dm:>=2026-01-01",
        "dm:2026-01-01..2026-02-01",
        "dm:..yesterday",
        "!attrib:S;H",
        "\"dm:today\"",
    ] {
        assert!(Query::parse(ok).is_ok(), "{ok}");
    }
    for (bad, at) in [
        ("size:>10zb", 5),
        ("dm:someday", 3),
        ("attrib:Q", 7),
        ("size:", 0),
    ] {
        let err = Query::parse(bad).unwrap_err();
        assert_eq!(err.span().start, at, "{bad}: {err:?}");
    }
}

/// Above the prefilter threshold, rare and broad filters (alone, negated,
/// with a name term, in name and metadata order, with and without a limit)
/// return exactly the oracle's hits in the oracle's order.
#[test]
fn prefiltered_pages_match_the_oracle() {
    const N: u64 = 70_000;
    let b = IndexBuilder::with_columns(ColumnSet::DEFAULT);
    for i in 0..N {
        let size = if i.is_multiple_of(997) {
            7
        } else {
            1_000_000 + i
        };
        let meta = EntryMetadata {
            size: Some(size),
            modified: Some(((i * 7_919) % N) as i64 * SECOND),
            ..Default::default()
        };
        let name = format!(
            "{}-{i:06}.{}",
            ["a", "b"][i as usize % 2],
            ["log", "txt"][i as usize % 3 % 2]
        );
        b.add_with_metadata(ROOT_PARENT, name.as_bytes(), 0, meta);
    }
    let index = b.freeze();
    let names = |hits: &[u32]| -> Vec<String> {
        hits.iter()
            .map(|&id| String::from_utf8_lossy(index.name(id)).into_owned())
            .collect()
    };
    for (query, keep) in [
        (
            "size:<1kb",
            (|i: u64| i.is_multiple_of(997)) as fn(u64) -> bool,
        ),
        ("*.log size:<1kb", |i| {
            i.is_multiple_of(997) && i % 3 % 2 == 0
        }),
        ("!size:>1kb b-", |i| i.is_multiple_of(997) && i % 2 == 1),
        ("size:>1kb", |i| !i.is_multiple_of(997)),
    ] {
        let q = Query::parse(query).unwrap();
        let expected: Vec<u64> = (0..N).filter(|&i| keep(i)).collect();
        let mut by_name: Vec<String> = expected
            .iter()
            .map(|&i| {
                format!(
                    "{}-{i:06}.{}",
                    ["a", "b"][i as usize % 2],
                    ["log", "txt"][i as usize % 3 % 2]
                )
            })
            .collect();
        by_name.sort();
        for limit in [Some(100), None] {
            let result = search(
                &index,
                &q,
                &SearchOptions {
                    limit,
                    ..Default::default()
                },
            );
            assert!(!result.cancelled);
            let want = &by_name[..limit.unwrap_or(usize::MAX).min(by_name.len())];
            assert_eq!(names(&result.hits), want, "{query} {limit:?}");
            let reverse = search(
                &index,
                &q,
                &SearchOptions {
                    limit,
                    descending: true,
                    ..Default::default()
                },
            );
            let mut want: Vec<_> = by_name.iter().rev().cloned().collect();
            want.truncate(limit.unwrap_or(usize::MAX));
            assert_eq!(names(&reverse.hits), want, "{query} {limit:?} descending");
        }
        // Metadata order: modified ascending is (i * 7919) % N.
        index.request_sort(SortKey::Modified);
        assert_eq!(
            index
                .sort_ticket(SortKey::Modified)
                .unwrap()
                .wait_timeout(std::time::Duration::from_secs(30)),
            ferrow_index::SortStatus::Ready
        );
        let mut by_modified = expected.clone();
        by_modified.sort_by_key(|&i| (i * 7_919) % N);
        let result = search(
            &index,
            &q,
            &SearchOptions {
                sort: SortKey::Modified,
                limit: Some(50),
                ..Default::default()
            },
        );
        let got: Vec<u64> = names(&result.hits)
            .iter()
            .map(|n| n[2..8].parse().unwrap())
            .collect();
        assert_eq!(
            got,
            by_modified[..50.min(by_modified.len())],
            "{query} by modified"
        );
        assert_eq!(
            count(&index, &q, &CancelToken::new()).0,
            expected.len(),
            "{query} count"
        );
    }
}

/// A cancelled search with a rare filter returns promptly and empty.
#[test]
fn prefiltered_search_honours_cancellation() {
    let b = IndexBuilder::with_columns(ColumnSet::DEFAULT);
    for i in 0..70_000u64 {
        let meta = EntryMetadata {
            size: Some(1_000_000 + i),
            ..Default::default()
        };
        b.add_with_metadata(ROOT_PARENT, format!("f{i}").as_bytes(), 0, meta);
    }
    let index = b.freeze();
    let cancel = CancelToken::new();
    cancel.cancel();
    let q = Query::parse("size:<1kb").unwrap();
    let result = search(
        &index,
        &q,
        &SearchOptions {
            cancel,
            ..Default::default()
        },
    );
    assert!(result.cancelled && result.hits.is_empty());
}
