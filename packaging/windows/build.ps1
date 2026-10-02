# 编一个 Windows 安装包：target\installer\gouhuo-setup-<版本>.exe
#
# 用法：
#   .\packaging\windows\build.ps1              # 先编 dist 版的 gouhuo.exe，再打包
#   .\packaging\windows\build.ps1 -SkipBuild   # exe 已经编好了，只打包
#
# 要装 Inno Setup 6：winget install JRSoftware.InnoSetup
# 版本号从 Cargo.toml 的 [workspace.package] 里读，不在这里另写一份。

[CmdletBinding()]
param(
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
Set-Location $root

# ---- 版本号 ----
$inWorkspace = $false
$version = $null
foreach ($line in Get-Content Cargo.toml) {
    if ($line -match '^\[') { $inWorkspace = ($line -eq '[workspace.package]') }
    if ($inWorkspace -and $line -match '^version\s*=\s*"([^"]+)"') { $version = $Matches[1]; break }
}
if (-not $version) { throw "Cargo.toml 的 [workspace.package] 里找不到 version" }

# ---- ISCC ----
$iscc = (Get-Command ISCC.exe -ErrorAction SilentlyContinue).Source
if (-not $iscc) {
    $candidates = @(
        "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
    )
    $iscc = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $iscc) { throw "找不到 Inno Setup 6 的 ISCC.exe。装一下：winget install JRSoftware.InnoSetup" }

# ---- exe ----
if (-not $SkipBuild) {
    # cargo 的进度写在 stderr 上；PowerShell 5.1 在 Stop 模式下会把它当成错误。
    $ErrorActionPreference = "Continue"
    cargo build --profile dist -p client
    $ErrorActionPreference = "Stop"
    if ($LASTEXITCODE -ne 0) { throw "cargo build 失败" }
}
$exe = Join-Path $root "target\dist\gouhuo.exe"
if (-not (Test-Path $exe)) { throw "没有 $exe —— 去掉 -SkipBuild 再跑一次" }

# ---- 打包 ----
& $iscc /Qp "/DAppVersion=$version" "/DExePath=$exe" "/DOutputDir=$(Join-Path $root 'target\installer')" `
    (Join-Path $PSScriptRoot "gouhuo.iss")
if ($LASTEXITCODE -ne 0) { throw "ISCC 失败" }

$setup = Join-Path $root "target\installer\gouhuo-setup-$version.exe"
& (Join-Path $PSScriptRoot 'check-size.ps1') -InstallerPath $setup
