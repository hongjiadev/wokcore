[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string] $ArtifactDirectory,
    [Parameter(Mandatory)]
    [string[]] $ExpectedNames,
    [Parameter(Mandatory)]
    [string] $OutputPath
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$ArtifactDirectory = [IO.Path]::GetFullPath($ArtifactDirectory)
$OutputPath = [IO.Path]::GetFullPath($OutputPath)
if (-not [IO.Directory]::Exists($ArtifactDirectory)) {
    throw "Artifact directory does not exist."
}
if (
    [IO.Path]::GetDirectoryName($OutputPath) -cne $ArtifactDirectory -or
    [IO.Path]::GetFileName($OutputPath) -cne "SHA256SUMS"
) {
    throw "Checksums must use the fixed name in the artifact directory."
}
if ($ExpectedNames.Count -eq 0) {
    throw "At least one checksum input is required."
}

$seen = [Collections.Generic.HashSet[string]]::new(
    [StringComparer]::Ordinal
)
$names = [string[]] @($ExpectedNames)
foreach ($name in $names) {
    if (
        [string]::IsNullOrWhiteSpace($name) -or
        $name -cne [IO.Path]::GetFileName($name) -or
        $name -ceq "SHA256SUMS" -or
        -not $seen.Add($name)
    ) {
        throw "Checksum inputs must be unique fixed file names."
    }
}
[Array]::Sort($names, [StringComparer]::Ordinal)

$lines = foreach ($name in $names) {
    $path = Join-Path $ArtifactDirectory $name
    if (-not [IO.File]::Exists($path)) {
        throw "Checksum input is missing: $name"
    }
    $item = Get-Item -LiteralPath $path
    if (
        ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or
        $item.Length -le 0 -or
        $item.Length -gt 536870912
    ) {
        throw "Checksum input is symbolic, empty, or oversized: $name"
    }
    $hash = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
    "$hash  $name"
}

$temporary = Join-Path $ArtifactDirectory (
    ".SHA256SUMS." + [Guid]::NewGuid().ToString("N") + ".tmp"
)
try {
    [IO.File]::WriteAllText(
        $temporary,
        ([string]::Join("`n", $lines) + "`n"),
        [Text.UTF8Encoding]::new($false)
    )
    if ([IO.File]::Exists($OutputPath)) {
        [IO.File]::Delete($OutputPath)
    }
    [IO.File]::Move($temporary, $OutputPath)
} finally {
    if ([IO.File]::Exists($temporary)) {
        [IO.File]::Delete($temporary)
    }
}

Write-Output $OutputPath
