[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string] $ManifestPath,
    [Parameter(Mandatory)]
    [string] $SignaturePath,
    [Parameter(Mandatory)]
    [string] $PublicKeyPath,
    [Parameter(Mandatory)]
    [string] $ArtifactDirectory,
    [Parameter(Mandatory)]
    [string] $ChecksumsPath,
    [Parameter(Mandatory)]
    [string] $ExpectedVersion,
    [Parameter(Mandatory)]
    [string] $ExpectedSigningKeyId,
    [Parameter(Mandatory)]
    [string] $MinisignPath
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$maximumManifestBytes = 131072
$maximumSignatureBytes = 4096
$maximumPublicKeyBytes = 1024
$maximumChecksumsBytes = 8192
$maximumArtifactBytes = 536870912
$semverPattern = "^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-(?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*))*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$"

function Read-BoundedUtf8 {
    param(
        [Parameter(Mandatory)]
        [string] $Path,
        [Parameter(Mandatory)]
        [long] $MaximumBytes
    )

    if (-not [IO.File]::Exists($Path)) {
        throw "Required release input is missing: $Path"
    }
    $info = Get-Item -LiteralPath $Path
    if (
        ($info.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or
        $info.Length -le 0 -or
        $info.Length -gt $MaximumBytes
    ) {
        throw "Release input is symbolic, empty, or oversized: $Path"
    }
    $bytes = [IO.File]::ReadAllBytes($Path)
    if (
        $bytes.Length -ge 3 -and
        $bytes[0] -eq 0xef -and
        $bytes[1] -eq 0xbb -and
        $bytes[2] -eq 0xbf
    ) {
        throw "Release text inputs must be UTF-8 without a byte-order mark."
    }
    return [Text.UTF8Encoding]::new($false, $true).GetString($bytes)
}

function Test-JsonInteger {
    param(
        [Parameter(Mandatory)]
        [object] $Value
    )

    return (
        $Value -is [sbyte] -or
        $Value -is [byte] -or
        $Value -is [int16] -or
        $Value -is [uint16] -or
        $Value -is [int32] -or
        $Value -is [uint32] -or
        $Value -is [int64] -or
        $Value -is [uint64]
    )
}

function Get-MinisignPublicKeyId {
    param(
        [Parameter(Mandatory)]
        [string] $PublicKeyText
    )

    $normalized = $PublicKeyText.Replace("`r`n", "`n").TrimEnd("`n")
    $lines = @($normalized.Split("`n"))
    if (
        $lines.Count -ne 2 -or
        $lines[0] -cnotmatch
            "^untrusted comment: minisign public key ([0-9A-F]{16})$"
    ) {
        throw "Minisign public key text has an invalid shape."
    }
    $commentKeyId = $Matches[1]
    try {
        [byte[]] $decoded = [Convert]::FromBase64String($lines[1])
    } catch {
        throw "Minisign public key payload is not valid base64."
    }
    if (
        $decoded.Length -ne 42 -or
        $decoded[0] -ne 0x45 -or
        $decoded[1] -ne 0x64
    ) {
        throw "Minisign public key payload is not an Ed25519 key."
    }
    [byte[]] $keyIdBytes = $decoded[2..9]
    [Array]::Reverse($keyIdBytes)
    $payloadKeyId = [BitConverter]::ToString($keyIdBytes).Replace("-", "")
    if ($payloadKeyId -cne $commentKeyId) {
        throw "Minisign public key comment does not match its key payload."
    }
    return $payloadKeyId
}

function Assert-ExactProperties {
    param(
        [Parameter(Mandatory)]
        [object] $Value,
        [Parameter(Mandatory)]
        [string[]] $Expected,
        [Parameter(Mandatory)]
        [string] $Context
    )

    $actual = @($Value.PSObject.Properties.Name | Sort-Object)
    $expectedSorted = @($Expected | Sort-Object)
    if (
        [string]::Join("`n", $actual) -cne
        [string]::Join("`n", $expectedSorted)
    ) {
        throw "$Context contains missing or unknown fields."
    }
}

function Get-ArchiveEntries {
    param(
        [Parameter(Mandatory)]
        [string] $Path,
        [Parameter(Mandatory)]
        [string] $FileName
    )

    if ($FileName.EndsWith(".zip", [StringComparison]::Ordinal)) {
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $archive = [IO.Compression.ZipFile]::OpenRead($Path)
        try {
            return @($archive.Entries | ForEach-Object FullName)
        } finally {
            $archive.Dispose()
        }
    }
    if ($FileName.EndsWith(".tar.gz", [StringComparison]::Ordinal)) {
        $tar = Get-Command tar -ErrorAction Stop
        $entries = @(& $tar.Source -tzf $Path)
        if ($LASTEXITCODE -ne 0) {
            throw "Release tar archive cannot be listed."
        }
        return $entries
    }
    throw "Release archive extension is invalid."
}

$ManifestPath = [IO.Path]::GetFullPath($ManifestPath)
$SignaturePath = [IO.Path]::GetFullPath($SignaturePath)
$PublicKeyPath = [IO.Path]::GetFullPath($PublicKeyPath)
$ArtifactDirectory = [IO.Path]::GetFullPath($ArtifactDirectory)
$ChecksumsPath = [IO.Path]::GetFullPath($ChecksumsPath)
$MinisignPath = [IO.Path]::GetFullPath($MinisignPath)

if (
    $ExpectedVersion.Length -gt 128 -or
    $ExpectedVersion -cnotmatch $semverPattern
) {
    throw "Expected release version is not canonical SemVer."
}
if ($ExpectedSigningKeyId -cnotmatch "^[0-9A-F]{16}$") {
    throw "Expected Minisign key id is invalid."
}
if (
    -not [IO.Directory]::Exists($ArtifactDirectory) -or
    -not [IO.File]::Exists($MinisignPath)
) {
    throw "Artifact directory or Minisign executable is missing."
}
$manifestName = [IO.Path]::GetFileName($ManifestPath)
$fileSchemaVersion = if ($manifestName -ceq "wokcore-update-v1.json") {
    1
} elseif ($manifestName -ceq "wokcore-update-v2.json") {
    2
} else {
    0
}
if (
    [IO.Path]::GetDirectoryName($ManifestPath) -cne $ArtifactDirectory -or
    $fileSchemaVersion -eq 0 -or
    [IO.Path]::GetDirectoryName($SignaturePath) -cne $ArtifactDirectory -or
    [IO.Path]::GetFileName($SignaturePath) -cne "$manifestName.minisig" -or
    [IO.Path]::GetDirectoryName($ChecksumsPath) -cne $ArtifactDirectory -or
    [IO.Path]::GetFileName($ChecksumsPath) -cne "SHA256SUMS"
) {
    throw "Release metadata must use fixed names in the artifact directory."
}

$manifestText = Read-BoundedUtf8 `
    -Path $ManifestPath `
    -MaximumBytes $maximumManifestBytes
$null = Read-BoundedUtf8 `
    -Path $SignaturePath `
    -MaximumBytes $maximumSignatureBytes
$publicKeyText = Read-BoundedUtf8 `
    -Path $PublicKeyPath `
    -MaximumBytes $maximumPublicKeyBytes
$checksumsText = Read-BoundedUtf8 `
    -Path $ChecksumsPath `
    -MaximumBytes $maximumChecksumsBytes

try {
    $document = $manifestText | ConvertFrom-Json
} catch {
    throw "Release manifest is not valid JSON."
}
$canonicalManifest = ($document | ConvertTo-Json -Depth 6 -Compress) + "`n"
if ($manifestText -cne $canonicalManifest) {
    throw "Release manifest is not in its unique canonical JSON form."
}
Assert-ExactProperties `
    -Value $document `
    -Expected @(
        "schema_version",
        "product",
        "api_major",
        "version",
        "signing_key_id",
        "artifacts"
    ) `
    -Context "Release manifest"
if (
    -not (Test-JsonInteger -Value $document.schema_version) -or
    [int64] $document.schema_version -ne $fileSchemaVersion -or
    $document.product -isnot [string] -or
    $document.product -cne "wokcore" -or
    -not (Test-JsonInteger -Value $document.api_major) -or
    [int64] $document.api_major -ne 1 -or
    $document.version -isnot [string] -or
    $document.version -cne $ExpectedVersion -or
    $document.version -cnotmatch $semverPattern -or
    $document.signing_key_id -isnot [string] -or
    $document.artifacts -isnot [array] -or
    $document.signing_key_id -cne $ExpectedSigningKeyId
) {
    throw "Release manifest identity or version is invalid."
}
$publicKeyId = Get-MinisignPublicKeyId -PublicKeyText $publicKeyText
if (
    $publicKeyId -cne $ExpectedSigningKeyId -or
    $document.signing_key_id -cne $publicKeyId
) {
    throw "Release manifest signing key id does not match the public key."
}

& $MinisignPath `
    -V `
    -q `
    -m $ManifestPath `
    -x $SignaturePath `
    -p $PublicKeyPath 2>&1 | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw "Release manifest Minisign verification failed."
}

Import-Module (Join-Path $PSScriptRoot "WokCore.ReleaseContract.psm1") -Force
$specifications = if ($fileSchemaVersion -eq 1) {
    @(
        Get-WokCoreTargetContracts -Version $ExpectedVersion |
            Where-Object LegacyV1
    )
} else {
    @(Get-WokCoreTargetContracts -Version $ExpectedVersion)
}
$artifacts = @($document.artifacts)
if ($artifacts.Count -ne $specifications.Count) {
    throw "Release manifest artifact count does not match its schema."
}

for ($index = 0; $index -lt $specifications.Count; $index++) {
    $specification = $specifications[$index]
    $artifact = $artifacts[$index]
    Assert-ExactProperties `
        -Value $artifact `
        -Expected @("target", "file", "executable", "size", "sha256", "url") `
        -Context "Release artifact"

    $expectedFile = if ($fileSchemaVersion -eq 1) {
        $specification.LegacyV1Name
    } else {
        $specification.FriendlyPortableName
    }
    $expectedUrl = "https://github.com/hongjiadev/wokcore/releases/download/v$ExpectedVersion/$expectedFile"
    if (
        $artifact.target -isnot [string] -or
        $artifact.target -cne $specification.Target -or
        $artifact.file -isnot [string] -or
        $artifact.file -cne $expectedFile -or
        $artifact.executable -isnot [string] -or
        $artifact.executable -cne $specification.Executable -or
        $artifact.url -isnot [string] -or
        $artifact.url -cne $expectedUrl -or
        $artifact.sha256 -isnot [string] -or
        $artifact.sha256 -cnotmatch "^[0-9a-f]{64}$" -or
        -not (Test-JsonInteger -Value $artifact.size)
    ) {
        throw "Release artifact contract contains an invalid JSON type."
    }
    [long] $size = $artifact.size
    if (
        $size -le 0 -or
        $size -gt $maximumArtifactBytes
    ) {
        throw "Release artifact contract does not match its target."
    }

    $artifactPath = Join-Path $ArtifactDirectory $expectedFile
    if (-not [IO.File]::Exists($artifactPath)) {
        throw "Release artifact is missing: $expectedFile"
    }
    $item = Get-Item -LiteralPath $artifactPath
    if (
        ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or
        $item.Length -ne $size
    ) {
        throw "Release artifact size or file type is invalid."
    }
    $actualHash = (
        Get-FileHash -LiteralPath $artifactPath -Algorithm SHA256
    ).Hash.ToLowerInvariant()
    if ($actualHash -cne $artifact.sha256) {
        throw "Release artifact SHA-256 does not match the manifest."
    }

    $expectedEntries = @(
        $specification.Executable,
        "LICENSE-APACHE",
        "LICENSE-MIT",
        "NOTICE.md",
        "README.md"
    )
    $entries = @(Get-ArchiveEntries -Path $artifactPath -FileName $expectedFile)
    if (
        [string]::Join("`n", $entries) -cne
        [string]::Join("`n", $expectedEntries)
    ) {
        throw "Release archive entries are not exact."
    }
    foreach ($entry in $entries) {
        if (
            [string]::IsNullOrWhiteSpace($entry) -or
            $entry.StartsWith("/", [StringComparison]::Ordinal) -or
            $entry.StartsWith("\", [StringComparison]::Ordinal) -or
            $entry -match "(^|[\\/])\.\.([\\/]|$)" -or
            $entry -match "(^|[\\/])wokcore-(provider-sim|loadgen)(\.exe)?$"
        ) {
            throw "Release archive contains an unsafe or forbidden entry."
        }
    }
}

$checksumNames = [string[]] @(
    @(
        Get-WokCorePayloadNames `
            -Version $ExpectedVersion `
            -IncludeLegacyV1
    ) + @(
        "wokcore-update-v1.json",
        "wokcore-update-v2.json"
    )
)
[Array]::Sort($checksumNames, [StringComparer]::Ordinal)
$checksumLines = [Collections.Generic.List[string]]::new()
foreach ($name in $checksumNames) {
    $path = Join-Path $ArtifactDirectory $name
    if (-not [IO.File]::Exists($path)) {
        throw "Release checksum input is missing: $name"
    }
    $item = Get-Item -LiteralPath $path
    if (
        ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or
        $item.Length -le 0 -or
        $item.Length -gt $maximumArtifactBytes
    ) {
        throw "Release checksum input is symbolic, empty, or oversized: $name"
    }
    $hash = (
        Get-FileHash -LiteralPath $path -Algorithm SHA256
    ).Hash.ToLowerInvariant()
    $checksumLines.Add("$hash  $name")
}
$expectedChecksums = [string]::Join("`n", $checksumLines) + "`n"
if ($checksumsText -cne $expectedChecksums) {
    throw "SHA256SUMS does not exactly match the release artifacts and manifest."
}

Write-Output "release manifest verification passed"
