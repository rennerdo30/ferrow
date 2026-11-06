# ADR 0003: optional file metadata and asynchronous sort preparation

Status: revised implementation decision for FR-14 (2026-11-07). The parent
preserved `b6d4c23` as the functional v2 checkpoint and authorized the measured
optimization below after Part 0. Performance acceptance requires hosted evidence.

## Columns and precision

Each enabled column owns one packed value array, a validity bitmap, and sparse
exact overflow arrays. Disabled columns own no arrays and perform no optional
scan work. Defaults are size and modified; created, accessed and attributes are
opt-in. Unknown values are never fabricated.

| Column | Packed representation | Exact overflow policy |
|---|---|---|
| Size | unsigned 40-bit bytes, 0 through 2^40-1 | Larger sizes use sorted entry ids plus exact u64 values |
| Modified/created/accessed | unsigned 32-bit Unix seconds, 1970 through 2106-02-07 06:28:15 UTC | Outside that interval, sorted entry ids plus exact i64 Unix 100ns ticks |
| Attributes | unsigned 32-bit Windows flags or Unix complete mode/type | No overflow needed |

The public `EntryMetadata` time unit remains signed Unix-epoch **100ns ticks**.
Representable packed dates are rounded down to whole seconds; outside the
packed interval the overflow table preserves the original value. Every
representable second sorts exactly; same-second packed values use the name tie
order. Unsupported or out-of-range OS timestamps remain unknown. A packed
maximum value with no overflow record is the legitimate maximum. Unknown slots
are canonical zero and all validity tail bits must be zero.

Size uses 40 rather than 32 bits because the existing metadata fixture spans
0..64GiB: a 32-bit representation would overflow almost every row. This decision
preserves that fixture distribution. The exact-overflow policy still supports
files through u64::MAX, negative dates, and dates after 2106.

## Sorting and memory target

Name order remains precomputed. A non-name sort is prepared on a dedicated
background thread on first use. It never uses the query Rayon pool. Queries
return name order with an explicit pending status until preparation completes;
no index-sized allocation, clone, sort or wait happens on the query thread.
Missing columns remain explicit unavailable-sort errors.

Each key caches **one** ascending permutation. Stable sorting starts from name
order, so ties retain folded-name/raw-name/id order. Descending iteration visits
value groups in reverse while retaining ascending names within a group; unknown
values remain last. Group boundaries are found without linear scans of a large
equal group. Query cancellation and bounded first-page admission remain intact.

Cached ids use 24 bits through 2^24 entries and 32 bits above that range. At 5M:

| Column | Values + validity | One ready order | Total bytes/entry |
|---|---:|---:|---:|
| Size | 5.125 | 3 | 8.125 |
| Each date | 4.125 | 3 | 7.125 |
| Attributes | 4.125 | 0 | 4.125 |
| Defaults, both orders ready | 9.25 | 6 | 15.25 |

Add 12 bytes per overflow and small fixed objects/capacity tails. Before either
sort is requested, defaults cost 9.25 bytes/entry. Above 2^24 entries, each
prepared order adds one further byte/entry. Background scratch and old snapshots
are reported separately from retained current-snapshot heap.

The parent targets are about **16 extra bytes/entry**, about **66 total**, and
at most about **15% default-column load overhead** over the same core index.
Common modified-descending pages must stay within 16ms. `report 2024` and no-hit
are accepted as FR-8 rare-hit cases for now, with their numbers still reported.
Targets are never weakened to obtain a green gate.

## Persistence and preparation

Format v3 stores the column mask, exact metadata payload length, packed arrays,
validity and sorted overflow ids/values, all checksum-covered. Exact file length
is validated before payload allocation. Packed widths, overflow counts, ranges,
strict id ordering, canonical values and bitmap tails are validated. Existing
parent-cycle, packed-name, folded-arena and name-permutation checks remain.

Metadata permutations are **not stored or constructed during load**. The v1
core format can load with metadata absent, supporting the exact shared fixture
used by the interleaved gate. The functional-checkpoint v2 format receives a
clear rebuild message. Fresh snapshots own fresh preparation state, so a cache
cannot outlive its source data or be applied to another snapshot.

Preparation exposes pending/ready/failure status and a completion ticket that a
worker may wait on to post a GUI event. No GUI polling timer is needed. The
checkpoint's persisted two-direction arrays remain the measured alternative:
32.25 extra B/entry and 1315ms load versus 684ms core, so they fail the new targets.
Lazy preparation is measured separately and queries remain usable while pending.

## Platform cost and cheapest disproof

On Windows directory enumeration already carries all five fields;
[`DirEntry::metadata`](https://doc.rust-lang.org/std/fs/struct.DirEntry.html#method.metadata)
requires no extra per-entry syscall. Roots and existing identity/exclude checks
are separate costs. Unix requests one lstat/statx-equivalent record per retained
candidate when any optional column is enabled. None requests no optional record.
M2d can provide these fields during MFT enumeration later.

The cheapest disproof is the unchanged hosted fixture: report cold and warmed
heap, compare warmed core/default loads with alternating samples, measure sort
preparation, and measure the six modified-descending queries in-process and over
IPC. Compare the same Unix tree with no columns/default columns. Stop with
numbers and trade-offs if the memory or load target cannot be reached. No local
benchmark, large fixture or full-suite run is allowed.
