# ADR 0002: per-user visibility in the system service (M2c)

Status: proposed (2026-11-07). Needs a separate security review before any
code merges (M2 acceptance item 3).

## Problem

The system service runs as SYSTEM (root on Unix) and indexes every name on
the machine. Until now it answers only SYSTEM and elevated Administrators
(the interim rule in `docs/service.md`), so the desktop app, which runs
non-elevated, keeps its own per-user index. That index is lost when the app
closes mid-scan, is never updated in the background, and duplicates the
service's work. The owner wants the app to search the always-running service.

The service may only show a caller the entries that caller could find through
the file system themselves.

## Rule

An entry is visible to a caller when **every directory on its path, from the
root entry down to its containing directory, grants that caller
`FILE_LIST_DIRECTORY`** (Unix: read and execute). A root entry itself is
visible when the caller may list it, or — for a bare drive or `/` — always
(the name of a volume root is not a secret).

This is stricter than the OS in one case: with "bypass traverse checking"
(granted to Everyone by default on Windows) a user can open a folder by its
full path even when a parent folder denies listing. We hide such entries: a
name the caller could only reach by already knowing it is not discoverable,
and the stricter rule never shows too much.

Fail closed everywhere: a directory whose security descriptor could not be
read, an access check that errors, a caller whose token or credentials are
missing, an index without visibility data — all mean "not visible". Elevated
Administrators and SYSTEM keep today's unfiltered view (they can read every
descriptor anyway).

## Data captured at scan time

The walker already visits every directory as SYSTEM. For each directory it
records a **security class**, a small integer:

- **Windows:** read the directory's security descriptor
  (`GetFileSecurityW`, `OWNER | GROUP | DACL`; no SACL, so no
  `SeSecurityPrivilege` needed), normalise it to self-relative form, and
  intern it in a table `descriptor bytes -> class id`. NTFS stores few
  distinct descriptors (inheritance makes most folders share one), so the
  table stays small (thousands, not millions).
- **Unix:** intern `(owner uid, group gid, mode bits)`; a directory with a
  POSIX ACL (`system.posix_acl_access` present) gets a class flagged
  "ACL": visible only to its owner and root until ACL evaluation is
  implemented (fail closed).

A directory whose descriptor cannot be read gets class `DENY` (0), visible
to no filtered caller.

Storage: a `u32` class per **directory**, kept in a dense array indexed by a
per-directory rank (`dir_rank[id]` comes from the existing DIR flag plus a
rank/select structure over the directory bitmap: about 0.2 bytes per entry),
plus the class table. At 5 M entries with ~10 % directories this is about
2 MB of class ids, well inside the per-entry memory budget (SPEC §3). The
index format gains an optional `visibility` section; an index without it
serves filtered callers nothing (fail closed), and the service rescans.

## Query-time check

1. **Caller key.** Windows: the impersonation token captured from the pipe
   (`CallerIdentity::access_token`). The cache key is the token's user SID,
   group SIDs with their attributes, restricted SIDs, integrity level and
   AppContainer SID, hashed (SHA-256) — two tokens with the same key get the
   same answer from `AccessCheck`. Unix: `(uid, gid, supplementary gids)`
   from `SO_PEERCRED`/`getpeereid` plus `getgrouplist` resolved by the
   service (never from the client).
2. **Class decisions.** For each class the caller meets, one decision:
   Windows `AccessCheck(descriptor, token, FILE_LIST_DIRECTORY,
   file generic mapping)`; Unix the owner/group/other mode-bit rule with
   supplementary groups. Decisions are cached per (caller key, index
   generation, class id).
3. **Directory visibility bitset.** Per (caller key, index generation) the
   service computes `visible_dir[rank] = allowed(class(dir)) &&
   visible_dir[rank(parent)]` in parent-before-child order (ids are assigned
   parent first, so one pass in id order suffices). Cost: one table lookup
   per directory, about 1–3 ms at 5 M entries on CI; computed lazily on the
   caller's first query, off the request thread's lock, bounded and
   cancellable like any query.
4. **Filtering.** The visibility bitset joins the matcher as a prefilter
   (the same skip mechanism as the metadata prefilter, `prefilter_bits` in
   `ferrow-query`): an entry passes when `visible_dir[rank(parent(entry))]`, and a
   directory entry additionally needs nothing more (seeing a folder's name
   needs only its parent listable). Counts, first pages, sorts and
   highlights all see the filtered set; there is no unfiltered path for a
   filtered caller.

## Cache lifetime and invalidation

- Keyed by index generation: a rescan or a future live update (M2d) bumps
  the generation, and old bitsets and decisions are dropped.
- At most 8 caller bitsets are kept (LRU); each is `dirs / 8` bytes
  (60 KB at 5 M entries).
- Group membership changes take effect at the caller's next logon (a new
  token gives a new key), as with the OS itself.
- With M2d, a USN security change (`USN_REASON_SECURITY_CHANGE`) re-reads
  that directory's descriptor, re-interns its class and bumps the
  generation.

## What the protocol shows

Unchanged messages. A filtered caller simply gets fewer hits and a smaller
count; `Status` reports entry totals only for unfiltered callers (a total
would leak the existence of hidden entries) and reports `visibility:
filtered` to others. Errors never include hidden paths.

## Endpoint access

The pipe ACL already admits Authenticated Users with read/write
(`transport.rs`, `PIPE_SDDL`). The interim `Unauthorized` gate in
`ferrowd::handle` is replaced by: unfiltered for SYSTEM/elevated
Administrators/root, filtered for every other authenticated caller,
refused for anonymous or unidentified callers.

## What the service indexes

With filtering in place the service can index what the app indexes today:
by default **every fixed drive**, counting a `subst` alias once (a drive
whose `QueryDosDeviceW` target starts with `\??\` is an alias of a folder on
another drive and is skipped). The default stays opt-in at install time
(the setup checkbox); the roots and excludes become a service setting that
only administrators can change.

## App behaviour

The app searches the service when it is reachable and answers; otherwise it
falls back to its own index exactly as today, and says which one it uses in
the status bar. Rebuilds of the service index run in the service, so closing
the app no longer loses them.

## Tests

- Unit: class interning, the parent-before-child bitset pass, cache keying
  and LRU, fail-closed paths (unreadable descriptor, missing token, index
  without the section).
- Windows CI (`windows-latest`, already elevated): create two local users;
  a folder tree with `icacls` grants (user A only, user B only, both, a
  denied parent with an allowed child); run queries through the installed
  service as each user (`CreateProcessWithLogonW` from the test script) and
  assert exact visible sets, including counts and an empty result for
  B-only names as A.
- Linux CI (`ubuntu-latest`): the same matrix with `useradd`, `chmod`,
  `setfacl` (the ACL case must be hidden from non-owners) and `sudo -u`.
- Never on the owner's PC: these tests create users and change ACLs.

## Performance

The first query of a new caller pays the bitset pass (~ms); later queries
pay one bit test per visited candidate (the prefilter skip). The CI bench
gains a "filtered caller" arm next to the IPC arm; the paired gate keeps
the unfiltered numbers strict.

## Not decided here

POSIX ACL evaluation (hidden until then), live security-change handling
(M2d), and showing a filtered user's own files from outside the configured
roots.
