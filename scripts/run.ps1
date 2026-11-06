# Build and run Ferrow. Usage:
#   ./scripts/run.ps1 [-Release] [ferrow arguments...]
# Examples:
#   ./scripts/run.ps1
#   ./scripts/run.ps1 -Release "*.rs"
param(
    [switch]$Release,
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$FerrowArgs
)
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent $PSScriptRoot)

$cargoArgs = @('run', '--package', 'ferrow')
if ($Release) { $cargoArgs += '--release' }
$cargoArgs += '--'
if ($FerrowArgs) { $cargoArgs += $FerrowArgs }

& cargo @cargoArgs
exit $LASTEXITCODE
