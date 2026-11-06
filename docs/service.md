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

**Interim privacy rule, removed by M2c:** the system service answers
only OS-authenticated SYSTEM or enabled Administrators on Windows,
and root (uid 0) on Unix. All others receive `Error{Unauthorized}`
without paths, counts or status data. The CLI explains the refusal
and falls back to the per-user saved index. Development mode serves
only the OS user who started it. Per-user result filtering remains M2c.

## Install (needs admin/root once)

Installation copies `ferrowd` into `%ProgramFiles%\Ferrow\` on Windows
or `/usr/local/libexec/ferrow/` on Unix and registers only that fixed path.
Unix directories and binaries must be root-owned 0755, with no group/other
write access or symbolic-link component. Existing unsafe paths are refused.

Windows components must be owned by SYSTEM, Administrators or TrustedInstaller
and contain no reparse points. Non-admin ACEs must grant no WRITE_DAC,
WRITE_OWNER or DELETE on ancestors, and no FILE_DELETE_CHILD on their parents.
Create-subdirectory permission on a protected ancestor such as `C:\` is allowed.
The final directory additionally refuses non-admin add-file, add-subdirectory,
delete-child and other write rights, including inherited grants to new files.
The binary refuses non-admin write, append, delete, WRITE_DAC and WRITE_OWNER.
Constructed security descriptors test these rules, including create-only roots.
No user-supplied executable path is substituted into systemd or launchd files.

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
