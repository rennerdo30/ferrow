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
`/run/ferrow/ferrowd.sock`, macOS `/var/run/ferrow/ferrowd.sock`.
Caller identity always comes from the OS (Windows client PID and
token, Linux `SO_PEERCRED`, macOS `getpeereid`), never from the
client's own claim.

**Privacy status:** per-user result filtering is milestone M2c
and is **not implemented yet**. Until it lands, any local user
who can connect can see every indexed file name. Do not run the
system service on a shared multi-user machine yet.

## Install (needs admin/root once)

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

The system service reads its index from
`%ProgramData%\ferrow\index.bin` (Windows) or
`/var/lib/ferrow/index.bin` (Unix). Build one with
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
