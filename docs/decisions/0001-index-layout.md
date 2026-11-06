# ADR 0001: index layout and file format

Status: accepted for M1 (2026-11-06). Implements the design in
[`../milestones/M1-base.md`](../milestones/M1-base.md) and SPEC §3.

## Decision

### In-memory layout (`ferrow-index`)

The index is columnar (struct of arrays), one `Vec` per column:

| Column | Type | Meaning |
|---|---|---|
| `parent` | `u32` per entry | parent entry id; `u32::MAX` for roots |
| `name_off` | `u32` per entry | offset into the name arena |
| `name_len` | `u16` per entry | name length in bytes |
| `flags` | `u16` per entry | dir / hidden / symlink / name-not-UTF-8 |
| `arena` | `Vec<u8>` | all name bytes back to back |
| `folded_arena` | `Vec<u8>` | case-folded names, same offsets/lengths |
| `sorted` | `u32` per entry | entry ids in name-sort order |

Fixed columns cost 16 bytes per entry; the two arenas cost twice the mean
name length. With typical names this stays under the ~60 bytes/entry
target (measured in CI by the bench job; `Index::heap_bytes()` reports the
real total).

Root entries store the full root path as their name. Every other entry
stores one path component. Full paths are never stored; `path(id)` walks
the parent chain and joins the components into a caller buffer.

**Names are lossless.** On Unix the raw `OsStr` bytes are stored. On
Windows names are stored as WTF-8: valid UTF-16 encodes as plain UTF-8,
lone surrogates as their 3-byte forms, so every name round-trips exactly
(`ferrow-index/src/osname.rs`). The `NAME_NOT_UTF8` flag marks names that
are not valid UTF-8.

**Case folding is length-preserving.** The folded arena shares offsets and
lengths with the original arena, which halves the lookup work in the
matcher. `fold_name` applies Unicode simple case folding
(`CaseFolding.txt`, status C and S, via the MIT-licensed `casefold`
crate) and keeps every mapping whose UTF-8 byte length is unchanged.
That covers the full same-length equivalence classes: Latin, Greek and
Cyrillic pairs like `Ä`/`ä`, the sigma forms `Σ`/`σ`/`ς` (all fold to
`σ`), the micro sign `µ` → `μ`, and Cherokee, whose fold target is the
uppercase letter. Two groups do **not** fold in M1 and are a known
limitation for a later milestone: simple folds that change the byte
length (e.g. Kelvin sign U+212A → `k`, long s `ſ` → `s`) and all full
(multi-character) folds (e.g. `ß` → `ss`, `İ` → `i̇`). The query side
folds with the same function, so matches are consistent. Lifting the
limitation means a second offset/length column pair (+6 bytes/entry).

**Sort order** is by folded name bytes, then raw name bytes, then id —
deterministic on every platform and independent of locale.

**Building.** `IndexBuilder` is sharded (16 mutex shards, dense atomic
ids), so many threads can add entries in any order and parent ids stay
valid after `freeze()`. Freeze merges the shards, builds both arenas
(folding in parallel), and computes the sort order with a parallel sort.
After freeze the index is immutable and shared as `Arc<Index>`.

### File format (version 1)

One file, little-endian, no per-entry allocation on load:

| Field | Type | Notes |
|---|---|---|
| magic | `[u8; 8]` | `FERROWID` |
| version | `u32` | 1 |
| entry_count | `u32` | |
| arena_len | `u32` | both arenas have this length |
| checksum | `u32` | CRC-32 (IEEE) over everything after this field |
| `parent`, `name_off`, `name_len`, `flags` | columns | as above |
| `arena`, `folded_arena` | bytes | |
| `sorted` | `u32` × n | |

Loading first reads and validates the bounded header (magic, version,
counts), checks the exact expected total length against the file size
(with a 16 GiB ceiling), and only then reads the payload in one
sequential read. Trailing bytes are rejected; they are not covered by
trusting the checksum alone. After the checksum, the loader first
requires the canonical packed name layout the writer produces (names
contiguous in entry-id order, each `name_off` equal to the running sum
of previous lengths, the total exactly the arena length), checked with
checked arithmetic before any content validation; then every
parent/name/sort id is range-checked and parent cycles are rejected.
The derived data is **validated, not rebuilt**: the sort column must
be a permutation (no duplicate ids) in exact sort order, and every
folded name must equal `fold_name` of the raw name. Validation was
chosen over rebuilding on load because, under the packed layout, the
sum of all name lengths equals the arena length, so folding plus
adjacent sort-key comparisons cost O(n + arena bytes) in total — not
O(sum of referenced name lengths), which overlapping ranges would
allow — while rebuilding would pay a full parallel sort on every cold
start, against the load-time target. The derived-data validation
(folded-arena consistency and adjacent sort-order checks) runs in
parallel over entry-id-aligned chunks, which keeps each chunk's
arena access sequential for the fold check. `Index::load_with_timings`
reports the load as additive parts — read, checksum, decode,
structure validation, derived-data validation, plus an unattributed
remainder — and the CI bench summary publishes that breakdown. A crafted file with a valid checksum
but inconsistent derived data is rejected as corrupt. Any problem is a typed error, never a panic
(`ferrow-index/src/persist.rs`). Saving creates a unique temp file
beside the destination with exclusive creation (mode 0600 on Unix),
writes and syncs it, then renames it over the destination, so a crash
or a concurrent save never leaves a half-written index at the target
path and no unrelated sibling file is touched.

The load total covers file open and metadata through temporary payload disposal
and file close. CRC-32 is a hand-written, table-driven implementation with no
dependency. Cached-read measurements show checksum work can dominate load;
they do not establish disk throughput. Accelerating the same IEEE CRC is
format-compatible; changing checksum semantics requires a format-version bump.

### Exclude matching (`ferrow-walk`)

Excludes are matched by **filesystem identity**, never by path
spelling. (String comparison failed twice in review: verbatim `\\?\`
prefixes and WTF-8 components with non-ASCII case differences both
named the same directory under a different string.) At scan start
every exclude path that exists is resolved to its identity — Unix
`(st_dev, st_ino)`, Windows volume serial number plus the 128-bit
file id from `GetFileInformationByHandleEx(FileIdInfo)` (falling back
to the 64-bit file index from `GetFileInformationByHandle` where
128-bit ids are unsupported). A path that does not exist at scan
start is reported in the scan report as `exclude not found: …` and
otherwise ignored.

**Exclude contract.** An exclude names a filesystem object, resolved once at
scan start; links in the exclude path itself are resolved to their target.
An entry is excluded when its own location, or the location it resolves to,
lies inside an excluded directory, or is itself an excluded file. Being inside
means the excluded directory's identity appears among the identities of the
resolved location's ancestor chain. Excluded entries and everything below
them are not indexed, whatever spelling, link, junction or alias reaches them,
and whether following is on or off.

One location check applies to every root and to links and name-surrogate
reparse points (including junctions), even when they remain traversal leaves.
It uses `std::fs::canonicalize` to locate real ancestors, then compares their
identities, never the canonical strings. This also avoids treating a lexical
`..` detour as an ancestor. A link's containing location is checked as well as
its resolved target, so an alias inside an excluded directory cannot escape
the exclusion by pointing outside. Successful ancestor results are cached by
directory identity for the scan. Ordinary real parent-to-child directory
edges only check the child's identity: the parent chain has already passed.
The stored path spelling is preserved independently of these checks.

File excludes match by identity inside the directory that contains the
resolved exclude path; explicit file roots are checked directly. Identity
lookup failures on existing paths are counted with their reason, rather than
reported as missing excludes. Windows identity handles use Rust's
`OpenOptions` path conversion, attribute-only access, read/write/delete sharing
and backup semantics, and close through RAII.

Consequences of identity matching, by construction:

- Any spelling the filesystem resolves to the same object — normal
  vs verbatim, differently cased components, 8.3 names — excludes it.
  Case sensitivity is whatever the filesystem itself applies, so
  per-directory case-sensitive Windows directories are honoured
  exactly instead of approximated by folding.
- Symlinks and junctions resolve to their target's identity: reaching
  an excluded directory through an alias excludes it, and excluding
  an alias excludes its target.
- Directory hard links do not exist on the supported platforms; file
  hard links share one identity, so a file exclude also covers other
  hard-linked names of the same file in the same containing
  directory.

Cost: one identity lookup per ordinary directory (only while excludes are
configured or symlinks are followed; the same lookup feeds cycle detection),
plus per-file lookups only inside a directory that contains a file exclude.
Roots and aliases alone pay O(depth) ancestor lookups, with cached results
making repeated targets cheap. Files in general are never resolved.

### Limitations

- Excludes are start-of-scan identities, not retained handles or an atomic
  filesystem snapshot. Replacement, deletion and identity reuse during a
  scan can change the result; M1 is a best-effort walk.
- FAT, network and ReFS volumes with unstable, zero or non-unique identities
  need provider validation before support can be promised; this is later scope.
- File hard links in other containing directories stay indexed. M1 does not
  perform global per-file identity lookups.
- Test fixture creation failures fail tests. Platform exclusions use `cfg` or
  real `#[ignore = "reason"]` attributes. The Unix unreadable-directory test
  omits its permission-denial assertion when a privileged process can still
  read mode-000 directories, and prints an explicit ignored-assertion reason.

## Alternatives considered

- **Storing full paths**: simpler result rendering, but multiplies memory
  by path depth and breaks the bytes-per-entry target.
- **One arena, fold on the fly**: saves memory but pays folding on every
  keystroke for every entry — against P0 (speed first).
- **Separate folded offsets for full Unicode folding**: correct for the
  expanding cases, +6 bytes/entry and a second offset lookup per term.
  Revisit if the missed pairs matter in practice.
- **Per-entry structs instead of columns**: worse cache behaviour for the
  parallel scan, which only touches names and the sort order.
