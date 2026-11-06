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
trusting the checksum alone. After the checksum, every parent/name/sort
id is range-checked and parent cycles are rejected. The derived data
is **validated, not rebuilt**: the sort column must be a permutation
(no duplicate ids) in exact sort order, and every folded name must
equal `fold_name` of the raw name. Validation was chosen over
rebuilding on load because it is O(n) plus one pass over the arena,
while rebuilding would pay a full parallel sort on every cold start,
against the load-time target. A crafted file with a valid checksum
but inconsistent derived data is rejected as corrupt. Any problem is a typed error, never a panic
(`ferrow-index/src/persist.rs`). Saving creates a unique temp file
beside the destination with exclusive creation (mode 0600 on Unix),
writes and syncs it, then renames it over the destination, so a crash
or a concurrent save never leaves a half-written index at the target
path and no unrelated sibling file is touched.

CRC-32 is a hand-written, table-driven implementation: it is short, has
no dependency, and easily outruns the disk on load. If load ever becomes
CPU-bound at larger sizes, switching the checksum is a format-version
bump.

### Exclude matching (`ferrow-walk`)

Excludes are compared against absolute paths component by component: a
path is excluded when it equals an excluded path or lies under one.
The check applies to scan roots exactly as to children, so an excluded
directory cannot be re-included by naming it (or a directory under it)
as an explicit root. On **Windows the comparison is case-insensitive**
(components are case-folded with the same `fold_name` used for search),
matching the platform's usual path semantics; on Unix it is byte-exact.

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
