[CmdletBinding()]
param(
    [string] $ArtifactManifestPath,

    [Parameter(Mandatory = $true)]
    [string] $TargetDirectory,

    [string] $RepositoryRoot,

    [string] $CargoCommand = "cargo",

    [string] $Toolchain = "+1.97.1",

    [string] $Target,

    [switch] $Offline,

    [string[]] $HarnessArguments = @()
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

if (-not (Test-Path -LiteralPath $TargetDirectory -PathType Container)) {
    New-Item -ItemType Directory -Path $TargetDirectory | Out-Null
}
$targetPath = (Resolve-Path -LiteralPath $TargetDirectory).Path
$fixedHost = Join-Path $targetPath "wokcore-test-host.exe"

if ([string]::IsNullOrWhiteSpace($ArtifactManifestPath)) {
    if ([string]::IsNullOrWhiteSpace($RepositoryRoot)) {
        $RepositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
    } else {
        $RepositoryRoot = (Resolve-Path -LiteralPath $RepositoryRoot).Path
    }
    $ArtifactManifestPath = Join-Path $targetPath "wokcore-test-artifacts.jsonl"
    $cargoArguments = @(
        $Toolchain,
        "test",
        "--workspace",
        "--all-features",
        "--locked",
        "--no-run",
        "--message-format=json-render-diagnostics",
        "--target-dir",
        $targetPath
    )
    if ($Offline) {
        $cargoArguments += "--offline"
    }
    if (-not [string]::IsNullOrWhiteSpace($Target)) {
        $cargoArguments += @("--target", $Target)
    }

    $utf8WithoutBom = New-Object System.Text.UTF8Encoding($false)
    $manifestWriter = New-Object System.IO.StreamWriter(
        $ArtifactManifestPath,
        $false,
        $utf8WithoutBom
    )
    try {
        Push-Location $RepositoryRoot
        try {
            $previousErrorActionPreference = $ErrorActionPreference
            $ErrorActionPreference = "Continue"
            & $CargoCommand @cargoArguments 2>&1 | ForEach-Object {
                $line = $_.ToString()
                $manifestWriter.WriteLine($line)
                $message = $null
                try {
                    $message = $line | ConvertFrom-Json
                } catch {
                    $message = $null
                }
                if ($null -eq $message) {
                    Write-Host $line
                } elseif (
                    $null -ne $message.PSObject.Properties["message"] -and
                    $null -ne $message.message.PSObject.Properties["rendered"] -and
                    -not [string]::IsNullOrWhiteSpace([string] $message.message.rendered)
                ) {
                    Write-Host $message.message.rendered -NoNewline
                }
            }
            $cargoExitCode = $LASTEXITCODE
        } finally {
            $ErrorActionPreference = $previousErrorActionPreference
            Pop-Location
        }
    } finally {
        $manifestWriter.Dispose()
    }
    if ($cargoExitCode -ne 0) {
        throw "Cargo test compilation failed with exit code $cargoExitCode"
    }
}

$manifestPath = (Resolve-Path -LiteralPath $ArtifactManifestPath).Path
$artifacts = @(
    Get-Content -LiteralPath $manifestPath | ForEach-Object {
        try {
            $record = $_ | ConvertFrom-Json
        } catch {
            return
        }
        if (
            $record.reason -eq "compiler-artifact" -and
            $record.profile.test -eq $true -and
            -not [string]::IsNullOrWhiteSpace([string] $record.executable)
        ) {
            [PSCustomObject] @{
                Name = [string] $record.target.name
                Executable = [string] $record.executable
            }
        }
    } | Sort-Object Executable -Unique
)

if ($artifacts.Count -eq 0) {
    throw "Cargo artifact manifest contains no test executables: $manifestPath"
}

foreach ($artifact in $artifacts) {
    if (-not (Test-Path -LiteralPath $artifact.Executable -PathType Leaf)) {
        throw "Test executable is missing: $($artifact.Executable)"
    }
    Copy-Item -LiteralPath $artifact.Executable -Destination $fixedHost -Force
    Write-Output "running $($artifact.Name) through $fixedHost"
    & $fixedHost @HarnessArguments
    if ($LASTEXITCODE -ne 0) {
        throw "Test executable $($artifact.Name) failed with exit code $LASTEXITCODE"
    }
}
