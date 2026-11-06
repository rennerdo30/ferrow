# Ferrow: specification (DRAFT v0, 2026-11-05)

Status: **draft for owner review.** Nothing below is binding until the owner signs off; open decisions are listed in §9.

## 1. Owner rules (fixed)

1. **Multi-platform:** Windows, Linux and macOS are first-class targets from day one (owner, 2026-11-05).
2. **Rewritten in Rust,** not a port or wrapper of any existing tool (owner, 2026-11-05).
3. **Public and open source on GitHub** from the start (owner, 2026-11-05). Every file, commit and dependency must be fit for publication: no secrets, no machine paths in committed config, no third-party trademarks or assets, only permissively licensed dependencies.

## 2. Product in one sentence

Ferrow finds any file or folder on your machines by name **as fast as you type**: millions of entries, results on every keystroke, always up to date, on every desktop OS.

## 3. P0: speed and lightness

Speed is the reason this project exists; it is never traded for features. Proposed targets, to be confirmed by the owner and then measured in CI on a fixed fixture:

| Measure | Proposed target |
|---|---|
| Keystroke → first results painted, 5 M entries | ≤ 16 ms (one frame) |
| Full re-query and sort, 5 M entries | ≤ 50 ms, can be cancelled by the next keystroke |
| Cold start with a saved index | ≤ 300 ms to a searchable window |
| Live change visible in results | ≤ 1 s |
| Daemon when idle | 0 % CPU, no polling, no timers |
| Memory | ≤ ~60 bytes per entry for name + path index (excluding optional columns) |

Design consequences:

- **Columnar, arena-based index:**
  - All names live in one string arena.
  - Entries are `u32` records holding a parent id, a name offset and flags.
  - Optional columns (size, dates) are separate arrays, allocated only when enabled.
  - Paths are never stored in full; they are rebuilt from parent ids on demand.
- **Sort orders are precomputed per indexed column** and kept up to date incrementally, so changing the sort is free.
- **Parallel, SIMD-accelerated matching:**
  - The scan is split into chunks across cores (rayon) and uses `memchr`/`memmem`.
  - Literal queries are pre-filtered before regex.
  - A new keystroke cancels the query in flight.
  - Incremental narrowing: when the new query extends the old one, only the previous hits are searched.
- **Indexing never blocks search.** The indexer applies changes in batches; readers see a consistent snapshot.
- **Expensive work is opt-in and bounded:** content search, hashes and media properties. It runs on a separate low-priority pool and never on the keystroke path.
- **Persisted index plus change cursor,** so a restart does not mean a rescan where the OS allows it (§5).

## 4. Feature scope

### v1 (MVP)
- Index local volumes, live-updated (§5); configurable include/exclude paths.
- Query language:
  - substring default; AND (space), OR `|`, NOT `!`, grouping, quotes;
  - wildcards `*` `?`, `regex:`;
  - modifiers `case:`, `path:`, `wholeword:`, `file:`, `folder:`.
- Filters: `ext:`, `size:`, `dm:`/`dc:` (with ranges and words such as `today`, `last7days`), `parent:`, `depth:`, `type:` macros (`audio:`, `video:`, `pic:`, `doc:`, `archive:`, `exe:`).
- **CLI** `ferrow <query>`: plain, JSON and NUL-separated output, sort and limit flags, for scripts and launchers.
- **GUI:**
  - search box, virtual result list (millions of rows), sortable columns, match highlighting;
  - open, open folder, copy path, drag and drop;
  - native context menu where the OS has one;
  - global hotkey and tray/menu-bar icon;
  - light and dark theme following the OS.
- Local IPC API, versioned and documented, so other tools (launchers) can query the daemon.

### v1.x
- Bookmarks and saved filters; search history; run count (`runcount:`, `dr:`).
- Duplicate finder `dupe:` (name, size, then hash on demand).
- Preview pane.
- Indexing network shares and removable drives (rescan + watch), and file-list import/export.
- Remote search: a Ferrow daemon serving another machine over an authenticated, encrypted connection.

### Later / maybe
- Content search with an optional content index.
- Media properties (image size, audio tags).
- HTTP/web UI.

### Never
- Telemetry or any network traffic that is not explicitly enabled.
- Bundled third-party branding or assets.

## 5. Platform backends

| | Windows | Linux | macOS |
|---|---|---|---|
| Full scan | MFT enumeration (`FSCTL_ENUM_USN_DATA`) on NTFS/ReFS; parallel walk elsewhere | parallel `getdents64` walk | parallel walk, `getattrlistbulk` |
| Live updates | USN journal; `ReadDirectoryChangesW` fallback | `fanotify` (`FAN_REPORT_DFID_NAME`, filesystem marks) when privileged; `inotify` fallback | FSEvents |
| Restart catch-up | saved USN cursor | parallel rescan diffed against the saved index | FSEvents `sinceWhen` |

Every backend reports which mode it runs in and why (for example "fanotify refused: no CAP_SYS_ADMIN → inotify mode"), and the UI shows it.

## 6. Architecture (proposed)

```
ferrow (CLI) ─┐                    ┌─ backend-windows (MFT + USN)
ferrow-gui ───┼─ IPC ─ ferrowd ────┼─ backend-linux   (fanotify / inotify)
launchers ────┘     (daemon)       └─ backend-macos   (FSEvents)
                       │
                  ferrow-index  (arena, columns, sort orders, snapshotting, persistence)
                  ferrow-query  (parser → plan → parallel matcher)
```

- `ferrowd` is the only process that needs privileges, and it is **required for v1 on all three platforms** (owner, 2026-11-06). It runs as a Windows service under LocalSystem, a Linux systemd system service with minimal capabilities, and a macOS LaunchDaemon with Full Disk Access. The UI and CLI stay unprivileged. Plan: [`milestones/M2-service.md`](milestones/M2-service.md).
- **Single-user fallback:** with no daemon installed, the CLI and GUI can index in-process using the unprivileged backends.
- **Privacy:** the daemon must only return entries that the requesting user may list. This applies on multi-user machines, where a privileged index would otherwise leak names. This is a security requirement, not an option.
- Crates: `ferrow-index`, `ferrow-query`, `ferrow-backend-*`, `ferrow-ipc`, `ferrowd`, `ferrow` (CLI), `ferrow-gui`.

## 7. Quality bar

- `cargo fmt`, `clippy -D warnings`, tests on all three OSes in CI (GitHub-hosted runners, public repo).
- Benchmarks on a generated fixture tree (a synthetic 5 M-entry index) are tracked per PR. A regression against the §3 targets fails CI once the targets are confirmed.
- Fuzzing for the query parser and the index persistence format.
- Dependencies: permissive licences only (MIT, Apache-2.0, BSD, Zlib, ISC, Unicode), checked by `cargo-deny` in CI.

## 8. Build and repository conventions

- Toolchain pinned in `rust-toolchain.toml` (stable 1.99.1 on 2026-11-05).
- The committed `Cargo.toml` holds the portable build profiles. `.cargo/config.toml` is **machine-local and gitignored**; `.cargo/config.example.toml` shows an optional local setup (sccache wrapper, cache dir, target dir outside the repository).
- License: MIT (same as the owner's other public Rust project, kwil).

## 9. Owner decisions so far

- 2026-11-06: excludes **fail closed**. If the scanner cannot determine whether an entry is excluded (for example, its identity query fails), it leaves the entry out and reports the error.
- 2026-11-06: **Windows Start menu integration** is parked for now. Microsoft documents no supported way to add local file results (the web-provider API is EEA-only and returns web links, and cloud-files search needs a Copilot+ PC). An unofficial injection add-on is a watched option, to be revisited after M2.
- 2026-11-06: a background index service on every platform (see §6).
- 2026-11-06: a Pair Desk project `ferrow` tracks issues and the handoff.
- 2026-11-07: **high performance is the most important property of the app.** Every milestone measures its effect on the §3 numbers in CI. The speed work (M2e) comes next after M2a/M2b land, before per-user visibility (M2c) and the MFT/USN backend (M2d). The query path through the service is benchmarked like the in-process path.
- 2026-11-07: **never freeze.** Freezes in existing tools are why Ferrow exists. No code path may block the UI, a search or the service on indexing, disk I/O, a slow or unreachable volume, a lock or a large result set. Work is bounded and cancellable, and freeze resistance is tested (for example with mocked slow stages).

## 10. Open decisions for the owner

1. **GUI toolkit:**
   - **Recommendation:** decide after a short spike that measures the 5 M-row virtual list and the start time.
   - Candidates: Slint, egui, iced, or a custom winit + softbuffer/wgpu renderer like kwil.
2. **Confirm the §3 targets** (or set others).
3. **v1 scope in §4:** add or remove items.
4. **Pair Desk project** `ferrow` for issues and the handoff.
5. **When to create the public GitHub repository** `rennerdo30/ferrow`. That is a publication, so it needs the owner's yes.
