$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "../.."))
$cargoPath = Join-Path $repositoryRoot "Cargo.toml"
$workflowPath = Join-Path $repositoryRoot ".github\workflows\release.yml"
$ciPath = Join-Path $repositoryRoot ".github\workflows\ci.yml"
$assembleBundlePath = Join-Path `
    $repositoryRoot `
    "tests\release\assemble-release-bundle.ps1"
$schemaV1Path = Join-Path $repositoryRoot "release\manifest-v1.schema.json"
$schemaV2Path = Join-Path $repositoryRoot "release\manifest-v2.schema.json"
foreach ($path in @(
    $cargoPath,
    $workflowPath,
    $ciPath,
    $assembleBundlePath,
    $schemaV1Path,
    $schemaV2Path
)) {
    if (-not [IO.File]::Exists($path)) {
        throw "Release workflow input is missing: $path"
    }
}
$cargo = [IO.File]::ReadAllText($cargoPath, [Text.Encoding]::UTF8)
$source = [IO.File]::ReadAllText($workflowPath, [Text.Encoding]::UTF8)
$ciSource = [IO.File]::ReadAllText($ciPath, [Text.Encoding]::UTF8)
$assembleBundleSource = [IO.File]::ReadAllText(
    $assembleBundlePath,
    [Text.Encoding]::UTF8
)
$schemaV1 = Get-Content -Raw -LiteralPath $schemaV1Path | ConvertFrom-Json
$schemaV2 = Get-Content -Raw -LiteralPath $schemaV2Path | ConvertFrom-Json

if ($cargo -notmatch '(?ms)\[workspace\.package\].*?version\s*=\s*"0\.1\.4"') {
    throw "Workspace package version must be 0.1.4."
}
if (
    $source -notmatch
        '(?ms)\$tagVersion\s*=\s*\$env:GITHUB_REF_NAME\.Substring\(1\).*?' +
        '\$tagVersion\s+-cne\s+\$version'
) {
    throw "Release tags must match the resolved workspace version."
}
if (
    $source -notmatch
        '(?ms)assemble-release-bundle\.ps1.*?' +
        '-Version\s+"\$\{\{\s*steps\.version\.outputs\.version\s*\}\}"'
) {
    throw "Release assembly must receive the resolved workspace version."
}
if (
    $assembleBundleSource -notmatch
        '(?ms)foreach\s*\(\$schemaVersion\s+in\s+@\(1,\s*2\)\).*?' +
        '&\s+\$writeManifest.*?-Version\s+\$Version.*?' +
        '-SchemaVersion\s+\$schemaVersion'
) {
    throw "Manifest schemas 1 and 2 must receive the resolved version."
}

# These checks catch a release contract that omits a supported target, misses a
# public or legacy payload, or leaks a Rust vendor segment into a public name.
Import-Module (Join-Path $PSScriptRoot "WokCore.ReleaseContract.psm1") -Force
$contracts = @(Get-WokCoreTargetContracts -Version "0.1.4")
if ($contracts.Count -ne 6) { throw "Expected six WokCore targets." }
$payloads = @(Get-WokCorePayloadNames -Version "0.1.4" -IncludeLegacyV1)
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
    "release/minisign.pub",
    "secrets.WOKCORE_MINISIGN_SECRET_KEY",
    "actions/upload-artifact@",
    "actions/download-artifact@",
    "gh release",
    "gh release delete-asset",
    "--draft",
    "--draft=false",
    "aarch64-pc-windows-msvc",
    "wokcore-update-v2.json",
    "WokCore.ReleaseContract.psm1",
    "sign-release-bundle.ps1",
    "verify-release-bundle.ps1",
    "gh release download"
)) {
    if ($source.IndexOf($required, [StringComparison]::Ordinal) -lt 0) {
        throw "Release workflow is missing a required contract: $required"
    }
}

$secretReference = "secrets.WOKCORE_MINISIGN_SECRET_KEY"
if (
    [regex]::Matches(
        $source,
        [regex]::Escape($secretReference)
    ).Count -ne 1
) {
    throw "Only the assemble/sign job may receive the Minisign secret."
}
$assembleStart = $source.IndexOf(
    "  assemble-release:",
    [StringComparison]::Ordinal
)
$assembleEnd = $source.IndexOf(
    "  windows-release-gate:",
    [StringComparison]::Ordinal
)
if (
    $assembleStart -lt 0 -or
    $assembleEnd -le $assembleStart -or
    $source.Substring(
        $assembleStart,
        $assembleEnd - $assembleStart
    ).IndexOf($secretReference, [StringComparison]::Ordinal) -lt 0
) {
    throw "The Minisign secret must be scoped to the assemble/sign job."
}

$publishStart = $source.IndexOf(
    "  publish-release:",
    [StringComparison]::Ordinal
)
if ($publishStart -lt 0) {
    throw "The publish job is missing."
}
$publishSource = $source.Substring($publishStart)
foreach ($requiredNeed in @(
    "release-contract",
    "build-release-package",
    "assemble-release",
    "windows-release-gate",
    "portable-soak"
)) {
    if (
        $publishSource.IndexOf(
            "      - $requiredNeed",
            [StringComparison]::Ordinal
        ) -lt 0
    ) {
        throw "Publish must need every release build and gate: $requiredNeed"
    }
}
foreach ($required in @(
    "--json isDraft,tagName",
    '[[ "$(jq -r .isDraft <<<"$existing")" == "true" ]]',
    '[[ "$(jq -r .tagName <<<"$existing")" == "$GITHUB_REF_NAME" ]]',
    '[[ "${#expected[@]}" -eq 45 ]]',
    "mktemp -d"
)) {
    if ($publishSource.IndexOf($required, [StringComparison]::Ordinal) -lt 0) {
        throw "Publish is missing an atomic draft contract: $required"
    }
}
$remoteDownload = $publishSource.IndexOf(
    "gh release download",
    [StringComparison]::Ordinal
)
$remoteVerification = $publishSource.IndexOf(
    "verify-release-bundle.ps1",
    $remoteDownload + 1,
    [StringComparison]::Ordinal
)
$makePublic = $publishSource.IndexOf(
    "--draft=false",
    [StringComparison]::Ordinal
)
if (
    $remoteDownload -lt 0 -or
    $remoteVerification -le $remoteDownload -or
    $makePublic -le $remoteVerification
) {
    throw "Remote release bytes must verify before the draft becomes public."
}
if (
    [regex]::IsMatch(
        $publishSource,
        "(?ms)gh release edit.*?--draft(?![=])"
    )
) {
    throw "Publish must never convert an existing public release back to draft."
}
if (
    $publishSource.IndexOf(
        '"$dist"/*',
        [StringComparison]::Ordinal
    ) -ge 0
) {
    throw "Publish must upload the verified exact inventory without a glob."
}
$assembleSource = $source.Substring(
    $assembleStart,
    $assembleEnd - $assembleStart
)
if (
    $assembleSource.IndexOf(
        "Upload verified release bundle",
        [StringComparison]::Ordinal
    ) -lt 0 -or
    $assembleSource.IndexOf(
        "if: always()",
        [StringComparison]::Ordinal
    ) -ge 0
) {
    throw "A failed assemble/sign step must not upload partial output."
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
$windowsReleaseContracts = @(
    @("windows-latest", "x86_64-pc-windows-msvc", "zip", "x86_64", "true"),
    @("windows-latest", "aarch64-pc-windows-msvc", "zip", "arm64", "false")
)
$unixReleaseContracts = @(
    @("macos-15-intel", "x86_64-apple-darwin", "tar.gz", "x86_64"),
    @("macos-14", "aarch64-apple-darwin", "tar.gz", "arm64"),
    @("ubuntu-24.04", "x86_64-unknown-linux-gnu", "tar.gz", "x86_64"),
    @("ubuntu-24.04-arm", "aarch64-unknown-linux-gnu", "tar.gz", "arm64")
)
$matrixContracts = @($windowsReleaseContracts + $unixReleaseContracts)
foreach ($contract in $windowsReleaseContracts) {
    $runner, $target, $extension, $publicArch, $runBinary = $contract
    $pattern = "(?ms)- os:\s+$([regex]::Escape($runner))\s+" +
        "target:\s+$([regex]::Escape($target))\s+" +
        "extension:\s+$([regex]::Escape($extension))\s+" +
        "public_arch:\s+$([regex]::Escape($publicArch))\s+" +
        "run_binary:\s+$([regex]::Escape($runBinary))\s*"
    if ([regex]::Matches($matrixSource, $pattern).Count -ne 1) {
        throw "Release package matrix has an invalid Windows target mapping: $target"
    }
}
foreach ($contract in $unixReleaseContracts) {
    $runner, $target, $extension, $publicArch = $contract
    $pattern = "(?ms)- os:\s+$([regex]::Escape($runner))\s+" +
        "target:\s+$([regex]::Escape($target))\s+" +
        "extension:\s+$([regex]::Escape($extension))\s+" +
        "public_arch:\s+$([regex]::Escape($publicArch))\s*"
    if ([regex]::Matches($matrixSource, $pattern).Count -ne 1) {
        throw "Release package matrix has an invalid native target mapping: $target"
    }
}

foreach ($required in @(
    "release-contract:",
    "macos-15-intel",
    "ubuntu-24.04-arm",
    "tests/release/build-package.ps1",
    "tests/release/build-windows-assets.ps1",
    "tests/release/build-windows-assets.tests.ps1",
    "tests/release/build-package.sh",
    "tests/release/normalize-minisign-public-key.ps1",
    "tests/release/verify-manifest.tests.ps1",
    "ci-wokcore-",
    "actions/download-artifact@"
)) {
    if ($ciSource.IndexOf($required, [StringComparison]::Ordinal) -lt 0) {
        throw "CI is missing a required release contract: $required"
    }
}

$targetCheckStart = $ciSource.IndexOf(
    "  target-check:",
    [StringComparison]::Ordinal
)
$targetCheckEnd = $ciSource.IndexOf(
    "  release-contract:",
    [StringComparison]::Ordinal
)
if ($targetCheckStart -lt 0 -or $targetCheckEnd -le $targetCheckStart) {
    throw "CI target-check boundaries are invalid."
}
$targetCheckSource = $ciSource.Substring(
    $targetCheckStart,
    $targetCheckEnd - $targetCheckStart
)
$unixCiContracts = @(
    @("macos-15-intel", "x86_64-apple-darwin", "tar.gz", "x86_64"),
    @("macos-15", "aarch64-apple-darwin", "tar.gz", "arm64"),
    @("ubuntu-24.04", "x86_64-unknown-linux-gnu", "tar.gz", "x86_64"),
    @("ubuntu-24.04-arm", "aarch64-unknown-linux-gnu", "tar.gz", "arm64")
)
$windowsCiContracts = @(
    @(
        "windows-latest",
        "x86_64-pc-windows-msvc",
        "zip",
        "x86_64",
        "true"
    ),
    @(
        "windows-latest",
        "aarch64-pc-windows-msvc",
        "zip",
        "arm64",
        "false"
    )
)
foreach ($contract in $windowsCiContracts) {
    $runner, $target, $extension, $publicArch, $runBinary = $contract
    $pattern = "(?ms)- os:\s+$([regex]::Escape($runner))\s+" +
        "target:\s+$([regex]::Escape($target))\s+" +
        "extension:\s+$([regex]::Escape($extension))\s+" +
        "public_arch:\s+$([regex]::Escape($publicArch))\s+" +
        "run_binary:\s+$([regex]::Escape($runBinary))\s*"
    if ([regex]::Matches($targetCheckSource, $pattern).Count -ne 1) {
        throw "CI has an invalid Windows target/runtime mapping: $target"
    }
}
foreach ($contract in $unixCiContracts) {
    $runner, $target, $extension, $publicArch = $contract
    $pattern = "(?ms)- os:\s+$([regex]::Escape($runner))\s+" +
        "target:\s+$([regex]::Escape($target))\s+" +
        "extension:\s+$([regex]::Escape($extension))\s+" +
        "public_arch:\s+$([regex]::Escape($publicArch))\s*"
    if ([regex]::Matches($targetCheckSource, $pattern).Count -ne 1) {
        throw "CI has an invalid Unix target/public-architecture mapping: $target"
    }
}
foreach ($testScript in @(
    "build-linux-assets.tests.sh",
    "build-macos-assets.tests.sh"
)) {
    $pattern = 'bash\s+tests/release/' +
        [regex]::Escape($testScript) +
        '\s+"\$\{\{ matrix\.target \}\}"\s+"\$\{\{ matrix\.public_arch \}\}"'
    if ([regex]::Matches($targetCheckSource, $pattern).Count -ne 1) {
        throw "CI target-check does not run $testScript for its exact target."
    }
}
foreach ($required in @(
    "powershell.exe -NoProfile -ExecutionPolicy Bypass -File tests/release/build-windows-assets.tests.ps1",
    "tests/release/build-windows-assets.ps1",
    "0xaa64",
    'if: runner.os == ''Windows'' && matrix.run_binary',
    'if: runner.os == ''Windows'' && !matrix.run_binary',
    '${{ runner.temp }}/wokcore-release/WokCore-v${{ steps.version.outputs.version }}-Windows-${{ matrix.public_arch }}-Portable.zip',
    '${{ runner.temp }}/wokcore-release/WokCore-v${{ steps.version.outputs.version }}-Windows-${{ matrix.public_arch }}.msi',
    "bash tests/release/build-linux-assets.sh",
    "bash tests/release/build-macos-assets.sh",
    '${{ runner.temp }}/wokcore-release/WokCore-v${{ steps.version.outputs.version }}-Linux-${{ matrix.public_arch }}.tar.gz',
    '${{ runner.temp }}/wokcore-release/WokCore-v${{ steps.version.outputs.version }}-Linux-${{ matrix.public_arch }}.deb',
    '${{ runner.temp }}/wokcore-release/WokCore-v${{ steps.version.outputs.version }}-Linux-${{ matrix.public_arch }}.rpm',
    '${{ runner.temp }}/wokcore-release/WokCore-v${{ steps.version.outputs.version }}-macOS-${{ matrix.public_arch }}.tar.gz',
    '${{ runner.temp }}/wokcore-release/WokCore-v${{ steps.version.outputs.version }}-macOS-${{ matrix.public_arch }}.zip'
)) {
    if ($targetCheckSource.IndexOf($required, [StringComparison]::Ordinal) -lt 0) {
        throw "CI target-check is missing an exact asset contract: $required"
    }
}
if (
    $targetCheckSource.IndexOf(
        '${{ runner.temp }}/wokcore-release/WokCore-v*',
        [StringComparison]::Ordinal
    ) -ge 0
) {
    throw "CI target-check uses a wildcard for friendly asset upload."
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
$legacyV1Contracts = @(
    $matrixContracts |
        Where-Object { $_[1] -cne "aarch64-pc-windows-msvc" }
)
if (
    $schemaV1.properties.artifacts.items -ne $false -or
    $prefixItems.Count -ne $legacyV1Contracts.Count
) {
    throw "Release schema must define exactly five ordered artifact contracts."
}
for ($index = 0; $index -lt $legacyV1Contracts.Count; $index++) {
    $target = $legacyV1Contracts[$index][1]
    $extension = $legacyV1Contracts[$index][2]
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
