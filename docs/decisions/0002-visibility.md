# ADR 0002: per-user visibility in the system service (M2c)

Status: proposed, **revision 5** (2026-11-07). Review 4 required 4 changes
(`0002-visibility-review-4.md`), applied here. Revisions 1–3 and their
reviews: `0002-visibility-review-1.md` (13 changes), `-review-2.md` (10),
`-review-3.md` (5). The owner chose strict filtering and, on 2026-11-07,
**accepted known gap G1 below** (name, path and old-metadata inference
by repeated guessing) and known gap G2 (50 ms revocation window). Needs review 4 before code; the ordinary-caller gate
stays closed until a review accepts it. Live updates (ADR 0005) are separate.

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
   `FILE_LIST_DIRECTORY | FILE_TRAVERSE` (both rights on every directory used
   for descent, whatever the caller's bypass-traverse privilege; review 3
   item 3), without reparse traversal, and its
   `(volume serial, 128-bit file id)` equals the one recorded for that
   directory in the snapshot. Any mismatch, error or reparse point: hidden.
2. **Current child.** The row's own name is opened relative to its parent
   handle for `FILE_READ_ATTRIBUTES` (granted when the parent allows listing),
   without reparse traversal. If it does not exist now: hidden. This closes
   historical names (review 2 item 1): a file moved out of a folder can no
   longer be returned from it, whoever is later granted the folder.
3. **Attached now.** After the child open, the child handle's complete
   volume-relative name (`GetFileInformationByHandleEx(FileNameInfo)`, which
   must start with `\`; an incomplete or malformed result is rejected, never
   repaired) on the verified volume must equal the row's path built from the
   checked components **exactly, UTF-16 unit for unit**. Windows allows
   case sensitivity per directory, so no case-insensitive comparison is
   used: a case-only rename, or a move between `A` and `a` in a
   case-sensitive folder, makes the row hidden for that request (a
   conservative miss; the next scan picks up the new spelling). For a hard
   link the reported name is that handle's link; if it differs from the
   checked path the row is hidden, and the other link's path is never
   substituted (review 4 items 1 and 2).
4. **Fresh grants.** A directory grant (an open of rule 1) may be reused for
   other rows of the same request only within **50 ms** of being obtained;
   after that the chain from that directory down is opened again. So a row is
   admitted only if every directory on its path granted list+traverse to the
   caller within the 50 ms before the row's attachment check. Windows checks
   access when a handle is opened and does not revoke granted handles, so
   this bounded window is the guarantee: **a revocation is honoured for every
   row admitted more than 50 ms after it completed** (known gap G2, below).
5. **Fresh metadata.** Size, dates, attributes and the file/folder type
   returned for the row are read from that child handle now, never from the
   index. The **whole query expression** is evaluated again on the fresh
   values (OR, NOT, `file:`, `folder:`, `size:`, `dm:` …); a row that no
   longer matches is dropped. Candidate selection still uses indexed values,
   so which rows are *considered* can reflect old metadata; that is part of
   known gap G1 (review 3 item 1).

Namespace scope: names are the long names the scan enumerated; 8.3 short
names, alternate data streams and case-variant aliases are never rows and
never substitute for a checked name. Two names that differ only in case in
a case-sensitive directory are two separate rows, each checked on its own. A volume-root row is shown when the
volume root opened for the caller; a configured file root is a row like any
file.

Opens use `NtCreateFile` with `RootDirectory` set to the parent handle,
`FILE_DIRECTORY_FILE` for directories, `FILE_OPEN_REPARSE_POINT`,
**without** `FILE_OPEN_FOR_BACKUP_INTENT` (so no
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
     the handle is held for the whole check, so the process id cannot be
     reused meanwhile (the process can still exit; step 4 catches that);
  3. that process's token user is LocalSystem, and its image
     (`QueryFullProcessImageNameW`) opened by path has the same volume and
     file id as `%ProgramFiles%\Ferrow\service\ferrowd.exe` (the known folder
     is the trusted source). This identifies the file now at that path, not the
     loaded image; under the threat model (install folder writable only by
     SYSTEM and Administrators) that is sufficient;
  4. the pipe is checked again to still be connected to that process
     (`GetNamedPipeServerProcessId` unchanged).
  Any failure: disconnect without writing. A squatter can impersonate only
  after reading a client write, so it never obtains the token. Development
  and custom endpoints never raise the level.
- **Server.** The token is captured per request frame (as today), duplicated
  at impersonation level and owned by that request. Stream clones used for
  writing carry no token. The response may finish before a stalled job does;
  the token is released when the request's **last outstanding job returns**,
  never earlier (closing it under a running job is not allowed) and never
  later.
- **Authorization pool.** Directory and child opens run only on a dedicated
  pool of 4 OS threads, never on Rayon or async executors. The SYSTEM and
  elevated-admin exemptions apply only after the restricted, AppContainer and
  integrity exclusions pass; a failed token query denies the exemption. A job is: install
  the request's token with `SetThreadToken` (checked), do the opens, then
  `RevertToSelf` (checked). No other code runs inside the guard, guards
  never nest, and a failed revert terminates the service process (the
  service restarts through its failure actions) rather than returning a
  thread that might act as someone else.

## Search pipeline for a filtered caller

The query engine (Rayon, as SYSTEM, in memory only) produces candidate rows
in the requested order, in batches; the request thread hands each batch to the
authorization pool and keeps only authorized rows (rules 1–5), memoising
per request: `directory → authorized handle or denied` (at most 4 096 open
handles per request, least recently used closed first; batches of 256 rows).
Rows are emitted only
after authorization; limits, pagination, `more` (meaning *more authorized
rows may exist*), byte budgets and highlights apply to authorized rows only.
Filtered and unfiltered requests are different internal request types, so a
missing authorization context cannot reach the unfiltered handler (review 2
suggestion; review 1 item 9).

**Ordering.** Candidates are ordered by indexed values; the page is re-sorted
by the fresh values before it is returned. Across pages, a row's position can
still reflect its indexed (older) size or date: known gap G1.

**Count.** A filtered count applies **all five rules**, including the fresh
whole-expression evaluation (rule 5), before counting a row: a row that
search would drop is never counted (review 4 item 3). "Exact" means every
candidate was validated within the budget; because candidates come from the
shared index (gap G1), it is not a claim about every matching file on disk.
Otherwise the count is a lower bound (`at_least`).

## Budgets, stalls, resources and fairness (review 2 items 7, 8; review 3 item 5)

- **Budget.** Each filtered request has a total budget (default 1 s for
  searches, 3 s for counts) covering parsing, scanning, authorization and
  serialisation. When it ends the response is sent (`partial` / `at_least`);
  queued jobs of that request are dropped, running jobs finish but their
  results are discarded (a per-request generation flag), so nothing is
  written late.
- **Stalls.** A job running longer than 200 ms marks its **volume** stalled
  for filtered callers until it returns: rows from that volume are hidden,
  other volumes keep working. The response, request teardown and dispatcher
  never join or wait on a stalled syscall; the dispatcher's existing joins
  are changed so a request's writer can finish while its jobs are still out.
- **Outstanding work.** Jobs count against their SID and volume until the
  syscall returns, not until the response ends: at most 2 outstanding jobs
  per SID and 2 per volume; at most 4 overall (the pool size). Beyond that,
  that SID's or volume's rows wait within the budget, then the request
  returns `partial` or `Busy`. A stalled SID cannot occupy the threads other
  SIDs need for healthy volumes.
- **Retained state.** A request holds one snapshot, at most 4 096 handles and
  at most 1 MB of batch and result state; at most 8 filtered requests hold
  snapshots at once, so old snapshots are released promptly.
- **Connections.** Before the handshake no SID is known: at most 8
  connections may be in the handshake at once, each must finish within 2 s.
  After it: at most 4 per user SID, idle filtered connections close after
  30 s, and 4 of the 32 slots are reserved for SYSTEM and elevated
  administrators. At most 2 filtered requests per SID run at once,
  round-robin across SIDs; the queue holds at most 16, then `Busy`.
- **Scan isolation.** The service's scan (and later the journal reader) runs
  on its own thread pool, never on the Rayon pool that answers queries, so
  blocked scan I/O cannot stall searches. (Today's walker uses the global
  pool; this changes for the service.)
- **Cost.** Exact broad counts are not achievable for filtered callers (5 M
  rows in 3 s on 4 workers leaves 2.4 µs per row), so they are lower bounds;
  responsiveness comes from prompt `partial` / `at_least` / `Busy` replies.

## Status and errors

Unchanged from revision 2: a versioned filtered `Status` built from an
allowlist (protocol version, `visibility: filtered`, ready/scanning), no
totals, report strings, paths or samples; errors to filtered callers carry no
paths.

## Known gap G1: name and metadata inference (accepted 2026-11-07, open TODO)

**This is a real disclosure gap, accepted by the owner for now and tracked
as future work** (`docs/milestones/M2-service.md`, "Open security gaps",
G1). It does not let anyone open or read a file.

**What leaks.** The service answers every caller from one shared index and
only filters the rows it returns. The work a search does — and therefore its
response time, whether the budget runs out, and the `partial` / `at_least`
flags — depends on all matching names, including hidden ones. Candidate
selection and cross-page order also use indexed (possibly older) metadata.

**What an attacker can recover.** Another local account that may query the
service and is willing to send many queries can:

1. learn whether a hidden name matching a chosen pattern exists;
2. by extending a prefix one character at a time, and narrowing with
   `path:` / `parent:`, **reconstruct full names and folder paths of files
   it cannot list**;
3. by sweeping `size:`, `dm:`/`dc:`/`da:`, `attrib:`, `file:`/`folder:`,
   `ext:` and every other searchable predicate, and by comparing sort
   positions with files of its own, **estimate every indexed property** of
   such files (size, dates, attributes, file or folder type), possibly from
   an earlier file that had the same name.

It cannot open, read or modify those files, and it cannot see them as
results.

**Who is affected.** Any caller the service admits whose effective token
cannot list some indexed folder: another local account, but also a
**restricted, low-integrity or AppContainer process of the same account**
(for example a sandboxed browser or app). On a single-account PC with no
sandboxed or restricted processes asking the service, nobody learns more
than they could list. Elevated administrators can see everything anyway.

**Mitigations now.** Budgets limit how fast the signal can be read, elapsed
times are not sent to filtered callers, and per-SID limits slow bulk
probing. These reduce the rate, not the kind of information.

**Future fix (TODO G1).** Remove the hidden-dependent signals, for example:

- per-user candidate structures: the service keeps, per active user, the set
  of directories that user can list (built acting as the user, kept fresh
  by live updates, ADR 0005), and searches only those, so no work, timing or
  ordering depends on hidden names;
- or constant-work queries over a caller-specific view.

Either needs live updates first, more memory per active user, and a new
design and security review. Until G1 is done, the docs (`docs/service.md`,
`docs/INSTALL.md`) and the setup page state that on machines with several
accounts other users may be able to infer file names (not contents) through
the service.

## Known gap G2: 50 ms revocation window (accepted 2026-11-07, open TODO)

Windows checks permissions when a handle is opened and never revokes a
granted handle. Rule 4 reuses a directory grant within a request for at most
50 ms, so a permission removed less than 50 ms before a row's attachment
check may still let that row through in that request. Any later request
honours it. Lowering the window costs directory opens (each path's chain is
opened again); 0 ms means opening every chain per row. Tracked with G1 in
`docs/milestones/M2-service.md` and Pair Desk. **Accepted by the owner on
2026-11-07**; TODO: measure the open cost and shrink the window.

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
  no token outlives the last outstanding job of its request, and none is
  closed while a job runs (handle counts).
- **Review 4 probes:** after a directory grant, revoke B's access and move a
  previously indexed secret into that folder (deterministic, mocked
  scheduler and real CI): rows admitted more than 50 ms after the revocation
  never show it; a move of `d` from `A` to `a` in a case-sensitive folder
  where B may list `A` but not `a`: hidden; a hard link whose handle reports
  the other link: hidden; Query and Count agree for a replaced file with
  different metadata, with OR/NOT; a restricted and an AppContainer process
  of the same account are filtered.
- **Squatting:** a pipe server running as A, created first and **able to
  impersonate**, never receives a write from B's client and never holds B's
  token.
- **Stalls and fairness:** a mocked blocked open; all pool threads stalled →
  `Busy`; A holding 4 idle connections and many broad queries does not starve
  B or an administrator.
- **Every query kind:** small/broad, prefix, OR/NOT, each sort, count exact and
  lower-bound, partial page, highlights, filtered Status.
- **Review 3 probes:** replace a privately indexed file with a differently
  sized one of the same name, grant B, sweep `size:`/`dm:` with OR/NOT;
  only fresh values may decide which rows are *returned* (which are
  *considered* is gap G1); a move between each component open and the child
  open (mocked) and a case-only rename (the row is hidden, or shown with its
  current path); traverse denied at each depth with bypass-traverse absent,
  on NTFS and ReFS; repeated expiry and disconnect of A with mocked blocked
  opens: fixed resource bounds, prompt teardown, no late output, B keeps
  working on a healthy volume; blocked scan workers while queries still run;
  restricted or low-integrity SYSTEM tokens and enabled-admin-but-not-full
  tokens are filtered.

## Performance

Unfiltered callers: unchanged; the paired gate stays strict. Filtered callers:
a CI arm on the installed service over a real tree reports first page and
count with cold per-request memos, split into scan, authorization and
serialisation; the open cost is measured, not assumed.

## Not decided here

Unix filtering, live updates (ADR 0005), any cross-request authorization
cache (only with a freshness mechanism a future review accepts), and the
fix for gap G1.
