# ADR 0002: per-user visibility in the system service (M2c)

Status: proposed, **revision 3, review 3 requires 5 changes** (`0002-visibility-review-3.md`, 2026-11-07). Revision 1 (scan-time
descriptors): review 1 required 13 changes. Revision 2 (per-search checks):
review 2 required 10 more (`0002-visibility-review-2.md`). The owner chose
strict filtering over simpler options. Needs review 3 before code; the
ordinary-caller gate stays closed until a review accepts it. Live updates
(ADR 0005) are separate.

## Goal and threat model

The system service runs as SYSTEM and indexes every name on fixed local
NTFS and ReFS volumes. A non-elevated caller (including a filtered admin
token) may learn from it **only names and metadata that the caller could
obtain from the file system at the moment of the search by listing folders
from the volume root down**. Elevated administrators and SYSTEM are
unfiltered.

Trusted: the installed service binary and its directory (writable only by
SYSTEM and Administrators), the kernel. Untrusted: every other local user,
their processes, and any file system content they control. A compromised
installed service is trusted-code compromise: it could keep a caller's token
and impersonate them; nothing below defends against that, and the install
ACLs (`protection.rs`) are part of the security boundary.

## Rule

For a filtered caller, the service returns a row only if, within the request
and **acting as the caller on the OS thread doing the work**:

1. **Folder chain.** Starting from the volume root, each directory on the
   row's path is opened *relative to the previous handle* for
   `FILE_LIST_DIRECTORY`, without reparse traversal, and its
   `(volume serial, 128-bit file id)` equals the one recorded for that
   directory in the snapshot. Any mismatch, error or reparse point: hidden.
2. **Current child.** The row's own name is opened relative to its parent
   handle for `FILE_READ_ATTRIBUTES` (granted when the parent allows listing),
   without reparse traversal. If it does not exist now: hidden. This closes
   historical names (review 2 item 1): a file moved out of a folder can no
   longer be returned from it, whoever is later granted the folder.
3. **Fresh metadata.** Size, dates and attributes returned for the row are
   read from that child handle now, never from the index. A metadata filter
   (`size:`, `dm:` …) is re-evaluated on the fresh values; a row that no longer
   matches is dropped.

Opens use `NtCreateFile` with `RootDirectory` set to the parent handle,
`FILE_OPEN_REPARSE_POINT`, **without** `FILE_OPEN_FOR_BACKUP_INTENT` (so no
backup-privilege bypass for anyone, including Backup Operators; review 2
item 6), share mode read|write|delete, synchronous I/O. The kernel evaluates
everything else: deny and inherited ACEs, owner rights, mandatory labels,
conditional ACEs and central access policies, restricted and AppContainer
tokens, traverse with the caller's own privileges.

"Stricter than the OS" (a folder reachable only by knowing its path is
hidden) was accepted as policy in review 1.

## Who is filtered

Unfiltered: the token user is SYSTEM, or the token is elevated
(`TokenElevationTypeFull`) with an enabled Administrators group **and** is
not restricted, not AppContainer and at least high integrity. The exclusions
are checked first (no SID shortcut). Everyone else authenticated is filtered.
Anonymous, identification-only or missing tokens are refused.

## Snapshot contents (review 2 items 2 and 3)

Only volumes whose identity Windows guarantees are served to filtered
callers: local fixed NTFS and ReFS. FAT, exFAT, network, removable and
`subst`-only paths are indexed for unfiltered callers only (deduplicating
`subst` is not a security measure).

The service's scan of such a volume:

- Opens the volume root (`\\?\Volume{GUID}\`), then every directory **relative
  to its parent's handle** (no path strings, no reparse traversal; reparse
  points are recorded as leaves).
- From **the same handle** it reads the directory's `FILE_ID_INFO` (volume
  serial + full 128-bit id) and enumerates it
  (`FileIdExtdDirectoryInfo`, with buffer and continuation validation).
- Records, per directory, the 128-bit id (16 bytes per directory, about 1.6
  bytes per entry); per volume, the volume GUID and serial; per configured
  root below a volume root, the ids of its ancestors up to the volume root.

The index, these records and the volume table are one immutable snapshot
published through the existing `SnapshotStore` swap; a request holds one
snapshot from start to serialisation. A snapshot without these records (an
older index file, an unsupported volume) serves filtered callers nothing.
Volume attach/detach, reformat or a changed volume serial invalidates that
volume for filtered callers until it is rescanned. File ids can be reused
after deletion; rule 2 (current child) and the per-component id check make a
reused id harmless: a stale snapshot can only hide rows, never show a row
that is not currently listable.

## Impersonation (review 2 items 4 and 6)

- **Client.** For the system endpoint only, the client connects with
  `SECURITY_SQOS_PRESENT | SECURITY_IMPERSONATION | SECURITY_EFFECTIVE_ONLY`
  (static tracking). Before writing anything — Hello included — on every
  connection and reconnect, it authenticates the server:
  1. the existing connected-pipe owner check (`authenticate_server`, owner
     LocalSystem) stays;
  2. `GetNamedPipeServerProcessId` → `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)`;
     the handle is held for the whole check, so the process cannot exit and
     its id be reused meanwhile;
  3. that process's token user is LocalSystem, and its image
     (`QueryFullProcessImageNameW`) opened by path has the same volume and
     file id as `%ProgramFiles%\Ferrow\service\ferrowd.exe` (the known folder
     is the trusted source);
  4. the pipe is checked again to still be connected to that process
     (`GetNamedPipeServerProcessId` unchanged).
  Any failure: disconnect without writing. A squatter can impersonate only
  after reading a client write, so it never obtains the token. Development
  and custom endpoints never raise the level.
- **Server.** The token is captured per request frame (as today), duplicated
  at impersonation level and owned by that request. Stream clones used for
  writing carry no token. The request's token is released when its last
  authorization job finishes.
- **Authorization pool.** Directory and child opens run only on a dedicated
  pool of 4 OS threads, never on Rayon or async executors. A job is: install
  the request's token with `SetThreadToken` (checked), do the opens, then
  `RevertToSelf` (checked). No other code runs inside the guard, guards
  never nest, and a failed revert terminates the service process (the
  service restarts through its failure actions) rather than returning a
  thread that might act as someone else.

## Search pipeline for a filtered caller

The query engine (Rayon, as SYSTEM, in memory only) produces candidate rows
in the requested order, in batches; the request thread hands each batch to the
authorization pool and keeps only authorized rows (rules 1–3), memoising
per request: `directory → authorized handle or denied`. Rows are emitted only
after authorization; limits, pagination, `more` (meaning *more authorized
rows may exist*), byte budgets and highlights apply to authorized rows only.
Filtered and unfiltered requests are different internal request types, so a
missing authorization context cannot reach the unfiltered handler (review 2
suggestion; review 1 item 9).

**Ordering.** Candidates are ordered by indexed values; the page is re-sorted
by the fresh values before it is returned. Across pages, a row's position can
still reflect its indexed (older) size or date — see residual risks.

**Count.** A filtered count authorizes every candidate (rules 1–3). It is
exact if it finishes within the request budget, else returned as a lower
bound (`at_least`).

## Budgets, stalls and fairness (review 2 items 7 and 8)

- Each filtered request has a total budget (default 1 s for searches, 3 s for
  counts) covering parsing, scanning, authorization and serialisation.
- An authorization job that exceeds 200 ms marks its **volume** as stalled for
  filtered callers: rows from it are treated as hidden until the job
  completes. The response never waits for a stalled job; the stalled thread
  keeps its token until the OS returns, then reverts and releases it.
  At most 4 jobs can be outstanding (the pool size); when all threads are
  stalled, filtered requests get `Busy`. Unfiltered requests never use the
  pool.
- Connections: at most 4 per user SID; the handshake must complete within 2 s;
  an idle filtered connection closes after 30 s; 4 of the 32 connection slots
  are reserved for SYSTEM and elevated administrators. At most 2 filtered
  requests per SID run at once, scheduled round-robin across SIDs; the queue
  holds at most 16 requests, then `Busy`.

## Status and errors

Unchanged from revision 2: a versioned filtered `Status` built from an
allowlist (protocol version, `visibility: filtered`, ready/scanning), no
totals, report strings, paths or samples; errors to filtered callers carry no
paths.

## Residual risks the owner must accept (review 2 item 9)

These cannot be removed without per-user indexes, and are stated so they can
be accepted or rejected explicitly:

1. **Existence inference by chosen queries.** A caller can type a guessed name
   or prefix and learn from response time, budget exhaustion, `partial` or
   `at_least` whether hidden names matching it exist (not what they are, and
   not where).
2. **Cross-page ordering by older metadata.** When sorting by size or date,
   the page a currently listable file appears on is chosen from its indexed
   value, which may be older than the fresh value shown.

Mitigations: budgets cap the signal's resolution; elapsed times are not sent
to filtered callers.

## Unix

Unchanged: non-root callers stay `Unauthorized` until a revision covers
effective groups, ACLs and MAC (review 1 items 5 and 6).

## Tests (merge gates; disposable Windows CI only)

Two local users A and B, the installed service, a prepared tree:

- **Static:** A-only, B-only, both; deny on a parent with allow on a child;
  inherit-only and deny ACEs; owner-rights ACE; an explicit **no-read-up**
  mandatory label against a medium caller; traverse denied for a caller
  without `SeChangeNotifyPrivilege`; a junction, a symlink and a mount point in
  a path; hard links in an A-only and a B-only folder; a configured root whose
  ancestor denies B; expected sets defined for this stricter policy (not plain
  equality with `Get-ChildItem`).
- **Freshness:** revoke and restore access between two searches; move a file
  out of a folder, then grant B the folder (old name must not appear); replace
  a file by another of the same name (fresh metadata only); delete and reuse.
- **Binding:** swap a directory for a junction during the scan (mocked
  scheduler in unit tests, real swap in CI); an index file without identity
  records; a changed volume serial.
- **Tokens:** restricted token and low-integrity process for A; a
  non-elevated admin is filtered; an elevated admin is not; a failed
  `SetThreadToken`/`RevertToSelf` (fault injection) terminates the service;
  no token outlives its request (handle counts).
- **Squatting:** a pipe server running as A, created first and **able to
  impersonate**, never receives a write from B's client and never holds B's
  token.
- **Stalls and fairness:** a mocked blocked open; all pool threads stalled →
  `Busy`; A holding 4 idle connections and many broad queries does not starve
  B or an administrator.
- **Every query kind:** small/broad, prefix, OR/NOT, each sort, count exact and
  lower-bound, partial page, highlights, filtered Status.

## Performance

Unfiltered callers: unchanged; the paired gate stays strict. Filtered callers:
a CI arm on the installed service over a real tree reports first page and
count with cold per-request memos, split into scan, authorization and
serialisation; the open cost is measured, not assumed.

## Not decided here

Unix filtering, live updates (ADR 0005), any cross-request authorization
cache (only with a freshness mechanism a future review accepts).
