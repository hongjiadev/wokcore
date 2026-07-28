$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\.."))
$runner = Join-Path $PSScriptRoot "run-provider-gates.sh"
$ciWorkflow = Join-Path $repositoryRoot ".github\workflows\ci.yml"
$releaseWorkflow = Join-Path $repositoryRoot ".github\workflows\release.yml"
$workspaceManifest = Join-Path $repositoryRoot "Cargo.toml"
$appManifest = Join-Path $repositoryRoot "apps\wokcore\Cargo.toml"
$serverManifest = Join-Path $repositoryRoot "crates\wokcore-server\Cargo.toml"
$appMain = Join-Path $repositoryRoot "apps\wokcore\src\main.rs"
$memoryBackend = Join-Path $repositoryRoot "crates\wokcore-server\src\lifecycle\memory.rs"

foreach ($path in @(
    $runner,
    $ciWorkflow,
    $releaseWorkflow,
    $workspaceManifest,
    $appManifest,
    $serverManifest,
    $appMain,
    $memoryBackend
)) {
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
    'vmmap -summary -resident',
    'macos_vmmap',
    'vmmap_diagnostic',
    'parse-vmmap-summary.py',
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
$workspaceManifestSource = [IO.File]::ReadAllText(
    $workspaceManifest,
    [Text.Encoding]::UTF8
)
$appManifestSource = [IO.File]::ReadAllText(
    $appManifest,
    [Text.Encoding]::UTF8
)
$serverManifestSource = [IO.File]::ReadAllText(
    $serverManifest,
    [Text.Encoding]::UTF8
)
$appMainSource = [IO.File]::ReadAllText($appMain, [Text.Encoding]::UTF8)
$memoryBackendSource = [IO.File]::ReadAllText(
    $memoryBackend,
    [Text.Encoding]::UTF8
)
foreach ($contract in @(
    @{
        Source = $workspaceManifestSource
        Required = @(
            'tikv-jemalloc-sys = "=0.7.1"',
            'tikv-jemallocator = "=0.7.0"'
        )
    },
    @{
        Source = $appManifestSource
        Required = @(
            "[target.'cfg(target_os = `"macos`")'.dependencies]",
            "tikv-jemalloc-sys.workspace = true",
            "tikv-jemallocator.workspace = true"
        )
    },
    @{
        Source = $serverManifestSource
        Required = @(
            "[target.'cfg(target_os = `"macos`")'.dependencies]",
            "tikv-jemalloc-sys.workspace = true"
        )
    },
    @{
        Source = $appMainSource
        Required = @(
            "#[global_allocator]",
            "_rjem_malloc_conf",
            "narenas:2,dirty_decay_ms:0,muzzy_decay_ms:0",
            'c"opt.narenas"',
            'c"opt.dirty_decay_ms"',
            'c"opt.muzzy_decay_ms"'
        )
    },
    @{
        Source = $memoryBackendSource
        Required = @(
            "tikv_jemalloc_sys::mallctl",
            "arena.4096.purge"
        )
    }
)) {
    foreach ($required in $contract.Required) {
        if (
            $contract.Source.IndexOf(
                $required,
                [StringComparison]::Ordinal
            ) -lt 0
        ) {
            throw "macOS allocator policy is missing a required invariant: $required"
        }
    }
}
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
