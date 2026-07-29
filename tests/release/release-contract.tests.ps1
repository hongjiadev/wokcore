$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

# These checks catch fictional or omitted targets, incorrect target mappings,
# leaked Rust vendor segments, incorrect v1 bridge membership, and missing,
# incorrectly ordered public or legacy payloads.
Import-Module (Join-Path $PSScriptRoot "WokCore.ReleaseContract.psm1") -Force

$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "../.."))
$attributes = [IO.File]::ReadAllLines(
    (Join-Path $repositoryRoot ".gitattributes"),
    [Text.Encoding]::UTF8
)
if ($attributes -cnotcontains "/release/minisign.pub text eol=lf") {
    throw "The production Minisign public key must retain LF line endings."
}

$contracts = @(Get-WokCoreTargetContracts -Version "0.1.1")
$expectedContracts = @(
    [pscustomobject]@{
        Target = "x86_64-pc-windows-msvc"; System = "Windows"; Architecture = "x86_64"; Executable = "wokcore.exe"; PortableExtension = "zip"; FriendlyPortableName = "WokCore-v0.1.1-Windows-x86_64-Portable.zip"; LegacyV1Name = "wokcore-v0.1.1-x86_64-pc-windows-msvc.zip"; LegacyV1 = $true
    },
    [pscustomobject]@{
        Target = "aarch64-pc-windows-msvc"; System = "Windows"; Architecture = "arm64"; Executable = "wokcore.exe"; PortableExtension = "zip"; FriendlyPortableName = "WokCore-v0.1.1-Windows-arm64-Portable.zip"; LegacyV1Name = "wokcore-v0.1.1-aarch64-pc-windows-msvc.zip"; LegacyV1 = $false
    },
    [pscustomobject]@{
        Target = "x86_64-apple-darwin"; System = "macOS"; Architecture = "x86_64"; Executable = "wokcore"; PortableExtension = "tar.gz"; FriendlyPortableName = "WokCore-v0.1.1-macOS-x86_64.tar.gz"; LegacyV1Name = "wokcore-v0.1.1-x86_64-apple-darwin.tar.gz"; LegacyV1 = $true
    },
    [pscustomobject]@{
        Target = "aarch64-apple-darwin"; System = "macOS"; Architecture = "arm64"; Executable = "wokcore"; PortableExtension = "tar.gz"; FriendlyPortableName = "WokCore-v0.1.1-macOS-arm64.tar.gz"; LegacyV1Name = "wokcore-v0.1.1-aarch64-apple-darwin.tar.gz"; LegacyV1 = $true
    },
    [pscustomobject]@{
        Target = "x86_64-unknown-linux-gnu"; System = "Linux"; Architecture = "x86_64"; Executable = "wokcore"; PortableExtension = "tar.gz"; FriendlyPortableName = "WokCore-v0.1.1-Linux-x86_64.tar.gz"; LegacyV1Name = "wokcore-v0.1.1-x86_64-unknown-linux-gnu.tar.gz"; LegacyV1 = $true
    },
    [pscustomobject]@{
        Target = "aarch64-unknown-linux-gnu"; System = "Linux"; Architecture = "arm64"; Executable = "wokcore"; PortableExtension = "tar.gz"; FriendlyPortableName = "WokCore-v0.1.1-Linux-arm64.tar.gz"; LegacyV1Name = "wokcore-v0.1.1-aarch64-unknown-linux-gnu.tar.gz"; LegacyV1 = $true
    }
)
if ($contracts.Count -ne $expectedContracts.Count) { throw "Expected six WokCore targets." }
if (@($contracts.Target | Sort-Object -Unique).Count -ne 6) {
    throw "WokCore targets must be unique."
}
foreach ($expectedContract in $expectedContracts) {
    $actualContracts = @($contracts | Where-Object Target -ceq $expectedContract.Target)
    if ($actualContracts.Count -ne 1) {
        throw "Missing or duplicate WokCore target: $($expectedContract.Target)"
    }
    foreach ($property in @(
        "Target", "System", "Architecture", "Executable", "PortableExtension",
        "FriendlyPortableName", "LegacyV1Name", "LegacyV1"
    )) {
        if ($actualContracts[0].$property -cne $expectedContract.$property) {
            throw "WokCore target mapping is incorrect: $($expectedContract.Target) $property"
        }
    }
}
if (@($contracts | Where-Object FriendlyPortableName -Match "unknown").Count -ne 0) {
    throw "Public WokCore names must not expose the Rust vendor segment."
}
if (@($contracts | Where-Object LegacyV1).Count -ne 5) {
    throw "Exactly five contracts must remain in the v1 bridge."
}
$expectedPublicPayloads = @(
    "WokCore-v0.1.1-Linux-arm64.deb",
    "WokCore-v0.1.1-Linux-arm64.rpm",
    "WokCore-v0.1.1-Linux-arm64.tar.gz",
    "WokCore-v0.1.1-Linux-x86_64.deb",
    "WokCore-v0.1.1-Linux-x86_64.rpm",
    "WokCore-v0.1.1-Linux-x86_64.tar.gz",
    "WokCore-v0.1.1-Windows-arm64-Portable.zip",
    "WokCore-v0.1.1-Windows-arm64.msi",
    "WokCore-v0.1.1-Windows-x86_64-Portable.zip",
    "WokCore-v0.1.1-Windows-x86_64.msi",
    "WokCore-v0.1.1-macOS-arm64.tar.gz",
    "WokCore-v0.1.1-macOS-arm64.zip",
    "WokCore-v0.1.1-macOS-x86_64.tar.gz",
    "WokCore-v0.1.1-macOS-x86_64.zip"
)
$expectedPayloadsWithLegacyV1 = @(
    "WokCore-v0.1.1-Linux-arm64.deb",
    "WokCore-v0.1.1-Linux-arm64.rpm",
    "WokCore-v0.1.1-Linux-arm64.tar.gz",
    "WokCore-v0.1.1-Linux-x86_64.deb",
    "WokCore-v0.1.1-Linux-x86_64.rpm",
    "WokCore-v0.1.1-Linux-x86_64.tar.gz",
    "WokCore-v0.1.1-Windows-arm64-Portable.zip",
    "WokCore-v0.1.1-Windows-arm64.msi",
    "WokCore-v0.1.1-Windows-x86_64-Portable.zip",
    "WokCore-v0.1.1-Windows-x86_64.msi",
    "WokCore-v0.1.1-macOS-arm64.tar.gz",
    "WokCore-v0.1.1-macOS-arm64.zip",
    "WokCore-v0.1.1-macOS-x86_64.tar.gz",
    "WokCore-v0.1.1-macOS-x86_64.zip",
    "wokcore-v0.1.1-aarch64-apple-darwin.tar.gz",
    "wokcore-v0.1.1-aarch64-unknown-linux-gnu.tar.gz",
    "wokcore-v0.1.1-x86_64-apple-darwin.tar.gz",
    "wokcore-v0.1.1-x86_64-pc-windows-msvc.zip",
    "wokcore-v0.1.1-x86_64-unknown-linux-gnu.tar.gz"
)
$publicPayloads = @(Get-WokCorePayloadNames -Version "0.1.1")
if ([string]::Join("`n", $publicPayloads) -cne [string]::Join("`n", $expectedPublicPayloads)) {
    throw "Public WokCore payloads must match the 14-name ordered contract."
}
$payloadsWithLegacyV1 = @(Get-WokCorePayloadNames -Version "0.1.1" -IncludeLegacyV1)
if ([string]::Join("`n", $payloadsWithLegacyV1) -cne [string]::Join("`n", $expectedPayloadsWithLegacyV1)) {
    throw "WokCore payloads with legacy v1 must match the 19-name ordered contract."
}

Write-Output "release contract tests passed: six targets and 19 payloads"
