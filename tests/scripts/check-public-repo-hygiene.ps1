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

$violations = foreach ($line in $IndexLines) {
    if ($line -notmatch "^(?<mode>\d{6}) [0-9a-f]{40} \d+\t(?<path>.+)$") {
        throw "Unrecognized git index line: $line"
    }

    $mode = $Matches.mode
    $path = $Matches.path.Replace("\", "/")
    if (
        $path -match "(^|/)docs/superpowers(/|$)" -or
        $path -match "(^|/)\.superpowers(/|$)" -or
        $path -match "(^|/)\.subpowers(/|$)" -or
        $path -match "(^|/)\.wokdocs(/|$)" -or
        ($mode -eq "120000" -and $path -eq "docs/superpowers")
    ) {
        $line
    }
}

if (@($violations).Count -gt 0) {
    throw "Public repository contains private workflow artifacts:`n$($violations -join "`n")"
}

Write-Output "public repository hygiene check passed"
