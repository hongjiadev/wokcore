function Get-WokCoreTargetContracts {
    [CmdletBinding()]
    param([Parameter(Mandatory)][string] $Version)

    $rows = @(
        @("x86_64-pc-windows-msvc", "Windows", "x86_64", "wokcore.exe", "zip", $true),
        @("aarch64-pc-windows-msvc", "Windows", "arm64", "wokcore.exe", "zip", $false),
        @("x86_64-apple-darwin", "macOS", "x86_64", "wokcore", "tar.gz", $true),
        @("aarch64-apple-darwin", "macOS", "arm64", "wokcore", "tar.gz", $true),
        @("x86_64-unknown-linux-gnu", "Linux", "x86_64", "wokcore", "tar.gz", $true),
        @("aarch64-unknown-linux-gnu", "Linux", "arm64", "wokcore", "tar.gz", $true)
    )
    foreach ($row in $rows) {
        $target, $system, $architecture, $executable, $extension, $legacyV1 = $row
        $friendly = if ($system -eq "Windows") {
            "WokCore-v$Version-$system-$architecture-Portable.zip"
        } else {
            "WokCore-v$Version-$system-$architecture.$extension"
        }
        [pscustomobject]@{
            Target = $target
            System = $system
            Architecture = $architecture
            Executable = $executable
            PortableExtension = $extension
            FriendlyPortableName = $friendly
            LegacyV1Name = "wokcore-v$Version-$target.$extension"
            LegacyV1 = [bool] $legacyV1
        }
    }
}

function Get-WokCorePayloadNames {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string] $Version,
        [switch] $IncludeLegacyV1
    )
    $names = [Collections.Generic.List[string]]::new()
    foreach ($contract in Get-WokCoreTargetContracts -Version $Version) {
        $names.Add($contract.FriendlyPortableName)
        if ($contract.System -eq "Linux") {
            $names.Add("WokCore-v$Version-Linux-$($contract.Architecture).deb")
            $names.Add("WokCore-v$Version-Linux-$($contract.Architecture).rpm")
        } elseif ($contract.System -eq "macOS") {
            $names.Add("WokCore-v$Version-macOS-$($contract.Architecture).zip")
        } elseif ($contract.System -eq "Windows") {
            $names.Add("WokCore-v$Version-Windows-$($contract.Architecture).msi")
        }
        if ($IncludeLegacyV1 -and $contract.LegacyV1) {
            $names.Add($contract.LegacyV1Name)
        }
    }
    return @($names | Sort-Object -CaseSensitive)
}

Export-ModuleMember -Function Get-WokCoreTargetContracts, Get-WokCorePayloadNames
