# 检查完整安装包，阈值统一读取 redline.rs；MB 使用十进制。
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$InstallerPath,
    [string]$RedlinePath = (Join-Path $PSScriptRoot '..\..\crates\voice-core\src\redline.rs')
)

$ErrorActionPreference = 'Stop'
$source = Get-Content -LiteralPath $RedlinePath -Raw
function Read-Limit([string]$Name) {
    $pattern = '(?m)^pub const ' + [regex]::Escape($Name) + ': f64 = ([0-9]+(?:\.[0-9]+)?);\s*$'
    $matchesFound = [regex]::Matches($source, $pattern)
    if ($matchesFound.Count -ne 1) { throw "redline.rs 中找不到唯一的数值常量 $Name" }
    return [double]::Parse($matchesFound[0].Groups[1].Value, [Globalization.CultureInfo]::InvariantCulture)
}

$productMB = Read-Limit 'INSTALLER_MB'
$gateMB = Read-Limit 'GATE_INSTALLER_MB'
$measuredMB = Read-Limit 'MEASURED_INSTALLER_MB'
if ($measuredMB -le 0 -or $measuredMB -ge $gateMB -or $gateMB -ge $productMB) {
    throw '安装包阈值无效：必须满足 0 < 实测 < 回归闸 < 产品线'
}
$installer = Get-Item -LiteralPath $InstallerPath
if ($installer.PSIsContainer -or $installer.Length -le 0) { throw '安装包必须是非空文件' }
$bytes = $installer.Length
$mb = $bytes / 1000000.0
Write-Host ("{0}: {1} bytes ({2:F6} MB), gate < {3} MB, product < {4} MB" -f $installer.FullName, $bytes, $mb, $gateMB, $productMB)
if ($bytes -ge $productMB * 1000000.0) { throw "安装包超过产品线：必须小于 $productMB MB" }
if ($bytes -ge $gateMB * 1000000.0) { throw "安装包超过防回归闸：必须小于 $gateMB MB" }
