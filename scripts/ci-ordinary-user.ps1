param([Parameter(Mandatory)][string]$Cli)
$ErrorActionPreference = 'Stop'
$index = 'C:\ProgramData\Ferrow\index.bin'
$readable = $false
try { $null = [IO.File]::ReadAllBytes($index); $readable = $true } catch [UnauthorizedAccessException] {}
if ($readable) { throw 'ordinary user can read the privileged index' }
$writable = $false
try { $stream = [IO.File]::Open($index, 'Open', 'Write', 'ReadWrite'); $stream.Dispose(); $writable = $true } catch [UnauthorizedAccessException] {}
if ($writable) { throw 'ordinary user can write the privileged index' }
$status = & $Cli status 2>&1 | Out-String
if ($LASTEXITCODE -ne 0 -or $status -notmatch 'Unauthorized' -or $status -match 'entries:') {
    throw "ordinary user did not receive an index-free refusal: $status"
}
