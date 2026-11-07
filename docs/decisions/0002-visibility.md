# ADR 0002: per-user visibility in the system service (M2c)

Status: proposed, **revision 2** (2026-11-07). Revision 1 (scan-time
descriptors) was accepted only with 13 required changes
(`0002-visibility-review-1.md`); the owner chose per-search checks by
Windows itself. Needs a new security review before the ordinary-caller gate
opens. Live index updates (ADR 0005) are a separate project and not a
prerequisite.

## Problem

The system service runs as SYSTEM and indexes every name on the machine.
Today it answers only SYSTEM and elevated Administrators, so the desktop app
(non-elevated) keeps its own index: lost when the app closes mid-scan, never
updated in the background, duplicating the service's work. The app should
search the always-running service, and each caller must see only what they
could find through the file system themselves, **as of the moment of the
search**.

## Rule

A result row is shown to a filtered caller only if, during the request that
returns it, **Windows opened every directory on the row's path — from the
volume root down to the row's containing directory — for listing, acting as
the caller**. The row itself is not opened (seeing a name needs only its
folder listable). Nothing else grants visibility.

- Stricter than the OS for bypass-traverse users: a folder reachable only by
  already knowing its path is hidden (accepted as policy in review 1).
- The decision is Windows' own: open under the caller's impersonation token
  with `CreateFileW(path, FILE_LIST_DIRECTORY | SYNCHRONIZE,
  FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, OPEN_EXISTING,
  FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)`. The kernel
  evaluates the DACL with deny and inherited ACEs, owner rights, mandatory
  integrity labels, conditional ACEs, central access policies, restricted and
  AppContainer tokens, and traverse checks with the caller's own privileges.
  The service re-implements none of it (review 1 items 3 and 4).
- `FILE_FLAG_BACKUP_SEMANTICS` grants nothing by itself: backup privilege
  applies only if the *caller's* token has it enabled, and such callers are
  elevated administrators, who are unfiltered anyway.
- The opened handle is verified to be the indexed object: its file id
  (`GetFileInformationByHandleEx(FileIdInfo)`) must equal the id recorded at
  scan time (the service's walk records each directory's 64-bit NTFS or
  128-bit ReFS file id: directories only, about 1–2 bytes per entry), and it
  must be a directory without a reparse point (junctions,
  symlinks and mount points in a path fail the check). Otherwise: hidden
  (review 1 item 8).
- Any error — open failure, id mismatch, impersonation failure, a missing or
  identification-only token, a timeout — means hidden. Fail closed.

Unfiltered callers: SYSTEM and administrators whose token is elevated
(`TokenElevationType` full, Administrators group enabled). Everyone else is
filtered, including filtered admin tokens, restricted, low-integrity and
AppContainer tokens (no SID shortcut, review 1 item 3). Anonymous or
unidentified callers are refused.

## Freshness

Decisions are cached **only inside one request**. A permission change made
before a request starts is always honoured by it. A change during a request
may or may not be seen by that request (the linearisation point is each
directory's open); the next request sees it. There is no cross-request cache,
so a revocation never lingers (review 1 item 1).

## Impersonation level and server verification

Checking by opening needs a token at `SecurityImpersonation` level; today the
client connects with `SECURITY_IDENTIFICATION` (`transport.rs`). Change for
the **system endpoint only**:

1. The client opens the pipe with `SECURITY_SQOS_PRESENT |
   SECURITY_IMPERSONATION` (static tracking, no effective-only change).
2. Before writing a single byte it verifies the server: the pipe's server
   process (`GetNamedPipeServerProcessId`) must run as LocalSystem
   (`S-1-5-18`) from the installed service image
   (`%ProgramFiles%\Ferrow\service\ferrowd.exe`, compared by file id).
   Otherwise it disconnects without writing. A pipe squatter can impersonate
   only after reading from the client, so it never gets the token.
3. The service duplicates the captured token at impersonation level, uses it
   only on the request's worker thread between `SetThreadToken` and
   `RevertToSelf` (guarded so every exit path reverts), and closes it at the
   end of the request. Development endpoints keep identification level.

## Where the filter applies

Visibility is mandatory authorization, not an optimisation (review 1 item 9):

- The service wraps every `Query` and `Count` for a filtered caller in a
  visibility context; the query engine evaluates `visible(row) && query(row)`
  on every path — ordered search, count, every sort, OR/NOT, prefix
  candidates — and applies it before limits, pagination, `more`, byte budgets
  and highlights. There is no code path that serves a filtered caller without
  the context; a missing context is an error, never unfiltered.
- Per request, a directory decision is memoised (`dir id → visible`), with
  ancestors resolved first, so a path's directories are opened once.
- Root entries: a bare volume root (`C:\`) is opened like any directory. A
  configured root below a volume root (for example `E:\Data`) has its
  ancestors (`E:\`) opened too, by path, before anything under it is shown
  (review 1 item 7). A file root is visible if its containing directory chain
  is.
- Hard links: each returned name is checked through its own parent chain.
- Alternate data streams are not indexed; 8.3 and case variants are not
  separate rows, and the check uses the stored canonical path.

## Bounded work, counts and partial pages

Opening a directory costs tens of microseconds. Per request:

- **First page:** the scan continues until the page holds `limit` visible
  rows. A budget (default 250 ms and 20 000 directory opens) bounds it; when
  it runs out, the page returns what it has, flagged `partial`.
- **Count:** for a filtered caller the count is exact when it finishes within
  the budget (default 500 ms); otherwise the reply is a lower bound flagged
  `at_least` (the protocol gains this flag; old clients get an error rather
  than a wrong number).
- Directory opens run on a bounded per-request pool, separate from the query
  pool; cancellation stops them. Per-user limits: at most 2 concurrent
  filtered requests per user SID and 8 overall; further requests queue
  fairly (round-robin by SID) with a short wait, then get `Busy`
  (review 1 item 12).

## Timing

Hidden rows still cost scan time, so response time can depend on how many
hidden names match. Mitigations: the work budget caps the worst case; elapsed
times are not returned to filtered callers. The residual timing channel —
learning roughly how many hidden names match a pattern, not the names — is
documented as accepted (review 1 item 11, P2).

## Status and errors

`Status` for a filtered caller is built from an allowlist: protocol version,
`visibility: filtered`, service state (ready / scanning). No entry totals,
no report strings, no paths, no error samples (review 1 item 10). Errors to
filtered callers never include paths. The protocol version is bumped; the
filtered status is a new variant.

## What the service indexes

By default every fixed drive, counting a `subst` alias once (an alias whose
`QueryDosDeviceW` target starts with `\??\` is skipped; deduplication is
not a security boundary). Roots and excludes become a service setting only
administrators can change; the setup checkbox stays.

## Unix

Linux and macOS keep the current rule (root only) until a revision covers
them: `getgrouplist` is not the caller's effective group set, and macOS ACLs
and sandbox/TCC cannot be checked from mode bits (review 1 items 5 and 6).
The service on Unix answers non-root callers with `Unauthorized`, and the app
there keeps its own index.

## App behaviour

The app searches the service when it is reachable and answers; otherwise it
falls back to its own index, and the status bar says which. Partial pages and
lower-bound counts are shown as such ("at least 12 400").

## Tests (merge gates, disposable CI only)

Windows (`windows-latest`, elevated): create two local users and run the
installed service.

- Static matrix: A-only, B-only, both, denied parent with allowed child,
  inherit-only and deny ACEs, owner-rights ACE, mandatory label `High` on a
  folder (low/medium caller), a junction and a symlink in a path, a hard link
  in an A-only and a B-only folder, a configured root whose ancestor denies B.
- Revocation: B searches, A removes B's access, B's next search no longer
  shows the rows; restored access shows them again.
- Tokens: a restricted token and a low-integrity process for A see less than
  A; a non-elevated admin is filtered; an elevated admin is not.
- Squatting: a test pipe server running as A, created before the service,
  never receives a write from B's client.
- Every query kind: small and broad queries, prefix, OR/NOT, each sort,
  count (exact and lower-bound), partial page, highlights, Status contents.
- Fairness: user A's broad queries do not starve user B (latency bound).
- Native comparison: results equal a native `Get-ChildItem -Recurse` run as
  the test user over the test tree.

Never on the owner's PC: these tests create users and change ACLs.

## Performance

Unfiltered callers: unchanged (the paired gate stays strict). Filtered
callers: the CI bench gains a filtered arm on the installed service with a
real directory tree, reporting first page and count with cold per-request
caches; targets are the SPEC §3 first-page numbers plus the measured open
cost, and the budgets above.

## Not decided here

Unix filtering, live updates (ADR 0005), cross-request caching (only with a
freshness mechanism that a future review accepts).
