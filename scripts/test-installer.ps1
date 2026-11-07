# Installer lifecycle test: fresh install, upgrade with data kept, the updater's
# refusal paths, silent uninstall with data kept.
# DESTRUCTIVE: installs a LocalSystem service. Disposable, already elevated CI
# runners only; it refuses to run anywhere else and never requests elevation.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Setup,
    [Parameter(Mandatory)][string]$Version
)
$ErrorActionPreference = 'Stop'
if (!$env:CI -or !$env:GITHUB_ACTIONS) { throw 'Runs only on disposable CI runners (installs a LocalSystem service).' }
if (![Environment]::Is64BitProcess) { throw 'Use x64 PowerShell.' }
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
if (!([Security.Principal.WindowsPrincipal]::new($identity)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'CI must already be elevated.' }

$dir = Join-Path $env:ProgramFiles 'Ferrow'
$data = Join-Path $env:ProgramData 'Ferrow'
$key = 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Ferrow'
$shortcut = Join-Path ([Environment]::GetFolderPath('CommonPrograms')) 'Ferrow.lnk'
foreach ($path in $dir, $data, $key, $shortcut) {
    if (Test-Path -LiteralPath $path) { throw "Refusing to touch an existing installation or data: $path" }
}
if (Get-Service ferrowd -ErrorAction SilentlyContinue) { throw 'A ferrowd service already exists.' }

function Assert([bool]$Condition, [string]$Message) {
    if (!$Condition) { throw "FAILED: $Message" }
    Write-Host "ok: $Message"
}
function Invoke-Bounded([string]$File, [string[]]$Arguments, [int]$Seconds = 180) {
    $process = Start-Process -FilePath $File -ArgumentList $Arguments -PassThru -WindowStyle Hidden
    Write-Host "Started PID $($process.Id): $File $Arguments (timeout $Seconds s)"
    try {
        if (!$process.WaitForExit($Seconds * 1000)) { $process.Kill($true); $process.WaitForExit(); throw "$File timed out" }
        return $process.ExitCode
    } finally { $process.Dispose() }
}

# Fresh install.
Assert ((Invoke-Bounded $Setup @('/S')) -eq 0) 'silent install exits 0'
foreach ($file in 'ferrow-gui.exe', 'ferrow.exe', 'ferrowd.exe', 'ferrow-update.exe', 'ferrow.ico', 'LICENSE.txt', 'uninstall.exe', 'install-manifest.txt', 'service/ferrowd.exe') {
    Assert (Test-Path -LiteralPath (Join-Path $dir $file)) "installed $file"
}
$service = Get-CimInstance Win32_Service -Filter "Name='ferrowd'"
Assert ($service.State -eq 'Running') 'service is running'
Assert ($service.StartName -eq 'LocalSystem') 'service runs as LocalSystem'
Assert ($service.StartMode -eq 'Auto') 'service starts automatically'
Assert ($service.PathName.StartsWith('"' + (Join-Path $dir 'service\ferrowd.exe') + '"')) "service binary is the protected copy, quoted ($($service.PathName))"
$failure = (& sc.exe qfailure ferrowd) -join "`n"
Assert (([regex]::Matches($failure, 'RESTART')).Count -ge 3) 'service restarts after failures'
$serviceAcl = Get-Acl -LiteralPath (Join-Path $dir 'service')
Assert ($serviceAcl.AreAccessRulesProtected) 'service folder does not inherit Program Files permissions'
$users = $serviceAcl.Access | Where-Object { $_.IdentityReference -match 'Users|Everyone|Authenticated' }
Assert (!$users) 'ordinary users have no access to the service folder'
$dataAcl = Get-Acl -LiteralPath $data
Assert ($dataAcl.AreAccessRulesProtected -and $dataAcl.GetOwner([Security.Principal.SecurityIdentifier]).Value -eq 'S-1-5-18') 'service data folder is SYSTEM-owned and protected'
$entry = Get-ItemProperty -LiteralPath $key
Assert ($entry.DisplayName -eq 'Ferrow' -and $entry.DisplayVersion -eq $Version) 'Installed apps name and version'
Assert ($entry.Publisher -eq 'rennerdo30' -and $entry.InstallLocation -eq $dir) 'Installed apps publisher and location'
Assert ($entry.QuietUninstallString -eq ('"' + (Join-Path $dir 'uninstall.exe') + '" /S')) 'silent uninstall command'
Assert ($entry.EstimatedSize -gt 0 -and $entry.NoModify -eq 1 -and $entry.NoRepair -eq 1) 'Installed apps size and flags'
$shell = New-Object -ComObject WScript.Shell
Assert ((Test-Path -LiteralPath $shortcut) -and $shell.CreateShortcut($shortcut).TargetPath -eq (Join-Path $dir 'ferrow-gui.exe')) 'Start menu entry opens the app'
$status = (& (Join-Path $dir 'ferrow.exe') status) -join "`n"
Assert ($status -match 'mode: service') "the CLI reaches the installed service ($status)"

# Upgrade over the existing install: service restarted, service data kept.
$sentinel = Join-Path $data 'ci-preserve.txt'
Set-Content -LiteralPath $sentinel -Value 'keep'
Assert ((Invoke-Bounded $Setup @('/S')) -eq 0) 'silent upgrade exits 0'
Assert ((Get-Service ferrowd).Status -eq 'Running') 'upgrade restarted the service'
Assert ((Get-Content -LiteralPath $sentinel -Raw).Trim() -eq 'keep') 'upgrade keeps the service data'
$lines = Get-Content -LiteralPath (Join-Path $dir 'install-manifest.txt')
Assert ($lines -contains 'F|ferrow-gui.exe') 'manifest lists the payload'

# Updater refusals: a bad argument, and a release that does not exist. Neither
# may leave a staging folder behind or run anything.
$updater = Join-Path $dir 'ferrow-update.exe'
Assert ((Invoke-Bounded $updater @('--version', 'not-a-version') 30) -eq 2) 'updater rejects an invalid version'
Assert ((Invoke-Bounded $updater @('--version', '0.0.1-ci.404') 120) -eq 4) 'updater reports a missing release as a download failure'
Assert (!(Get-ChildItem -LiteralPath $env:ProgramFiles -Directory -Filter 'Ferrow-update-*')) 'updater removed its staging folder'

# Upgrade without the service.
Assert ((Invoke-Bounded $Setup @('/S', '/NOSERVICE')) -eq 0) 'upgrade with /NOSERVICE exits 0'
Assert (!(Get-Service ferrowd -ErrorAction SilentlyContinue)) '/NOSERVICE removes the service'
Assert ((Invoke-Bounded $Setup @('/S')) -eq 0) 'plain upgrade exits 0'
Assert (!(Get-Service ferrowd -ErrorAction SilentlyContinue)) 'a plain upgrade keeps the service off'
Assert ((Invoke-Bounded $Setup @('/S', '/SERVICE')) -eq 0) 'upgrade with /SERVICE exits 0'
Assert ((Get-Service ferrowd).Status -eq 'Running') 'service is back'

# Silent uninstall. NSIS copies the uninstaller to TEMP and returns early;
# wait for its results instead of terminating anything.
Invoke-Bounded (Join-Path $dir 'uninstall.exe') @('/S') | Out-Null
$deadline = (Get-Date).AddSeconds(120)
while (((Test-Path -LiteralPath $key) -or (Test-Path -LiteralPath (Join-Path $dir 'ferrow-gui.exe'))) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 250 }
Assert (!(Get-Service ferrowd -ErrorAction SilentlyContinue)) 'service removed'
Assert (!(Test-Path -LiteralPath $key)) 'Installed apps entry removed'
Assert (!(Test-Path -LiteralPath $shortcut)) 'Start menu entry removed'
Assert (!(Test-Path -LiteralPath (Join-Path $dir 'service\ferrowd.exe'))) 'service program removed'
$deadline = (Get-Date).AddSeconds(30)
while ((Test-Path -LiteralPath $dir) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 250 }
Assert (!(Test-Path -LiteralPath $dir)) 'install folder removed'
Assert ((Get-Content -LiteralPath $sentinel -Raw).Trim() -eq 'keep') 'service data survives uninstall'
Write-Host 'Installer lifecycle passed.'
exit 0
