[CmdletBinding()]
param(
    [string] $MinisignPath = $env:WOKCORE_TEST_MINISIGN
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "../.."))
$buildPackage = Join-Path $PSScriptRoot "build-package.ps1"
$writeManifest = Join-Path $PSScriptRoot "write-manifest.ps1"
$writeChecksums = Join-Path $PSScriptRoot "write-checksums.ps1"
$verifyManifest = Join-Path $PSScriptRoot "verify-manifest.ps1"
$normalizePublicKey = Join-Path $PSScriptRoot "normalize-minisign-public-key.ps1"
$schemaV1 = Join-Path $repositoryRoot "release\manifest-v1.schema.json"
$schemaV2 = Join-Path $repositoryRoot "release\manifest-v2.schema.json"
$releaseContract = Join-Path $PSScriptRoot "WokCore.ReleaseContract.psm1"

foreach ($path in @(
    $buildPackage,
    $writeManifest,
    $writeChecksums,
    $verifyManifest,
    $normalizePublicKey,
    $schemaV1,
    $schemaV2,
    $releaseContract
)) {
    if (-not [IO.File]::Exists($path)) {
        throw "Missing release contract implementation: $path"
    }
}
Import-Module $releaseContract -Force
if ([string]::IsNullOrWhiteSpace($MinisignPath)) {
    $command = Get-Command minisign -ErrorAction SilentlyContinue
    if ($null -eq $command) {
        throw "Minisign is required for the release contract test."
    }
    $MinisignPath = $command.Source
}
$MinisignPath = [IO.Path]::GetFullPath($MinisignPath)
if (-not [IO.File]::Exists($MinisignPath)) {
    throw "Minisign executable does not exist: $MinisignPath"
}

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

function Write-Utf8NoBom {
    param(
        [Parameter(Mandatory)]
        [string] $Path,
        [Parameter(Mandatory)]
        [string] $Value
    )

    [IO.File]::WriteAllText(
        $Path,
        $Value,
        [Text.UTF8Encoding]::new($false)
    )
}

function New-TestTarPackage {
    param(
        [Parameter(Mandatory)]
        [string] $Path
    )

    $staging = Join-Path ([IO.Path]::GetDirectoryName($Path)) (
        "tar-" + [Guid]::NewGuid().ToString("N")
    )
    [IO.Directory]::CreateDirectory($staging) | Out-Null
    try {
        [IO.File]::WriteAllBytes(
            (Join-Path $staging "wokcore"),
            [byte[]] @(0x57, 0x4f, 0x4b, 0x43, 0x4f, 0x52, 0x45)
        )
        foreach ($name in @(
            "LICENSE-APACHE",
            "LICENSE-MIT",
            "NOTICE.md",
            "README.md"
        )) {
            [IO.File]::Copy(
                (Join-Path $repositoryRoot $name),
                (Join-Path $staging $name)
            )
        }
        $tar = Get-Command tar -ErrorAction Stop
        & $tar.Source -czf $Path -C $staging `
            "wokcore" `
            "LICENSE-APACHE" `
            "LICENSE-MIT" `
            "NOTICE.md" `
            "README.md"
        if ($LASTEXITCODE -ne 0) {
            throw "Creating a test tar package failed."
        }
    } finally {
        if ([IO.Directory]::Exists($staging)) {
            [IO.Directory]::Delete($staging, $true)
        }
    }
}

function Invoke-Minisign {
    param(
        [Parameter(Mandatory)]
        [string[]] $Arguments
    )

    $output = @(& $MinisignPath @Arguments 2>&1)
    $exitCode = $LASTEXITCODE
    if ($exitCode -ne 0) {
        throw "Minisign failed with exit code $exitCode."
    }
}

$testRoot = Join-Path ([IO.Path]::GetTempPath()) (
    "wokcore-release-contract-" + [Guid]::NewGuid().ToString("N")
)
[IO.Directory]::CreateDirectory($testRoot) | Out-Null

try {
    $legacyPublicKey = Join-Path $testRoot "legacy-leading-zero.pub"
    [byte[]] $legacyPayload = [byte[]]::new(42)
    $legacyPayload[0] = 0x45
    $legacyPayload[1] = 0x64
    [byte[]] $legacyKeyId = @(
        0xef,
        0xcd,
        0xab,
        0x89,
        0x67,
        0x45,
        0x23,
        0x01
    )
    [Array]::Copy($legacyKeyId, 0, $legacyPayload, 2, $legacyKeyId.Length)
    Write-Utf8NoBom -Path $legacyPublicKey -Value (
        "untrusted comment: minisign public key 123456789ABCDEF`n" +
        [Convert]::ToBase64String($legacyPayload) +
        "`n"
    )
    $normalizedLegacyKeyId = (
        & $normalizePublicKey -Path $legacyPublicKey
    ).Trim()
    $normalizedLegacyText = [IO.File]::ReadAllText(
        $legacyPublicKey,
        [Text.Encoding]::UTF8
    )
    if (
        $normalizedLegacyKeyId -cne "0123456789ABCDEF" -or
        -not $normalizedLegacyText.StartsWith(
            "untrusted comment: minisign public key 0123456789ABCDEF`n",
            [StringComparison]::Ordinal
        )
    ) {
        throw "A legacy leading-zero Minisign key id was not normalized."
    }
    Write-Utf8NoBom -Path $legacyPublicKey -Value (
        "untrusted comment: minisign public key 223456789ABCDEF`n" +
        [Convert]::ToBase64String($legacyPayload) +
        "`n"
    )
    Assert-Fails `
        -Message "A mismatched short Minisign key id was normalized." `
        -Operation {
            & $normalizePublicKey -Path $legacyPublicKey
        }

    $packages = Join-Path $testRoot "packages"
    $secondPackages = Join-Path $testRoot "packages-second"
    [IO.Directory]::CreateDirectory($packages) | Out-Null
    [IO.Directory]::CreateDirectory($secondPackages) | Out-Null
    $executable = Join-Path $testRoot "wokcore.exe"
    [IO.File]::WriteAllBytes(
        $executable,
        [byte[]] @(0x4d, 0x5a, 0x57, 0x4f, 0x4b, 0x43, 0x4f, 0x52, 0x45)
    )
    [IO.File]::SetLastWriteTimeUtc(
        $executable,
        [DateTime]::new(2020, 1, 2, 3, 4, 5, [DateTimeKind]::Utc)
    )

    foreach ($windowsTarget in @(
        "x86_64-pc-windows-msvc",
        "aarch64-pc-windows-msvc"
    )) {
        & $buildPackage `
            -ExecutablePath $executable `
            -RepositoryRoot $repositoryRoot `
            -OutputDirectory $packages `
            -Version "1.2.3" `
            -Target $windowsTarget
        [IO.File]::SetLastWriteTimeUtc(
            $executable,
            [DateTime]::new(2030, 1, 2, 3, 4, 5, [DateTimeKind]::Utc)
        )
        & $buildPackage `
            -ExecutablePath $executable `
            -RepositoryRoot $repositoryRoot `
            -OutputDirectory $secondPackages `
            -Version "1.2.3" `
            -Target $windowsTarget
        $windowsName = "wokcore-v1.2.3-$windowsTarget.zip"
        $firstHash = (
            Get-FileHash `
                -LiteralPath (Join-Path $packages $windowsName) `
                -Algorithm SHA256
        ).Hash
        $secondHash = (
            Get-FileHash `
                -LiteralPath (Join-Path $secondPackages $windowsName) `
                -Algorithm SHA256
        ).Hash
        if ($firstHash -cne $secondHash) {
            throw "Windows release package is not deterministic: $windowsTarget"
        }
    }

    $windowsName = "wokcore-v1.2.3-x86_64-pc-windows-msvc.zip"
    $firstWindows = Join-Path $packages $windowsName

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($firstWindows)
    try {
        $entries = @($zip.Entries | ForEach-Object FullName)
        $timestamps = @($zip.Entries | ForEach-Object LastWriteTime)
    } finally {
        $zip.Dispose()
    }
    $expectedEntries = @(
        "wokcore.exe",
        "LICENSE-APACHE",
        "LICENSE-MIT",
        "NOTICE.md",
        "README.md"
    )
    if (
        [string]::Join("`n", $entries) -cne
        [string]::Join("`n", $expectedEntries) -or
        @(
            $timestamps | Where-Object {
                $_.DateTime -ne [DateTime]::new(
                    1980,
                    1,
                    1,
                    0,
                    0,
                    0,
                    [DateTimeKind]::Unspecified
                )
            }
        ).Count -ne 0
    ) {
        throw "Windows release archive entries or timestamps are not exact."
    }

    foreach ($target in @(
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu"
    )) {
        New-TestTarPackage -Path (
            Join-Path $packages "wokcore-v1.2.3-$target.tar.gz"
        )
    }
    foreach ($contract in Get-WokCoreTargetContracts -Version "1.2.3") {
        $friendlyPath = Join-Path $packages $contract.FriendlyPortableName
        $sourceName = if ($contract.LegacyV1) {
            $contract.LegacyV1Name
        } else {
            "wokcore-v1.2.3-aarch64-pc-windows-msvc.zip"
        }
        [IO.File]::Copy(
            (Join-Path $packages $sourceName),
            $friendlyPath
        )
    }
    $expectedPayloadNames = @(
        Get-WokCorePayloadNames -Version "1.2.3" -IncludeLegacyV1
    )
    if ($expectedPayloadNames.Count -ne 19) {
        throw "Dual-manifest fixture must contain 19 payloads."
    }
    foreach ($name in $expectedPayloadNames) {
        $path = Join-Path $packages $name
        if (-not [IO.File]::Exists($path)) {
            [IO.File]::WriteAllBytes(
                $path,
                [Text.Encoding]::UTF8.GetBytes($name)
            )
        }
    }

    $publicKey = Join-Path $testRoot "fixture.pub"
    $secretKey = Join-Path $testRoot "fixture.key"
    Invoke-Minisign -Arguments @(
        "-G", "-W", "-f",
        "-p", $publicKey,
        "-s", $secretKey
    )
    $keyId = (& $normalizePublicKey -Path $publicKey).Trim()
    $publicText = [IO.File]::ReadAllText($publicKey, [Text.Encoding]::UTF8)
    if (
        $keyId -cnotmatch "^[0-9A-F]{16}$" -or
        $publicText -notmatch "minisign public key ([0-9A-F]{16})" -or
        $Matches[1] -cne $keyId
    ) {
        throw "Fixture public key does not expose a stable key id."
    }

    $manifest = Join-Path $packages "wokcore-update-v1.json"
    $manifestV2 = Join-Path $packages "wokcore-update-v2.json"
    $checksums = Join-Path $packages "SHA256SUMS"
    & $writeManifest `
        -ArtifactDirectory $packages `
        -Version "1.2.3" `
        -SigningKeyId $keyId `
        -SchemaVersion 1 `
        -OutputPath $manifest
    & $writeManifest `
        -ArtifactDirectory $packages `
        -Version "1.2.3" `
        -SigningKeyId $keyId `
        -SchemaVersion 2 `
        -OutputPath $manifestV2

    $v1 = Get-Content -Raw -LiteralPath $manifest | ConvertFrom-Json
    $v2 = Get-Content -Raw -LiteralPath $manifestV2 | ConvertFrom-Json
    if ($v1.artifacts.Count -ne 5) { throw "v1 must remain five-target." }
    if ($v2.artifacts.Count -ne 6) { throw "v2 must be six-target." }
    if ($v2.artifacts.file -match "unknown") {
        throw "v2 files must be friendly."
    }
    if ($v2.artifacts.target -notcontains "aarch64-pc-windows-msvc") {
        throw "v2 must contain Windows ARM64."
    }
    & $writeChecksums `
        -ArtifactDirectory $packages `
        -ExpectedNames @(
            $expectedPayloadNames +
            @("wokcore-update-v1.json", "wokcore-update-v2.json")
        ) `
        -OutputPath $checksums

    $signature = "$manifest.minisig"
    $signatureV2 = "$manifestV2.minisig"
    Invoke-Minisign -Arguments @(
        "-S", "-W",
        "-s", $secretKey,
        "-m", $manifest,
        "-x", $signature,
        "-c", "WokCore release contract test",
        "-t", "wokcore test fixture"
    )
    Invoke-Minisign -Arguments @(
        "-S", "-W",
        "-s", $secretKey,
        "-m", $manifestV2,
        "-x", $signatureV2,
        "-c", "WokCore release contract test",
        "-t", "wokcore v2 test fixture"
    )

    & $verifyManifest `
        -ManifestPath $manifest `
        -SignaturePath $signature `
        -PublicKeyPath $publicKey `
        -ArtifactDirectory $packages `
        -ChecksumsPath $checksums `
        -ExpectedVersion "1.2.3" `
        -ExpectedSigningKeyId $keyId `
        -MinisignPath $MinisignPath
    & $verifyManifest `
        -ManifestPath $manifestV2 `
        -SignaturePath $signatureV2 `
        -PublicKeyPath $publicKey `
        -ArtifactDirectory $packages `
        -ChecksumsPath $checksums `
        -ExpectedVersion "1.2.3" `
        -ExpectedSigningKeyId $keyId `
        -MinisignPath $MinisignPath

    $manifestText = [IO.File]::ReadAllText($manifest, [Text.Encoding]::UTF8)
    if ($manifestText.Contains($repositoryRoot)) {
        throw "Release manifest retained a local absolute path."
    }

    $savedSignature = [IO.File]::ReadAllBytes($signature)
    $savedManifest = [IO.File]::ReadAllBytes($manifest)
    $savedPublicKey = [IO.File]::ReadAllBytes($publicKey)

    $duplicateManifest = [IO.File]::ReadAllText(
        $manifest,
        [Text.Encoding]::UTF8
    ).Replace(
        '"schema_version":1,',
        '"schema_version":1,"schema_version":1,'
    )
    Write-Utf8NoBom -Path $manifest -Value $duplicateManifest
    Assert-Fails -Message "A duplicate JSON property was accepted." -Operation {
        & $verifyManifest `
            -ManifestPath $manifest `
            -SignaturePath $signature `
            -PublicKeyPath $publicKey `
            -ArtifactDirectory $packages `
            -ChecksumsPath $checksums `
            -ExpectedVersion "1.2.3" `
            -ExpectedSigningKeyId $keyId `
            -MinisignPath $MinisignPath
    }
    [IO.File]::WriteAllBytes($manifest, $savedManifest)

    $typedDocument = Get-Content -Raw -LiteralPath $manifest | ConvertFrom-Json
    $typedDocument.schema_version = "1"
    Write-Utf8NoBom -Path $manifest -Value (
        ($typedDocument | ConvertTo-Json -Depth 6 -Compress) + "`n"
    )
    Invoke-Minisign -Arguments @(
        "-S", "-W",
        "-s", $secretKey,
        "-m", $manifest,
        "-x", $signature,
        "-c", "WokCore release contract test",
        "-t", "wokcore string schema fixture"
    )
    Assert-Fails -Message "A string schema version was accepted." -Operation {
        & $verifyManifest `
            -ManifestPath $manifest `
            -SignaturePath $signature `
            -PublicKeyPath $publicKey `
            -ArtifactDirectory $packages `
            -ChecksumsPath $checksums `
            -ExpectedVersion "1.2.3" `
            -ExpectedSigningKeyId $keyId `
            -MinisignPath $MinisignPath
    }
    [IO.File]::WriteAllBytes($manifest, $savedManifest)
    [IO.File]::WriteAllBytes($signature, $savedSignature)

    $typedDocument = Get-Content -Raw -LiteralPath $manifest | ConvertFrom-Json
    $typedDocument.artifacts[0].size = [string] $typedDocument.artifacts[0].size
    Write-Utf8NoBom -Path $manifest -Value (
        ($typedDocument | ConvertTo-Json -Depth 6 -Compress) + "`n"
    )
    Invoke-Minisign -Arguments @(
        "-S", "-W",
        "-s", $secretKey,
        "-m", $manifest,
        "-x", $signature,
        "-c", "WokCore release contract test",
        "-t", "wokcore string size fixture"
    )
    Assert-Fails -Message "A string artifact size was accepted." -Operation {
        & $verifyManifest `
            -ManifestPath $manifest `
            -SignaturePath $signature `
            -PublicKeyPath $publicKey `
            -ArtifactDirectory $packages `
            -ChecksumsPath $checksums `
            -ExpectedVersion "1.2.3" `
            -ExpectedSigningKeyId $keyId `
            -MinisignPath $MinisignPath
    }
    [IO.File]::WriteAllBytes($manifest, $savedManifest)
    [IO.File]::WriteAllBytes($signature, $savedSignature)

    $publicText = [IO.File]::ReadAllText($publicKey, [Text.Encoding]::UTF8)
    $differentKeyId = if ($keyId[0] -eq "0") {
        "1" + $keyId.Substring(1)
    } else {
        "0" + $keyId.Substring(1)
    }
    Write-Utf8NoBom -Path $publicKey -Value (
        $publicText.Replace($keyId, $differentKeyId)
    )
    Assert-Fails -Message "A public-key comment detached from its payload was accepted." -Operation {
        & $verifyManifest `
            -ManifestPath $manifest `
            -SignaturePath $signature `
            -PublicKeyPath $publicKey `
            -ArtifactDirectory $packages `
            -ChecksumsPath $checksums `
            -ExpectedVersion "1.2.3" `
            -ExpectedSigningKeyId $keyId `
            -MinisignPath $MinisignPath
    }
    [IO.File]::WriteAllBytes($publicKey, $savedPublicKey)

    [IO.File]::Delete($signature)
    Assert-Fails -Message "Missing manifest signature was accepted." -Operation {
        & $verifyManifest `
            -ManifestPath $manifest `
            -SignaturePath $signature `
            -PublicKeyPath $publicKey `
            -ArtifactDirectory $packages `
            -ChecksumsPath $checksums `
            -ExpectedVersion "1.2.3" `
            -ExpectedSigningKeyId $keyId `
            -MinisignPath $MinisignPath
    }
    [IO.File]::WriteAllBytes($signature, $savedSignature)

    $document = Get-Content -Raw -LiteralPath $manifest | ConvertFrom-Json
    $document.artifacts[0].target = "aarch64-apple-darwin"
    Write-Utf8NoBom -Path $manifest -Value (
        ($document | ConvertTo-Json -Depth 6 -Compress) + "`n"
    )
    Invoke-Minisign -Arguments @(
        "-S", "-W",
        "-s", $secretKey,
        "-m", $manifest,
        "-x", $signature,
        "-c", "WokCore release contract test",
        "-t", "wokcore target mismatch fixture"
    )
    Assert-Fails -Message "A target-mismatched manifest was accepted." -Operation {
        & $verifyManifest `
            -ManifestPath $manifest `
            -SignaturePath $signature `
            -PublicKeyPath $publicKey `
            -ArtifactDirectory $packages `
            -ChecksumsPath $checksums `
            -ExpectedVersion "1.2.3" `
            -ExpectedSigningKeyId $keyId `
            -MinisignPath $MinisignPath
    }
    [IO.File]::WriteAllBytes($manifest, $savedManifest)
    [IO.File]::WriteAllBytes($signature, $savedSignature)

    $savedArchive = [IO.File]::ReadAllBytes($firstWindows)
    [IO.File]::AppendAllText($firstWindows, "tampered")
    Assert-Fails -Message "An artifact with a mismatched hash was accepted." -Operation {
        & $verifyManifest `
            -ManifestPath $manifest `
            -SignaturePath $signature `
            -PublicKeyPath $publicKey `
            -ArtifactDirectory $packages `
            -ChecksumsPath $checksums `
            -ExpectedVersion "1.2.3" `
            -ExpectedSigningKeyId $keyId `
            -MinisignPath $MinisignPath
    }
    [IO.File]::WriteAllBytes($firstWindows, $savedArchive)

    Write-Output "release manifest contract tests passed"
} finally {
    if ([IO.Directory]::Exists($testRoot)) {
        [IO.Directory]::Delete($testRoot, $true)
    }
}
