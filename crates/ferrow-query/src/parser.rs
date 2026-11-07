use regex::bytes::{Regex, RegexBuilder};
use regex_syntax::hir::literal::Extractor;
use std::{fmt, ops::Range, sync::Arc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    Term(usize),
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
}

/// Metadata predicates (ADR 0004, Part 2). They need the matching index
/// column; see [`Query::required_columns`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataFilter {
    /// Inclusive byte bounds. Matches files only.
    Size { min: Option<u64>, max: Option<u64> },
    /// Half-open `[start, end)` range in Unix 100-ns ticks, resolved
    /// against the local clock when the query is parsed.
    Date {
        field: DateField,
        start: Option<i64>,
        end: Option<i64>,
    },
    /// Every one of these attribute bits must be set.
    Attributes(u32),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DateField {
    Modified,
    Created,
    Accessed,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    Extension(Vec<String>),
    Parent(String),
    Depth(u32),
    Metadata(MetadataFilter),
}

#[derive(Debug, Clone)]
pub struct RegexPlan {
    pub(crate) regex: Arc<Regex>,
    pub(crate) prefix: Vec<u8>,
}
impl PartialEq for RegexPlan {
    fn eq(&self, other: &Self) -> bool {
        self.regex.as_str() == other.regex.as_str() && self.prefix == other.prefix
    }
}
impl Eq for RegexPlan {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pattern {
    Literal,
    WordLiteral(RegexPlan),
    Wildcard,
    Regex(RegexPlan),
    Filter(Filter),
    EntryType,
}

/// User regexes may nest groups this deep.
const REGEX_NEST_LIMIT: u32 = 32;
/// Bound for the normalised wholeword form: its HIR display adds
/// non-capturing groups around concatenations, so it needs more depth
/// than the user's source, but stays bounded.
const NORMALISED_NEST_LIMIT: u32 = 4 * REGEX_NEST_LIMIT + 8;

fn compile_regex(text: &str, case: bool, word: bool) -> Result<RegexPlan, String> {
    // `fold` is the default case-insensitivity applied to user syntax.
    // The normalised wholeword form already has every case decision
    // (defaults and inline flags) applied, so it is compiled with no
    // default folding at all.
    let (pattern, fold, nest) = if word {
        // Wrap the parsed form, not the raw text: in verbose mode
        // (`(?x)`) a `#` comment would otherwise swallow the closing
        // group. The user's syntax is validated at the normal depth;
        // the generated form gets its own bound.
        let inner = regex_syntax::ParserBuilder::new()
            .case_insensitive(!case)
            .utf8(false)
            .nest_limit(REGEX_NEST_LIMIT)
            .build()
            .parse(text)
            .map_err(|e| format!("invalid regex: {e}"))?;
        (
            format!(r"(?u:\b{{start-half}})(?:{inner})(?u:\b{{end-half}})"),
            false,
            NORMALISED_NEST_LIMIT,
        )
    } else {
        (text.to_owned(), !case, REGEX_NEST_LIMIT)
    };
    let compiled = RegexBuilder::new(&pattern)
        .case_insensitive(fold)
        .size_limit(1 << 20)
        .dfa_size_limit(1 << 20)
        .nest_limit(nest)
        .build()
        .map_err(|e| format!("invalid regex: {e}"))?;
    let hir = regex_syntax::ParserBuilder::new()
        .case_insensitive(fold)
        .utf8(false)
        .nest_limit(nest)
        .build()
        .parse(&pattern)
        .map_err(|e| format!("invalid regex: {e}"))?;
    let seq = Extractor::new().extract(&hir);
    Ok(RegexPlan {
        regex: Arc::new(compiled),
        prefix: seq.longest_common_prefix().unwrap_or_default().to_vec(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    pub text: String,
    pub negated: bool,
    pub case_sensitive: bool,
    pub path: bool,
    pub wholeword: bool,
    /// Some(true) means directories; Some(false) means files.
    pub directory: Option<bool>,
    pub pattern: Pattern,
    pub span: Range<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub terms: Vec<Term>,
    pub expression: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    UnclosedQuote { at: usize },
    EmptyTerm { at: usize },
    Syntax { span: Range<usize>, message: String },
}
impl ParseError {
    pub fn span(&self) -> Range<usize> {
        match self {
            Self::UnclosedQuote { at } | Self::EmptyTerm { at } => *at..at + 1,
            Self::Syntax { span, .. } => span.clone(),
        }
    }
}
impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnclosedQuote { at } => write!(f, "unclosed quote starting at position {at}"),
            Self::EmptyTerm { at } => write!(f, "empty search term at position {at}"),
            Self::Syntax { span, message } => {
                write!(f, "{message} at bytes {}..{}", span.start, span.end)
            }
        }
    }
}
impl std::error::Error for ParseError {}

impl Query {
    /// Index columns the query's metadata filters read. Entries of an
    /// index without them never match those filters.
    pub fn required_columns(&self) -> ferrow_index::ColumnSet {
        use ferrow_index::ColumnSet;
        self.terms.iter().fold(ColumnSet::NONE, |set, term| {
            set.union(match &term.pattern {
                Pattern::Filter(Filter::Metadata(m)) => match m {
                    MetadataFilter::Size { .. } => ColumnSet::SIZE,
                    MetadataFilter::Date { field, .. } => match field {
                        DateField::Modified => ColumnSet::MODIFIED,
                        DateField::Created => ColumnSet::CREATED,
                        DateField::Accessed => ColumnSet::ACCESSED,
                    },
                    MetadataFilter::Attributes(_) => ColumnSet::ATTRIBUTES,
                },
                _ => ColumnSet::NONE,
            })
        })
    }

    pub fn parse(input: &str) -> Result<Self, ParseError> {
        let mut p = Parser {
            input,
            at: 0,
            terms: Vec::new(),
        };
        if input.len() > 4096 {
            return Err(p.error(0, input.len(), "query exceeds 4096 bytes"));
        }
        p.space();
        let expression = if p.at == input.len() {
            None
        } else {
            Some(p.or(0)?)
        };
        if p.at != input.len() {
            return Err(p.error(p.at, p.at + 1, "unexpected token"));
        }
        Ok(Self {
            terms: p.terms,
            expression,
        })
    }
}

struct Parser<'a> {
    input: &'a str,
    at: usize,
    terms: Vec<Term>,
}
impl Parser<'_> {
    fn error(&self, start: usize, end: usize, message: &str) -> ParseError {
        ParseError::Syntax {
            span: self.input.floor_char_boundary(start.min(self.input.len()))
                ..self.input.ceil_char_boundary(end.min(self.input.len())),
            message: message.into(),
        }
    }
    fn byte(&self) -> Option<u8> {
        self.input.as_bytes().get(self.at).copied()
    }
    fn space(&mut self) -> bool {
        let start = self.at;
        while self.byte().is_some_and(|b| b.is_ascii_whitespace()) {
            self.at += 1;
        }
        self.at > start
    }
    fn or(&mut self, depth: usize) -> Result<Expr, ParseError> {
        let mut parts = vec![self.and(depth)?];
        while self.byte() == Some(b'|') {
            self.at += 1;
            self.space();
            parts.push(self.and(depth)?);
        }
        Ok(if parts.len() == 1 {
            parts.pop().unwrap()
        } else {
            Expr::Or(parts)
        })
    }
    fn and(&mut self, depth: usize) -> Result<Expr, ParseError> {
        let mut parts = vec![self.unary(depth)?];
        loop {
            let separated = self.space();
            if matches!(self.byte(), None | Some(b'|' | b'>')) {
                break;
            }
            if !separated {
                return Err(self.error(self.at, self.at + 1, "expected whitespace or operator"));
            }
            parts.push(self.unary(depth)?);
        }
        Ok(if parts.len() == 1 {
            parts.pop().unwrap()
        } else {
            Expr::And(parts)
        })
    }
    fn unary(&mut self, depth: usize) -> Result<Expr, ParseError> {
        if self.at == self.input.len() {
            return Err(self.error(self.at, self.at, "missing operand"));
        }
        if depth >= 32 {
            return Err(self.error(self.at, self.at + 1, "nesting exceeds 32"));
        }
        let start = self.at;
        if self.byte() == Some(b'!') {
            self.at += 1;
            if matches!(self.byte(), None | Some(b' ' | b'|' | b'>')) {
                return Err(ParseError::EmptyTerm { at: start });
            }
            let expr = self.unary(depth + 1)?;
            return Ok(match expr {
                Expr::Term(i) => {
                    self.terms[i].negated = !self.terms[i].negated;
                    Expr::Term(i)
                }
                other => Expr::Not(Box::new(other)),
            });
        }
        if self.byte() == Some(b'<') {
            self.at += 1;
            self.space();
            let expr = self.or(depth + 1)?;
            if self.byte() != Some(b'>') {
                return Err(self.error(start, self.at, "unclosed group"));
            }
            self.at += 1;
            return Ok(expr);
        }
        self.term()
    }
    fn term(&mut self) -> Result<Expr, ParseError> {
        let start = self.at;
        let (mut case, mut path, mut word, mut regex, mut directory) =
            (false, false, false, false, None);
        loop {
            let rest = &self.input[self.at..];
            let (prefix, kind) = [
                ("case:", 0),
                ("path:", 1),
                ("wholeword:", 2),
                ("regex:", 3),
                ("file:", 4),
                ("folder:", 5),
            ]
            .into_iter()
            .find(|(s, _)| rest.starts_with(s))
            .unwrap_or(("", 6));
            match kind {
                0 => case = true,
                1 => path = true,
                2 => word = true,
                3 => regex = true,
                4 | 5 => {
                    let wanted = kind == 5;
                    if directory.is_some_and(|d| d != wanted) {
                        return Err(self.error(
                            start,
                            self.at + prefix.len(),
                            "conflicting entry types",
                        ));
                    }
                    directory = Some(wanted);
                }
                _ => break,
            }
            self.at += prefix.len();
        }
        let mut filter = None;
        if !regex {
            for prefix in [
                "ext:", "parent:", "depth:", "size:", "dm:", "dc:", "da:", "attrib:",
            ] {
                if self.input[self.at..].starts_with(prefix) {
                    filter = Some(prefix);
                    self.at += prefix.len();
                    break;
                }
            }
        }
        let quoted = self.byte() == Some(b'"');
        let value_start = self.at + usize::from(quoted);
        if quoted {
            self.at += 1;
            while self.byte().is_some_and(|b| b != b'"') {
                self.at += 1;
            }
            if self.byte().is_none() {
                return Err(ParseError::UnclosedQuote {
                    at: value_start - 1,
                });
            }
        } else {
            // Size and date values may start with a comparison
            // (`size:>10mb`, `dm:<=2026-01-01`); elsewhere `<` and `>`
            // group terms.
            if matches!(filter, Some("size:" | "dm:" | "dc:" | "da:")) {
                while self.byte().is_some_and(|b| b"<>=".contains(&b)) {
                    self.at += 1;
                }
            }
            while self
                .byte()
                .is_some_and(|b| !b.is_ascii_whitespace() && !b"!|<>\"".contains(&b))
            {
                self.at += 1;
            }
        }
        let text = self.input[value_start..self.at].to_string();
        if quoted {
            self.at += 1;
        }
        let pattern = if text.is_empty() {
            if directory.is_some() && !quoted && filter.is_none() && !regex && !path && !word {
                Pattern::EntryType
            } else {
                return Err(ParseError::EmptyTerm { at: start });
            }
        } else if let Some(prefix) = filter {
            if path || word || regex {
                return Err(self.error(start, self.at, "text modifier on filter"));
            }
            Pattern::Filter(match prefix {
                "ext:" => {
                    let values: Vec<_> = text.split(';').map(str::to_string).collect();
                    if values
                        .iter()
                        .any(|v| v.is_empty() || v.contains(['/', '\\', '.']))
                    {
                        return Err(self.error(value_start, self.at, "invalid extension list"));
                    }
                    Filter::Extension(values)
                }
                "parent:" => Filter::Parent(text.clone()),
                "size:" => {
                    let (min, max) = crate::filters::parse_size(&text)
                        .map_err(|e| self.error(value_start, self.at, &e))?;
                    Filter::Metadata(MetadataFilter::Size { min, max })
                }
                "dm:" | "dc:" | "da:" => {
                    let field = match prefix {
                        "dm:" => DateField::Modified,
                        "dc:" => DateField::Created,
                        _ => DateField::Accessed,
                    };
                    let (start, end) =
                        crate::filters::parse_date(&text, &crate::filters::Clock::local())
                            .map_err(|e| self.error(value_start, self.at, &e))?;
                    Filter::Metadata(MetadataFilter::Date { field, start, end })
                }
                "attrib:" => Filter::Metadata(MetadataFilter::Attributes(
                    crate::filters::parse_attributes(&text)
                        .map_err(|e| self.error(value_start, self.at, &e))?,
                )),
                _ => Filter::Depth(text.parse().map_err(|_| {
                    self.error(value_start, self.at, "depth must be a nonnegative u32")
                })?),
            })
        } else if regex {
            Pattern::Regex(
                compile_regex(&text, case, word)
                    .map_err(|e| self.error(value_start, self.at, &e))?,
            )
        } else if !quoted && text.contains(['*', '?']) {
            Pattern::Wildcard
        } else if word {
            let mut literal = text.as_bytes().to_vec();
            if !case {
                ferrow_index::fold_name(text.as_bytes(), &mut literal);
            }
            let escaped = regex::escape(&String::from_utf8_lossy(&literal));
            Pattern::WordLiteral(
                compile_regex(&escaped, true, true)
                    .map_err(|e| self.error(value_start, self.at, &e))?,
            )
        } else {
            Pattern::Literal
        };
        if self.terms.len() == 256 {
            return Err(self.error(start, self.at, "query exceeds 256 terms"));
        }
        let i = self.terms.len();
        self.terms.push(Term {
            text,
            negated: false,
            case_sensitive: case,
            path,
            wholeword: word,
            directory,
            pattern,
            span: start..self.at,
        });
        Ok(Expr::Term(i))
    }
}
