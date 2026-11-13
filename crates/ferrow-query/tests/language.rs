use ferrow_index::{Index, IndexBuilder, flags};
use ferrow_query::{Query, SearchOptions, search};

fn sample() -> Index {
    let b = IndexBuilder::new();
    let root = b.add_root(b"root", flags::DIR);
    let docs = b.add(root, b"docs", flags::DIR);
    for name in [
        "Report 2024.txt",
        "report draft.txt",
        "tmp.log",
        "a*b",
        "Äσ文.log",
    ] {
        b.add(docs, name.as_bytes(), 0);
    }
    b.add(root, b"report_2024.log", 0);
    b.freeze()
}

/// Deliberately simple dynamic-programming oracle; no memmem or production glob.
fn wildcard(pattern: &[u8], name: &[u8]) -> bool {
    let mut rows = vec![vec![false; name.len() + 1]; pattern.len() + 1];
    rows[0][0] = true;
    for p in 0..pattern.len() {
        for n in 0..=name.len() {
            if pattern[p] == b'*' {
                rows[p + 1][n] = rows[p][n] || (n > 0 && rows[p + 1][n - 1]);
            } else if rows[p][n] && n < name.len() {
                if pattern[p] == b'?' {
                    for width in 1..=4.min(name.len() - n) {
                        if std::str::from_utf8(&name[n..n + width])
                            .is_ok_and(|s| s.chars().count() == 1)
                        {
                            rows[p + 1][n + width] = true;
                            break;
                        }
                    }
                } else if pattern[p] == name[n] {
                    rows[p + 1][n + 1] = true;
                }
            }
        }
    }
    rows[pattern.len()][name.len()]
}

fn fold(bytes: &[u8], case: bool) -> Vec<u8> {
    let mut out = bytes.to_vec();
    if !case {
        ferrow_index::fold_name(bytes, &mut out);
    }
    out
}
fn word_edges(hay: &[u8], start: usize, end: usize) -> bool {
    let is_word = regex_syntax::is_word_character;
    std::str::from_utf8(&hay[..start])
        .ok()
        .and_then(|s| s.chars().last())
        .is_none_or(|c| !is_word(c))
        && std::str::from_utf8(&hay[end..])
            .ok()
            .and_then(|s| s.chars().next())
            .is_none_or(|c| !is_word(c))
}

fn oracle(index: &Index, query: &Query) -> Vec<u32> {
    use ferrow_query::{Expr, Filter, Pattern};
    let needles: Vec<_> = query
        .terms
        .iter()
        .map(|t| fold(t.text.as_bytes(), t.case_sensitive))
        .collect();
    let regexes: Vec<_> = query
        .terms
        .iter()
        .map(|t| {
            if matches!(t.pattern, Pattern::Regex(_)) {
                Some(
                    regex::bytes::RegexBuilder::new(&t.text)
                        .case_insensitive(!t.case_sensitive)
                        .build()
                        .unwrap(),
                )
            } else {
                None
            }
        })
        .collect();
    fn eval(
        expr: &Expr,
        query: &Query,
        index: &Index,
        id: u32,
        needles: &[Vec<u8>],
        regexes: &[Option<regex::bytes::Regex>],
    ) -> bool {
        match expr {
            Expr::And(v) => v
                .iter()
                .all(|e| eval(e, query, index, id, needles, regexes)),
            Expr::Or(v) => v
                .iter()
                .any(|e| eval(e, query, index, id, needles, regexes)),
            Expr::Not(e) => !eval(e, query, index, id, needles, regexes),
            Expr::Term(i) => {
                let term = &query.terms[*i];
                let name = index.name(id);
                let mut path = Vec::new();
                if term.path {
                    index.path_bytes(id, &mut path);
                }
                let raw = if term.path { &path } else { name };
                let text = fold(raw, term.case_sensitive);
                let directory = index.flags(id) & flags::DIR != 0;
                let found = term.directory.is_none_or(|d| d == directory)
                    && match &term.pattern {
                        Pattern::Literal | Pattern::WordLiteral(_) => {
                            text.windows(needles[*i].len()).enumerate().any(|(at, w)| {
                                w == needles[*i]
                                    && (!term.wholeword || word_edges(&text, at, at + w.len()))
                            })
                        }
                        Pattern::Wildcard => wildcard(&needles[*i], &text),
                        Pattern::Regex(_) => {
                            let regex = regexes[*i].as_ref().unwrap();
                            if term.wholeword {
                                // Naive oracle tries each scalar boundary, independently
                                // of production's compiled half-boundary assertions.
                                (0..=raw.len())
                                    .filter(|at| std::str::from_utf8(&raw[..*at]).is_ok())
                                    .any(|at| {
                                        regex
                                            .find_at(raw, at)
                                            .is_some_and(|m| word_edges(raw, m.start(), m.end()))
                                    })
                            } else {
                                regex.is_match(raw)
                            }
                        }
                        Pattern::EntryType => true,
                        Pattern::Filter(Filter::Depth(want)) => {
                            let mut depth = 0;
                            let mut cur = id;
                            while index.parent(cur) != ferrow_index::ROOT_PARENT {
                                depth += 1;
                                cur = index.parent(cur);
                            }
                            depth == *want
                        }
                        Pattern::Filter(Filter::Parent(parent)) => {
                            let p = index.parent(id);
                            if p == ferrow_index::ROOT_PARENT {
                                false
                            } else {
                                index.path_bytes(p, &mut path);
                                fold(&path, term.case_sensitive)
                                    == fold(parent.as_bytes(), term.case_sensitive)
                            }
                        }
                        Pattern::Filter(Filter::Extension(exts)) => {
                            !directory
                                && name.iter().rposition(|b| *b == b'.').is_some_and(|dot| {
                                    dot > 0
                                        && dot + 1 < name.len()
                                        && exts.iter().any(|e| {
                                            fold(&name[dot + 1..], term.case_sensitive)
                                                == fold(e.as_bytes(), term.case_sensitive)
                                        })
                                })
                        }
                        Pattern::Filter(Filter::Metadata(_)) => {
                            panic!("reserved filters must not parse")
                        }
                    };
                found != term.negated
            }
        }
    }
    index
        .sorted()
        .iter()
        .copied()
        .filter(|id| {
            query
                .expression
                .as_ref()
                .is_none_or(|e| eval(e, query, index, *id, &needles, &regexes))
        })
        .collect()
}

#[test]
fn every_language_feature_equals_reference_on_100k() {
    let index = ferrow_fixture::generate(100_000, 2026);
    let mut queries = vec![
        "*",
        "**",
        "*.log",
        "report*",
        "*2024*",
        "report*2024*",
        "a*b*c",
        "?.txt",
        "*?文*",
        "*Σ*",
        "case:*Report*",
        "file:ext:txt;md",
        "folder:",
        "depth:2",
        "parent:root",
        "path:docs report",
        "case:path:*Documents*",
        "wholeword:report",
        "regex:\"^(report|文書).*\"",
        "case:regex:^Report",
        "wholeword:regex:report",
        "<a|re> !tmp",
        "!<*.log|tmp> report",
        "<folder:|file:ext:txt> !case:TMP",
        "\"*\"",
        // Rare plain literals take the id-order prefilter path.
        "zzzz-no-hit-zzzz",
        "report zzzz",
        "!tmp zzzz",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    let mut rng = ferrow_fixture::Rng::new(211);
    for _ in 0..16 {
        let id = rng.below(index.len()) as u32;
        let name = String::from_utf8_lossy(index.name(id));
        let chars: Vec<_> = name.chars().collect();
        if chars.len() >= 3 {
            queries.push(format!("{}*{}", chars[0], chars.last().unwrap()));
            queries.push(format!("?{}*", chars[1]));
            // A whole name as a plain literal: a handful of hits.
            if !name.contains(['*', '?', '"', '|', '!', '<', '>', ' ']) {
                queries.push(name.into_owned());
            }
        }
    }
    for text in queries {
        let query = Query::parse(&text).unwrap();
        let expected = oracle(&index, &query);
        for limit in [None, Some(0), Some(100)] {
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
                expected[..limit.unwrap_or(expected.len()).min(expected.len())],
                "{text}, {limit:?}"
            );
        }
        let (n, cancelled) = ferrow_query::count(&index, &query, &ferrow_query::CancelToken::new());
        assert_eq!(n, expected.len(), "{text}");
        assert!(!cancelled);
    }
}

#[test]
fn paths_parent_cache_word_boundaries_unicode_and_highlights() {
    let index = sample();
    let parent = format!("root{}docs", std::path::MAIN_SEPARATOR);
    let text = format!("parent:\"{parent}\" path:docs");
    assert_eq!(names(&index, &text), names(&index, "depth:2"));
    for text in [
        "a|report !draft",
        "!<tmp|draft>",
        "!!report",
        "wholeword:report",
        "wholeword:文",
        "regex:\"a|report\"",
        "regex:\"(report)?draft\"",
        "file:",
        "!folder:",
        "case:folder:DOCS",
    ] {
        let q = Query::parse(text).unwrap();
        assert_eq!(
            search(&index, &q, &SearchOptions::default()).hits,
            oracle(&index, &q),
            "{text}"
        );
    }
    use ferrow_query::highlight_ranges;
    assert_eq!(
        highlight_ranges(&Query::parse("report !draft").unwrap(), b"Report draft.txt"),
        vec![0..6]
    );
    assert_eq!(
        highlight_ranges(&Query::parse("*.txt").unwrap(), b"Report.txt"),
        vec![0..10]
    );
    assert!(highlight_ranges(&Query::parse("!<report|path:draft>").unwrap(), b"report").is_empty());
    assert_eq!(
        highlight_ranges(&Query::parse("σ").unwrap(), "ÄΣ.txt".as_bytes()),
        vec![2..4]
    );
}

#[test]
fn raw_names_scalar_questions_and_prefix_pruning_are_sound() {
    let b = IndexBuilder::new();
    let root = b.add_root("Σ".as_bytes(), flags::DIR);
    for name in [
        b"a".as_slice(),
        b"ab",
        b"aab",
        b"abab",
        "Ä文".as_bytes(),
        b"\xff",
        b"\xce\xa3\xff",
    ] {
        b.add(root, name, 0);
    }
    let index = b.freeze();
    for text in [
        "*",
        "?",
        "??",
        "*?",
        "?*",
        "a*a",
        "a*ab",
        "*ab*ab",
        "a**?*b",
        "*文",
        "case:Σ*",
        "path:σ",
        "path:Σ",
        "<a*|ab*>",
        "<a*|文*>",
        "!a*",
        "!<a*|ab*>",
    ] {
        let q = Query::parse(text).unwrap();
        assert_eq!(
            search(&index, &q, &SearchOptions::default()).hits,
            oracle(&index, &q),
            "{text}"
        );
    }
    let large = ferrow_fixture::generate(100_000, 2026);
    let result = search(
        &large,
        &Query::parse("report*2024*").unwrap(),
        &SearchOptions {
            limit: Some(100),
            ..SearchOptions::default()
        },
    );
    assert!(
        result.visited < large.len() / 10,
        "anchored prefix should skip unrelated names"
    );
}

#[test]
fn wildcard_and_path_cancellation_preserve_the_completed_prefix() {
    use ferrow_query::CancelToken;
    let index = ferrow_fixture::generate(100_000, 2026);
    for text in ["*.log", "path:*"] {
        let query = Query::parse(text).unwrap();
        let token = CancelToken::new();
        let hook = token.clone();
        let result = search(
            &index,
            &query,
            &SearchOptions {
                cancel: token,
                on_block: Some(std::sync::Arc::new(move |block| {
                    if block == 0 {
                        hook.cancel();
                    }
                })),
                ..SearchOptions::default()
            },
        );
        assert!(result.cancelled);
        assert!(result.visited <= ferrow_query::BLOCK * rayon::current_num_threads());
        let expected = oracle(&index, &query);
        assert_eq!(result.hits, expected[..result.hits.len()], "{text}");
        let cancelled = CancelToken::cancelled();
        let result = search(
            &index,
            &query,
            &SearchOptions {
                cancel: cancelled.clone(),
                ..SearchOptions::default()
            },
        );
        assert_eq!(result.visited, 0);
        assert!(result.hits.is_empty() && result.cancelled);
        assert_eq!(ferrow_query::count(&index, &query, &cancelled), (0, true));
    }
}

#[test]
fn wholeword_retries_overlapping_candidates_and_highlights_them() {
    for (name, literal, span) in [("a--- ", "--", 2..4), ("xa-a-a ", "a-a", 3..6)] {
        let b = IndexBuilder::new();
        b.add_root(name.as_bytes(), 0);
        let index = b.freeze();
        for text in [
            format!("wholeword:{literal}"),
            format!("wholeword:regex:{literal}"),
        ] {
            let query = Query::parse(&text).unwrap();
            assert_eq!(
                search(&index, &query, &SearchOptions::default()).hits,
                vec![0],
                "{text}"
            );
            assert_eq!(
                search(&index, &query, &SearchOptions::default()).hits,
                oracle(&index, &query)
            );
            assert_eq!(
                ferrow_query::highlight_ranges(&query, name.as_bytes()),
                vec![span.clone()],
                "{text}"
            );
            assert_eq!(
                ferrow_query::count(&index, &query, &ferrow_query::CancelToken::new()),
                (1, false)
            );
        }
    }
    let b = IndexBuilder::new();
    b.add_root("été\u{301}".as_bytes(), 0);
    b.add_root("pré été post".as_bytes(), 0);
    b.add_root("été".as_bytes(), 0);
    let index = b.freeze();
    for text in ["wholeword:été", "wholeword:regex:été"] {
        let query = Query::parse(text).unwrap();
        let hits = search(&index, &query, &SearchOptions::default()).hits;
        assert_eq!(hits, vec![1, 2], "{text}");
        assert_eq!(hits, oracle(&index, &query), "{text}");
        assert_eq!(
            ferrow_query::highlight_ranges(&query, "pré été post".as_bytes()),
            vec![5..10]
        );
        assert!(ferrow_query::highlight_ranges(&query, "été\u{301}".as_bytes()).is_empty());
    }
    for text in ["a|b", "regex:a|regex:b", "wholeword:a"] {
        let query = Query::parse(text).unwrap();
        // Stay inside macOS's 1023-byte target bound while exceeding the
        // shared 256-range budget, including the combined two-term case.
        let name = if text.contains('|') {
            "a b ".repeat(200)
        } else {
            "a ".repeat(300)
        };
        let ranges = ferrow_query::highlight_ranges(&query, name.as_bytes());
        assert_eq!(ranges.len(), 256, "{text}");
    }
}

#[test]
fn bounded_regex_on_maximum_path_cancels_and_counts_the_100k_fixture() {
    use ferrow_query::{CancelToken, MAX_REGEX_INPUT_UNITS, count};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    let b = IndexBuilder::new();
    let mut parent = b.add_root(b"AA", flags::DIR);
    let mut length = 2;
    while length + 256 < MAX_REGEX_INPUT_UNITS {
        parent = b.add(parent, &[b'A'; 255], flags::DIR);
        length += 256;
    }
    let leaf = b.add(parent, &vec![b'A'; MAX_REGEX_INPUT_UNITS - length - 1], 0);
    let overlong = b.add(leaf, b"A", 0);
    ferrow_fixture::generate_into(&b, 100_000 - b.len(), 2026);
    let index = b.freeze();
    assert_eq!(index.len(), 100_000);
    let mut path = Vec::new();
    index.path_bytes(leaf, &mut path);
    assert_eq!(path.len(), MAX_REGEX_INPUT_UNITS);
    let query = Query::parse(r#"wholeword:case:path:regex:".*[^A-Z/\\]|[A-Z]""#).unwrap();
    let (n, cancelled) = count(&index, &query, &CancelToken::new());
    assert!(!cancelled && n <= index.len());
    let token = CancelToken::new();
    let stop = token.clone();
    let cancelled_at = Arc::new(Mutex::new(None));
    let timestamp = cancelled_at.clone();
    let result = search(
        &index,
        &query,
        &SearchOptions {
            cancel: token,
            on_block: Some(Arc::new(move |block| {
                if block == 0 {
                    *timestamp.lock().unwrap() = Some(Instant::now());
                    stop.cancel();
                }
            })),
            ..SearchOptions::default()
        },
    );
    assert!(result.cancelled);
    assert!(cancelled_at.lock().unwrap().unwrap().elapsed() <= Duration::from_secs(2));
    let any = Query::parse("path:regex:.").unwrap();
    let result = search(&index, &any, &SearchOptions::default());
    assert!(result.hits.contains(&leaf));
    assert!(
        !result.hits.contains(&overlong),
        "over-limit regex inputs must be refused"
    );
}

fn names(index: &Index, text: &str) -> Vec<String> {
    search(
        index,
        &Query::parse(text).unwrap(),
        &SearchOptions::default(),
    )
    .hits
    .iter()
    .map(|&id| String::from_utf8(index.name(id).to_vec()).unwrap())
    .collect()
}

#[test]
fn wildcard_boolean_modifiers_and_filters() {
    let index = sample();
    for (text, expected) in [
        ("*.log", vec!["report_2024.log", "tmp.log", "Äσ文.log"]),
        ("report*2024*", vec!["Report 2024.txt", "report_2024.log"]),
        ("*draft*", vec!["report draft.txt"]),
        (
            "report*",
            vec!["Report 2024.txt", "report draft.txt", "report_2024.log"],
        ),
        ("???.log", vec!["tmp.log", "Äσ文.log"]),
        ("**.log", vec!["report_2024.log", "tmp.log", "Äσ文.log"]),
        (
            "<*.log|draft> !tmp",
            vec!["report draft.txt", "report_2024.log", "Äσ文.log"],
        ),
        ("ext:txt", vec!["Report 2024.txt", "report draft.txt"]),
        (
            "path:docs report",
            vec!["Report 2024.txt", "report draft.txt"],
        ),
        ("case:report*2024*", vec!["report_2024.log"]),
        ("file:wholeword:draft", vec!["report draft.txt"]),
        ("folder:docs", vec!["docs"]),
        (
            "regex:\"^report.*2024\"",
            vec!["Report 2024.txt", "report_2024.log"],
        ),
        ("depth:1", vec!["docs", "report_2024.log"]),
        ("\"a*b\"", vec!["a*b"]),
    ] {
        assert_eq!(names(&index, text), expected, "{text}");
    }
}

#[test]
fn wholeword_regex_keeps_verbose_mode_comments_valid() {
    let b = IndexBuilder::new();
    b.add_root(b"a foo b", 0);
    b.add_root(b"foobar", 0);
    let index = b.freeze();
    for text in [
        "wholeword:regex:(?x)foo#comment",
        r#"wholeword:regex:"(?x) f o o # spaced""#,
    ] {
        let query = Query::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"));
        let hits = search(&index, &query, &SearchOptions::default()).hits;
        assert_eq!(hits, vec![0], "{text}");
        assert_eq!(hits, oracle(&index, &query), "{text}");
    }
}

#[test]
fn wholeword_regex_keeps_inline_case_flags() {
    let b = IndexBuilder::new();
    b.add_root(b"foo", 0);
    b.add_root(b"FOO", 0);
    b.add_root(b"FOObar", 0);
    b.add_root(b"fooBAR", 0);
    let index = b.freeze();
    let cases: [(&str, Vec<u32>); 4] = [
        (r#"wholeword:regex:"(?-i:foo)""#, vec![0]),
        (r#"wholeword:regex:"(?i:foo)(?-i:bar)""#, vec![2]),
        ("wholeword:regex:foo", vec![0, 1]),
        ("case:wholeword:regex:(?i)foo", vec![0, 1]),
    ];
    for (text, expected) in cases {
        let query = Query::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"));
        let mut hits = search(&index, &query, &SearchOptions::default()).hits;
        hits.sort_unstable();
        assert_eq!(hits, expected, "{text}");
    }
    let query = Query::parse(r#"wholeword:regex:"(?-i:foo)""#).unwrap();
    assert!(ferrow_query::highlight_ranges(&query, b"FOO").is_empty());
    assert_eq!(ferrow_query::highlight_ranges(&query, b"foo"), vec![0..3]);
}

#[test]
fn wholeword_regex_accepts_previously_valid_nesting() {
    let b = IndexBuilder::new();
    b.add_root(b"abbbbbbbb", 0);
    let index = b.freeze();
    let query = Query::parse(r#"case:wholeword:regex:"((((((((ab)?b)?b)?b)?b)?b)?b)?b)?""#)
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(
        search(&index, &query, &SearchOptions::default()).hits,
        vec![0]
    );
    // Genuinely over-limit user nesting is still a parse error.
    let deep = format!("wholeword:regex:{}a{}", "(".repeat(40), ")".repeat(40));
    assert!(Query::parse(&deep).is_err());
}
