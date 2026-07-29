[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string] $ArtifactDirectory,
    [Parameter(Mandatory)]
    [string] $Version,
    [Parameter(Mandatory)]
    [string] $SigningKeyId,
    [Parameter(Mandatory)]
    [ValidateSet(1, 2)]
    [int] $SchemaVersion,
    [Parameter(Mandatory)]
    [string] $OutputPath
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$semverPattern = "^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-(?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*))*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$"
if ($Version.Length -gt 128 -or $Version -cnotmatch $semverPattern) {
    throw "Release version is not canonical SemVer."
}
if ($SigningKeyId -cnotmatch "^[0-9A-F]{16}$") {
    throw "Minisign key id must be 16 uppercase hexadecimal characters."
}

$ArtifactDirectory = [IO.Path]::GetFullPath($ArtifactDirectory)
$OutputPath = [IO.Path]::GetFullPath($OutputPath)
if (-not [IO.Directory]::Exists($ArtifactDirectory)) {
    throw "Artifact directory does not exist."
}
$manifestName = "wokcore-update-v$SchemaVersion.json"
if (
    [IO.Path]::GetDirectoryName($OutputPath) -cne $ArtifactDirectory -or
    [IO.Path]::GetFileName($OutputPath) -cne $manifestName
) {
    throw "Manifest must use its fixed schema-versioned name in the artifact directory."
}

Import-Module (Join-Path $PSScriptRoot "WokCore.ReleaseContract.psm1") -Force
$contracts = if ($SchemaVersion -eq 1) {
    @(Get-WokCoreTargetContracts -Version $Version | Where-Object LegacyV1)
} else {
    @(Get-WokCoreTargetContracts -Version $Version)
}

$artifacts = foreach ($contract in $contracts) {
    $fileName = if ($SchemaVersion -eq 1) {
        $contract.LegacyV1Name
    } else {
        $contract.FriendlyPortableName
    }
    $path = Join-Path $ArtifactDirectory $fileName
    if (-not [IO.File]::Exists($path)) {
        throw "Release artifact is missing: $fileName"
    }
    $item = Get-Item -LiteralPath $path
    if (
        ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or
        $item.Length -le 0 -or
        $item.Length -gt 536870912
    ) {
        throw "Release artifact is symbolic, empty, or oversized: $fileName"
    }
    $sha256 = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
    [ordered]@{
        target = $contract.Target
        file = $fileName
        executable = $contract.Executable
        size = [long] $item.Length
        sha256 = $sha256
        url = "https://github.com/hongjiadev/wokcore/releases/download/v$Version/$fileName"
    }
}

$document = [ordered]@{
    schema_version = $SchemaVersion
    product = "wokcore"
    api_major = 1
    version = $Version
    signing_key_id = $SigningKeyId
    artifacts = @($artifacts)
}
$json = ($document | ConvertTo-Json -Depth 6 -Compress) + "`n"
$temporaryManifest = Join-Path $ArtifactDirectory (
    ".$manifestName." + [Guid]::NewGuid().ToString("N") + ".tmp"
)
try {
    [IO.File]::WriteAllText(
        $temporaryManifest,
        $json,
        [Text.UTF8Encoding]::new($false)
    )
    if ([IO.File]::Exists($OutputPath)) {
        [IO.File]::Delete($OutputPath)
    }
    [IO.File]::Move($temporaryManifest, $OutputPath)
} finally {
    if ([IO.File]::Exists($temporaryManifest)) {
        [IO.File]::Delete($temporaryManifest)
    }
}

Write-Output $OutputPath
