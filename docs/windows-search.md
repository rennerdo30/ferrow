# Windows Search (Start menu) integration

**Experimental, Windows 11 (64-bit x86), off by default.**

When turned on, file and folder searches in the Windows Start menu are
answered from Ferrow's index instead of the Windows Search index. You get
Ferrow's coverage (every indexed drive, not just the folders Windows
indexes) and freshness (live updates with the Ferrow service) without
opening the app first.

## Turn it on

1. Start Ferrow. With the Ferrow service installed (recommended, see
   [`docs/service.md`](service.md)), Start searches use the service's
   live index. Without it, they use the app's own index.
2. Open **Settings → Windows** and turn on
   **Power Windows Search with Ferrow**.
3. The line under the toggle shows the live state, for example
   "On: Start menu searches use the Ferrow service index."
4. Press the Windows key and type a file name. Matching files and
   folders now come from Ferrow.

Turning the toggle off takes effect on the next Start search. No restart
is needed in either direction.

## How it behaves

- Only file and folder results are Ferrow's. Apps, settings and web
  results still come from Windows.
- If Ferrow finds nothing for a query, or cannot answer in time,
  Windows answers exactly as before. A slow, missing or stopped Ferrow
  never blocks or empties the Start menu.
- Results are the first matches in name order (the app's richer sorting
  and ranking stay in the app for now).
- All queries stay on this PC. Nothing is uploaded, and no search text
  is logged.
- No administrator rights are used. The integration runs as your user,
  inside your own Start search process.

## Limits to know

- **The Ferrow app must be running** in service mode. If you close the
  app completely, Start searches fall back to Windows until you start
  Ferrow again. (With the app's own index — no service — small indexes
  keep answering even with the app closed.)
- This uses an unofficial mechanism: Microsoft offers no supported way
  to add local file results to Start search. A Windows update can break
  it at any time; the fallback is always plain Windows Search, and
  Settings → Windows shows the error. Details and trade-offs:
  [ADR 0006](decisions/0006-windows-search.md).
- 64-bit x86 Windows only for now. ARM64 Windows reports "not active"
  in Settings.
- Antivirus tools may look twice at this feature because it loads a
  Ferrow DLL into the shell's search process (`SearchHost.exe`). That
  loading is the feature; if your antivirus blocks it, Start simply
  stays native.

## Turn it off / remove it

- Toggle off in **Settings → Windows**: Start searches are native again
  immediately. The (now inert) DLL leaves memory when `SearchHost.exe`
  next restarts, at the latest at sign-out.
- Uninstalling Ferrow removes the installed DLL with the rest of the
  program. As with all Ferrow data, `%LOCALAPPDATA%\ferrow` is kept;
  delete that folder to also remove the staged DLL copy and markers
  under `%LOCALAPPDATA%\ferrow\winsearch`.

## Troubleshooting

- **"On: waiting to connect to Windows Search…"** — the shell search
  host has not been attached yet. Open the Start menu once and check
  again; attachment retries by itself and also follows Start host
  restarts while Ferrow runs.
- **"On, but not active: …"** — attachment failed; the message says
  why. Toggling off and on retries. After a Windows update this can
  mean the integration needs a Ferrow update.
- **Start shows no Ferrow results but Settings says On** — check that
  the file exists in Ferrow itself first (same search in the app). If
  the service refuses your account (standard accounts, see
  [`docs/service.md`](service.md)), Start falls back to Windows by
  design.
- **Everything is native again after closing Ferrow** — expected in
  service mode; see Limits.
