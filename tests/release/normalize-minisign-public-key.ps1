[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string] $Path
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$fullPath = [IO.Path]::GetFullPath($Path)
if (-not [IO.File]::Exists($fullPath)) {
    throw "Minisign public key is missing: $fullPath"
}
$bytes = [IO.File]::ReadAllBytes($fullPath)
if (
    $bytes.Length -ge 3 -and
    $bytes[0] -eq 0xef -and
    $bytes[1] -eq 0xbb -and
    $bytes[2] -eq 0xbf
) {
    throw "Minisign public key must be UTF-8 without a byte-order mark."
}
$text = [Text.UTF8Encoding]::new($false, $true).GetString($bytes)
$normalized = $text.Replace("`r`n", "`n").TrimEnd("`n")
$lines = @($normalized.Split("`n"))
if ($lines.Count -ne 2) {
    throw "Minisign public key text has an invalid shape."
}
$commentMatch = [regex]::Match(
    $lines[0],
    "^untrusted comment: minisign public key ([0-9A-F]{1,16})$"
)
if (-not $commentMatch.Success) {
    throw "Minisign public key comment has an invalid key id."
}
$commentKeyId = $commentMatch.Groups[1].Value
try {
    [byte[]] $decoded = [Convert]::FromBase64String($lines[1])
} catch {
    throw "Minisign public key payload is not valid base64."
}
if (
    $decoded.Length -ne 42 -or
    $decoded[0] -ne 0x45 -or
    $decoded[1] -ne 0x64
) {
    throw "Minisign public key payload is not an Ed25519 key."
}
[byte[]] $keyIdBytes = $decoded[2..9]
[Array]::Reverse($keyIdBytes)
$payloadKeyId = [BitConverter]::ToString($keyIdBytes).Replace("-", "")
$prefixLength = $payloadKeyId.Length - $commentKeyId.Length
if (
    $prefixLength -lt 0 -or
    -not $payloadKeyId.EndsWith(
        $commentKeyId,
        [StringComparison]::Ordinal
    ) -or
    $payloadKeyId.Substring(0, $prefixLength) -cnotmatch "^0*$"
) {
    throw "Minisign public key comment does not match its key payload."
}
if ($commentKeyId -cne $payloadKeyId) {
    [IO.File]::WriteAllText(
        $fullPath,
        (
            "untrusted comment: minisign public key $payloadKeyId`n" +
            $lines[1] +
            "`n"
        ),
        [Text.UTF8Encoding]::new($false)
    )
}

Write-Output $payloadKeyId
