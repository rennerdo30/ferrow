# Run the same checks as CI: format, clippy, tests.
# Usage: ./scripts/check.ps1 [-Package ferrow-query,ferrow]
param([string[]]$Package = @())
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent $PSScriptRoot)

$tests = if ($Package.Count) {
    @('test', '--locked', '-j', '8') + @($Package | ForEach-Object { '-p'; $_ })
} else { @('test', '--workspace', '--locked', '-j', '8') }
foreach ($step in @(
        @('fmt', '--all', '--check'),
        @('clippy', '--workspace', '--all-targets', '--locked', '-j', '8', '--', '-D', 'warnings'),
        $tests
    )) {
    Write-Host "> cargo $($step -join ' ')"
    & cargo @step
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}
Write-Host 'All checks passed.'
