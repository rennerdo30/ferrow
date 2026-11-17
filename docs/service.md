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
  small second index, and a background rescan folds them in once that
  grows large or the journal is reset.
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
  rescan (why, earliest time), then the last 50 events. It is answered to the
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
