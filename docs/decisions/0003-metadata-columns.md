# ADR 0003: optional file metadata and ready-to-use sort orders

Status: implementation decision for FR-14 (2026-11-07). Performance
acceptance remains conditional on hosted measurements.

## Representation and public contract

Each enabled column owns one dense value array and a packed validity bitmap
(one bit per entry). A disabled column owns no arrays, including no sort array.
Missing or unsupported values are unknown, never zero or the current time.
`EntryMetadata` transports optional values; `Index::metadata(id)` and
`Index::columns_present()` expose them to consumers. Builder callers choose
columns explicitly; the portable walker defaults to size and modified time.

| Column | Representation | Range and precision | Value bytes/entry |
|---|---|---|---:|
| Size | `u64` | Full 0 through 2^64-1 bytes | 8 |
| Modified | `i64` | Signed Unix-epoch 100 ns ticks, about +/-29,227 years | 8 |
| Created | `i64` | Same; unsupported birth time is unknown | 8 |
| Accessed | `i64` | Same | 8 |
| Attributes | `u32` | Windows attribute bits; Unix complete mode including file type | 4 |

Times retain 100 ns precision. Sub-100 ns fractions are truncated toward the
earlier instant, including before 1970. Out-of-range times remain unknown.
This preserves seconds for display and orders representable timestamps to
100 ns. Attributes describe the indexed entry, including a symlink itself;
following a link controls traversal only. Metadata collection errors are
reported while the entry remains searchable with unknown metadata.

## Sorting and cost

Name order remains precomputed. Each enabled sortable metadata column stores
two `u32` permutations (8 bytes/entry), with the primary value ascending or
descending and ties always in ascending folded-name/raw-name/id order.
Unknown values sort last in either direction. Attributes are display-only.
An unavailable sort is a typed query outcome, never a silent name fallback.
Name descending can traverse the existing name permutation backward.

Including validity, each sortable column costs **16.125 bytes/entry**;
attributes cost **4.125**. Default size + modified cost **32.25 bytes/entry**
(161,250,000 bytes at 5 M, plus bounded vector/object overhead). The existing
name/path budget is accounted separately. The last reviewed fixture retained
49.582959 bytes/entry without metadata; this layout predicts about
**81.832959 bytes/entry total**, subject to actual capacity measurements.

Precomputing both directions avoids building a permutation, reversing equal
groups, or taking a cache lock during a search. All sorting happens during
worker-side freeze; loaded permutations are validated rather than rebuilt.
Changing sort selects a ready immutable slice and retains the existing bounded,
cancellable first-page scheduler. Common and medium-rare modified-descending
pages must meet 16 ms at 5 M; rare/no-hit behavior remains tracked by FR-8.

A lazy first-use alternative saves 8 bytes/entry per unused order pair but
requires a pending-sort state and background cache publication. CI compares
freeze time with isolated permutation-build time and reports the memory saved
by the lazy alternative. Those measurements can disprove precomputation if
load/memory cost is unacceptable. No unmeasured latency claim is acceptance.
The upper bound is zero permutation-building work on the search path; the
cheapest disproof is the hosted modified-sort page measurement on the existing
fixture. No local performance workload is permitted.

## Persistence

Format v2 appends a column-presence bitmask and enabled arrays/permutations to
the v1 payload. The mask is checksum-covered. The bounded header still carries
counts; exact total length is checked before payload allocation using the mask.
Unknown mask bits, noncanonical validity bits/unknown slots, invalid ids,
duplicate ids and misordered permutations are corrupt-file errors.
All existing name, folded-arena, parent-cycle and checksum checks remain.
v1 files are rejected with a clear unsupported-version/rebuild message. No
metadata is fabricated. A rebuild explicitly creates the enabled columns.

## Platform scan cost

On Windows `DirEntry::metadata()` returns the size, times and attribute bits
already present in directory enumeration data, without an additional syscall:
[Rust standard-library contract](https://doc.rust-lang.org/std/fs/struct.DirEntry.html#method.metadata).
The walker reuses that metadata for hidden/reparse classification. Root
metadata and existing exclude/identity operations are separate from this claim.

On Unix directory enumeration does not supply these fields. An enabled column
set requires one `DirEntry::metadata()` (`lstat`/`statx`) per retained candidate;
all enabled fields come from that one record. The empty column set makes no
metadata request for optional columns. Existing traversal/exclude checks stay
unchanged. CI measures the same fixture tree with none and default columns,
reporting entry/error equality and scan-time delta, never timing assertions in
unit tests. M2d's future MFT backend can supply these fields during enumeration.

## Performance gates

Existing paired metrics and their inventory remain required. New
modified-descending engine/IPC observations are mandatory head-only evidence
until main can supply that sort. Invalid or incomplete evidence fails closed.
Report core heap and default-column heap separately; preserve the paired core
memory gate. A default-column load regression against the section 3 target
requires parent review with the hosted numbers before acceptance.
