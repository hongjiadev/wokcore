[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string] $ExecutablePath,
    [Parameter(Mandatory)]
    [string] $RepositoryRoot,
    [Parameter(Mandatory)]
    [string] $OutputDirectory,
    [Parameter(Mandatory)]
    [string] $Version,
    [Parameter(Mandatory)]
    [ValidateSet("x86_64-pc-windows-msvc")]
    [string] $Target
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$semverPattern = "^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-(?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*))*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$"
if ($Version.Length -gt 128 -or $Version -cnotmatch $semverPattern) {
    throw "Release version is not canonical SemVer."
}

$ExecutablePath = [IO.Path]::GetFullPath($ExecutablePath)
$RepositoryRoot = [IO.Path]::GetFullPath($RepositoryRoot)
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
if (
    -not [IO.File]::Exists($ExecutablePath) -or
    [IO.Path]::GetFileName($ExecutablePath) -cne "wokcore.exe"
) {
    throw "The Windows release executable must be the fixed wokcore.exe file."
}
$executableInfo = Get-Item -LiteralPath $ExecutablePath
if (
    ($executableInfo.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0
) {
    throw "The release executable cannot be a reparse point."
}

$entries = @(
    [pscustomobject]@{ Source = $ExecutablePath; Name = "wokcore.exe" },
    [pscustomobject]@{
        Source = Join-Path $RepositoryRoot "LICENSE-APACHE"
        Name = "LICENSE-APACHE"
    },
    [pscustomobject]@{
        Source = Join-Path $RepositoryRoot "LICENSE-MIT"
        Name = "LICENSE-MIT"
    },
    [pscustomobject]@{
        Source = Join-Path $RepositoryRoot "NOTICE.md"
        Name = "NOTICE.md"
    },
    [pscustomobject]@{
        Source = Join-Path $RepositoryRoot "README.md"
        Name = "README.md"
    }
)
foreach ($entry in $entries) {
    if (-not [IO.File]::Exists($entry.Source)) {
        throw "Release package input is missing: $($entry.Source)"
    }
    $info = Get-Item -LiteralPath $entry.Source
    if (($info.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "Release package inputs cannot be reparse points."
    }
}

[IO.Directory]::CreateDirectory($OutputDirectory) | Out-Null
$archiveName = "wokcore-v$Version-$Target.zip"
$archivePath = Join-Path $OutputDirectory $archiveName
$temporaryPath = Join-Path $OutputDirectory (
    ".$archiveName." + [Guid]::NewGuid().ToString("N") + ".tmp"
)

Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

try {
    $output = [IO.FileStream]::new(
        $temporaryPath,
        [IO.FileMode]::CreateNew,
        [IO.FileAccess]::Write,
        [IO.FileShare]::None
    )
    try {
        $archive = [IO.Compression.ZipArchive]::new(
            $output,
            [IO.Compression.ZipArchiveMode]::Create,
            $false,
            [Text.Encoding]::UTF8
        )
        try {
            foreach ($item in $entries) {
                $zipEntry = $archive.CreateEntry(
                    $item.Name,
                    [IO.Compression.CompressionLevel]::Optimal
                )
                $zipEntry.LastWriteTime = [DateTimeOffset]::new(
                    1980,
                    1,
                    1,
                    0,
                    0,
                    0,
                    [TimeSpan]::Zero
                )
                $source = [IO.File]::Open(
                    $item.Source,
                    [IO.FileMode]::Open,
                    [IO.FileAccess]::Read,
                    [IO.FileShare]::Read
                )
                try {
                    $destination = $zipEntry.Open()
                    try {
                        $source.CopyTo($destination)
                    } finally {
                        $destination.Dispose()
                    }
                } finally {
                    $source.Dispose()
                }
            }
        } finally {
            $archive.Dispose()
        }
    } finally {
        $output.Dispose()
    }

    if ([IO.File]::Exists($archivePath)) {
        [IO.File]::Delete($archivePath)
    }
    [IO.File]::Move($temporaryPath, $archivePath)
} finally {
    if ([IO.File]::Exists($temporaryPath)) {
        [IO.File]::Delete($temporaryPath)
    }
}

Write-Output $archivePath
