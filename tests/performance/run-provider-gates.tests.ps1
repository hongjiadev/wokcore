$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$runner = Join-Path $PSScriptRoot "run-provider-gates.ps1"
. $runner -LibraryOnly

function New-PhaseEvidence {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Phase,
        [uint64] $PeakPrivateWorkingSetBytes = 1,
        [uint64] $WriteBytesPerSecond = 0,
        [uint64] $InitialPrivateWorkingSetBytes = 100,
        [uint64] $FinalPrivateWorkingSetBytes = 100,
        [uint32] $InitialHandleCount = 20,
        [uint32] $FinalHandleCount = 20,
        [uint32] $InitialThreadCount = 4,
        [uint32] $FinalThreadCount = 4,
        [uint32] $MaxConsecutivePrivateWorkingSetIncreases = 0,
        [uint32] $Samples = 10
    )
    [PSCustomObject] @{
        SchemaVersion = 1
        Phase = $Phase
        Samples = $Samples
        PeakPrivateWorkingSetBytes = $PeakPrivateWorkingSetBytes
        WriteBytesPerSecond = $WriteBytesPerSecond
        InitialPrivateWorkingSetBytes = $InitialPrivateWorkingSetBytes
        FinalPrivateWorkingSetBytes = $FinalPrivateWorkingSetBytes
        InitialHandleCount = $InitialHandleCount
        FinalHandleCount = $FinalHandleCount
        InitialThreadCount = $InitialThreadCount
        FinalThreadCount = $FinalThreadCount
        MaxConsecutivePrivateWorkingSetIncreases =
            $MaxConsecutivePrivateWorkingSetIncreases
    }
}

function New-LoadReport {
    param(
        [Parameter(Mandatory = $true)]
        [uint32] $Concurrency,
        [uint64] $Started = $Concurrency,
        [uint32] $Active = 0,
        [uint32] $PeakActive = $Concurrency,
        [uint64] $Completed = $Concurrency,
        [uint64] $Cancelled = 0,
        [uint64] $Errors = 0
    )
    [PSCustomObject] @{
        configured_concurrency = $Concurrency
        started = $Started
        active = $Active
        peak_active = $PeakActive
        completed = $Completed
        cancelled = $Cancelled
        errors = $Errors
    }
}

$thresholdPath = Join-Path $PSScriptRoot "provider-gates.toml"
$thresholds = Import-WokCoreProviderGateThresholds -Path $thresholdPath
if (
    $thresholds.WarmIdlePrivateWorkingSetBytes -ne 67108864 -or
    $thresholds.Standard500PrivateWorkingSetBytes -ne 536870912 -or
    $thresholds.RecoveryMultiplierMilli -ne 1500 -or
    $thresholds.Long500WriteBytesPerSecond -ne 131072 -or
    $thresholds.ObservationConcurrency -ne 1000
) {
    throw "Provider gate threshold parsing failed."
}

$warm = New-PhaseEvidence `
    -Phase "warm_idle" `
    -PeakPrivateWorkingSetBytes 67108864
$standard = New-PhaseEvidence `
    -Phase "standard_500" `
    -PeakPrivateWorkingSetBytes 536870912
$long = New-PhaseEvidence `
    -Phase "long_500" `
    -WriteBytesPerSecond 131072
$recovery = New-PhaseEvidence `
    -Phase "recovery" `
    -PeakPrivateWorkingSetBytes 100663296
$observation = New-PhaseEvidence `
    -Phase "observation_1000" `
    -InitialPrivateWorkingSetBytes 100000000 `
    -FinalPrivateWorkingSetBytes 125000000 `
    -InitialHandleCount 20 `
    -FinalHandleCount 84 `
    -InitialThreadCount 4 `
    -FinalThreadCount 12 `
    -MaxConsecutivePrivateWorkingSetIncreases 7
$standardLoad = New-LoadReport -Concurrency 500
$longLoad = New-LoadReport -Concurrency 500
$observationLoad = New-LoadReport -Concurrency 1000

function Invoke-SyntheticGate {
    param(
        [object] $WarmIdle = $warm,
        [object] $Standard500 = $standard,
        [object] $Long500 = $long,
        [object] $Recovery = $recovery,
        [object] $Observation1000 = $observation,
        [object] $StandardLoad = $standardLoad,
        [object] $LongLoad = $longLoad,
        [object] $ObservationLoad = $observationLoad
    )
    Test-WokCoreProviderGateEvidence `
        -Thresholds $thresholds `
        -WarmIdle $WarmIdle `
        -Standard500 $Standard500 `
        -Long500 $Long500 `
        -Recovery $Recovery `
        -Observation1000 $Observation1000 `
        -StandardLoad $StandardLoad `
        -LongLoad $LongLoad `
        -ObservationLoad $ObservationLoad
}

$passing = Invoke-SyntheticGate
if (-not $passing.Passed -or $passing.Failures.Count -ne 0) {
    throw "Exact threshold boundaries did not pass."
}

$cases = @(
    @{
        Code = "warm_idle_memory"
        Arguments = @{
            WarmIdle = New-PhaseEvidence `
                -Phase "warm_idle" `
                -PeakPrivateWorkingSetBytes 67108865
        }
    },
    @{
        Code = "standard_500_memory"
        Arguments = @{
            Standard500 = New-PhaseEvidence `
                -Phase "standard_500" `
                -PeakPrivateWorkingSetBytes 536870913
        }
    },
    @{
        Code = "recovery_memory"
        Arguments = @{
            Recovery = New-PhaseEvidence `
                -Phase "recovery" `
                -PeakPrivateWorkingSetBytes 100663297
        }
    },
    @{
        Code = "long_500_write_rate"
        Arguments = @{
            Long500 = New-PhaseEvidence `
                -Phase "long_500" `
                -WriteBytesPerSecond 131073
        }
    },
    @{
        Code = "observation_peak_active"
        Arguments = @{
            ObservationLoad = New-LoadReport -Concurrency 1000 -PeakActive 999
        }
    },
    @{
        Code = "observation_incomplete"
        Arguments = @{
            ObservationLoad = New-LoadReport `
                -Concurrency 1000 `
                -Started 1000 `
                -Active 1 `
                -Completed 999
        }
    },
    @{
        Code = "observation_errors"
        Arguments = @{
            ObservationLoad = New-LoadReport `
                -Concurrency 1000 `
                -Completed 999 `
                -Errors 1
        }
    },
    @{
        Code = "observation_handle_growth"
        Arguments = @{
            Observation1000 = New-PhaseEvidence `
                -Phase "observation_1000" `
                -InitialHandleCount 20 `
                -FinalHandleCount 85
        }
    },
    @{
        Code = "observation_thread_growth"
        Arguments = @{
            Observation1000 = New-PhaseEvidence `
                -Phase "observation_1000" `
                -InitialThreadCount 4 `
                -FinalThreadCount 13
        }
    },
    @{
        Code = "observation_monotonic_growth"
        Arguments = @{
            Observation1000 = New-PhaseEvidence `
                -Phase "observation_1000" `
                -InitialPrivateWorkingSetBytes 100000000 `
                -FinalPrivateWorkingSetBytes 125000001 `
                -MaxConsecutivePrivateWorkingSetIncreases 8
        }
    },
    @{
        Code = "standard_500_errors"
        Arguments = @{
            StandardLoad = New-LoadReport -Concurrency 500 -Completed 499 -Errors 1
        }
    }
)

foreach ($case in $cases) {
    $arguments = $case.Arguments
    $result = Invoke-SyntheticGate @arguments
    if ($result.Passed -or $case.Code -notin $result.Failures) {
        throw "Expected provider gate failure was not reported: $($case.Code)"
    }
}

$missing = [PSCustomObject] @{
    SchemaVersion = 1
    Phase = "warm_idle"
    Samples = 10
}
$missingResult = Invoke-SyntheticGate -WarmIdle $missing
if ($missingResult.Passed -or "ambiguous_evidence" -notin $missingResult.Failures) {
    throw "Missing evidence did not fail closed."
}

$wrongPhase = New-PhaseEvidence -Phase "active"
$wrongPhaseResult = Invoke-SyntheticGate -WarmIdle $wrongPhase
if ($wrongPhaseResult.Passed -or "ambiguous_evidence" -notin $wrongPhaseResult.Failures) {
    throw "Ambiguous phase evidence did not fail closed."
}

$runnerSource = [IO.File]::ReadAllText($runner, [Text.Encoding]::UTF8)
foreach ($required in @(
    'wokcore.exe',
    'wokcore-provider-sim.exe',
    'wokcore-loadgen.exe',
    '$env:APPDATA = $roaming',
    '$env:LOCALAPPDATA = $local',
    '$env:USERPROFILE = $isolatedHome',
    '$env:HOME = $isolatedHome',
    'catalog_id = "ollama"',
    'endpoint = "http://127.0.0.1:',
    'kind = "local"',
    'warmup-primary-500',
    'warmup-stabilize-500',
    '-SampleDurationSeconds 5',
    'Provider gate evidence must remain outside the public repository.',
    'Remove-WokCoreNewSyntheticCredential'
)) {
    if (
        $runnerSource.IndexOf(
            $required,
            [StringComparison]::Ordinal
        ) -lt 0
    ) {
        throw "Provider gate runner is missing an isolation invariant."
    }
}
foreach ($forbidden in @(
    "api.openai.com",
    "api.anthropic.com",
    "generativelanguage.googleapis.com",
    "Invoke-WebRequest",
    "Invoke-RestMethod"
)) {
    if (
        $runnerSource.IndexOf(
            $forbidden,
            [StringComparison]::OrdinalIgnoreCase
        ) -ge 0
    ) {
        throw "Provider gate runner contains a forbidden network capability."
    }
}

$temporaryThresholds = Join-Path ([IO.Path]::GetTempPath()) (
    "wokcore-provider-gates-{0}.toml" -f [Guid]::NewGuid().ToString("N")
)
try {
    [IO.File]::WriteAllText(
        $temporaryThresholds,
        "[windows]`nunknown = 1`n",
        [Text.UTF8Encoding]::new($false)
    )
    $failed = $false
    try {
        Import-WokCoreProviderGateThresholds -Path $temporaryThresholds | Out-Null
    } catch {
        $failed = $true
    }
    if (-not $failed) {
        throw "Unknown threshold fields were not rejected."
    }
} finally {
    Remove-Item -LiteralPath $temporaryThresholds -Force -ErrorAction SilentlyContinue
}

Write-Output "provider gate logic tests passed"
