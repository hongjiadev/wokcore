$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$checker = Join-Path $PSScriptRoot "check-public-repo-hygiene.ps1"
if (-not (Test-Path -LiteralPath $checker)) {
    throw "Missing hygiene checker: $checker"
}

$clean = @(
    "100644 0000000000000000000000000000000000000000 0`tREADME.md",
    "100644 0000000000000000000000000000000000000000 0`tdocs/architecture.md"
)
& $checker -IndexLines $clean

foreach ($forbidden in @(
    "100644 0000000000000000000000000000000000000000 0`tdocs/superpowers/plan.md",
    "100644 0000000000000000000000000000000000000000 0`t.superpowers/review.md",
    "100644 0000000000000000000000000000000000000000 0`t.subpowers/sessions/active.md",
    "120000 0000000000000000000000000000000000000000 0`tdocs/superpowers"
)) {
    $failed = $false
    try {
        & $checker -IndexLines @($forbidden)
    } catch {
        $failed = $true
    }
    if (-not $failed) {
        throw "Expected forbidden index entry to fail: $forbidden"
    }
}

& $checker

Write-Output "public repository hygiene tests passed"
