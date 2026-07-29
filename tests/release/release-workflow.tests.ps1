$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "../.."))
$workflowPath = Join-Path $repositoryRoot ".github\workflows\release.yml"
$ciPath = Join-Path $repositoryRoot ".github\workflows\ci.yml"
$schemaV1Path = Join-Path $repositoryRoot "release\manifest-v1.schema.json"
$schemaV2Path = Join-Path $repositoryRoot "release\manifest-v2.schema.json"
foreach ($path in @($workflowPath, $ciPath, $schemaV1Path, $schemaV2Path)) {
    if (-not [IO.File]::Exists($path)) {
        throw "Release workflow input is missing: $path"
    }
}
$source = [IO.File]::ReadAllText($workflowPath, [Text.Encoding]::UTF8)
$ciSource = [IO.File]::ReadAllText($ciPath, [Text.Encoding]::UTF8)
$schemaV1 = Get-Content -Raw -LiteralPath $schemaV1Path | ConvertFrom-Json
$schemaV2 = Get-Content -Raw -LiteralPath $schemaV2Path | ConvertFrom-Json

# These checks catch a release contract that omits a supported target, misses a
# public or legacy payload, or leaks a Rust vendor segment into a public name.
Import-Module (Join-Path $PSScriptRoot "WokCore.ReleaseContract.psm1") -Force
$contracts = @(Get-WokCoreTargetContracts -Version "0.1.1")
if ($contracts.Count -ne 6) { throw "Expected six WokCore targets." }
$payloads = @(Get-WokCorePayloadNames -Version "0.1.1" -IncludeLegacyV1)
if ($payloads.Count -ne 19) { throw "Expected 19 WokCore payloads." }
$publicNames = $payloads | Where-Object { $_ -clike "WokCore-*" }
if ($publicNames -match "unknown") { throw "Public names expose a Rust vendor segment." }

foreach ($required in @(
    "build-release-package:",
    "release-contract:",
    "assemble-release:",
    "publish-release:",
    "windows-latest",
    "macos-15-intel",
    "macos-15",
    "ubuntu-24.04",
    "ubuntu-24.04-arm",
    "x86_64-pc-windows-msvc",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "tests/release/build-package.ps1",
    "tests/release/build-package.sh",
    "tests/release/build-package.tests.sh",
    "tests/release/verify-manifest.tests.ps1",
    "tests/release/write-manifest.ps1",
    "tests/release/verify-manifest.ps1",
    "release/minisign.pub",
    "secrets.WOKCORE_MINISIGN_SECRET_KEY",
    "wokcore-update-v1.json.minisig",
    "SHA256SUMS",
    "actions/upload-artifact@",
    "actions/download-artifact@",
    "gh release",
    "gh release delete-asset",
    "--draft",
    "--draft=false",
    "verify-manifest.ps1",
    "wokcore-v`$version-aarch64-apple-darwin.tar.gz"
)) {
    if ($source.IndexOf($required, [StringComparison]::Ordinal) -lt 0) {
        throw "Release workflow is missing a required contract: $required"
    }
}

$matrixStart = $source.IndexOf(
    "  build-release-package:",
    [StringComparison]::Ordinal
)
$matrixEnd = $source.IndexOf(
    "  assemble-release:",
    [StringComparison]::Ordinal
)
if ($matrixStart -lt 0 -or $matrixEnd -le $matrixStart) {
    throw "Release package matrix boundaries are invalid."
}
$matrixSource = $source.Substring($matrixStart, $matrixEnd - $matrixStart)
$matrixContracts = @(
    @("windows-latest", "x86_64-pc-windows-msvc", "zip"),
    @("macos-15-intel", "x86_64-apple-darwin", "tar.gz"),
    @("macos-15", "aarch64-apple-darwin", "tar.gz"),
    @("ubuntu-24.04", "x86_64-unknown-linux-gnu", "tar.gz"),
    @("ubuntu-24.04-arm", "aarch64-unknown-linux-gnu", "tar.gz")
)
foreach ($contract in $matrixContracts) {
    $runner, $target, $extension = $contract
    $pattern = "(?ms)- os:\s+$([regex]::Escape($runner))\s+" +
        "target:\s+$([regex]::Escape($target))\s+" +
        "extension:\s+$([regex]::Escape($extension))\s*"
    if ([regex]::Matches($matrixSource, $pattern).Count -ne 1) {
        throw "Release package matrix has an invalid native target mapping: $target"
    }
}

foreach ($required in @(
    "release-contract:",
    "macos-15-intel",
    "ubuntu-24.04-arm",
    "tests/release/build-package.ps1",
    "tests/release/build-package.sh",
    "tests/release/normalize-minisign-public-key.ps1",
    "tests/release/verify-manifest.tests.ps1",
    "ci-wokcore-package-",
    "actions/download-artifact@"
)) {
    if ($ciSource.IndexOf($required, [StringComparison]::Ordinal) -lt 0) {
        throw "CI is missing a required release contract: $required"
    }
}

foreach ($workflowSource in @($source, $ciSource)) {
    foreach ($match in [regex]::Matches(
        $workflowSource,
        "(?m)^\s+(?:-\s+)?uses:\s+([^\s#]+)"
    )) {
        $reference = $match.Groups[1].Value
        if (
            -not $reference.StartsWith("./", [StringComparison]::Ordinal) -and
            $reference -cnotmatch "@[0-9a-f]{40}$"
        ) {
            throw "Workflow action is not pinned to an immutable commit: $reference"
        }
    }
}

$prefixItems = @($schemaV1.properties.artifacts.prefixItems)
if (
    $schemaV1.properties.artifacts.items -ne $false -or
    $prefixItems.Count -ne $matrixContracts.Count
) {
    throw "Release schema must define exactly five ordered artifact contracts."
}
for ($index = 0; $index -lt $matrixContracts.Count; $index++) {
    $target = $matrixContracts[$index][1]
    $extension = $matrixContracts[$index][2]
    $item = $prefixItems[$index]
    $expectedExecutable = if ($extension -ceq "zip") {
        "wokcore.exe"
    } else {
        "wokcore"
    }
    if (
        $item.properties.target.const -cne $target -or
        $item.properties.executable.const -cne $expectedExecutable -or
        $item.properties.file.pattern.IndexOf(
            $target,
            [StringComparison]::Ordinal
        ) -lt 0
    ) {
        throw "Release schema target mapping is not exact: $target"
    }
}

$v2Contracts = @(
    @(
        "x86_64-pc-windows-msvc",
        "wokcore.exe",
        "^WokCore-v[0-9]+\.[0-9]+\.[0-9]+-Windows-x86_64-Portable\.zip$",
        "^https://github\.com/hongjiadev/wokcore/releases/download/v[0-9]+\.[0-9]+\.[0-9]+/WokCore-v[0-9]+\.[0-9]+\.[0-9]+-Windows-x86_64-Portable\.zip$"
    ),
    @(
        "aarch64-pc-windows-msvc",
        "wokcore.exe",
        "^WokCore-v[0-9]+\.[0-9]+\.[0-9]+-Windows-arm64-Portable\.zip$",
        "^https://github\.com/hongjiadev/wokcore/releases/download/v[0-9]+\.[0-9]+\.[0-9]+/WokCore-v[0-9]+\.[0-9]+\.[0-9]+-Windows-arm64-Portable\.zip$"
    ),
    @(
        "x86_64-apple-darwin",
        "wokcore",
        "^WokCore-v[0-9]+\.[0-9]+\.[0-9]+-macOS-x86_64\.tar\.gz$",
        "^https://github\.com/hongjiadev/wokcore/releases/download/v[0-9]+\.[0-9]+\.[0-9]+/WokCore-v[0-9]+\.[0-9]+\.[0-9]+-macOS-x86_64\.tar\.gz$"
    ),
    @(
        "aarch64-apple-darwin",
        "wokcore",
        "^WokCore-v[0-9]+\.[0-9]+\.[0-9]+-macOS-arm64\.tar\.gz$",
        "^https://github\.com/hongjiadev/wokcore/releases/download/v[0-9]+\.[0-9]+\.[0-9]+/WokCore-v[0-9]+\.[0-9]+\.[0-9]+-macOS-arm64\.tar\.gz$"
    ),
    @(
        "x86_64-unknown-linux-gnu",
        "wokcore",
        "^WokCore-v[0-9]+\.[0-9]+\.[0-9]+-Linux-x86_64\.tar\.gz$",
        "^https://github\.com/hongjiadev/wokcore/releases/download/v[0-9]+\.[0-9]+\.[0-9]+/WokCore-v[0-9]+\.[0-9]+\.[0-9]+-Linux-x86_64\.tar\.gz$"
    ),
    @(
        "aarch64-unknown-linux-gnu",
        "wokcore",
        "^WokCore-v[0-9]+\.[0-9]+\.[0-9]+-Linux-arm64\.tar\.gz$",
        "^https://github\.com/hongjiadev/wokcore/releases/download/v[0-9]+\.[0-9]+\.[0-9]+/WokCore-v[0-9]+\.[0-9]+\.[0-9]+-Linux-arm64\.tar\.gz$"
    )
)
$v2PrefixItems = @($schemaV2.properties.artifacts.prefixItems)
if (
    $schemaV2.properties.schema_version.const -ne 2 -or
    $schemaV2.properties.artifacts.minItems -ne 6 -or
    $schemaV2.properties.artifacts.maxItems -ne 6 -or
    $schemaV2.properties.artifacts.items -ne $false -or
    $v2PrefixItems.Count -ne 6
) {
    throw "Release schema v2 must define exactly six ordered artifact contracts."
}
for ($index = 0; $index -lt $v2Contracts.Count; $index++) {
    $target, $executable, $filePattern, $urlPattern = $v2Contracts[$index]
    $properties = $v2PrefixItems[$index].allOf[1].properties
    if (
        $properties.target.const -cne $target -or
        $properties.executable.const -cne $executable -or
        $properties.file.pattern -cne $filePattern -or
        $properties.url.pattern -cne $urlPattern
    ) {
        throw "Release schema v2 target mapping is not exact: $target"
    }
}

Write-Output "release workflow contract tests passed: six targets and 19 payloads"
