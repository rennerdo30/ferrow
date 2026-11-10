# The Ferrow background service (`ferrowd`)

`ferrowd` holds the index in memory and answers searches over a
local, access-controlled IPC channel (crate `ferrow-ipc`). The CLI
(and later the GUI) stay unprivileged; only the daemon needs the
rights to read every directory (and, in later milestones, the
Windows MFT/USN journal).

## Modes

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
user who started it. Per-user result filtering for standard accounts is
M2c (ADR 0002).

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
