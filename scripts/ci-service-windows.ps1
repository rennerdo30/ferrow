param([switch]$CleanupOnly, [switch]$FailAfterStart)
$ErrorActionPreference = 'Stop'
$bin = Join-Path $PSScriptRoot '../target/debug'
$receipt = Join-Path $env:RUNNER_TEMP 'ferrow-service-pid.txt'

function Invoke-Bounded([string]$Exe, [string[]]$Arguments) {
    $info = [Diagnostics.ProcessStartInfo]::new($Exe)
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) }
    $child = [Diagnostics.Process]::Start($info)
    Write-Host "Started command PID $($child.Id): $Exe"
    $stdout = $child.StandardOutput.ReadToEndAsync()
    $stderr = $child.StandardError.ReadToEndAsync()
    try {
        if (-not $child.WaitForExit(25000)) { $child.Kill($true); $child.WaitForExit(); throw "command timeout: $Exe" }
        $output = $stdout.GetAwaiter().GetResult()
        $errors = $stderr.GetAwaiter().GetResult()
        if ($child.ExitCode -ne 0) { throw "$Exe exited $($child.ExitCode): $errors $output" }
        return $output
    } finally { $child.Dispose() }
}

function Cleanup-Service {
    $service = Get-CimInstance Win32_Service -Filter "Name='ferrowd'"
    if ($service) {
        if ($service.State -ne 'Stopped') { Invoke-Bounded sc.exe @('stop', 'ferrowd') | Write-Host }
        if (Test-Path $receipt) {
            $recorded = [int](Get-Content $receipt)
            $process = Get-Process -Id $recorded -ErrorAction SilentlyContinue
            if ($process -and -not $process.WaitForExit(20000)) { throw "recorded service PID $recorded did not exit" }
        }
        Invoke-Bounded (Join-Path $bin 'ferrow.exe') @('service', 'uninstall') | Write-Host
        if (Get-CimInstance Win32_Service -Filter "Name='ferrowd'") { throw 'registration remains after uninstall' }
    }
}

if ($CleanupOnly) { Cleanup-Service; exit 0 }
$expectedFailure = $false
try {
    $tree = Join-Path $env:RUNNER_TEMP 'svc-tree'
    New-Item -ItemType Directory -Force $tree | Out-Null
    Set-Content (Join-Path $tree 'ci-needle.txt') 'x'
    Invoke-Bounded (Join-Path $bin 'ferrow.exe') @('service', 'install') | Write-Host
    Invoke-Bounded (Join-Path $bin 'ferrow.exe') @('index', $tree, '--out', 'C:\ProgramData\Ferrow\index.bin') | Write-Host
    Invoke-Bounded sc.exe @('start', 'ferrowd') | Write-Host
    $service = Get-CimInstance Win32_Service -Filter "Name='ferrowd'"
    if (-not $service.ProcessId) { throw 'SCM supplied no service PID' }
    $servicePid = [int]$service.ProcessId
    Set-Content $receipt $servicePid
    $process = Get-Process -Id $servicePid
    if ($FailAfterStart) { $expectedFailure = $true; throw 'injected failure after start' }
    $ready = $false
    foreach ($attempt in 1..30) {
        $status = Invoke-Bounded (Join-Path $bin 'ferrow.exe') @('status')
        if ($status -match 'mode: service') { $ready = $true; break }
        Start-Sleep -Milliseconds 100
    }
    if (-not $ready) { throw 'service did not become ready' }
    $hits = Invoke-Bounded (Join-Path $bin 'ferrow.exe') @('search', 'ci-needle')
    if ($hits -notmatch 'ci-needle.txt') { throw "service missed query: $hits" }
    $cpu1 = $process.TotalProcessorTime.TotalSeconds
    Start-Sleep -Seconds 3
    $process.Refresh()
    $delta = $process.TotalProcessorTime.TotalSeconds - $cpu1
    if ($delta -gt 0.5) { throw "idle CPU delta: $delta" }
} catch {
    if (-not $expectedFailure) { throw }
    Write-Host 'Observed injected failure; finally must still remove the service.'
} finally { Cleanup-Service }
