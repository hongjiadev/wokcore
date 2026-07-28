$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\.."))
$runner = Join-Path $PSScriptRoot "run-provider-gates.sh"
$ciWorkflow = Join-Path $repositoryRoot ".github\workflows\ci.yml"
$releaseWorkflow = Join-Path $repositoryRoot ".github\workflows\release.yml"

foreach ($path in @($runner, $ciWorkflow, $releaseWorkflow)) {
    if (-not [IO.File]::Exists($path)) {
        throw "Portable Provider gate input is missing: $path"
    }
}

$runnerSource = [IO.File]::ReadAllText($runner, [Text.Encoding]::UTF8)
foreach ($required in @(
    'PROFILE_SECONDS=300',
    'PROFILE_SECONDS=1800',
    'wokcore',
    'wokcore-provider-sim',
    'wokcore-loadgen',
    '127.0.0.1',
    'catalog_id = "ollama"',
    'kind = "local"',
    'OPENAI_API_KEY',
    'ANTHROPIC_API_KEY',
    'GOOGLE_API_KEY',
    'AZURE_OPENAI_API_KEY',
    'env -i',
    'DBUS_SESSION_BUS_ADDRESS',
    'gnome-keyring-daemon',
    'security create-keychain',
    'network_loopback_only',
    'recovery_rss_kib',
    'final_fd_count',
    'final_task_count',
    '131072'
)) {
    if (
        $runnerSource.IndexOf(
            $required,
            [StringComparison]::Ordinal
        ) -lt 0
    ) {
        throw "Portable Provider gate is missing a required invariant: $required"
    }
}
foreach ($forbidden in @(
    "api.openai.com",
    "api.anthropic.com",
    "generativelanguage.googleapis.com",
    "secrets.",
    "curl ",
    "wget "
)) {
    if (
        $runnerSource.IndexOf(
            $forbidden,
            [StringComparison]::OrdinalIgnoreCase
        ) -ge 0
    ) {
        throw "Portable Provider gate contains a forbidden capability: $forbidden"
    }
}

$ciSource = [IO.File]::ReadAllText($ciWorkflow, [Text.Encoding]::UTF8)
$releaseSource = [IO.File]::ReadAllText($releaseWorkflow, [Text.Encoding]::UTF8)
$releasePerformanceStart = $releaseSource.IndexOf(
    "  windows-release-gate:",
    [StringComparison]::Ordinal
)
$releasePerformanceEnd = $releaseSource.IndexOf(
    "  publish-release:",
    [StringComparison]::Ordinal
)
if (
    $releasePerformanceStart -lt 0 -or
    $releasePerformanceEnd -le $releasePerformanceStart
) {
    throw "Release performance job boundaries are invalid."
}
$releasePerformanceSource = $releaseSource.Substring(
    $releasePerformanceStart,
    $releasePerformanceEnd - $releasePerformanceStart
)
foreach ($required in @(
    "windows-performance:",
    "portable-performance:",
    "ubuntu-24.04",
    "macos-15",
    "portable-provider-gates.tests.ps1",
    "run-provider-gates.tests.sh",
    "run-provider-gates.ps1",
    "run-provider-gates.sh",
    "actions/upload-artifact@",
    "persist-credentials: false",
    "contents: read"
)) {
    if (
        $ciSource.IndexOf(
            $required,
            [StringComparison]::Ordinal
        ) -lt 0
    ) {
        throw "CI workflow is missing a portable performance invariant: $required"
    }
}
foreach ($required in @(
    "portable-soak:",
    "ubuntu-24.04",
    "macos-15",
    "--profile soak",
    "actions/upload-artifact@",
    "persist-credentials: false",
    "contents: read"
)) {
    if (
        $releaseSource.IndexOf(
            $required,
            [StringComparison]::Ordinal
        ) -lt 0
    ) {
        throw "Release workflow is missing a soak invariant: $required"
    }
}
foreach ($source in @($ciSource, $releasePerformanceSource)) {
    foreach ($forbidden in @(
        "secrets.",
        "OPENAI_API_KEY: `${{",
        "ANTHROPIC_API_KEY: `${{",
        "GOOGLE_API_KEY: `${{",
        "AZURE_OPENAI_API_KEY: `${{"
    )) {
        if (
            $source.IndexOf(
                $forbidden,
                [StringComparison]::OrdinalIgnoreCase
            ) -ge 0
        ) {
            throw "A performance workflow exposes a secret context."
        }
    }
}
$releaseSecretReferences = @(
    [regex]::Matches(
        $releaseSource,
        "secrets\.[A-Za-z0-9_]+"
    ) | ForEach-Object Value
)
if (
    $releaseSecretReferences.Count -ne 1 -or
    $releaseSecretReferences[0] -cne
        "secrets.WOKCORE_MINISIGN_SECRET_KEY"
) {
    throw "Release workflow references an unapproved secret context."
}

Write-Output "portable provider gate policy tests passed"
