use crate::{CancelToken, DateField, Expr, Filter, MetadataFilter, Pattern, Query, Term};
use ferrow_index::{Index, ROOT_PARENT, SortKey, flags, fold_name};
use memchr::memmem;
use std::ops::Range;

fn folded(bytes: &[u8], case: bool) -> Vec<u8> {
    if case {
        bytes.to_vec()
    } else {
        let mut v = Vec::new();
        fold_name(bytes, &mut v);
        v
    }
}
fn finder(bytes: &[u8]) -> memmem::Finder<'static> {
    memmem::Finder::new(bytes).into_owned()
}

/// Maximum regex target length: UTF-16 units on Windows, bytes on Unix.
#[cfg(windows)]
pub const MAX_REGEX_INPUT_UNITS: usize = 32_767;
#[cfg(target_os = "macos")]
pub const MAX_REGEX_INPUT_UNITS: usize = 1_023;
#[cfg(not(any(windows, target_os = "macos")))]
pub const MAX_REGEX_INPUT_UNITS: usize = 4_095;

fn regex_input_allowed(hay: &[u8], cancel: &CancelToken) -> bool {
    if cancel.is_cancelled() {
        return false;
    }
    if hay.len() <= MAX_REGEX_INPUT_UNITS {
        return true;
    }
    #[cfg(windows)]
    {
        if hay.len() > MAX_REGEX_INPUT_UNITS * 3 {
            return false;
        }
        let (mut at, mut units) = (0, 0usize);
        while at < hay.len() {
            if units.is_multiple_of(256) && cancel.is_cancelled() {
                return false;
            }
            let scalar = width(&hay[at..]);
            let surrogate = hay.get(at..at + 3).is_some_and(|b| {
                b[0] == 0xed && (0xa0..=0xbf).contains(&b[1]) && (0x80..=0xbf).contains(&b[2])
            });
            let bytes = scalar.unwrap_or(if surrogate { 3 } else { 1 });
            units += if bytes == 4 { 2 } else { 1 };
            if units > MAX_REGEX_INPUT_UNITS {
                return false;
            }
            at += bytes;
        }
        true
    }
    #[cfg(not(windows))]
    {
        false
    }
}

struct Prepared {
    term: Term,
    needle: Vec<u8>,
    finder: memmem::Finder<'static>,
    pieces: Vec<memmem::Finder<'static>>,
    extensions: Vec<Vec<u8>>,
}
impl Prepared {
    fn new(term: &Term) -> Self {
        let needle = folded(term.text.as_bytes(), term.case_sensitive);
        let pieces = if matches!(term.pattern, Pattern::Wildcard) {
            needle
                .split(|b| *b == b'*' || *b == b'?')
                .filter(|p| !p.is_empty())
                .map(finder)
                .collect()
        } else {
            Vec::new()
        };
        let finder = match &term.pattern {
            Pattern::Regex(r) => finder(&r.prefix),
            _ => finder(&needle),
        };
        Self {
            term: term.clone(),
            needle,
            finder,
            pieces,
            extensions: match &term.pattern {
                Pattern::Filter(Filter::Extension(exts)) => exts
                    .iter()
                    .map(|v| folded(v.as_bytes(), term.case_sensitive))
                    .collect(),
                _ => Vec::new(),
            },
        }
    }
    fn text_match(&self, hay: &[u8], cancel: &CancelToken) -> bool {
        match &self.term.pattern {
            Pattern::Literal => self.finder.find(hay).is_some(),
            Pattern::Wildcard => {
                let prefix = self
                    .needle
                    .split(|b| *b == b'*' || *b == b'?')
                    .next()
                    .unwrap_or_default();
                let suffix = self
                    .needle
                    .rsplit(|b| *b == b'*' || *b == b'?')
                    .next()
                    .unwrap_or_default();
                if !hay.starts_with(prefix) || !hay.ends_with(suffix) {
                    return false;
                }
                let mut at = 0;
                for piece in &self.pieces {
                    let Some(found) = piece.find(&hay[at..]) else {
                        return false;
                    };
                    at += found + piece.needle().len();
                }
                !self.needle.contains(&b'?') || glob(&self.needle, hay, cancel)
            }
            Pattern::WordLiteral(r) | Pattern::Regex(r) => {
                regex_input_allowed(hay, cancel)
                    && self.finder.find(hay).is_some()
                    && r.regex.is_match(hay)
            }
            _ => false,
        }
    }
}

/// Buffers belong to one worker and are reused for every candidate.
#[derive(Default)]
pub(crate) struct Scratch {
    path: Vec<u8>,
    folded: Vec<u8>,
    chain: Vec<u32>,
    ready: Option<u32>,
    component: Vec<u8>,
}
impl Scratch {
    fn path(&mut self, index: &Index, id: u32, cancel: &CancelToken) -> bool {
        if self.ready == Some(id) {
            return true;
        }
        self.path.clear();
        self.folded.clear();
        self.chain.clear();
        let mut cur = id;
        loop {
            if self.chain.len().is_multiple_of(64) && cancel.is_cancelled() {
                return false;
            }
            self.chain.push(cur);
            cur = index.parent(cur);
            if cur == ROOT_PARENT {
                break;
            }
        }
        let sep = std::path::MAIN_SEPARATOR as u8;
        for (i, &part) in self.chain.iter().rev().enumerate() {
            if i.is_multiple_of(64) && cancel.is_cancelled() {
                return false;
            }
            if i > 0 && !self.path.ends_with(&[sep]) {
                self.path.push(sep);
                self.folded.push(sep);
            }
            self.path.extend_from_slice(index.name(part));
            fold_name(index.name(part), &mut self.component);
            self.folded.extend_from_slice(&self.component);
        }
        // One invalid component makes fold_name's full-path contract
        // ASCII-only, including otherwise valid Unicode ancestors.
        if std::str::from_utf8(&self.path).is_err() {
            self.folded.clear();
            for chunk in self.path.chunks(65536) {
                if cancel.is_cancelled() {
                    return false;
                }
                self.folded.extend(chunk.iter().map(u8::to_ascii_lowercase));
            }
        }
        self.ready = Some(id);
        true
    }
}

/// Static dispatch is selected once per operation, outside the entry loop.
pub(crate) enum Matcher {
    Plain(PlainMatcher),
    General(GeneralMatcher),
}
#[cfg(feature = "test-hooks")]
thread_local! { static COMPACT_PLAN: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) }; }
/// Consume this thread's last planner decision. No normal-build dispatch cost.
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub fn take_compact_plan_for_test() -> Option<bool> {
    COMPACT_PLAN.with(|plan| plan.take())
}
fn metadata(filter: &MetadataFilter, index: &Index, id: u32) -> bool {
    match filter {
        MetadataFilter::Size { min, max } => {
            index.flags(id) & flags::DIR == 0
                && index.metadata_value(SortKey::Size, id).is_some_and(|v| {
                    min.is_none_or(|m| v >= i128::from(m)) && max.is_none_or(|m| v <= i128::from(m))
                })
        }
        MetadataFilter::Date { field, start, end } => {
            let key = match field {
                DateField::Modified => SortKey::Modified,
                DateField::Created => SortKey::Created,
                DateField::Accessed => SortKey::Accessed,
            };
            index.metadata_value(key, id).is_some_and(|v| {
                start.is_none_or(|s| v >= i128::from(s)) && end.is_none_or(|e| v < i128::from(e))
            })
        }
        MetadataFilter::Attributes(all) => index.attributes(id).is_some_and(|a| a & all == *all),
    }
}
pub(crate) trait EntryMatcher: Sync {
    const CANCELLABLE_TERM: bool;
    fn candidates<'a>(&self, index: &'a Index) -> &'a [u32] {
        index.sorted()
    }
    fn matches(&self, index: &Index, id: u32, scratch: &mut Scratch, cancel: &CancelToken) -> bool;
    /// Whether the query has conjuncts that read only metadata columns.
    fn has_prefilter(&self) -> bool {
        false
    }
    /// Those conjuncts alone: a necessary condition for `matches`. Reads
    /// only the id-ordered metadata columns, so an id-order pass is cheap.
    fn prefilter(&self, _index: &Index, _id: u32) -> bool {
        true
    }
}
pub(crate) struct PlainMatcher {
    // Preserve main's compact layout and exact memmem loop.
    finders: Vec<(memmem::Finder<'static>, bool, bool)>,
}
impl EntryMatcher for PlainMatcher {
    const CANCELLABLE_TERM: bool = false;
    #[inline]
    fn matches(&self, index: &Index, id: u32, _: &mut Scratch, _: &CancelToken) -> bool {
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
impl Matcher {
    pub(crate) fn new(query: &Query) -> Self {
        let flat = match &query.expression {
            None | Some(Expr::Term(_)) => true,
            Some(Expr::And(v)) => v.iter().all(|e| matches!(e, Expr::Term(_))),
            _ => false,
        };
        if flat
            && query.terms.iter().all(|t| {
                matches!(t.pattern, Pattern::Literal)
                    && !t.path
                    && !t.wholeword
                    && t.directory.is_none()
            })
        {
            #[cfg(feature = "test-hooks")]
            COMPACT_PLAN.with(|plan| plan.set(Some(true)));
            return Self::Plain(PlainMatcher {
                finders: query
                    .terms
                    .iter()
                    .map(|t| {
                        (
                            finder(&folded(t.text.as_bytes(), t.case_sensitive)),
                            t.negated,
                            t.case_sensitive,
                        )
                    })
                    .collect(),
            });
        }
        #[cfg(feature = "test-hooks")]
        COMPACT_PLAN.with(|plan| plan.set(Some(false)));
        Self::General(GeneralMatcher::new(query))
    }
}
pub(crate) struct GeneralMatcher {
    terms: Vec<Prepared>,
    expression: Option<Expr>,
    /// Top-level conjuncts that are (possibly negated) metadata filters.
    prefilter: Vec<Expr>,
}
impl EntryMatcher for GeneralMatcher {
    const CANCELLABLE_TERM: bool = true;
    fn candidates<'a>(&self, index: &'a Index) -> &'a [u32] {
        self.candidates(index)
    }
    fn matches(&self, index: &Index, id: u32, scratch: &mut Scratch, cancel: &CancelToken) -> bool {
        scratch.ready = None;
        self.expression
            .as_ref()
            .is_none_or(|e| self.eval(e, index, id, scratch, cancel))
    }
    fn has_prefilter(&self) -> bool {
        !self.prefilter.is_empty()
    }
    fn prefilter(&self, index: &Index, id: u32) -> bool {
        self.prefilter
            .iter()
            .all(|e| self.metadata_eval(e, index, id))
    }
}
impl GeneralMatcher {
    /// A required anchored name prefix narrows the existing sorted order.
    /// OR keeps only a common prefix; NOT never narrows candidates.
    pub(crate) fn candidates<'a>(&self, index: &'a Index) -> &'a [u32] {
        fn prefix(expr: &Expr, terms: &[Prepared]) -> Option<Vec<u8>> {
            match expr {
                Expr::Term(i) => {
                    let p = &terms[*i];
                    if p.term.negated || p.term.path || !matches!(p.term.pattern, Pattern::Wildcard)
                    {
                        return None;
                    }
                    let end = p
                        .needle
                        .iter()
                        .position(|b| {
                            *b == b'*' || *b == b'?' || (p.term.case_sensitive && !b.is_ascii())
                        })
                        .unwrap_or(0);
                    (end > 0).then(|| folded(&p.needle[..end], false))
                }
                Expr::And(v) => v
                    .iter()
                    .filter_map(|e| prefix(e, terms))
                    .max_by_key(Vec::len),
                Expr::Or(v) => {
                    let mut prefixes = v.iter().map(|e| prefix(e, terms));
                    let mut common = prefixes.next()??;
                    for next in prefixes {
                        let next = next?;
                        let n = common
                            .iter()
                            .zip(next)
                            .take_while(|(a, b)| **a == *b)
                            .count();
                        common.truncate(n);
                    }
                    (!common.is_empty()).then_some(common)
                }
                Expr::Not(_) => None,
            }
        }
        let sorted = index.sorted();
        let Some(prefix) = self
            .expression
            .as_ref()
            .and_then(|e| prefix(e, &self.terms))
        else {
            return sorted;
        };
        let start = sorted.partition_point(|&id| index.folded_name(id) < prefix.as_slice());
        let end = sorted[start..].partition_point(|&id| index.folded_name(id).starts_with(&prefix));
        &sorted[start..start + end]
    }
    pub(crate) fn new(query: &Query) -> Self {
        fn cost(expr: &Expr, terms: &[Term]) -> bool {
            match expr {
                Expr::Term(i) => {
                    terms[*i].path
                        || matches!(
                            terms[*i].pattern,
                            Pattern::Filter(Filter::Parent(_) | Filter::Depth(_))
                        )
                }
                Expr::And(v) | Expr::Or(v) => v.iter().any(|e| cost(e, terms)),
                Expr::Not(e) => cost(e, terms),
            }
        }
        fn order(expr: &mut Expr, terms: &[Term]) {
            match expr {
                Expr::And(v) => {
                    for e in v.iter_mut() {
                        order(e, terms);
                    }
                    v.sort_by_key(|e| cost(e, terms));
                }
                Expr::Or(v) => {
                    for e in v {
                        order(e, terms);
                    }
                }
                Expr::Not(e) => order(e, terms),
                _ => (),
            }
        }
        fn metadata_only(expr: &Expr, terms: &[Term]) -> bool {
            match expr {
                Expr::Term(i) => matches!(terms[*i].pattern, Pattern::Filter(Filter::Metadata(_))),
                Expr::Not(e) => metadata_only(e, terms),
                _ => false,
            }
        }
        let mut expression = query.expression.clone();
        if let Some(e) = &mut expression {
            order(e, &query.terms);
        }
        let prefilter = match &expression {
            Some(Expr::And(v)) => v
                .iter()
                .filter(|e| metadata_only(e, &query.terms))
                .cloned()
                .collect(),
            Some(e) if metadata_only(e, &query.terms) => vec![e.clone()],
            _ => Vec::new(),
        };
        Self {
            terms: query.terms.iter().map(Prepared::new).collect(),
            expression,
            prefilter,
        }
    }
    /// `eval` for a (possibly negated) metadata filter term; no scratch.
    fn metadata_eval(&self, expr: &Expr, index: &Index, id: u32) -> bool {
        match expr {
            Expr::Not(e) => !self.metadata_eval(e, index, id),
            Expr::Term(i) => {
                let p = &self.terms[*i];
                let Pattern::Filter(Filter::Metadata(filter)) = &p.term.pattern else {
                    unreachable!("prefilter holds metadata filters only")
                };
                let found = p
                    .term
                    .directory
                    .is_none_or(|d| d == (index.flags(id) & flags::DIR != 0))
                    && metadata(filter, index, id);
                found != p.term.negated
            }
            _ => unreachable!("prefilter holds metadata filters only"),
        }
    }
    fn eval(
        &self,
        expr: &Expr,
        index: &Index,
        id: u32,
        scratch: &mut Scratch,
        cancel: &CancelToken,
    ) -> bool {
        match expr {
            Expr::And(v) => v.iter().all(|e| self.eval(e, index, id, scratch, cancel)),
            Expr::Or(v) => v.iter().any(|e| self.eval(e, index, id, scratch, cancel)),
            Expr::Not(e) => !self.eval(e, index, id, scratch, cancel),
            Expr::Term(i) => {
                let p = &self.terms[*i];
                let found = p
                    .term
                    .directory
                    .is_none_or(|d| d == (index.flags(id) & flags::DIR != 0))
                    && self.term(p, index, id, scratch, cancel);
                found != p.term.negated
            }
        }
    }
    fn term(
        &self,
        p: &Prepared,
        index: &Index,
        id: u32,
        s: &mut Scratch,
        cancel: &CancelToken,
    ) -> bool {
        match &p.term.pattern {
            Pattern::EntryType => true,
            Pattern::Filter(Filter::Metadata(filter)) => metadata(filter, index, id),
            Pattern::Filter(Filter::Extension(_)) => {
                let name = if p.term.case_sensitive {
                    index.name(id)
                } else {
                    index.folded_name(id)
                };
                if index.flags(id) & flags::DIR != 0 {
                    return false;
                }
                let Some(dot) = memchr::memrchr(b'.', name) else {
                    return false;
                };
                dot != 0
                    && dot + 1 < name.len()
                    && p.extensions.iter().any(|ext| name[dot + 1..] == *ext)
            }
            Pattern::Filter(Filter::Parent(_)) => {
                let id = index.parent(id);
                id != ROOT_PARENT
                    && s.path(index, id, cancel)
                    && (if p.term.case_sensitive {
                        &s.path
                    } else {
                        &s.folded
                    }) == &p.needle
            }
            Pattern::Filter(Filter::Depth(want)) => {
                let mut cur = id;
                let mut depth = 0;
                while index.parent(cur) != ROOT_PARENT {
                    if depth > *want || cancel.is_cancelled() {
                        return false;
                    }
                    depth += 1;
                    cur = index.parent(cur);
                }
                depth == *want
            }
            _ => {
                let regex = matches!(p.term.pattern, Pattern::Regex(_));
                let hay = if p.term.path {
                    if !s.path(index, id, cancel) {
                        return false;
                    }
                    if p.term.case_sensitive || regex {
                        &s.path
                    } else {
                        &s.folded
                    }
                } else if p.term.case_sensitive || regex {
                    index.name(id)
                } else {
                    index.folded_name(id)
                };
                p.text_match(hay, cancel)
            }
        }
    }
}

fn width(bytes: &[u8]) -> Option<usize> {
    let n = match *bytes.first()? {
        0..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return None,
    };
    std::str::from_utf8(bytes.get(..n)?).ok().map(|_| n)
}
fn glob(pattern: &[u8], hay: &[u8], cancel: &CancelToken) -> bool {
    let (mut p, mut h, mut star) = (0, 0, None);
    let mut steps = 0usize;
    while h < hay.len() {
        if steps.is_multiple_of(256) && cancel.is_cancelled() {
            return false;
        }
        steps += 1;
        if pattern.get(p) == Some(&b'*') {
            star = Some((p + 1, h));
            p += 1;
            continue;
        }
        if pattern.get(p) == Some(&b'?')
            && let Some(n) = width(&hay[h..])
        {
            p += 1;
            h += n;
            continue;
        }
        if pattern.get(p).is_some_and(|b| *b != b'?' && *b == hay[h]) {
            p += 1;
            h += 1;
            continue;
        }
        let Some((next, retry)) = star else {
            return false;
        };
        h = retry + 1;
        p = next;
        star = Some((next, h));
    }
    while pattern.get(p) == Some(&b'*') {
        p += 1;
    }
    p == pattern.len()
}

/// Positive matching name ranges for visible rows. Path/filter/NOT terms
/// contribute no ranges. Unicode folding preserves original byte offsets.
/// At most 256 candidate ranges per query/row bound regex iteration work.
pub fn highlight_ranges(query: &Query, name: &[u8]) -> Vec<Range<usize>> {
    if !regex_input_allowed(name, &CancelToken::new()) {
        return Vec::new();
    }
    fn visit(expr: &Expr, negated: bool, query: &Query, name: &[u8], out: &mut Vec<Range<usize>>) {
        if out.len() >= 256 {
            return;
        }
        match expr {
            Expr::Not(e) => visit(e, !negated, query, name, out),
            Expr::And(v) | Expr::Or(v) => {
                for e in v {
                    visit(e, negated, query, name, out);
                }
            }
            Expr::Term(i) => {
                let term = &query.terms[*i];
                if negated != term.negated || term.path {
                    return;
                }
                let p = Prepared::new(term);
                let folded = folded(name, term.case_sensitive);
                let hay = if matches!(term.pattern, Pattern::Regex(_)) {
                    name
                } else {
                    &folded
                };
                if !p.text_match(hay, &CancelToken::new()) {
                    return;
                }
                match &term.pattern {
                    Pattern::Literal => out.extend(
                        p.finder
                            .find_iter(hay)
                            .take(256 - out.len())
                            .map(|at| at..at + p.needle.len()),
                    ),
                    Pattern::Wildcard => {
                        if !name.is_empty() {
                            out.push(0..name.len());
                        }
                    }
                    Pattern::WordLiteral(r) | Pattern::Regex(r) => out.extend(
                        r.regex
                            .find_iter(hay)
                            .take(256 - out.len())
                            .map(|m| m.range()),
                    ),
                    _ => (),
                }
            }
        }
    }
    let mut ranges = Vec::new();
    if let Some(e) = &query.expression {
        visit(e, false, query, name, &mut ranges);
    }
    ranges.sort_by_key(|r| (r.start, r.end));
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        if let Some(last) = merged.last_mut()
            && range.start <= last.end
        {
            last.end = last.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrow_index::IndexBuilder;

    #[test]
    fn ordinary_queries_select_the_compact_literal_matcher() {
        for text in [
            "",
            "a",
            "!tmp log",
            "report 2024",
            "case:Report",
            "\"a phrase\"",
            "!case:\"Foo Bar\"",
        ] {
            assert!(
                matches!(
                    Matcher::new(&Query::parse(text).unwrap()),
                    Matcher::Plain(_)
                ),
                "{text}"
            );
        }
        for text in [
            "a|b",
            "*.log",
            "path:docs",
            "wholeword:report",
            "regex:report",
            "file:",
        ] {
            assert!(
                matches!(
                    Matcher::new(&Query::parse(text).unwrap()),
                    Matcher::General(_)
                ),
                "{text}"
            );
        }
    }

    #[test]
    fn name_prefilters_precede_path_work_and_paths_are_cancellable() {
        let b = IndexBuilder::new();
        let root = b.add_root(b"root", flags::DIR);
        let leaf = b.add(root, b"leaf", 0);
        let index = b.freeze();
        let Matcher::General(matcher) = Matcher::new(&Query::parse("path:root absent").unwrap())
        else {
            panic!("path requires general matcher");
        };
        let mut scratch = Scratch::default();
        assert!(!matcher.matches(&index, leaf, &mut scratch, &CancelToken::new()));
        assert!(
            scratch.chain.is_empty(),
            "name rejection must avoid path rebuild"
        );
        assert!(!scratch.path(&index, leaf, &CancelToken::cancelled()));
        assert!(
            scratch.chain.is_empty(),
            "cancellation must stop parent traversal"
        );
        assert!(
            !glob(b"*?a", b"aaaa", &CancelToken::cancelled()),
            "scalar wildcard retry must observe cancellation"
        );
    }
}
