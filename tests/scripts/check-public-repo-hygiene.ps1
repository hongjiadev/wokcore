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

$usingRepositoryIndex = $null -eq $IndexLines
if ($usingRepositoryIndex) {
    $IndexLines = @(& git -C $RepositoryRoot ls-files --stage)
    if ($LASTEXITCODE -ne 0) {
        throw "git ls-files failed for $RepositoryRoot"
    }
}
$contentViolationPaths = [Collections.Generic.HashSet[string]]::new(
    [StringComparer]::OrdinalIgnoreCase
)
if ($usingRepositoryIndex) {
    $privateKeyComment = "untrusted comment: minisign encrypted " + "secret key"
    $pemPrivateKey = "-----BEGIN (OPENSSH |RSA |EC |DSA |ENCRYPTED )?PRIVATE " +
        "KEY-----"
    $privateDocsLink = "(^|/|\\|\()wok" + "docs(/|\\|$)"
    $contentPattern = "$privateKeyComment|$pemPrivateKey|$privateDocsLink"
    $markerPaths = @(
        & git -C $RepositoryRoot grep --cached -I -i -l -E $contentPattern 2>$null
    )
    $grepExitCode = $LASTEXITCODE
    if ($grepExitCode -eq 1) {
        $markerPaths = @()
    } elseif ($grepExitCode -ne 0) {
        throw "git grep failed for $RepositoryRoot"
    }
    foreach ($path in $markerPaths) {
        if (-not [string]::IsNullOrWhiteSpace($path)) {
            $contentViolationPaths.Add($path) | Out-Null
        }
    }

    $secretKeyCandidates = @(
        & git -C $RepositoryRoot grep --cached -I -n -E `
            "^[A-Za-z0-9+/]{208,216}={0,2}$" 2>$null
    )
    $grepExitCode = $LASTEXITCODE
    if ($grepExitCode -eq 1) {
        $secretKeyCandidates = @()
    } elseif ($grepExitCode -ne 0) {
        throw "git grep failed while scanning Minisign key payloads."
    }
    foreach ($candidate in $secretKeyCandidates) {
        if (
            $candidate -notmatch
                "^(?<path>.*):[0-9]+:(?<payload>[A-Za-z0-9+/]{208,216}={0,2})$"
        ) {
            throw "Unrecognized git grep candidate shape."
        }
        try {
            [byte[]] $decoded = [Convert]::FromBase64String($Matches.payload)
        } catch {
            continue
        }
        if (
            $decoded.Length -eq 158 -and
            $decoded[0] -eq 0x45 -and
            $decoded[1] -in @(0x44, 0x64)
        ) {
            $contentViolationPaths.Add($Matches.path) | Out-Null
        }
    }
}

$privateWorkflowMarkers = @(
    "ai", "codex", "claude", "cursor", "superpowers", "subpowers", "wokdocs"
)
$privateWorkflowArtifacts = @(
    "review", "reviews", "progress", "handoff", "handoffs",
    "plan", "plans", "spec", "specs", "workflow", "workflows"
)
$allowedUpdateFixturePaths = [Collections.Generic.HashSet[string]]::new(
    [StringComparer]::Ordinal
)
foreach ($path in @(
        "crates/wokcore-platform/tests/fixtures/update/minisign.pub",
        "crates/wokcore-platform/tests/fixtures/update/wokcore-update-v1.json",
        "crates/wokcore-platform/tests/fixtures/update/wokcore-update-v1.json.minisig",
        "crates/wokcore-platform/tests/fixtures/update/install-minisign.pub",
        "crates/wokcore-platform/tests/fixtures/update/install-wokcore-update-v1.json",
        "crates/wokcore-platform/tests/fixtures/update/install-wokcore-update-v1.json.minisig"
    )) {
    $allowedUpdateFixturePaths.Add($path) | Out-Null
}

$violations = foreach ($line in $IndexLines) {
    if ($line -notmatch "^(?<mode>\d{6}) [0-9a-f]{40} \d+\t(?<path>.+)$") {
        throw "Unrecognized git index line: $line"
    }

    $mode = $Matches.mode
    $path = $Matches.path.Replace("\", "/")
    $lowerPath = $path.ToLowerInvariant()
    $isAllowedUpdateFixture = $allowedUpdateFixturePaths.Contains($path)
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
    $isSignedReleasePayload =
        $lowerPath -match
            "\.(key|sec|secret|private|pem|p12|pfx|zip|tgz|tar\.gz|minisig|exe|dll|dylib|so)$" -or
        $lowerPath -match
            "(^|/)(sha256sums|wokcore-update-v1\.json|wokcore-update-v1\.json\.minisig)$" -or
        $lowerPath -match
            "(^|/)wokcore-v[0-9][0-9a-z.+-]*-(x86_64|aarch64)-[^/]+\.(zip|tar\.gz)$" -or
        $lowerPath -in @(
            "release/wokcore",
            "wokcore",
            "wokcore.exe"
        )
    if (
        $mode -eq "120000" -or
        $path -match "(^|/)docs/superpowers(/|$)" -or
        $path -match "(^|/)\.superpowers(/|$)" -or
        $path -match "(^|/)\.subpowers(/|$)" -or
        $path -match "(^|/)\.wokdocs(/|$)" -or
        $lowerPath -match "(^|/)(target|dist|artifacts?)(/|$)" -or
        ($isSignedReleasePayload -and -not $isAllowedUpdateFixture) -or
        $hasPrivateWorkflowName
    ) {
        $line
    }
}

if (
    @($violations).Count -gt 0 -or
    $contentViolationPaths.Count -gt 0
) {
    $contentPaths = @($contentViolationPaths | Sort-Object)
    throw (
        "Public repository contains private workflow artifacts, generated " +
        "binaries, secret material, private-document links, or symbolic links." +
        "`nIndex entries:`n$($violations -join "`n")" +
        "`nContent paths:`n$($contentPaths -join "`n")"
    )
}

Write-Output "public repository hygiene check passed"
