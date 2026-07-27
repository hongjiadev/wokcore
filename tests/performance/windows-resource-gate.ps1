[CmdletBinding()]
param(
    [uint32] $ProcessId,
    [string] $ExpectedExecutablePath,
    [ValidateSet("warm_idle", "active", "recovery")]
    [string] $Phase = "active",
    [ValidateRange(1, 1800)]
    [int] $DurationSeconds = 10,
    [ValidateRange(100, 10000)]
    [int] $IntervalMilliseconds = 250,
    [ValidateSet("json", "csv")]
    [string] $OutputFormat = "json",
    [string] $OutputPath,
    [switch] $LibraryOnly
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Initialize-WokCoreNativeProcessMetrics {
    if ("WokCore.Performance.NativeProcessMetrics" -as [type]) {
        return
    }
    Add-Type -TypeDefinition @"
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;

namespace WokCore.Performance {
    public static class NativeProcessMetrics {
        private const uint PROCESS_QUERY_INFORMATION = 0x0400;
        private const uint PROCESS_VM_READ = 0x0010;

        [StructLayout(LayoutKind.Sequential)]
        private struct PROCESS_MEMORY_COUNTERS_EX2 {
            public uint cb;
            public uint PageFaultCount;
            public UIntPtr PeakWorkingSetSize;
            public UIntPtr WorkingSetSize;
            public UIntPtr QuotaPeakPagedPoolUsage;
            public UIntPtr QuotaPagedPoolUsage;
            public UIntPtr QuotaPeakNonPagedPoolUsage;
            public UIntPtr QuotaNonPagedPoolUsage;
            public UIntPtr PagefileUsage;
            public UIntPtr PeakPagefileUsage;
            public UIntPtr PrivateUsage;
            public UIntPtr PrivateWorkingSetSize;
            public ulong SharedCommitUsage;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct IO_COUNTERS {
            public ulong ReadOperationCount;
            public ulong WriteOperationCount;
            public ulong OtherOperationCount;
            public ulong ReadTransferCount;
            public ulong WriteTransferCount;
            public ulong OtherTransferCount;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct FILETIME {
            public uint Low;
            public uint High;
        }

        public sealed class Snapshot {
            public ulong IdentityToken { get; set; }
            public ulong PrivateWorkingSetBytes { get; set; }
            public ulong PeakPrivateBytes { get; set; }
            public ulong ReadBytes { get; set; }
            public ulong WriteBytes { get; set; }
            public uint HandleCount { get; set; }
        }

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern IntPtr OpenProcess(uint access, bool inherit, uint processId);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool CloseHandle(IntPtr handle);

        [DllImport("kernel32.dll", EntryPoint = "K32GetProcessMemoryInfo", SetLastError = true)]
        private static extern bool GetProcessMemoryInfo(
            IntPtr process,
            ref PROCESS_MEMORY_COUNTERS_EX2 counters,
            uint size);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool GetProcessIoCounters(IntPtr process, ref IO_COUNTERS counters);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool GetProcessHandleCount(IntPtr process, out uint count);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool GetProcessTimes(
            IntPtr process,
            out FILETIME creation,
            out FILETIME exit,
            out FILETIME kernel,
            out FILETIME user);

        public static Snapshot Read(uint processId) {
            IntPtr handle = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, processId);
            if (handle == IntPtr.Zero) {
                throw new Win32Exception(Marshal.GetLastWin32Error());
            }
            try {
                PROCESS_MEMORY_COUNTERS_EX2 memory = new PROCESS_MEMORY_COUNTERS_EX2();
                memory.cb = (uint)Marshal.SizeOf<PROCESS_MEMORY_COUNTERS_EX2>();
                if (!GetProcessMemoryInfo(handle, ref memory, memory.cb)) {
                    throw new Win32Exception(Marshal.GetLastWin32Error());
                }
                IO_COUNTERS io = new IO_COUNTERS();
                if (!GetProcessIoCounters(handle, ref io)) {
                    throw new Win32Exception(Marshal.GetLastWin32Error());
                }
                uint handles;
                if (!GetProcessHandleCount(handle, out handles)) {
                    throw new Win32Exception(Marshal.GetLastWin32Error());
                }
                FILETIME creation;
                FILETIME exit;
                FILETIME kernel;
                FILETIME user;
                if (!GetProcessTimes(handle, out creation, out exit, out kernel, out user)) {
                    throw new Win32Exception(Marshal.GetLastWin32Error());
                }
                return new Snapshot {
                    IdentityToken = ((ulong)creation.High << 32) | creation.Low,
                    PrivateWorkingSetBytes = memory.PrivateWorkingSetSize.ToUInt64(),
                    PeakPrivateBytes = memory.PeakPagefileUsage.ToUInt64(),
                    ReadBytes = io.ReadTransferCount,
                    WriteBytes = io.WriteTransferCount,
                    HandleCount = handles
                };
            } finally {
                CloseHandle(handle);
            }
        }
    }
}
"@
}

function Get-WokCoreProcessSample {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)]
        [uint32] $TargetProcessId,
        [Parameter(Mandatory = $true)]
        [string] $ExactExecutablePath
    )

    if ($TargetProcessId -eq 0) {
        throw "Process ID must be non-zero."
    }
    $expected = [IO.Path]::GetFullPath($ExactExecutablePath)
    $process = Get-Process -Id $TargetProcessId -ErrorAction Stop
    if ($process.HasExited) {
        throw "Target process already exited."
    }
    $actual = [IO.Path]::GetFullPath($process.Path)
    if (-not [StringComparer]::OrdinalIgnoreCase.Equals($actual, $expected)) {
        throw "Target executable path does not match the exact expected path."
    }
    Initialize-WokCoreNativeProcessMetrics
    $native = [WokCore.Performance.NativeProcessMetrics]::Read($TargetProcessId)
    $observed = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    $created = $native.IdentityToken
    $fileTimeUnixEpoch = [uint64] 116444736000000000
    if ($created -lt $fileTimeUnixEpoch) {
        throw "Process creation time is invalid."
    }
    $createdMs = [uint64] (($created - $fileTimeUnixEpoch) / 10000)
    [PSCustomObject] @{
        Pid = $TargetProcessId
        IdentityToken = $created
        ObservedMs = [uint64] $observed
        PrivateWorkingSetBytes = [uint64] $native.PrivateWorkingSetBytes
        PeakPrivateBytes = [uint64] $native.PeakPrivateBytes
        ReadBytes = [uint64] $native.ReadBytes
        WriteBytes = [uint64] $native.WriteBytes
        HandleCount = [uint32] $native.HandleCount
        ThreadCount = [uint32] $process.Threads.Count
        LifetimeMs = [uint64] ([Math]::Max(0, $observed - $createdMs))
    }
}

function Test-WokCoreProcessSampleSeries {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)]
        [object[]] $Samples
    )

    if ($Samples.Count -eq 0) {
        throw "At least one process sample is required."
    }
    $first = $Samples[0]
    $previous = $null
    foreach ($sample in $Samples) {
        foreach ($field in @(
            "Pid",
            "IdentityToken",
            "ObservedMs",
            "PrivateWorkingSetBytes",
            "PeakPrivateBytes",
            "ReadBytes",
            "WriteBytes",
            "HandleCount",
            "ThreadCount",
            "LifetimeMs"
        )) {
            if ($null -eq $sample.PSObject.Properties[$field]) {
                throw "Process sample is missing a required field."
            }
        }
        if ($sample.Pid -ne $first.Pid -or $sample.IdentityToken -ne $first.IdentityToken) {
            throw "Process identity changed while sampling."
        }
        if ($null -ne $previous) {
            if (
                $sample.ObservedMs -lt $previous.ObservedMs -or
                $sample.LifetimeMs -lt $previous.LifetimeMs
            ) {
                throw "Process sample time moved backwards."
            }
            if (
                $sample.ReadBytes -lt $previous.ReadBytes -or
                $sample.WriteBytes -lt $previous.WriteBytes
            ) {
                throw "Process I/O counters moved backwards."
            }
        }
        $previous = $sample
    }
    $true
}

function ConvertTo-WokCorePhaseEvidence {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)]
        [object[]] $Samples,
        [Parameter(Mandatory = $true)]
        [ValidateSet("warm_idle", "active", "recovery")]
        [string] $PhaseName,
        [Parameter(Mandatory = $true)]
        [string] $ExecutableName
    )

    Test-WokCoreProcessSampleSeries -Samples $Samples | Out-Null
    $first = $Samples[0]
    $last = $Samples[$Samples.Count - 1]
    $durationMs = [uint64] [Math]::Max(1, $last.ObservedMs - $first.ObservedMs)
    $writeDelta = [uint64] ($last.WriteBytes - $first.WriteBytes)
    $readDelta = [uint64] ($last.ReadBytes - $first.ReadBytes)
    [PSCustomObject] @{
        SchemaVersion = 1
        ExecutableName = [IO.Path]::GetFileName($ExecutableName)
        Pid = [uint32] $first.Pid
        Phase = $PhaseName
        Samples = [uint32] $Samples.Count
        DurationMs = $durationMs
        PeakPrivateWorkingSetBytes = [uint64] (
            $Samples |
                Measure-Object -Property PrivateWorkingSetBytes -Maximum
        ).Maximum
        PeakPrivateBytes = [uint64] (
            $Samples |
                Measure-Object -Property PeakPrivateBytes -Maximum
        ).Maximum
        PeakHandleCount = [uint32] (
            $Samples |
                Measure-Object -Property HandleCount -Maximum
        ).Maximum
        PeakThreadCount = [uint32] (
            $Samples |
                Measure-Object -Property ThreadCount -Maximum
        ).Maximum
        ReadBytes = $readDelta
        WriteBytes = $writeDelta
        WriteBytesPerSecond = [uint64] [Math]::Ceiling($writeDelta * 1000.0 / $durationMs)
        ProcessLifetimeMs = [uint64] $last.LifetimeMs
    }
}

function Measure-WokCoreProcessPhase {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)]
        [uint32] $TargetProcessId,
        [Parameter(Mandatory = $true)]
        [string] $ExactExecutablePath,
        [Parameter(Mandatory = $true)]
        [ValidateSet("warm_idle", "active", "recovery")]
        [string] $PhaseName,
        [Parameter(Mandatory = $true)]
        [int] $SampleDurationSeconds,
        [Parameter(Mandatory = $true)]
        [int] $SampleIntervalMilliseconds
    )

    $samples = [Collections.Generic.List[object]]::new()
    $deadline = [Diagnostics.Stopwatch]::StartNew()
    do {
        $samples.Add(
            (Get-WokCoreProcessSample `
                -TargetProcessId $TargetProcessId `
                -ExactExecutablePath $ExactExecutablePath)
        )
        if ($deadline.Elapsed.TotalSeconds -lt $SampleDurationSeconds) {
            Start-Sleep -Milliseconds $SampleIntervalMilliseconds
        }
    } while ($deadline.Elapsed.TotalSeconds -lt $SampleDurationSeconds)
    ConvertTo-WokCorePhaseEvidence `
        -Samples $samples.ToArray() `
        -PhaseName $PhaseName `
        -ExecutableName $ExactExecutablePath
}

if (-not $LibraryOnly) {
    if (-not $PSBoundParameters.ContainsKey("ProcessId")) {
        throw "ProcessId is required unless LibraryOnly is set."
    }
    if ([String]::IsNullOrWhiteSpace($ExpectedExecutablePath)) {
        throw "ExpectedExecutablePath is required."
    }
    $evidence = Measure-WokCoreProcessPhase `
        -TargetProcessId $ProcessId `
        -ExactExecutablePath $ExpectedExecutablePath `
        -PhaseName $Phase `
        -SampleDurationSeconds $DurationSeconds `
        -SampleIntervalMilliseconds $IntervalMilliseconds
    $serialized = if ($OutputFormat -eq "json") {
        $evidence | ConvertTo-Json -Depth 4 -Compress
    } else {
        ($evidence | ConvertTo-Csv -NoTypeInformation) -join [Environment]::NewLine
    }
    if ([String]::IsNullOrWhiteSpace($OutputPath)) {
        Write-Output $serialized
    } else {
        $resolvedParent = [IO.Path]::GetDirectoryName([IO.Path]::GetFullPath($OutputPath))
        if (-not [IO.Directory]::Exists($resolvedParent)) {
            throw "Evidence output parent directory does not exist."
        }
        [IO.File]::WriteAllText(
            [IO.Path]::GetFullPath($OutputPath),
            $serialized,
            [Text.UTF8Encoding]::new($false)
        )
    }
}
