# Changelog

Release notes for each version. The release workflow copies a version's section into its GitHub release.

## 0.1.0-beta.18

- **Fixed: the service rescanned every drive while git was busy.** When git, git-lfs or another tool moved a folder into an indexed place, the service could not know what was inside it and read every drive again (about 75 seconds of heavy CPU on a large PC, up to every 11 minutes). Now only that folder is read, in the background at low priority, and its files show up in search within moments. A full rescan happens only for a moved folder with more than about 50,000 files, after 30 seconds, or on an error.
- **Much less memory while the index is built.** Building the index (at start and on a rescan) used to need about four times the memory it keeps afterwards, roughly 10 GB on a PC with 27 million files. It now needs well under half of that; the memory it keeps afterwards, the saved index and search speed are unchanged, and loading is a little faster.
- **Service health in Settings.** A new **Service health** section shows at a glance whether the Ferrow service is healthy, stopped, not installed, older than the app or not answering, with its version, uptime, memory, items, last save, every drive's state (live updates, last scan, a rescan that is waiting) and its recent messages. Buttons: **Start**, **Restart**, **Rescan now**, **Repair…** (each asks for administrator approval) and **Copy diagnostics** (a report for bug reports, without file names). The **Fast index** card points here when the service needs attention. It looks at the service at most every 5 seconds and only while Settings is visible; hidden in the tray, Ferrow does not ask at all.
- **The same from a terminal:** `ferrow service health` (`--json` for tools), `ferrow service rescan`, `ferrow service start | stop | restart` (elevated terminal / sudo) and `ferrow service repair` (elevated). Repair checks the installed program and its registration, the service's data folder and the saved index without loading it; if everything is fine it says **Nothing to repair** and leaves the service running, otherwise it fixes what is wrong (an index that is damaged is renamed to `index.bin.bad-<time>`, never deleted, so the service rebuilds it) and restarts the service.
- **The service keeps a log** (`service.log` next to its index, administrators only, at most about 2 MiB), including how its memory is used after every scan.
- **Settings fixes:** the section list on the left highlights the section you are reading (also right after you click one), and content scrolls under a clean edge below the title instead of showing half a line.

## 0.1.0-beta.17

- **Standard accounts can use the Ferrow service.** Until now only administrator accounts could use the service's fast, live index; a standard account fell back to its own slower one. An administrator now chooses who may use it, under Settings → **Fast index → Who can use the service**:
  - **Allow me** for the account you are signed in with: on a PC with separate admin and daily accounts, type the admin's password at the prompt and your daily account switches to the service within a second or two, without restarting Ferrow.
  - **Add account…** for another account by `DOMAIN\user`, `user` or SID: Ferrow looks it up first and shows what it found.
  - **Allow everyone** for every person who signs in to the PC (it asks first, because they then see every file name on it; service and guest accounts are never included), or **Administrators only**.
  - Nothing changes unless you change it: only administrators are served, as before. The same settings work from an elevated terminal: `ferrow service access show | admins | everyone | add <account> | remove <account>`. Run Setup (or the in-app update) so the service is updated too.
- **You can see at once where results come from.** The status bar starts with **● Live · Ferrow service** (green) or **○ Own index** (amber) with the reason, for example *account not allowed* or *service stopped*; click it to open the right place in Settings. While Ferrow builds its own index, it shows the items found so far, the folder being read and the elapsed time.
- **Results stay put.** When the service stops answering (for example during an update), your results stay and a slim bar says what happened. A search text that doesn't parse yet keeps the last results (dimmed) and shows the message at the bottom instead of blanking the list.
- **Easier to read.** Long folder paths keep the folders that tell rows apart (`…\Daft Punk\Album 1`), long names keep their extension and the word you searched for, and in narrow windows the Folder column keeps its room. Japanese names sit on the line. "No results" says what was searched and which filters were in the query. The search syntax help opens under the search box, fits the window and is grouped.
- **Settings shows what matters first.** The top says whether the fast index is on (with the item count) or off and why, with the matching action. Settings that only apply without the service (locations, excluded folders, rebuild and so on) are grouped as "This app's own index" and folded away while the service runs. Wide windows get a section list on the left; text no longer runs under switches; the version and last update check are shown once.
- **First start offers a choice:** your home folder (recommended), all drives, or choose folders.
- **Faster start with the Size and Date modified columns:** loading the saved index with them took about 15% longer than without; they are now read alongside the rest and add no measurable time at 5 million files.

## 0.1.0-beta.16

- **Fixed: the service kept rescanning and used gigabytes of memory.** On a busy disk, almost any folder moved into an indexed place, or a burst of changes, started a full rescan of every drive. While a rescan ran, two indexes were in memory and the search threads were busy with the scan, and afterwards the whole index file was written again. Now:
  - A rescan waits until at least nine times its own duration has passed since the last one (between 1 and 30 minutes). Changes keep showing up live while it waits.
  - Rescans run on a few low-priority threads with low disk priority, so searches and your own programs come first.
  - The index file is saved at most every 30 minutes while the service runs, and always when it stops.
  - About 16 bytes per entry that were only needed at startup are released (about 500 MB at 33 million entries).
- **Fixed: a whole drive could lose live updates and take minutes to scan.** If the drive's change journal filled up while its file table was being read, Ferrow gave up and walked every folder instead (170 s for 19 million entries on the test PC), and that drive then got no live updates. Ferrow now continues from where the journal currently starts.
- **Fixed: the app kept searching while hidden in the tray.** Every change on disk made it run the current search again through the service, up to five times a second, even with the window hidden. It now searches again at most twice a second, and only while the window is shown (it catches up as soon as you open it).
- **Faster live updates for deleted or moved folders:** removing a folder no longer checks every pending change once per subfolder.

## 0.1.0-beta.15

- **Drag results out (Windows):** drag a result into Explorer, onto the desktop, into a mail, chat or editor window to copy, move or link the file, as from Explorer.
- **Faster searches for names that sort last:** searches whose matches all sort near the end (Japanese, Chinese and other non-Latin names, for example `文書`) show their first page about three times sooner (5 million entries: 280 ms → 79 ms).
- **Faster start:** checking the saved index while loading is quicker (about 45 ms less at 5 million entries).

## 0.1.0-beta.14

- **Keyboard shortcut (Windows):** press **Ctrl+Alt+F** anywhere to bring up Ferrow with the search box ready; press it again to put Ferrow back in the tray. Change it or turn it off in Settings → Windows → Keyboard shortcut (Ctrl+Alt+Space and Alt+Space are offered too). If another app already uses the shortcut, Settings says so.
- **Explorer right-click menu (Windows):** right-clicking a result shows the same menu as in Explorer (Open with, Send to, Properties, your cloud and archive tools), with Ferrow's **Open file location**, **Copy full path** and **Copy name** on top. Hold Shift for the extended menu. The menu runs separately from the window, so a slow shell extension or an unreachable network drive can't freeze Ferrow.
- **File type filters:** `audio:`, `video:`, `pic:`, `doc:`, `archive:` (or `zip:`) and `exe:` find files by kind. Alone they list every file of that kind; with a word they narrow it, for example `audio:beatles`, `pic: holiday` or `!exe: setup`.
- **Faster start:** the saved index loads about a third faster (5 million entries: 464 ms → 314 ms on the test machine), and loading no longer needs a second copy of the index file in memory.
- **Faster searches with few results:** a search that matches almost nothing shows its (empty or short) first page several times sooner (5 million entries: 194 ms → 34 ms), and `path:` searches combined with a word are about twice as fast. The new type filters count their results about twice as fast as at first (`doc:`: 145 ms → 77 ms).

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
