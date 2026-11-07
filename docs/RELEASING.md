# Releasing Ferrow

Every release needs the owner's yes. CI builds and tests the setup and prepares a **draft** GitHub release; publishing the draft is that yes. Nothing in CI publishes on its own.

## Branches

| Branch | Purpose | CI on push |
|---|---|---|
| `main` | Development. Every change lands here first through a pull request. | Full checks, plus a tested setup as a 14-day build artifact (not a release). |
| `beta` | The beta line. Fast-forwarded or merged from `main` when a state is ready for testers. Beta tags are cut here. | Same as `main`. |
| `release/X.Y` | A stable line, branched from `beta` (or `main`) when `X.Y.0` is ready. Only fixes land here, cherry-picked from `main`. Stable tags `vX.Y.Z` are cut here. | Same as `main`. |

Pull requests run the same checks for any target branch. The release workflow refuses a tag whose commit is not on an allowed branch: beta tags on `main` or `beta`, stable tags on `main` or a `release/*` branch.

## Versions

One version for the whole workspace: `version` under `[workspace.package]` in `Cargo.toml`, following SemVer. Betas are `X.Y.Z-beta.N` (for example `0.1.0-beta.1`). A stable `0.1.0` ranks above every `0.1.0-beta.N`.

## Cutting a release

1. On the branch for the release (`beta` for a beta, `release/X.Y` for a stable release), set the workspace version and add a `## X.Y.Z[-beta.N]` section to [`CHANGELOG.md`](../CHANGELOG.md). Merge through a pull request so CI runs.
2. Tag that commit and push the tag:
   ```sh
   git tag -a v0.1.0-beta.1 -m "Ferrow 0.1.0-beta.1"
   git push origin refs/tags/v0.1.0-beta.1
   ```
3. The **release** workflow ([`.github/workflows/release.yml`](../.github/workflows/release.yml)):
   - checks that the tag matches the workspace version and comes from an allowed branch;
   - builds the release binaries, packages `Ferrow-<version>-setup.exe` with the pinned NSIS and writes `SHA256SUMS.txt`;
   - runs the installer lifecycle test on the disposable runner (install, upgrade with data kept, updater refusals, uninstall with data kept);
   - creates a **draft** release with both files and this version's changelog section. A `-beta.N` tag becomes a pre-release.
   Rerunning it (Actions, release, Run workflow, with the tag) refreshes the files of a draft and never touches a published release.
4. **The owner reviews the draft and publishes it.** Only then do update checks see it.

The setup's file name and `SHA256SUMS.txt` are the contract with the in-app updater; keep both names.

## Update channels

Settings → Updates in the app:

- **Stable** asks GitHub for `releases/latest`: the newest published stable release.
- **Beta** takes the newest published release, beta or stable.
- **Automatic** (the default) follows the installed version: a beta build updates on Beta, a stable build on Stable.

Drafts are invisible to both. An installed copy checks once when it opens, at most once every 24 hours, and on **Check now**; development builds check only on **Check now** and link to the release page instead of installing.

## What protects an update

The app passes only the version to `ferrow-update.exe`, which runs with administrator approval. It downloads `SHA256SUMS.txt` and the setup itself from the fixed release URL into a fresh administrator-only folder under `%ProgramFiles%`, checks the SHA-256 through a handle that blocks changes, and runs the setup only on a match. Nothing from a user-writable folder is ever run elevated.

The checksums come from the same release as the setup, so this protects integrity, not authenticity. The setup is not Authenticode-signed yet, so Windows SmartScreen warns on first run.

## Building the setup locally

`scripts\package.bat <path to makensis.exe>` (or `scripts/package.ps1 -MakeNsis <path>`) builds the release binaries and `artifacts\installer\Ferrow-<version>-setup.exe`. It needs the portable NSIS 3.13 ZIP named at the top of [`scripts/package.ps1`](../scripts/package.ps1) and never installs anything. Never run `scripts/test-installer.ps1` outside CI: it installs a LocalSystem service.
