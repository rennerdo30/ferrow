# Research: the live delta limit (`MAX_DELTA` = 50,000) and the full rescan it triggers

Written against `5685bef` (main after beta.18), before any change. Part 1 of the plan in
section 4 is implemented by FR-42 part 1 (see `CHANGELOG.md`, `docs/service.md` and
`crates/ferrowd/examples/delta_limit_bench.rs`, the measurement example behind the figures
below); the compaction (section 3 (c), part 2) is not built yet.

## Short answer

1. **50,000 has no recorded rationale.** It is in the first live-index commit (`f92859a`,
   2026-11-10) with the comment "Rescan once the overlay holds this many live delta entries".
   ADR 0005 planned something else: an in-memory background re-freeze at
   `max(64 K entries, 2 % of the base)`. The code shipped a disk rescan instead and the ADR was
   never revised. The only stated reasons for a bound (comment on `HARD_DELTA`, `28949fb`) are
   "memory and per-batch view cost". Both are real and measured below.
2. **The limit is not scaled to the index.** 50 k is 5 % of a 1 M index but 0.19 % of 27 M, so
   the bigger the PC, the more often a rescan is requested and the more it costs.
3. **The costly part of a large delta is `Live::view()`, not memory and not search.**
   `view()` re-adds every live delta entry to an `IndexBuilder` and freezes it (including a
   parallel name sort) after every 200 ms batch that has changes. Cost is O(delta) per publish.
4. **Recommendation:** two steps.
   - **Now (S to M):** scale the limit with the index, stop dead delta entries piling up, move
     the view build to the low-priority pool and throttle it, and pass the limit to the delta
     search.
   - **Then (L):** replace the disk rescan for "too many live changes" / "too many replaced
     entries" with an in-memory compaction (option c). Measured proxy: about 15 to 20 times less
     CPU than rebuilding with a builder, hundreds of times less than a rescan, and no disk.
   - Option (d) is only worth it after (c).

## 1. Where 50,000 comes from, and what grows with the delta

### History

| Commit | What it did |
|---|---|
| `f92859a` live index updates | `MAX_DELTA = 50_000`, `MAX_REMOVED_DIVISOR = 10`, `MAX_MOVE = 100_000`. No documented rationale. |
| `28949fb` rescan storm | Added `RESCAN_DUTY = 9` (gap 1 to 30 min) and `HARD_DELTA = 8 * MAX_DELTA` (400 k, rescan at once). Comment: "bounds its memory and per-batch view cost". Added `delta_children`. |
| `43a1a05` folder moved in | `MAX_DELTA` also became the walk budget (`MAX_DELTA - delta_alive`) and the change-log size `MAX_TOUCHED`. Raising `MAX_DELTA` changes these too. |

`docs/decisions/0005-live-index.md` (rejected in review 1, never revised): "Re-freeze. When the
delta exceeds max(64 K entries, 2 % of the base), or every 10 minutes while non-empty, a
low-priority background thread builds a new base from base − removed + added … Memory peaks at
about two bases." So an in-memory compaction was the original design.

### How the pieces behave (code in `live.rs`, `live_windows.rs`, `lib.rs`)

| Item | Cost in the delta size D |
|---|---|
| `Live::apply` | O(batch). Hash lookups only. Measured below. In the real service the per-record `fetch` (an NTFS record read) dominates, and this example does not measure it. |
| `Live::view()` | O(D + base/64 + overrides). Re-adds all live entries and re-sorts them (`IndexBuilder::freeze`: parallel name fold plus `par_sort_unstable_by`). Also copies the `allowed` bitset (base/64 words) and clones the `overrides` map. Runs after every batch with changes, on the global rayon pool (all cores), not the low-priority rescan pool. |
| Memory | O(D): `live.delta` + `live.delta_children` + `overlay` entries + the published view. Dead entries stay until the next rescan. |
| Search | O(D) per query. `snapshot_search` runs a second full search over the delta index with `limit: None` and re-sorts all its hits (O(m log m), m ≤ D) before merging. Early exit of the base search (first page) does not apply to the delta. |
| Count | O(D), one more scan of the delta. |

Two defects found in the same code:

- **Dead delta entries accumulate.** `apply_one` removes and re-adds a delta-resident file
  whenever its metadata changes (only folders are updated in place). Measured: one file written
  in 2000 batches leaves `delta_len = 2000` with 1 alive entry. `check_limits` only counts
  `delta_alive`; `HARD_DELTA` counts dead ones but only matters when a rescan is already pending.
  A chatty file (log, WAL, browser cache) leaks about 200 B and one skipped loop iteration per
  batch until some other trigger rescans. Memory has no ceiling in between.
- **Real journals also report folder changes.** Every created file changes its parent folder's
  dates, which becomes an `overrides` entry (`HashMap`, cloned in full on every `view()`). The
  benchmark's fake source does not produce these, so its `view()` numbers are a lower bound.

## 2. Measurements

### What was measured, and how

`crates/ferrowd/examples/delta_limit_bench.rs`, release build, one process, a few seconds of
work, 1,000,000-entry synthetic base from `ferrow_fixture::generate_with` (default columns,
records numbered `id + 16`, one volume), the real `Live`, `Live::view`, `ferrowd::handle` (the
real search path including the delta merge) and `memory::process()`.

- **Changes:** 500 per batch. Mix: 70 % new file (in one of 2000 build-output-like folders),
  25 % modified existing file (base entry hidden, delta entry added), 5 % deleted file. Entries
  are added until each checkpoint (50 k, 200 k and 1 M live delta entries).
- **apply:** mean and worst of the last 20 batches before the checkpoint (`Live::apply` only, with a
  hash-map source instead of NTFS reads).
- **view:** `Live::view()` on the default rayon pool (32 threads), on a 1-thread pool
  (a CPU-time proxy), and on an 8-thread pool (the service's rescan pool size here).
- **publish:** `apply(500)` plus `view()`, median of 5: what the maintainer does after each batch.
- **search:** `ferrowd::handle` with `Request::Query`, name sort, limit 100, median of 7 after
  one warm-up. The reference is the same base without a live view.
- **memory:** capacity-counted bytes (`Live::memory_parts`, `LiveView::memory_parts`) per entry,
  and the growth of the process's private bytes.

Caveats: wall times on a 32-thread PC that other agents were also using (build jobs were running
during the runs), so treat numbers as ±20 %. A 1 M base understates the base search time (a
27 M base scans about 27 times more), so the added delta cost looks larger relatively than it
will on the big PCs, in absolute terms it is the same.

### Results (base 1,000,000 entries, steady index 58.6 B/entry)

| Live delta entries | 50 k | 200 k | 1 M |
|---|---:|---:|---:|
| `apply`, ms per 500 changes (mean, worst) | 0.24 (0.42) | 0.31 (0.38) | 0.32 (0.49) |
| `view()` wall, default pool | 21 to 26 ms | 77 ms | 217 to 263 ms |
| `view()` wall, 8-thread pool | 28 ms | 68 ms | 325 ms |
| `view()` one thread (CPU proxy) | 40 ms | 117 ms | 436 ms |
| `view()` per view entry, one thread | 0.64 µs | 0.50 µs | 0.42 µs |
| publish (apply 500 + view), wall | 22 ms | 70 ms | 235 ms |
| view build peak growth (process) | 9 MB | 24 MB | 81 MB |
| search "report", first 100 (reference 0.37 ms) | 1.2 ms | 3.8 ms | 7.1 ms |
| search delta-only, first 100 (reference 6 to 10 ms) | 6.8 to 7.8 ms | 11.7 to 12.5 ms | 15 to 39 ms |
| search no match (reference 7.5 ms) | 7.1 ms | 10.7 ms | 14.8 ms |
| count (reference 4.0 ms) | 5.5 ms | 6.5 ms | 8.0 ms |
| live layer, capacity bytes per delta entry | 202 | 203 | 160 |
| published view, capacity bytes per view entry | 104 | 89 | 71 |
| **live + view, bytes per delta entry (capacity)** | **333** | **306** | **234** |
| process private growth, bytes per delta entry | 470 | 420 | 330 |
| `removed` bits + `overlay` + overrides total | 2.7 MB | 10 MB | 77 MB |

Reading it:

- **apply is not a problem** (sub-millisecond per 500, flat in D; in the service the NTFS record
  fetch is the real per-batch cost). Complexity O(batch).
- **view() is linear in D** at 0.4 to 0.6 µs per entry of one-thread CPU, and it runs after every
  batch. In a sustained change storm (a build, an install) the maintainer repeats it every
  200 ms: at 50 k that is about 35 to 40 ms of CPU per 200 ms (about 20 % of a core, as a burst
  over all cores of the global pool); at 200 k about 60 %; at about 400 k (`HARD_DELTA`) a full
  core or more. This is the real reason a bound is needed, and the reason 1 M deltas are
  impractical without changing `view()`.
- **Memory** is 230 to 330 B per delta entry by capacity, 330 to 470 B by process counters. That
  is 4 to 6 times a frozen entry (58.6 B). It is cheap in absolute terms: 50 k is 17 to 24 MB,
  270 k (1 % of 27 M) is about 90 to 125 MB, 1 M is 235 to 330 MB.
- **Search merge** adds roughly 12 to 16 ns per delta entry per query, plus an O(m log m) sort
  for broad queries (about 25 ms extra at m = 1 M). Selective first-page queries are the most
  affected in relative terms (0.37 ms to 1.2 ms at 50 k) because the base search stops after 100
  hits while the delta search scans everything. On the big PCs it is a few milliseconds at
  100 to 300 k. Not a reason to keep 50 k.
- **The removed-entry limit** (`base/10 + 1000` = 101 k here, 2.7 M at 27 M) was never reached
  with 25 % modified files at 1 M deltas on a 1 M base: the first ask was the delta limit.
  The removed bitset is cheap (1 bit per base entry).

### Cost of a rebuild from memory (proxies for option c)

Same process, base 1 M entries, old index still held:

| Method | CPU (one thread) | Wall (32 threads) | Peak growth |
|---|---:|---:|---:|
| Re-add all entries to a new `IndexBuilder` and `freeze()` (builder path) | 420 to 535 ms | 120 to 135 ms | 87 B/entry |
| Merge path: rank table, remap of the sorted order and parent column, copy of names (folded arena similar, not included), 10 % hidden | about 24 ms (about 50 ms with the folded arena) | n/a (single pass) | 49 MB for 0.9 M kept (about 55 B/entry) |

Scaled linearly to 27 M entries (n log n makes the builder path slightly worse):

- builder path: about 11 to 14 s of CPU, about 3.5 to 4 s wall on 32 threads, peak +2.3 GB
  (old index 1.6 GB stays);
- merge path: about 1.3 to 2 s of CPU, peak about +1.6 GB;
- for comparison, a full rescan: about 74 s (MFT) on one of the author's PCs, 198 s on the other, peak about
  161 B/entry (about 4.3 GB at 27 M) plus the old index, plus a full index save and many GB of
  disk reads (a walk is slower and seek-bound).

## 3. Options

Size: S = under a day, M = one PR with tests, L = several PRs or a new primitive.

### (a) Scale the limit with the index: S, low risk, partial

`MAX_DELTA` = `clamp(base_len / 100, 50_000, 250_000)`, `HARD_DELTA` = 8 times that (or 1.6 times
`MAX_DELTA` plus a dead-entry count), `MAX_TOUCHED` and the walk budget follow.

- 1 M: 50 k. 18.6 M: 186 k. 27 M: 250 k. That is about 5 times fewer rescans on the big PCs.
- Memory at the cap: 250 k × 330 B is about 80 to 120 MB, 5 to 7 % of a 27 M index.
- Without the fixes under (b) the per-publish `view()` at 250 k is about 130 ms CPU every 200 ms
  in a storm. So (a) alone swaps "a rescan every 11 to 30 min" for a steady 60 % core load during
  storms. Do not ship (a) alone.

### (b) Make the delta cheaper: M for the cheap parts, L for the structural part

Cheap parts (M, one PR, together with (a)):

1. **In-place update of a delta-resident entry** in `apply_one` when parent, name and flags are
   unchanged (a rewritten file: the same branch that already handles folders). Removes the dead
   entry leak; no new entry and no new `delta_children` item per write.
2. **Run `view()` on the low-priority background pool** (`pool.install(|| live.view())`). Measured:
   the 8-thread pool is as fast as the whole machine (28 / 68 / 325 ms against 26 / 78 / 263 ms),
   so searches keep their cores.
3. **Throttle publication by duty**: publish the next view no earlier than 10 × the last view's
   build time after the last publish (at 200 k: about 0.7 s; the 1 s freshness target in M2
   holds up to about 300 k; coalesce changes in between, which are already merged into `Live`).
4. **Give the delta search the limit** when the sort is `Name` (the first `limit` delta hits in
   name order are enough), and skip the redundant re-sort when the key is `Name`. Removes the
   O(m log m) part and lets first-page queries stop early.
5. **Slim `DeltaEntry`** (it holds a `Vec<u8>` name, a full `EntryMetadata`, `usize` volume and
   `u64` record): names into a shared arena and a packed metadata cut 160 to 200 B to about 60 to
   80 B. Optional (S to M).

Structural part (L): publish delta as several frozen segments (an LSM of immutable chunks, merge
equal sizes) so `view()` costs O(batch log D) instead of O(D). It touches `snapshot_search`,
`snapshot_count`, path building and `Snapshot::index_of`, needs per-segment shadow roots, and
makes each search merge k sorted streams. Not recommended: (c) removes the need.

### (c) In-memory compaction instead of a disk rescan: L, medium risk, the real fix

Build a new frozen base from base − removed + delta (overrides folded in) and a new `Live` from
the new ids, then swap both. No MFT read, no directory walk, no disk I/O (except the normal index
save later).

- **CPU:** builder path about 11 to 14 s CPU at 27 M (about 7 to 8 s CPU at 18.6 M); merge path
  about 1.3 to 2 s. A rescan today is 74 s to 198 s of heavy CPU and disk.
  So 5 to 15 times less with the builder path and 40 to 100 times less with a merge path, and no
  disk.
- **Peak memory:** builder path +87 B/entry (about 2.3 GB at 27 M), merge path about +55 to 60
  B/entry plus about 8 to 16 B/entry for the new `Live` tables. A full rescan peaks at 161 B/entry
  (PR #90) and holds the old index too. Both compaction paths are lower; the spike is short (a few
  seconds), not minutes. The old index and `Live` are freed once the last in-flight search drops
  the old snapshot.
- **Never freeze:** the swap is one `SnapshotStore::swap`; searches use the old snapshot
  meanwhile. Run it on the maintainer thread at background priority (the rescan pool) so there
  is a single writer: no replay log is needed, journal changes queue in the maintainer's channel
  for those seconds (lag of changes only, never of searches). Defer while any folder walk is
  outstanding (`walks` non-empty), and apply changes first.
- **Correctness points:** ids change, which is safe because `Page` re-runs the query statelessly.
  `Live::new` already builds every table from `(record, id)` pairs, so the new `Live` is built
  from `base_record` (alive ids) plus the alive delta entries' `(volume, record)` with their new
  ids; `excluded` and `roots` records carry over. Entries with `record == u64::MAX` (no record)
  are kept in the index but get no map entry.
- **Reasons covered:** `too many live changes`, `too many replaced entries`, and also the delta
  growth under `HARD_DELTA`. Not covered (they still need a real rescan): journal reset or
  overrun, moved-in folder failures, administrator request, startup.
- **Two stages:**
  1. Builder path (M-L): reuses `IndexBuilder`; one new function `Live::compact`.
  2. Merge primitive in `ferrow-index` (L): `Index::merge(base, alive, delta_entries,
     overrides)`; sorted order by binary search of the delta entries into the old order and a
     rank remap, folded arena copied. Needs the metadata columns' scatter API. Lazy sort orders
     (size, modified) are rebuilt on first use as after a rescan today.
- With (c) in place `MAX_DELTA` can be small again (compaction every 50 to 250 k changes costs
  seconds), and `view()` cost stays bounded by the limit.

### (d) Rescan only the volumes whose changes overflowed: L, higher risk, only after (c)

All volumes share one `Index`, one `Live` and one scan call (`scan_with_maps(&options)` over all
roots). A per-volume rescan needs the splice primitive of (c) (old base without volume v plus the
new scan of v), a scan with roots filtered to v, a per-volume reset of `Live` tables, and a
journal cursor per volume. It saves the other volumes' share. On the slower PC today two of three
drives are walked, so most of the 3 min 18 s would be saved (after the other job's fix the drives
are MFT-read and the saving is about the other drives' share). The overflow is counted for the
whole `Live`, and the busy volume is often the biggest, so the saving is uneven. Once (c) exists,
only rare causes (journal reset, moved-in failures) rescan at all, and (d) is mostly moot.

### (e) Other findings

- The dead-entry leak above (fix: in-place update, and count dead entries in the bound).
- `RescanPolicy` gap is `9 × scan time`, max 30 min. On the slower PC the first rescan is
  allowed after 30 min and, if a storm outlasts it, the next after another 30 min. That is the
  "many times a day" seen there. With (c) a compaction gap of 10 s to a few minutes is fine.
- A save of the whole index follows every new base (`persistence.rs`, at most every 30 min).
  Compaction should not save more often than that, and a compaction without any journal reset
  could skip the save (the saved file is only a quick start; every service start rescans).
- `live.base_children` (8 B per base entry, 216 MB at 27 M) is built on the first folder rename
  or delete and kept until the next rescan. Compaction would also free it.
- `view()` clones `overrides` and builds `allowed` (base/64 words) every publish; a frozen
  `Arc` shared until changed would remove it.

## 4. Recommendation

Do (b)-cheap + (a) now as one small PR, then (c) as the follow-up, in this order:

1. **PR 1 (M, low risk, next beta):** in-place delta-resident updates; `max_delta(base_len)` and
   a dead-entry bound; `view()` on the background pool; publish duty throttle (10 %); delta search
   limit; optional `DeltaEntry` slimming. Result on the 27 M PC: about 5 times fewer rescans, no
   per-batch CPU spike, memory ceiling of about 100 MB for the delta.
2. **PR 2 (L, the actual fix):** `Live::compact` builder path, the maintainer runs it for the two
   overflow reasons at background priority with a short gap; rescan only for journal and moved-in
   failures. Then consider lowering the scaled limit back (compaction at 1 % of the index).
3. **PR 3 (optional, L):** merge primitive in `ferrow-index` to take compaction from seconds of
   CPU to about a second, and lower the spike.
4. (d) only if rescans for the remaining reasons turn out to be frequent.

## 5. Files and functions an implementation would change

- `crates/ferrowd/src/live.rs`
  - `MAX_DELTA`, `HARD_DELTA`, `MAX_TOUCHED`, `walk_limits_text`, `check_limits`
    (line 778), `walk_moved_in` budget (922), tests using `MAX_DELTA` (1796 to 1850):
    functions of the base length.
  - `apply_one` (1177): update a delta entry in place; `Live::add`, `delta_len`.
  - `DeltaEntry` (178) slimming; `view()` (1290): reuse of `overrides`/`allowed`.
  - New `Live::compact` (builds new `Index` plus `VolumeMap`s from the alive base ids, the alive
    delta entries and overrides) and the merge variant.
  - `reason_kind` (81): add the compaction counter name.
- `crates/ferrowd/src/live_windows.rs`
  - `maintain` (377 to 607): publish gate and background-pool `view()` around line 601; compaction
    branch where `rescan.due(...)` (491) breaks out; `STATE.count_scan`, `log_memory`
    ("before/after compaction"), `RescanPolicy` (a separate short gap for compaction).
- `crates/ferrowd/src/lib.rs`: `snapshot_search` (342 to 439): pass `limit` to the delta search
  for `Name` sort, skip the re-sort; `snapshot_count` (441).
- `crates/ferrowd/src/health.rs` and `crates/ferrow-ipc/src/health.rs`: scan reasons list
  ("compaction"); `docs/service.md`, `CHANGELOG.md`, ADR 0005 (the re-freeze section becomes
  true).
- `crates/ferrow-index/src/lib.rs`: only for the merge primitive (`Index::merge`, access to
  `sorted`, columns and the metadata scatter code).

## 6. Tests and CI bench rows

Tests (all platform-independent, in `live.rs` using the existing `fixture`/`Fake` helpers):

- `max_delta(base_len)` table: 1 M gives 50 k, 18.6 M gives 186 k, 27 M gives the cap; walk
  budget and log size follow.
- Repeated write to one delta-resident file keeps `delta_len` constant (fails on today's code:
  2000 writes give `delta_len = 2000`, shown by the benchmark's `DELTA-REPEAT` line).
- Dead-entry bound: many dead entries with few alive ones still request compaction.
- Compaction oracle: random create, rename, folder move, delete, hard link and metadata
  change sequences; after `compact()` the set of `(path, metadata)` of the new index equals that
  of the old base plus view (the existing view tests already compare by path); then further
  changes applied to the new `Live` give the same result as applying them to the old one.
- Compaction keeps `excluded`, `roots` and hard-link overlays; refuses or defers with an
  outstanding walk.
- Never-freeze test with a mocked slow compaction stage (sleep as a fake timer): a search through
  `handle` returns while the compaction is running, and the old snapshot is intact until the swap.
- Publish gate with a fake clock: view build time `t` gives the next publish no earlier than
  `10 t` after, and the last batch is always published eventually.
- Delta search with the limit returns the same first `limit` hits as the unlimited search plus
  merge for `Name` sort (property test against the old path).

CI bench rows (promote `delta_limit_bench` into the Windows and Linux bench jobs next to
`memory_breakdown` in `.github/workflows/ci.yml` lines 424 to 554, informational first, with a
20 k smoke run like the memory example):

- `apply_ms_per_500` at 50 k / 200 k / 1 M;
- `view_ms` (default pool, 1 thread) and `view_us_per_entry`;
- `publish_ms` (apply plus view), and with the throttle the published interval;
- search ms (selective first page, delta-only, no match) with delta against none, and count;
- bytes per delta entry (capacity and process) with a gate after the slimming (for example
  at most 150 B);
- `compact_ms` (CPU and wall) and `compact_peak_B_per_entry` at 1 M and 5 M, with a gate (for
  example at most 90 B/entry for the builder path), plus the rescan comparison from
  `memory_breakdown`;
- dead-entry row: `delta_len` after 2000 rewrites of one file must be 1.

## 7. Not measured

- Real NTFS `fetch` cost per record and folder date overrides (the fake source has none).
- The service's real change rate on the two PCs: the event logs would show how many of the "N
  live changes" are distinct files (git, node_modules) and how many rewrites of the same file.
- Merge-path compaction as real code: only its single-thread rank/remap/copy part was timed.
- 18 to 27 M entry bases (1 M base used, within the machine rule); all larger figures are linear
  extrapolations and should be confirmed by the CI bench at 5 M.

## 8. Part 1 as built (FR-42), and what it measured

Same example (`delta_limit_bench`), 1,000,000-entry synthetic base, release build, one
PC (32 threads) that was otherwise quiet. "Before" is `5685bef`, "after" is part 1. Searches are
the median of 7 runs, name sort, first 100; two reruns of the search rows agreed within
about 0.3 ms.

| Live delta entries | 50 k | 200 k | 1 M |
|---|---:|---:|---:|
| delta entries after one file is written 2000 times | 2000 before, 1 after | | |
| `view()` on the low-priority pool, ms (before / after) | 24.7 / 22.9 | 76.2 / 68.4 | 241.7 / 248.8 |
| `apply(500)` plus `view()`, ms (before / after) | 19.6 / 21.4 | 59.8 / 69.5 | 210 / 221 |
| views built per minute of storm (before / after) | 300 / 150 | 300 / 75 | 300 / 24 |
| core share of a storm, one-thread CPU proxy (before / after) | 0.19 / 0.09 | 0.61 / 0.15 | 2.50 / 0.20 |
| longest gap between two views (after), ms | 423 | 868 | 2849 |
| search "build-obj" (delta hits), ms (before / after) | 3.1 / 2.1 | 5.9 to 6.5 / 2.3 | 18.8 / 2.3 |
| search "report" (no delta hits), ms (before / after) | 0.75 to 0.86 / 0.75 to 0.80 | 1.3 to 1.6 / 1.3 to 1.8 | 2.4 / 1.1 |
| bytes per delta entry, live layer plus view (before / after) | 333 / 333 | 306 / 306 | 234 / 234 |

- The view itself is not cheaper: it is built less often, on the background pool, and the
  delta search stops after one page. The per-entry memory is unchanged (the optional
  `DeltaEntry` slimming of section 3 (b) 5 was left out).
- The limit is the same 50,000 up to a 5 M entry index and grows from there:
  18.6 M gives 186,000 and 27 M gives 250,000. At 250,000 entries the delta costs about 80 MB
  (333 B per entry), which is 5 % of a 27 M index; a rescan peaks at about 4.3 GB.
- Not measured: real NTFS record reads, the 18 to 27 M sizes (the figures above are linear
  from 1 M), and the effect of folder-date overrides on the view.
