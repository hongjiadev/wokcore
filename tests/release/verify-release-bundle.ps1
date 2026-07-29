[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string] $ArtifactDirectory,
    [Parameter(Mandatory)]
    [string] $Version,
    [Parameter(Mandatory)]
    [string] $PublicKeyPath,
    [Parameter(Mandatory)]
    [string] $MinisignPath
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$maximumArtifactBytes = 536870912
$maximumManifestBytes = 131072
$maximumSignatureBytes = 4096
$maximumPublicKeyBytes = 1024
$maximumChecksumsBytes = 8192
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
                throw "Release verification paths must not contain reparse points."
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
    if ($Path.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "$Description must remain outside the release bundle."
    }
}

function Read-BoundedUtf8 {
    param(
        [Parameter(Mandatory)][string] $Path,
        [Parameter(Mandatory)][long] $MaximumBytes,
        [Parameter(Mandatory)][string] $Description
    )

    Assert-BoundedFile `
        -Path $Path `
        -MaximumBytes $MaximumBytes `
        -Description $Description
    [byte[]] $bytes = [IO.File]::ReadAllBytes($Path)
    if (
        $bytes.Length -ge 3 -and
        $bytes[0] -eq 0xef -and
        $bytes[1] -eq 0xbb -and
        $bytes[2] -eq 0xbf
    ) {
        throw "$Description must be UTF-8 without a byte-order mark."
    }
    return [Text.UTF8Encoding]::new($false, $true).GetString($bytes)
}

function Get-MinisignPublicKeyId {
    param([Parameter(Mandatory)][string] $PublicKeyText)

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

function Assert-MinisignSignatureText {
    param(
        [Parameter(Mandatory)][string] $Path,
        [Parameter(Mandatory)][string] $UntrustedComment,
        [Parameter(Mandatory)][string] $TrustedComment
    )

    $text = Read-BoundedUtf8 `
        -Path $Path `
        -MaximumBytes $maximumSignatureBytes `
        -Description "Release signature $Path"
    $normalized = $text.Replace("`r`n", "`n")
    $lines = @($normalized.Split("`n"))
    if (
        $lines.Count -ne 5 -or
        $lines[0] -cne "untrusted comment: $UntrustedComment" -or
        $lines[2] -cne "trusted comment: $TrustedComment" -or
        $lines[4] -cne ""
    ) {
        throw "Release signature text has an invalid shape or comment."
    }
    try {
        [byte[]] $messageSignature = [Convert]::FromBase64String($lines[1])
        [byte[]] $trustedCommentSignature =
            [Convert]::FromBase64String($lines[3])
    } catch {
        throw "Release signature text contains invalid base64."
    }
    if (
        $messageSignature.Length -ne 74 -or
        $messageSignature[0] -ne 0x45 -or
        $messageSignature[1] -ne 0x44 -or
        $trustedCommentSignature.Length -ne 64
    ) {
        throw "Release signature payload has an invalid shape."
    }
}

if ($Version.Length -gt 128 -or $Version -cnotmatch $semverPattern) {
    throw "Release version is not canonical SemVer."
}

$ArtifactDirectory = [IO.Path]::GetFullPath($ArtifactDirectory)
$PublicKeyPath = [IO.Path]::GetFullPath($PublicKeyPath)
$MinisignPath = [IO.Path]::GetFullPath($MinisignPath)
foreach ($path in @($ArtifactDirectory, $PublicKeyPath, $MinisignPath)) {
    Assert-NoReparsePath -Path $path
}
if (-not [IO.Directory]::Exists($ArtifactDirectory)) {
    throw "Artifact directory does not exist."
}
Assert-BoundedFile `
    -Path $PublicKeyPath `
    -MaximumBytes $maximumPublicKeyBytes `
    -Description "Minisign public key"
Assert-OutsideArtifactDirectory `
    -Path $PublicKeyPath `
    -Description "Trusted Minisign public key"
if (-not [IO.File]::Exists($MinisignPath)) {
    throw "Minisign executable is missing."
}

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
$signedContent = [string[]] @($checksumNames + @("SHA256SUMS"))
[Array]::Sort($signedContent, [StringComparer]::Ordinal)

$expectedNames = [Collections.Generic.List[string]]::new()
foreach ($name in $signedContent) {
    $expectedNames.Add($name)
    $expectedNames.Add("$name.minisig")
}
$expectedNames.Add("WokCore-Minisign.pub")
$expectedNameArray = [string[]] $expectedNames.ToArray()
[Array]::Sort($expectedNameArray, [StringComparer]::Ordinal)

$actualItems = @(Get-ChildItem -LiteralPath $ArtifactDirectory -Force)
$seen = [Collections.Generic.HashSet[string]]::new(
    [StringComparer]::OrdinalIgnoreCase
)
$actualNames = [string[]] @(
    $actualItems | ForEach-Object {
        if (
            $_.PSIsContainer -or
            ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or
            -not $seen.Add($_.Name)
        ) {
            throw "Release bundle contains a directory, reparse point, or duplicate name."
        }
        $_.Name
    }
)
[Array]::Sort($actualNames, [StringComparer]::Ordinal)
if (
    $actualNames.Count -ne 45 -or
    [string]::Join("`n", $actualNames) -cne
    [string]::Join("`n", $expectedNameArray)
) {
    throw "WokCore release inventory differs from the exact 45-file contract."
}

foreach ($name in $payloads) {
    Assert-BoundedFile `
        -Path (Join-Path $ArtifactDirectory $name) `
        -MaximumBytes $maximumArtifactBytes `
        -Description "Release payload $name"
}
foreach ($name in @("wokcore-update-v1.json", "wokcore-update-v2.json")) {
    Assert-BoundedFile `
        -Path (Join-Path $ArtifactDirectory $name) `
        -MaximumBytes $maximumManifestBytes `
        -Description "Release manifest $name"
}
$checksumsPath = Join-Path $ArtifactDirectory "SHA256SUMS"
$checksumsText = Read-BoundedUtf8 `
    -Path $checksumsPath `
    -MaximumBytes $maximumChecksumsBytes `
    -Description "Release checksums"
foreach ($name in $signedContent) {
    $comment = if ($name -ceq "SHA256SUMS") {
        "WokCore checksums"
    } else {
        "WokCore release asset"
    }
    Assert-MinisignSignatureText `
        -Path (Join-Path $ArtifactDirectory "$name.minisig") `
        -UntrustedComment $comment `
        -TrustedComment "WokCore v$Version"
}
$bundledPublicKeyPath = Join-Path $ArtifactDirectory "WokCore-Minisign.pub"
$bundledPublicKeyText = Read-BoundedUtf8 `
    -Path $bundledPublicKeyPath `
    -MaximumBytes $maximumPublicKeyBytes `
    -Description "Bundled Minisign public key"

[byte[]] $externalPublicKeyBytes = [IO.File]::ReadAllBytes($PublicKeyPath)
[byte[]] $bundledPublicKeyBytes = [IO.File]::ReadAllBytes($bundledPublicKeyPath)
if (
    $externalPublicKeyBytes.Length -ne $bundledPublicKeyBytes.Length -or
    [Convert]::ToBase64String($externalPublicKeyBytes) -cne
    [Convert]::ToBase64String($bundledPublicKeyBytes)
) {
    throw "Bundled Minisign public key does not match the trusted public key."
}
$keyId = Get-MinisignPublicKeyId -PublicKeyText $bundledPublicKeyText

$checksumLines = [Collections.Generic.List[string]]::new()
foreach ($name in $checksumNames) {
    $hash = (
        Get-FileHash `
            -LiteralPath (Join-Path $ArtifactDirectory $name) `
            -Algorithm SHA256
    ).Hash.ToLowerInvariant()
    $checksumLines.Add("$hash  $name")
}
$expectedChecksums = [string]::Join("`n", $checksumLines) + "`n"
if ($checksumsText -cne $expectedChecksums) {
    throw "SHA256SUMS is not the exact ordinal 21-file checksum inventory."
}

foreach ($name in $signedContent) {
    & $MinisignPath `
        -V `
        -q `
        -m (Join-Path $ArtifactDirectory $name) `
        -x (Join-Path $ArtifactDirectory "$name.minisig") `
        -p $PublicKeyPath 2>&1 |
        Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "Invalid Minisign signature for $name."
    }
}

$verifyManifest = Join-Path $PSScriptRoot "verify-manifest.ps1"
foreach ($schemaVersion in @(1, 2)) {
    $manifestName = "wokcore-update-v$schemaVersion.json"
    & $verifyManifest `
        -ManifestPath (Join-Path $ArtifactDirectory $manifestName) `
        -SignaturePath (Join-Path $ArtifactDirectory "$manifestName.minisig") `
        -PublicKeyPath $PublicKeyPath `
        -ArtifactDirectory $ArtifactDirectory `
        -ChecksumsPath $checksumsPath `
        -ExpectedVersion $Version `
        -ExpectedSigningKeyId $keyId `
        -MinisignPath $MinisignPath |
        Out-Null
}

Write-Output "WokCore release bundle verification passed"
