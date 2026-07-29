[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string] $ExecutablePath,
    [Parameter(Mandatory)]
    [string] $PortableArchivePath,
    [Parameter(Mandatory)]
    [string] $RepositoryRoot,
    [Parameter(Mandatory)]
    [string] $OutputDirectory,
    [Parameter(Mandatory)]
    [string] $Version,
    [Parameter(Mandatory)]
    [ValidateSet(
        "x86_64-pc-windows-msvc",
        "aarch64-pc-windows-msvc"
    )]
    [string] $Target
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Get-ValidatedPath {
    param(
        [Parameter(Mandatory)]
        [string] $Path,
        [Parameter(Mandatory)]
        [string] $Description
    )

    if ([string]::IsNullOrWhiteSpace($Path)) {
        throw "The $Description path is required."
    }
    $candidate = if ([IO.Path]::IsPathRooted($Path)) {
        $Path
    } else {
        Join-Path (Get-Location).Path $Path
    }
    $root = [IO.Path]::GetPathRoot($candidate)
    if ([string]::IsNullOrEmpty($root)) {
        throw "The $Description path is invalid."
    }
    $cursor = $root
    $remainder = $candidate.Substring($root.Length)
    foreach ($segment in $remainder.Split(
        [char[]] @([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar),
        [StringSplitOptions]::RemoveEmptyEntries
    )) {
        if ($segment -ceq ".") {
            continue
        }
        if ($segment -ceq "..") {
            $parent = [IO.Directory]::GetParent($cursor)
            if ($null -ne $parent) {
                $cursor = $parent.FullName
            }
            continue
        }
        $cursor = Join-Path $cursor $segment
        if ([IO.File]::Exists($cursor) -or [IO.Directory]::Exists($cursor)) {
            $attributes = [IO.File]::GetAttributes($cursor)
            if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "The $Description path cannot contain a reparse point."
            }
        }
    }
    return [IO.Path]::GetFullPath($candidate)
}

function Get-WixTool {
    param(
        [Parameter(Mandatory)]
        [string] $Name
    )

    $command = Get-Command $Name -CommandType Application -ErrorAction SilentlyContinue
    if ($null -eq $command) {
        throw "WiX 3.14.1 $Name is required."
    }
    $toolPath = Get-ValidatedPath `
        -Path $command.Source `
        -Description "WiX $Name"
    $versionOutput = (& $toolPath "-?" 2>&1 | Out-String)
    if (
        $LASTEXITCODE -ne 0 -or
        $versionOutput -cnotmatch "\bversion 3\.14\.1(?:\.[0-9]+)?\b"
    ) {
        throw "WiX $Name must report version 3.14.1."
    }
    return $toolPath
}

function Remove-ValidatedTemporaryDirectory {
    param(
        [Parameter(Mandatory)]
        [string] $Path
    )

    $fullPath = Get-ValidatedPath -Path $Path -Description "temporary directory"
    $temporaryRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    $expectedPrefix = $temporaryRoot.TrimEnd(
        [IO.Path]::DirectorySeparatorChar,
        [IO.Path]::AltDirectorySeparatorChar
    ) + [IO.Path]::DirectorySeparatorChar
    if (
        -not $fullPath.StartsWith(
            $expectedPrefix,
            [StringComparison]::OrdinalIgnoreCase
        ) -or
        [IO.Directory]::GetParent($fullPath).FullName -cne $temporaryRoot.TrimEnd(
            [IO.Path]::DirectorySeparatorChar,
            [IO.Path]::AltDirectorySeparatorChar
        ) -or
        [IO.Path]::GetFileName($fullPath) -cnotmatch (
            "^wokcore-(?:windows-assets|msi)-[0-9a-f]{32}$"
        )
    ) {
        throw "Refusing to recursively remove an untrusted temporary directory."
    }
    if ([IO.Directory]::Exists($fullPath)) {
        $resolved = (Resolve-Path -LiteralPath $fullPath).Path
        if ($resolved -cne $fullPath) {
            throw "Refusing to recursively remove a redirected temporary directory."
        }
        [IO.Directory]::Delete($fullPath, $true)
    }
}

function Get-MsiTemplate {
    param(
        [Parameter(Mandatory)]
        [string] $Path
    )

    $installer = $null
    $database = $null
    $summary = $null
    try {
        $installer = New-Object -ComObject WindowsInstaller.Installer
        $database = $installer.OpenDatabase($Path, 0)
        $summary = $database.SummaryInformation(0)
        return [string] $summary.Property(7)
    } finally {
        foreach ($item in @($summary, $database, $installer)) {
            if ($null -ne $item -and [Runtime.InteropServices.Marshal]::IsComObject($item)) {
                [void] [Runtime.InteropServices.Marshal]::FinalReleaseComObject($item)
            }
        }
    }
}

function Get-MsiInstallDirectory {
    param(
        [Parameter(Mandatory)]
        [string] $Path
    )

    $installer = $null
    $database = $null
    $view = $null
    $record = $null
    try {
        $installer = New-Object -ComObject WindowsInstaller.Installer
        $database = $installer.OpenDatabase($Path, 0)
        $view = $database.OpenView(
            "SELECT ``Directory_Parent``, ``DefaultDir`` " +
            "FROM ``Directory`` WHERE ``Directory``='INSTALLFOLDER'"
        )
        [void] $view.Execute()
        $record = $view.Fetch()
        if ($null -eq $record) {
            throw "MSI INSTALLFOLDER row is missing."
        }
        return [pscustomobject]@{
            Parent = [string] $record.StringData(1)
            Name = [string] $record.StringData(2)
        }
    } finally {
        foreach ($item in @($record, $view, $database, $installer)) {
            if ($null -ne $item -and [Runtime.InteropServices.Marshal]::IsComObject($item)) {
                [void] [Runtime.InteropServices.Marshal]::FinalReleaseComObject($item)
            }
        }
    }
}

function Get-MsiFileRows {
    param(
        [Parameter(Mandatory)]
        [string] $Path
    )

    $installer = $null
    $database = $null
    $view = $null
    $record = $null
    try {
        $installer = New-Object -ComObject WindowsInstaller.Installer
        $database = $installer.OpenDatabase($Path, 0)
        $view = $database.OpenView(
            "SELECT ``File``.``File``, ``File``.``FileName``, " +
            "``Component``.``Directory_`` " +
            "FROM ``File``, ``Component`` " +
            "WHERE ``File``.``Component_`` = ``Component``.``Component``"
        )
        [void] $view.Execute()
        while ($null -ne ($record = $view.Fetch())) {
            $encodedName = [string] $record.StringData(2)
            $separator = $encodedName.IndexOf(
                "|",
                [StringComparison]::Ordinal
            )
            $targetName = if ($separator -ge 0) {
                $encodedName.Substring($separator + 1)
            } else {
                $encodedName
            }
            [pscustomobject]@{
                Id = [string] $record.StringData(1)
                TargetName = $targetName
                Directory = [string] $record.StringData(3)
            }
            [void] [Runtime.InteropServices.Marshal]::FinalReleaseComObject(
                $record
            )
            $record = $null
        }
    } finally {
        foreach ($item in @($record, $view, $database, $installer)) {
            if ($null -ne $item -and [Runtime.InteropServices.Marshal]::IsComObject($item)) {
                [void] [Runtime.InteropServices.Marshal]::FinalReleaseComObject($item)
            }
        }
    }
}

if ($Version -cnotmatch "^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$") {
    throw "Windows MSI version must be canonical x.y.z."
}

$architecture = if ($Target -ceq "x86_64-pc-windows-msvc") {
    [pscustomobject]@{
        Public = "x86_64"
        Wix = "x64"
        MsiTemplate = "x64;1033"
    }
} else {
    [pscustomobject]@{
        Public = "arm64"
        Wix = "arm64"
        MsiTemplate = "Arm64;1033"
    }
}

$ExecutablePath = Get-ValidatedPath `
    -Path $ExecutablePath `
    -Description "release executable"
$PortableArchivePath = Get-ValidatedPath `
    -Path $PortableArchivePath `
    -Description "technical archive"
$RepositoryRoot = Get-ValidatedPath `
    -Path $RepositoryRoot `
    -Description "repository root"
$OutputDirectory = Get-ValidatedPath `
    -Path $OutputDirectory `
    -Description "output directory"

if (
    -not [IO.File]::Exists($ExecutablePath) -or
    [IO.Path]::GetFileName($ExecutablePath) -cne "wokcore.exe"
) {
    throw "The Windows release executable must be the fixed wokcore.exe file."
}
$expectedArchiveName = "wokcore-v$Version-$Target.zip"
if (
    -not [IO.File]::Exists($PortableArchivePath) -or
    [IO.Path]::GetFileName($PortableArchivePath) -cne $expectedArchiveName
) {
    throw "The technical archive name must match the version and target."
}
if (-not [IO.Directory]::Exists($RepositoryRoot)) {
    throw "The repository root is missing."
}

$wixSource = Get-ValidatedPath `
    -Path (Join-Path $RepositoryRoot "release\windows\WokCore.wxs") `
    -Description "WiX source"
$documents = @(
    [pscustomobject]@{
        Name = "LICENSE-APACHE"
        Path = Get-ValidatedPath `
            -Path (Join-Path $RepositoryRoot "LICENSE-APACHE") `
            -Description "release document"
    },
    [pscustomobject]@{
        Name = "LICENSE-MIT"
        Path = Get-ValidatedPath `
            -Path (Join-Path $RepositoryRoot "LICENSE-MIT") `
            -Description "release document"
    },
    [pscustomobject]@{
        Name = "NOTICE.md"
        Path = Get-ValidatedPath `
            -Path (Join-Path $RepositoryRoot "NOTICE.md") `
            -Description "release document"
    },
    [pscustomobject]@{
        Name = "README.md"
        Path = Get-ValidatedPath `
            -Path (Join-Path $RepositoryRoot "README.md") `
            -Description "release document"
    }
)
foreach ($path in @($wixSource) + @($documents.Path)) {
    if (-not [IO.File]::Exists($path)) {
        throw "Windows release input is missing: $path"
    }
}

$candle = Get-WixTool -Name "candle.exe"
$light = Get-WixTool -Name "light.exe"
[IO.Directory]::CreateDirectory($OutputDirectory) | Out-Null
if (-not [IO.Directory]::Exists($OutputDirectory)) {
    throw "The output directory is not a regular directory."
}

$friendlyPrefix = "WokCore-v$Version-Windows-$($architecture.Public)"
$friendlyArchive = Get-ValidatedPath `
    -Path (Join-Path $OutputDirectory "$friendlyPrefix-Portable.zip") `
    -Description "portable output"
$msiPath = Get-ValidatedPath `
    -Path (Join-Path $OutputDirectory "$friendlyPrefix.msi") `
    -Description "MSI output"

$workRoot = Join-Path ([IO.Path]::GetTempPath()) (
    "wokcore-windows-assets-" + [Guid]::NewGuid().ToString("N")
)
$extractRoot = Join-Path ([IO.Path]::GetTempPath()) (
    "wokcore-msi-" + [Guid]::NewGuid().ToString("N")
)
[IO.Directory]::CreateDirectory($workRoot) | Out-Null
try {
    $wixObject = Join-Path $workRoot "WokCore.wixobj"
    $temporaryMsi = Join-Path $workRoot "$friendlyPrefix.msi"
    $temporaryArchive = Join-Path $workRoot "$friendlyPrefix-Portable.zip"

    [IO.File]::Copy($PortableArchivePath, $temporaryArchive)
    $sourceBytes = [IO.File]::ReadAllBytes($PortableArchivePath)
    $copiedBytes = [IO.File]::ReadAllBytes($temporaryArchive)
    if (-not [Linq.Enumerable]::SequenceEqual($sourceBytes, $copiedBytes)) {
        throw "The friendly portable archive is not a byte-for-byte copy."
    }

    & $candle `
        "-nologo" `
        "-arch" $architecture.Wix `
        "-dWokCoreVersion=$Version" `
        "-dWokCorePlatform=$($architecture.Wix)" `
        "-dWokCoreExe=$ExecutablePath" `
        "-dApacheLicense=$($documents[0].Path)" `
        "-dMitLicense=$($documents[1].Path)" `
        "-dNotice=$($documents[2].Path)" `
        "-dReadme=$($documents[3].Path)" `
        "-out" $wixObject `
        $wixSource
    if ($LASTEXITCODE -ne 0 -or -not [IO.File]::Exists($wixObject)) {
        throw "WiX candle.exe failed to compile the WokCore MSI."
    }
    & $light `
        "-nologo" `
        "-out" $temporaryMsi `
        $wixObject
    if ($LASTEXITCODE -ne 0 -or -not [IO.File]::Exists($temporaryMsi)) {
        throw "WiX light.exe failed to link the WokCore MSI."
    }

    $template = Get-MsiTemplate -Path $temporaryMsi
    if ($template -cne $architecture.MsiTemplate) {
        throw "The WokCore MSI architecture is '$template', expected '$($architecture.MsiTemplate)'."
    }
    $directoryContract = Get-MsiInstallDirectory -Path $temporaryMsi
    if (
        $directoryContract.Parent -cne "ProgramFiles64Folder" -or
        $directoryContract.Name -cne "WokCore"
    ) {
        throw "MSI does not target ProgramFiles64Folder\WokCore."
    }
    $fileRows = @(Get-MsiFileRows -Path $temporaryMsi)
    $actualFileContracts = @(
        $fileRows |
            ForEach-Object {
                "$($_.Id)|$($_.TargetName)|$($_.Directory)"
            } |
            Sort-Object -CaseSensitive
    )
    $expectedFileContracts = @(
        "ApacheLicense|LICENSE-APACHE|INSTALLFOLDER",
        "MitLicense|LICENSE-MIT|INSTALLFOLDER",
        "Notice|NOTICE.md|INSTALLFOLDER",
        "Readme|README.md|INSTALLFOLDER",
        "WokCoreExe|wokcore.exe|INSTALLFOLDER"
    ) | Sort-Object -CaseSensitive
    if (
        $actualFileContracts.Count -ne $expectedFileContracts.Count -or
        (Compare-Object `
            $expectedFileContracts `
            $actualFileContracts `
            -CaseSensitive)
    ) {
        throw "MSI File table must contain exactly the five WokCore files in INSTALLFOLDER."
    }

    [IO.Directory]::CreateDirectory($extractRoot) | Out-Null
    $process = Start-Process msiexec.exe `
        -ArgumentList @(
            "/a",
            "`"$temporaryMsi`"",
            "/qn",
            "TARGETDIR=`"$extractRoot`""
        ) `
        -Wait `
        -PassThru `
        -WindowStyle Hidden
    if ($process.ExitCode -ne 0) {
        throw "MSI administrative extraction failed."
    }
    $installedExecutables = @(
        Get-ChildItem -LiteralPath $extractRoot -Recurse -File |
            Where-Object { $_.Name -ceq "wokcore.exe" }
    )
    if ($installedExecutables.Count -ne 1) {
        throw "MSI does not contain exactly one wokcore.exe."
    }
    $installDirectory = $installedExecutables[0].Directory
    if (
        $installDirectory.Name -cne "WokCore" -or
        $installDirectory.Parent.FullName -cne $extractRoot
    ) {
        throw "MSI administrative image has the wrong installation directory."
    }
    $actualNames = @(
        Get-ChildItem -LiteralPath $installDirectory.FullName -File |
            Select-Object -ExpandProperty Name |
            Sort-Object -CaseSensitive
    )
    $expectedNames = @(
        "LICENSE-APACHE",
        "LICENSE-MIT",
        "NOTICE.md",
        "README.md",
        "wokcore.exe"
    ) | Sort-Object -CaseSensitive
    if (
        $actualNames.Count -ne $expectedNames.Count -or
        (Compare-Object $expectedNames $actualNames -CaseSensitive)
    ) {
        throw "MSI payload must contain exactly wokcore.exe and four documents."
    }
    $expectedPayloads = @(
        [pscustomobject]@{
            Name = "wokcore.exe"
            Source = $ExecutablePath
        }
    ) + @(
        $documents | ForEach-Object {
            [pscustomobject]@{
                Name = $_.Name
                Source = $_.Path
            }
        }
    )
    foreach ($payload in $expectedPayloads) {
        $sourceBytes = [IO.File]::ReadAllBytes($payload.Source)
        $installedBytes = [IO.File]::ReadAllBytes(
            (Join-Path $installDirectory.FullName $payload.Name)
        )
        if (-not [Linq.Enumerable]::SequenceEqual(
            $sourceBytes,
            $installedBytes
        )) {
            throw "MSI payload bytes differ for $($payload.Name)."
        }
    }

    foreach ($destination in @($friendlyArchive, $msiPath)) {
        if ([IO.File]::Exists($destination)) {
            [IO.File]::Delete($destination)
        }
    }
    [IO.File]::Move($temporaryArchive, $friendlyArchive)
    [IO.File]::Move($temporaryMsi, $msiPath)
} finally {
    Remove-ValidatedTemporaryDirectory -Path $extractRoot
    Remove-ValidatedTemporaryDirectory -Path $workRoot
}

Write-Output $friendlyArchive
Write-Output $msiPath
