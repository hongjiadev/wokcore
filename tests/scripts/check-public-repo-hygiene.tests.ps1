$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$checker = Join-Path $PSScriptRoot "check-public-repo-hygiene.ps1"
if (-not (Test-Path -LiteralPath $checker)) {
    throw "Missing hygiene checker: $checker"
}

$clean = @(
    "100644 0000000000000000000000000000000000000000 0`tREADME.md",
    "100644 0000000000000000000000000000000000000000 0`tdocs/architecture.md",
    "100644 0000000000000000000000000000000000000000 0`tdocs/api-spec.md",
    "100644 0000000000000000000000000000000000000000 0`t.github/workflows/ci.yml",
    "100644 0000000000000000000000000000000000000000 0`tnotes/daily-progress.md",
    "100644 0000000000000000000000000000000000000000 0`trelease/minisign.pub",
    "100644 0000000000000000000000000000000000000000 0`tcrates/wokcore-platform/tests/fixtures/update/minisign.pub",
    "100644 0000000000000000000000000000000000000000 0`tcrates/wokcore-platform/tests/fixtures/update/wokcore-update-v1.json",
    "100644 0000000000000000000000000000000000000000 0`tcrates/wokcore-platform/tests/fixtures/update/wokcore-update-v1.json.minisig",
    "100644 0000000000000000000000000000000000000000 0`tcrates/wokcore-platform/tests/fixtures/update/install-minisign.pub",
    "100644 0000000000000000000000000000000000000000 0`tcrates/wokcore-platform/tests/fixtures/update/install-wokcore-update-v1.json",
    "100644 0000000000000000000000000000000000000000 0`tcrates/wokcore-platform/tests/fixtures/update/install-wokcore-update-v1.json.minisig",
    "100644 0000000000000000000000000000000000000000 0`tapps/wokcore/tests/fixtures/update/migration-minisign.pub",
    "100644 0000000000000000000000000000000000000000 0`tapps/wokcore/tests/fixtures/update/migration-wokcore-update-v1.json",
    "100644 0000000000000000000000000000000000000000 0`tapps/wokcore/tests/fixtures/update/migration-wokcore-update-v1.json.minisig",
    "100644 0000000000000000000000000000000000000000 0`tapps/wokcore/tests/fixtures/update/migration-wokcore-update-v2.json",
    "100644 0000000000000000000000000000000000000000 0`tapps/wokcore/tests/fixtures/update/migration-wokcore-update-v2.json.minisig"
)
& $checker -IndexLines $clean

foreach ($forbidden in @(
    "100644 0000000000000000000000000000000000000000 0`tdocs/superpowers/plan.md",
    "100644 0000000000000000000000000000000000000000 0`t.superpowers/review.md",
    "100644 0000000000000000000000000000000000000000 0`t.subpowers/sessions/active.md",
    "120000 0000000000000000000000000000000000000000 0`tdocs/superpowers",
    "120000 0000000000000000000000000000000000000000 0`tinternal-docs",
    "100644 0000000000000000000000000000000000000000 0`tnotes/ai-review.md",
    "100644 0000000000000000000000000000000000000000 0`tnotes/ai-progress.md",
    "100644 0000000000000000000000000000000000000000 0`tnotes/codex-handoff.md",
    "100644 0000000000000000000000000000000000000000 0`tnotes/plan-claude.md",
    "100644 0000000000000000000000000000000000000000 0`tnotes/ai-private-review.md",
    "100644 0000000000000000000000000000000000000000 0`tnotes/review-generated-by-codex.md",
    "100644 0000000000000000000000000000000000000000 0`tnotes/CLAUDE_internal_PROGRESS.txt",
    "100644 0000000000000000000000000000000000000000 0`trelease/minisign.key",
    "100644 0000000000000000000000000000000000000000 0`trelease/wokcore-update-v1.json.minisig",
    "100644 0000000000000000000000000000000000000000 0`trelease/wokcore-update-v1.json",
    "100644 0000000000000000000000000000000000000000 0`trelease/SHA256SUMS",
    "100644 0000000000000000000000000000000000000000 0`trelease/WokCore-v1.2.3-Windows-x86_64.msi",
    "100644 0000000000000000000000000000000000000000 0`trelease/WokCore-v1.2.3-Linux-x86_64.deb",
    "100644 0000000000000000000000000000000000000000 0`trelease/WokCore-v1.2.3-Linux-x86_64.rpm",
    "100644 0000000000000000000000000000000000000000 0`trelease/WokCore-v1.2.3-Linux-x86_64.AppImage",
    "100644 0000000000000000000000000000000000000000 0`trelease/WokCore-v1.2.3-macOS-x86_64.dmg",
    "100644 0000000000000000000000000000000000000000 0`trelease/wokcore-v1.2.3-x86_64-pc-windows-msvc.zip",
    "100644 0000000000000000000000000000000000000000 0`trelease/wokcore-v1.2.3-aarch64-apple-darwin.tar.gz",
    "100644 0000000000000000000000000000000000000000 0`tkeys/minisign.key",
    "100644 0000000000000000000000000000000000000000 0`trelease/private.pem",
    "100644 0000000000000000000000000000000000000000 0`tdist/SHA256SUMS",
    "100644 0000000000000000000000000000000000000000 0`twokcore-v1.2.3-x86_64-pc-windows-msvc.zip",
    "100644 0000000000000000000000000000000000000000 0`tcrates/wokcore-platform/tests/fixtures/update/unapproved.minisig",
    "100644 0000000000000000000000000000000000000000 0`tapps/wokcore/tests/fixtures/update/wokcore-update-v2.json",
    "120000 0000000000000000000000000000000000000000 0`tcrates/wokcore-platform/tests/fixtures/update/wokcore-update-v1.json",
    "100644 0000000000000000000000000000000000000000 0`tcrates/WokCore-platform/tests/fixtures/update/wokcore-update-v1.json"
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

function Remove-TestRepository {
    param(
        [Parameter(Mandatory)]
        [string] $Path
    )

    if (-not [IO.Directory]::Exists($Path)) {
        return
    }
    foreach ($file in [IO.Directory]::EnumerateFiles(
        $Path,
        "*",
        [IO.SearchOption]::AllDirectories
    )) {
        [IO.File]::SetAttributes($file, [IO.FileAttributes]::Normal)
    }
    [IO.Directory]::Delete($Path, $true)
}

function Assert-StagedContentRejected {
    param(
        [Parameter(Mandatory)]
        [string] $RelativePath,
        [Parameter(Mandatory)]
        [string] $Content
    )

    $fixture = Join-Path ([IO.Path]::GetTempPath()) (
        "wokcore-hygiene-" + [Guid]::NewGuid().ToString("N")
    )
    [IO.Directory]::CreateDirectory($fixture) | Out-Null
    try {
        & git -C $fixture init --quiet
        if ($LASTEXITCODE -ne 0) {
            throw "Failed to initialize the hygiene fixture repository."
        }
        $path = Join-Path $fixture $RelativePath
        [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($path)) |
            Out-Null
        [IO.File]::WriteAllText(
            $path,
            $Content,
            [Text.UTF8Encoding]::new($false)
        )
        & git -C $fixture add -- $RelativePath
        if ($LASTEXITCODE -ne 0) {
            throw "Failed to stage the hygiene fixture."
        }
        $failed = $false
        try {
            & $checker -RepositoryRoot $fixture
        } catch {
            $failed = $true
        }
        if (-not $failed) {
            throw "Expected forbidden staged content to fail."
        }
    } finally {
        Remove-TestRepository -Path $fixture
    }
}

Assert-StagedContentRejected `
    -RelativePath "keys/minisign.txt" `
    -Content (
        "untrusted comment: minisign encrypted " +
        "secret key`n"
    )
Assert-StagedContentRejected `
    -RelativePath "keys/hidden.txt" `
    -Content (
        "ordinary comment`n" +
        [Convert]::ToBase64String(
            (& {
                $bytes = [byte[]]::new(158)
                $bytes[0] = 0x45
                $bytes[1] = 0x64
                $bytes
            })
        ) +
        "`n"
    )
Assert-StagedContentRejected `
    -RelativePath "notes/fixture.txt" `
    -Content ("-----BEGIN OPENSSH PRIVATE " + "KEY-----`n")
Assert-StagedContentRejected `
    -RelativePath "README.md" `
    -Content ("[private](WoK" + "DoCs/index.md)`n")
Assert-StagedContentRejected `
    -RelativePath "notes/windows-link.txt" `
    -Content ("[private](WoK" + "DoCs\index.md)`n")

foreach ($allowedContent in @(
    "untrusted comment: minisign public key 0000000000000000`n",
    "https://github.com/hongjiadev/wokcore`n"
)) {
    $failed = $false
    try {
        $fixture = Join-Path ([IO.Path]::GetTempPath()) (
            "wokcore-hygiene-allowed-" + [Guid]::NewGuid().ToString("N")
        )
        [IO.Directory]::CreateDirectory($fixture) | Out-Null
        & git -C $fixture init --quiet
        [IO.File]::WriteAllText(
            (Join-Path $fixture "allowed.txt"),
            $allowedContent,
            [Text.UTF8Encoding]::new($false)
        )
        & git -C $fixture add -- "allowed.txt"
        & $checker -RepositoryRoot $fixture
    } catch {
        $failed = $true
    } finally {
        Remove-TestRepository -Path $fixture
    }
    if ($failed) {
        throw "Allowed staged content was rejected."
    }
}

& $checker

Write-Output "public repository hygiene tests passed"
