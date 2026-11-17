# GUI screenshots

A test renders every reachable screen of the real desktop app
(`crates/ferrow-gui`) to PNG files **without opening a window**, for human
review and before/after checks of UI changes. Nothing is compared pixel by
pixel.

```text
FERROW_SCREENSHOT_DIR=/some/folder \
  cargo test -p ferrow-gui --bin ferrow-gui --features screenshots screenshots -- --ignored --nocapture
```

- `FERROW_SCREENSHOT_DIR` is the output folder. Without it a folder under the
  system temp directory is used and its path is printed.
- Files are named `NN-screen-theme-WxH.png` (for example
  `05-results-name-dark-1040x680.png`). Every screen is rendered in dark and
  light; the main states also at the narrow minimum size 560x360. Scale is
  100 %.
- The test is `#[ignore]` and the `screenshots` feature is off by default, so
  `cargo test --workspace` and release builds never compile or run it
  (`cargo tree -p ferrow-gui -e normal` is unchanged).
- Rendering is offscreen with wgpu through `egui_kittest`. On GitHub's
  `windows-latest` it uses the WARP software adapter. The workflow
  `.github/workflows/gui-screenshots.yml` runs it on pull requests that touch
  `crates/ferrow-gui/**` (and on manual dispatch) and uploads the
  `gui-screenshots` artifact. It is a separate workflow and not a required
  check.
- It needs the OS UI font (Segoe UI on Windows), like the app. Dates are
  shown in UTC and the fixture index is fixed, so two runs of the same code
  give the same pictures except for font hinting differences between GPUs and
  the "Add drive" list on the Settings pictures, which shows the machine's
  real drives. Test builds format dates in UTC (`format_time`), so the
  local-time path is only exercised by the app itself.
- Before a run it removes only its own pictures (`NN-screen-dark|light-WxH.png`)
  from the output folder; other files there are kept.

## How the app is isolated

The test drives the real `FerrowApp` and the real settings page, not a copy.
Nothing touches the machine's state:

- In test builds `engine::default_index_path()` has no real-folder branch. It
  returns the scratch folder the screenshot test sets (inside the output
  folder, deleted afterwards) or `None`. A forgotten or broken override can
  therefore never fall through to `%LOCALAPPDATA%` (settings, index,
  tray marker).
- The engine is `Engine::isolated`: no service connect or probe, no index
  file, no scans. Searches run over an in-memory fixture index; the test injects
  the events the worker would send.
- The app is built with `FerrowApp::assemble`, which skips the tray, global
  hotkey, sign-in registration, Windows Search controller, single-instance
  and setup-exit listeners. The update check is off.
- Right-click and double-click on a result row are never sent: on Windows they
  open the real Explorer menu or the file.

## Adding a screen

Add it to `capture_all` in `crates/ferrow-gui/src/screenshots.rs`: build a
`Session`, bring the app into the state (inject an engine `Event`, set a
field, hover or click at a position), then `save(number, name)`. Keep the
number order so the files sort like the tour of the app.

## Not captured

- The Explorer shell menu on a result row (opens a real OS menu).
- A distinct "service refused this account" screen: the app does not have
  one. A refused connection looks exactly like "no service running" and the
  app uses its own index silently.
- Anything that needs a real file dialog, tray icon, global hotkey or the
  Windows caption buttons' system hover (those are drawn by Windows).
