[CmdletBinding()]
param(
    [string] $RepositoryRoot,
    [string[]] $IndexLines
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

if ([string]::IsNullOrWhiteSpace($RepositoryRoot)) {
    $RepositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
}

if ($null -eq $IndexLines) {
    $IndexLines = @(& git -C $RepositoryRoot ls-files --stage)
    if ($LASTEXITCODE -ne 0) {
        throw "git ls-files failed for $RepositoryRoot"
    }
}

$privateWorkflowMarkers = @(
    "ai", "codex", "claude", "cursor", "superpowers", "subpowers", "wokdocs"
)
$privateWorkflowArtifacts = @(
    "review", "reviews", "progress", "handoff", "handoffs",
    "plan", "plans", "spec", "specs", "workflow", "workflows"
)

$violations = foreach ($line in $IndexLines) {
    if ($line -notmatch "^(?<mode>\d{6}) [0-9a-f]{40} \d+\t(?<path>.+)$") {
        throw "Unrecognized git index line: $line"
    }

    $mode = $Matches.mode
    $path = $Matches.path.Replace("\", "/")
    $hasPrivateWorkflowName = $false
    foreach ($segment in $path.Split("/")) {
        $tokens = @(($segment.ToLowerInvariant() -split "[-_.]+") | Where-Object { $_ })
        $hasMarker = @(
            $tokens | Where-Object { $privateWorkflowMarkers -contains $_ }
        ).Count -gt 0
        $hasArtifact = @(
            $tokens | Where-Object { $privateWorkflowArtifacts -contains $_ }
        ).Count -gt 0
        if ($hasMarker -and $hasArtifact) {
            $hasPrivateWorkflowName = $true
            break
        }
    }
    if (
        $mode -eq "120000" -or
        $path -match "(^|/)docs/superpowers(/|$)" -or
        $path -match "(^|/)\.superpowers(/|$)" -or
        $path -match "(^|/)\.subpowers(/|$)" -or
        $path -match "(^|/)\.wokdocs(/|$)" -or
        $hasPrivateWorkflowName
    ) {
        $line
    }
}

if (@($violations).Count -gt 0) {
    throw "Public repository contains private workflow artifacts or symbolic links:`n$($violations -join "`n")"
}

Write-Output "public repository hygiene check passed"
