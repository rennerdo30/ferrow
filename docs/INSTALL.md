# Installing Ferrow

## Windows

Download `Ferrow-<version>-setup.exe` from the [releases page](https://github.com/rennerdo30/ferrow/releases) and run it. Setup asks for administrator approval once.

**What setup installs**

- The Ferrow app (`ferrow-gui.exe`), the `ferrow` command line and the updater in `C:\Program Files\Ferrow`.
- A Start menu entry **Ferrow** for all users.
- Optionally (checked by default) the **Ferrow index service** `ferrowd`: a Windows service that runs as SYSTEM, starts with Windows and restarts after a crash. Its program lives in its own administrator-only folder, `C:\Program Files\Ferrow\service`, and its index in `C:\ProgramData\Ferrow`, readable by administrators only. The `ferrow` command line searches it. The app keeps its own index in your profile (`%LOCALAPPDATA%\ferrow`) for now, so it works with or without the service.

Setup writes nothing into your profile and never changes PATH.

**Silent install:** `Ferrow-<version>-setup.exe /S`. Add `/NOSERVICE` to leave the service out (or remove it on an upgrade). Exit code 0 means success. The log is `%TEMP%\Ferrow-installer-<version>.log`.

## Updating

- **In the app:** Settings → Updates → **Install update**, then approve the administrator prompt. Ferrow closes, updates and opens again.
- **By hand:** run the newer setup over the existing install.

Setup asks Ferrow to close in every signed-in session and waits for it; it never kills programs. If Ferrow stays open, setup offers Retry or Cancel (a silent setup stops without changing anything). The service is stopped (it saves its index first), replaced and started again. The index and settings are kept.

**When another administrator approves:** if a different administrator enters their credentials at the prompt, setup still updates the program and the service, but it does not start the app under that administrator's account; start Ferrow from the Start menu afterwards. A standard user can only update with an administrator's approval; without it nothing is downloaded or run.

Channels (Stable, Beta) are explained in [RELEASING.md](RELEASING.md#update-channels).

## Uninstalling

Settings → Apps → Installed apps → Ferrow → Uninstall, or silently `"C:\Program Files\Ferrow\uninstall.exe" /S`.

Uninstall closes Ferrow (it never kills it), stops and removes the service, and removes the installed files, the Start menu entry and the Installed apps entry. **It never deletes your data:** `C:\ProgramData\Ferrow` (the service index) and `%LOCALAPPDATA%\ferrow` (the app's index and settings) stay. Delete them yourself if you no longer need them.

## Linux and macOS

Packages are not available yet; build from source (see the [README](../README.md#building)). The service can be installed with `sudo ferrow service install` ([service.md](service.md)).
