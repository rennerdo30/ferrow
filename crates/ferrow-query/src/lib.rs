//! Query parser and parallel name matcher.
//!
//! Space means AND, `|` means OR, `!` means NOT and `<...>` groups terms.
//! Plain terms are substrings; wildcard terms match the whole name.
//! Quotes protect literal phrases. Modifiers and filters are described in
//! ADR 0004. Parsing and matching are bounded; errors carry byte spans.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use ferrow_index::Index;
pub use ferrow_index::{SortKey, SortStatus};

use rayon::prelude::*;

mod parser;
pub use parser::{
    DateField, Expr, Filter, MetadataFilter, ParseError, Pattern, Query, RegexPlan, Term,
};

/// Shared cancellation flag, checked per chunk while searching.
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    /// Share an existing cancellation owner, for event-driven hosts.
    pub fn from_flag(flag: Arc<AtomicBool>) -> Self {
        Self { flag }
    }

    pub fn new() -> CancelToken {
        CancelToken::default()
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    /// A token that starts out cancelled.
    pub fn cancelled() -> CancelToken {
        let t = CancelToken::new();
        t.cancel();
        t
    }
}

/// Cancellation owner for an interactive query stream. Starting the next
/// query cancels the previous token without waiting for its workers.
#[derive(Default)]
pub struct QuerySession {
    active: Mutex<Option<CancelToken>>,
}

impl QuerySession {
    pub fn start(&self) -> CancelToken {
        let token = CancelToken::new();
        if let Some(previous) = self
            .active
            .lock()
            .expect("query session poisoned")
            .replace(token.clone())
        {
            previous.cancel();
        }
        token
    }
}

/// Search options.
#[derive(Clone, Default)]
pub struct SearchOptions {
    pub sort: SortKey,
    pub descending: bool,
    /// Stop after this many hits (in name-sort order).
    pub limit: Option<usize>,
    /// Cancellation flag shared with the caller.
    pub cancel: CancelToken,
    /// Test hook invoked after a completed chunk with its chunk index.
    /// Other admitted chunks can run concurrently. Only present with the
    /// non-default `test-hooks` feature, so normal builds neither
    /// expose it nor pay for its dispatch.
    #[cfg(feature = "test-hooks")]
    pub on_block: Option<Arc<dyn Fn(usize) + Send + Sync>>,
}

impl fmt::Debug for SearchOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("SearchOptions");
        s.field("limit", &self.limit).field("cancel", &self.cancel);
        #[cfg(feature = "test-hooks")]
        s.field("on_block", &self.on_block.is_some());
        s.finish()
    }
}

/// Search outcome. `hits` are entry ids in name-sort order; on
/// cancellation an exact completed prefix of hits is returned.
/// `visited` counts the entries actually examined, so callers (and
/// tests) can prove that `limit`/cancellation stopped the scan early.
#[derive(Debug)]
pub struct SearchResult {
    /// Pending sorts return the existing name order without waiting.
    pub sort_status: SortStatus,
    /// Requested column was not indexed. No fallback results are returned.
    pub sort_unavailable: Option<SortKey>,
    pub hits: Vec<u32>,
    pub cancelled: bool,
    pub visited: usize,
}

/// Disjoint wall-clock spans from one operation. Select a median-total
/// sample, rather than adding independently selected component medians.
#[derive(Debug, Default, Clone, Copy)]
pub struct QueryTimings {
    pub total: std::time::Duration,
    pub prepare: std::time::Duration,
    pub scan: std::time::Duration,
    pub collect: std::time::Duration,
}

impl QueryTimings {
    pub fn unattributed(&self) -> std::time::Duration {
        self.total
            .saturating_sub(self.prepare + self.scan + self.collect)
    }
}

mod matcher;
#[cfg(feature = "test-hooks")]
pub use matcher::take_compact_plan_for_test;
use matcher::{EntryMatcher, Matcher, Scratch};
pub use matcher::{MAX_REGEX_INPUT_UNITS, highlight_ranges};

/// Entries admitted per worker chunk; cancellation also runs inside it.
const BLOCK_SIZE: usize = 4_096;
const CANCEL_INTERVAL: usize = 256;

/// Block size, exposed only to the test-hooks build so integration
/// tests can assert exact per-block behaviour.
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub const BLOCK: usize = BLOCK_SIZE;

/// Run a search over the whole index, in name-sort order, in parallel.
///
/// At most one chunk per pool worker is admitted ahead of the completed
/// prefix. A limit stops admission as soon as that prefix has enough hits.
/// Cancellation is checked every 256 entries, including inside a chunk.
pub fn search(index: &Index, query: &Query, options: &SearchOptions) -> SearchResult {
    search_impl::<false>(index, query, options).0
}

/// Instrumented search for CI. The normal entry point compiles timers out.
pub fn search_with_timings(
    index: &Index,
    query: &Query,
    options: &SearchOptions,
) -> (SearchResult, QueryTimings) {
    search_impl::<true>(index, query, options)
}

fn search_impl<const TIMED: bool>(
    index: &Index,
    query: &Query,
    options: &SearchOptions,
) -> (SearchResult, QueryTimings) {
    let started = TIMED.then(std::time::Instant::now);
    let matcher = Matcher::new(query);
    let (result, mut timings) = match &matcher {
        Matcher::Plain(m) => search_prepared::<TIMED, _>(index, m, options, started),
        Matcher::General(m) => search_prepared::<TIMED, _>(index, m, options, started),
    };
    drop(matcher);
    if let Some(t) = started {
        timings.total = t.elapsed();
    }
    (result, timings)
}

fn search_prepared<const TIMED: bool, M: EntryMatcher>(
    index: &Index,
    matcher: &M,
    options: &SearchOptions,
    started: Option<std::time::Instant>,
) -> (SearchResult, QueryTimings) {
    let mut timings = QueryTimings::default();
    let sort_status = index.request_sort(options.sort);
    if sort_status == SortStatus::Unavailable {
        if let Some(t) = started {
            timings.total = t.elapsed();
        }
        return (
            SearchResult {
                hits: Vec::new(),
                cancelled: options.cancel.is_cancelled(),
                visited: 0,
                sort_unavailable: Some(options.sort),
                sort_status,
            },
            timings,
        );
    }
    let metadata_order = index.metadata_order(options.sort, options.descending);
    // Name-prefix candidates are ordered by name. A prepared metadata order
    // addresses the complete index, so its positions must never use that slice.
    let sorted = if metadata_order.is_some() {
        index.sorted()
    } else {
        matcher.candidates(index)
    };
    let reverse = options.sort == SortKey::Name && options.descending;
    let width = rayon::current_num_threads();
    // Only a window of `width` chunks can be admitted ahead of the
    // completed prefix. A slow early chunk cannot accumulate unbounded
    // later results. One parallel operation owns the whole scan.
    struct Ordered {
        next: usize,
        prefix: usize,
        hits: usize,
        stop: bool,
        chunks: Vec<Option<Vec<u32>>>,
    }
    let ordered = Mutex::new(Ordered {
        next: 0,
        prefix: 0,
        hits: 0,
        stop: options.limit == Some(0),
        chunks: (0..sorted.len().div_ceil(BLOCK_SIZE))
            .map(|_| None)
            .collect(),
    });
    let ready = Condvar::new();
    let visited = AtomicUsize::new(0);
    if let Some(t) = started {
        timings.prepare = t.elapsed();
    }
    let scan_started = TIMED.then(std::time::Instant::now);
    (0..width).into_par_iter().for_each(|_| {
        let mut scratch = Scratch::default();
        loop {
            let chunk = {
                let mut state = ordered.lock().expect("query progress poisoned");
                loop {
                    if state.stop
                        || options.cancel.is_cancelled()
                        || state.next == state.chunks.len()
                    {
                        return;
                    }
                    if state.next < state.prefix + width {
                        let next = state.next;
                        state.next += 1;
                        break next;
                    }
                    // An admitted chunk always owns the missing prefix. Its
                    // completion (including cancellation) wakes every waiter.
                    state = ready.wait(state).expect("query progress poisoned");
                }
            };
            let start = chunk * BLOCK_SIZE;
            let block = &sorted[start..(start + BLOCK_SIZE).min(sorted.len())];
            let mut found = Vec::new();
            let mut ordered_ids = Vec::new();
            let mut examined = 0;
            for offset in (0..block.len()).step_by(CANCEL_INTERVAL) {
                if options.cancel.is_cancelled() {
                    break;
                }
                let end = (offset + CANCEL_INTERVAL).min(block.len());
                let ids = if let Some(order) = &metadata_order {
                    ordered_ids.clear();
                    ordered_ids.extend(order.range(start + offset, start + end));
                    ordered_ids.as_slice()
                } else if reverse {
                    ordered_ids.clear();
                    ordered_ids.extend(
                        (start + offset..start + end).map(|p| sorted[sorted.len() - 1 - p]),
                    );
                    ordered_ids.as_slice()
                } else {
                    &sorted[start + offset..start + end]
                };
                for &id in ids {
                    if matcher.matches(index, id, &mut scratch, &options.cancel)
                        && options.limit.is_none_or(|limit| found.len() < limit)
                    {
                        found.push(id);
                    }
                }
                if M::CANCELLABLE_TERM && options.cancel.is_cancelled() {
                    break;
                }
                examined += end - offset;
            }
            visited.fetch_add(examined, Ordering::Relaxed);
            #[cfg(feature = "test-hooks")]
            if examined == block.len()
                && let Some(hook) = &options.on_block
            {
                hook(chunk);
            }
            let mut state = ordered.lock().expect("query progress poisoned");
            if examined == block.len() {
                state.chunks[chunk] = Some(found);
                while state.prefix < state.chunks.len() {
                    let Some(hits) = &state.chunks[state.prefix] else {
                        break;
                    };
                    state.hits += hits.len();
                    state.prefix += 1;
                }
                if options.limit.is_some_and(|limit| state.hits >= limit) {
                    state.stop = true;
                }
            }
            if options.cancel.is_cancelled() {
                state.stop = true;
            }
            ready.notify_all();
        }
    });
    if let Some(t) = scan_started {
        timings.scan = t.elapsed();
    }
    let collect_started = TIMED.then(std::time::Instant::now);
    let state = ordered.into_inner().expect("query progress poisoned");
    // Cancellation returns an exact sorted prefix, dropping partial chunks.
    let mut hits: Vec<u32> = state
        .chunks
        .into_iter()
        .take(state.prefix)
        .flatten()
        .flatten()
        .collect();
    if let Some(limit) = options.limit {
        hits.truncate(limit);
    }
    let visited = visited.into_inner();
    if let Some(t) = collect_started {
        timings.collect = t.elapsed();
    }
    let result = SearchResult {
        sort_status: if metadata_order.is_some() || options.sort == SortKey::Name {
            SortStatus::Ready
        } else {
            sort_status
        },
        sort_unavailable: None,
        hits,
        cancelled: options.cancel.is_cancelled(),
        visited,
    };
    if let Some(t) = started {
        timings.total = t.elapsed();
    }
    (result, timings)
}

/// Count all hits without storing them. Returns the count found and
/// whether the count was cut short by cancellation.
pub fn count(index: &Index, query: &Query, cancel: &CancelToken) -> (usize, bool) {
    count_impl::<false>(index, query, cancel).0
}

pub fn count_with_timings(
    index: &Index,
    query: &Query,
    cancel: &CancelToken,
) -> ((usize, bool), QueryTimings) {
    count_impl::<true>(index, query, cancel)
}

fn count_impl<const TIMED: bool>(
    index: &Index,
    query: &Query,
    cancel: &CancelToken,
) -> ((usize, bool), QueryTimings) {
    let started = TIMED.then(std::time::Instant::now);
    let matcher = Matcher::new(query);
    let (result, mut timings) = match &matcher {
        Matcher::Plain(m) => count_prepared::<TIMED, _>(index, m, cancel, started),
        Matcher::General(m) => count_prepared::<TIMED, _>(index, m, cancel, started),
    };
    drop(matcher);
    if let Some(t) = started {
        timings.total = t.elapsed();
    }
    (result, timings)
}

fn count_prepared<const TIMED: bool, M: EntryMatcher>(
    index: &Index,
    matcher: &M,
    cancel: &CancelToken,
    started: Option<std::time::Instant>,
) -> ((usize, bool), QueryTimings) {
    let mut timings = QueryTimings::default();
    if let Some(t) = started {
        timings.prepare = t.elapsed();
    }
    let scan_started = TIMED.then(std::time::Instant::now);
    let total = (0..index.len().div_ceil(BLOCK_SIZE))
        .into_par_iter()
        .map(|chunk| {
            let mut scratch = Scratch::default();
            let start = chunk * BLOCK_SIZE;
            let end = (start + BLOCK_SIZE).min(index.len());
            let mut total = 0;
            for start in (start..end).step_by(CANCEL_INTERVAL) {
                if cancel.is_cancelled() {
                    break;
                }
                total += (start..(start + CANCEL_INTERVAL).min(end))
                    .filter(|&id| matcher.matches(index, id as u32, &mut scratch, cancel))
                    .count();
            }
            total
        })
        .sum();
    if let Some(t) = scan_started {
        timings.scan = t.elapsed();
    }
    let result = (total, cancel.is_cancelled());
    if let Some(t) = started {
        timings.total = t.elapsed();
    }
    (result, timings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrow_index::{IndexBuilder, flags};

    fn sample() -> Index {
        let b = IndexBuilder::new();
        let root = b.add_root(b"root", flags::DIR);
        b.add(root, b"Report 2024.txt", 0);
        b.add(root, b"report draft.txt", 0);
        b.add(root, b"Budget 2024.xlsx", 0);
        b.add(root, b"notes tmp.log", 0);
        b.add(root, "Ärger gross.txt".as_bytes(), 0);
        b.freeze()
    }

    fn names(idx: &Index, result: &SearchResult) -> Vec<String> {
        result
            .hits
            .iter()
            .map(|&id| String::from_utf8(idx.name(id).to_vec()).unwrap())
            .collect()
    }

    #[test]
    fn parse_grammar() {
        let q = Query::parse("report 2024").unwrap();
        assert_eq!(
            q.terms.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
            ["report", "2024"]
        );
        let q = Query::parse("!tmp log").unwrap();
        assert!(q.terms[0].negated && q.terms[0].text == "tmp");
        assert!(!q.terms[1].negated && q.terms[1].text == "log");
        let q = Query::parse("\"report 2024\"").unwrap();
        assert_eq!(q.terms.len(), 1);
        assert_eq!(q.terms[0].text, "report 2024");
        let q = Query::parse("case:Report").unwrap();
        assert!(q.terms[0].case_sensitive && q.terms[0].text == "Report");
        let q = Query::parse("!case:\"Foo Bar\"").unwrap();
        assert!(q.terms[0].negated && q.terms[0].case_sensitive);
        assert_eq!(q.terms[0].text, "Foo Bar");
        let q = Query::parse("  \t ").unwrap();
        assert!(q.terms.is_empty());
    }

    #[test]
    fn parse_errors_are_precise_and_never_panic() {
        assert_eq!(
            Query::parse("foo \"bar"),
            Err(ParseError::UnclosedQuote { at: 4 })
        );
        assert_eq!(Query::parse("!"), Err(ParseError::EmptyTerm { at: 0 }));
        assert_eq!(
            Query::parse("ok \"\""),
            Err(ParseError::EmptyTerm { at: 3 })
        );
        assert_eq!(Query::parse("case:"), Err(ParseError::EmptyTerm { at: 0 }));
        // Adversarial inputs must not panic.
        for s in [
            "\"",
            "!",
            "case:",
            "!case:",
            "\"\"",
            "!!",
            "a\"b\"c",
            "case:case:",
            "日本語 !",
            "\"\u{10FFFF}\"",
            "casecase:x",
        ] {
            let _ = Query::parse(s);
        }
    }

    #[test]
    fn matches_case_insensitive_by_default() {
        let idx = sample();
        let r = search(
            &idx,
            &Query::parse("REPORT").unwrap(),
            &SearchOptions::default(),
        );
        assert_eq!(names(&idx, &r), ["Report 2024.txt", "report draft.txt"]);
    }

    #[test]
    fn matches_case_sensitive_term() {
        let idx = sample();
        let r = search(
            &idx,
            &Query::parse("case:report").unwrap(),
            &SearchOptions::default(),
        );
        assert_eq!(names(&idx, &r), ["report draft.txt"]);
    }

    #[test]
    fn matches_and_not_and_quotes() {
        let idx = sample();
        let r = search(
            &idx,
            &Query::parse("2024 !budget").unwrap(),
            &SearchOptions::default(),
        );
        assert_eq!(names(&idx, &r), ["Report 2024.txt"]);
        let r = search(
            &idx,
            &Query::parse("\"report 2024\"").unwrap(),
            &SearchOptions::default(),
        );
        assert_eq!(names(&idx, &r), ["Report 2024.txt"]);
        // Without quotes "2024.txt" is one term and still matches 2 files.
        let r = search(
            &idx,
            &Query::parse("2024.txt").unwrap(),
            &SearchOptions::default(),
        );
        assert_eq!(r.hits.len(), 1);
    }

    #[test]
    fn matches_nonascii_case_insensitively() {
        let idx = sample();
        let r = search(
            &idx,
            &Query::parse("ärger GROSS").unwrap(),
            &SearchOptions::default(),
        );
        assert_eq!(names(&idx, &r), ["Ärger gross.txt"]);
    }

    #[test]
    fn limit_truncates_in_sort_order() {
        let idx = sample();
        let opts = SearchOptions {
            limit: Some(2),
            ..SearchOptions::default()
        };
        // Empty query matches everything, in name-sort order.
        let r = search(&idx, &Query::parse("").unwrap(), &opts);
        assert_eq!(r.hits.len(), 2);
        assert_eq!(r.hits[0], idx.sorted()[0]);
        assert_eq!(r.hits[1], idx.sorted()[1]);
    }

    #[test]
    fn default_case_matching_unifies_final_sigma() {
        let b = IndexBuilder::new();
        b.add_root("ς".as_bytes(), 0);
        let idx = b.freeze();
        let r = search(&idx, &Query::parse("Σ").unwrap(), &SearchOptions::default());
        assert_eq!(r.hits.len(), 1);
        let r = search(&idx, &Query::parse("σ").unwrap(), &SearchOptions::default());
        assert_eq!(r.hits.len(), 1);
    }

    #[test]
    fn cancelled_search_reports_it() {
        let idx = sample();
        let opts = SearchOptions {
            cancel: CancelToken::cancelled(),
            ..SearchOptions::default()
        };
        let r = search(&idx, &Query::parse("report").unwrap(), &opts);
        assert!(r.cancelled);
        assert!(r.hits.is_empty());
    }

    #[test]
    fn count_matches_search() {
        let idx = sample();
        let q = Query::parse("2024").unwrap();
        let (n, cancelled) = count(&idx, &q, &CancelToken::new());
        assert_eq!(n, 2);
        assert!(!cancelled);
    }
}
