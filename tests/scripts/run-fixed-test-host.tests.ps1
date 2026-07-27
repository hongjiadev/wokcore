$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$runner = Join-Path $PSScriptRoot "run-fixed-test-host.ps1"
$temporaryRoot = Join-Path ([System.IO.Path]::GetTempPath()) (
    "wokcore-fixed-test-host-{0}" -f [Guid]::NewGuid().ToString("N")
)

try {
    New-Item -ItemType Directory -Path $temporaryRoot | Out-Null
    $firstArtifact = Join-Path $temporaryRoot "first-artifact.exe"
    $secondArtifact = Join-Path $temporaryRoot "second-artifact.exe"
    Copy-Item -LiteralPath $env:ComSpec -Destination $firstArtifact
    Copy-Item -LiteralPath $env:ComSpec -Destination $secondArtifact

    $manifest = Join-Path $temporaryRoot "artifacts.jsonl"
    @(
        @{
            reason = "compiler-artifact"
            package_id = "fixture 0.1.0"
            target = @{ name = "first"; kind = @("test") }
            profile = @{ test = $true }
            executable = $firstArtifact
        },
        @{
            reason = "compiler-artifact"
            package_id = "fixture 0.1.0"
            target = @{ name = "second"; kind = @("test") }
            profile = @{ test = $true }
            executable = $secondArtifact
        }
    ) | ForEach-Object {
        $_ | ConvertTo-Json -Compress -Depth 5
    } | Set-Content -LiteralPath $manifest -Encoding UTF8

    $observed = Join-Path $temporaryRoot "observed-hosts.txt"
    & $runner `
        -ArtifactManifestPath $manifest `
        -TargetDirectory $temporaryRoot `
        -HarnessArguments @(
            "/d",
            "/c",
            "echo %CMDCMDLINE%>>`"$observed`""
        )

    $hosts = @(Get-Content -LiteralPath $observed)
    if ($hosts.Count -ne 2) {
        throw "Expected two test artifacts to execute, observed $($hosts.Count)"
    }
    $fixedHost = Join-Path $temporaryRoot "wokcore-test-host.exe"
    foreach ($hostCommandLine in $hosts) {
        if ($hostCommandLine -notlike "*$fixedHost*") {
            throw "Test artifact did not execute through the fixed host: $hostCommandLine"
        }
    }
    if (-not (Test-Path -LiteralPath $fixedHost -PathType Leaf)) {
        throw "Fixed test host was not retained at $fixedHost"
    }

    Remove-Item -LiteralPath $observed
    $fakeCargo = Join-Path $temporaryRoot "cargo-fixture.cmd"
    @"
@echo off
echo Finished fixture 1>&2
type "$manifest"
exit /b 0
"@ | Set-Content -LiteralPath $fakeCargo -Encoding ASCII
    & $runner `
        -CargoCommand $fakeCargo `
        -RepositoryRoot $temporaryRoot `
        -TargetDirectory $temporaryRoot `
        -HarnessArguments @(
            "/d",
            "/c",
            "echo %CMDCMDLINE%>>`"$observed`""
        )
    $compiledHosts = @(Get-Content -LiteralPath $observed)
    if ($compiledHosts.Count -ne 2) {
        throw "Expected the Cargo discovery path to execute two artifacts"
    }
    foreach ($hostCommandLine in $compiledHosts) {
        if ($hostCommandLine -notlike "*$fixedHost*") {
            throw "Cargo-discovered artifact bypassed the fixed host: $hostCommandLine"
        }
    }

    $failed = $false
    try {
        & $runner `
            -ArtifactManifestPath $manifest `
            -TargetDirectory $temporaryRoot `
            -HarnessArguments @("/d", "/c", "exit /b 23")
    } catch {
        $failed = $_.Exception.Message -like "*exit code 23*"
    }
    if (-not $failed) {
        throw "Expected the fixed host runner to propagate test exit code 23"
    }

    Write-Output "fixed test host runner tests passed"
} finally {
    if (Test-Path -LiteralPath $temporaryRoot) {
        Remove-Item -LiteralPath $temporaryRoot -Recurse -Force
    }
}
