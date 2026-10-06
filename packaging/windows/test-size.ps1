# 无需 Rust/Inno Setup：真实文件长度验证边界、产品线及配置错误。
$ErrorActionPreference = 'Stop'
$fixtureDir = Join-Path ([IO.Path]::GetTempPath()) ('gouhuo-size-' + [guid]::NewGuid())
New-Item -ItemType Directory -Path $fixtureDir | Out-Null
$installer = Join-Path $fixtureDir 'setup.exe'
$limits = Join-Path $fixtureDir 'redline.rs'
$checker = Join-Path $PSScriptRoot 'check-size.ps1'
function Set-Size([long]$Bytes) {
    $stream = [IO.File]::Open($installer, [IO.FileMode]::Create)
    try { $stream.SetLength($Bytes) } finally { $stream.Dispose() }
}
function Expect-Failure([string]$Message, [string]$Config) {
    $failed = $false
    try {
        & $checker -InstallerPath $installer -RedlinePath $Config
    } catch {
        if ($_.Exception.Message -notlike "*$Message*") { throw }
        $failed = $true
    }
    if (-not $failed) { throw "预期失败：$Message" }
}
try {
    $realLimits = Join-Path $PSScriptRoot '..\..\crates\voice-core\src\redline.rs'
    Set-Size 19999999
    & $checker -InstallerPath $installer
    Set-Size 20000000
    Expect-Failure '防回归闸' $realLimits
    Set-Size 60000000
    Expect-Failure '产品线' $realLimits
    Set-Size 0
    Expect-Failure '非空文件' $realLimits
    Set-Content -LiteralPath $limits -Value 'pub const INSTALLER_MB: f64 = 60.0;'
    Expect-Failure 'GATE_INSTALLER_MB' $limits
    Set-Content -LiteralPath $limits -Value @(
        'pub const INSTALLER_MB: f64 = 60.0;'
        'pub const GATE_INSTALLER_MB: f64 = 60.0;'
        'pub const MEASURED_INSTALLER_MB: f64 = 8.023011;'
    )
    Expect-Failure '阈值无效' $limits
    Write-Host '安装包体积门禁：6 项验证通过'
} finally {
    foreach ($fixture in @($installer, $limits)) {
        if (Test-Path -LiteralPath $fixture) { Remove-Item -LiteralPath $fixture }
    }
    Remove-Item -LiteralPath $fixtureDir
}
