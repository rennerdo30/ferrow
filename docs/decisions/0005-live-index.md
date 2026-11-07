# ADR 0005: live index updates on Windows (M2d)

Status: proposed (2026-11-07). Prerequisite for per-user visibility
(ADR 0002, review 1 item 1: permissions must stay fresh). Needs an
independent review before code merges.

## Goal

The service keeps its index current while it runs: a created, renamed,
deleted or changed file shows up in results within 1 s (M2 acceptance 4),
size and dates stay right, and permission changes are seen as they happen.
After a restart only the changes since the last save are replayed. Idle CPU
stays at 0 % (no polling).

## Overview

```
volume scan (handle-relative, FRN-aware) --> frozen base Index
NTFS change journal (FSCTL_READ_USN_JOURNAL) --> change batches
change batches --> Delta (tombstones + added + changed) --> new Snapshot
Delta too large --> background re-freeze --> new base, empty Delta
```

A query takes one immutable `Snapshot { base, delta, version }` and never
waits for an update; an update builds the next snapshot and swaps it in one
atomic step (the same publish point ADR 0002 review item 2 requires for
visibility data).

## Index core: a delta layer over the frozen base

The M1 base layout (ADR 0001) stays: frozen columns, arenas, name order,
metadata columns. A `Delta` adds, per snapshot:

- `removed`: a bitset over base ids (deleted, moved or renamed base
  entries). Empty → no per-candidate cost; otherwise one bit test per
  visited base candidate, the same skip as `prefilter_bits`.
- `added`: entries with ids `base.len() ..`, stored like a small index
  (parent, name, flags, metadata, folded name), plus their own name order.
  A parent id may point into the base or the delta.
- `changed`: metadata overrides for base entries whose size, times or
  attributes changed (a bitset over base ids plus a sorted `(id, metadata)`
  array). Reads check the bit first; no hash lookup on the hot path.
- `moved_to`: for a renamed or moved base **directory**, its new delta id.
  Children keep their base parent id; parent resolution maps a removed
  directory through `moved_to`, so a folder rename touches one entry, not
  its subtree. Path building and `path:`/`parent:` matching resolve
  through it.

A rename or move is "remove + add" for the entry itself (its name order
changes); the delta entry keeps the file reference number (FRN).

**Queries.** The base scan skips `removed` ids and reads `changed`
metadata; the delta's entries are matched separately (they are few) in the
same order key; first pages merge the two sorted streams and truncate to the
limit; counts add. Metadata sort orders merge the same way (the delta keeps
per-key sorted lists, built lazily like the base's). Cancellation and
bounded work rules are unchanged.

**Re-freeze.** When the delta exceeds max(64 K entries, 2 % of the base),
or every 10 minutes while non-empty, a low-priority background thread
builds a new base from `base − removed + added` with `changed` applied (in
parent-before-child order, so ids stay dense), then swaps it in. Changes
that arrive during the re-freeze are recorded against both the old delta
and a pending log and replayed onto the new base's empty delta before the
swap. Memory peaks at about two bases during a re-freeze; the bench reports
that peak.

Budget: the delta costs about the base per-entry size for its entries; the
bitsets cost 0.25 bytes per base entry when in use.

## File reference numbers

The journal names files by FRN (NTFS: 48-bit MFT record number + 16-bit
sequence). Mapping FRN → entry id:

- A per-volume table indexed by **MFT record number** holding the entry id
  (`u32`, `u32::MAX` = none), sized to the volume's MFT record count
  (`FSCTL_GET_NTFS_VOLUME_DATA`): about 4–5 bytes per entry on typical
  volumes (the MFT has some free records).
- A `u16` sequence number per entry; a record whose sequence differs is a
  different (reused) file.
- ReFS (128-bit ids) and any volume where the table would be sparse
  (> 4 × entries) use a sorted `(id128, entry)` array instead.

Total about 6–7 bytes per entry, only in the service and only on journal
volumes.

## Full scan

The walker enumerates with opened directory handles
(`GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)`), which returns
each child's FRN, size, times and attributes in one call per buffer, and
descends parent-relative by FRN (`OpenFileById`) rather than by path
string. That gives the FRNs the journal needs, keeps metadata free, and
binds enumeration to the object whose security descriptor M2c will read
(review 1 item 8). It records the volume's journal id and next USN
**before** walking, so changes during the walk are replayed after it.

Faster raw `$MFT` reading (as Everything does) is a later optimisation; the
interface allows swapping it in.

## Journal reader

One thread per journal volume:

- `FSCTL_QUERY_USN_JOURNAL` for the journal id and range; `FSCTL_CREATE_USN_JOURNAL`
  is not called (the service never changes volume configuration).
- `FSCTL_READ_USN_JOURNAL` with `BytesToWaitFor = 1` on an overlapped
  handle: the thread sleeps in the kernel until a record arrives; shutdown
  cancels it with `CancelIoEx`. No polling, 0 % idle CPU.
- Reasons used: create, delete, rename old/new name, data extend /
  truncate / overwrite, basic-info change, hard-link change, security
  change, reparse-point change, and close. Records are coalesced per FRN
  and applied in batches at most every 200 ms (acceptance is 1 s).
- Changed metadata is read with `OpenFileById` + `GetFileInformationByHandleEx`
  on the batch thread; a file that vanished in between is a delete.
- Excludes and roots are applied to journal changes exactly as in the walk
  (by identity), so an excluded folder's changes never enter the delta.

## Persistence and restart

The index file gains a section per volume: journal id, last applied USN,
and the FRN table. It is saved atomically on shutdown and at most every
5 minutes while there are unsaved changes (low-priority, off the query
path). On start:

- same journal id and the saved USN still inside the journal's range →
  replay from the saved USN, then go live;
- otherwise (journal deleted, recreated, wrapped past the cursor, volume
  changed) → full rescan of that volume, reported as
  `journal reset: rescanning E:`. Until it finishes, the old data for that
  volume stays searchable for administrators and is marked stale; for
  filtered callers (ADR 0002) a stale volume is unavailable.

## Volumes and modes

- Local fixed NTFS/ReFS volumes with a journal: scan + live updates.
- Volumes without a journal (FAT, exFAT, journal disabled), network shares,
  and `subst` aliases: walk only; `subst` aliases are skipped (counted once
  through their real drive); the others are re-walked on the configured
  rebuild schedule and reported as `walk: no live updates`.
- Folder mount points and junctions: a mounted volume is its own journal;
  junctions are not followed in the service (review 1 item 8).
- Linux and macOS keep the walk until M3 (fanotify, FSEvents).

`ferrow status` reports per volume, e.g.
`C: ntfs, 1 290 846 entries, live (usn 0x1a2b…)` or
`E: ntfs, rescanning (journal reset)`.

## Never freeze

The journal reader and batch thread never take a lock a query holds; a
snapshot swap is a pointer swap. A slow `OpenFileById` delays only its batch
(bounded per batch, cancellable). A burst of changes (a build writing
100 000 files) grows the delta and triggers a background re-freeze; queries
keep answering from the current snapshot throughout.

## Performance acceptance (CI bench)

- Base-only queries: unchanged (the paired gate stays strict).
- New arm: the 5 M fixture with a 1 % delta (removed, added and changed
  entries) — first page and count reported, limit base + 25 %.
- Apply cost: a 10 000-change batch applies in ≤ 50 ms; a re-freeze of
  5 M entries reports time and peak memory.

## Tests

- Unit (all platforms): delta semantics against an oracle that rebuilds a
  fresh index after each change sequence (insert, delete, rename,
  directory move with children, metadata change, re-freeze mid-stream);
  query merge order for every sort; FRN table with reused sequences.
- Mocked journal stream (all platforms): coalescing, ordering, wrap and
  reset handling, cursor persistence.
- Windows CI (`windows-latest`, elevated, disposable): the installed
  service sees create, rename and delete within 1 s; the cursor survives a
  service restart; deleting the journal (`fsutil usn deletejournal` on a
  VHD created for the test) triggers a rescan. Never on the owner's PC.

## Not decided here

Raw `$MFT` reading, ReFS-specific optimisations, Linux/macOS live backends
(M3), and per-user visibility itself (ADR 0002, revised on top of this).
