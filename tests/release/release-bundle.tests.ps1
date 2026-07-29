[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string] $MinisignPath
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$signer = Join-Path $PSScriptRoot "sign-release-bundle.ps1"
$verifier = Join-Path $PSScriptRoot "verify-release-bundle.ps1"
$writeManifest = Join-Path $PSScriptRoot "write-manifest.ps1"
$normalizePublicKey = Join-Path $PSScriptRoot "normalize-minisign-public-key.ps1"
$releaseContract = Join-Path $PSScriptRoot "WokCore.ReleaseContract.psm1"
$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "../.."))

$MinisignPath = [IO.Path]::GetFullPath($MinisignPath)
if (-not [IO.File]::Exists($MinisignPath)) {
    throw "Minisign executable is missing."
}
Import-Module $releaseContract -Force

function Clear-TestSecretKey {
    param([Parameter(Mandatory)][string] $Path)

    if (-not [IO.File]::Exists($Path)) {
        return
    }
    $length = (Get-Item -LiteralPath $Path).Length
    $stream = [IO.File]::Open(
        $Path,
        [IO.FileMode]::Open,
        [IO.FileAccess]::Write,
        [IO.FileShare]::None
    )
    try {
        $zeros = [byte[]]::new(4096)
        $remaining = $length
        while ($remaining -gt 0) {
            $count = [Math]::Min([long] $zeros.Length, $remaining)
            $stream.Write($zeros, 0, [int] $count)
            $remaining -= $count
        }
        $stream.Flush($true)
    } finally {
        $stream.Dispose()
    }
    [IO.File]::Delete($Path)
}

function New-TestZipPackage {
    param(
        [Parameter(Mandatory)][string] $Path,
        [Parameter(Mandatory)][string] $Executable
    )

    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $stream = [IO.File]::Open(
        $Path,
        [IO.FileMode]::CreateNew,
        [IO.FileAccess]::ReadWrite,
        [IO.FileShare]::None
    )
    try {
        $archive = [IO.Compression.ZipArchive]::new(
            $stream,
            [IO.Compression.ZipArchiveMode]::Create,
            $true
        )
        try {
            foreach ($name in @(
                $Executable,
                "LICENSE-APACHE",
                "LICENSE-MIT",
                "NOTICE.md",
                "README.md"
            )) {
                $entry = $archive.CreateEntry(
                    $name,
                    [IO.Compression.CompressionLevel]::Optimal
                )
                $entryStream = $entry.Open()
                try {
                    $bytes = if ($name -ceq $Executable) {
                        [Text.Encoding]::UTF8.GetBytes(
                            "bounded executable fixture: $Path"
                        )
                    } else {
                        [IO.File]::ReadAllBytes(
                            (Join-Path $repositoryRoot $name)
                        )
                    }
                    $entryStream.Write($bytes, 0, $bytes.Length)
                } finally {
                    $entryStream.Dispose()
                }
            }
        } finally {
            $archive.Dispose()
        }
    } finally {
        $stream.Dispose()
    }
}

function New-TestTarPackage {
    param(
        [Parameter(Mandatory)][string] $Path,
        [Parameter(Mandatory)][string] $Executable
    )

    $staging = Join-Path ([IO.Path]::GetDirectoryName($Path)) (
        "tar-" + [Guid]::NewGuid().ToString("N")
    )
    [IO.Directory]::CreateDirectory($staging) | Out-Null
    try {
        [IO.File]::WriteAllBytes(
            (Join-Path $staging $Executable),
            [Text.Encoding]::UTF8.GetBytes(
                "bounded executable fixture: $Path"
            )
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
            $Executable `
            "LICENSE-APACHE" `
            "LICENSE-MIT" `
            "NOTICE.md" `
            "README.md"
        if ($LASTEXITCODE -ne 0) {
            throw "Creating a bounded tar fixture failed."
        }
    } finally {
        if ([IO.Directory]::Exists($staging)) {
            [IO.Directory]::Delete($staging, $true)
        }
    }
}

function Copy-TestBundle {
    param(
        [Parameter(Mandatory)][string] $Source,
        [Parameter(Mandatory)][string] $Destination
    )

    [IO.Directory]::CreateDirectory($Destination) | Out-Null
    foreach ($file in [IO.Directory]::EnumerateFiles($Source)) {
        [IO.File]::Copy(
            $file,
            (Join-Path $Destination ([IO.Path]::GetFileName($file)))
        )
    }
}

function Assert-VerifierFails {
    param(
        [Parameter(Mandatory)][string] $ArtifactDirectory,
        [Parameter(Mandatory)][string] $FailureMessage,
        [string] $VerificationPublicKey = $public
    )

    $failed = $false
    try {
        & $verifier `
            -ArtifactDirectory $ArtifactDirectory `
            -Version "1.2.3" `
            -PublicKeyPath $VerificationPublicKey `
            -MinisignPath $MinisignPath
    } catch {
        $failed = $true
    }
    if (-not $failed) {
        throw $FailureMessage
    }
}

$testRoot = Join-Path ([IO.Path]::GetTempPath()) (
    "wokcore release bundle " + [Guid]::NewGuid().ToString("N")
)
$bundle = Join-Path $testRoot "bundle with spaces"
$keys = Join-Path $testRoot "ephemeral keys"
$public = Join-Path $keys "fixture.pub"
$secret = Join-Path $keys "fixture.key"
$secondPublic = Join-Path $keys "second.pub"
$secondSecret = Join-Path $keys "second.key"
$junction = Join-Path $testRoot "bundle junction"
$maximumVersionBundle = Join-Path ([IO.Path]::GetTempPath()) (
    "wb-" + [Guid]::NewGuid().ToString("N")
)
[IO.Directory]::CreateDirectory($bundle) | Out-Null
[IO.Directory]::CreateDirectory($keys) | Out-Null

try {
    $payloads = @(
        Get-WokCorePayloadNames -Version "1.2.3" -IncludeLegacyV1
    )
    if ($payloads.Count -ne 19) {
        throw "Expected exactly 19 WokCore payload fixtures."
    }
    foreach ($name in $payloads) {
        $path = Join-Path $bundle $name
        if (
            $name.EndsWith(".zip", [StringComparison]::Ordinal) -and
            (
                $name.Contains("-Portable.") -or
                $name.StartsWith("wokcore-", [StringComparison]::Ordinal)
            )
        ) {
            New-TestZipPackage -Path $path -Executable "wokcore.exe"
        } elseif (
            $name.EndsWith(".tar.gz", [StringComparison]::Ordinal)
        ) {
            New-TestTarPackage -Path $path -Executable "wokcore"
        } else {
            [IO.File]::WriteAllBytes(
                $path,
                [Text.Encoding]::UTF8.GetBytes("bounded fixture: $name")
            )
        }
    }

    & $MinisignPath -G -W -f -p $public -s $secret 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "Minisign fixture-key generation failed."
    }
    $keyId = (& $normalizePublicKey -Path $public).Trim()
    & $writeManifest `
        -ArtifactDirectory $bundle `
        -Version "1.2.3" `
        -SigningKeyId $keyId `
        -SchemaVersion 1 `
        -OutputPath (Join-Path $bundle "wokcore-update-v1.json") |
        Out-Null
    & $writeManifest `
        -ArtifactDirectory $bundle `
        -Version "1.2.3" `
        -SigningKeyId $keyId `
        -SchemaVersion 2 `
        -OutputPath (Join-Path $bundle "wokcore-update-v2.json") |
        Out-Null

    New-Item -ItemType Junction -Path $junction -Target $bundle |
        Out-Null
    $signerRejectedJunction = $false
    try {
        & $signer `
            -ArtifactDirectory $junction `
            -Version "1.2.3" `
            -SecretKeyPath $secret `
            -PublicKeyPath $public `
            -MinisignPath $MinisignPath
    } catch {
        $signerRejectedJunction = $true
    }
    if (-not $signerRejectedJunction) {
        throw "The signer accepted a reparse-point bundle ancestor."
    }
    [IO.Directory]::Delete($junction)

    & $signer `
        -ArtifactDirectory $bundle `
        -Version "1.2.3" `
        -SecretKeyPath $secret `
        -PublicKeyPath $public `
        -MinisignPath $MinisignPath

    $actualNames = [string[]] @(
        Get-ChildItem -LiteralPath $bundle -Force |
            ForEach-Object Name
    )
    if ($actualNames.Count -ne 45) {
        throw "Expected exactly 45 WokCore release assets."
    }
    [Array]::Sort($actualNames, [StringComparer]::Ordinal)
    $signedContent = [string[]] @(
        $payloads +
        @(
            "wokcore-update-v1.json",
            "wokcore-update-v2.json",
            "SHA256SUMS"
        )
    )
    [Array]::Sort($signedContent, [StringComparer]::Ordinal)
    $expectedNames = [Collections.Generic.List[string]]::new()
    foreach ($name in $signedContent) {
        $expectedNames.Add($name)
        $expectedNames.Add("$name.minisig")
    }
    $expectedNames.Add("WokCore-Minisign.pub")
    $expectedNameArray = [string[]] $expectedNames.ToArray()
    [Array]::Sort($expectedNameArray, [StringComparer]::Ordinal)
    if (
        [string]::Join("`n", $actualNames) -cne
        [string]::Join("`n", $expectedNameArray)
    ) {
        throw "The signed bundle inventory is not the exact 45-file contract."
    }

    $checksumNames = [string[]] @(
        $payloads +
        @("wokcore-update-v1.json", "wokcore-update-v2.json")
    )
    [Array]::Sort($checksumNames, [StringComparer]::Ordinal)
    $checksumLines = @(
        [IO.File]::ReadAllText(
            (Join-Path $bundle "SHA256SUMS"),
            [Text.UTF8Encoding]::new($false, $true)
        ).TrimEnd("`n").Split("`n")
    )
    $actualChecksumNames = [string[]] @(
        $checksumLines | ForEach-Object {
            if ($_ -cnotmatch "^[0-9a-f]{64}  (?<name>[^\\/]+)$") {
                throw "SHA256SUMS contains a malformed line."
            }
            $Matches.name
        }
    )
    if (
        $checksumLines.Count -ne 21 -or
        [string]::Join("`n", $actualChecksumNames) -cne
        [string]::Join("`n", $checksumNames)
    ) {
        throw "SHA256SUMS is not the exact ordinal 21-file inventory."
    }

    foreach ($name in $signedContent) {
        $signatureText = [IO.File]::ReadAllText(
            (Join-Path $bundle "$name.minisig"),
            [Text.UTF8Encoding]::new($false, $true)
        ).Replace("`r`n", "`n")
        $comment = if ($name -ceq "SHA256SUMS") {
            "WokCore checksums"
        } else {
            "WokCore release asset"
        }
        if (
            -not $signatureText.StartsWith(
                "untrusted comment: $comment`n",
                [StringComparison]::Ordinal
            ) -or
            -not $signatureText.Contains(
                "`ntrusted comment: WokCore v1.2.3`n"
            )
        ) {
            throw "A release signature contains unexpected comments: $name"
        }
    }

    & $verifier `
        -ArtifactDirectory $bundle `
        -Version "1.2.3" `
        -PublicKeyPath $public `
        -MinisignPath $MinisignPath

    Assert-VerifierFails `
        -ArtifactDirectory $bundle `
        -VerificationPublicKey (
            Join-Path $bundle "WokCore-Minisign.pub"
        ) `
        -FailureMessage (
            "The verifier trusted the public key supplied by its own bundle."
        )

    $tampered = Join-Path $testRoot "tampered payload"
    Copy-TestBundle -Source $bundle -Destination $tampered
    [IO.File]::AppendAllText((Join-Path $tampered $payloads[0]), "tampered")
    Assert-VerifierFails `
        -ArtifactDirectory $tampered `
        -FailureMessage "A tampered payload was accepted."

    $extra = Join-Path $testRoot "extra file"
    Copy-TestBundle -Source $bundle -Destination $extra
    [IO.File]::WriteAllBytes(
        (Join-Path $extra "unexpected.bin"),
        [byte[]] @(1)
    )
    Assert-VerifierFails `
        -ArtifactDirectory $extra `
        -FailureMessage "An unexpected release asset was accepted."

    $missing = Join-Path $testRoot "missing signature"
    Copy-TestBundle -Source $bundle -Destination $missing
    [IO.File]::Delete((Join-Path $missing "$($payloads[0]).minisig"))
    Assert-VerifierFails `
        -ArtifactDirectory $missing `
        -FailureMessage "A missing release signature was accepted."

    $wrongCase = Join-Path $testRoot "wrong case"
    Copy-TestBundle -Source $bundle -Destination $wrongCase
    $fixedPublicName = Join-Path $wrongCase "WokCore-Minisign.pub"
    $temporaryPublicName = Join-Path $wrongCase "renaming.pub"
    [IO.File]::Move($fixedPublicName, $temporaryPublicName)
    [IO.File]::Move(
        $temporaryPublicName,
        (Join-Path $wrongCase "wokcore-minisign.pub")
    )
    Assert-VerifierFails `
        -ArtifactDirectory $wrongCase `
        -FailureMessage "A case-mismatched release name was accepted."

    $directoryCase = Join-Path $testRoot "unexpected directory"
    Copy-TestBundle -Source $bundle -Destination $directoryCase
    [IO.Directory]::CreateDirectory(
        (Join-Path $directoryCase "unexpected")
    ) | Out-Null
    Assert-VerifierFails `
        -ArtifactDirectory $directoryCase `
        -FailureMessage "A directory inside the release bundle was accepted."

    $oversized = Join-Path $testRoot "oversized payload"
    Copy-TestBundle -Source $bundle -Destination $oversized
    $oversizedStream = [IO.File]::Open(
        (Join-Path $oversized $payloads[0]),
        [IO.FileMode]::Open,
        [IO.FileAccess]::Write,
        [IO.FileShare]::None
    )
    try {
        $oversizedStream.SetLength(536870913)
    } finally {
        $oversizedStream.Dispose()
    }
    Assert-VerifierFails `
        -ArtifactDirectory $oversized `
        -FailureMessage "An oversized release payload was accepted."

    $badChecksums = Join-Path $testRoot "bad checksums"
    Copy-TestBundle -Source $bundle -Destination $badChecksums
    [IO.File]::AppendAllText(
        (Join-Path $badChecksums "SHA256SUMS"),
        "tampered"
    )
    Assert-VerifierFails `
        -ArtifactDirectory $badChecksums `
        -FailureMessage "A tampered checksum inventory was accepted."

    $badSignature = Join-Path $testRoot "bad signature"
    Copy-TestBundle -Source $bundle -Destination $badSignature
    [IO.File]::AppendAllText(
        (Join-Path $badSignature "$($payloads[0]).minisig"),
        "tampered"
    )
    Assert-VerifierFails `
        -ArtifactDirectory $badSignature `
        -FailureMessage "A tampered payload signature was accepted."

    & $MinisignPath `
        -G -W -f `
        -p $secondPublic `
        -s $secondSecret 2>&1 |
        Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "Second Minisign fixture-key generation failed."
    }
    & $normalizePublicKey -Path $secondPublic | Out-Null
    Assert-VerifierFails `
        -ArtifactDirectory $bundle `
        -VerificationPublicKey $secondPublic `
        -FailureMessage "A mismatched release public key was accepted."

    $maximumVersion = "1.0.0+" + ("a" * 122)
    if ($maximumVersion.Length -ne 128) {
        throw "Maximum-version fixture must be exactly 128 characters."
    }
    [IO.Directory]::CreateDirectory($maximumVersionBundle) | Out-Null
    $maximumVersionPayloads = @(
        Get-WokCorePayloadNames `
            -Version $maximumVersion `
            -IncludeLegacyV1
    )
    foreach ($name in $maximumVersionPayloads) {
        $path = Join-Path $maximumVersionBundle $name
        if (
            $name.EndsWith(".zip", [StringComparison]::Ordinal) -and
            (
                $name.Contains("-Portable.") -or
                $name.StartsWith("wokcore-", [StringComparison]::Ordinal)
            )
        ) {
            New-TestZipPackage -Path $path -Executable "wokcore.exe"
        } elseif (
            $name.EndsWith(".tar.gz", [StringComparison]::Ordinal)
        ) {
            New-TestTarPackage -Path $path -Executable "wokcore"
        } else {
            [IO.File]::WriteAllBytes(
                $path,
                [Text.Encoding]::UTF8.GetBytes("bounded fixture: $name")
            )
        }
    }
    & $writeManifest `
        -ArtifactDirectory $maximumVersionBundle `
        -Version $maximumVersion `
        -SigningKeyId $keyId `
        -SchemaVersion 1 `
        -OutputPath (
            Join-Path $maximumVersionBundle "wokcore-update-v1.json"
        ) |
        Out-Null
    & $writeManifest `
        -ArtifactDirectory $maximumVersionBundle `
        -Version $maximumVersion `
        -SigningKeyId $keyId `
        -SchemaVersion 2 `
        -OutputPath (
            Join-Path $maximumVersionBundle "wokcore-update-v2.json"
        ) |
        Out-Null
    & $signer `
        -ArtifactDirectory $maximumVersionBundle `
        -Version $maximumVersion `
        -SecretKeyPath $secret `
        -PublicKeyPath $public `
        -MinisignPath $MinisignPath
    $maximumChecksumsLength = (
        Get-Item -LiteralPath (
            Join-Path $maximumVersionBundle "SHA256SUMS"
        )
    ).Length
    if (
        $maximumChecksumsLength -le 4096 -or
        $maximumChecksumsLength -gt 8192
    ) {
        throw "Maximum-version SHA256SUMS did not exercise the size boundary."
    }
    & $verifier `
        -ArtifactDirectory $maximumVersionBundle `
        -Version $maximumVersion `
        -PublicKeyPath $public `
        -MinisignPath $MinisignPath

    New-Item -ItemType Junction -Path $junction -Target $bundle |
        Out-Null
    Assert-VerifierFails `
        -ArtifactDirectory $junction `
        -FailureMessage "The verifier accepted a reparse-point bundle ancestor."
    [IO.Directory]::Delete($junction)
} finally {
    if ([IO.Directory]::Exists($junction)) {
        [IO.Directory]::Delete($junction)
    }
    Clear-TestSecretKey -Path $secret
    Clear-TestSecretKey -Path $secondSecret
    if ([IO.Directory]::Exists($maximumVersionBundle)) {
        [IO.Directory]::Delete($maximumVersionBundle, $true)
    }
    if ([IO.Directory]::Exists($testRoot)) {
        [IO.Directory]::Delete($testRoot, $true)
    }
}

Write-Output "release bundle tests passed"
