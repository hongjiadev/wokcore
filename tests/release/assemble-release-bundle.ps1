[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string] $IntermediateDirectory,
    [Parameter(Mandatory)]
    [string] $ArtifactDirectory,
    [Parameter(Mandatory)]
    [string] $Version,
    [Parameter(Mandatory)]
    [string] $SigningKeyId
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

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
                throw "Release assembly paths must not contain reparse points."
            }
        }
        $parent = [IO.Directory]::GetParent($current)
        $current = if ($null -eq $parent) { $null } else { $parent.FullName }
    }
}

$IntermediateDirectory = [IO.Path]::GetFullPath($IntermediateDirectory)
$ArtifactDirectory = [IO.Path]::GetFullPath($ArtifactDirectory)
foreach ($path in @($IntermediateDirectory, $ArtifactDirectory)) {
    Assert-NoReparsePath -Path $path
}
if (-not [IO.Directory]::Exists($IntermediateDirectory)) {
    throw "Intermediate directory does not exist."
}

Import-Module (Join-Path $PSScriptRoot "WokCore.ReleaseContract.psm1") -Force
$payloadNames = @(
    Get-WokCorePayloadNames -Version $Version -IncludeLegacyV1
)
$intermediateItems = @(
    Get-ChildItem -LiteralPath $IntermediateDirectory -Recurse -Force
)
foreach ($item in $intermediateItems) {
    if (
        ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0
    ) {
        throw "Intermediate payload paths must not contain reparse points."
    }
}
$intermediateFiles = @($intermediateItems | Where-Object { -not $_.PSIsContainer })
$seenNames = [Collections.Generic.HashSet[string]]::new(
    [StringComparer]::OrdinalIgnoreCase
)
foreach ($file in $intermediateFiles) {
    if (-not $seenNames.Add($file.Name)) {
        throw "Intermediate payload names must be unique ignoring case."
    }
}
$actualNames = [string[]] @($intermediateFiles | ForEach-Object Name)
$expectedNames = [string[]] @($payloadNames)
[Array]::Sort($actualNames, [StringComparer]::Ordinal)
[Array]::Sort($expectedNames, [StringComparer]::Ordinal)
if (
    $actualNames.Count -ne 19 -or
    [string]::Join("`n", $actualNames) -cne
    [string]::Join("`n", $expectedNames)
) {
    throw "Intermediate payloads differ from the exact 19-file contract."
}

$payloadSources = [Collections.Generic.List[IO.FileInfo]]::new()
foreach ($name in $payloadNames) {
    $matches = @(
        $intermediateFiles | Where-Object { $_.Name -ceq $name }
    )
    if ($matches.Count -ne 1) {
        throw "Expected one intermediate file named $name."
    }
    $payloadSources.Add($matches[0])
}

if ([IO.Directory]::Exists($ArtifactDirectory)) {
    if (@(Get-ChildItem -LiteralPath $ArtifactDirectory -Force).Count -ne 0) {
        throw "Artifact directory must be empty."
    }
}
$artifactParent = [IO.Path]::GetDirectoryName($ArtifactDirectory)
if (
    [string]::IsNullOrEmpty($artifactParent) -or
    -not [IO.Directory]::Exists($artifactParent)
) {
    throw "Artifact directory parent does not exist."
}
$stagingDirectory = Join-Path $artifactParent (
    "." + [IO.Path]::GetFileName($ArtifactDirectory) + "." +
    [Guid]::NewGuid().ToString("N") + ".tmp"
)
[IO.Directory]::CreateDirectory($stagingDirectory) | Out-Null
try {
    for ($index = 0; $index -lt $payloadNames.Count; $index++) {
        [IO.File]::Copy(
            $payloadSources[$index].FullName,
            (Join-Path $stagingDirectory $payloadNames[$index]),
            $false
        )
    }

    $writeManifest = Join-Path $PSScriptRoot "write-manifest.ps1"
    foreach ($schemaVersion in @(1, 2)) {
        & $writeManifest `
            -ArtifactDirectory $stagingDirectory `
            -Version $Version `
            -SigningKeyId $SigningKeyId `
            -SchemaVersion $schemaVersion `
            -OutputPath (
                Join-Path `
                    $stagingDirectory `
                    "wokcore-update-v$schemaVersion.json"
            ) |
            Out-Null
    }

    if ([IO.Directory]::Exists($ArtifactDirectory)) {
        [IO.Directory]::Delete($ArtifactDirectory, $false)
    }
    [IO.Directory]::Move($stagingDirectory, $ArtifactDirectory)
} finally {
    if ([IO.Directory]::Exists($stagingDirectory)) {
        [IO.Directory]::Delete($stagingDirectory, $true)
    }
}

Write-Output "WokCore release bundle assembly passed"
