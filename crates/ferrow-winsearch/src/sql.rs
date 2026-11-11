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

    // Boolean meaning must survive translation. The whole predicate is
    // parsed as a boolean expression and accepted only when it reduces
    // to a conjunction of filename term sets (plus an optional folder
    // restriction). Every other leaf or operator falls back to native.
    let (terms, only_folders) = parse_predicate_expression(predicate)?;
    if terms.is_empty() {
        return None;
    }

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PredicateComponent {
    Terms(usize),
    FolderOnly,
}

#[derive(Clone, Debug)]
enum PredicateExpr {
    Leaf(PredicateComponent),
    And(Vec<PredicateExpr>),
    Or(Vec<PredicateExpr>),
}

/// Reduce an expression to a conjunction only by sound identities:
/// nested ANDs flatten, and an OR survives only when every branch is
/// the same conjunction. Anything else has no exact Ferrow AND form.
fn predicate_conjunction(
    expr: &PredicateExpr,
) -> Option<std::collections::BTreeSet<PredicateComponent>> {
    match expr {
        PredicateExpr::Leaf(component) => Some([*component].into_iter().collect()),
        PredicateExpr::And(parts) => {
            let mut combined = std::collections::BTreeSet::new();
            for part in parts {
                combined.extend(predicate_conjunction(part)?);
            }
            Some(combined)
        }
        PredicateExpr::Or(parts) => {
            let mut branches = parts.iter();
            let first = predicate_conjunction(branches.next()?)?;
            branches
                .all(|part| predicate_conjunction(part).as_ref() == Some(&first))
                .then_some(first)
        }
    }
}

fn parse_predicate_expression(predicate: &str) -> Option<(Vec<String>, bool)> {
    let mut parser = PredicateParser {
        predicate,
        bytes: predicate.as_bytes(),
        pos: 0,
        distinct_term_sets: Vec::new(),
        representatives: Vec::new(),
    };
    let expr = parser.parse_or()?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return None;
    }
    let components = predicate_conjunction(&expr)?;
    let mut terms = Vec::new();
    let mut only_folders = false;
    for component in components {
        match component {
            PredicateComponent::Terms(index) => {
                for term in &parser.representatives[index] {
                    push_unique_term(&mut terms, term.clone());
                }
            }
            PredicateComponent::FolderOnly => only_folders = true,
        }
    }
    Some((terms, only_folders))
}

struct PredicateParser<'a> {
    predicate: &'a str,
    bytes: &'a [u8],
    pos: usize,
    distinct_term_sets: Vec<Vec<String>>,
    representatives: Vec<Vec<String>>,
}

impl PredicateParser<'_> {
    fn skip_ws(&mut self) {
        while self
            .bytes
            .get(self.pos)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.pos += 1;
        }
    }

    fn consume_word(&mut self, word: &str) -> bool {
        self.skip_ws();
        let needle = word.as_bytes();
        let end = self.pos + needle.len();
        if end <= self.bytes.len()
            && self.bytes[self.pos..end].eq_ignore_ascii_case(needle)
            && self
                .bytes
                .get(end)
                .is_none_or(|b| !(b.is_ascii_alphanumeric() || *b == b'_'))
        {
            self.pos = end;
            true
        } else {
            false
        }
    }

    fn consume_byte(&mut self, byte: u8) -> bool {
        self.skip_ws();
        if self.bytes.get(self.pos) == Some(&byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn parse_or(&mut self) -> Option<PredicateExpr> {
        let mut parts = vec![self.parse_and()?];
        while self.consume_word("OR") {
            parts.push(self.parse_and()?);
        }
        Some(if parts.len() == 1 {
            parts.pop().unwrap()
        } else {
            PredicateExpr::Or(parts)
        })
    }

    fn parse_and(&mut self) -> Option<PredicateExpr> {
        let mut parts = vec![self.parse_unary()?];
        while self.consume_word("AND") {
            parts.push(self.parse_unary()?);
        }
        Some(if parts.len() == 1 {
            parts.pop().unwrap()
        } else {
            PredicateExpr::And(parts)
        })
    }

    fn parse_unary(&mut self) -> Option<PredicateExpr> {
        if self.consume_byte(b'(') {
            let expr = self.parse_or()?;
            self.consume_byte(b')').then_some(expr)
        } else {
            self.parse_leaf().map(PredicateExpr::Leaf)
        }
    }
}

impl PredicateParser<'_> {
    fn register_terms(&mut self, terms: Vec<String>) -> Option<PredicateComponent> {
        let mut normalized: Vec<String> =
            terms.iter().map(|term| term.to_ascii_lowercase()).collect();
        normalized.sort();
        normalized.dedup();
        if normalized.is_empty() {
            return None;
        }
        if let Some(index) = self
            .distinct_term_sets
            .iter()
            .position(|existing| existing == &normalized)
        {
            return Some(PredicateComponent::Terms(index));
        }
        let index = self.distinct_term_sets.len();
        self.distinct_term_sets.push(normalized);
        self.representatives.push(terms);
        Some(PredicateComponent::Terms(index))
    }

    fn parse_leaf(&mut self) -> Option<PredicateComponent> {
        self.skip_ws();
        if self.consume_word("CONTAINS") {
            self.skip_ws();
            if self.bytes.get(self.pos) != Some(&b'(') {
                return None;
            }
            let open = self.pos;
            let call_end = find_matching_paren(self.bytes, open)?;
            let args = &self.predicate[open + 1..call_end - 1];
            let (prop_prefix, literal, _) = parse_contains_call(args)?;
            self.pos = call_end;
            let terms = parse_contains_condition(&literal)?;
            let property = normalize_property(prop_prefix);
            if property == "system.kind" {
                let folder_only = terms.len() == 1 && terms[0].eq_ignore_ascii_case("folder");
                return folder_only.then_some(PredicateComponent::FolderOnly);
            }
            if !is_name_property_list(&property) {
                return None;
            }
            return self.register_terms(terms);
        }

        let property = self.read_property()?;
        if property == "system.kind" {
            // Only the exact folder restriction has a Ferrow form
            // (`folder:`); every other kind value or operator falls
            // back to the native provider.
            if !self.consume_byte(b'=') {
                return None;
            }
            self.skip_ws();
            let value = self.parse_sql_literal()?;
            return value
                .eq_ignore_ascii_case("folder")
                .then_some(PredicateComponent::FolderOnly);
        }
        if !self.consume_word("LIKE") {
            return None;
        }
        self.skip_ws();
        let pattern = self.parse_sql_literal()?;
        if !is_name_property(&property) {
            return None;
        }
        let inner = pattern.strip_prefix('%')?.strip_suffix('%')?;
        if inner.is_empty() || inner.contains(['%', '_', '*', '?']) {
            return None;
        }
        self.register_terms(vec![clean_search_token(inner)?])
    }

    fn read_property(&mut self) -> Option<String> {
        self.skip_ws();
        let start = self.pos;
        match self.bytes.get(self.pos) {
            Some(b'"') => {
                self.pos += 1;
                let inner_start = self.pos;
                while self.bytes.get(self.pos).is_some_and(|b| *b != b'"') {
                    self.pos += 1;
                }
                if self.bytes.get(self.pos) != Some(&b'"') {
                    return None;
                }
                let property = self.predicate[inner_start..self.pos].to_owned();
                self.pos += 1;
                Some(normalize_property(&property))
            }
            Some(b'[') => {
                self.pos += 1;
                let inner_start = self.pos;
                while self.bytes.get(self.pos).is_some_and(|b| *b != b']') {
                    self.pos += 1;
                }
                if self.bytes.get(self.pos) != Some(&b']') {
                    return None;
                }
                let property = self.predicate[inner_start..self.pos].to_owned();
                self.pos += 1;
                Some(normalize_property(&property))
            }
            _ => {
                while self
                    .bytes
                    .get(self.pos)
                    .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'*'))
                {
                    self.pos += 1;
                }
                (self.pos > start).then(|| normalize_property(&self.predicate[start..self.pos]))
            }
        }
    }

    fn parse_sql_literal(&mut self) -> Option<String> {
        if self.bytes.get(self.pos) != Some(&b'\'') {
            return None;
        }
        self.pos += 1;
        let mut literal = String::new();
        let rest = &self.predicate[self.pos..];
        let mut chars = rest.char_indices().peekable();
        while let Some((idx, ch)) = chars.next() {
            if ch == '\'' {
                if let Some(&(_, '\'')) = chars.peek() {
                    chars.next();
                    literal.push('\'');
                } else {
                    self.pos += idx + 1;
                    return Some(literal);
                }
            } else {
                literal.push(ch);
            }
        }
        None
    }
}

fn normalize_property(property: &str) -> String {
    property
        .trim()
        .trim_matches(|c: char| matches!(c, '"' | '[' | ']'))
        .trim()
        .to_ascii_lowercase()
}

fn is_name_property(property: &str) -> bool {
    matches!(
        property,
        "*" | "filename"
            | "system.filename"
            | "itemname"
            | "system.itemname"
            | "itemnamedisplay"
            | "system.itemnamedisplay"
    )
}

/// A CONTAINS property may be one name property or a parenthesized
/// list of name properties (the same terms against each column).
fn is_name_property_list(property: &str) -> bool {
    if property.is_empty() {
        return true; // CONTAINS('<literal>') searches the default columns.
    }
    let inner = property
        .strip_prefix('(')
        .and_then(|value| value.strip_suffix(')'))
        .unwrap_or(property);
    inner
        .split(',')
        .all(|part| is_name_property(&normalize_property(part)))
}

fn find_matching_paren(bytes: &[u8], open: usize) -> Option<usize> {
    if bytes.get(open) != Some(&b'(') {
        return None;
    }
    let mut depth = 0usize;
    let mut in_single = false;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => {
                if in_single && bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                in_single = !in_single;
            }
            b'(' if !in_single => depth += 1,
            b')' if !in_single => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
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
            "SELECT TOP 30 {DOCS_TAB_PROJECTION} FROM \"SystemIndex\" WHERE (CONTAINS(\"System.ItemNameDisplay\", '\"budget*\" AND \"2026*\"', 2057) OR CONTAINS(*, '\"budget*\" AND \"2026*\"', 2057)) ORDER BY \"System.Search.Rank\" DESC"
        );
        let parsed = parse_windows_search_sql(&sql).expect("valid Documents tab query");
        assert_eq!(parsed.columns.len(), 19);
        assert_eq!(parsed.limit, 30);
        assert_eq!(parsed.query, "budget 2026");

        // The real Documents tab also restricts `System.Kind` to
        // documents. Ferrow has no exact form for that kind, so the
        // whole predicate falls back to native instead of silently
        // dropping the restriction and returning non-documents.
        let with_kind = format!(
            "SELECT TOP 30 {DOCS_TAB_PROJECTION} FROM \"SystemIndex\" WHERE (CONTAINS(\"System.ItemNameDisplay\", '\"budget*\" AND \"2026*\"', 2057) OR CONTAINS(*, '\"budget*\" AND \"2026*\"', 2057)) AND CONTAINS(\"System.Kind\", '\"document\"') ORDER BY \"System.Search.Rank\" DESC"
        );
        assert!(parse_windows_search_sql(&with_kind).is_none());
    }

    #[test]
    fn handles_mixed_grouped_and_kind_predicates() {
        // Mixed CONTAINS + LIKE under AND: both term sets survive.
        let sql = "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE CONTAINS(System.FileName, '\"alpha*\"') AND System.FileName LIKE '%beta%'";
        let parsed = parse_windows_search_sql(sql).expect("mixed AND parses");
        assert_eq!(parsed.query, "alpha beta");

        // Grouped OR of identical term sets ANDed with another term.
        let sql = "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE (CONTAINS(System.FileName, '\"alpha*\"') OR System.FileName LIKE '%alpha%') AND CONTAINS(System.FileName, '\"beta*\"')";
        let parsed = parse_windows_search_sql(sql).expect("identical OR branches parse");
        assert_eq!(parsed.query, "alpha beta");

        // Precedence: AND binds tighter than OR; identical conjunctions
        // on both sides still reduce to one conjunction.
        let sql = "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE CONTAINS(System.FileName, '\"alpha*\"') AND CONTAINS(System.FileName, '\"beta*\"') OR CONTAINS(System.FileName, '\"beta*\"') AND CONTAINS(System.FileName, '\"alpha*\"')";
        let parsed = parse_windows_search_sql(sql).expect("equal conjunctions parse");
        assert_eq!(parsed.query, "alpha beta");

        // The exact folder restriction, in both shell spellings.
        for sql in [
            "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE CONTAINS(System.FileName, '\"alpha*\"') AND CONTAINS(\"System.Kind\", '\"folder\"')",
            "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE CONTAINS(System.FileName, '\"alpha*\"') AND System.Kind = 'folder'",
            "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE \"System.Kind\" = 'FOLDER' AND CONTAINS(System.FileName, '\"alpha*\"')",
        ] {
            let parsed = parse_windows_search_sql(sql)
                .unwrap_or_else(|| panic!("folder restriction parses: {sql}"));
            assert_eq!(parsed.query, "folder: alpha", "for: {sql}");
        }

        for unsupported in [
            // Grouped OR of different terms cannot become an AND.
            "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE (CONTAINS(System.FileName, '\"alpha*\"') OR CONTAINS(System.FileName, '\"beta*\"')) AND CONTAINS(System.FileName, '\"gamma*\"')",
            // Precedence makes this alpha OR (beta AND gamma).
            "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE CONTAINS(System.FileName, '\"alpha*\"') OR CONTAINS(System.FileName, '\"beta*\"') AND CONTAINS(System.FileName, '\"gamma*\"')",
            // Kind values Ferrow cannot express exactly.
            "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE CONTAINS(System.FileName, '\"alpha*\"') AND System.Kind = 'document'",
            // Folder as an OR branch is a union, not a restriction.
            "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE CONTAINS(System.FileName, '\"alpha*\"') OR CONTAINS(\"System.Kind\", '\"folder\"')",
            // Unknown CONTAINS property.
            "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE CONTAINS(System.Title, '\"alpha*\"')",
            // Unhandled comparison leaf beside a handled one.
            "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE CONTAINS(System.FileName, '\"alpha*\"') AND System.Size > 100",
            // Grouped NOT.
            "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE CONTAINS(System.FileName, '\"alpha*\"') AND NOT (CONTAINS(System.FileName, '\"beta*\"'))",
        ] {
            assert!(
                parse_windows_search_sql(unsupported).is_none(),
                "expected None (native fallback) for: {unsupported}"
            );
        }
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
