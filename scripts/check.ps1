# Run the same checks as CI: format, clippy, tests.
# Usage: ./scripts/check.ps1
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent $PSScriptRoot)

foreach ($step in @(
        @('fmt', '--all', '--check'),
        @('clippy', '--workspace', '--all-targets', '--locked', '--', '-D', 'warnings'),
        @('test', '--workspace', '--locked')
    )) {
    Write-Host "> cargo $($step -join ' ')"
    & cargo @step
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}
Write-Host 'All checks passed.'
