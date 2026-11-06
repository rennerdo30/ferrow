# Milestone 1: the base (index core, walk backend, query, CLI, benchmark)

Status: **specified 2026-11-06.** It builds on [`../SPEC.md`](../SPEC.md). Where the spec is still a draft, this file fixes what M1 needs; the §3 targets are working targets until the owner confirms them.

## Goal

A working, measured core: scan a directory tree into a compact in-memory index, save and load it, and answer name queries in parallel. A CLI drives it, and a CI benchmark on a synthetic 5 M-entry fixture produces the first real numbers against §3.

**Not in M1:** live updates, the daemon and IPC, the Windows MFT/USN, fanotify and FSEvents backends, filters such as `size:`/`dm:`, incremental narrowing, and the GUI. Those come in later milestones.

## Crates

All crates live under `crates/`, with workspace-inherited package metadata and `publish = false`.

### `ferrow-index`: the store
- **Layout, struct of arrays:**
  - per entry: `parent: u32`, `name_off: u32`, `name_len: u16`, `flags: u16` (directory, hidden, symlink, name-not-UTF-8);
  - one name arena (`Vec<u8>`);
  - one case-folded name arena for case-insensitive search;
  - one name-sorted permutation (`Vec<u32>`).
- **Root entries** have `parent = u32::MAX`, and their name is the root path.
- **Names are lossless:**
  - Unix: the raw `OsStr` bytes.
  - Windows: UTF-8 when the name is valid UTF-16, otherwise WTF-8 (a small hand-written encoder, round-trip tested).
- **Paths are never stored.** `path(id)` rebuilds a path from the parent chain into a caller-supplied buffer.
- **Builder:** an `IndexBuilder` accepts entries in any order from many threads (sharded) and then freezes into an immutable `Index`. The freeze computes the folded arena and the sort order in parallel. Queries hold an `Arc<Index>`.
- **Persistence:** one versioned binary file.
  - It holds a magic number, a format version, and a header with counts and a checksum (xxhash or crc32).
  - Columns follow the header, little-endian.
  - Loading does one sequential read with no per-entry allocation.
  - A file with a wrong magic, version or checksum is rejected with a clear error, never a panic.
- **Memory:** `Index::heap_bytes()` reports the actual bytes. The target is ≤ ~60 bytes per entry, including both arenas and the sort order.

### `ferrow-query`: parser and matcher
- **Grammar:**
  - terms separated by spaces mean AND;
  - `!term` means NOT;
  - `"quoted phrase"` is a literal (spaces included);
  - matching is case-insensitive by default, and `case:term` makes a term case-sensitive.
- **Errors:** the parser returns a typed AST and precise errors, and it never panics on any input (fuzz-friendly).
- **Matcher:**
  - Each term is a substring match on the name, using `memchr::memmem` over the folded arena (the original arena for `case:`).
  - Evaluation runs in parallel chunks with rayon.
  - Results come back in name-sort order and stop early when a `limit` is given.
  - A query can be cancelled via a shared generation counter or flag, checked per chunk.
- **Reference check:** a naive, single-threaded reference matcher in the test code must give identical results.

### `ferrow-walk`: the portable backend
- **Scan:** a parallel directory walk (std `read_dir` on a rayon or work-stealing pool, or a permissive crate such as `jwalk`) that feeds the `IndexBuilder`.
- **Options:** include roots, exclude paths, and a follow-symlinks switch (off by default).
- **Errors:** permission and IO errors are counted and sampled into the report, never fatal.
- **Report:** every scan states its mode and statistics, for example `walk: parallel read_dir, 8 threads, 1234567 entries, 12 errors, 3.2 s`.

### `ferrow-fixture`: synthetic data
- **Generator:** a deterministic, seeded generator of an index with N entries.
  - realistic shape: depth up to about 12, a mixed fan-out, and names drawn from word lists plus extensions, numbers and dates;
  - a share of non-ASCII names;
  - no real-world file system needed.
- **Used by:** tests (≤ 100 k entries) and benchmarks (5 M entries).

### `ferrow`: the CLI
- `ferrow index <root>... [--exclude <path>]... [--out <file>]`: scans, prints the scan report, and saves the index.
- `ferrow search <query> [--index <file>] [--limit N] [--json | --null] [--count]`: loads the index and prints full paths.
- `ferrow stats [--index <file>]`: prints entries, heap bytes per entry, and load time.
- **Default index location:** the platform data directory (for example the `directories` crate, if its licence is permissive).

## Benchmark (CI only)

- **Library:** `divan` or `criterion` benches over a 5 M-entry fixture.
- **Measures:**
  - fixture freeze time;
  - bytes per entry;
  - load time of a saved index;
  - query latency for the first page (`limit 100`) and a full count, for `a`, `re`, `report 2024`, `!tmp log`, a no-hit query and a non-ASCII query.
- **CI job:** a separate `bench` job on `ubuntu-latest` runs them and writes a Markdown table to the job summary. It does not gate merges until the owner confirms the targets.
- **Never run the 5 M benches on the owner's PC.** Local runs use the small fixtures only.

## Quality gates
- CI stays green on Linux, Windows and macOS (fmt, clippy `-D warnings`, tests).
- Add `cargo-deny` to CI with a `deny.toml` that allows only MIT, Apache-2.0, BSD-2/3-Clause, Zlib, ISC and Unicode licences.
- No `unsafe` outside small, documented, tested spots, each with a `// SAFETY:` comment.
- Record the index layout and file format in an ADR: `docs/decisions/0001-index-layout.md`.

## Acceptance (finite list)
1. The five crates above exist, build and pass fmt, clippy and tests on all three CI platforms.
2. Index tests cover:
   - path rebuild;
   - sort order (including non-ASCII names);
   - the WTF-8 round trip on Windows;
   - persistence round trip;
   - rejection of corrupt, truncated and wrong-version files.
3. Query tests cover the full grammar. A property test shows the parallel matcher equals the reference matcher on a 100 k fixture, with and without `limit`, and checks cancellation.
4. A walk test on a temporary tree shows every entry is found with the right parent. A Unix test shows that an unreadable directory is counted, not fatal.
5. A CLI end-to-end test runs `index` on a temporary tree, then `search` (plain, JSON, NUL, `--count`).
6. The CI `bench` job publishes the measures listed above for the 5 M fixture.
7. `cargo-deny` runs in CI and passes.
8. The ADR is written, and the README gains a short "Try it" section with CLI usage.
