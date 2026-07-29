$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

# These checks catch omitted or duplicate targets, leaked Rust vendor segments,
# incorrect v1 bridge membership, and missing public or legacy payloads.
Import-Module (Join-Path $PSScriptRoot "WokCore.ReleaseContract.psm1") -Force

$contracts = @(Get-WokCoreTargetContracts -Version "0.1.1")
if ($contracts.Count -ne 6) { throw "Expected six WokCore targets." }
if (@($contracts.Target | Sort-Object -Unique).Count -ne 6) {
    throw "WokCore targets must be unique."
}
if (@($contracts | Where-Object FriendlyPortableName -Match "unknown").Count -ne 0) {
    throw "Public WokCore names must not expose the Rust vendor segment."
}
if (@($contracts | Where-Object LegacyV1).Count -ne 5) {
    throw "Exactly five contracts must remain in the v1 bridge."
}
$payloads = @(Get-WokCorePayloadNames -Version "0.1.1" -IncludeLegacyV1)
if ($payloads.Count -ne 19) { throw "Expected 14 public and five legacy payloads." }

Write-Output "release contract tests passed: six targets and 19 payloads"
