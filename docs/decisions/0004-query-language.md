# ADR 0004: bounded query language

Status: accepted for M2g (FR-15); metadata predicates are Part 2.

## Grammar and meaning

```text
query   := or | empty
or      := and ("|" and)*
and     := unary (whitespace unary)*
unary   := "!"* ("<" or ">" | modifier* value)
modifier:= "case:" | "path:" | "wholeword:" | "file:" | "folder:" | "regex:"
value   := bare | '"' literal '"'
```

NOT binds strongest, then AND, then OR. Groups may nest. Adjacent values
require whitespace (group/OR punctuation needs none). Empty input matches
everything; empty terms/groups and missing operands are errors. Quotes
protect operators, whitespace and wildcards: `"a*b"` is a literal substring.
There is no backslash escape syntax; backslashes remain path/regex bytes.
Quote a regex containing spaces, `!`, `|`, `<` or `>`.

A bare term containing `*` or `?` matches the **whole** selected name/path.
`*` consumes any run, including empty; `?` consumes one Unicode scalar.
Consecutive stars are equivalent to one star. Other terms are substrings.
Invalid filename bytes remain searchable literally; `?` refuses invalid
UTF-8 rather than treating a byte as a Unicode scalar. `*` accepts raw bytes.
Default matching uses the index's length-preserving simple case folding,
including its documented limitations. `case:` selects original bytes.

Modifiers stack on one term in any order; `file:` and `folder:` test the
directory flag (using both is an error). They may stand alone as type
predicates. `path:` selects the indexed full path, joined with the native
separator, without filesystem I/O or path canonicalization. `wholeword:`
requires both match edges to be next to the start/end or a non-word scalar;
letters, numbers and underscore are word characters. Regex syntax and
matching use the permissively licensed Rust `regex` crate, including its
Unicode case-insensitive semantics and inline flags. Plain and wildcard
terms retain the index fold; regex character classes follow the crate's
Unicode rules, including folds whose byte length differs.

Current predicates: `ext:log;txt` compares the final nonempty extension,
excluding directories and leading-dot-only names; `parent:"/some/folder"`
selects direct children by exact indexed parent path; `depth:2` counts parent
edges from an indexed root (root depth zero). Modifiers may qualify these
predicates where meaningful (`case:parent:...`, `file:depth:2`); `regex:`,
`path:` and `wholeword:` apply only to text patterns.

## Public API and Part 2

The AST has term leaves, AND, OR and NOT. Each term carries its source byte
span and typed pattern/predicate; parse errors expose `span()` for GUI
underlining. `highlight_ranges(query, name)` returns original name byte ranges
for positive matching name terms; path/type/filter/negated terms do not
invent name highlights. The caller runs it only on visible rows.

Part 2 adds a `Filter` variant for each typed metadata predicate:

```text
size: comparison? quantity | quantity ".." quantity
quantity := integer (b | kb | mb | gb | tb)?   # decimal units
dm: | dc: | da: (today | yesterday | last7days | ISO-date [".." ISO-date])
attrib: attribute (";" attribute)*
comparison := ">" | ">=" | "<" | "<=" | "="
```

The reserved prefixes produce an explicit unsupported-filter error today.
Part 2 introduces typed size bounds, date field/range/relative-day values and
attribute sets alongside the current `Filter` enum after M2f defines columns.
Do not silently reinterpret a reserved filter as a filename substring.

## Performance and bounded work

Keep the plain AND substring fast path. Compile regexes at parse time once
per query with a 1 MiB compiled-size and DFA-cache limit. A required literal
prefix, when available, is extracted from regex syntax and checked by memmem
before regex work; alternations/optional prefixes must never cause rejection
based on a non-required literal. Wildcards precheck anchored suffix/prefix
and ordered literal pieces with memmem before scalar-aware matching.

Evaluate name-only AND children before path children, including when `path:`
appears first in the input. Rebuild paths only for surviving candidates,
with per-worker reused path, folded path and ancestor buffers; no index or
SearchOptions changes. Check cancellation while traversing parent chains.
Keep existing ordered first-page admission and cancellation contracts.
Scalar wildcard retry checks cancellation every 256 steps, so an adversarial
pattern can be interrupted inside a single long name.
An anchored wildcard's required name prefix also narrows the existing sorted
name order by two binary searches. AND may use any required prefix; OR only
uses a shared required prefix, and NOT never narrows. Non-ASCII sensitive
prefixes do not assume Unicode folding on invalid-byte names.

Input is limited to 4096 bytes, 256 leaves and 32 nested groups/NOT nodes;
overflow returns a precise byte span. These bounds prevent recursive parser,
matcher or drop-stack exhaustion and cap regex preparation. No filesystem
operations occur in query evaluation. The existing 100k oracle and bounded
cancellation probes are the cheapest disproof of correctness; hosted 5M
paired measurements are the performance evidence, never local benchmarks.

## CI gate migration (parent approved)

Retain all 26 existing paired metrics and their count-equivalence and
regression checks. The main IPC arm explicitly uses the six legacy queries.
Require all 20 additional head-only measurements: first page and count, both
in-process and IPC, for `*.log`, `report*2024*`, `path:docs report`,
`ext:txt;md` and `<a|re> !tmp`. Report 46 metrics total, identifying new
language observations without a fabricated baseline. Common wildcard queries
(`*.log`, `report*2024*` when at least 100 hits exist) must return their first
page within 16 ms in both modes. Missing/partial observations fail closed.
