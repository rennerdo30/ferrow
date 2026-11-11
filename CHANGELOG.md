# Changelog

Release notes for each version. The release workflow copies a version's section into its GitHub release.

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
