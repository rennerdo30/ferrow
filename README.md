<p align="center">
  <img src="docs/assets/logo.svg" width="160" alt="Ferrow logo: a ferret peeking out of a magnifying-glass burrow">
</p>

<h1 align="center">Ferrow</h1>

<p align="center">
  <strong>Find any file by name, as fast as you type.</strong><br>
  A live file index for Windows, Linux and macOS, written in Rust.
</p>

<p align="center">
  <a href="https://github.com/rennerdo30/ferrow/actions/workflows/ci.yml"><img src="https://github.com/rennerdo30/ferrow/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="MIT license"></a>
</p>

---

Ferrow keeps an index of every file and folder on your drives in memory and answers each keystroke in milliseconds, even with millions of entries. It follows the file system's own change feeds, so results stay current without rescanning, and it uses no CPU while you are not searching.

> **Status: milestone 1.** The index core, parallel walk backend, query engine and CLI work (see "Try it" below); live updates and the GUI are next. The draft specification is in [`docs/SPEC.md`](docs/SPEC.md); feedback is welcome in the issues.

## Why

Instant file-name search has been a Windows-only, closed-source niche for a long time. Ferrow aims to make it:

- **Fast first.** Speed and a light footprint come before any feature. Every pull request is measured against fixed targets.
- **Cross-platform.** Windows, Linux and macOS are supported from day one, each with its native fast path.
- **Open.** MIT-licensed, no telemetry, no network traffic unless you turn it on.

## Planned features

- **Results on every keystroke**, sortable by any column at no extra cost (sort orders are kept precomputed).
- **Always up to date:**

  | | Initial scan | Live updates |
  |---|---|---|
  | Windows | NTFS Master File Table | USN change journal |
  | Linux | parallel directory walk | fanotify, falling back to inotify |
  | macOS | parallel `getattrlistbulk` walk | FSEvents |

- **A real query language:** AND/OR/NOT, grouping, wildcards, `regex:`, and filters such as `ext:`, `size:`, `dm:` (date modified), `path:`, `parent:` and `depth:`.
- **Desktop GUI:** a virtual result list for millions of rows, match highlighting, a global hotkey, and a tray or menu-bar icon.
- **CLI** `ferrow <query>` with plain, JSON and NUL-separated output for scripts and launchers.
- **Local API**, versioned, so launchers and other tools can query the index.
- **Private on shared machines:** a user only ever sees entries they are allowed to list.

## Proposed speed targets

These are drafts in [`docs/SPEC.md`](docs/SPEC.md) and will be enforced in CI once they are fixed.

| Measure | Target |
|---|---|
| Keystroke to first results, 5 M entries | ≤ 16 ms |
| Full re-query and sort, 5 M entries | ≤ 50 ms |
| Cold start with a saved index | ≤ 300 ms |
| File change visible in results | ≤ 1 s |
| Idle CPU | 0 % |

## Try it

Milestone 1 is a working core: scan a tree into an index, then search it.

```sh
# Build an index of one or more roots (saved to the platform data dir)
cargo run --release -- index ~/Documents /data

# Search it (case-insensitive substring match on file names)
cargo run --release -- search "report 2024"
cargo run --release -- search "!tmp log" --limit 20
cargo run --release -- search "case:Report" --json

# Index statistics (entries, bytes per entry, load time)
cargo run --release -- stats
```

Query syntax: space means AND, `!term` means NOT, `"quoted phrase"` is
literal, and `case:term` is case-sensitive. Useful flags: `--out <file>`
and `--index <file>` choose the index location, `--exclude <path>` skips
a subtree while indexing, and `--count` / `--null` change the search
output for scripts. Live updates, the daemon and the GUI come in later
milestones — see [`docs/SPEC.md`](docs/SPEC.md).

## Building

```sh
cargo build --release
```

The toolchain is pinned in [`rust-toolchain.toml`](rust-toolchain.toml); `rustup` picks it up automatically. No other setup is needed. [`.cargo/config.example.toml`](.cargo/config.example.toml) shows an optional local setup with an sccache compile cache.

### Running

Helper scripts build and start Ferrow in one step. Arguments after the optional release flag go to `ferrow`.

| | Windows (PowerShell) | Linux / macOS |
|---|---|---|
| Run (debug build) | `./scripts/run.ps1` | `./scripts/run.sh` |
| Run (optimised build) | `./scripts/run.ps1 -Release "*.rs"` | `./scripts/run.sh --release '*.rs'` |
| Run all CI checks | `./scripts/check.ps1` | `./scripts/check.sh` |

The check scripts run the same steps as CI on every push (format, clippy with warnings as errors, tests).

## Contributing

The design is still being settled in [`docs/SPEC.md`](docs/SPEC.md). Issues with ideas, platform knowledge or use cases are very welcome. Dependencies must use permissive licences (MIT, Apache-2.0, BSD, Zlib, ISC).

## License

[MIT](LICENSE)
