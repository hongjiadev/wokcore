$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$parser = Join-Path $PSScriptRoot "parse-vmmap-summary.py"
$python = (Get-Command python -ErrorAction Stop).Source

function Invoke-VmmapSummaryParser {
    param([Parameter(Mandatory = $true)][string] $Fixture)

    $Fixture | & $python $parser
    if ($LASTEXITCODE -ne 0) {
        throw "vmmap summary parser rejected a valid fixture."
    }
}

$legacy = Invoke-VmmapSummaryParser @'
Physical footprint:             128.5M
                                VIRTUAL   RESIDENT
MALLOC                           64.0M      32.0M
'@
$legacyResult = $legacy | ConvertFrom-Json
if (
    $legacyResult.physical_footprint_kib -ne 131584 -or
    $legacyResult.malloc_resident_kib -ne 32768 -or
    $legacyResult.malloc_resident_parser_status -cne "parsed"
) {
    throw "Legacy vmmap summary was parsed incorrectly."
}

$modern = Invoke-VmmapSummaryParser @'
Physical footprint:             1024K
                                VIRTUAL   REGION
MALLOC                           64.0M          3
'@
$modernResult = $modern | ConvertFrom-Json
if (
    $modernResult.physical_footprint_kib -ne 1024 -or
    $null -ne $modernResult.malloc_resident_kib -or
    $modernResult.malloc_resident_parser_status -cne "unavailable"
) {
    throw "Modern vmmap summary should retain the footprint and mark MALLOC resident unavailable."
}

$previousErrorActionPreference = $ErrorActionPreference
$ErrorActionPreference = "Continue"
$invalid = "MALLOC 64.0M 32.0M" | & $python $parser 2>&1
$invalidExitCode = $LASTEXITCODE
$ErrorActionPreference = $previousErrorActionPreference
if ($invalidExitCode -eq 0 -or ($invalid -join "`n") -notmatch "physical footprint") {
    throw "Missing physical footprint should be a clear parser failure."
}

Write-Output "vmmap summary parser tests passed"
