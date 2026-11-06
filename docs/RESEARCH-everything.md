# Research: what voidtools Everything does (2026-11-05)

Reference for defining Ferrow. Everything is closed-source freeware for Windows by voidtools. Ferrow is an independent, original implementation: we take no code, name, artwork or UI from it. This page only describes Everything's features so we can decide what Ferrow should cover.

Versions (checked 2026-11-05): stable 1.4.1.x; 1.5 is still an **alpha** (build 1.5.0.1408a, April 2026), shipped as a separate instance next to 1.4.

## 1. Indexing

| Feature | How Everything does it |
|---|---|
| Initial scan | Reads the NTFS **Master File Table** directly instead of walking directories. That is why a full index of a drive takes seconds. Needs admin rights, or the "Everything Service" running as SYSTEM. |
| Live updates | Follows the NTFS/ReFS **USN change journal**, so changes appear almost at once with no polling. |
| Restart | Saves the index to `Everything.db` with the last journal position, then replays only the journal delta on start. |
| Other file systems | "Folder indexing" for FAT/exFAT, network shares and mapped drives: a periodic rescan, plus optional change monitoring. |
| File lists | `.efu` (CSV) lists that can be indexed and searched like a drive, for offline media or archives. |
| Indexed columns | Name and path always. Size, date modified/created/accessed, attributes and folder size are optional. 1.5 can also index any Windows property (image size, audio tags and so on) and, optionally, file content. |
| Excludes | Paths, wildcards, hidden/system files, whole volumes. |
| Run count | Tracks how often a result was opened (`runcount:`, `dr:` = date run). |

## 2. Search

- **Instant, while typing.** Every keystroke re-queries the whole index, and results update live as files change.
- **Default match:** case-insensitive substring on the name. Space = AND, `|` = OR, `!` = NOT, `< >` = grouping, `"..."` = literal phrase.
- **Wildcards** `*` `?` (these match the whole name), and **regex** via `regex:`.
- **Modifiers:** `case:`, `wholeword:`, `path:`, `diacritics:`, `file:`, `folder:`, `nocase:` and so on, either per term or as global toggles.
- **Functions (filters):**
  - Size: `size:>1mb`, ranges `2mb..10mb`, named sizes such as `huge`.
  - Dates: `dm:` `dc:` `da:` `dr:` with natural ranges (`dm:last7days`, `dm:2026`).
  - Location and shape: `ext:`, `type:`, `attrib:`, `parent:`, `depth:`, `childcount:`, `empty:`, `len:`, `root:`.
  - Media and properties (1.5): `width:`, `height:`, `artist:`, `album:`, …
  - Duplicates: `dupe:` (1.5 can combine up to 8 properties, e.g. `dupe:name;size`, `dupe:size;sha256`), `dupefrom:`.
  - Content: `content:`, `utf8content:`. Not indexed, so very slow on a big result set.
  - File lists: `filelist:`.
- **Macros:** `audio:`, `video:`, `pic:`, `doc:`, `exe:`, `zip:`, plus user-defined macros.
- **Filters and bookmarks:** saved searches with their sort and column layout; search history.
- **Sort** by any column, instantly. Indexed columns are kept pre-sorted, so re-sorting millions of rows is free.

## 3. User interface

- One window: a search box, a virtual results list (millions of rows, only visible rows are drawn) and a status bar.
- Global hotkey and tray icon; the window shows instantly.
- Full shell integration: the native context menu, drag and drop, open/open path/copy path, rename and delete in place.
- Search terms are highlighted in names and paths.
- 1.5 adds a **preview pane**, sidebar filters, a dark theme, multiple windows and a "tabs-like" filter bar.
- Customisable columns, fonts, keyboard shortcuts and context menu entries.

## 4. Remote and integration

- **ETP server:** an Everything client can search another PC's index.
- **HTTP server:** search and download files from a browser.
- **FTP server.**
- **`es.exe` CLI**, which talks to a running Everything over IPC.
- **SDK:** a DLL and IPC API used by launchers (PowerToys Run, Flow Launcher, Listary and others).
- Command-line options and the `es:` URL protocol.

## 5. Known pain points (why we build our own)

- **Windows only.** There is no Linux or macOS version.
- **Closed source,** with one developer and a slow release cadence: 1.5 has been in alpha for years.
- **Performance regressions in 1.5:**
  - Reported memory leaks in the service (1.5.0.1364a–1365a, fixed in 1366a).
  - Random high CPU when idle.
  - High RAM use (25–40 % reported).
  - Expensive "total size" and property columns.
  - `content:` scans unindexed files.
- The index lives in one process with a single UI thread; heavy property gathering and content search compete with typing.

Our own observation (owner, 2026-11-05): "lately the performance is really really bad."

## 6. Cross-platform equivalents (for Ferrow's design)

| Need | Windows | Linux | macOS |
|---|---|---|---|
| Fast initial scan | MFT read via `FSCTL_ENUM_USN_DATA` (admin/service); fallback to parallel directory walk | parallel `getdents64` walk (no MFT equivalent) | parallel walk with `getattrlistbulk` |
| Live changes | USN journal (`FSCTL_READ_USN_JOURNAL`); fallback `ReadDirectoryChangesW` | `fanotify` with `FAN_REPORT_DFID_NAME` (kernel ≥ 5.1, whole filesystem, needs `CAP_SYS_ADMIN`); fallback `inotify` (per-directory watches, limited count) | FSEvents (whole volume, persistent event IDs) |
| Catch-up after restart | stored USN cursor | none from the kernel: rescan, but in parallel and diffed against the saved index | FSEvents `sinceWhen` event ID |
| Privilege model | small indexing service (SYSTEM) + unprivileged UI | optional privileged daemon (fanotify) or unprivileged inotify mode | unprivileged (Full Disk Access permission for protected folders) |

Sources: [voidtools searching help](https://ftp.voidtools.com/support/everything/searching), [find duplicates](https://www.voidtools.com/es-ve/support/everything/find_duplicates), [1.5 changelog thread](https://voidtools.com/forum/viewtopic.php?p=58024), [1.5 alpha memory leak](https://ns1.voidtools.com/forum/viewtopic.php?p=63780), [RAM 25–40 %](https://mail.voidtools.com/forum/viewtopic.php?p=63499), [Everything review 2026](https://filelocator-net.pages.dev/reviews/everything/).
