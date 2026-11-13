# Changelog

Release notes for each version. The release workflow copies a version's section into its GitHub release.

## 0.1.0-beta.13

- **Starts with Windows, in the tray:** new setting **Start Ferrow when you sign in** (Settings → Windows, on by default). Ferrow starts hidden in the notification area, so search and the Start menu integration are ready right after you sign in. Opening Ferrow yourself still shows the window.
- **Updates keep Ferrow in the tray:** if Ferrow was in the tray when an update closed it, the updated copy starts there too instead of opening its window.
- **Fixed:** result columns could not be resized. The resize grips lay under the header cells, so dragging a divider hit the header instead, and the divider right of Folder did nothing. All dividers now resize; next to Folder (which takes the remaining space) they resize the column on the other side.
- **Better Start results:** Windows' prefetch traces (`C:\Windows\Prefetch\NAME.EXE-1234.pf`, one per program run) are no longer offered to Start, so searching a program name shows the program instead of its traces.

## 0.1.0-beta.12

- **Fixed:** the app could crash about 10 seconds after an update (or whenever the Ferrow service came up after the app). It had started on its own index while the service was restarting, and the switch to the service left the result list pointing into the index it just dropped. While it was down, Start searches fell back to Windows. The result list is now cleared on every index switch, and rows from a replaced index are never read.
- **Fixed:** every Ferrow update left one more copy of its Start-search DLL loaded in `SearchHost.exe` until you signed out. When an older copy is loaded, Ferrow now restarts `SearchHost.exe` once (Start search reloads; Windows starts it again by itself), so only the current copy stays.

## 0.1.0-beta.11

- **Tray icon (Windows):** closing the window now keeps Ferrow running in the notification area, so the live index and the Start menu integration keep working. Click the icon to bring the window back (the search box is ready to type); right-click it for **Open Ferrow** and **Quit Ferrow**. Starting Ferrow again while it runs shows the running window instead of a second copy.

## 0.1.0-beta.10

- **Fixed:** programs (`.exe`, `.msi`, `.bat`, ...) that Ferrow found never showed in Start, because Start hides file results of the kind "program". Ferrow now reports them as plain files, so for example `animasky.exe` lists every copy Ferrow knows.
- Windows' prefetch files (`C:\Windows\Prefetch\*.pf`) now rank last in Start instead of taking places from the files you look for.
- **Fixed:** buttons in Settings did not react to the mouse. They now light up on hover and darken while pressed, like Windows buttons.

## 0.1.0-beta.9

- **Fixed:** **Power Windows Search with Ferrow** showed "On" but Start never used Ferrow on current Windows 11 builds: Start sends its searches in a newer format that Ferrow did not understand, so every search went to Windows. Ferrow now answers them, including names with dots or dashes.
- **Start results ranked like Windows:** the whole name first, then names starting with what you typed, then words starting with it; newest first; cache, AppData and temp folders last. Other accounts' profile folders and dot folders (`.git`, `.cache`) are left out. Before, Start got the first 30 matches in name order, so "Best match" could be a file that only contained the word somewhere in the middle.
- **Documents, Photos, Music and Videos in Start** now come from Ferrow too, using Windows' own file kinds, so these groups and tabs list files outside the Windows index.
- **Faster typing in Start:** only the newest search per keystroke is run; slow searches no longer hold up the finished word.
- **Diagnostics:** an empty `diag` file in `%LOCALAPPDATA%\ferrow\winsearch` makes the Start integration log what Start asked and why a search went to Windows (search text masked). See `docs/windows-search.md`.

## 0.1.0-beta.8

- **Fixed:** after installing or updating Ferrow, the app could miss the Ferrow service because the service was still loading its index, then keep using its own slower index and show **Fast NTFS index and live updates** as off until restarted. The app now keeps checking in the background and switches to the service as soon as it answers, also after the service restarts.

## 0.1.0-beta.7

- **Fixed:** turning on **Power Windows Search with Ferrow** could fail with `Could not attach to SearchHost.exe: FerrowAttach returned non-zero status: 0x80070057`. The narrow folder permission granted to `SearchHost.exe` was missing `SYNCHRONIZE`, which prevented the AppContainer from verifying the Ferrow data folder at attach time.
- **Fixed:** upgrading Ferrow while `SearchHost.exe` still held an older Windows Search DLL loaded could reuse the old module instead of attaching the updated one. Each build now stages a read-only, content-addressed DLL copy outside the writable request folder and detaches any older loaded copy cleanly before attaching.

## 0.1.0-beta.6

- **Fixed:** turning on **Power Windows Search with Ferrow** failed on installed copies with "Could not remove the legacy all-AppContainer bridge access: Access is denied". Cleanup no longer tries to rewrite permissions it never granted (for example Windows' own inherited access on the installed program files), so the toggle now enables as intended.

## 0.1.0-beta.5

- **Windows Start menu search (experimental, Windows 11):** the new opt-in **Power Windows Search with Ferrow** toggle (Settings → Windows, off by default) answers file and folder searches in the Start menu from Ferrow's index — the live service index when the service is installed. Anything Ferrow cannot answer falls back to Windows Search unchanged. See `docs/windows-search.md`.

## 0.1.0-beta.4

- **Instant, live index through the Ferrow service:** with the service installed (the setup option, now recommended), the app searches the service's index. No more pressing Rebuild: the service indexes all internal drives by itself and keeps the index current as files are created, renamed, moved or deleted (changes appear within a second; no CPU use while nothing changes).
- **Fast NTFS indexing:** whole NTFS drives are read from the drive's own file table instead of folder by folder: seconds instead of minutes on an SSD.
- **Administrator accounts:** the service now answers the normal (not elevated) app of an administrator account. Standard accounts keep the app's own index until per-user filtering arrives.
- **Note:** the app keeps its own, slower index when the service is not installed. Single-folder locations are still scanned folder by folder.

## 0.1.0-beta.3

- **Fixed:** with "All internal drives" selected, a drive letter that is only a `subst` alias of another drive (for example D: pointing at E:) was indexed a second time, doubling the scan time and showing every file twice. Aliases are now skipped; the real drive is indexed once.

## 0.1.0-beta.2

- **Filters by size, date and attributes:** `size:>10mb`, `size:1mb..5mb`, `dm:today`, `dm:last10minutes`, `dm:2026-01-01..2026-02-01`, `dc:`/`da:` for created and accessed, `attrib:H`. They combine with everything else, for example `*.log dm:last7days` or `!attrib:H`.
- **Faster first page for rare filters:** a filter few files match (such as `size:<1mb` on 5 million files) shows its first results about 5x sooner.
- **Fixed:** clicking a column header (Date modified, Size, ...) did not change the sort.
- **Setup:** an easier-to-read welcome page.

## 0.1.0-beta.1

The first beta.

- **Desktop app** with a Windows 11 look: results as you type, an Everything-style search language (wildcards, `|`, `!`, quotes, `case:`, `path:`, `ext:` and more), configurable columns with size and dates, sorting by any column, and a settings page for the indexed locations.
- **Index** of names with size and modification time (optionally creation and access times and attributes).
- **Command line** `ferrow` with search and an optional background service.
- **Windows setup** for all users, with an optional index service, in-app updates on a Stable or Beta channel, and an uninstaller that keeps your data.
