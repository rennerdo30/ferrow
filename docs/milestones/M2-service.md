# Milestone 2: background index service, IPC, per-user privacy, Windows MFT/USN

Status: **specified 2026-11-06.** This builds on M1 (`docs/milestones/M1-base.md`) and [`../SPEC.md`](../SPEC.md) §5–§6.

Owner requirement (2026-11-06): a background index service runs on every platform, because a system-level service is what gives the indexer the rights it needs (on Windows, the MFT and the USN journal).

## Goal

`ferrowd` runs as a system service on Windows, Linux and macOS. It holds the index and answers queries over a local, access-controlled IPC channel. It returns only the entries the calling user may list. On Windows it builds the index from the NTFS Master File Table and keeps it live from the USN journal. The CLI talks to the service when it runs, and falls back to M1's in-process mode when it doesn't.

## Work packages

Each package gets its own branch and PR, stacked in this order. Each one is reviewed before the next one lands.

### M2a: `ferrow-ipc`, the protocol and transport
- **Framing:** a versioned, length-prefixed binary format. Requests are `Hello{version}`, `Query{text, limit, sort, flags}`, `Count{text}`, `Status` and `Cancel{id}`. Responses are `Results{paths…}` streamed in pages, `Count{n}`, `Status{entries, mode, backend report}` and `Error{code, msg}`.
- **Robustness:** the parser never panics and enforces frame size limits. Write fuzz-style tests for it.
- **IPC deadlines:** public helpers renew a five-second budget for each handshake, request write and response frame. The budget covers partial frame I/O without renewal on byte progress. Servers allow five minutes of idle time between requests; the first byte starts the separate five-second frame budget. Embedders implement `DeadlineIo` to enforce these boundaries. Idle expiration is a resource policy, not the lifetime of an otherwise active session.
- **Windows transport:** a named pipe `\\.\pipe\ferrowd` with an explicit security descriptor. The service account and administrators get full control; authenticated users may connect, read and write. Remote clients are rejected (`PIPE_REJECT_REMOTE_CLIENTS`).
- **Unix transport:** Linux `/run/ferrow/ferrowd.sock`, macOS `/private/var/run/ferrow/ferrowd.sock` (the real directory behind `/var/run`). System endpoint directories are root-owned 0755; development endpoints live in a uid-owned 0700 directory under the canonical temporary directory. Ancestors must be trusted and not writable by others, except root-owned sticky temporary ancestors. Symlinked ancestors are refused. A lifetime lease serializes binds; only owned, stale sockets can be reclaimed, and drop unlinks only the original socket inode.
- **Caller identity comes from the OS, never from the client's own claim:**
- **Unix server authentication:** before Hello, clients validate the socket directory and its ancestors, then check the connected server's OS peer uid: root for the system endpoint, current effective uid for development. The directory must have that same owner and no other writers. Failure is a connection refusal with a reason, allowing the CLI's explained in-process fallback.
  - Windows: `GetNamedPipeClientProcessId`, then the client token (or `ImpersonateNamedPipeClient`).
  - Linux: `SO_PEERCRED`.
  - macOS: `getpeereid` / `LOCAL_PEERCRED`.
- **Tests:** client and server round trip in one test process using an unprivileged per-test pipe or socket name; frame-limit tests; version mismatch handling.

### M2b: `ferrowd`, the service host
- **Interim privacy boundary until M2c:** the system service answers only OS-authenticated SYSTEM/Administrators on Windows or uid 0 on Unix. All other Query/Count/Status requests return `Error{Unauthorized}` without paths, counts or status data. The CLI explains the refusal and falls back to its per-user saved index. Development mode serves only its OS-derived starting user. M2c removes this interim restriction when per-user visibility is enforced.
- **Index ownership:** holds the index as an `Arc` snapshot. Updates build a new snapshot and swap it in, so readers never block (SPEC §3).
- **Windows:** a Service Control Manager service via the `windows-service` crate (check its licence), running as LocalSystem. Start, stop and shutdown are handled cleanly.
- **Linux:** a systemd unit file. It grants only the needed capabilities (`AmbientCapabilities=CAP_DAC_READ_SEARCH CAP_SYS_ADMIN`) and uses hardening options.
- **macOS:** a launchd LaunchDaemon plist. The docs explain the Full Disk Access grant.
- **CLI:** `ferrow service install | uninstall | status | run`. `run` starts the daemon in the foreground for development, unprivileged and with a per-user pipe or socket name. Install and uninstall need admin rights once. Without admin rights they print what is needed, and they never try to elevate themselves.
- **Client fallback:** CLI search talks to the service when it is reachable. Otherwise it falls back to the M1 in-process index. `ferrow status` says which mode is active and why (SPEC §5).
- **Idle cost:** 0 % CPU when idle, with no polling timers.
- **Persistence:** the index file is readable only by the service (a SYSTEM/admin ACL on Windows, mode 0600 root on Unix). It is saved on shutdown and periodically after changes.

### M2c: per-user visibility (security-relevant, ADR first)
- **Rule:** the service only returns entries that the calling user could list. That means every ancestor directory is traversable and the containing directory is listable for that user.
- **ADR `0002-visibility.md` before any code:**
  - Windows: `AccessCheck` with the caller's impersonation token against the directory security descriptors (`FILE_LIST_DIRECTORY`, `FILE_TRAVERSE`).
  - Unix: uid/gid/supplementary groups against mode bits, plus POSIX ACLs where present.
  - Decisions are cached per (caller identity, directory id) and invalidated on change events.
  - Fail-closed: when a check errors, the entry is hidden.
- **Tests:** a two-user matrix wherever CI can create a second user (Linux and Windows hosted runners can), plus unit tests of the cache and invalidation.
- **Review:** a separate security review before merge.

### M2d: Windows MFT and USN backend (`ferrow-backend-windows`)
- **Full scan:** `FSCTL_ENUM_USN_DATA` over each local NTFS/ReFS volume. It builds entries from file reference numbers and parent references. Hard links and multiple names are handled; see the ADR.
- **Live updates:** `FSCTL_READ_USN_JOURNAL` with a persisted journal id and cursor. On restart it replays only the delta. A journal reset or wrap triggers a rescan, and it says so.
- **Index core change:** the index needs incremental insert, delete and rename. Design it in an ADR: a mutable delta layer over the frozen columns, periodically re-frozen and swapped. The M1 layout and the per-entry memory budget stay, and a query must keep seeing one consistent snapshot.
- **Reporting:** the backend reports its mode, for example `mft: C: 1234567 entries in 2.1 s; usn live` or `walk fallback: not elevated`.
- **Testing:** MFT and USN calls need admin rights. They run only in CI on `windows-latest` (hosted runners are elevated), or in Windows Sandbox. Never elevated on the owner's PC. Local tests mock the USN record stream.

### M2e: speed items from the M1 reviews
These are measured with additive timings (shared rule S10) on the CI bench:
- faster CRC-32 (same polynomial, no format change);
- count in arena order;
- remove the per-block scheduling barriers;
- freeze scratch buffers;
- the load path's derived-data validation cost.

Goal: move toward the SPEC §3 targets: load ≤ 300 ms and full count ≤ 50 ms at 5 M entries.

## Open security gaps (accepted, to fix)

- **G1, name and metadata inference through the system service**
  (accepted by the owner 2026-11-07; details and fix options in
  [ADR 0002](../decisions/0002-visibility.md#known-gap-g1-name-and-metadata-inference-accepted-2026-11-07-open-todo)).
  Any caller the service admits whose token cannot list some indexed folder
  (another local account, or a restricted, low-integrity or AppContainer
  process of the same account) can, with many guessed queries, reconstruct
  names and folder paths of files it cannot list and estimate every indexed
  property (size, dates, attributes, type); it can never open or read them.
  **TODO:** per-user candidate structures (or constant-work queries) built on
  live updates (M2d), with a new design and security review. Until then the
  service docs, the install guide and the setup page state the gap.
- **G2, revocation during a row's hold** (owner accepted 50 ms on 2026-11-07; revision 6 shrinks it to 5 ms; [ADR 0002](../decisions/0002-visibility.md#known-gap-g2-revocation-during-a-rows-hold-accepted-2026-11-07-open-todo)):
  a permission removed while a search holds that folder chain (at most 5 ms)
  may still let rows of that hold through. **TODO:** measure hold times and
  lower the limit if the open cost allows.

## Not in M2
The Linux fanotify and macOS FSEvents live backends (M3), the GUI (its toolkit spike runs separately), installers, and release builds. Installers and releases need the owner's yes.

## Acceptance (finite list)
1. M2a: the IPC crate with a versioned protocol, both transports, OS-derived caller identity, and tests, green on all three CI platforms.
2. M2b: `ferrowd` with `service install/uninstall/status/run`. CI installs, starts, queries, stops and uninstalls the service on `windows-latest` and `ubuntu-latest`. macOS covers `run` mode and a plist lint. The CLI fallback is tested, and idle CPU is checked over a short window in CI.
3. M2c: ADR 0002 is accepted by a security review, and the visibility filter has two-user tests in CI.
4. M2d: the MFT scan and USN live updates work on the CI runner's system volume: create, rename and delete show up in query results within 1 s. The cursor survives a service restart. The delta-layer ADR is written.
5. M2e: the CI bench shows the additive load breakdown, and each speed change has a before/after measurement.
6. The docs cover the service and the privacy model, and the README gains service usage.
