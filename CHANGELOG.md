# Changelog

Release notes for each version. The release workflow copies a version's section into its GitHub release.

## Unreleased

- **The app no longer stays frozen when the graphics driver hangs.** On a PC with an AMD graphics card the app froze for good: its window showed "Not responding", and clicking the tray icon or starting Ferrow again did nothing. The driver never came back from waiting for the screen's next refresh (vsync) when showing a frame. The app no longer waits for that refresh. It draws only when something changes and has no animations, so it draws no more frames than before. If a driver still blocks the window for about 20 seconds, the app starts a fresh copy of itself and ends the stuck one. The fresh copy comes back in the tray, or with the window open if it was open or you had just tried to open it. It tries this at most three times in a row, and the count starts again once the app has run fine for 10 minutes. Not yet tried on the PC that froze: the restart decision is tested, the hang itself is not.

## 0.1.0-beta.22

- **A full disk no longer puts the live tables in memory either.** After a scan the service builds three tables for live updates (which entry is which file record, and which folder holds which entries): about 20 bytes per file, roughly 0.5 GB on a PC with 27 million files. When there was no room for their temporary files they were built in memory instead, with only a warning. A large table is now refused instead: the service serves the new index as the scan read it, **without live updates**, says why in its log and in Service health (every drive it read shows "live updates unavailable" with the reason, the problem list leads with it, and the retry is shown as a rescan pending with its time), deletes the saved tables of the old index (they take room the retry needs), and tries the scan again after 15 minutes, then 30, up to 12 hours (the retry first checks for room, so a full disk only waits again; **Rescan now** ends the wait). Live updates are back after the scan that works. If only the children table of folders has no room, live updates go on, the tables are not saved for the next start (the next start scans), and renaming or deleting a folder that was in the index asks for a rescan instead of being applied; the refusal is remembered for a minute, so a batch of such changes no longer repeats the pass over the whole index (hundreds of milliseconds to seconds at 27 million files) and a 108 MB file for each one, and a large table checks for room (about 8 bytes per file) before it starts. Tables of up to a million entries still fall back to memory, counted. Not tried on a real PC or a real full disk: the refusal and the skip are tested with made-up sizes and a missing folder, and the service loop that skips live updates is not driven by any test. The layer a merge of live changes makes refuses its children table the same way; the merge's own two record tables (12 bytes per file) are still built in memory.
- **A failed rescan no longer freezes live updates.** When a rescan could not run (no room beside the index, or a build that could not be written) the service waited 15 minutes, then 30, up to 12 hours without applying any live change, so the index it kept serving fell behind by every change made meanwhile. The service now keeps applying live changes to the current index while the rescan waits for its retry; the retry and the reason for it are unchanged, Service health shows the rescan as pending, and **Rescan now** still ends the wait at once. To keep the scan's memory low the live changes are kept through a rescan only while they are small (up to 32 MiB, about 110,000 changes); with more, or when they grow past their hard limit during the wait (the size is checked again at every retry, so a long wait on a busy PC can end it), live updates stop until a scan works, as before. The first start with no index is unchanged (nothing is served until a scan works). Not tried on a real PC: it needs raw volume access; the session hand-over is tested with made-up volumes and records.

## 0.1.0-beta.21

- **A full disk no longer pushes the index into memory.** When the index file could not be written (usually a full disk), a rescan or merge used to build the whole index in memory instead (gigabytes on a large PC). Now a merge first checks that the disk has room for the new file (about 2.5 times the current one) and otherwise keeps the index as it is and tries again later, and a large index that could not be written is not built in memory. A rescan also checks for room first, and both wait longer after each failure (15 minutes, then 30, up to 12 hours); Service health says so while merging changes keeps failing. Small indexes (up to a million entries) still fall back to memory. On Linux the file's disk space is reserved before it is written, so a full disk is an error, not a crash.
- **Sorting by size or date no longer adds to the service's memory.** The sort order the service prepares the first time you sort by size or a date (about 100 MB per column with 27 million files, plus as much again while it is built) is now kept in a temporary file beside the index, in Windows' file cache like the index itself, and removed once the index it belongs to is no longer in use. Service health's "Index file" figure counts these files too.
- **Much less service memory: the index is read from its file.** The service used to keep its whole index in its own memory: about 3.7 GB on a PC with 27 million files. After every scan and every in-memory merge it now saves the index and reads it back through a memory map, so the index lives in Windows' file cache, which other programs can take back at any time, instead of in the service's own memory. Searches are as fast as before while the file is cached. The first search after Windows has dropped it from the cache reads from disk. Task Manager should show well under 1 GB for the service after its first scan (estimated, not yet measured on a real PC). Starting, rescans and merges still need extra memory for now; the next steps of this work (ADR 0007) remove that.
- **Rescans and merges no longer need gigabytes of extra memory.** A rescan or an in-memory merge built the new index in the service's memory (about 160 extra bytes per file for a rescan, 85 for a merge: gigabytes on a PC with 27 million files, far over the 1 GB limit) and only then saved it. It now writes the new index straight into its file: the entries wait in a small buffer that spills to temporary files beside the index, and the finished index is put together, sorted and checked inside the file, which Windows keeps in its file cache instead of the service's memory. Measured on a synthetic 2 million file index, putting the index together grew the service by 4.9 MB (the heap build grew it by 77 MB) and a merge grew it by 79 MB (the heap merge: 155 MB); the saving grows with the size of the index. Not measured on a real 27 million file PC yet. It needs room for the new index file and about 60 bytes per file of temporary files (estimated) beside the old one while it runs, and takes about the same time as before (up to about 15 % longer on the synthetic 2 million file index). If the file cannot be written (full disk) the old index stays; see the entry on full disks above. The scan's own tables and the live tables of a merge are still in memory; the next steps remove them.
- **A full scan no longer needs gigabytes of memory to read the drives.** Reading a drive's file table (the first start, after the change journal was reset, **Rescan now**) kept everything it had read in the service's own memory until the index was built: about 180 bytes for every file at its peak, roughly 4 to 5 GB on a PC with 27 million files (extrapolated from a synthetic test), far over the 1 GB limit. The read now keeps what it parsed, the table that maps each file to its place on the drive and the tables the live updates start from in temporary files next to the index (they are deleted when the scan ends, however it ends), which Windows keeps in its file cache instead of the service's memory. Measured on a synthetic 1 million file drive, the scan grew the service by about 58 MB at its peak (182 MB before) and that figure does not grow with the number of files, so it should be about 60 MB plus 64 MB of read buffers on a 27 million file PC (estimated, not measured there). It needs free disk space next to the index while a drive is read: about 160 bytes per file-table record for the read and 130 for the shards and the new index it writes (about 5 to 8 GB on that PC, estimated; the scan checks this before it reads a drive, and removes temporary files a crashed service left behind first), and takes about 10 % more time in the steps that parse and sort the read (50 to 60 ms per million files in the synthetic test; a scan of about 53 s should take 1 to 1.5 s longer, estimated). **If there is not enough room, a large drive is not read into memory instead:** the scan fails with "the index could not be built", the service keeps its current index, says so in its log and tries again after 15 minutes, then 30, up to 12 hours. Until a retry succeeds, changes on the drives are not applied to that index (searches find the files as they were before the scan was started). A drive with up to a million records still falls back to memory, and the log and Service health say so. Not yet tried on a real PC (the failing paths are tested with a made-up size limit and free space, not with a real full disk), and a merge of live changes into the index still builds its own tables in memory.
- **Saving the index no longer builds the whole file in memory first** (a peak as large as the file, about 3 GB with 27 million files); it is written in small pieces.
- **Service health tells the two apart:** "Memory … in use" is now the figure Task Manager shows, and a new "Index file" line shows how much of the index is read through the file cache. `ferrow service health` shows the same.
- **No full scan when the service starts.** After every scan and merge the service also saves its record tables (`live.bin`, next to the index). When it starts, it loads the saved index and those tables and only replays the Windows change journal since they were saved, instead of reading every drive again. That removes the minute of heavy disk reading at every boot and update, and the memory peak that came with it (6.2 GB on a PC with 27 million files). It still scans when the journal was reset, no longer reaches back far enough or holds more than about a million changes since the save, when a drive letter now points at another volume, when the roots or excludes changed, after an update of Ferrow, or when a drive is not read through its file table. To keep the saved state current, the record tables alone are saved again once the journal has moved about 64 MB past it (at most a quarter of a small journal), and a merge, which rewrites the whole index file, runs only once it has moved about 192 MB (at most half a small journal) or after 12 hours. A failed attempt waits 15 minutes before the next. Not yet tried on a real PC.
- **The record tables are read from that file too**, not kept in the service's own memory (about 600 MB with 27 million files; estimated).
- Index files are now saved as version 4. Older versions of Ferrow cannot read them (they rebuild the index); this version still reads the old files once.

## 0.1.0-beta.20

- **Fixed: renaming or moving a large folder still rescanned every drive.** When a folder with more than about 100,000 files and folders below it was renamed or moved (a tool that swaps or cleans up a large folder does this often), the service read every drive again: about 55 s of heavy CPU and disk on a PC with 27 million files, up to every few minutes. It now merges the move into the index in memory instead, with no disk reads. The folder shows up under its new name at once. The files inside show up under the new path once the merge is done, and are never shown under the old path meanwhile. The merge usually starts at once and takes a few seconds on a PC with tens of millions of files (estimated, not yet measured there); right after another merge, or while the index is being saved or moved-in folders are being read, it can take longer.

## 0.1.0-beta.19

- **Fixed: drives with a large change journal were read folder by folder and got no live updates.** On a drive whose Windows change journal is bigger than 64 MiB, the service read the whole file table, then asked the journal for the last few minutes of changes at a position Windows refuses (error 87, "The parameter is incorrect") and threw the finished read away. Service health said the drive was "not NTFS", the drive got no live updates, and a rescan took minutes instead of seconds. The service now asks at a position Windows accepts. If the journal still cannot be re-read, it keeps the finished read and goes live anyway, with a warning that files changed just before the scan may show their old state until the next scan.
- **No more full rescans when many changes pile up.** When many changes had collected on top of the index (a build, git, an install), the service read every drive again: over a minute of heavy CPU and disk on a PC with tens of millions of files. It now merges the changes into the index in memory instead, with no disk reads: 0.1 to 0.35 s for 1 million items, on low-priority background threads, with about half the extra memory a rescan needs. Searches keep answering from the old index meanwhile. A full rescan still happens when the change journal was reset, when a moved-in folder could not be read, when you ask for one, and when the service starts.
- **Less CPU while many files change at once.** Keeping search up to date during a storm of changes now runs at low priority and less often: about half the CPU at 50,000 waiting changes and a quarter at 200,000. Changes show up within about a second (at most 5 seconds while very many are waiting). A file that is written again and again (a log, a database) now counts as one change, not as more and more. The number of changes that may wait grows with the index (1 %, from 50,000 up to 250,000).
- **Faster first results with many recent changes.** Searches that match recently changed files were about 2.5 times faster with 200,000 changes waiting; other searches are unchanged.
- **Service health:**
  - A drive that had to be walked shows the real error instead of a guess.
  - The scan line reads "last scan today 18:09, took 3 min 18 s, after 50,001 live changes".
  - A new "Live changes" line shows the last in-memory merge.
  - Long messages in Recent events are kept up to 600 characters and end with "…" instead of being cut off.
  - `ferrow service health` shows the same.

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
