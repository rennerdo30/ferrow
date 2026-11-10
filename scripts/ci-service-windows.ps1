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
$ordinaryUser = $null
try {
    $tree = Join-Path $env:RUNNER_TEMP 'svc-tree'
    New-Item -ItemType Directory -Force $tree | Out-Null
    Set-Content (Join-Path $tree 'ci-needle.txt') 'x'
    # A quiet folder root: the idle-CPU check below must not see live
    # updates reacting to the runner's own disk activity on whole drives.
    Invoke-Bounded (Join-Path $bin 'ferrow.exe') @('service', 'install', $tree) | Write-Host
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
    $stateAcl = Get-Acl 'C:\ProgramData\Ferrow'
    if ($stateAcl.GetOwner([Security.Principal.SecurityIdentifier]).Value -ne 'S-1-5-18' -or -not $stateAcl.AreAccessRulesProtected) { throw 'state owner/inheritance is not protected' }
    $ordinaryUser = 'ferrow-ci-' + [Guid]::NewGuid().ToString('N').Substring(0, 8)
    $password = ConvertTo-SecureString ([Guid]::NewGuid().ToString('N') + '!aA1') -AsPlainText -Force
    $account = New-LocalUser -Name $ordinaryUser -Password $password
    Add-LocalGroupMember -SID 'S-1-5-32-545' -Member $account
    $credential = [Management.Automation.PSCredential]::new("$env:COMPUTERNAME\$ordinaryUser", $password)
    $probe = Join-Path $PSScriptRoot 'ci-ordinary-user.ps1'
    $ordinaryLog = Join-Path $env:RUNNER_TEMP 'ferrow-ordinary-user.log'
    $ordinaryError = Join-Path $env:RUNNER_TEMP 'ferrow-ordinary-user.err'
    $ordinary = Start-Process -FilePath (Join-Path $PSHOME 'pwsh.exe') -Credential $credential -WindowStyle Hidden -PassThru -RedirectStandardOutput $ordinaryLog -RedirectStandardError $ordinaryError -ArgumentList @('-NoProfile', '-NonInteractive', '-File', "`"$probe`"", '-Cli', "`"$bin\ferrow.exe`"")
    Write-Host "Started ordinary-user probe PID $($ordinary.Id)"
    try {
        if (-not $ordinary.WaitForExit(15000)) { $ordinary.Kill($true); $ordinary.WaitForExit(); throw 'ordinary-user probe timed out' }
        if ($ordinary.ExitCode -ne 0) { throw "ordinary-user ACL/refusal probe failed: $(Get-Content $ordinaryError -Raw)" }
    } finally { $ordinary.Dispose() }
    $hits = Invoke-Bounded (Join-Path $bin 'ferrow.exe') @('search', 'ci-needle')
    if ($hits -notmatch 'ci-needle.txt') { throw "service missed query: $hits" }
    $cpu1 = $process.TotalProcessorTime.TotalSeconds
    if (-not ('FerrowClock' -as [type])) {
        Add-Type 'using System.Runtime.InteropServices; public static class FerrowClock { [DllImport("kernel32.dll", SetLastError=true)] public static extern bool GetSystemTimeAdjustment(out uint adjustment, out uint increment, out bool disabled); }'
    }
    [uint32]$adjustment = 0
    [uint32]$increment = 0
    [bool]$disabled = $false
    if (-not [FerrowClock]::GetSystemTimeAdjustment([ref]$adjustment, [ref]$increment, [ref]$disabled)) { throw 'cannot obtain CPU accounting resolution' }
    $resolution = $increment / 10000000.0
    Start-Sleep -Seconds 3
    $process.Refresh()
    $delta = $process.TotalProcessorTime.TotalSeconds - $cpu1
    # Two accounting quanta cover rounding at both window boundaries.
    Write-Host "PID $servicePid idle CPU=$delta seconds; resolution=$resolution seconds; budget=$($resolution * 2) seconds"
    if ($delta -gt ($resolution * 2)) { throw "idle CPU delta: $delta" }
} catch {
    if (-not $expectedFailure) { throw }
    Write-Host 'Observed injected failure; finally must still remove the service.'
} finally {
    try { Cleanup-Service } finally { if ($ordinaryUser) { Remove-LocalUser -Name $ordinaryUser } }
}
