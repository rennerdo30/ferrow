# The Ferrow background service (`ferrowd`)

`ferrowd` holds the index in memory and answers searches over a
local, access-controlled IPC channel (crate `ferrow-ipc`). The CLI
(and later the GUI) stay unprivileged; only the daemon needs the
rights to read every directory (and, in later milestones, the
Windows MFT/USN journal).

## Indexing on Windows: MFT and live updates

- **What it indexes.** Without roots on its command line, the system
  service indexes every local fixed drive (drive letters that are only
  `subst` aliases are skipped). `ferrow service install <folders…>`
  limits it to the given roots.
- **Fast scan.** A root that is a whole NTFS volume is read from the
  volume's master file table in one sequential, unbuffered pass: names,
  folders, sizes, dates and attributes for millions of files in seconds
  on an SSD. Folder roots, other file systems and processes without raw
  volume access fall back to the parallel directory walk.
- **Live updates.** After the scan, one thread per NTFS volume sleeps
  inside the change journal read until the volume changes (no polling;
  0 % CPU while nothing changes). Changes are applied in 200 ms batches
  and searches see them within a second. The frozen index is never
  rebuilt for a change: replaced entries are hidden, new ones live in a
  small second index (the delta), and the service merges them into a new
  frozen index in memory once that grows large (a rescan of the drives only
  when the journal is reset or a change cannot be applied; see below).
  - **The limit follows the index:** a compaction is asked for when more than
    1 % of the index's entries are live delta entries, at least 50,000 and
    at most 250,000 (1 million entries: 50,000; 18.6 million: 186,000;
    27 million and more: 250,000), or when dead delta entries (replaced or
    deleted again, kept until the next compaction) outnumber twice that limit,
    or when more than a tenth of the frozen entries are hidden; it starts at
    once when the delta with its dead entries grows past 3 times the
    limit (at least 400,000, at most 750,000), without waiting for its rate
    limit. A file that is
    written again and again keeps one delta entry, so a busy log or
    database file is no reason to compact.
  - **Compaction instead of a rescan (FR-42, part 2).** For the three size
    reasons (`too many live changes`, `too many replaced entries`,
    `too many superseded entries`) the maintainer thread runs
    `Live::compact` on the low-priority rescan pool: a new frozen index from
    the frozen entries still shown (folder dates folded in) plus the live
    entries, and a new live layer over it with the same volumes, roots,
    excluded records and hard links. It reads no disk and no file table. A
    search never waits: it uses the snapshot published before, and one
    `SnapshotStore::swap` publishes the new index with no delta. The old
    index and live layer are freed when the last search that holds the old
    snapshot ends. Journal changes that arrive meanwhile wait in the
    maintainer's channel (the single writer; no replay log) and are applied
    right after, so a compaction delays only how soon a change shows, never
    a search. It starts only when no folder read is outstanding (a read's
    result is merged by entry id) and no index save is running (the saver
    holds the old index; it is looked at again every 2 s), at least 60 s
    after the last scan or compaction and 5 times the last compaction's
    duration (at most 10 min) after the last compaction; past the hard delta
    limit it does not wait for its gap. The hard limit stays a cap: a delta
    past it that has waited 40 s (the longest a folder read lasts) for reads
    or a save, or past twice the limit, is not held back by a save, and if
    folder reads still keep arriving the volumes are rescanned instead, as
    before compactions existed. What answers a reason is carried with the
    reason (not read from its text). A pending real rescan covers a pending
    compaction, with one exception below.

    A large folder move (a renamed or moved folder with more than 100,000
    frozen entries below it, folders and dead entries included) is answered
    by a compaction too. The folder's new entry adopts its frozen children
    instead of copying them (one bit per entry and a pass over the subtree;
    the first move after a scan or compaction also builds the children
    table, 8 bytes per entry). The moved contents are hidden from searches
    until the compaction gives them their new path; they are never shown
    under the old one. That compaction waits only for its duty share (5
    times the last one's duration), not for the 60 s gap, and runs even
    while a rescan is waiting (the rescan still follows). Once its duty
    share allows it to start, it waits for an index save at most 40 s, and
    for folder reads until they end (each ends within its 40 s limit;
    hidden entries hold no extra memory, so they never cause a rescan). A
    folder
    moved below a folder that was inside it waits in its batch until that
    one has moved out; if that move is not reported yet, the volumes are
    rescanned (`folders swapped nesting`), because placing it would hang it
    below itself.

    These still read the drives again: a journal reset or overrun, a
    moved-in folder that could not be read, a request by an administrator,
    and the start of the service.

    A compaction's memory: it builds the new index straight in its file
    (see "Rescans and merges are built in the file" below), so the builder
    and a second copy of every column are not in the heap: what a
    compaction adds in private memory is a fixed working set (about 55 MB
    of spilling shards and 5 MB while the file is placed, measured at 2
    million entries) plus the new live tables (about 22 bytes per entry,
    until ADR 0007 decision 6 moves them into the file). The in-memory
    build it replaced added about 85 bytes per entry (a rescan added about
    161). The new index starts without its cached metadata sort orders (size,
    dates); right after the swap the ones that were ready on the old index
    are prepared again in the background, one thread each at the lowest
    priority (about 0.13 s at 1 million entries), and a sorted search
    answers in name order until then. The merged index replaces the file
    and is read from it (see "The index is read from its file" below).
    Health keeps
    it apart from the scans: `live_stats` has `compactions`, `compaction_ms`
    and `last_compaction` (when, how long; a later scan clears it), and it is
    not in `scans_by_reason`, and never shows "scanning". The log carries
    "memory before compaction" and "memory after compaction" tables like the
    rescan's, and a line with the duration.
  - **The delta's view is built cheaply:** after a batch the delta is
    published as a small searchable index on the low-priority background
    pool the rescans use (never on the cores searches use), at most about
    9 % of the time: the next one is published 10 times the last build's
    duration after the last, at most 5 s later, and the changes in between
    are merged into it. A search never waits for a build; it reads the view
    published before. A first page of results in name order asks the delta
    only for as many hits as the page holds.
- **Folders moved into the index.** A folder that arrives from outside
  the indexed places (git and git-lfs move temporary folders into place
  all the time) gets its entry at once, and one low-priority thread lists
  its contents by file reference (up to the delta's room, 1 % of the index,
  50,000 to 250,000 entries, and 30 s). Changes the journal reports while it is listed are read
  again after the result is merged, so none is lost or doubled. Only a
  folder too large or too slow to list, or a failed read, asks for the
  full rescan (the size of the live layer alone no longer does: it is
  compacted). The health report counts scans by reason and the
  folder reads (`live_stats`); each fallback and each read of a second or
  more is an event in the service log.
- **Start.** The service answers from its saved index at once (or an
  empty one on first start) while the first scan runs.

## Modes

- **The app.** When the system service answers this account (an
  administrator, or an account the access policy allows; see the access rule
  below), the Ferrow app searches through
  it, loads further rows while scrolling and refreshes its results on
  live changes. Otherwise it keeps its own index.
- **Service mode (default when reachable).** `ferrow search`
  connects to the daemon and streams results from it.
- **In-process fallback.** When no daemon answers — or when
  `--index <file>` pins an index file — the CLI loads the saved
  index itself, exactly as in milestone 1.
- `ferrow status` prints which mode is active, the endpoint, the
  entry count and the backend report.

Endpoints: Windows named pipe `\\.\pipe\ferrowd` (remote clients
rejected; LocalSystem and administrators full control,
authenticated users connect/read/write), Linux
`/run/ferrow/ferrowd.sock`, macOS `/private/var/run/ferrow/ferrowd.sock`.
Caller identity always comes from the OS (Windows effective pipe
token after reads, Linux `SO_PEERCRED`, macOS `getpeereid`), never from the
client's own claim.

**Interim access rule, replaced by M2c:** the system service answers
SYSTEM and **any process of an account in the local Administrators
group, elevated or not** (owner decision 2026-11-10), so the normal,
non-elevated app of an admin account can use the service's fast index.
Restricted, AppContainer and below-medium-integrity processes (sandboxes)
are refused even for admin accounts. On Unix only root (uid 0) is
answered. Everyone else, including standard (non-admin) accounts,
receives `Error{Unauthorized}` without paths, counts or status data; the
CLI and the app explain the refusal and fall back to the per-user saved
index. An admin account could elevate and read every file name anyway,
so this exposes nothing new to it. Development mode serves only the OS
user who started it. This is the default (mode `admins`); an administrator
can widen it with the access policy below. Per-user result filtering for
standard accounts is M2c (ADR 0002).

### Access policy: who the system service answers

An administrator can widen the rule above without waiting for M2c, at the
cost of showing file names unfiltered to the chosen accounts. The policy is
the file `access.json` next to the service index (`%ProgramData%\Ferrow\` on
Windows, `/var/lib/ferrow/` on Linux, `/private/var/lib/ferrow/` on macOS):

```json
{"version":1,"mode":"listed","users":[{"id":"S-1-5-21-…-1001","name":"PC\\alice"}]}
```

- `mode` is `admins` (the default: SYSTEM, root and administrators, exactly
  the rule above), `everyone`, or `listed` (administrators plus the accounts
  in `users`). `id` is the Windows SID or Unix uid and is what authorizes;
  `name` is display only. SYSTEM and root are always answered.
  - **`everyone` means every person who signs in to this PC**, not every
    account that exists. Windows: the caller's token holds the INTERACTIVE
    group (S-1-5-4: console and remote-desktop sign-ins) as an enabled group
    and is not a Guest (not in Guests S-1-5-32-546, not relative id 501), and
    it is not sandboxed (restricted token, AppContainer, below medium
    integrity). Service, virtual and network-logon tokens have no INTERACTIVE
    group and are refused. Unix: a regular uid, at or above the platform's
    first one (Linux `UID_MIN` from `/etc/login.defs`, read once when the
    service starts, default 1000; macOS 501), and not `nobody` (65534, or -2
    on macOS).
  - **`listed` admits exactly the listed ids** (an administrator chose them,
    so a listed service account works) but still refuses sandboxed callers.
- A missing file means `admins`. The file is validated like `index.bin`
  (protected folder and file, administrator-only grants); an unprotected,
  unreadable, malformed (unknown version, mode or field, a non-canonical id,
  more than 256 users) or larger than 64 KiB file is ignored, logged once,
  and means `admins`. A bad file never widens access. One leading UTF-8 byte
  order mark is accepted. An id is a canonical SID (`S-1-` and plain decimal
  parts, at least two) or a canonical decimal uid.
- The service checks the file at most once a second. Whichever request
  thread wins a `try_lock` does it; every other request uses the current
  policy at once, so a slow disk or a stalled validation delays one request
  at most and never the rest (no extra thread, no per-request file read).
  Each check re-validates the file's protection (an ACL or owner change
  applies even when the content did not change), compares its size, time and
  identity, and reloads on change. A failed check is retried on the next
  tick and remembers nothing (a transient error does not stick), and the
  policy swaps in atomically. A change applies within about a second and a
  removed account is refused on its next request. The development endpoint
  (`ferrow service run`) ignores the policy and serves only its owner.
- **`AccessInfo` request** (every authenticated caller of the system endpoint,
  refused ones too): the mode, whether the caller is answered, and the
  caller's own id (the display name only when the policy lists it; the service
  never looks up accounts on a request thread, because a domain lookup can
  wait). The user list goes only to an allowed administrator. It is a new
  message kind and the protocol version stays 4: an older service does not
  know it and never answers it (it only drops the connection later), so
  clients ask on a connection of their own with a short deadline and treat
  silence as "the service is older, or busy".
- **Command line:** `ferrow service access show` (any account, asks the
  service), and `admins`, `everyone`, `add <account-or-SID>`,
  `remove <account-or-SID>`, which need an elevated terminal / sudo and say so
  otherwise. The file is replaced atomically: the new policy must parse under
  the same limits the service applies (so a file the service would refuse is
  never written), goes into a temporary file in the same protected folder (so
  it inherits only the protected grants; on Windows its owner is set to
  Administrators), is validated like the service validates it, and is renamed
  and made durable (folder sync on Unix, buffer flush on Windows). The command
  reads the file's stamp first and refuses with "the access list changed
  meanwhile, try again" if the file changed before the rename, so two
  administrators cannot silently overwrite each other (the window between that
  check and the rename is a few microseconds). `add` lists the user (and switches `admins` to `listed`),
  `everyone` and `admins` clear the list, and removing the last listed user
  returns to `admins`. Names resolve with `LookupAccountNameW` /
  `getpwnam_r` in the short-lived administrator process; groups are refused,
  and control characters in names are escaped when printed.
- **The app** (Settings → **Fast index → Who can use the service**; the
  **Fast index** card at the top says whether the live index is on and, if
  not, why) shows in one line whether this
  account uses the service, and runs the same commands through the installed
  `ferrow.exe` with a UAC prompt (Windows; elsewhere it shows the `sudo`
  commands to copy). When a standard user approves it with an
  administrator's password the command runs as that administrator, so
  **Allow me** passes the standard user's SID explicitly (taken from
  `AccessInfo`), never "the current user". The status query uses a connection
  of its own with an 800 ms deadline, so a service that predates the request
  (or is busy) shows "did not answer: older, or busy" with a Retry button
  without delaying the window. After the elevated command exits, the app asks
  the service again (at 0, 1.2 and 2.4 seconds, off the UI thread) until the
  answer shows the change; that answer, not the exit code, decides whether it
  worked, and it applies even when Settings is not open. Elevated launches
  only start `ferrow.exe` from the Setup-installed Program Files folder (found
  with the shell's known-folder API, with that folder as the working
  directory), never from a portable or development copy.
- **Add account…** (same card) adds another account by name. The name
  (`DOMAIN\user`, `user` or a SID) is looked up in the app's own, unelevated
  process with `LookupAccountNameW` (`LookupAccountSidW` for the display
  name) on a thread of its own with a 5 s deadline, since a domain lookup can
  stall; at most one such thread exists, and a new lookup while a stuck one
  still runs says so. Unknown names and anything that is not a user account
  (groups, well-known SIDs such as Everyone or SYSTEM) get an error. Only the
  looked-up canonical SID is passed to the elevated `ferrow service access add`
  command line, never the typed text, so nothing typed reaches the `runas`
  parameters; a plain number is not accepted as an id on Windows. Without
  elevation the command is shown to copy (Linux and macOS show
  `sudo ferrow service access add <name>`, with the name limited to letters,
  digits and `. _ - @`; nothing is looked up there). A standard user who
  approves the prompt with administrator credentials cannot read the account
  list, so the answer cannot show another account's addition or removal: the
  app then goes by how the command ended and says it cannot confirm
  (a declined prompt is never reported as done).
- Uninstall keeps `access.json` with the rest of the state (as it keeps
  `index.bin`), so a reinstall picks the choice up again. Delete the folder to
  reset it, or run `ferrow service access admins`.

## Health, event log and repair

- **`Health` request** (`ferrow service health [--json]`): what the service
  knows about itself, built only when asked from state it already keeps:
  version, pid, start time and uptime, memory (Windows: working set and
  private bytes; Unix: resident set), index entries and mode, the access mode,
  live changes applied, connection errors, the last save (time, result,
  duration) and per volume its root, mode (`mft+usn live`, `walk` or
  `live unavailable` with the reason), entries, journal state (`ok`, `reset`,
  `reader error` with the text), last scan (time, why, duration) and a pending
  rescan (why, earliest time); and, once for the service, the last merge of
  live changes into the index in memory (time, why, duration; not a scan),
  then the last 50 events. It is answered to the
  same callers as searches; a refused caller gets `Unauthorized`. The human
  output starts with `OK` or `ATTENTION` and the list of problems. `--json` is
  the report on one line (`ferrow_ipc::health::Health`; readers ignore unknown
  fields). Like `AccessInfo` it is a new message kind at protocol version 4: an
  older service never answers, so the CLI and the app ask on a connection of
  their own with a two-second deadline and say "older, or busy".
- **Nothing is polled.** Every figure is written when its event happens (a scan
  ended, a save ended, a batch was applied, a journal reset); the idle service
  still has no timer, and a search does no health work.
- **`Rescan` request** (`ferrow service rescan`): administrators, SYSTEM and
  root only, even when the access policy lets others search. It asks the live
  maintainer for a full rescan of every drive now, once past the usual rate
  limit, but not within a minute of the last scan's end and not while one runs
  or is already requested; the answer says why and when. Where no live
  maintainer runs (Linux and macOS scan at start; Windows without raw volume
  access) it says so; restart the service to rescan.
- **Event log**: the last 200 events are kept in memory (they feed `Health`),
  and appended to `service.log` in the protected state folder
  (`%ProgramData%\Ferrow\`, `/var/lib/ferrow/`, `/private/var/lib/ferrow/`
  on macOS); at 1 MiB it moves to `service.1.log` (the previous one is
  replaced, so at most about 2 MiB). The file is created like the index
  (protected folder, no links followed, re-validated), so only administrators
  read it. Events are state changes, never per request: a repeat only raises a
  count, the file takes at most 30 lines a minute (one line then says how many
  were not written), connection errors are counted in `Health` instead of
  logged, and a failure to write the log never affects the service. Event
  texts carry no file names beyond volume roots and the service's own paths.
- **Control** (`ferrow service start | stop | restart`, elevated / sudo): the
  Windows Service Control Manager, `systemctl`, or `launchctl`. Waits are
  bounded (30 s; a service that does not stop in time is reported, never
  killed), and every manager command runs with a timeout and its PID is printed.
- **`ferrow service repair`** (elevated / sudo) first runs read-only checks
  (registration, state folder, saved index, and whether the service answers a
  `Health` request, or `Status` for an installed service older than `Health`).
  When all pass it prints `Nothing to repair` (with any note about a policy
  file that fails its check) and leaves the service running (exit 0). A service
  that reports no problems and a successful save needs no index check at all;
  otherwise the index is checked by streaming, never loaded: the header, the
  length and the whole-file checksum are read in pieces of 1 MiB, so the check
  needs 1 MiB of memory whatever the index's size (it does not validate the
  offsets inside the sections; the service does that when it loads). Otherwise it runs, in order, and prints one line per step:
  (1) the installed binary exists at its fixed path and passes the binary
  protection check (it never copies a binary, and never takes a path from the
  user; if it is missing, reinstall with Setup, or `sudo ferrow service install`
  on Linux and macOS), and the registration (Windows service entry, systemd
  unit or launchd plist) runs exactly that binary, and a Linux unit is also
  enabled (`systemctl is-enabled`; a disabled unit is enabled, a masked one is
  reported and left for `sudo systemctl unmask ferrowd`). A service whose binary is
  still installed is re-registered from that path when its registration is
  missing or points elsewhere (a Windows registration keeps its `--root` list;
  a unit or plist is only trusted when it is a root-owned, single-link regular
  file with the exact text, otherwise it is rewritten or refused); (2) stop
  the service; (3) the state folder passes its protection check (a missing one
  is created; one that fails cannot be repaired automatically and the repair
  stops with the reason) and a log file that fails its check is moved aside;
  (4) the saved index passes the protection check and the streaming check
  above. Only an index
  whose content is damaged (wrong magic, truncated, checksum mismatch, corrupt)
  is renamed to `index.bin.bad-<UTC time>` (a new name is made first, so an
  existing file is never overwritten, and nothing is deleted) and the service
  rebuilds it at start. A read error, an index version this build does not read
  or a protection failure is reported and the repair stops without moving the
  file; (5) start the service; (6) wait up to 30 s until it answers. The first
  failing step ends the repair and says what is left; after the stop it also
  says that the service is stopped. On macOS the plist sets `ExitTimeOut` to
  60 s so the final index save can finish (launchd's default is 20 s); `repair`
  rewrites an older plist.
- **Exit codes** of the `service` verbs (the app relies on them): `0` done,
  `1` failed, `2` needs administrator/root (nothing changed), `3` no service
  reachable or no answer within 2 s, `4` the service refused this account,
  `5` `rescan` did not start (reason on stdout).

## Install (needs admin/root once)

Installation copies `ferrowd` into its own administrator-only folder `%ProgramFiles%\Ferrow\service\` on Windows
or `/usr/local/libexec/ferrow/` on Unix and registers only that fixed path.
Unix directories and binaries must be root-owned 0755, with no group/other
write access or symbolic-link component. Existing unsafe paths are refused.
On Linux and macOS the install directory, binary, state directory, index and
socket directory must have no extended ACL entries (including inherited and
default ACLs). Ancestor ACLs may grant reads or root-only changes, but must grant
no non-root write, delete or permission-changing rights. ACL query failures
refuse use. Checks inspect opened, no-follow descriptors together with ownership
and mode, so chmod alone cannot make a hostile ACL acceptable.

Windows components must be owned by SYSTEM, Administrators or TrustedInstaller
and contain no reparse points. Non-admin ACEs must grant no WRITE_DAC,
WRITE_OWNER or DELETE on ancestors, and no FILE_DELETE_CHILD on their parents.
Create-subdirectory permission on a protected ancestor such as `C:\` is allowed.
The final directory additionally refuses non-admin add-file, add-subdirectory,
delete-child and other write rights, including inherited grants to new files.
The binary refuses non-admin write, append, delete, WRITE_DAC and WRITE_OWNER.
Constructed security descriptors test these rules, including create-only roots.
No user-supplied executable path is substituted into systemd or launchd files.
Unix unit and plist files are atomically published through protected root-owned
parents with an explicit, verified 0644 mode. Exclusive no-follow temporary
creation, ACL validation and final owner/mode/link checks apply independently of
umask. Unsafe existing definitions are refused before any manager action.

Ferrow never elevates itself: run the command from an elevated
terminal / with sudo, as it tells you.

- Windows (elevated terminal): `ferrow service install`, then
  `sc start ferrowd`. Registered as LocalSystem, automatic start.
  Remove with `ferrow service uninstall`.
- Linux: `sudo ferrow service install` writes
  `/etc/systemd/system/ferrowd.service` (template:
  [`packaging/systemd/ferrowd.service`](../packaging/systemd/ferrowd.service);
  only `CAP_DAC_READ_SEARCH`/`CAP_SYS_ADMIN`, state in
  `/var/lib/ferrow`, mode 0700) and enables it. Start with
  `sudo systemctl start ferrowd`. Remove with
  `sudo ferrow service uninstall`.
- macOS: `sudo ferrow service install` writes
  `/Library/LaunchDaemons/com.github.rennerdo30.ferrowd.plist`
  (template in
  [`packaging/launchd/`](../packaging/launchd/com.github.rennerdo30.ferrowd.plist)).
  Load it with `sudo launchctl bootstrap system <plist>`, and
  grant `ferrowd` **Full Disk Access** under System Settings →
  Privacy & Security → Full Disk Access, otherwise protected
  folders (including most of each home directory) stay
  unindexed. Remove with `sudo ferrow service uninstall`.

The system service validates its state before every load and save:
`%ProgramData%\Ferrow\index.bin` (Windows) or
`/var/lib/ferrow/index.bin` (Unix; macOS uses its real `/private/var` ancestor).
The Windows directory is SYSTEM-owned with inheritance disabled and only
SYSTEM/Administrators full control; reparse points and hostile existing state
are refused. New index files inherit only those protected grants. Unix state
is root-owned 0700 and the index is root-owned 0600. Build an index with
`ferrow index <roots> --out <that path>` from an elevated shell,
or pass roots in the service command line. Keep that directory
service-only: the index lists every file name on the machine.

**The index is read from its file (ADR 0007).**

- **When it is saved and mapped:** after every scan and every in-memory merge, the new index is in `index.bin` and mapped read-only. The live maintainer builds it in that file in the first place (next section); only when that fails does it build in memory, save, map the file back and drop the copy.
- **Where the index lives:** its columns are read through the map. They sit in the operating system's file cache, shared and reclaimable, and are not part of the service's private memory. Health reports the two apart:
  - `private_working_set`: what Task Manager's Memory column shows.
  - `index_mapped_bytes`: the mapped file.
- **Format:** version 4, which is version 3 with every column aligned in the file. A save writes the file in 1 MiB pieces through a temporary file and renames it over `index.bin`.
- **Replacing a mapped file:** on Windows the map's handle allows delete sharing, so the rename succeeds while the old map still serves the old contents. Old pages go when the last search that holds the old snapshot ends.
- **Version 3 files:** they are still read, into the heap, once.
- **If saving or mapping fails:** the index stays in memory (correct, only larger), with a warning in the log.
- **No duplicate saves:** the background saver skips an index that is already mapped. One lock keeps it and the maintainer from writing the file at the same time.

**Rescans and merges are built in the file (ADR 0007, step 8).**

- **What it replaces:** a rescan or merge built its index in the heap (the builder's shards, about 50 to 64 bytes per entry, then every final column, both name arenas and the name order, about 110 more) and then saved it. At 27 million entries that was several GB on top of the running index, far over the 1 GB limit.
- **While entries are added:** the builder's shards hold at most a fixed share of 32 MiB; a full shard is written to an anonymous temporary file beside `index.bin` (deleted when closed, so a failed or killed build leaves nothing) and its memory is reused. The heap holds about 1.6 times that budget at most (measured), whatever the number of entries.
- **The freeze:** the new file is sized exactly and mapped writable. The shards' columns, names and metadata are placed in it (a spilled run is read back twice: 12 bytes per entry for the columns, then the names), the names are folded, the name order is sorted in place and the checksum is computed, all inside the map. The pages are file cache, not private memory: freezing 2 million entries grew the process by 4.9 MB (measured), and the figure stays fixed as the index grows because it depends on the shard budget, not on the entry count. The file is flushed, renamed over `index.bin` under the save lock and mapped read-only; its bytes are exactly what a save of the same index writes.
- **Failure:** a temporary file that cannot be created or written (a full disk) leaves `index.bin` as it was and no temporary file behind. An index of up to a million entries is then built in memory instead and saved and mapped as before, with a warning in the log; a larger one is not built (that would take gigabytes): a rescan or merge first checks for room (2.5 times the current `index.bin`), and after a failure no merge (urgent or not) and no rescan starts for a gap that doubles from 15 minutes up to 12 hours (health: `merge_blocked`). The live changes stay in memory meanwhile and grow with them. A shard that cannot be spilled stays in memory (correct, only larger).
- **Disk:** during a rescan or merge the disk holds the old index, the new file being built (about the index size) and the spilled shards (about 60 bytes per entry with names averaging 42 bytes, estimated; gone when the build ends).
- **Time:** the freeze, the save and the map-back became one step. In the synthetic benchmark the whole rescan build (emit, freeze, save, map) took the same time at 1 million entries and about 15 % longer at 2 million. Writing the spilled shards cost about 25 ms for 67 MB; part of the rest is the order the benchmark runs in (a plain in-heap builder run third was also about 100 ms slower than the same one run second). At 27 million entries it should be neutral to a little slower (estimated, not measured).
- **Not part of this step:** the scan's own tables (the record map, 16 bytes per entry, and the parsed volume) and the live layer's new tables stayed in the heap; the next section moves them. A merge's own new tables (`Live::compact`) still do.

**A full scan reads the drives into files, not memory (ADR 0007, step 9).**

- **What it replaces:** the MFT read of a full scan (first start, journal reset or wrap, **Rescan now**) held everything it parsed in the heap until the index was built: the per-record table (48 bytes for every MFT record, free ones included), the names (24 bytes per name plus the name text), the grouping by parent (4 to 8 bytes) and then the record map (16 bytes per entry). The CI memory benchmark measured 102 to 106 bytes per entry for the parse alone and about 180 bytes per entry for the whole scan, about 4 to 5 GB at 27 million entries. Windows also holds the live layer's two new tables (8 + 4 bytes per entry) and the children table for the saved tables (8) until the saved copy replaces them.
- **What it does now:** every one of those tables is an anonymous temporary file beside `index.bin` (the same kind of file the builder's shards spill to: deleted when closed, so a failed, killed or panicked scan leaves nothing) written through a memory map or appended in chunks as the read arrives:
  - the per-record table, a map of a zero-filled file the size of the MFT;
  - the names and the name text, appended chunk by chunk (their offsets are shifted as they are written);
  - the grouping by parent, built by counting in the files;
  - the record map, appended during emission and mapped read-only when the volume is done;
  - the live layer's frozen tables and the children table, built in files beside the index and replaced by the saved copy right after.

  The pages are the operating system's file cache and not the service's private memory. The heap keeps the read buffers (up to four 16 MiB reads in flight), one parsed chunk, a 256 KiB write buffer per appended file and the builder's shards (32 MiB budget).
- **Same result:** the same code reads the tables from the heap or from files, so the entries, their order, the record map and the saved index are the same bytes. Tests build the same synthetic volume (hard links, names in extension records, forgotten records, patches) both ways with spilling forced and compare the tables, the children of every folder, the saved index file byte for byte and the record map.
- **Measured** (synthetic 1 million entry MFT, the `memory_breakdown` benchmark, release build, this PC, two runs): the service's scan path grew the process by 106 bytes per entry while parsing and 74 while emitting (about 182 B/entry at its peak, 182 MB); it now grows it by 8.2 to 8.4 MB while parsing, nothing while the volume is joined, 57.6 to 57.9 MB while emitting (the builder's budget), 5.0 MB while the index is frozen into its file and 1.8 MB while the live tables are built: about 58 MB at 1 million entries, and the figure is a fixed amount, not per entry. At 27 million entries that is about 60 MB plus the read buffers (about 64 MB, not in the synthetic stream), against about 4.9 GB (extrapolated from 182 B/entry). Not measured at 27 million entries.
- **Time** (same benchmark, 1 million entries, two runs each, run to run noise about 30 ms): the parse of the stream took 224 to 229 ms in the heap and 261 to 274 ms with files; joining 13 to 17 ms either way; the emission 243 to 272 ms in the heap and 296 to 322 ms with files. About 50 to 60 ms more per million entries (about 10 %). In a real scan the parse runs beside the disk read (about 900 MB/s), so most of it is hidden; at 27 million entries that is about 1 to 1.5 s more on a scan of about 53 s (estimated, not measured).
- **Forgotten records:** a journal refresh after a long read forgets the records that changed meanwhile, and the grouping asks for every name of the volume (twice) whether it was forgotten. That check is a bitmap over the record numbers (3 MB at 24 million records, built only when something was forgotten) in front of a binary search over the forgotten records; a hash set was used before. Measured: `finish` of the 1 million entry volume took 12.5 ms with nothing forgotten, 14.3 ms with 20 000 records forgotten and 17.0 ms with 100 000 (`memory_breakdown`, round `forgotten`); per name, 1.8 ns against 7.5 ns for the hash set with 20 000 records forgotten and random record order (the ignored test `forgotten_lookup_cost`).
- **Disk:** during emission the temporary files hold about 120 to 160 bytes per MFT record of the volume being read (per-record table 48, names 24 and their text, about 20 in the synthetic stream and 42 on average on the owner's PC, grouping 8, record map 16; the volume's files go when its emission is done, its record map stays) plus the builder's spilled shards (about 60 bytes per entry, estimated), beside the old and the new index file (about 70 bytes per entry). About 3.8 GB per volume at 23.5 million records, a peak of about 5 GB at 27 million entries (estimated).
- **No room, no silent memory:** before a volume's MFT is read the scan estimates what it will write (160 bytes per record for the read's files and 130 for the shards and the new index: `ntfs::files_estimate`, an estimate) and compares it with the free space beside the index (`GetDiskFreeSpaceExW`; unknown means no check). The files are then given real disk space before the first write. A volume with more than a million MFT records that does not fit, whose file cannot be made, or whose file fails during the read is **not** read into memory instead: the read stops at once, no index is written, and the scan fails with "the index could not be built: …" (the same `BUILD_FAILED` a build that cannot read its shards back gives). The service keeps serving the index it has and tries again after its back-off (15 minutes, then 30, up to 12 hours), with a warning in the log. **Live updates go on meanwhile:** a due rescan hands its live layer back to the maintainer instead of dropping it, and when the scan (or its room check) fails, the same layer goes on over the index served now. The journal readers never stopped (they deliver into the same channel while the scan runs, as during a merge), so what arrived meanwhile is applied and no journal position is replayed; the retry is the session's own rescan timer (15 minutes, then 30, up to 12 hours), health shows the rescan as pending with its time, and **Rescan now** ends the wait at once. The rescan reason stands: a journal reset, a large move or an administrator's request still ends in a scan, and until it works the index lacks only what the reset journal lost. Limits: the layer is kept through the scan only while its heap is at most 32 MiB (about 291 bytes per change, measured: it covers the smallest rescan threshold of 50,000 changes, not the hard limit), so the scan stays within the memory of ADR 0007 decision 10; a larger layer is dropped as before. The size is judged again at every retry: a layer that grew past it during a long back-off is dropped then. While a rescan waits for its retry the layer is not rescanned for an outgrown delta (it would fail again at the cost of reading the drives): when it reaches its hard limit the session ends instead and live updates stop until a scan works. Live updates also stop meanwhile when there is no session to keep: the first start with no index (nothing is served either), a startup whose saved tables could not be used (the old, loaded index is served as it was), other fatal scan errors, and volumes that cannot be reopened. The record map's file failing during a large volume stops its emission at the next folder and fails the scan the same way. Before a rescan the service removes temporary files a dead service left behind (a crash mid-scan may leave gigabytes), so they cannot keep the room check failing. Walking the volume instead is not done: it would fill the same disk, slower.
- **Small volumes** (up to a million MFT records, a few hundred MB at most) fall back to the heap: a table that cannot get its file stays in the heap, a names file that fails is read back (up to 128 MB) and continues in memory, and a read that cannot recover runs again from the start in the heap. Every such fallback is counted (`ferrow_index::store::scratch_in_heap`, the same counter the sort orders use); the event log says "the scan could not keep all of its file-table read in temporary files (…); that part was kept in memory", and the service warns when the count grows. The live layer's new tables (`Live::new_in`) are counted the same way; they are 8 and 4 bytes per entry and are replaced by the saved copy right after when the tables can be saved (otherwise they stay for the session; refusing them, and serving without live updates instead, is a follow-up). When it worked the log says "the scan's file-table read kept up to N MiB of tables in temporary files, not in memory", and the scan line in Health says "tables in temporary files".
- **Without an index file** (no directory, CLI scans, tests) nothing changes: the heap path.
- **Not verified:** the real MFT read with files (`read_volume_in` on a raw volume) and the scan's handling of a failed large read were not run on this PC (the session is not elevated); the elevated CI test covers the success path only. The failing paths are covered by tests of the builder with an injected size limit and free space, not of the whole scan.

**Restart without a scan (ADR 0007).**

- **What is saved:** after each scan and each in-memory merge, the maintainer also writes `live.bin` beside `index.bin` (module `saved`), then maps it back so the live layer reads its record tables from it. The file holds:
  - the frozen entry → record table;
  - the frozen children table;
  - each volume's record → entry table and hard links;
  - each volume's device, change journal id and the USN every saved change is up to: the scan's starting position after a scan, the last applied batch's position after a merge.
- **How it is tied to its index:** it carries the checksum field of the `index.bin` it belongs to, its entry count, and a fingerprint of the roots, excludes, options and Ferrow version.
- **When it is saved:** only when every root is a volume read through its MFT whose journal could be re-read right after the read. It is not saved after a merge while a rescan is waiting, because the index is then known to miss changes.
- **At start:** the service uses the mapped `index.bin` with `live.bin` instead of scanning when:
  - the checksum, the entry count and the fingerprint match;
  - every volume opens;
  - its change journal has the same id;
  - the saved position is still inside the journal.

  The journal readers then start at the saved positions, and the changes since the save are applied like any other batch.
- **Otherwise it scans**, with the reason in the log: "startup scan: …".
- **Crash safety:** a crash between writing the two files leaves a mismatch, so the next start scans. Every value of `live.bin` is checked when it is read.

## Development: `ferrow service run`

```sh
ferrow service run ~/Documents   # foreground, per-user endpoint
ferrow status                    # mode: service (...)
ferrow search report             # answered by the daemon
```

`run` needs no privileges and uses a per-user pipe/socket name,
so it never collides with an installed system service. The
`FERROW_ENDPOINT_NAME` environment variable pins a short
endpoint name for tests and multiple side-by-side daemons.

An idle daemon uses 0 % CPU: it parks on `accept` and has no
timers. The index is an immutable snapshot; rescans build a new
one and swap it in without blocking queries.

Snapshot publication wakes a coalesced persistence worker. A dirty index arms
one 250 ms deadline; a clean index waits indefinitely for an event. Controlled
shutdown saves the latest snapshot and reports persistence failures. Publications
advance a generation after swapping the snapshot. If shutdown arrives during a
save, the worker compares its captured generation with the current generation
and saves again until it covers the newest publication. Hosts stop publishing
before finishing persistence. Unix
foreground hosts handle SIGTERM/SIGINT cooperatively; SCM STOP/SHUTDOWN uses
the same final-save path. A failed load or state-policy check refuses startup.

The host admits at most 32 connections, with one in-flight operation each.
Handshake and frame I/O have absolute five-second deadlines. Shutdown cancels
and joins owned workers. A reply is capped at 10,000 paths and 4 MiB of encoded
path data; exceeding either returns `TooLarge` with a request to use `--limit`.
