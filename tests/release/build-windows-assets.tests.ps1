[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "../.."))
$buildPackage = Join-Path $PSScriptRoot "build-package.ps1"
$buildWindowsAssets = Join-Path $PSScriptRoot "build-windows-assets.ps1"
$wixSource = Join-Path $repositoryRoot "release\windows\WokCore.wxs"

function Assert-Fails {
    param(
        [Parameter(Mandatory)]
        [scriptblock] $Operation,
        [Parameter(Mandatory)]
        [string] $Message
    )

    $failed = $false
    try {
        & $Operation
    } catch {
        $failed = $true
    }
    if (-not $failed) {
        throw $Message
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

function Expand-Msi {
    param(
        [Parameter(Mandatory)]
        [string] $Path,
        [Parameter(Mandatory)]
        [string] $Destination
    )

    [IO.Directory]::CreateDirectory($Destination) | Out-Null
    $process = Start-Process msiexec.exe `
        -ArgumentList @(
            "/a",
            "`"$Path`"",
            "/qn",
            "TARGETDIR=`"$Destination`""
        ) `
        -Wait `
        -PassThru `
        -WindowStyle Hidden
    if ($process.ExitCode -ne 0) {
        throw "Test MSI administrative extraction failed with exit code $($process.ExitCode)."
    }
}

foreach ($path in @($buildPackage, $buildWindowsAssets, $wixSource)) {
    if (-not [IO.File]::Exists($path)) {
        throw "Missing Windows asset implementation: $path"
    }
}
foreach ($commandName in @("candle.exe", "light.exe", "msiexec.exe")) {
    if ($null -eq (Get-Command $commandName -ErrorAction SilentlyContinue)) {
        throw "Windows asset tests require $commandName."
    }
}

$testRoot = Join-Path ([IO.Path]::GetTempPath()) (
    "wokcore windows assets test " + [Guid]::NewGuid().ToString("N")
)
$fixtureRepository = Join-Path $testRoot "repository"
$fixtureExecutable = Join-Path $testRoot "wokcore.exe"
$junctions = [Collections.Generic.List[string]]::new()

try {
    [IO.Directory]::CreateDirectory($fixtureRepository) | Out-Null
    [IO.Directory]::CreateDirectory(
        (Join-Path $fixtureRepository "release\windows")
    ) | Out-Null
    [IO.File]::Copy(
        $wixSource,
        (Join-Path $fixtureRepository "release\windows\WokCore.wxs")
    )
    [IO.File]::WriteAllBytes(
        $fixtureExecutable,
        [byte[]] @(0x4D, 0x5A, 0x57, 0x6F, 0x6B, 0x43, 0x6F, 0x72, 0x65)
    )
    foreach ($name in @(
        "LICENSE-APACHE",
        "LICENSE-MIT",
        "NOTICE.md",
        "README.md"
    )) {
        [IO.File]::WriteAllText(
            (Join-Path $fixtureRepository $name),
            "$name fixture`n",
            [Text.UTF8Encoding]::new($false)
        )
    }

    $targetContracts = @(
        [pscustomobject]@{
            Target = "x86_64-pc-windows-msvc"
            PublicArchitecture = "x86_64"
            MsiTemplate = "x64;1033"
        },
        [pscustomobject]@{
            Target = "aarch64-pc-windows-msvc"
            PublicArchitecture = "arm64"
            MsiTemplate = "Arm64;1033"
        }
    )
    foreach ($contract in $targetContracts) {
        $dist = Join-Path $testRoot $contract.PublicArchitecture
        [IO.Directory]::CreateDirectory($dist) | Out-Null
        & $buildPackage `
            -ExecutablePath $fixtureExecutable `
            -RepositoryRoot $fixtureRepository `
            -OutputDirectory $dist `
            -Version "1.2.3" `
            -Target $contract.Target

        $technicalArchive = Join-Path $dist (
            "wokcore-v1.2.3-$($contract.Target).zip"
        )
        & $buildWindowsAssets `
            -ExecutablePath $fixtureExecutable `
            -PortableArchivePath $technicalArchive `
            -RepositoryRoot $fixtureRepository `
            -OutputDirectory $dist `
            -Version "1.2.3" `
            -Target $contract.Target

        $friendlyArchive = Join-Path $dist (
            "WokCore-v1.2.3-Windows-$($contract.PublicArchitecture)-Portable.zip"
        )
        $msi = Join-Path $dist (
            "WokCore-v1.2.3-Windows-$($contract.PublicArchitecture).msi"
        )
        $expectedNames = @(
            [IO.Path]::GetFileName($friendlyArchive),
            [IO.Path]::GetFileName($msi),
            [IO.Path]::GetFileName($technicalArchive)
        ) | Sort-Object -CaseSensitive
        $actualNames = @(
            Get-ChildItem -LiteralPath $dist -File |
                Select-Object -ExpandProperty Name |
                Sort-Object -CaseSensitive
        )
        if (
            $expectedNames.Count -ne $actualNames.Count -or
            (Compare-Object $expectedNames $actualNames -CaseSensitive)
        ) {
            throw "Windows asset names are not exact for $($contract.Target)."
        }
        $technicalBytes = [IO.File]::ReadAllBytes($technicalArchive)
        $friendlyBytes = [IO.File]::ReadAllBytes($friendlyArchive)
        if (-not [Linq.Enumerable]::SequenceEqual($technicalBytes, $friendlyBytes)) {
            throw "Friendly Windows ZIP is not a byte-for-byte copy."
        }
        $template = Get-MsiTemplate -Path $msi
        if ($template -cne $contract.MsiTemplate) {
            throw "MSI template is '$template', expected '$($contract.MsiTemplate)'."
        }
        $directoryContract = Get-MsiInstallDirectory -Path $msi
        if (
            $directoryContract.Parent -cne "ProgramFiles64Folder" -or
            $directoryContract.Name -cne "WokCore"
        ) {
            throw "MSI does not target ProgramFiles64Folder\WokCore."
        }

        $extract = Join-Path $testRoot (
            "test-extract-" + $contract.PublicArchitecture
        )
        Expand-Msi -Path $msi -Destination $extract
        $installedExecutable = @(
            Get-ChildItem -LiteralPath $extract -Recurse -File |
                Where-Object { $_.Name -ceq "wokcore.exe" }
        )
        if ($installedExecutable.Count -ne 1) {
            throw "MSI must install exactly one wokcore.exe."
        }
        $installDirectory = $installedExecutable[0].Directory
        if (
            $installDirectory.Name -cne "WokCore" -or
            $installDirectory.Parent.FullName -cne $extract
        ) {
            throw "MSI administrative image uses the wrong installation directory."
        }
        $installedNames = @(
            Get-ChildItem -LiteralPath $installDirectory.FullName -File |
                Select-Object -ExpandProperty Name |
                Sort-Object -CaseSensitive
        )
        $expectedInstalledNames = @(
            "LICENSE-APACHE",
            "LICENSE-MIT",
            "NOTICE.md",
            "README.md",
            "wokcore.exe"
        ) | Sort-Object -CaseSensitive
        if (
            $installedNames.Count -ne $expectedInstalledNames.Count -or
            (Compare-Object `
                $expectedInstalledNames `
                $installedNames `
                -CaseSensitive)
        ) {
            throw "MSI payload is not exactly wokcore.exe and four documents."
        }
    }

    $armDist = Join-Path $testRoot "arm64"
    $armArchive = Join-Path $armDist (
        "wokcore-v1.2.3-aarch64-pc-windows-msvc.zip"
    )
    $common = @{
        ExecutablePath = $fixtureExecutable
        PortableArchivePath = $armArchive
        RepositoryRoot = $fixtureRepository
        OutputDirectory = (Join-Path $testRoot "rejected")
        Version = "1.2.3"
        Target = "aarch64-pc-windows-msvc"
    }

    Assert-Fails `
        -Message "Windows builder accepted a non-canonical version." `
        -Operation {
            $arguments = $common.Clone()
            $arguments.Version = "1.2"
            & $buildWindowsAssets @arguments
        }
    Assert-Fails `
        -Message "Windows builder accepted a mismatched technical ZIP name." `
        -Operation {
            $wrongArchive = Join-Path $armDist (
                "wokcore-v1.2.3-x86_64-pc-windows-msvc.zip"
            )
            [IO.File]::Copy($armArchive, $wrongArchive, $true)
            $arguments = $common.Clone()
            $arguments.PortableArchivePath = $wrongArchive
            & $buildWindowsAssets @arguments
        }

    $technicalJunction = Join-Path $testRoot "technical-junction"
    New-Item `
        -ItemType Junction `
        -Path $technicalJunction `
        -Target $armDist | Out-Null
    $junctions.Add($technicalJunction)
    Assert-Fails `
        -Message "Windows builder accepted a technical ZIP through a reparse ancestor." `
        -Operation {
            $arguments = $common.Clone()
            $arguments.PortableArchivePath = (
                Join-Path $technicalJunction (
                    "wokcore-v1.2.3-aarch64-pc-windows-msvc.zip"
                )
            )
            & $buildWindowsAssets @arguments
        }

    $executableDirectory = Join-Path $testRoot "executable-real"
    [IO.Directory]::CreateDirectory($executableDirectory) | Out-Null
    [IO.File]::Copy(
        $fixtureExecutable,
        (Join-Path $executableDirectory "wokcore.exe")
    )
    $executableJunction = Join-Path $testRoot "executable-junction"
    New-Item `
        -ItemType Junction `
        -Path $executableJunction `
        -Target $executableDirectory | Out-Null
    $junctions.Add($executableJunction)
    Assert-Fails `
        -Message "Windows builder accepted an executable through a reparse ancestor." `
        -Operation {
            $arguments = $common.Clone()
            $arguments.ExecutablePath = (
                Join-Path $executableJunction "wokcore.exe"
            )
            & $buildWindowsAssets @arguments
        }

    $repositoryJunction = Join-Path $testRoot "repository-junction"
    New-Item `
        -ItemType Junction `
        -Path $repositoryJunction `
        -Target $fixtureRepository | Out-Null
    $junctions.Add($repositoryJunction)
    Assert-Fails `
        -Message "Windows builder accepted a repository through a reparse ancestor." `
        -Operation {
            $arguments = $common.Clone()
            $arguments.RepositoryRoot = $repositoryJunction
            & $buildWindowsAssets @arguments
        }

    $outputReal = Join-Path $testRoot "output-real"
    [IO.Directory]::CreateDirectory($outputReal) | Out-Null
    $outputJunction = Join-Path $testRoot "output-junction"
    New-Item `
        -ItemType Junction `
        -Path $outputJunction `
        -Target $outputReal | Out-Null
    $junctions.Add($outputJunction)
    Assert-Fails `
        -Message "Windows builder accepted an output through a reparse ancestor." `
        -Operation {
            $arguments = $common.Clone()
            $arguments.OutputDirectory = (
                Join-Path $outputJunction "nested"
            )
            & $buildWindowsAssets @arguments
        }

    $fakeWix = Join-Path $testRoot "fake-wix"
    [IO.Directory]::CreateDirectory($fakeWix) | Out-Null
    [IO.File]::Copy(
        (Join-Path $env:SystemRoot "System32\cmd.exe"),
        (Join-Path $fakeWix "candle.exe")
    )
    [IO.File]::Copy(
        (Join-Path $env:SystemRoot "System32\cmd.exe"),
        (Join-Path $fakeWix "light.exe")
    )
    $savedPath = $env:PATH
    try {
        $env:PATH = "$fakeWix;$savedPath"
        Assert-Fails `
            -Message "Windows builder accepted an incompatible WiX CLI." `
            -Operation {
                & $buildWindowsAssets @common
            }
    } finally {
        $env:PATH = $savedPath
    }

    Write-Output "Windows asset builder tests passed: x86_64 and arm64"
} finally {
    foreach ($junction in $junctions) {
        if ([IO.Directory]::Exists($junction)) {
            [IO.Directory]::Delete($junction)
        }
    }
    if ([IO.Directory]::Exists($testRoot)) {
        [IO.Directory]::Delete($testRoot, $true)
    }
}
