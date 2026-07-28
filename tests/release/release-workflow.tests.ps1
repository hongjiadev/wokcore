$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "../.."))
$workflowPath = Join-Path $repositoryRoot ".github\workflows\release.yml"
$ciPath = Join-Path $repositoryRoot ".github\workflows\ci.yml"
$schemaPath = Join-Path $repositoryRoot "release\manifest-v1.schema.json"
foreach ($path in @($workflowPath, $ciPath, $schemaPath)) {
    if (-not [IO.File]::Exists($path)) {
        throw "Release workflow input is missing: $path"
    }
}
$source = [IO.File]::ReadAllText($workflowPath, [Text.Encoding]::UTF8)
$ciSource = [IO.File]::ReadAllText($ciPath, [Text.Encoding]::UTF8)
$schema = Get-Content -Raw -LiteralPath $schemaPath | ConvertFrom-Json

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

$prefixItems = @($schema.properties.artifacts.prefixItems)
if (
    $schema.properties.artifacts.items -ne $false -or
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

Write-Output "release workflow contract tests passed"
