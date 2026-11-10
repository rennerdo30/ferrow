# ADR 0006: Windows Search (Start menu) integration

Status: **accepted by the owner (2026-11-10)**, pending an independent
security review before merge. Supersedes the "parked" note in
`docs/SPEC.md` §9 (2026-11-06).

## Goal

File and folder searches typed into the Windows 11 Start menu are answered
from Ferrow's index: same coverage and freshness as the app itself
(all drives, live updates), instead of the Windows Search indexer's
subset. Opt-in only, off by default, Windows only.

## Context: there is no supported API

Microsoft documents no supported way to add local file results to Start
search. The web-provider API is EEA-only and returns web links; cloud-files
search integration needs a Copilot+ PC. That is why this was parked on
2026-11-06. The only route that exists is the unofficial one other tools
use: intercept the shell's own query path. The owner un-parked it on
2026-11-10 knowing it is unofficial, unsupported by Microsoft, and may
break on any Windows update. Every failure mode below therefore ends in
"native Windows Search answers instead", never in a broken Start menu.

## Overview

```text
Start menu -> SearchHost.exe -> tquery.dll ICommand::Execute
  -> ferrow_winsearch.dll (vtable hook, in SearchHost)
       parse SystemIndex SQL -> Ferrow query
       file bridge %LOCALAPPDATA%\ferrow\winsearch (req/resp files)
  -> ferrow-gui responder thread (ReadDirectoryChangesW, 0% idle CPU)
       app's in-memory index, or ferrowd over ferrow-ipc (service mode)
  -> in-memory OLE DB rowset (IRowset/IAccessor/IColumnsInfo/IGetRow)
  -> Start menu renders Ferrow's rows
Any error, timeout, unsupported query or zero hits -> original Execute
```

## Design

Crate `ferrow-winsearch` (cdylib `ferrow_winsearch.dll` + rlib used by
`ferrow-gui`):

- `sql`: extracts projection columns, `TOP` limit (clamped 1..=100) and
  search terms from `CONTAINS`/`LIKE` predicates on `SystemIndex`.
  Anything else returns "unsupported" and the native provider runs —
  including any `NOT`, and `OR` between different term sets, because
  Ferrow ANDs terms and either would change the result set. The
  observed same-terms-across-columns `OR` shape is kept.
  Folder-only `System.Kind` predicates map to `folder:`.
- `hook`: pins its own module, verifies the `ICommandText::Execute`
  vtable slot (slot 4 on `CLSID_CollatorDataSource`) belongs to
  `tquery.dll`, then swaps it with `VirtualProtect` + atomic store. The
  detour is `catch_unwind`-guarded and falls through to the original on
  any doubt. `FerrowAttach`/`FerrowDetach` are the remote-thread exports.
  Routing is re-checked per query against the `enabled` marker file, so
  the Settings toggle takes effect without restarting `SearchHost.exe`.
- `rowset`: pure-Rust OLE DB rowset. `SearchHost.exe` runs in a LowBox
  AppContainer where `CLSID_CADORecordset` fails, and it reads item
  properties via `IGetRow -> IPropertyStore`, so both the accessor path
  and memory property stores (`PSCreateMemoryPropertyStore`) are
  implemented. Rank is positional (1000, 990, ...) like a first page;
  there is no run-history signal yet.
- `ipc`: the file bridge and the responder. The responder mirrors the
  app's own source (published as `local`/`service`): the in-memory
  `Arc<Index>` in local mode, otherwise one deadline-bound
  `ferrow-ipc` `Page` query against `ferrowd` (200 ms budget inside the
  shell's 250 ms wait; reconnect with 800 ms budget and 2 s backoff on
  the responder thread only). With the GUI closed, local mode searches
  a preloaded `index.bin` cache in `SearchHost.exe` (<= 256 MiB, loaded
  off the query path at attach, searched with a 200 ms cancellation
  deadline; a cold cache answers natively until the preload lands);
  service mode never serves that stale file (the service index in
  `C:\ProgramData\Ferrow` is SYSTEM-protected), so native search answers.
- `controller` (GUI side): stages the DLL into the winsearch folder,
  grants AppContainer access (below), attaches to the same-session
  `SearchHost.exe` via `LoadLibraryW` + `FerrowAttach` remote threads,
  and runs a watcher that blocks on the process handle and re-attaches
  after shell restarts. Attachment is serialized; state and the last
  error feed the Settings status line. 64-bit x86 only; other
  architectures report an error instead of silently doing nothing.

Bounds: at most 100 rows per query, 250 ms shell-side wait, token/framing
caps in `sql.rs` and `ferrow-ipc`. Accessor bindings are validated
against the caller's declared row-buffer size at creation and again in
`GetData`, so the rowset can never write outside it. The DLL is built with the `winsearch`
profile (`panic = "unwind"`; `panic` cannot be set per package) so the
detour's `catch_unwind` works; everything else keeps `panic = "abort"`.
`scripts/package.ps1` and the workflows build the DLL separately.

## Security analysis

This is injection into a shell process and is reviewed as
security-relevant code. Deliberate properties:

- **No elevation, same user only.** Attachment targets `SearchHost.exe`
  in the caller's own session from the unprivileged GUI. The service
  (`ferrowd`, LocalSystem) is never involved in injection; it only
  answers ordinary `ferrow-ipc` queries under its existing caller checks
  (peer identity from the OS; per-user visibility status is ADR 0002's,
  currently the interim administrator gate). If the service refuses the
  caller, the responder gets an error and Start falls back to native.
  In local mode only the user's own index can answer. Either way, Start
  results for a user come from an index that user could already search
  in the app.
- **AppContainer grants, scoped to the shell package.** For the LowBox
  shell to reach the bridge, enabling grants access to the AppContainer
  SID read from the verified `SearchHost.exe` token — never to `ALL
  APPLICATION PACKAGES`: traverse-only (no listing/reading) on the app
  data folder, read on `index.bin` and the staged DLL, modify on the
  `winsearch` bridge folder. Other AppContainer apps get nothing;
  full-trust processes of the same user already had equivalent access.
  Identity is verified from the opened process handle (image name +
  session), closing the pid-reuse race. Grants are recorded
  (`granted-sid`) and revoked on disable, which also removes the staged
  DLL copy; legacy all-packages grants from earlier builds are revoked
  on enable. Uninstall without disabling first still leaves the
  per-user grants behind (user data is kept by design); follow-up (1).
- **Bridge files are transient.** Request files hold one query string;
  they are deleted when handled or on timeout, and leftovers are swept
  after 10 s. No query text is logged. Responses carry the same paths
  the Start menu is about to display to this user.
- **Shell stability before features.** Hook install refuses vtables not
  owned by `tquery.dll`; both modules are pinned; detach restores the
  original slot; the query path is allocation- and time-bounded and was
  audited for panic paths (slicing only at ASCII boundaries, no
  `unwrap` on external input). Residual risks that remain: abort on
  allocation failure inside `SearchHost.exe`, a Windows update changing
  `tquery.dll` internals between install-check and use, and antivirus
  heuristics. The setup and DLL are not Authenticode-signed yet
  (`docs/RELEASING.md`), which makes the last one more likely, not less.
- **Kill switch is instant and total.** Turning the toggle off removes
  the `enabled` marker; the next Start query goes native even though
  the DLL stays loaded (inert) until `SearchHost.exe` restarts.

## Alternatives considered

- **Supported search extensibility** (web provider, cloud-files):
  cannot return local file results; see Context.
- **Staying parked:** safest, but Start stays limited to the Windows
  indexer's coverage, which is the gap users hit first on Windows.
- **Answering from `ferrowd` directly inside `SearchHost.exe`** (named
  pipe from the AppContainer): the file bridge exists because LowBox
  pipe access and multi-gigabyte in-shell indexes are both dead ends;
  the unprivileged GUI responder keeps the service's authenticated IPC
  as the only service entry point.

## Consequences

- A Windows update can break attachment or query shapes at any time;
  the failure mode is "native results + an error in Settings", and
  keeping the SQL/rowset contract current is ongoing maintenance.
- The GUI must be running for Start results in service mode (the
  responder lives there). Local mode keeps working with the GUI closed
  for indexes up to 256 MiB.
- Only the extracted SQL subset is Ferrow-answered. When Ferrow answers
  a query it replaces that query's native file results (no merging);
  when it has zero hits, native results show unchanged.
- The catalog-dependent end-to-end test can only run where Windows
  Search itself answers (it skips on CI runners without a catalog);
  the rowset contract test runs everywhere. Live verification on a real
  desktop remains a release checklist item (see
  `docs/windows-search.md`).

## Follow-ups

1. Remove the per-user AppContainer grants on uninstall when the
   feature was still enabled (disable already revokes them; uninstall
   keeps user data today, so this needs a per-user cleanup step).
2. Offer detach (`FerrowDetach`) on disable so upgrades do not rely on
   move-aside plus shell restart.
3. Ranking beyond name order (run count / recency) once the app has it.
4. ARM64 Windows support (DLL is x86-64 only; reported in Settings).
5. Authenticode signing (already tracked for setup) also covers this DLL.
