//! Query parser and parallel name matcher.
//!
//! Grammar (M1):
//! - terms separated by whitespace mean AND;
//! - `!term` means NOT;
//! - `"quoted phrase"` is one literal term (spaces included);
//! - matching is case-insensitive by default; `case:term` (also after
//!   `!`, e.g. `!case:Foo`) makes one term case-sensitive.
//!
//! Matching is a substring test on the entry name only. The parser never
//! panics on any input.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ferrow_index::{Index, fold_name};
use memchr::memmem;
use rayon::prelude::*;

/// One search term.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    pub text: String,
    pub negated: bool,
    pub case_sensitive: bool,
}

/// A parsed query: all terms must match (AND).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub terms: Vec<Term>,
}

impl Query {
    /// Parse a query string. An empty (or whitespace-only) query has no
    /// terms and matches every entry.
    pub fn parse(input: &str) -> Result<Query, ParseError> {
        let bytes = input.as_bytes();
        let mut terms = Vec::new();
        let mut i = 0usize;
        while i < bytes.len() {
            // Skip separators (ASCII whitespace; non-ASCII bytes form terms).
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= bytes.len() {
                break;
            }
            let term_start = i;
            let mut negated = false;
            let mut case_sensitive = false;
            if bytes[i] == b'!' {
                negated = true;
                i += 1;
            }
            if input[i..].starts_with("case:") {
                case_sensitive = true;
                i += "case:".len();
            }
            let text: String;
            if i < bytes.len() && bytes[i] == b'"' {
                let quote_at = i;
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += 1;
                }
                if i >= bytes.len() {
                    return Err(ParseError::UnclosedQuote { at: quote_at });
                }
                text = input[start..i].to_string();
                i += 1; // closing quote
            } else {
                let start = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                text = input[start..i].to_string();
            }
            if text.is_empty() {
                return Err(ParseError::EmptyTerm { at: term_start });
            }
            terms.push(Term {
                text,
                negated,
                case_sensitive,
            });
        }
        Ok(Query { terms })
    }
}

/// Precise parse failures. `at` is a byte offset into the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    UnclosedQuote { at: usize },
    EmptyTerm { at: usize },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::UnclosedQuote { at } => {
                write!(f, "unclosed quote starting at position {at}")
            }
            ParseError::EmptyTerm { at } => write!(f, "empty search term at position {at}"),
        }
    }
}
impl std::error::Error for ParseError {}

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

/// Search options.
#[derive(Clone, Default)]
pub struct SearchOptions {
    /// Stop after this many hits (in name-sort order).
    pub limit: Option<usize>,
    /// Cancellation flag shared with the caller.
    pub cancel: CancelToken,
    /// Test hook invoked after each visited block with the block
    /// index, before the next block starts. Only present with the
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
/// cancellation the hits found so far (in order) are still returned.
/// `visited` counts the entries actually examined, so callers (and
/// tests) can prove that `limit`/cancellation stopped the scan early.
#[derive(Debug)]
pub struct SearchResult {
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

struct Matcher {
    finders: Vec<(memmem::Finder<'static>, bool, bool)>, // finder, negated, case_sensitive
}

impl Matcher {
    fn new(query: &Query) -> Matcher {
        let finders = query
            .terms
            .iter()
            .map(|t| {
                let needle: Vec<u8> = if t.case_sensitive {
                    t.text.as_bytes().to_vec()
                } else {
                    let mut folded = Vec::new();
                    fold_name(t.text.as_bytes(), &mut folded);
                    folded
                };
                (
                    memmem::Finder::new(&needle).into_owned(),
                    t.negated,
                    t.case_sensitive,
                )
            })
            .collect();
        Matcher { finders }
    }

    fn matches(&self, index: &Index, id: u32) -> bool {
        self.finders
            .iter()
            .all(|(finder, negated, case_sensitive)| {
                let haystack = if *case_sensitive {
                    index.name(id)
                } else {
                    index.folded_name(id)
                };
                finder.find(haystack).is_some() != *negated
            })
    }
}

/// Entries examined per parallel block; cancellation and the
/// `test-hooks` hook act at this granularity.
const BLOCK_SIZE: usize = 65_536;

/// Block size, exposed only to the test-hooks build so integration
/// tests can assert exact per-block behaviour.
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub const BLOCK: usize = BLOCK_SIZE;

/// Run a search over the whole index, in name-sort order, in parallel.
///
/// With a `limit`, the sorted order is walked in blocks (each block is
/// matched in parallel) and the search stops as soon as enough hits are
/// found, so a first-page query never scans the whole index.
#[cfg_attr(
    not(feature = "test-hooks"),
    allow(
        clippy::unused_enumerate_index,
        reason = "index feeds the test-hooks hook"
    )
)]
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
    let mut timings = QueryTimings::default();
    let matcher = Matcher::new(query);
    let sorted = index.sorted();
    let mut hits: Vec<u32> = Vec::new();
    let mut visited = 0usize;
    if let Some(t) = started {
        timings.prepare = t.elapsed();
    }
    if options.limit == Some(0) {
        if let Some(t) = started {
            timings.total = t.elapsed();
        }
        return (
            SearchResult {
                hits,
                cancelled: options.cancel.is_cancelled(),
                visited,
            },
            timings,
        );
    }
    for (_block_index, block) in sorted.chunks(BLOCK_SIZE).enumerate() {
        if options.cancel.is_cancelled() {
            if let Some(t) = started {
                timings.total = t.elapsed();
            }
            return (
                SearchResult {
                    hits,
                    cancelled: true,
                    visited,
                },
                timings,
            );
        }
        let scan_started = TIMED.then(std::time::Instant::now);
        let mut found: Vec<u32> = block
            .par_iter()
            .filter(|&&id| matcher.matches(index, id))
            .copied()
            .collect();
        if let Some(t) = scan_started {
            timings.scan += t.elapsed();
        }
        let collect_started = TIMED.then(std::time::Instant::now);
        visited += block.len();
        hits.append(&mut found);
        #[cfg(feature = "test-hooks")]
        if let Some(hook) = &options.on_block {
            hook(_block_index);
        }
        if let Some(limit) = options.limit
            && hits.len() >= limit
        {
            hits.truncate(limit);
            if let Some(t) = collect_started {
                timings.collect += t.elapsed();
            }
            break;
        }
        if let Some(t) = collect_started {
            timings.collect += t.elapsed();
        }
        // Without a limit we still walk block by block; each block is a
        // full parallel pass and gives cancellation a chance per block.
    }
    let result = SearchResult {
        hits,
        cancelled: options.cancel.is_cancelled(),
        visited,
    };
    drop(matcher);
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
    let mut timings = QueryTimings::default();
    let matcher = Matcher::new(query);
    if let Some(t) = started {
        timings.prepare = t.elapsed();
    }
    let mut total = 0usize;
    for block in index.sorted().chunks(BLOCK_SIZE) {
        if cancel.is_cancelled() {
            if let Some(t) = started {
                timings.total = t.elapsed();
            }
            return ((total, true), timings);
        }
        let scan_started = TIMED.then(std::time::Instant::now);
        total += block
            .par_iter()
            .filter(|&&id| matcher.matches(index, id))
            .count();
        if let Some(t) = scan_started {
            timings.scan += t.elapsed();
        }
    }
    let result = (total, cancel.is_cancelled());
    drop(matcher);
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
            q.terms,
            vec![
                Term {
                    text: "report".into(),
                    negated: false,
                    case_sensitive: false
                },
                Term {
                    text: "2024".into(),
                    negated: false,
                    case_sensitive: false
                },
            ]
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
