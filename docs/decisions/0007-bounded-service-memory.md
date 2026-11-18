# 0007: Bounded service memory (mapped and compressed index)

Status: accepted (owner, 2026-11-18). Tracked as FR-46.

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
8. **Rebuilds are bounded.**
   - Rescans and compactions write the new index to a file section by section
     (names and columns spill to disk) instead of holding a second index in
     the heap.
   - The heap peak of a rebuild is a fixed working set plus the name-order
     permutation (4 B/entry).
9. **Metadata sort orders go to the file.**
   - A sort order prepared on request (size, dates) is written beside the
     index and mapped, not kept in the heap (4 B/entry each).

## Budget (27.2 M entries, private bytes)

| State | Before | After (estimate) |
|---|---|---|
| Steady | 3.7 GB | 150–400 MB |
| Start | 6.2 GB | the steady figure plus replay (no scan) |
| Rescan or compaction | +2.3–4.4 GB | +110 MB name order + fixed buffers |

The file cache holding the mapped index is not counted against the budget,
but it is reported. Service health shows "mapped" next to "private" so that a
reader of the diagnostics sees both.

## Order of work

1. Map the frozen index (format v4, mapped columns, save-then-map after
   scans). This alone takes the steady state below 1 GB.
2. Journal replay at start.
3. Record maps and overlay in the file.
4. Names stored once.
5. Bounded rebuilds.
6. Sort orders in the file.
7. CI gate: the Windows memory-breakdown job asserts private bytes per
   entry, so a later change cannot quietly grow it again.

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
