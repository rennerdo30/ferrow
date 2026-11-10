//! Windows Search SQL (`SystemIndex`) parser and query extractor.
//!
//! Parses `SELECT [TOP <N>] <columns> FROM ["]SystemIndex["] WHERE <predicate>`
//! queries emitted by `SearchHost.exe` (`tquery.dll`) and extracts both the
//! requested projection column list and a clean `ferrow_query::Query` string.

use ferrow_query::Query;

/// Default maximum rows returned to Windows Search when `TOP <N>` is omitted.
pub const DEFAULT_TOP_LIMIT: usize = 30;
/// Upper bound on rows materialized per Start menu query.
pub const MAX_TOP_LIMIT: usize = 100;

/// Parsed Windows Search SQL query ready for Ferrow execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedSql {
    /// Projection column names in ordinal order (1-based in OLE DB).
    pub columns: Vec<String>,
    /// Row limit from `TOP <N>` (clamped to `1..=MAX_TOP_LIMIT`).
    pub limit: usize,
    /// Extracted query string validated with `ferrow_query::Query::parse`.
    pub query: String,
}

/// Extract projection columns and `TOP <N>` limit from a `SystemIndex` SQL query.
pub fn parse_projection(sql: &str) -> Option<(Vec<String>, usize)> {
    let (before_from, _after_from) = split_from_system_index(sql)?;
    let before_from = before_from.trim();
    let mut tokens = before_from.split_whitespace();
    let first = tokens.next()?;
    if !first.eq_ignore_ascii_case("SELECT") {
        return None;
    }
    let second = tokens.next()?;
    let (cols_part, limit) = if second.eq_ignore_ascii_case("TOP") {
        let num_str = tokens.next()?;
        let parsed_limit = num_str.parse::<usize>().ok()?.clamp(1, MAX_TOP_LIMIT);
        let after_select = before_from[first.len()..].trim_start();
        let after_top = after_select[second.len()..].trim_start();
        let rest = after_top[num_str.len()..].trim();
        (rest, parsed_limit)
    } else {
        let rest = before_from[first.len()..].trim();
        (rest, DEFAULT_TOP_LIMIT)
    };

    let cols: Vec<String> = cols_part
        .split(',')
        .map(|c| c.trim().trim_matches('"').to_owned())
        .filter(|c| !c.is_empty() && c != "*")
        .collect();
    if cols.is_empty() {
        None
    } else {
        Some((cols, limit))
    }
}

/// Parse a full Windows Search `SystemIndex` SQL command into projection columns
/// and a validated Ferrow query. Returns `None` for non-`SystemIndex` queries,
/// zero-term queries, or unsupported SQL predicates so the caller can fall back
/// to the native provider.
pub fn parse_windows_search_sql(sql: &str) -> Option<ParsedSql> {
    let (columns, limit) = parse_projection(sql)?;
    let (_before_from, after_from) = split_from_system_index(sql)?;
    let predicate = extract_where_predicate(after_from)?;

    // Boolean meaning must survive translation. Ferrow terms are ANDed,
    // so NOT anywhere (at SQL level or inside a CONTAINS condition) and
    // OR between *different* term sets are unsupported: native answers.
    // The one observed OR shape — the same terms repeated across
    // columns (`CONTAINS(name, ...) OR CONTAINS(*, ...)`) — is kept.
    if contains_word_outside_single_quotes(predicate, "NOT") {
        return None;
    }

    let mut terms: Vec<String> = Vec::new();
    match extract_contains_term_sets(predicate) {
        None => return None,
        Some(sets) if !sets.is_empty() => {
            let mut distinct: Vec<Vec<String>> = Vec::new();
            for set in &sets {
                let mut normalized: Vec<String> =
                    set.iter().map(|t| t.to_ascii_lowercase()).collect();
                normalized.sort();
                if !distinct.contains(&normalized) {
                    distinct.push(normalized);
                }
            }
            if distinct.len() > 1 && contains_word_outside_single_quotes(predicate, "OR") {
                return None;
            }
            for set in sets {
                for term in set {
                    push_unique_term(&mut terms, term);
                }
            }
        }
        Some(_) => {
            terms = extract_like_terms(predicate);
            if terms.len() > 1 && contains_word_outside_single_quotes(predicate, "OR") {
                return None;
            }
        }
    }
    if terms.is_empty() {
        return None;
    }

    let only_folders = is_folder_only_kind(predicate);
    let query = format_ferrow_query(&terms, only_folders)?;
    Query::parse(&query).ok()?;

    Some(ParsedSql {
        columns,
        limit,
        query,
    })
}

fn split_from_system_index(sql: &str) -> Option<(&str, &str)> {
    let upper = sql.to_ascii_uppercase();
    for needle in [" FROM SYSTEMINDEX ", " FROM \"SYSTEMINDEX\" "] {
        if let Some(idx) = upper.find(needle) {
            return Some((&sql[..idx], &sql[idx + needle.len()..]));
        }
    }
    None
}

fn extract_where_predicate(after_from: &str) -> Option<&str> {
    let trimmed = after_from.trim_start();
    if trimmed.len() < 6 || !trimmed[..6].eq_ignore_ascii_case("WHERE ") {
        return None;
    }
    let after_where = &trimmed[6..];
    let end = find_clause_end_outside_quotes(after_where);
    let pred = after_where[..end].trim();
    (!pred.is_empty()).then_some(pred)
}

fn find_clause_end_outside_quotes(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut in_single = false;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if in_single && i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                i += 2;
                continue;
            }
            in_single = !in_single;
            i += 1;
            continue;
        }
        if !in_single {
            for kw in [b" ORDER BY ".as_slice(), b" GROUP BY ".as_slice()] {
                if i + kw.len() <= bytes.len() && bytes[i..i + kw.len()].eq_ignore_ascii_case(kw) {
                    return i;
                }
            }
        }
        i += 1;
    }
    s.len()
}

/// Whole word `word` present in `s` outside single-quoted SQL
/// literals (`''` is an escaped quote). Word characters are
/// alphanumerics and `_`, so e.g. `CANNOT` does not match `NOT`.
fn contains_word_outside_single_quotes(s: &str, word: &str) -> bool {
    let bytes = s.as_bytes();
    let needle = word.as_bytes();
    let mut in_single = false;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if in_single && i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                i += 2;
                continue;
            }
            in_single = !in_single;
            i += 1;
            continue;
        }
        if !in_single
            && i + needle.len() <= bytes.len()
            && bytes[i..i + needle.len()].eq_ignore_ascii_case(needle)
        {
            let before_ok =
                i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
            let after = i + needle.len();
            let after_ok = after == bytes.len()
                || !(bytes[after].is_ascii_alphanumeric() || bytes[after] == b'_');
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// One term set per accepted `CONTAINS` call (kind/itemtype calls are
/// skipped: they filter kinds, handled separately). `None` when any
/// condition itself uses unsupported boolean structure.
fn extract_contains_term_sets(predicate: &str) -> Option<Vec<Vec<String>>> {
    let mut sets = Vec::new();
    let upper = predicate.to_ascii_uppercase();
    let mut search_from = 0;

    while let Some(rel) = upper[search_from..].find("CONTAINS(") {
        let start = search_from + rel;
        let args_start = start + "CONTAINS(".len();
        let Some((prop_prefix, literal, next_pos)) = parse_contains_call(&predicate[args_start..])
        else {
            search_from = args_start;
            continue;
        };
        search_from = args_start + next_pos;

        let prop_lower = prop_prefix.to_ascii_lowercase();
        if prop_lower.contains("kind") || prop_lower.contains("itemtype") {
            continue;
        }

        sets.push(parse_contains_condition(&literal)?);
    }

    Some(sets)
}

/// Parses `<prop>, '<literal>' ...)` or `'<literal>' ...)` starting right after `CONTAINS(`.
fn parse_contains_call(s: &str) -> Option<(&str, String, usize)> {
    let quote_idx = s.find('\'')?;
    let prop_prefix = s[..quote_idx].trim().trim_end_matches(',').trim();
    let after_quote = &s[quote_idx + 1..];
    let mut literal = String::new();
    let mut chars = after_quote.char_indices().peekable();
    let mut end_offset = s.len();

    while let Some((idx, ch)) = chars.next() {
        if ch == '\'' {
            if let Some(&(_, '\'')) = chars.peek() {
                chars.next();
                literal.push('\'');
            } else {
                end_offset = quote_idx + 1 + idx + 1;
                break;
            }
        } else {
            literal.push(ch);
        }
    }

    Some((prop_prefix, literal, end_offset))
}

/// Terms of one CONTAINS condition. `None` when the condition uses
/// OR or NOT: Ferrow ANDs terms, so either operator inside a single
/// condition would change the result set.
fn parse_contains_condition(condition: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut in_quotes = false;
    let mut current = String::new();
    let mut outside = String::new();
    let mut saw_quotes = false;

    for ch in condition.chars() {
        if ch == '"' {
            saw_quotes = true;
            if in_quotes {
                if let Some(cleaned) = clean_search_token(&current) {
                    out.push(cleaned);
                }
                current.clear();
                in_quotes = false;
            } else {
                in_quotes = true;
            }
        } else if in_quotes {
            current.push(ch);
        } else {
            outside.push(ch);
        }
    }

    let unsupported_operator = outside
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|w| w.eq_ignore_ascii_case("NOT") || w.eq_ignore_ascii_case("OR"));
    if unsupported_operator {
        return None;
    }

    if !saw_quotes {
        for word in condition.split_whitespace() {
            if matches!(
                word.to_ascii_uppercase().as_str(),
                "AND" | "NEAR" | "FORMSOF" | "INFLECTIONAL"
            ) {
                continue;
            }
            if let Some(cleaned) = clean_search_token(word) {
                out.push(cleaned);
            }
        }
    }

    Some(out)
}

fn extract_like_terms(predicate: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let upper = predicate.to_ascii_uppercase();
    let mut search_from = 0;

    while let Some(rel) = upper[search_from..].find(" LIKE '") {
        let lit_start = search_from + rel + " LIKE '".len();
        let rest = &predicate[lit_start..];
        let Some(end_quote) = rest.find('\'') else {
            break;
        };
        let pattern = &rest[..end_quote];
        search_from = lit_start + end_quote + 1;

        // Ignore patterns with single-character SQL wildcards (`_`) or inner `%`.
        let inner = pattern.trim_matches('%');
        if inner.is_empty()
            || inner.contains('%')
            || inner.contains('_')
            || rest[end_quote + 1..].contains("--")
        {
            continue;
        }
        if let Some(cleaned) = clean_search_token(inner) {
            push_unique_term(&mut terms, cleaned);
        }
    }

    terms
}

fn clean_search_token(raw: &str) -> Option<String> {
    let trimmed = raw
        .trim()
        .trim_matches(|c: char| matches!(c, '*' | '%' | '"' | '\''));
    let trimmed = trimmed.trim();
    if trimmed.is_empty() || trimmed.len() > 256 {
        return None;
    }
    Some(trimmed.to_owned())
}

fn push_unique_term(terms: &mut Vec<String>, candidate: String) {
    if !terms
        .iter()
        .any(|existing| existing.eq_ignore_ascii_case(&candidate))
    {
        terms.push(candidate);
    }
}

fn is_folder_only_kind(predicate: &str) -> bool {
    let lower = predicate.to_ascii_lowercase();
    if !lower.contains("system.kind") {
        return false;
    }
    lower.contains("'folder'")
        && !lower.contains("'document'")
        && !lower.contains("'picture'")
        && !lower.contains("'music'")
        && !lower.contains("'video'")
        && !lower.contains("'program'")
}

fn format_ferrow_query(terms: &[String], only_folders: bool) -> Option<String> {
    let mut parts = Vec::with_capacity(terms.len() + usize::from(only_folders));
    if only_folders {
        parts.push("folder:".to_owned());
    }
    for term in terms {
        let sanitized: String = term.chars().filter(|&c| c != '"').collect();
        let sanitized = sanitized.trim();
        if sanitized.is_empty() {
            continue;
        }
        let needs_quotes = sanitized
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '|' | '!' | '<' | '>' | ':' | '*' | '?'));
        if needs_quotes {
            parts.push(format!("\"{sanitized}\""));
        } else {
            parts.push(sanitized.to_owned());
        }
    }
    if parts.is_empty() || (only_folders && parts.len() == 1) {
        None
    } else {
        Some(parts.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_TAB_PROJECTION: &str = "\"System.Kind\",\"System.Search.Rank\",path,\"System.DateCreated\",\"System.DateModified\",\"System.Search.QueryPropertyHits\",\"System.ItemNameDisplay\",\"System.FileExtension\",\"System.ItemType\",\"System.Title\",\"System.Music.AlbumTitle\",\"System.Music.DisplayArtist\",\"System.DateAccessed\",\"System.Link.TargetParsingPath\",\"System.SFGAOFlags\"";

    const DOCS_TAB_PROJECTION: &str = "\"System.Kind\",\"System.Search.Rank\",path,\"System.DateCreated\",\"System.DateModified\",\"System.Search.QueryPropertyHits\",\"System.ItemNameDisplay\",\"System.FileExtension\",\"System.ItemType\",\"System.Search.AutoSummary\",\"System.Keywords\",\"System.Title\",\"System.Author\",\"System.Document.LastAuthor\",\"System.Photo.TagViewAggregate\",\"System.Music.DisplayArtist\",\"System.Music.Genre\",\"System.Link.TargetParsingPath\",\"System.SFGAOFlags\"";

    #[test]
    fn parses_start_menu_all_tab_query() {
        let sql = format!(
            "SELECT TOP 30 {ALL_TAB_PROJECTION} FROM SystemIndex WHERE CONTAINS(System.FileName, '\"ferrow-native-query-proof*\"') ORDER BY System.Search.Rank DESC"
        );
        let parsed = parse_windows_search_sql(&sql).expect("valid Start menu query");
        assert_eq!(parsed.columns.len(), 15);
        assert_eq!(parsed.columns[0], "System.Kind");
        assert_eq!(parsed.columns[2], "path");
        assert_eq!(parsed.limit, 30);
        assert_eq!(parsed.query, "ferrow-native-query-proof");
    }

    #[test]
    fn parses_start_menu_documents_tab_19_columns_and_multi_term_contains() {
        let sql = format!(
            "SELECT TOP 30 {DOCS_TAB_PROJECTION} FROM \"SystemIndex\" WHERE (CONTAINS(\"System.ItemNameDisplay\", '\"budget*\" AND \"2026*\"', 2057) OR CONTAINS(*, '\"budget*\" AND \"2026*\"', 2057)) AND CONTAINS(\"System.Kind\", '\"document\"') ORDER BY \"System.Search.Rank\" DESC"
        );
        let parsed = parse_windows_search_sql(&sql).expect("valid Documents tab query");
        assert_eq!(parsed.columns.len(), 19);
        assert_eq!(parsed.limit, 30);
        assert_eq!(parsed.query, "budget 2026");
    }

    #[test]
    fn rejects_booleans_whose_meaning_would_change() {
        for unsupported in [
            // SQL-level NOT CONTAINS exclusion.
            "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE CONTAINS(System.FileName, '\"report*\"') AND NOT CONTAINS(System.FileName, '\"private*\"')",
            // NOT inside one CONTAINS condition, quoted form.
            "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE CONTAINS(System.FileName, '\"report*\" AND NOT \"private*\"')",
            // NOT inside one CONTAINS condition, unquoted form.
            "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE CONTAINS(System.FileName, 'report AND NOT private')",
            // OR between different term sets (union, not AND).
            "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE CONTAINS(System.FileName, '\"alpha*\"') OR CONTAINS(System.FileName, '\"beta*\"')",
            // OR inside one CONTAINS condition.
            "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE CONTAINS(System.FileName, '\"alpha*\" OR \"beta*\"')",
            // OR between different LIKE patterns.
            "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE System.FileName LIKE '%alpha%' OR System.FileName LIKE '%beta%'",
        ] {
            assert!(
                parse_windows_search_sql(unsupported).is_none(),
                "expected None (native fallback) for: {unsupported}"
            );
        }

        // A search *term* containing the letters is not an operator.
        let sql = "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE CONTAINS(System.FileName, '\"cannot*\" AND \"notes*\"')";
        let parsed = parse_windows_search_sql(sql).expect("terms resembling operators still parse");
        assert_eq!(parsed.query, "cannot notes");
    }

    #[test]
    fn parses_like_query_and_rejects_wildcards_or_empty() {
        let sql = "SELECT TOP 12 System.ItemName FROM SystemIndex WHERE System.FileName LIKE '%report-2026%'";
        let parsed = parse_windows_search_sql(sql).expect("valid LIKE query");
        assert_eq!(parsed.columns, vec!["System.ItemName"]);
        assert_eq!(parsed.limit, 12);
        assert_eq!(parsed.query, "report-2026");

        for unsupported in [
            "SELECT * FROM SystemIndex",
            "SELECT TOP 12 System.ItemName FROM SystemIndex WHERE System.FileName LIKE '%a_b%'",
            "SELECT TOP 12 System.ItemName FROM SystemIndex WHERE System.FileName LIKE '%a%z%'",
            "SELECT TOP 12 System.ItemName FROM SystemIndex WHERE System.FileName LIKE '%%'",
            "SELECT TOP 1 System.ItemName FROM SystemIndex WHERE System.ItemName = 'exact-only.txt'",
        ] {
            assert!(
                parse_windows_search_sql(unsupported).is_none(),
                "expected None for: {unsupported}"
            );
        }
    }
}
