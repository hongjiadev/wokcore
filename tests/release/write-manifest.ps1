[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string] $ArtifactDirectory,
    [Parameter(Mandatory)]
    [string] $Version,
    [Parameter(Mandatory)]
    [string] $SigningKeyId,
    [Parameter(Mandatory)]
    [string] $OutputPath,
    [Parameter(Mandatory)]
    [string] $ChecksumsPath
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
$ChecksumsPath = [IO.Path]::GetFullPath($ChecksumsPath)
if (-not [IO.Directory]::Exists($ArtifactDirectory)) {
    throw "Artifact directory does not exist."
}
if (
    [IO.Path]::GetDirectoryName($OutputPath) -cne $ArtifactDirectory -or
    [IO.Path]::GetFileName($OutputPath) -cne "wokcore-update-v1.json" -or
    [IO.Path]::GetDirectoryName($ChecksumsPath) -cne $ArtifactDirectory -or
    [IO.Path]::GetFileName($ChecksumsPath) -cne "SHA256SUMS"
) {
    throw "Manifest and checksums must use their fixed names in the artifact directory."
}

$specifications = @(
    [pscustomobject]@{
        Target = "x86_64-pc-windows-msvc"
        Extension = "zip"
        Executable = "wokcore.exe"
    },
    [pscustomobject]@{
        Target = "x86_64-apple-darwin"
        Extension = "tar.gz"
        Executable = "wokcore"
    },
    [pscustomobject]@{
        Target = "aarch64-apple-darwin"
        Extension = "tar.gz"
        Executable = "wokcore"
    },
    [pscustomobject]@{
        Target = "x86_64-unknown-linux-gnu"
        Extension = "tar.gz"
        Executable = "wokcore"
    },
    [pscustomobject]@{
        Target = "aarch64-unknown-linux-gnu"
        Extension = "tar.gz"
        Executable = "wokcore"
    }
)

$artifacts = foreach ($specification in $specifications) {
    $fileName = "wokcore-v$Version-$($specification.Target).$($specification.Extension)"
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
        target = $specification.Target
        file = $fileName
        executable = $specification.Executable
        size = [long] $item.Length
        sha256 = $sha256
        url = "https://github.com/hongjiadev/wokcore/releases/download/v$Version/$fileName"
    }
}

$document = [ordered]@{
    schema_version = 1
    product = "wokcore"
    api_major = 1
    version = $Version
    signing_key_id = $SigningKeyId
    artifacts = @($artifacts)
}
$json = ($document | ConvertTo-Json -Depth 6 -Compress) + "`n"
$utf8 = [Text.UTF8Encoding]::new($false)
$temporaryManifest = Join-Path $ArtifactDirectory (
    ".wokcore-update-v1." + [Guid]::NewGuid().ToString("N") + ".tmp"
)
$temporaryChecksums = Join-Path $ArtifactDirectory (
    ".SHA256SUMS." + [Guid]::NewGuid().ToString("N") + ".tmp"
)

try {
    [IO.File]::WriteAllText($temporaryManifest, $json, $utf8)
    if ([IO.File]::Exists($OutputPath)) {
        [IO.File]::Delete($OutputPath)
    }
    [IO.File]::Move($temporaryManifest, $OutputPath)

    $checksumLines = foreach ($artifact in $artifacts) {
        "$($artifact.sha256)  $($artifact.file)"
    }
    $manifestHash = (
        Get-FileHash -LiteralPath $OutputPath -Algorithm SHA256
    ).Hash.ToLowerInvariant()
    $checksumLines += "$manifestHash  wokcore-update-v1.json"
    [IO.File]::WriteAllText(
        $temporaryChecksums,
        ([string]::Join("`n", $checksumLines) + "`n"),
        $utf8
    )
    if ([IO.File]::Exists($ChecksumsPath)) {
        [IO.File]::Delete($ChecksumsPath)
    }
    [IO.File]::Move($temporaryChecksums, $ChecksumsPath)
} finally {
    foreach ($temporary in @($temporaryManifest, $temporaryChecksums)) {
        if ([IO.File]::Exists($temporary)) {
            [IO.File]::Delete($temporary)
        }
    }
}

Write-Output $OutputPath
