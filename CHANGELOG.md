# Changelog

Release notes for each version. The release workflow copies a version's section into its GitHub release.

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
