//! Property check: the parallel matcher must equal a naive, sequential
//! reference matcher on a 100k-entry fixture, for many generated queries,
//! with and without a limit. Cancellation is checked too.

use ferrow_fixture::{Rng, generate};
use ferrow_index::{Index, fold_name};
use ferrow_query::{CancelToken, Query, SearchOptions, count, search};

/// Naive substring search (no memchr), the reference for `memmem`.
fn naive_contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn reference_search(index: &Index, query: &Query, limit: Option<usize>) -> Vec<u32> {
    let mut hits = Vec::new();
    'entries: for &id in index.sorted() {
        for term in &query.terms {
            let found = if term.case_sensitive {
                naive_contains(index.name(id), term.text.as_bytes())
            } else {
                let mut needle = Vec::new();
                fold_name(term.text.as_bytes(), &mut needle);
                naive_contains(index.folded_name(id), &needle)
            };
            if found == term.negated {
                continue 'entries;
            }
        }
        hits.push(id);
        if let Some(limit) = limit
            && hits.len() >= limit
        {
            break;
        }
    }
    hits
}

fn random_query(rng: &mut Rng, index: &Index) -> String {
    const NEEDLES: &[&str] = &[
        "a", "re", "report", "2024", "log", "tmp", "photo", "zzzznope", "txt", ".rs", "img",
        "über", "ärger", "文書", "Report", "DATA",
    ];
    let mut parts = Vec::new();
    for _ in 0..1 + rng.below(3) {
        // Half the time, carve a needle out of a real name so hits are common.
        let needle: String = if rng.chance(1, 2) {
            let id = rng.below(index.len()) as u32;
            let name = String::from_utf8_lossy(index.name(id)).into_owned();
            let clean: String = name
                .chars()
                .filter(|c| !c.is_whitespace() && *c != '"')
                .collect();
            if clean.chars().count() > 3 {
                clean.chars().take(1 + rng.below(4)).collect()
            } else {
                clean
            }
        } else {
            rng.pick(NEEDLES).to_string()
        };
        if needle.is_empty() {
            continue;
        }
        let mut part = String::new();
        if rng.chance(1, 4) {
            part.push('!');
        }
        if rng.chance(1, 5) {
            part.push_str("case:");
        }
        if rng.chance(1, 6) {
            part.push('"');
            part.push_str(&needle);
            part.push('"');
        } else {
            part.push_str(&needle);
        }
        parts.push(part);
    }
    parts.join(" ")
}

#[test]
fn parallel_matcher_equals_reference_on_100k() {
    let index = generate(100_000, 2026);
    let mut rng = Rng::new(99);
    for round in 0..120 {
        let text = random_query(&mut rng, &index);
        let query = match Query::parse(&text) {
            Ok(q) => q,
            Err(e) => panic!("generated query {text:?} should parse: {e}"),
        };
        let limit = if round % 2 == 0 {
            None
        } else {
            Some(1 + rng.below(200))
        };
        let got = search(
            &index,
            &query,
            &SearchOptions {
                limit,
                ..SearchOptions::default()
            },
        );
        assert!(!got.cancelled);
        let want = reference_search(&index, &query, limit);
        assert_eq!(got.hits, want, "query {text:?} limit {limit:?}");
        if limit.is_none() {
            let (n, cancelled) = count(&index, &query, &CancelToken::new());
            assert_eq!(n, want.len(), "count for query {text:?}");
            assert!(!cancelled);
        }
    }
}

#[test]
fn fixed_queries_equal_reference() {
    let index = generate(20_000, 5);
    for text in [
        "a",
        "re",
        "report 2024",
        "!tmp log",
        "zzzz-no-hit-zzzz",
        "文書",
        "case:Report",
        "!case:Report 2024",
        "\"report\"",
        "",
    ] {
        let query = Query::parse(text).unwrap();
        for limit in [None, Some(1), Some(7), Some(100)] {
            let got = search(
                &index,
                &query,
                &SearchOptions {
                    limit,
                    ..SearchOptions::default()
                },
            );
            assert_eq!(
                got.hits,
                reference_search(&index, &query, limit),
                "query {text:?} limit {limit:?}"
            );
        }
    }
}

#[test]
fn cancellation_stops_and_is_reported() {
    let index = generate(100_000, 11);
    let query = Query::parse("a").unwrap();
    // Pre-cancelled: nothing is scanned.
    let opts = SearchOptions {
        cancel: CancelToken::cancelled(),
        ..SearchOptions::default()
    };
    let got = search(&index, &query, &opts);
    assert!(got.cancelled);
    assert!(got.hits.is_empty());
    assert_eq!(got.visited, 0);
}

/// Cancellation after the first chunk admits at most one chunk per worker.
/// The result remains an exact prefix of the independent ordered oracle.
#[test]
fn cancellation_after_first_chunk_bounds_admitted_work() {
    let index = generate(100_000, 11);
    assert!(index.len() > ferrow_query::BLOCK, "fixture needs 2+ blocks");
    let query = Query::parse("a").unwrap();
    let token = CancelToken::new();
    let hook_token = token.clone();
    let got = search(
        &index,
        &query,
        &SearchOptions {
            limit: None,
            cancel: token,
            on_block: Some(std::sync::Arc::new(move |block: usize| {
                if block == 0 {
                    hook_token.cancel();
                }
            })),
            ..SearchOptions::default()
        },
    );
    assert!(got.cancelled, "cancellation was not reported");
    assert!(got.visited >= ferrow_query::BLOCK);
    assert!(got.visited <= ferrow_query::BLOCK * rayon::current_num_threads());
    // Correct prefix: reference hits restricted to the first block.
    let first_block: std::collections::HashSet<u32> = index.sorted()[..ferrow_query::BLOCK]
        .iter()
        .copied()
        .collect();
    let want: Vec<u32> = reference_search(&index, &query, None)
        .into_iter()
        .filter(|id| first_block.contains(id))
        .collect();
    assert!(got.hits.starts_with(&want));
    assert_eq!(
        got.hits,
        reference_search(&index, &query, None)[..got.hits.len()]
    );
}

/// `limit` must stop the scan early: the visited counter proves the
/// whole index was not examined for a first-page query.
#[test]
fn limit_stops_early_and_reports_visited() {
    let index = generate(100_000, 3);
    let got = search(
        &index,
        &Query::parse("").unwrap(),
        &SearchOptions {
            limit: Some(10),
            ..SearchOptions::default()
        },
    );
    assert_eq!(got.hits.len(), 10);
    assert!(got.visited <= ferrow_query::BLOCK * rayon::current_num_threads());
    assert!(got.visited < index.len());
    // Without a limit everything is visited.
    let got = search(
        &index,
        &Query::parse("zzzz-no-hit-zzzz").unwrap(),
        &SearchOptions::default(),
    );
    assert_eq!(got.visited, index.len());
    assert!(got.hits.is_empty());
}

/// A channel pauses an admitted stage until a replacement query cancels it.
/// No CPU load or timing race is needed to establish mid-scan cancellation.
#[test]
fn replacement_query_cancels_in_flight_within_two_seconds() {
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::{Duration, Instant};
    let index = Arc::new(generate(20_000, 11));
    let session = ferrow_query::QuerySession::default();
    let old = session.start();
    let (started_tx, started) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let released = Mutex::new(released);
    let (done_tx, done) = mpsc::channel();
    let old_index = index.clone();
    let worker = std::thread::spawn(move || {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap();
        let result = pool.install(|| {
            search(
                &old_index,
                &Query::parse("a").unwrap(),
                &SearchOptions {
                    cancel: old,
                    on_block: Some(Arc::new(move |chunk| {
                        if chunk == 0 {
                            started_tx.send(()).unwrap();
                            let _ = released
                                .lock()
                                .unwrap()
                                .recv_timeout(Duration::from_secs(2));
                        }
                    })),
                    ..SearchOptions::default()
                },
            )
        });
        done_tx.send(result).unwrap();
    });
    started.recv_timeout(Duration::from_secs(2)).unwrap();
    let t = Instant::now();
    let current = session.start();
    release.send(()).unwrap();
    let old_result = done.recv_timeout(Duration::from_secs(2));
    worker.join().unwrap();
    let old_result = old_result.expect("old query did not cancel within 2 seconds");
    assert!(t.elapsed() < Duration::from_secs(2));
    assert!(old_result.cancelled);
    assert!(old_result.visited <= 2 * ferrow_query::BLOCK);
    assert_eq!(
        old_result.hits,
        reference_search(&index, &Query::parse("a").unwrap(), None)[..old_result.hits.len()]
    );
    let result = search(
        &index,
        &Query::parse("re").unwrap(),
        &SearchOptions {
            cancel: current,
            limit: Some(100),
            ..SearchOptions::default()
        },
    );
    assert!(!result.cancelled);
    assert_eq!(
        result.hits,
        reference_search(&index, &Query::parse("re").unwrap(), Some(100))
    );
}
