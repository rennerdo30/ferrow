# Builds the Windows setup: artifacts/installer/Ferrow-<version>-setup.exe.
# Build only: never installs Ferrow, never starts the setup, never changes PATH.
#
# Needs NSIS 3.13 (zlib licence, a build tool, not shipped): the official portable
# nsis-3.13.zip from https://sourceforge.net/projects/nsis/files/NSIS%203/3.13/
# SHA-256 BA63DFFC4410EE89193E1CB5A41989991BD77C61068DA17E3156D136B7B0B3D8.
# Extract it anywhere and pass -MakeNsis <folder>\makensis.exe, or set FERROW_MAKENSIS.
#
#   scripts\package.ps1 [-MakeNsis <makensis.exe>] [-BinDir <folder with release exes>]
# Without -BinDir it builds the release binaries first (cargo, -j 8).
[CmdletBinding()]
param(
    [string]$MakeNsis = $env:FERROW_MAKENSIS,
    [string]$BinDir
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$cargoToml = Get-Content -LiteralPath (Join-Path $root 'Cargo.toml') -Raw
if ($cargoToml -notmatch '(?m)^\[workspace\.package\][\s\S]*?^version\s*=\s*"([^"]+)"') { throw 'No workspace version in Cargo.toml' }
$version = $Matches[1]
if ($version -notmatch '^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z]+(?:\.[0-9A-Za-z]+)*)?$') { throw "Unexpected version: $version" }
$numeric = "$($Matches[1]).$($Matches[2]).$($Matches[3]).0"
if (!$MakeNsis -or !(Test-Path -LiteralPath $MakeNsis -PathType Leaf)) {
    throw "NSIS 3 compiler not found ('$MakeNsis'). Extract the pinned portable ZIP (see the top of this script) and pass -MakeNsis <path>\makensis.exe."
}
$MakeNsis = (Resolve-Path -LiteralPath $MakeNsis).Path
Write-Host "Ferrow $version from commit $(& git -C $root rev-parse HEAD) (includes uncommitted changes)"

function Invoke-Bounded([string]$File, [string[]]$Arguments, [int]$Seconds) {
    $info = [Diagnostics.ProcessStartInfo]::new($File)
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.WorkingDirectory = $root
    foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) }
    $process = [Diagnostics.Process]::Start($info)
    Write-Host "Started PID $($process.Id): $File (timeout $Seconds s)"
    $stdout = $process.StandardOutput.ReadToEndAsync()
    $stderr = $process.StandardError.ReadToEndAsync()
    try {
        if (!$process.WaitForExit($Seconds * 1000)) {
            $process.Kill($true) # only this invocation and its own children
            $process.WaitForExit()
            throw "$File timed out after $Seconds s"
        }
        Write-Host $stdout.GetAwaiter().GetResult()
        Write-Host $stderr.GetAwaiter().GetResult()
        if ($process.ExitCode) { throw "$File failed ($($process.ExitCode))" }
    } finally { $process.Dispose() }
}

$programs = 'ferrow-gui.exe', 'ferrow.exe', 'ferrowd.exe', 'ferrow-update.exe'
if (!$BinDir) {
    $target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root 'target' }
    $cargo = (Get-Command cargo -ErrorAction Stop).Source
    Invoke-Bounded $cargo @('build', '--release', '--locked', '-j', '8', '-p', 'ferrow-gui', '-p', 'ferrow', '-p', 'ferrowd', '-p', 'ferrow-update') 3600
    $BinDir = Join-Path $target 'release'
}
foreach ($program in $programs) {
    if (!(Test-Path -LiteralPath (Join-Path $BinDir $program) -PathType Leaf)) { throw "Missing $program in $BinDir" }
}

$out = Join-Path $root 'artifacts/installer'
New-Item -ItemType Directory -Force -Path $out | Out-Null
# A fresh stage per run; older stages stay for inspection.
$stageRoot = Join-Path $out ('stage-' + [guid]::NewGuid().ToString('N'))
$stage = Join-Path $stageRoot 'publish'
New-Item -ItemType Directory -Path $stage | Out-Null
foreach ($program in $programs) { Copy-Item -LiteralPath (Join-Path $BinDir $program) -Destination $stage }
Copy-Item -LiteralPath (Join-Path $root 'installer/art/ferrow.ico') -Destination $stage
Copy-Item -LiteralPath (Join-Path $root 'LICENSE') -Destination (Join-Path $stage 'LICENSE.txt')

# Exact payload list for uninstall, and a lock check for every file setup
# replaces. ferrow-update.exe is skipped: it runs setup during in-app updates,
# so setup moves it aside instead (MoveAsideRunning in ferrow.nsi).
function Quote-Nsis([string]$Value) { $Value.Replace('$', '$$').Replace('"', '$\"') }
$manifest = [Collections.Generic.List[string]]::new()
$preflight = [Collections.Generic.List[string]]::new()
foreach ($file in Get-ChildItem -LiteralPath $stage -File -Recurse) {
    $relative = Quote-Nsis ([IO.Path]::GetRelativePath($stage, $file.FullName))
    $manifest.Add('FileWrite $8 "F|' + $relative + '$\r$\n"')
    if ($relative -eq 'ferrow-update.exe') { continue }
    $preflight.Add('${If} ${FileExists} "$INSTDIR\' + $relative + '"')
    $preflight.Add('ClearErrors')
    $preflight.Add('FileOpen $7 "$INSTDIR\' + $relative + '" a')
    $preflight.Add('IfErrors locked')
    $preflight.Add('FileClose $7')
    $preflight.Add('${EndIf}')
}
foreach ($directory in Get-ChildItem -LiteralPath $stage -Directory -Recurse | Sort-Object { $_.FullName.Length } -Descending) {
    $relative = Quote-Nsis ([IO.Path]::GetRelativePath($stage, $directory.FullName))
    $manifest.Add('FileWrite $8 "D|' + $relative + '$\r$\n"')
}
$manifest | Set-Content -LiteralPath (Join-Path $stageRoot 'install-manifest.nsh') -Encoding utf8
$preflight | Set-Content -LiteralPath (Join-Path $stageRoot 'preflight-files.nsh') -Encoding utf8

$setup = Join-Path $out "Ferrow-$version-setup.exe"
Invoke-Bounded $MakeNsis @('-V3', "-DVERSION=$version", "-DVI_VERSION=$numeric", "-DSRC=$stage", "-DOUTFILE=$setup", (Join-Path $root 'installer/ferrow.nsi')) 300
$artifact = Get-Item -LiteralPath $setup
$hash = (Get-FileHash -LiteralPath $setup -Algorithm SHA256).Hash.ToLowerInvariant()
"$hash  $($artifact.Name)" | Set-Content -LiteralPath (Join-Path $out 'SHA256SUMS.txt') -Encoding ascii
Write-Host ('{0:N0} bytes  {1}' -f $artifact.Length, $artifact.FullName)
Write-Host "SHA-256 $hash"
