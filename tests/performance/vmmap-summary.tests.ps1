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
MALLOC ZONE                         SIZE       SIZE
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

$modernResident = Invoke-VmmapSummaryParser @'
Physical footprint:             96.0M
                                VIRTUAL   RESIDENT    DIRTY
MALLOC ZONE                         SIZE       SIZE     SIZE
MALLOC guard page                   4.0K       0.0K     0.0K
MALLOC_NANO                        16.0M       8.0M     8.0M
MALLOC_SMALL                       32.0M      12.5M    12.5M
'@
$modernResidentResult = $modernResident | ConvertFrom-Json
if (
    $modernResidentResult.physical_footprint_kib -ne 98304 -or
    $modernResidentResult.malloc_resident_kib -ne 20992 -or
    $modernResidentResult.malloc_resident_parser_status -cne "parsed"
) {
    throw "Modern vmmap MALLOC subregions were parsed incorrectly."
}

$aggregateWithSubregions = Invoke-VmmapSummaryParser @'
Physical footprint:             128.5M
                                VIRTUAL   RESIDENT
MALLOC                           64.0M      32.0M
MALLOC_SMALL                     32.0M      12.5M
'@
$aggregateWithSubregionsResult = $aggregateWithSubregions | ConvertFrom-Json
if ($aggregateWithSubregionsResult.malloc_resident_kib -ne 32768) {
    throw "Aggregate MALLOC row must take precedence over subregions."
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
