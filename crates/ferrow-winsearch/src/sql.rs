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
/// Largest SQL command considered. Real shell queries are ~1-2 KiB;
/// anything bigger falls back to the native provider.
const MAX_SQL_LEN: usize = 64 * 1024;
/// Deepest predicate nesting the recursive-descent parser walks. The
/// parser runs on a `SearchHost.exe` thread, where a stack overflow
/// aborts the process and cannot be caught — nesting beyond this is
/// rejected for native fallback long before the stack is at risk.
const MAX_PREDICATE_DEPTH: usize = 32;

/// Parsed Windows Search SQL query ready for Ferrow execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedSql {
    /// Projection column names in ordinal order (1-based in OLE DB).
    pub columns: Vec<String>,
    /// Row limit from `TOP <N>` (clamped to `1..=MAX_TOP_LIMIT`).
    pub limit: usize,
    /// Extracted query string validated with `ferrow_query::Query::parse`.
    pub query: String,
    /// The first search term, used to rank hits the way Windows does
    /// (whole name, then name start, then word start). `None` when the
    /// term has characters a wildcard cannot carry.
    pub rank_term: Option<String>,
    /// `query` with the first term as a name-prefix wildcard (`term*`):
    /// the best Start matches, asked for first.
    pub prefix_query: Option<String>,
}

/// Leaves out everything inside a folder whose name starts with a dot
/// (`.git`, `.cache`, `.claude`, ...): Start silently drops those rows, so
/// they would only take places from files it shows.
const SKIP_DOT_FOLDERS: &str = "!path:\\.";

/// A term that can stand in a bare `term*` wildcard.
fn is_wildcard_safe(term: &str) -> bool {
    !term.is_empty()
        && term
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
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
    if sql.len() > MAX_SQL_LEN {
        return None;
    }
    let (columns, limit) = parse_projection(sql)?;
    let (_before_from, after_from) = split_from_system_index(sql)?;
    let predicate = extract_where_predicate(after_from)?;
    let (aliases, predicate) = split_column_group_aliases(predicate)?;

    // Boolean meaning must survive translation. The whole predicate is
    // parsed as a boolean expression and accepted only when it reduces
    // to a conjunction of filename term sets (plus an optional folder
    // restriction). Every other leaf or operator falls back to native.
    let (terms, kind) = parse_predicate_expression(predicate, aliases)?;
    if terms.is_empty() {
        return None;
    }

    let query = format!("{} {SKIP_DOT_FOLDERS}", format_ferrow_query(&terms, kind)?);
    Query::parse(&query).ok()?;

    let rank_term = terms.first().filter(|t| is_wildcard_safe(t)).cloned();
    let prefix_query = rank_term.as_ref().and_then(|first| {
        let mut parts = Vec::new();
        if let Some(kind) = kind {
            parts.push(kind.filter()?);
        }
        parts.push(format!("{first}*"));
        if terms.len() > 1 {
            parts.push(format_ferrow_query(&terms[1..], None)?);
        }
        parts.push(SKIP_DOT_FOLDERS.to_owned());
        let prefix = parts.join(" ");
        Query::parse(&prefix).is_ok().then_some(prefix)
    });

    Some(ParsedSql {
        columns,
        limit,
        query,
        rank_term,
        prefix_query,
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

/// Column group aliases: `#name` (lowercase) to its parenthesized column list.
type ColumnGroupAliases = Vec<(String, String)>;

/// Split leading `WITH (<columns>) AS #<name>` column group definitions
/// (newer Windows 11 Start queries) off the predicate. `None` when a
/// `WITH` clause is malformed.
fn split_column_group_aliases(mut predicate: &str) -> Option<(ColumnGroupAliases, &str)> {
    let mut aliases = Vec::new();
    loop {
        let trimmed = predicate.trim_start();
        let Some(rest) = strip_keyword(trimmed, "WITH") else {
            return Some((aliases, trimmed));
        };
        let rest = rest.trim_start();
        let close = find_matching_paren(rest.as_bytes(), 0)?;
        let columns = &rest[..close];
        let rest = strip_keyword(rest[close..].trim_start(), "AS")?.trim_start();
        let name_len = rest
            .strip_prefix('#')?
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len() - 1);
        if name_len == 0 {
            return None;
        }
        aliases.push((rest[..=name_len].to_ascii_lowercase(), columns.to_owned()));
        predicate = &rest[name_len + 1..];
    }
}

/// `s` after a leading case-insensitive `word` that ends at a non-word byte.
fn strip_keyword<'a>(s: &'a str, word: &str) -> Option<&'a str> {
    let head = s.get(..word.len())?;
    let rest = &s[word.len()..];
    (head.eq_ignore_ascii_case(word)
        && rest
            .bytes()
            .next()
            .is_none_or(|b| !(b.is_ascii_alphanumeric() || b == b'_')))
    .then_some(rest)
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

/// A `System.Kind` value Ferrow can answer: folders, or files by extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ItemKind {
    Folder,
    Document,
    Picture,
    Music,
    Video,
}

impl ItemKind {
    /// The kind for a `System.Kind` value the Start menu filters on.
    pub fn from_kind_value(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "folder" => Some(ItemKind::Folder),
            "document" => Some(ItemKind::Document),
            "picture" => Some(ItemKind::Picture),
            "music" => Some(ItemKind::Music),
            "video" => Some(ItemKind::Video),
            _ => None,
        }
    }

    /// The `System.Kind` value Windows reports for this kind.
    pub fn kind_value(self) -> &'static str {
        match self {
            ItemKind::Folder => "folder",
            ItemKind::Document => "document",
            ItemKind::Picture => "picture",
            ItemKind::Music => "music",
            ItemKind::Video => "video",
        }
    }

    /// The extensions Windows files under this kind (none for folders).
    pub fn extensions(self) -> Vec<&'static str> {
        match self {
            ItemKind::Folder => Vec::new(),
            kind => crate::kinds::extensions_of_kind(kind.kind_value())
                .into_iter()
                .filter(|ext| {
                    ext.chars()
                        .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
                })
                .collect(),
        }
    }

    /// The Ferrow filter for this kind (`folder:` or `ext:a;b;...`).
    /// `None` when no extension has this kind.
    fn filter(self) -> Option<String> {
        match self {
            ItemKind::Folder => Some("folder:".to_owned()),
            kind => {
                let extensions = kind.extensions();
                (!extensions.is_empty()).then(|| format!("ext:{}", extensions.join(";")))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PredicateComponent {
    Terms(usize),
    Kind(ItemKind),
    /// `ReuseWhere(<id>)`: the shell's location scope from an earlier
    /// query. Ferrow answers from every location it indexes, so it adds
    /// no restriction.
    Scope,
}

#[derive(Clone, Debug)]
enum PredicateExpr {
    Leaf(PredicateComponent),
    And(Vec<PredicateExpr>),
    Or(Vec<PredicateExpr>),
}

/// A conjunction of filename terms (each a case-insensitive substring of
/// the name), plus the kind restriction and the location scope.
#[derive(Clone, Debug, Default)]
struct Conjunction {
    terms: Vec<String>,
    kind: Option<ItemKind>,
    scope: bool,
}

impl Conjunction {
    /// `None` when the two restrict to different kinds (no item is both).
    fn and(mut self, other: Conjunction) -> Option<Self> {
        for term in other.terms {
            push_unique_term(&mut self.terms, term);
        }
        match (self.kind, other.kind) {
            (Some(a), Some(b)) if a != b => return None,
            (None, kind) => self.kind = kind,
            _ => {}
        }
        self.scope |= other.scope;
        Some(self)
    }

    /// True when every name matching `self` also matches `weaker`: each
    /// of its terms is contained in one of ours, and it restricts the
    /// kind or the scope only when we do too.
    fn implies(&self, weaker: &Conjunction) -> bool {
        (weaker.kind.is_none() || self.kind == weaker.kind)
            && (!weaker.scope || self.scope)
            && weaker.terms.iter().all(|w| {
                let w = w.to_lowercase();
                self.terms.iter().any(|t| t.to_lowercase().contains(&w))
            })
    }
}

/// Reduce an expression to a conjunction only by sound identities:
/// nested ANDs flatten, and an OR reduces to the branch that every other
/// branch implies (for example `"a.txt" OR ("a" AND "txt")` is `a txt`).
/// Anything else has no exact Ferrow AND form.
fn predicate_conjunction(expr: &PredicateExpr, term_sets: &[Vec<String>]) -> Option<Conjunction> {
    match expr {
        PredicateExpr::Leaf(component) => Some(match component {
            PredicateComponent::Terms(index) => Conjunction {
                terms: term_sets.get(*index)?.clone(),
                ..Default::default()
            },
            PredicateComponent::Kind(kind) => Conjunction {
                kind: Some(*kind),
                ..Default::default()
            },
            PredicateComponent::Scope => Conjunction {
                scope: true,
                ..Default::default()
            },
        }),
        PredicateExpr::And(parts) => {
            let mut combined = Conjunction::default();
            for part in parts {
                combined = combined.and(predicate_conjunction(part, term_sets)?)?;
            }
            Some(combined)
        }
        PredicateExpr::Or(parts) => {
            let branches = parts
                .iter()
                .map(|part| predicate_conjunction(part, term_sets))
                .collect::<Option<Vec<_>>>()?;
            branches
                .iter()
                .find(|weakest| branches.iter().all(|b| b.implies(weakest)))
                .cloned()
        }
    }
}

fn parse_predicate_expression(
    predicate: &str,
    aliases: ColumnGroupAliases,
) -> Option<(Vec<String>, Option<ItemKind>)> {
    let mut parser = PredicateParser {
        predicate,
        bytes: predicate.as_bytes(),
        pos: 0,
        depth: 0,
        distinct_term_sets: Vec::new(),
        representatives: Vec::new(),
        aliases,
    };
    let expr = parser.parse_or()?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return None;
    }
    let conjunction = predicate_conjunction(&expr, &parser.representatives)?;
    Some((conjunction.terms, conjunction.kind))
}

struct PredicateParser<'a> {
    predicate: &'a str,
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
    distinct_term_sets: Vec<Vec<String>>,
    representatives: Vec<Vec<String>>,
    aliases: ColumnGroupAliases,
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
            // The only recursion in the descent goes through grouping,
            // so bounding it bounds both parser and reduction stack use.
            self.depth += 1;
            if self.depth > MAX_PREDICATE_DEPTH {
                return None;
            }
            let expr = self.parse_or();
            self.depth -= 1;
            let expr = expr?;
            if !self.consume_byte(b')') {
                return None;
            }
            self.skip_rank_clause()?;
            Some(expr)
        } else {
            let leaf = self.parse_leaf()?;
            self.skip_rank_clause()?;
            Some(PredicateExpr::Leaf(leaf))
        }
    }

    /// Skip an optional `RANK BY <function>(<args>)`: it only orders
    /// native results and never changes which items match.
    fn skip_rank_clause(&mut self) -> Option<()> {
        let saved = self.pos;
        if !self.consume_word("RANK") {
            return Some(());
        }
        if !self.consume_word("BY") {
            self.pos = saved;
            return Some(());
        }
        self.skip_ws();
        while self
            .bytes
            .get(self.pos)
            .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
        {
            self.pos += 1;
        }
        self.skip_ws();
        self.pos = find_matching_paren(self.bytes, self.pos)?;
        Some(())
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
            let mut property = normalize_property(prop_prefix);
            if property.starts_with('#') {
                property = self
                    .aliases
                    .iter()
                    .find(|(alias, _)| *alias == property)
                    .map(|(_, columns)| normalize_property(columns))?;
            }
            if property == "system.kind" {
                if terms.len() != 1 {
                    return None;
                }
                return ItemKind::from_kind_value(&terms[0]).map(PredicateComponent::Kind);
            }
            if !is_name_property_list(&property) {
                return None;
            }
            return self.register_terms(terms);
        }

        if self.consume_word("ReuseWhere") {
            self.skip_ws();
            let end = find_matching_paren(self.bytes, self.pos)?;
            let id = self.predicate[self.pos + 1..end - 1].trim();
            if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            self.pos = end;
            return Some(PredicateComponent::Scope);
        }

        let property = self.read_property()?;
        if property == "system.kind" {
            // Kinds with a Ferrow form (`folder:` or an extension list);
            // every other kind value or operator falls back to native.
            if !self.consume_byte(b'=') {
                return None;
            }
            self.skip_ws();
            let value = self.parse_sql_literal()?;
            return ItemKind::from_kind_value(&value).map(PredicateComponent::Kind);
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

fn format_ferrow_query(terms: &[String], kind: Option<ItemKind>) -> Option<String> {
    let mut parts = Vec::with_capacity(terms.len() + 1);
    if let Some(kind) = kind {
        parts.push(kind.filter()?);
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
    if parts.is_empty() || (kind.is_some() && parts.len() == 1) {
        None
    } else {
        Some(parts.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Start query without the dot-folder exclusion every query ends with.
    fn plain(query: &str) -> &str {
        query
            .strip_suffix(&format!(" {SKIP_DOT_FOLDERS}"))
            .expect("dot folders skipped")
    }

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
        assert_eq!(plain(&parsed.query), "ferrow-native-query-proof");
    }

    #[test]
    fn parses_start_menu_documents_tab_19_columns_and_multi_term_contains() {
        let sql = format!(
            "SELECT TOP 30 {DOCS_TAB_PROJECTION} FROM \"SystemIndex\" WHERE (CONTAINS(\"System.ItemNameDisplay\", '\"budget*\" AND \"2026*\"', 2057) OR CONTAINS(*, '\"budget*\" AND \"2026*\"', 2057)) ORDER BY \"System.Search.Rank\" DESC"
        );
        let parsed = parse_windows_search_sql(&sql).expect("valid Documents tab query");
        assert_eq!(parsed.columns.len(), 19);
        assert_eq!(parsed.limit, 30);
        assert_eq!(plain(&parsed.query), "budget 2026");

        // The real Documents tab also restricts `System.Kind` to
        // documents: Ferrow answers it with the document extensions.
        let with_kind = format!(
            "SELECT TOP 30 {DOCS_TAB_PROJECTION} FROM \"SystemIndex\" WHERE (CONTAINS(\"System.ItemNameDisplay\", '\"budget*\" AND \"2026*\"', 2057) OR CONTAINS(*, '\"budget*\" AND \"2026*\"', 2057)) AND CONTAINS(\"System.Kind\", '\"document\"') ORDER BY \"System.Search.Rank\" DESC"
        );
        let parsed = parse_windows_search_sql(&with_kind).expect("Documents tab query parses");
        assert!(
            parsed.query.starts_with("ext:") && parsed.query.contains(";pdf;"),
            "{}",
            parsed.query
        );
        assert!(
            plain(&parsed.query).ends_with(" budget 2026"),
            "{}",
            parsed.query
        );
    }

    /// Windows 11 (2026) Start query: a `WITH ... AS #alias` column group,
    /// ranked CONTAINS branches and the location scope as `ReuseWhere`.
    #[test]
    fn parses_column_group_rank_and_reuse_where() {
        let sql = format!(
            "SELECT TOP 30 {ALL_TAB_PROJECTION} FROM SystemIndex WHERE WITH (System.ItemNameDisplay) AS #MRProps ((CONTAINS(#MRProps, '\"ferrowd\"', 1041) RANK BY COERCION(ABSOLUTE, 990)) OR (CONTAINS(#MRProps, '\"ferrowd*\"', 1041) RANK BY COERCION(ABSOLUTE, 980))) AND ReuseWhere(147)  ORDER BY \"System.Search.Rank\" DESC, \"System.DateModified\" DESC"
        );
        let parsed = parse_windows_search_sql(&sql).expect("Windows 11 Start query parses");
        assert_eq!(parsed.columns.len(), 15);
        assert_eq!(parsed.limit, 30);
        assert_eq!(plain(&parsed.query), "ferrowd");
        assert_eq!(parsed.rank_term.as_deref(), Some("ferrowd"));
        assert_eq!(parsed.prefix_query.as_deref().map(plain), Some("ferrowd*"));

        // A dotted query adds word-split branches; every branch implies
        // the weakest (`default` AND `rdp`), so the OR is exactly that.
        let sql = format!(
            "SELECT TOP 30 {ALL_TAB_PROJECTION} FROM SystemIndex WHERE WITH (System.ItemNameDisplay) AS #MRProps ((CONTAINS(#MRProps, '\"default.rdp\"', 1041) RANK BY COERCION(ABSOLUTE, 990)) OR (CONTAINS(#MRProps, '\"default.rdp*\"', 1041) RANK BY COERCION(ABSOLUTE, 980)) OR (CONTAINS(#MRProps, '\"default\" AND \"rdp\"', 1041) RANK BY COERCION(ABSOLUTE, 600)) OR (CONTAINS(#MRProps, '\"default*\" AND \"rdp*\"', 1041) RANK BY COERCION(ABSOLUTE, 550))) AND ReuseWhere(177)  ORDER BY \"System.Search.Rank\" DESC, \"System.DateModified\" DESC"
        );
        let parsed = parse_windows_search_sql(&sql).expect("dotted Start query parses");
        assert_eq!(plain(&parsed.query), "default rdp");
        assert_eq!(
            parsed.prefix_query.as_deref().map(plain),
            Some("default* rdp")
        );

        // The All tab's Documents and Photos groups: one query per kind.
        for (kind, ext) in [("document", ";pdf;"), ("picture", ";png;")] {
            let sql = format!(
                "SELECT TOP 30 {ALL_TAB_PROJECTION} FROM SystemIndex WHERE WITH (System.ItemNameDisplay) AS #MRProps ((CONTAINS(#MRProps, '\"readme\"', 1041) RANK BY COERCION(ABSOLUTE, 990)) OR (CONTAINS(#MRProps, '\"readme*\"', 1041) RANK BY COERCION(ABSOLUTE, 980))) AND \"System.Kind\" = '{kind}' AND ReuseWhere(280)  ORDER BY \"System.Search.Rank\" DESC, \"System.DateModified\" DESC"
            );
            let parsed = parse_windows_search_sql(&sql).expect("kind group query parses");
            assert!(
                parsed.query.starts_with("ext:") && parsed.query.contains(ext),
                "{}",
                parsed.query
            );
            assert!(
                plain(&parsed.query).ends_with(" readme"),
                "{}",
                parsed.query
            );
            let prefix = parsed.prefix_query.expect("prefix query");
            assert!(
                prefix.contains(ext) && plain(&prefix).ends_with(" readme*"),
                "{prefix}"
            );
        }

        for unsupported in [
            // An alias over a column Ferrow cannot search.
            "SELECT TOP 30 path FROM SystemIndex WHERE WITH (System.Title) AS #P CONTAINS(#P, '\"a*\"')",
            // An alias that was never defined.
            "SELECT TOP 30 path FROM SystemIndex WHERE CONTAINS(#P, '\"a*\"')",
            // The scope alone has no search terms.
            "SELECT TOP 30 path FROM SystemIndex WHERE ReuseWhere(147)",
            // Scope as an OR branch is a union, not a restriction.
            "SELECT TOP 30 path FROM SystemIndex WHERE CONTAINS(System.FileName, '\"a*\"') OR ReuseWhere(147)",
            "SELECT TOP 30 path FROM SystemIndex WHERE CONTAINS(System.FileName, '\"a*\"') AND ReuseWhere(x)",
        ] {
            assert!(
                parse_windows_search_sql(unsupported).is_none(),
                "should fall back: {unsupported}"
            );
        }
    }

    #[test]
    fn handles_mixed_grouped_and_kind_predicates() {
        // Mixed CONTAINS + LIKE under AND: both term sets survive.
        let sql = "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE CONTAINS(System.FileName, '\"alpha*\"') AND System.FileName LIKE '%beta%'";
        let parsed = parse_windows_search_sql(sql).expect("mixed AND parses");
        assert_eq!(plain(&parsed.query), "alpha beta");

        // Grouped OR of identical term sets ANDed with another term.
        let sql = "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE (CONTAINS(System.FileName, '\"alpha*\"') OR System.FileName LIKE '%alpha%') AND CONTAINS(System.FileName, '\"beta*\"')";
        let parsed = parse_windows_search_sql(sql).expect("identical OR branches parse");
        assert_eq!(plain(&parsed.query), "alpha beta");

        // Precedence: AND binds tighter than OR; identical conjunctions
        // on both sides still reduce to one conjunction.
        let sql = "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE CONTAINS(System.FileName, '\"alpha*\"') AND CONTAINS(System.FileName, '\"beta*\"') OR CONTAINS(System.FileName, '\"beta*\"') AND CONTAINS(System.FileName, '\"alpha*\"')";
        let parsed = parse_windows_search_sql(sql).expect("equal conjunctions parse");
        assert_eq!(plain(&parsed.query), "alpha beta");

        // The exact folder restriction, in both shell spellings.
        for sql in [
            "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE CONTAINS(System.FileName, '\"alpha*\"') AND CONTAINS(\"System.Kind\", '\"folder\"')",
            "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE CONTAINS(System.FileName, '\"alpha*\"') AND System.Kind = 'folder'",
            "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE \"System.Kind\" = 'FOLDER' AND CONTAINS(System.FileName, '\"alpha*\"')",
        ] {
            let parsed = parse_windows_search_sql(sql)
                .unwrap_or_else(|| panic!("folder restriction parses: {sql}"));
            assert_eq!(plain(&parsed.query), "folder: alpha", "for: {sql}");
        }

        for unsupported in [
            // Grouped OR of different terms cannot become an AND.
            "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE (CONTAINS(System.FileName, '\"alpha*\"') OR CONTAINS(System.FileName, '\"beta*\"')) AND CONTAINS(System.FileName, '\"gamma*\"')",
            // Precedence makes this alpha OR (beta AND gamma).
            "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE CONTAINS(System.FileName, '\"alpha*\"') OR CONTAINS(System.FileName, '\"beta*\"') AND CONTAINS(System.FileName, '\"gamma*\"')",
            // Kind values Ferrow cannot express exactly.
            "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE CONTAINS(System.FileName, '\"alpha*\"') AND System.Kind = 'email'",
            // Two different kinds: no item is both.
            "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE CONTAINS(System.FileName, '\"alpha*\"') AND System.Kind = 'picture' AND System.Kind = 'document'",
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
    fn rejects_deeply_nested_or_oversized_predicates() {
        // Nesting far beyond the cap must fall back to native, not
        // recurse until the SearchHost thread overflows its stack
        // (an abort `catch_unwind` cannot recover from).
        let leaf = "CONTAINS(System.FileName, '\"alpha*\"')";
        let deep = format!("{}{leaf}{}", "(".repeat(5_000), ")".repeat(5_000));
        let sql = format!("SELECT TOP 30 System.ItemName FROM SystemIndex WHERE {deep}");
        assert!(parse_windows_search_sql(&sql).is_none());

        // Nesting shell queries actually use still parses.
        let shallow = format!("{}{leaf}{}", "(".repeat(8), ")".repeat(8));
        let sql = format!("SELECT TOP 30 System.ItemName FROM SystemIndex WHERE {shallow}");
        let parsed = parse_windows_search_sql(&sql).expect("shallow nesting parses");
        assert_eq!(plain(&parsed.query), "alpha");

        // Oversized input is rejected before any parsing work.
        let sql = format!(
            "SELECT TOP 30 System.ItemName FROM SystemIndex WHERE {leaf} AND {}",
            "x".repeat(MAX_SQL_LEN)
        );
        assert!(parse_windows_search_sql(&sql).is_none());
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
        assert_eq!(plain(&parsed.query), "cannot notes");
    }

    #[test]
    fn parses_like_query_and_rejects_wildcards_or_empty() {
        let sql = "SELECT TOP 12 System.ItemName FROM SystemIndex WHERE System.FileName LIKE '%report-2026%'";
        let parsed = parse_windows_search_sql(sql).expect("valid LIKE query");
        assert_eq!(parsed.columns, vec!["System.ItemName"]);
        assert_eq!(parsed.limit, 12);
        assert_eq!(plain(&parsed.query), "report-2026");

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
