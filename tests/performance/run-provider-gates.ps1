[CmdletBinding()]
param(
    [ValidateSet("release")]
    [string] $Profile = "release",
    [string] $OutputDirectory,
    [string] $TargetDirectory,
    [switch] $SkipBuild,
    [switch] $LibraryOnly
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Import-WokCoreProviderGateThresholds {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)]
        [string] $Path
    )

    $exactPath = [IO.Path]::GetFullPath($Path)
    if (-not [IO.File]::Exists($exactPath)) {
        throw "Provider gate threshold file does not exist."
    }
    $rootValues = @{}
    $windowsValues = @{}
    $section = ""
    foreach ($rawLine in [IO.File]::ReadAllLines($exactPath, [Text.Encoding]::UTF8)) {
        $line = $rawLine.Trim()
        if ($line.Length -eq 0 -or $line.StartsWith("#", [StringComparison]::Ordinal)) {
            continue
        }
        if ($line -match '^\[([a-z_]+)\]$') {
            if ($Matches[1] -ne "windows" -or $section.Length -ne 0) {
                throw "Provider gate threshold file has an unknown or duplicate section."
            }
            $section = $Matches[1]
            continue
        }
        if ($line -notmatch '^([a-z][a-z0-9_]*)\s*=\s*([0-9]+)$') {
            throw "Provider gate threshold file has an invalid value."
        }
        $key = $Matches[1]
        $value = [uint64]::Parse(
            $Matches[2],
            [Globalization.CultureInfo]::InvariantCulture
        )
        $target = if ($section -eq "windows") {
            $windowsValues
        } elseif ($section.Length -eq 0) {
            $rootValues
        } else {
            throw "Provider gate threshold file has an unknown section."
        }
        if ($target.ContainsKey($key)) {
            throw "Provider gate threshold file has a duplicate field."
        }
        $target.Add($key, $value)
    }

    $rootKeys = @("schema_version")
    $windowsKeys = @(
        "warm_idle_private_working_set_bytes",
        "standard_500_private_working_set_bytes",
        "recovery_multiplier_milli",
        "long_500_write_bytes_per_second",
        "observation_concurrency",
        "observation_max_handle_growth",
        "observation_max_thread_growth",
        "observation_monotonic_increase_samples",
        "observation_growth_multiplier_milli"
    )
    if (
        $rootValues.Count -ne $rootKeys.Count -or
        $windowsValues.Count -ne $windowsKeys.Count
    ) {
        throw "Provider gate threshold file is incomplete."
    }
    foreach ($key in $rootValues.Keys) {
        if ($key -notin $rootKeys) {
            throw "Provider gate threshold file has an unknown field."
        }
    }
    foreach ($key in $windowsValues.Keys) {
        if ($key -notin $windowsKeys) {
            throw "Provider gate threshold file has an unknown field."
        }
    }
    if ($rootValues.schema_version -ne 1) {
        throw "Provider gate threshold schema is unsupported."
    }
    foreach ($key in $windowsKeys) {
        if ($windowsValues[$key] -eq 0) {
            throw "Provider gate thresholds must be non-zero."
        }
    }

    [PSCustomObject] @{
        SchemaVersion = [uint32] $rootValues.schema_version
        WarmIdlePrivateWorkingSetBytes =
            [uint64] $windowsValues.warm_idle_private_working_set_bytes
        Standard500PrivateWorkingSetBytes =
            [uint64] $windowsValues.standard_500_private_working_set_bytes
        RecoveryMultiplierMilli =
            [uint32] $windowsValues.recovery_multiplier_milli
        Long500WriteBytesPerSecond =
            [uint64] $windowsValues.long_500_write_bytes_per_second
        ObservationConcurrency =
            [uint32] $windowsValues.observation_concurrency
        ObservationMaxHandleGrowth =
            [uint32] $windowsValues.observation_max_handle_growth
        ObservationMaxThreadGrowth =
            [uint32] $windowsValues.observation_max_thread_growth
        ObservationMonotonicIncreaseSamples =
            [uint32] $windowsValues.observation_monotonic_increase_samples
        ObservationGrowthMultiplierMilli =
            [uint32] $windowsValues.observation_growth_multiplier_milli
    }
}

function Test-WokCoreEvidenceShape {
    param(
        [Parameter(Mandatory = $true)]
        [object] $Evidence,
        [Parameter(Mandatory = $true)]
        [string] $ExpectedPhase
    )

    $required = @(
        "SchemaVersion",
        "Phase",
        "Samples",
        "PeakPrivateWorkingSetBytes",
        "WriteBytesPerSecond",
        "InitialPrivateWorkingSetBytes",
        "FinalPrivateWorkingSetBytes",
        "InitialHandleCount",
        "FinalHandleCount",
        "InitialThreadCount",
        "FinalThreadCount",
        "MaxConsecutivePrivateWorkingSetIncreases"
    )
    foreach ($field in $required) {
        if ($null -eq $Evidence.PSObject.Properties[$field]) {
            return $false
        }
    }
    return (
        $Evidence.SchemaVersion -eq 1 -and
        $Evidence.Phase -ceq $ExpectedPhase -and
        [uint64] $Evidence.Samples -ge 2
    )
}

function Test-WokCoreLoadReportShape {
    param(
        [Parameter(Mandatory = $true)]
        [object] $Report
    )

    foreach ($field in @(
        "configured_concurrency",
        "started",
        "active",
        "peak_active",
        "completed",
        "cancelled",
        "errors"
    )) {
        if ($null -eq $Report.PSObject.Properties[$field]) {
            return $false
        }
    }
    return $true
}

function Test-WokCoreProviderGateEvidence {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)]
        [object] $Thresholds,
        [Parameter(Mandatory = $true)]
        [object] $WarmIdle,
        [Parameter(Mandatory = $true)]
        [object] $Standard500,
        [Parameter(Mandatory = $true)]
        [object] $Long500,
        [Parameter(Mandatory = $true)]
        [object] $Recovery,
        [Parameter(Mandatory = $true)]
        [object] $Observation1000,
        [Parameter(Mandatory = $true)]
        [object] $StandardLoad,
        [Parameter(Mandatory = $true)]
        [object] $LongLoad,
        [Parameter(Mandatory = $true)]
        [object] $ObservationLoad
    )

    $failures = [Collections.Generic.List[string]]::new()
    $phaseShapes = @(
        (Test-WokCoreEvidenceShape -Evidence $WarmIdle -ExpectedPhase "warm_idle"),
        (Test-WokCoreEvidenceShape -Evidence $Standard500 -ExpectedPhase "standard_500"),
        (Test-WokCoreEvidenceShape -Evidence $Long500 -ExpectedPhase "long_500"),
        (Test-WokCoreEvidenceShape -Evidence $Recovery -ExpectedPhase "recovery"),
        (
            Test-WokCoreEvidenceShape `
                -Evidence $Observation1000 `
                -ExpectedPhase "observation_1000"
        )
    )
    $loadShapes = @(
        (Test-WokCoreLoadReportShape -Report $StandardLoad),
        (Test-WokCoreLoadReportShape -Report $LongLoad),
        (Test-WokCoreLoadReportShape -Report $ObservationLoad)
    )
    if ($false -in $phaseShapes -or $false -in $loadShapes) {
        $failures.Add("ambiguous_evidence")
        return [PSCustomObject] @{
            SchemaVersion = 1
            Passed = $false
            Failures = @($failures.ToArray())
        }
    }

    if (
        [uint64] $WarmIdle.PeakPrivateWorkingSetBytes -gt
            [uint64] $Thresholds.WarmIdlePrivateWorkingSetBytes
    ) {
        $failures.Add("warm_idle_memory")
    }
    if (
        [uint64] $Standard500.PeakPrivateWorkingSetBytes -gt
            [uint64] $Thresholds.Standard500PrivateWorkingSetBytes
    ) {
        $failures.Add("standard_500_memory")
    }
    $recoveryLimit = [uint64] [Math]::Ceiling(
        [double] $WarmIdle.PeakPrivateWorkingSetBytes *
            [double] $Thresholds.RecoveryMultiplierMilli / 1000.0
    )
    if ([uint64] $Recovery.PeakPrivateWorkingSetBytes -gt $recoveryLimit) {
        $failures.Add("recovery_memory")
    }
    if (
        [uint64] $Long500.WriteBytesPerSecond -gt
            [uint64] $Thresholds.Long500WriteBytesPerSecond
    ) {
        $failures.Add("long_500_write_rate")
    }

    $loadChecks = @(
        @{
            Name = "standard_500"
            Required = 500
            Report = $StandardLoad
        },
        @{
            Name = "long_500"
            Required = 500
            Report = $LongLoad
        },
        @{
            Name = "observation"
            Required = [uint32] $Thresholds.ObservationConcurrency
            Report = $ObservationLoad
        }
    )
    foreach ($check in $loadChecks) {
        $report = $check.Report
        $required = [uint64] $check.Required
        if ([uint64] $report.configured_concurrency -ne $required) {
            $failures.Add("$($check.Name)_concurrency")
        }
        if ([uint64] $report.started -lt $required) {
            $failures.Add("$($check.Name)_started")
        }
        if ([uint64] $report.peak_active -lt $required) {
            $failures.Add("$($check.Name)_peak_active")
        }
        if (
            [uint64] $report.active -ne 0 -or
            (
                [uint64] $report.completed +
                    [uint64] $report.cancelled +
                    [uint64] $report.errors
            ) -ne [uint64] $report.started
        ) {
            $failures.Add("$($check.Name)_incomplete")
        }
        if ([uint64] $report.errors -ne 0) {
            $failures.Add("$($check.Name)_errors")
        }
    }

    if (
        [uint64] $Observation1000.FinalHandleCount -gt
            (
                [uint64] $Observation1000.InitialHandleCount +
                    [uint64] $Thresholds.ObservationMaxHandleGrowth
            )
    ) {
        $failures.Add("observation_handle_growth")
    }
    if (
        [uint64] $Observation1000.FinalThreadCount -gt
            (
                [uint64] $Observation1000.InitialThreadCount +
                    [uint64] $Thresholds.ObservationMaxThreadGrowth
            )
    ) {
        $failures.Add("observation_thread_growth")
    }
    $observationGrowthLimit = [uint64] [Math]::Ceiling(
        [double] $Observation1000.InitialPrivateWorkingSetBytes *
            [double] $Thresholds.ObservationGrowthMultiplierMilli / 1000.0
    )
    if (
        [uint64] $Observation1000.MaxConsecutivePrivateWorkingSetIncreases -ge
            [uint64] $Thresholds.ObservationMonotonicIncreaseSamples -and
        [uint64] $Observation1000.FinalPrivateWorkingSetBytes -gt
            $observationGrowthLimit
    ) {
        $failures.Add("observation_monotonic_growth")
    }

    [PSCustomObject] @{
        SchemaVersion = 1
        Passed = $failures.Count -eq 0
        Failures = @($failures.ToArray())
        WarmIdlePrivateWorkingSetBytes =
            [uint64] $WarmIdle.PeakPrivateWorkingSetBytes
        Standard500PrivateWorkingSetBytes =
            [uint64] $Standard500.PeakPrivateWorkingSetBytes
        RecoveryPrivateWorkingSetBytes =
            [uint64] $Recovery.PeakPrivateWorkingSetBytes
        RecoveryLimitBytes = $recoveryLimit
        Long500WriteBytesPerSecond = [uint64] $Long500.WriteBytesPerSecond
        ObservationPeakActive = [uint64] $ObservationLoad.peak_active
        ObservationFinalHandleGrowth = [int64] (
            [int64] $Observation1000.FinalHandleCount -
                [int64] $Observation1000.InitialHandleCount
        )
        ObservationFinalThreadGrowth = [int64] (
            [int64] $Observation1000.FinalThreadCount -
                [int64] $Observation1000.InitialThreadCount
        )
    }
}

function Get-WokCoreFreeLoopbackPort {
    $listener = [Net.Sockets.TcpListener]::new(
        [Net.IPAddress]::Loopback,
        0
    )
    try {
        $listener.Start()
        return [uint16] ([Net.IPEndPoint] $listener.LocalEndpoint).Port
    } finally {
        $listener.Stop()
    }
}

function Wait-WokCoreLoopbackPort {
    param(
        [Parameter(Mandatory = $true)]
        [uint16] $Port,
        [Parameter(Mandatory = $true)]
        [Diagnostics.Process] $Owner,
        [int] $TimeoutSeconds = 15
    )

    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    while ([DateTime]::UtcNow -lt $deadline) {
        $Owner.Refresh()
        if ($Owner.HasExited) {
            throw "A provider gate process exited before becoming ready."
        }
        $client = [Net.Sockets.TcpClient]::new()
        try {
            $client.Connect([Net.IPAddress]::Loopback, $Port)
            return
        } catch {
            Start-Sleep -Milliseconds 50
        } finally {
            $client.Dispose()
        }
    }
    throw "A provider gate loopback listener did not become ready."
}

function Wait-WokCoreServiceReady {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Executable,
        [Parameter(Mandatory = $true)]
        [Diagnostics.Process] $Owner,
        [int] $TimeoutSeconds = 20
    )

    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    while ([DateTime]::UtcNow -lt $deadline) {
        $Owner.Refresh()
        if ($Owner.HasExited) {
            throw "WokCore exited before its management plane became ready."
        }
        $statusOutput = & $Executable status --json 2>$null
        if ($LASTEXITCODE -eq 0) {
            try {
                $status = ($statusOutput -join [Environment]::NewLine) |
                    ConvertFrom-Json
                if ([string] $status.code -eq "running") {
                    return
                }
            } catch {
            }
        }
        Start-Sleep -Milliseconds 100
    }
    throw "WokCore management plane did not become ready within its bounded timeout."
}

function Wait-WokCoreProcessExit {
    param(
        [Parameter(Mandatory = $true)]
        [Diagnostics.Process] $Process,
        [int] $TimeoutSeconds = 15
    )

    if (-not $Process.WaitForExit($TimeoutSeconds * 1000)) {
        throw "A provider gate process did not exit within its bounded timeout."
    }
    return [int] $Process.ExitCode
}

function Stop-WokCoreGateProcess {
    param(
        [Diagnostics.Process] $Process
    )

    if ($null -eq $Process) {
        return
    }
    try {
        $Process.Refresh()
        if (-not $Process.HasExited) {
            Stop-Process -Id $Process.Id -Force -ErrorAction Stop
            $Process.WaitForExit(5000) | Out-Null
        }
    } catch {
        throw "A provider gate process could not be stopped."
    }
}

function Test-WokCorePortAvailable {
    param(
        [Parameter(Mandatory = $true)]
        [uint16] $Port
    )

    $listener = [Net.Sockets.TcpListener]::new(
        [Net.IPAddress]::Loopback,
        $Port
    )
    try {
        $listener.Start()
        return $true
    } catch {
        return $false
    } finally {
        $listener.Stop()
    }
}

function Assert-WokCoreLoopbackNetwork {
    param(
        [Parameter(Mandatory = $true)]
        [uint32[]] $ProcessIds
    )

    foreach ($targetProcessId in $ProcessIds) {
        $connections = @(
            Get-NetTCPConnection `
                -OwningProcess $targetProcessId `
                -ErrorAction SilentlyContinue
        )
        foreach ($connection in $connections) {
            $local = [Net.IPAddress]::Parse($connection.LocalAddress)
            if ($connection.State -eq "Listen") {
                if (-not [Net.IPAddress]::IsLoopback($local)) {
                    throw "A provider gate process opened a non-loopback TCP listener."
                }
            } else {
                $remote = [Net.IPAddress]::Parse($connection.RemoteAddress)
                $localIsUnspecified =
                    $local.Equals([Net.IPAddress]::Any) -or
                    $local.Equals([Net.IPAddress]::IPv6Any)
                $remoteIsUnspecified =
                    $remote.Equals([Net.IPAddress]::Any) -or
                    $remote.Equals([Net.IPAddress]::IPv6Any)
                $isUnconnectedBoundSocket =
                    $connection.State -eq "Bound" -and
                    $localIsUnspecified -and
                    $remoteIsUnspecified
                if (
                    -not $isUnconnectedBoundSocket -and
                    (
                        -not [Net.IPAddress]::IsLoopback($remote) -or
                        (
                            -not [Net.IPAddress]::IsLoopback($local) -and
                            -not $localIsUnspecified
                        )
                    )
                ) {
                    throw "A provider gate process opened a non-loopback TCP connection."
                }
            }
        }
        if (
            @(
                Get-NetUDPEndpoint `
                    -OwningProcess $targetProcessId `
                    -ErrorAction SilentlyContinue
            ).Count -ne 0
        ) {
            throw "A provider gate process opened a UDP endpoint."
        }
    }
}

function Get-WokCoreNativeCredentialTargets {
    $rendered = (& cmdkey.exe /list 2>$null) -join [Environment]::NewLine
    if ($LASTEXITCODE -ne 0) {
        throw "Windows Credential Manager enumeration failed."
    }
    @(
        [regex]::Matches(
            $rendered,
            'secret:[0-9a-fA-F-]{36}\.dev\.wokcore\.credentials'
        ) |
            ForEach-Object { $_.Value.ToLowerInvariant() } |
            Sort-Object -Unique
    )
}

function Remove-WokCoreNewSyntheticCredential {
    param(
        [Parameter(Mandatory = $true)]
        [AllowEmptyCollection()]
        [string[]] $Before
    )

    $after = @(Get-WokCoreNativeCredentialTargets)
    $created = @($after | Where-Object { $_ -notin $Before })
    if ($created.Count -eq 0) {
        return
    }
    if ($created.Count -gt 1) {
        throw "Synthetic Credential Manager cleanup was ambiguous."
    }
    $target = $created[0]
    if (
        $target -notmatch
            '^secret:[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}\.dev\.wokcore\.credentials$'
    ) {
        throw "Synthetic Credential Manager target was invalid."
    }
    & cmdkey.exe "/delete:$target" 1>$null 2>$null
    if ($LASTEXITCODE -ne 0) {
        throw "Synthetic Credential Manager cleanup failed."
    }
    if ($target -in @(Get-WokCoreNativeCredentialTargets)) {
        throw "Synthetic Credential Manager cleanup was not verified."
    }
}

function Set-WokCoreOwnerOnlyDirectory {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Path
    )

    [IO.Directory]::CreateDirectory($Path) | Out-Null
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $acl = [Security.AccessControl.DirectorySecurity]::new()
    $acl.SetOwner($identity.User)
    $acl.SetAccessRuleProtection($true, $false)
    $rule = [Security.AccessControl.FileSystemAccessRule]::new(
        $identity.User,
        [Security.AccessControl.FileSystemRights]::FullControl,
        (
            [Security.AccessControl.InheritanceFlags]::ContainerInherit -bor
                [Security.AccessControl.InheritanceFlags]::ObjectInherit
        ),
        [Security.AccessControl.PropagationFlags]::None,
        [Security.AccessControl.AccessControlType]::Allow
    )
    $acl.AddAccessRule($rule)
    Set-Acl -LiteralPath $Path -AclObject $acl
}

function Write-WokCoreUtf8File {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Path,
        [Parameter(Mandatory = $true)]
        [string] $Contents
    )

    [IO.File]::WriteAllText(
        [IO.Path]::GetFullPath($Path),
        $Contents,
        [Text.UTF8Encoding]::new($false)
    )
}

function Start-WokCoreSimulatorProcess {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Executable,
        [Parameter(Mandatory = $true)]
        [uint16] $Port,
        [Parameter(Mandatory = $true)]
        [string] $Scenario,
        [Parameter(Mandatory = $true)]
        [string] $WorkingDirectory,
        [Parameter(Mandatory = $true)]
        [string] $ArtifactDirectory,
        [Parameter(Mandatory = $true)]
        [string] $Name
    )

    $process = Start-Process `
        -FilePath $Executable `
        -ArgumentList @(
            "--bind",
            "127.0.0.1:$Port",
            "--scenario",
            $Scenario
        ) `
        -WorkingDirectory $WorkingDirectory `
        -WindowStyle Hidden `
        -RedirectStandardOutput (Join-Path $ArtifactDirectory "$Name.stdout") `
        -RedirectStandardError (Join-Path $ArtifactDirectory "$Name.stderr") `
        -PassThru
    Wait-WokCoreLoopbackPort -Port $Port -Owner $process
    $process
}

function Invoke-WokCoreLoadPhase {
    param(
        [Parameter(Mandatory = $true)]
        [string] $LoadGenerator,
        [Parameter(Mandatory = $true)]
        [string] $Token,
        [Parameter(Mandatory = $true)]
        [string] $Target,
        [Parameter(Mandatory = $true)]
        [uint32] $Concurrency,
        [Parameter(Mandatory = $true)]
        [string] $PayloadProfile,
        [Parameter(Mandatory = $true)]
        [string] $PhaseName,
        [Parameter(Mandatory = $true)]
        [Diagnostics.Process] $WokCoreProcess,
        [Parameter(Mandatory = $true)]
        [string] $WokCoreExecutable,
        [Parameter(Mandatory = $true)]
        [Diagnostics.Process] $SimulatorProcess,
        [Parameter(Mandatory = $true)]
        [string] $WorkingDirectory,
        [Parameter(Mandatory = $true)]
        [string] $ArtifactDirectory,
        [ValidateRange(3, 30)]
        [int] $SampleDurationSeconds = 3,
        [ValidateRange(0, 10000)]
        [int] $RampMilliseconds = 0,
        [ValidateRange(10000, 120000)]
        [int] $LoadDurationMilliseconds = 30000
    )

    $tokenPath = Join-Path $ArtifactDirectory "$PhaseName.token"
    $stdoutPath = Join-Path $ArtifactDirectory "$PhaseName.load.json"
    $stderrPath = Join-Path $ArtifactDirectory "$PhaseName.load.stderr"
    Write-WokCoreUtf8File -Path $tokenPath -Contents $Token
    $load = $null
    try {
        $load = Start-Process `
            -FilePath $LoadGenerator `
            -ArgumentList @(
                "--target",
                $Target,
                "--concurrency",
                $Concurrency,
                "--duration-ms",
                $LoadDurationMilliseconds,
                "--ramp-ms",
                $RampMilliseconds,
                "--protocol",
                "responses=1",
                "--payload-profile",
                $PayloadProfile,
                "--token-stdin",
                "--max-errors",
                0,
                "--require-peak-active",
                $Concurrency
            ) `
            -WorkingDirectory $WorkingDirectory `
            -WindowStyle Hidden `
            -RedirectStandardInput $tokenPath `
            -RedirectStandardOutput $stdoutPath `
            -RedirectStandardError $stderrPath `
            -PassThru
        Start-Sleep -Milliseconds 100
        $evidence = Measure-WokCoreProcessPhase `
            -TargetProcessId $WokCoreProcess.Id `
            -ExactExecutablePath $WokCoreExecutable `
            -PhaseName $PhaseName `
            -SampleDurationSeconds $SampleDurationSeconds `
            -SampleIntervalMilliseconds 100
        Assert-WokCoreLoopbackNetwork -ProcessIds @(
            $WokCoreProcess.Id,
            $SimulatorProcess.Id,
            $load.Id
        )
        $loadExitCode = Wait-WokCoreProcessExit `
            -Process $load `
            -TimeoutSeconds 45
        $serialized = [IO.File]::ReadAllText($stdoutPath, [Text.Encoding]::UTF8)
        if ($serialized.Length -eq 0 -or $serialized.Length -ge 65536) {
            throw "Load generator report was missing or exceeded its bound."
        }
        $report = $serialized | ConvertFrom-Json
        if ($loadExitCode -notin @(0, 2)) {
            throw "Load generator returned an unexpected failure exit code."
        }
        $report | Add-Member `
            -MemberType NoteProperty `
            -Name "exit_code" `
            -Value $loadExitCode
        [PSCustomObject] @{
            Evidence = $evidence
            Load = $report
        }
    } finally {
        Remove-Item -LiteralPath $tokenPath -Force -ErrorAction SilentlyContinue
        if ($null -ne $load) {
            $load.Refresh()
            if (-not $load.HasExited) {
                Stop-WokCoreGateProcess -Process $load
            }
        }
    }
}

function Invoke-WokCoreProviderGates {
    param(
        [Parameter(Mandatory = $true)]
        [string] $SelectedProfile,
        [string] $RequestedOutputDirectory,
        [string] $RequestedTargetDirectory,
        [switch] $WithoutBuild
    )

    if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
        throw "Exact provider resource gates require Windows."
    }

    $repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\.."))
    $resourceGate = Join-Path $PSScriptRoot "windows-resource-gate.ps1"
    . $resourceGate -LibraryOnly
    $thresholds = Import-WokCoreProviderGateThresholds `
        -Path (Join-Path $PSScriptRoot "provider-gates.toml")

    $gitCommon = (
        & git.exe `
            -C $repositoryRoot `
            rev-parse `
            --path-format=absolute `
            --git-common-dir
    )
    if ($LASTEXITCODE -ne 0 -or [String]::IsNullOrWhiteSpace($gitCommon)) {
        throw "The stable Cargo target directory could not be resolved."
    }
    $mainRepository = [IO.Path]::GetDirectoryName(
        [IO.Path]::GetFullPath($gitCommon.Trim())
    )
    $targetRoot = $RequestedTargetDirectory
    if ([String]::IsNullOrWhiteSpace($targetRoot)) {
        $targetRoot = Join-Path $mainRepository "target"
    }
    $targetRoot = [IO.Path]::GetFullPath($targetRoot)

    $releaseDirectory = Join-Path $targetRoot "release"
    $wokcoreExecutable = Join-Path $releaseDirectory "wokcore.exe"
    $simulatorExecutable = Join-Path $releaseDirectory "wokcore-provider-sim.exe"
    $loadGeneratorExecutable = Join-Path $releaseDirectory "wokcore-loadgen.exe"
    if (-not $WithoutBuild) {
        & cargo.exe `
            +1.97.1 `
            build `
            --workspace `
            --all-features `
            --release `
            --locked `
            --offline `
            --target-dir $targetRoot
        if ($LASTEXITCODE -ne 0) {
            throw "Release provider gate build failed."
        }
    }
    foreach ($executable in @(
        $wokcoreExecutable,
        $simulatorExecutable,
        $loadGeneratorExecutable
    )) {
        if (-not [IO.File]::Exists($executable)) {
            throw "A fixed-name release provider gate executable is missing."
        }
    }

    $finalOutputDirectory = $RequestedOutputDirectory
    if (-not [String]::IsNullOrWhiteSpace($finalOutputDirectory)) {
        $finalOutputDirectory = [IO.Path]::GetFullPath($finalOutputDirectory)
        foreach ($publicRoot in @($repositoryRoot, $mainRepository)) {
            $exactPublicRoot = [IO.Path]::GetFullPath($publicRoot).TrimEnd(
                [IO.Path]::DirectorySeparatorChar,
                [IO.Path]::AltDirectorySeparatorChar
            )
            $publicPrefix =
                $exactPublicRoot + [IO.Path]::DirectorySeparatorChar
            if (
                [StringComparer]::OrdinalIgnoreCase.Equals(
                    $finalOutputDirectory.TrimEnd(
                        [IO.Path]::DirectorySeparatorChar,
                        [IO.Path]::AltDirectorySeparatorChar
                    ),
                    $exactPublicRoot
                ) -or
                $finalOutputDirectory.StartsWith(
                    $publicPrefix,
                    [StringComparison]::OrdinalIgnoreCase
                )
            ) {
                throw "Provider gate evidence must remain outside the public repository."
            }
        }
        [IO.Directory]::CreateDirectory($finalOutputDirectory) | Out-Null
    }

    $runningWokCore = @(
        Get-Process -Name "wokcore" -ErrorAction SilentlyContinue
    )
    if ($runningWokCore.Count -ne 0) {
        throw "Another WokCore process is running; exact provider gates require isolation."
    }

    $credentialTargetsBefore = @(Get-WokCoreNativeCredentialTargets)
    $corePort = Get-WokCoreFreeLoopbackPort
    do {
        $simulatorPort = Get-WokCoreFreeLoopbackPort
    } while ($simulatorPort -eq $corePort)

    $temporaryParent = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    $temporaryRoot = Join-Path $temporaryParent (
        "wokcore-provider-gates-{0}" -f [Guid]::NewGuid().ToString("N")
    )
    if (
        -not [IO.Path]::GetFullPath($temporaryRoot).StartsWith(
            $temporaryParent,
            [StringComparison]::OrdinalIgnoreCase
        )
    ) {
        throw "Provider gate temporary path resolution failed."
    }
    $artifactDirectory = Join-Path $temporaryRoot "artifacts"
    $simulator = $null
    $wokcore = $null
    $credentialsCreated = $false
    $gateResult = $null
    $report = $null
    try {
        Set-WokCoreOwnerOnlyDirectory -Path $temporaryRoot
        Set-WokCoreOwnerOnlyDirectory -Path $artifactDirectory

        $roaming = Join-Path $temporaryRoot "roaming"
        $local = Join-Path $temporaryRoot "local"
        $isolatedHome = Join-Path $temporaryRoot "home"
        foreach ($directory in @($roaming, $local, $isolatedHome)) {
            [IO.Directory]::CreateDirectory($directory) | Out-Null
        }
        $env:APPDATA = $roaming
        $env:LOCALAPPDATA = $local
        $env:USERPROFILE = $isolatedHome
        $env:HOME = $isolatedHome
        $env:NO_PROXY = "127.0.0.1,::1"
        $env:no_proxy = "127.0.0.1,::1"

        $configDirectory = Join-Path $roaming "WokCore"
        [IO.Directory]::CreateDirectory($configDirectory) | Out-Null
        $configPath = Join-Path $configDirectory "config.toml"
        $config = @"
revision = 1

[server]
port = $corePort

[providers]

[[providers.instances]]
id = "synthetic"
catalog_id = "ollama"
enabled = true
endpoint = "http://127.0.0.1:$simulatorPort/v1"
allow_private_network = true

[[providers.accounts]]
id = "local"
provider = "synthetic"
enabled = true

[providers.accounts.auth]
kind = "local"

[routing]
aliases = []
rules = []

[routing.default]
provider = "synthetic"
model = "synthetic"
"@
        Write-WokCoreUtf8File -Path $configPath -Contents $config

        $standardScenario = Join-Path `
            $repositoryRoot `
            "crates\wokcore-provider-sim\scenarios\standard.toml"
        $slowScenario = Join-Path `
            $repositoryRoot `
            "crates\wokcore-provider-sim\scenarios\slow-stream.toml"
        $simulator = Start-WokCoreSimulatorProcess `
            -Executable $simulatorExecutable `
            -Port $simulatorPort `
            -Scenario $standardScenario `
            -WorkingDirectory $repositoryRoot `
            -ArtifactDirectory $artifactDirectory `
            -Name "bootstrap-standard"
        $wokcore = Start-Process `
            -FilePath $wokcoreExecutable `
            -ArgumentList @("serve", "--json") `
            -WorkingDirectory $repositoryRoot `
            -WindowStyle Hidden `
            -RedirectStandardOutput (Join-Path $artifactDirectory "wokcore.stdout") `
            -RedirectStandardError (Join-Path $artifactDirectory "wokcore.stderr") `
            -PassThru
        $credentialsCreated = $true
        Wait-WokCoreLoopbackPort -Port $corePort -Owner $wokcore
        Wait-WokCoreServiceReady `
            -Executable $wokcoreExecutable `
            -Owner $wokcore

        $authorizeOutput = & $wokcoreExecutable `
            authorize `
            --client wokcore-performance-gate `
            --scope proxy.use `
            --json 2>$null
        if ($LASTEXITCODE -ne 0) {
            throw "Synthetic provider gate authorization failed."
        }
        $authorized = ($authorizeOutput -join [Environment]::NewLine) |
            ConvertFrom-Json
        $token = [string] $authorized.token
        if (-not $token.StartsWith("wok_proxy_v1_", [StringComparison]::Ordinal)) {
            throw "Synthetic provider gate token shape was invalid."
        }

        Stop-WokCoreGateProcess -Process $simulator
        $simulator = $null
        if (-not (Test-WokCorePortAvailable -Port $simulatorPort)) {
            throw "The bootstrap simulator listener remained open."
        }
        $simulator = Start-WokCoreSimulatorProcess `
            -Executable $simulatorExecutable `
            -Port $simulatorPort `
            -Scenario $slowScenario `
            -WorkingDirectory $repositoryRoot `
            -ArtifactDirectory $artifactDirectory `
            -Name "warmup-long-500"
        foreach ($warmupPass in @(
            "warmup-primary-500",
            "warmup-stabilize-500"
        )) {
            $warmupResult = Invoke-WokCoreLoadPhase `
                -LoadGenerator $loadGeneratorExecutable `
                -Token $token `
                -Target "http://127.0.0.1:$corePort" `
                -Concurrency 500 `
                -PayloadProfile "long-reasoning" `
                -PhaseName "active" `
                -WokCoreProcess $wokcore `
                -WokCoreExecutable $wokcoreExecutable `
                -SimulatorProcess $simulator `
                -WorkingDirectory $repositoryRoot `
                -ArtifactDirectory $artifactDirectory `
                -RampMilliseconds 250
            if (
                [uint64] $warmupResult.Load.started -ne 500 -or
                [uint64] $warmupResult.Load.active -ne 0 -or
                [uint64] $warmupResult.Load.peak_active -ne 500 -or
                [uint64] $warmupResult.Load.completed -ne 500 -or
                [uint64] $warmupResult.Load.cancelled -ne 0 -or
                [uint64] $warmupResult.Load.errors -ne 0
            ) {
                $warmupFailure = (
                    "Synthetic WokCore {0} did not complete exactly " +
                    "(started={1}, active={2}, peak={3}, completed={4}, " +
                    "cancelled={5}, errors={6}, exit={7})."
                ) -f @(
                    $warmupPass,
                    $warmupResult.Load.started,
                    $warmupResult.Load.active,
                    $warmupResult.Load.peak_active,
                    $warmupResult.Load.completed,
                    $warmupResult.Load.cancelled,
                    $warmupResult.Load.errors,
                    $warmupResult.Load.exit_code
                )
                throw $warmupFailure
            }
        }
        Stop-WokCoreGateProcess -Process $simulator
        $simulator = $null
        if (-not (Test-WokCorePortAvailable -Port $simulatorPort)) {
            throw "The warmup simulator listener remained open."
        }
        $simulator = Start-WokCoreSimulatorProcess `
            -Executable $simulatorExecutable `
            -Port $simulatorPort `
            -Scenario $standardScenario `
            -WorkingDirectory $repositoryRoot `
            -ArtifactDirectory $artifactDirectory `
            -Name "standard-500"
        Start-Sleep -Seconds 10
        $warmIdle = Measure-WokCoreProcessPhase `
            -TargetProcessId $wokcore.Id `
            -ExactExecutablePath $wokcoreExecutable `
            -PhaseName "warm_idle" `
            -SampleDurationSeconds 3 `
            -SampleIntervalMilliseconds 100

        $standardResult = Invoke-WokCoreLoadPhase `
            -LoadGenerator $loadGeneratorExecutable `
            -Token $token `
            -Target "http://127.0.0.1:$corePort" `
            -Concurrency 500 `
            -PayloadProfile "standard32k" `
            -PhaseName "standard_500" `
            -WokCoreProcess $wokcore `
            -WokCoreExecutable $wokcoreExecutable `
            -SimulatorProcess $simulator `
            -WorkingDirectory $repositoryRoot `
            -ArtifactDirectory $artifactDirectory
        Stop-WokCoreGateProcess -Process $simulator
        $simulator = $null
        if (-not (Test-WokCorePortAvailable -Port $simulatorPort)) {
            throw "The standard simulator listener remained open."
        }

        $simulator = Start-WokCoreSimulatorProcess `
            -Executable $simulatorExecutable `
            -Port $simulatorPort `
            -Scenario $slowScenario `
            -WorkingDirectory $repositoryRoot `
            -ArtifactDirectory $artifactDirectory `
            -Name "long-500"
        $longResult = Invoke-WokCoreLoadPhase `
            -LoadGenerator $loadGeneratorExecutable `
            -Token $token `
            -Target "http://127.0.0.1:$corePort" `
            -Concurrency 500 `
            -PayloadProfile "long-reasoning" `
            -PhaseName "long_500" `
            -WokCoreProcess $wokcore `
            -WokCoreExecutable $wokcoreExecutable `
            -SimulatorProcess $simulator `
            -WorkingDirectory $repositoryRoot `
            -ArtifactDirectory $artifactDirectory `
            -SampleDurationSeconds 5
        Stop-WokCoreGateProcess -Process $simulator
        $simulator = $null
        if (-not (Test-WokCorePortAvailable -Port $simulatorPort)) {
            throw "The long-stream simulator listener remained open."
        }

        Start-Sleep -Seconds 60
        $recovery = Measure-WokCoreProcessPhase `
            -TargetProcessId $wokcore.Id `
            -ExactExecutablePath $wokcoreExecutable `
            -PhaseName "recovery" `
            -SampleDurationSeconds 3 `
            -SampleIntervalMilliseconds 100

        $simulator = Start-WokCoreSimulatorProcess `
            -Executable $simulatorExecutable `
            -Port $simulatorPort `
            -Scenario $standardScenario `
            -WorkingDirectory $repositoryRoot `
            -ArtifactDirectory $artifactDirectory `
            -Name "observation-1000"
        $observationResult = Invoke-WokCoreLoadPhase `
            -LoadGenerator $loadGeneratorExecutable `
            -Token $token `
            -Target "http://127.0.0.1:$corePort" `
            -Concurrency $thresholds.ObservationConcurrency `
            -PayloadProfile "standard32k" `
            -PhaseName "observation_1000" `
            -WokCoreProcess $wokcore `
            -WokCoreExecutable $wokcoreExecutable `
            -SimulatorProcess $simulator `
            -WorkingDirectory $repositoryRoot `
            -ArtifactDirectory $artifactDirectory `
            -SampleDurationSeconds 8 `
            -RampMilliseconds 500

        $gateResult = Test-WokCoreProviderGateEvidence `
            -Thresholds $thresholds `
            -WarmIdle $warmIdle `
            -Standard500 $standardResult.Evidence `
            -Long500 $longResult.Evidence `
            -Recovery $recovery `
            -Observation1000 $observationResult.Evidence `
            -StandardLoad $standardResult.Load `
            -LongLoad $longResult.Load `
            -ObservationLoad $observationResult.Load
        $report = [PSCustomObject] @{
            SchemaVersion = 1
            Profile = $SelectedProfile
            Offline = $true
            LoopbackOnly = $true
            FixedExecutables = @(
                "wokcore.exe",
                "wokcore-provider-sim.exe",
                "wokcore-loadgen.exe"
            )
            Gate = $gateResult
            Phases = @(
                $warmIdle,
                $standardResult.Evidence,
                $longResult.Evidence,
                $recovery,
                $observationResult.Evidence
            )
            Loads = @(
                $standardResult.Load,
                $longResult.Load,
                $observationResult.Load
            )
        }
    } finally {
        try {
            if ($null -ne $simulator) {
                Stop-WokCoreGateProcess -Process $simulator
            }
            if ($null -ne $wokcore) {
                $wokcore.Refresh()
                if (-not $wokcore.HasExited) {
                    & $wokcoreExecutable stop --json 1>$null 2>$null
                    if ($LASTEXITCODE -eq 0) {
                        $wokcore.WaitForExit(10000) | Out-Null
                    }
                }
                $wokcore.Refresh()
                if (-not $wokcore.HasExited) {
                    Stop-WokCoreGateProcess -Process $wokcore
                }
            }
            if ($credentialsCreated) {
                Remove-WokCoreNewSyntheticCredential -Before $credentialTargetsBefore
            }
        } finally {
            $resolvedTemporaryRoot = [IO.Path]::GetFullPath($temporaryRoot)
            if (
                -not $resolvedTemporaryRoot.StartsWith(
                    $temporaryParent,
                    [StringComparison]::OrdinalIgnoreCase
                )
            ) {
                throw "Provider gate temporary cleanup path was invalid."
            }
            if ([IO.Directory]::Exists($resolvedTemporaryRoot)) {
                Remove-Item -LiteralPath $resolvedTemporaryRoot -Recurse -Force
            }
        }
    }

    if (
        -not (Test-WokCorePortAvailable -Port $corePort) -or
        -not (Test-WokCorePortAvailable -Port $simulatorPort)
    ) {
        throw "A provider gate listener remained open after shutdown."
    }
    $serializedReport = $report | ConvertTo-Json -Depth 10 -Compress
    if ($serializedReport.Length -eq 0 -or $serializedReport.Length -ge 131072) {
        throw "Provider gate evidence was missing or exceeded its bound."
    }
    if ([String]::IsNullOrWhiteSpace($finalOutputDirectory)) {
        Write-Output $serializedReport
    } else {
        Write-WokCoreUtf8File `
            -Path (Join-Path $finalOutputDirectory "provider-gates-report.json") `
            -Contents $serializedReport
        Write-Output $serializedReport
    }

    if (-not $gateResult.Passed) {
        throw "One or more provider resource gates failed."
    }
}

if (-not $LibraryOnly) {
    Invoke-WokCoreProviderGates `
        -SelectedProfile $Profile `
        -RequestedOutputDirectory $OutputDirectory `
        -RequestedTargetDirectory $TargetDirectory `
        -WithoutBuild:$SkipBuild
}
