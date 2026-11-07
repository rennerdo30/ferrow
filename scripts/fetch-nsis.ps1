# CI helper: downloads the pinned portable NSIS 3.13 into -Destination, checks its
# SHA-256 and prints the makensis.exe path. Nothing is installed; PATH is unchanged.
[CmdletBinding()]
param([Parameter(Mandatory)][string]$Destination)
$ErrorActionPreference = 'Stop'
$url = 'https://downloads.sourceforge.net/project/nsis/NSIS%203/3.13/nsis-3.13.zip'
$expected = 'BA63DFFC4410EE89193E1CB5A41989991BD77C61068DA17E3156D136B7B0B3D8'
New-Item -ItemType Directory -Force -Path $Destination | Out-Null
$zip = Join-Path $Destination 'nsis-3.13.zip'
& "$env:SystemRoot\System32\curl.exe" --fail --silent --show-error --location --retry 4 --retry-all-errors --max-time 300 --output $zip $url
if ($LASTEXITCODE) { throw "NSIS download failed ($LASTEXITCODE)" }
$actual = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash
if ($actual -ne $expected) { throw "NSIS checksum mismatch: $actual" }
Expand-Archive -LiteralPath $zip -DestinationPath $Destination -Force
$makensis = Join-Path $Destination 'nsis-3.13/makensis.exe'
if (!(Test-Path -LiteralPath $makensis)) { throw 'makensis.exe missing from the archive' }
Write-Output $makensis
