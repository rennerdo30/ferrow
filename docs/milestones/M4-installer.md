# Milestone 4: installer, service setup and updates

Status: **specified 2026-11-07.** This builds on M2b (`ferrowd`, `ferrow service install`) and [`../SPEC.md`](../SPEC.md). The model is the owner's Corewright installer, adapted to Ferrow's rules.

Releases and publication need the owner's yes. This milestone builds and tests the installer in CI; it publishes nothing.

## Goal

One setup program installs Ferrow for all users. It installs the background service, the desktop app and the CLI, and it updates and uninstalls cleanly. That includes the cases where:
- the person approving UAC is a different administrator than the signed-in user;
- several users are signed in.

No step freezes the PC, polls in the background, or loses user data.

## Windows setup (`Ferrow-<version>-setup.exe`)

- **Tool:** NSIS 3. It is zlib-licensed and used as a build tool, not a dependency. A portable copy with a pinned SHA-256 lives under the shared tool cache; there is no system-wide install and no PATH change.
- **Location:** a fixed per-machine path, `%ProgramFiles%\Ferrow` (no `/D`). Setup asks for administrator approval once.
- **Payload:**
  - `ferrow-gui.exe`;
  - `ferrow.exe`, the CLI;
  - the service host;
  - `uninstall.exe`;
  - an exact-payload manifest generated at packaging time.
- **Service:**
  - Setup registers the service through the same code path as `ferrow service install`, so the protected-path, ACL and quoting rules from M2b apply unchanged.
  - The service runs as LocalSystem, starts automatically, and restarts after failure (5, 15, then 60 seconds).
- **Data:** `%ProgramData%\Ferrow` with a protected DACL created atomically (SYSTEM and Administrators full; the index is not user-readable; see M2b). Setup never adopts or re-ACLs an existing tree in place. It builds a fresh protected tree, copies through pinned, non-reparse, single-link handles, keeps the old tree as `Ferrow-preserved-<random>` and renames the new one into place.
- **Shortcuts:** an all-users Start menu entry **Ferrow** that opens the app.
- **Sign-in start:**
  - The options page has **Start Ferrow when I sign in**, checked by default.
  - Setup only writes the default to `%ProgramData%\Ferrow\defaults.json`. The unelevated app applies it once in the signed-in user's own profile (`HKCU\…\Run`) and records the decision.
  - Upgrades keep each user's choice. Setup never writes another account's HKCU.
- **Finish page:** **Start Ferrow** launches the app through `explorer.exe`, so it runs with the user's normal unelevated token, never elevated.
- **Silent mode:**
  - `/S` installs or upgrades, and `uninstall.exe /S` removes. Success returns 0.
  - Service operations have per-command timeouts and bounded state waits.
  - A log goes to `%TEMP%\Ferrow-installer-<version>.log`.

## Upgrades

- **Run setup over the existing install,** or use **Settings → Updates → Install update** in the app.
- **Preflight first:** check for processes and locked files before stopping anything.
- **Service stop:**
  - Wait up to 30 seconds for a graceful service stop; the service saves its index on stop (M2b).
  - An abort before the files are replaced restarts a service that was running.
- **Closing the app:**
  - Setup asks running app instances to exit through a `Global\Ferrow.Exit` event, in every signed-in session, and waits up to 15 seconds.
  - Setup **never kills** applications. If one stays open, it offers Retry or Cancel; silent setup aborts.
- **After the upgrade:** setup restarts the app unelevated in each session where it ran before.
- **Data:** the index and settings are preserved. The index format version decides whether the service loads it or rebuilds it in the background; search stays available on the old snapshot until the new one is ready.

## Installing updates as another user

Three cases need explicit handling, as in Corewright:

1. **A different administrator approves UAC** (over-the-shoulder credentials):
   - The elevated token's SID differs from the interactive session user, found through WTS.
   - Per-user steps are then **skipped and logged**, never applied to the credential-prompt administrator's profile. That covers the HKCU Run value, migrating a per-user index from `%LOCALAPPDATA%\ferrow`, and the restart of the user's app.
   - The machine-wide install and the service still update normally.
2. **Several users are signed in:**
   - Every session's app instance gets the exit request.
   - Each user's own autostart choice stays in their own profile.
   - Instances that were running restart only in the sessions they came from, unelevated.
3. **A standard user starts the update:**
   - The app shows the update and asks for administrator approval.
   - Without it, nothing is downloaded to a privileged location and nothing runs elevated.

## In-app update check

- **When it checks:**
  - Only while the app is open: once when it opens, at most once per 24 hours, and on **Check now**.
  - There is no background polling, and the service never checks for updates (SPEC §3: 0 % idle).
  - The check runs off the UI thread, has a timeout, and is cancelled when the window closes. A slow or absent network can never freeze the app.
- **Source:** fixed GitHub Releases URLs with no token.
  - **Stable** uses `/releases/latest`.
  - **Beta** uses the newest release.
  - By default the channel follows the installed version.
- **Install, after administrator consent,** through an elevated in-memory bootstrap from the installed program:
  1. It downloads setup and `SHA256SUMS.txt` again itself.
  2. It puts them in a fresh administrator-only `%ProgramFiles%\Ferrow-update-<random>` folder, with its ACL applied at creation. Every ancestor is checked without following reparse points.
  3. It holds the files against write and delete sharing through hashing and execution.
  4. It runs setup only after the SHA-256 matches.

  Nothing from a user-writable cache is ever elevated. A failed verification never runs setup.
- **Development builds** show a download link instead of installing.
- **Not covered:** checksums come from the same release as the setup, so this protects integrity, not authenticity. Authenticode signing is not available yet, so Windows SmartScreen will warn until it is.

## Uninstall

- **Where:** Settings → Apps, or `uninstall.exe /S`.
- **What it removes:**
  - It closes the app instances (never kills them) and stops the service.
  - It removes the service, the payload, the Start menu entry and the Installed apps entry.
  - It removes the current user's Run value, but only after the same-SID check.
- **Order:** it validates the removal manifest before it deletes anything, and returns nonzero if a removal is incomplete.
- **Data is never deleted** (owner rule). `%ProgramData%\Ferrow` and `%LOCALAPPDATA%\ferrow` stay, and the last page names both folders.

## Linux and macOS

- **Linux:**
  - A `.deb` and an `.rpm` with the systemd unit from M2b, a desktop entry and an icon.
  - Updates come through the package manager; the app only shows that a newer release exists.
- **macOS:**
  - A signed-later `.pkg` that installs the app bundle and the LaunchDaemon from M2b.
  - The docs explain the Full Disk Access grant. Updates come through the `.pkg`.
- **Order:** both come after Windows, in this milestone or the next. Each needs the same never-delete-data and no-elevated-autostart rules.

## Release workflow (CI)

1. A version tag starts the **Release** workflow. CI builds on `windows-latest`, packages with the pinned NSIS, and produces `Ferrow-<version>-setup.exe` plus `SHA256SUMS.txt` (sha256sum format). These two asset names are the updater's contract.
2. The installer test runs on the disposable, elevated runner only. It refuses an existing install and checks:
   - silent install;
   - service identity, start mode and failure actions;
   - the protected data ACL;
   - an upgrade over the previous build with data preserved;
   - the different-administrator SID path (a second local admin is created on the runner);
   - silent uninstall with data kept.
3. **Publishing the GitHub release waits for the owner's yes.** It is a publication, so it is not automatic, unlike Corewright.

## Never on the owner's PC
No installer, no service install and no elevated command runs on the owner's PC. Packaging only builds the setup; tests run on CI runners or in Windows Sandbox.

## Acceptance (finite list)
1. `scripts/package.ps1` builds the setup with the pinned NSIS and an exact-payload manifest.
2. The CI installer test covers fresh install, upgrade with data kept, the different-admin path, the multi-session exit request, and uninstall with data kept.
3. The app's update check and elevated updater work against a test release in CI. The SHA-256 mismatch path is tested: setup never runs.
4. Update checks have a timeout and are cancellable, and the app's idle CPU stays 0 % with the updater present.
5. `docs/INSTALL.md` documents install, upgrade, other-user updates, silent commands and uninstall.
