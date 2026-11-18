# 0007: Bounded service memory (mapped and compressed index)

Status: accepted (owner, 2026-11-18). Tracked as FR-46. Decision 8 (bounded rebuilds): built in FR-46 phase 5, on top of the mapped index (decisions 1 to 4). Decision 10 (bounded scan): built in FR-46 phase 7. CI gate (order of work, step 7): built, so these budgets are checked on every pull request.

## Context

On the owner's PC (27.2 million entries on two NTFS volumes), `ferrowd`
0.1.0-beta.20 showed:

- **3.7 GB steady** in Task Manager.
- **6.2 GB while starting.** The saved index is loaded so search works at once,
  then both drives are read again in full, so two indexes are held for about
  a minute, with about 500 MB/s of disk reads.

The service's own breakdown at 27.2 M entries, in bytes per entry:

| Part | B/entry | At 27.2 M |
|---|---|---|
| Names, as written (`name_arena`) | 41.8 | 1.14 GB |
| Names, case-folded (`folded_arena`) | 41.8 | 1.14 GB |
| `parent`, `name_off`, `name_len`, `flags`, name order | 16 | 0.43 GB |
| Size column | 5 | 0.14 GB |
| Live layer: record maps and hard-link overlay | 22.5 | 0.61 GB |
| Unattributed (allocator, runtime) | about 5 | 0.14 GB |

The owner's requirement: **never more than 1 GB, target 512 MB**, as Task
Manager shows it for `ferrowd.exe`. That is 37 and 19 bytes per entry.
The names alone are larger than that, so no amount of trimming the heap
reaches it. Everything uses about 100 bytes per file, so copying it does not
reach the target either.

Performance stays the first rule (`CLAUDE.md`). An in-memory compressed
index alone (about 700–900 MB at 27 M) would make searches several times
slower. The owner chose to do both: map the index and compress it.

## Decision

1. **The frozen index is mapped from its file.**
   - Its columns are slices of a read-only memory map of the saved index,
     not heap vectors.
   - Windows keeps the pages that searches touch in its file cache. They are
     shared, clean and reclaimable, and they are not in the process's private
     working set or commit (the Task Manager "Memory" column).
   - Search code is unchanged: it still sees `&[u32]` and `&[u8]` slices, laid
     out as before, so speed is the same while the pages are cached. A search
     after Windows evicted them reads them back from disk.
   - Owned (heap) columns stay for builders, tests and platforms where mapping
     fails.
2. **The file format allows mapping (version 4).**
   - Every column is aligned for its type: `u32` sections on 4 bytes, `u16`
     on 2 (version 3 plus up to 2 zero bytes before the name order).
   - Integers are stored little-endian, as before. A big-endian target does
     not build (none is supported).
   - A file that cannot be mapped is read into the heap.
   - Loading checks the header, the lengths, the checksum and every offset, as
     today, but reads through the map instead of copying.
3. **A mapped file is never written in place.**
   - A save writes a temporary file and renames it over the old one, as today.
   - The map's file handle is opened with `FILE_SHARE_DELETE`. Windows then
     lets the rename replace the file while the old map still serves the old
     contents (tested on Windows 11 with .NET's mapped files). Old pages go
     when the last snapshot that holds the old map is dropped.
4. **After every scan or compaction the new index is saved and then mapped.**
   - The heap index the builder made is dropped, so the steady state holds no
     heap copy.
   - The save after a scan already exists (about 3.5 s at 27 M). It now happens
     right away, not on the 30-minute schedule, in the thread's background
     mode (lower CPU, disk and memory priority).
   - The file just written is mapped without being checked again (its header,
     length and checksum field are compared with what was written); a full
     check would read gigabytes once more after every save.
   - From then on the live maintainer is the only writer of the file.
5. **No full scan at start (ADR 0005's restart plan, now built).**
   - The saved file carries, per volume:
     - the journal id;
     - the next USN;
     - the record of every entry.
   - At start the service maps the file and replays the journal from the saved
     USN.
   - It reads the drives again only when the journal was reset, wrapped or
     replaced, or the volume changed.
6. **The live layer's tables go into the file too.**
   - The record of each entry (`base_record`) and each volume's record → entry
     table (`record_base`) are written with the index and mapped.
   - The hard-link overlay becomes a sorted table in the file.
   - The heap keeps only:
     - what changed since the last save: the delta, the hidden and removed bits
       (1 bit per entry each), and folder date overrides;
     - the published view.
7. **Names are stored once (compression).**
   - The file keeps the case-folded name, which searches read, plus a small
     table of the positions whose case differs. The original name is rebuilt
     from those two when shown.
   - This removes about 40 B/entry from the file and the cache, and from the
     build.
   - Case-sensitive matching checks the rebuilt name only for candidates that
     already matched case-insensitively.
8. **Rebuilds are bounded** (built, FR-46 phase 5).
   - Rescans and compactions write the new index straight into its file
     instead of holding a second index in the heap.
     `IndexBuilder::freeze_to_file` sizes the file exactly, maps it writable,
     places the columns, folds the names, sorts the name order and
     computes the checksum inside the map, flushes, renames it over
     `index.bin` and maps it read-only. The bytes are those of
     `Index::save`.
   - The builder's shards spill to anonymous temporary files beside the
     index once they hold a fixed budget (32 MiB; the heap holds about 1.6
     times that while entries are added).
   - The name-order permutation is sorted in the file's map, so it is not in
     the heap either: the heap growth of the freeze is a fixed working set
     (4.9 MB at 2 M entries, measured), not 4 B/entry.
   - Not covered here: the scan's record map and parsed volume (decision 10),
     and the new live tables of a compaction (decision 6).
9. **Metadata sort orders go to the file.** (Done: the ids, the merge scratch and the finished order of a mapped index are anonymous temporary files beside `index.bin`, deleted on close.)
   - A sort order prepared on request (size, dates) is written beside the
     index and mapped, not kept in the heap (4 B/entry each).
10. **The scan's MFT read is bounded** (built, FR-46 phase 7).
    - A full scan (first start, journal reset, **Rescan now**) held what it
      parsed in the heap until the index was built: the per-record table (48
      B per MFT record), the names (24 B each plus the text), the grouping by
      parent and the record map (16 B/entry): 102 to 106 B/entry for the parse
      and about 180 B/entry for the scan at its peak (measured, synthetic 1 M
      entries), about 4 to 5 GB at 27 M.
    - Every one of those tables is now an anonymous temporary file beside the
      index (the kind decision 8 uses: deleted on close, so a failed, killed or
      panicked scan leaves nothing), written through a writable map or
      appended in chunks as the read arrives and mapped read-only afterwards.
      The live layer's frozen tables (entry to record, record to entry,
      children) are built the same way and replaced by the saved copy when the
      tables can be saved (they stay as files for the session otherwise).
    - One temporary-file layer serves every table of the build: a
      `store::Scratch` (a zero-filled file written through a map, then frozen
      read-only) for tables of known size, `store::Appender` (chunked appends)
      for names, their text and the record map, one creator of the files
      (`store::create_temp_file`) and one disk-space step (`store::reserve`,
      before the first mapped write). The same code reads a table from the heap
      or from a file, so the result is the same bytes.
    - **The live layer's tables are refused too** (FR-47 follow-up). The
      service builds them with `Live::try_new_in`: the entry to record table,
      each volume's record to entry table and the children table use
      `Scratch::try_zeroed` above `HEAP_FALLBACK_ENTRIES` elements, so a missing
      file is an error, not about 0.5 GB of heap at 27 M entries. The service
      then serves the new index without live updates (the event log, the
      snapshot and health say why: each volume is listed as unavailable with the
      reason) and scans again after its own back-off; the saved tables of the
      old index are deleted; a small table still falls back, counted. With no
      room for the children table the tables are not saved, and a move of a
      frozen folder is not applied but asks for a rescan; that refusal is
      remembered for a minute (no repeated pass over the index) and a large
      table checks for room (about 8 B/entry) first. The layer a merge makes
      carries the same placement, so its children table is refused too; its
      own record tables stay in the heap (decision 6).
    - **No large silent heap fallback** (the lesson of FR-47). Before a
      volume's MFT is read the scan estimates what it will write (160 B per MFT
      record for the read's files, 130 for the shards and the new index;
      estimates) and checks the free space beside the index, after removing
      the temporary files a dead service left. A volume above `HEAP_FALLBACK_ENTRIES` (1 M records) that does not
      fit, whose file cannot be made or whose file fails during the read is
      not read into the heap instead: the read stops at once and the scan fails
      with `BUILD_FAILED`, so the service keeps its index and retries with its
      back-off. A failed appender of a large volume loses its column rather
      than reading it back (a read-back is capped at 128 MB in any case). A
      small volume falls back to the heap; every fallback bumps
      `scratch_in_heap()`, which the service reports.
    - **A failed scan does not stop live updates** (FR-47 review). The live
      layer of the session a rescan interrupts is kept through the scan only
      when its heap is at most 32 MiB (`KEPT_LIVE_HEAP`; about 291 B per change,
      measured), so the scan's bound above holds with at most that much more;
      a larger layer is dropped before the scan, as before. If the scan fails,
      the kept layer goes on over the index served, and the retry is its own
      timer (the back-off of the failures is unchanged). The journal readers
      are never stopped, so nothing is replayed from `live.bin`; a rescan that
      waits for its retry is not started early by an outgrown delta, and a
      layer that reaches its hard limit meanwhile is dropped.
    - Chosen over shrinking the tables only (the per-record table is 1.1 GB
      at 27 M even at 48 B per record, and the names are 0.9 GB) and over a
      streaming emission (entry ids and their order, hence the saved bytes,
      follow the directory order the emission walks; changing it would change
      results). Not chosen: a smaller `RecordInfo`; it would save under a
      third and keep the heap growing with the volume.
    - Costs: temporary disk space (about 120 to 160 B per MFT record per
      volume plus the shards: about 5 GB at 27 M, estimated) beside the old and new index
      file, and about 10 % more CPU time in the parse and emission steps (50
      to 60 ms per million entries, measured at 1 M). The grouping's check for
      names a journal refresh forgot is a bitmap over record numbers (3 MB at
      24 M records) in front of a binary search: 12.5 ms to 14.3 ms of a
      volume's join at 1 M entries with 20 000 forgotten records (measured),
      not a hash lookup per name.

## Budget (27.2 M entries, private bytes)

| State | Before | After (estimate) |
|---|---|---|
| Steady | 3.7 GB | 150–400 MB |
| Start | 6.2 GB | the steady figure plus replay (no scan) |
| Full scan | +4–5 GB (extrapolated from 182 B/entry) | about 60 MB (builder shards, working set) plus the read buffers (about 64 MB), measured at 1 M entries and extrapolated; the tables are in temporary files (decision 10); no room for them fails the scan instead of using memory |
| Compaction | +2.3–4.4 GB | about 60 MB (shards, working set) + the new live tables until decision 6 |

The file cache holding the mapped index is not counted against the budget,
but it is reported. Service health shows "mapped" next to "private" so that a
reader of the diagnostics sees both.

## Order of work

1. Map the frozen index (format v4, mapped columns, save-then-map after
   scans). This alone takes the steady state below 1 GB.
2. Journal replay at start.
3. Record maps and overlay in the file.
4. Names stored once.
5. Bounded rebuilds (done: FR-46 phase 5).
6. Sort orders in the file (done: temporary files beside the index).
7. CI gate (done): the Windows memory-breakdown job runs the example with 5
   million and with 1 million synthetic entries (an 8 MiB spill budget) and
   `scripts/memory-gate.py` on both outputs, and fails, naming the phase and
   the number, when:
   - a phase is over its absolute limit: the full scan's peak growth (per
     phase and in total, a fixed amount), the steady state after the heap
     builds are dropped (private bytes, as the service holds them: the
     service never calls `HeapCompact`; and the live layer's own heap in bytes
     per entry), a compaction (bytes per entry), a restart from the saved
     files (the live tables' save, the load's peak, the private bytes after)
     and the first sort orders on the mapped index;
   - the **slope** is over its limit: the growth per entry between the two
     sizes, for the scan's parse, emit and total, the compaction and the sort
     orders. A fixed working set and the runner's noise leave 3 to 5 B/entry of
     room under an absolute limit (a compaction 4 B/entry over its measured
     value, 19 MiB at 5 million entries, stayed under its limit), and the
     slope sees them: the fixed part cancels in the difference, so +2 B/entry
     in a phase moves its slope by 2 (tested by replaying a real run's output
     with +2 B/entry injected);
   - a peak is not credible: a failed sampler self-check, a peak of 0 or below
     its start, or a growth below the floor of a phase that must grow (a dead
     sampler reads as no growth).

   It prints the tables and a fit to the owner's 27.2 M entry PC (fixed part
   + slope × entries, against the 1 GB limit and the 512 MB target). Measured on the hosted
   runner (8 MiB budget, ten runs of each size): scan 14 to 16 MiB in total
   (the service's 32 MiB budget adds about 52), steady state 7 to 9 MiB, restart 6 to
   7 MiB, compaction 18.5 to 21 B/entry with a slope of about 9 B/entry (about
   60 MiB fixed). Fitted to 27.2 M entries: steady state 18 MiB, steady state
   plus a rescan 90 MiB, plus a compaction 316 MiB, a restart 12 MiB. All are
   under the 512 MB target; the compaction would be the largest, and it only
   warns until decision 6 moves its live tables into the file. (The first
   version scaled every figure by the entry count and read 527 to 590 MiB for
   the compaction: that counted its 60 MiB fixed part 5.4 times.) The limits are
   in that script, each with the measured numbers it was set from. Not covered:
   real raw-volume reads and the service's own process on a real PC (the
   example is synthetic, calls the building blocks directly, and measures a
   steady state without changes or a view); per-change heap growth is not
   gated; see `docs/service.md`.
8. Bounded scan: the MFT read and the live tables a scan builds in
   temporary files (done: FR-46 phase 7; not yet measured on a 27 M entry
   PC; a compaction's own record tables are still in the heap).

Each step is its own PR, with the `docs/SPEC.md` §3 numbers from the CI
bench, and is reviewed for search speed.

## Consequences

- **Disk:**
  - every compaction writes a new index file (about 3 GB today, about 1.9 GB
    after step 4);
  - every save of a new index happens at once.
- **Search speed:** the first search after Windows evicted the cache reads from
  disk.
- **Memory measurement:** the memory breakdown separates heap, mapped and
  unattributed bytes. The existing per-part numbers stay comparable.
- **Saved files:** a version-4 file is not readable by older versions (the
  service rebuilds it). Version 3 files are still loaded, once, into the heap,
  and resaved as version 4.
