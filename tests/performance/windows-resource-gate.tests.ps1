$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$gate = Join-Path $PSScriptRoot "windows-resource-gate.ps1"
. $gate -LibraryOnly

function New-SyntheticSample {
    param(
        [uint32] $TargetProcessId = 42,
        [uint64] $IdentityToken = 77,
        [uint64] $ObservedMs = 1000,
        [uint64] $PrivateWorkingSetBytes = 33554432,
        [uint64] $PeakPrivateBytes = 50331648,
        [uint64] $ReadBytes = 100,
        [uint64] $WriteBytes = 200,
        [uint32] $HandleCount = 30,
        [uint32] $ThreadCount = 6,
        [uint64] $LifetimeMs = 900
    )
    [PSCustomObject] @{
        Pid = $TargetProcessId
        IdentityToken = $IdentityToken
        ObservedMs = $ObservedMs
        PrivateWorkingSetBytes = $PrivateWorkingSetBytes
        PeakPrivateBytes = $PeakPrivateBytes
        ReadBytes = $ReadBytes
        WriteBytes = $WriteBytes
        HandleCount = $HandleCount
        ThreadCount = $ThreadCount
        LifetimeMs = $LifetimeMs
    }
}

$samples = @(
    (New-SyntheticSample)
    (New-SyntheticSample `
        -ObservedMs 2000 `
        -PrivateWorkingSetBytes 67108864 `
        -PeakPrivateBytes 83886080 `
        -ReadBytes 2100 `
        -WriteBytes 4296 `
        -HandleCount 44 `
        -ThreadCount 9 `
        -LifetimeMs 1900)
)
$evidence = ConvertTo-WokCorePhaseEvidence `
    -Samples $samples `
    -PhaseName "active" `
    -ExecutableName "E:\Projects\wokcore\target\release\wokcore.exe"
if ($evidence.PeakPrivateWorkingSetBytes -ne 67108864) {
    throw "Peak private working set aggregation failed."
}
if ($evidence.PeakPrivateBytes -ne 83886080) {
    throw "Peak private byte aggregation failed."
}
if ($evidence.ReadBytes -ne 2000 -or $evidence.WriteBytes -ne 4096) {
    throw "I/O delta aggregation failed."
}
if ($evidence.PeakHandleCount -ne 44 -or $evidence.PeakThreadCount -ne 9) {
    throw "Handle or thread aggregation failed."
}
if ($evidence.WriteBytesPerSecond -ne 4096) {
    throw "Write rate aggregation failed."
}

$json = $evidence | ConvertTo-Json -Depth 4 -Compress
$csv = ($evidence | ConvertTo-Csv -NoTypeInformation) -join [Environment]::NewLine
if ($json.Length -ge 4096) {
    throw "Evidence exceeded its bounded aggregate size."
}
if ($csv.Length -ge 4096) {
    throw "CSV evidence exceeded its bounded aggregate size."
}
foreach ($forbidden in @(
    "E:\Projects",
    "command",
    "environment",
    "username",
    "payload",
    "prompt"
)) {
    if (
        $json.IndexOf($forbidden, [StringComparison]::OrdinalIgnoreCase) -ge 0 -or
        $csv.IndexOf($forbidden, [StringComparison]::OrdinalIgnoreCase) -ge 0
    ) {
        throw "Evidence contains forbidden content."
    }
}

foreach ($phaseName in @("warm_idle", "active", "recovery")) {
    $phaseEvidence = ConvertTo-WokCorePhaseEvidence `
        -Samples $samples `
        -PhaseName $phaseName `
        -ExecutableName "wokcore.exe"
    if ($phaseEvidence.Phase -ne $phaseName) {
        throw "Resource phase separation failed."
    }
}

$failed = $false
try {
    Test-WokCoreProcessSampleSeries -Samples @(
        (New-SyntheticSample)
        (New-SyntheticSample -IdentityToken 78 -ObservedMs 2000)
    ) | Out-Null
} catch {
    $failed = $true
}
if (-not $failed) {
    throw "Process restart was not rejected."
}

$failed = $false
try {
    Test-WokCoreProcessSampleSeries -Samples @(
        (New-SyntheticSample)
        (New-SyntheticSample -ObservedMs 2000 -ReadBytes 99)
    ) | Out-Null
} catch {
    $failed = $true
}
if (-not $failed) {
    throw "Counter rollback was not rejected."
}

$self = Get-Process -Id $PID
$real = Get-WokCoreProcessSample `
    -TargetProcessId $PID `
    -ExactExecutablePath $self.Path
if (
    $real.PrivateWorkingSetBytes -eq 0 -or
    $real.PeakPrivateBytes -eq 0 -or
    $real.HandleCount -eq 0 -or
    $real.ThreadCount -eq 0 -or
    $real.LifetimeMs -eq 0
) {
    throw "Live exact-PID process sampling returned an invalid zero value."
}

$failed = $false
try {
    Get-WokCoreProcessSample `
        -TargetProcessId $PID `
        -ExactExecutablePath (Join-Path ([IO.Path]::GetDirectoryName($self.Path)) "wokcore.exe") |
        Out-Null
} catch {
    $failed = $true
}
if (-not $failed) {
    throw "Exact executable path mismatch was not rejected."
}

Write-Output "windows resource gate tests passed"
