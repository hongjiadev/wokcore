[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string] $ArtifactDirectory,
    [Parameter(Mandatory)]
    [string] $Version,
    [Parameter(Mandatory)]
    [string] $SecretKeyPath,
    [Parameter(Mandatory)]
    [string] $PublicKeyPath,
    [Parameter(Mandatory)]
    [string] $MinisignPath
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$maximumArtifactBytes = 536870912
$maximumManifestBytes = 131072
$maximumPublicKeyBytes = 1024
$maximumSecretKeyBytes = 4096
$semverPattern = "^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-(?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*))*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$"

function Assert-NoReparsePath {
    param([Parameter(Mandatory)][string] $Path)

    $current = [IO.Path]::GetFullPath($Path)
    while ($null -ne $current) {
        if (
            [IO.File]::Exists($current) -or
            [IO.Directory]::Exists($current)
        ) {
            $item = Get-Item -LiteralPath $current -Force
            if (
                ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne
                0
            ) {
                throw "Release signing paths must not contain reparse points."
            }
        }
        $parent = [IO.Directory]::GetParent($current)
        $current = if ($null -eq $parent) { $null } else { $parent.FullName }
    }
}

function Assert-BoundedFile {
    param(
        [Parameter(Mandatory)][string] $Path,
        [Parameter(Mandatory)][long] $MaximumBytes,
        [Parameter(Mandatory)][string] $Description
    )

    if (-not [IO.File]::Exists($Path)) {
        throw "$Description is missing."
    }
    $item = Get-Item -LiteralPath $Path -Force
    if (
        ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or
        $item.Length -le 0 -or
        $item.Length -gt $MaximumBytes
    ) {
        throw "$Description is symbolic, empty, or oversized."
    }
}

function Assert-OutsideArtifactDirectory {
    param(
        [Parameter(Mandatory)][string] $Path,
        [Parameter(Mandatory)][string] $Description
    )

    $prefix = $ArtifactDirectory.TrimEnd(
        [IO.Path]::DirectorySeparatorChar,
        [IO.Path]::AltDirectorySeparatorChar
    ) + [IO.Path]::DirectorySeparatorChar
    if (
        $Path.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)
    ) {
        throw "$Description must remain outside the release bundle."
    }
}

function Invoke-MinisignSigning {
    param(
        [Parameter(Mandatory)][string] $Name,
        [Parameter(Mandatory)][string] $Path,
        [Parameter(Mandatory)][string] $Comment
    )

    & $MinisignPath `
        -S `
        -W `
        -s $SecretKeyPath `
        -m $Path `
        -x "$Path.minisig" `
        -c $Comment `
        -t "WokCore v$Version" 2>&1 |
        Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "Minisign signing failed for $Name."
    }
}

if ($Version.Length -gt 128 -or $Version -cnotmatch $semverPattern) {
    throw "Release version is not canonical SemVer."
}

$ArtifactDirectory = [IO.Path]::GetFullPath($ArtifactDirectory)
$SecretKeyPath = [IO.Path]::GetFullPath($SecretKeyPath)
$PublicKeyPath = [IO.Path]::GetFullPath($PublicKeyPath)
$MinisignPath = [IO.Path]::GetFullPath($MinisignPath)

foreach ($path in @(
    $ArtifactDirectory,
    $SecretKeyPath,
    $PublicKeyPath,
    $MinisignPath
)) {
    Assert-NoReparsePath -Path $path
}
if (-not [IO.Directory]::Exists($ArtifactDirectory)) {
    throw "Artifact directory does not exist."
}
Assert-BoundedFile `
    -Path $SecretKeyPath `
    -MaximumBytes $maximumSecretKeyBytes `
    -Description "Minisign secret key"
Assert-BoundedFile `
    -Path $PublicKeyPath `
    -MaximumBytes $maximumPublicKeyBytes `
    -Description "Minisign public key"
if (-not [IO.File]::Exists($MinisignPath)) {
    throw "Minisign executable is missing."
}
Assert-OutsideArtifactDirectory `
    -Path $SecretKeyPath `
    -Description "Minisign secret key"
Assert-OutsideArtifactDirectory `
    -Path $PublicKeyPath `
    -Description "Minisign public key"

Import-Module (Join-Path $PSScriptRoot "WokCore.ReleaseContract.psm1") -Force
$payloads = [string[]] @(
    Get-WokCorePayloadNames -Version $Version -IncludeLegacyV1
)
if ($payloads.Count -ne 19) {
    throw "Release contract must contain exactly 19 payloads."
}
$checksumNames = [string[]] @(
    $payloads +
    @("wokcore-update-v1.json", "wokcore-update-v2.json")
)
[Array]::Sort($checksumNames, [StringComparer]::Ordinal)

$actualItems = @(Get-ChildItem -LiteralPath $ArtifactDirectory -Force)
if (@($actualItems | Where-Object { $_.PSIsContainer }).Count -ne 0) {
    throw "Unsigned release input must contain files only."
}
$seen = [Collections.Generic.HashSet[string]]::new(
    [StringComparer]::OrdinalIgnoreCase
)
$actualNames = [string[]] @(
    $actualItems | ForEach-Object {
        if (
            ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or
            -not $seen.Add($_.Name)
        ) {
            throw "Unsigned release input contains symbolic or duplicate names."
        }
        $_.Name
    }
)
[Array]::Sort($actualNames, [StringComparer]::Ordinal)
if (
    [string]::Join("`n", $actualNames) -cne
    [string]::Join("`n", $checksumNames)
) {
    throw "Unsigned release input differs from the exact 21-file contract."
}

foreach ($name in $checksumNames) {
    $maximumBytes = if (
        $name -ceq "wokcore-update-v1.json" -or
        $name -ceq "wokcore-update-v2.json"
    ) {
        $maximumManifestBytes
    } else {
        $maximumArtifactBytes
    }
    Assert-BoundedFile `
        -Path (Join-Path $ArtifactDirectory $name) `
        -MaximumBytes $maximumBytes `
        -Description "Release input $name"
}

foreach ($name in $checksumNames) {
    Invoke-MinisignSigning `
        -Name $name `
        -Path (Join-Path $ArtifactDirectory $name) `
        -Comment "WokCore release asset"
}

$checksumPath = Join-Path $ArtifactDirectory "SHA256SUMS"
& (Join-Path $PSScriptRoot "write-checksums.ps1") `
    -ArtifactDirectory $ArtifactDirectory `
    -ExpectedNames $checksumNames `
    -OutputPath $checksumPath |
    Out-Null
Invoke-MinisignSigning `
    -Name "SHA256SUMS" `
    -Path $checksumPath `
    -Comment "WokCore checksums"
[IO.File]::Copy(
    $PublicKeyPath,
    (Join-Path $ArtifactDirectory "WokCore-Minisign.pub"),
    $false
)

Write-Output "WokCore release bundle signing passed"
