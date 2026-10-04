# 量客户端在各种状态下的开销：内存、CPU、唤醒次数（#11）。
#
# 用法（先编好：cargo build --profile dist -p client -p server
#             cargo build --profile dist -p voice-engine
#             cargo build --release -p client-core --example talker）：
#
#   .\scripts\m6-footprint.ps1                  # 四个状态各 30 秒
#   .\scripts\m6-footprint.ps1 -Seconds 60
#   .\scripts\m6-footprint.ps1 -LongMinutes 120 # 收在托盘里挂两小时，每分钟一个点
#
# 频道里有一个「说话机器人」（client-core/examples/talker）一直在发声，客户端要真的在
# 解码、混音、过抖动缓冲。为了不让测试音从你的音箱里放出来，客户端设置里把机器人的
# 音量调成 0（单人音量）—— 解码和混音照常，只是乘个 0。
#
# **全程不碰 UI 自动化。** 一旦有 UI 自动化客户端来访问，界面的无障碍支持就被激活、
# 而且一直开着（多起线程、多占内存），量出来的就不是普通用户那个数了。
#
# 麦克风指示灯会亮：客户端在频道里要开着采集设备。它是语音激活模式，房间里安静的话
# 不会往外发声（发了也只发到这个本机的测试服务器）。
#
# 每个状态记四个数：
#   工作集     任务管理器默认显示的「内存」。Windows 会在最小化时裁剪它，只看这个会被骗
#   私有字节   真正占着的（提交大小）。最小化时通常不变，只是换到页面文件里去了
#   CPU        单核占比，跟 redline.rs 的口径一样
#   唤醒/秒    所有线程每秒被切上 CPU 的次数（上下文切换）。后台挂着时它决定了
#              会不会跟游戏抢调度

[CmdletBinding()]
param(
    [int]$Seconds = 30,
    [int]$LongMinutes = 0,
    [string]$Out = ""
)
$ErrorActionPreference = "Stop"
Add-Type @"
using System; using System.Runtime.InteropServices;
public class FP {
 [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
 [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
 [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr h);
 [DllImport("user32.dll")] public static extern bool ShowWindowAsync(IntPtr h, int cmd);
 public delegate bool P(IntPtr h, IntPtr l);
 [DllImport("user32.dll")] public static extern bool EnumWindows(P cb, IntPtr l);
 [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
 [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr h, System.Text.StringBuilder s, int n);
 // 篝火的主窗口是 winit 建的「Window Class」。不能用 Process.MainWindowHandle ——
 // 刚启动时它指向 winit 内部的「Winit Thread Event Target」。
 public static IntPtr SlintWindow(uint pid) { IntPtr found = IntPtr.Zero; EnumWindows((h,l)=>{ uint p; GetWindowThreadProcessId(h, out p); if (p==pid) { var c = new System.Text.StringBuilder(64); GetClassName(h, c, 64); if (c.ToString()=="Window Class") { found = h; return false; } } return true; }, IntPtr.Zero); return found; }
}
"@

$root = Split-Path -Parent $PSScriptRoot
$client = Join-Path $root "target\dist\gouhuo.exe"
$engine = Join-Path $root "target\dist\gouhuo-voice.exe"
$serverExe = Join-Path $root "target\dist\gouhuo-server.exe"
$talker = Join-Path $root "target\release\examples\talker.exe"
foreach ($f in @($client, $engine, $serverExe, $talker)) { if (-not (Test-Path $f)) { throw "找不到 $f，先按文件头的命令编一下" } }

$work = Join-Path $env:TEMP "gouhuo-footprint"
Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
New-Item -ItemType Directory "$work\server", "$work\client" | Out-Null
$port = 20894
$started = @()

function Stop-All { foreach ($p in $script:started) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue } }

function Start-Client([string[]]$argv) {
    $env:APPDATA = "$work\client"
    if ($argv.Count -gt 0) { $p = Start-Process -FilePath $client -ArgumentList $argv -PassThru }
    else { $p = Start-Process -FilePath $client -PassThru }
    $script:started += $p
    for ($i = 0; $i -lt 50; $i++) { if ([FP]::SlintWindow([uint32]$p.Id) -ne [IntPtr]::Zero) { break }; Start-Sleep -Milliseconds 200 }
    return $p
}

# 一个进程的所有线程被切上 CPU 的累计次数（原始计数器，不随系统语言变）。
function Context-Switches($p) {
    $sum = 0
    Get-CimInstance Win32_PerfRawData_PerfProc_Thread -Filter "IDProcess=$($p.Id)" | ForEach-Object { $sum += [int64]$_.ContextSwitchesPersec }
    return $sum
}

function Measure-State($p, [string]$label, [int]$secs) {
    # 固定这一轮的父子 PID，内核退役时测量失败，不能把进程重启当成开销下降。
    $children = @(Get-CimInstance Win32_Process -Filter "ParentProcessId=$($p.Id) AND Name='gouhuo-voice.exe'")
    if ($children.Count -ne 1) { throw "需要一个运行中的声音内核，实际为 $($children.Count) 个" }
    $processes = @($p, (Get-Process -Id $children[0].ProcessId))
    $ws = @(); $priv = @()
    $cpu0 = 0; $cs0 = 0
    foreach ($process in $processes) {
        $process.Refresh()
        $cpu0 += $process.TotalProcessorTime.TotalSeconds
        $cs0 += Context-Switches $process
    }
    $t0 = Get-Date
    for ($i = 0; $i -lt $secs; $i++) {
        Start-Sleep -Seconds 1
        $totalWs = 0; $totalPrivate = 0
        foreach ($process in $processes) {
            $process.Refresh()
            if ($process.HasExited) { throw "测量中进程退出：$($process.Id)" }
            $totalWs += $process.WorkingSet64
            $totalPrivate += $process.PrivateMemorySize64
        }
        $ws += $totalWs / 1MB
        $priv += $totalPrivate / 1MB
    }
    $cpu1 = 0; $cs1 = 0; $threads = 0
    foreach ($process in $processes) {
        $process.Refresh()
        if ($process.HasExited) { throw "测量中进程退出：$($process.Id)" }
        $cpu1 += $process.TotalProcessorTime.TotalSeconds
        $cs1 += Context-Switches $process
        $threads += $process.Threads.Count
    }
    $wall = ((Get-Date) - $t0).TotalSeconds
    $cpu = ($cpu1 - $cpu0) / $wall * 100
    $wake = ($cs1 - $cs0) / $wall
    [pscustomobject]@{
        状态 = $label
        工作集MB = "{0:N1}（最高 {1:N1}）" -f ($ws | Measure-Object -Average).Average, ($ws | Measure-Object -Maximum).Maximum
        私有MB = "{0:N1}（最高 {1:N1}）" -f ($priv | Measure-Object -Average).Average, ($priv | Measure-Object -Maximum).Maximum
        CPU = "{0:N2}%" -f $cpu
        唤醒每秒 = "{0:N0}" -f $wake
        线程 = $threads
    }
}

try {
    # ---- 服务端 ----
    $env:GOUHUO_DATA = "$work\server"; $env:GOUHUO_PORT = "$port"; $env:GOUHUO_HOST = "127.0.0.1"; $env:GOUHUO_INVITE = ""
    $srv = Start-Process -FilePath $serverExe -PassThru -RedirectStandardOutput "$work\server.log"
    $started += $srv
    Start-Sleep -Seconds 2
    $link = (Select-String -Path "$work\server.log" -Pattern 'gouhuo://j/[a-z0-9]+').Matches[0].Value

    # ---- 先跑一次让客户端生成身份：第一次跑不会自动连 ----
    $p = Start-Client @()
    Start-Sleep -Seconds 2
    Stop-Process -Id $p.Id -Force; Start-Sleep -Seconds 1
    # 点 × 收到托盘，别弹问话
    Add-Content "$work\client\gouhuo\settings.txt" "close=tray"

    $rows = @()

    # ---- 1. 登录页空闲 ----
    $p = Start-Client @()
    Start-Sleep -Seconds 5
    $rows += Measure-State $p "登录页空闲" $Seconds
    Stop-Process -Id $p.Id -Force; Start-Sleep -Seconds 1

    # ---- 机器人进频道 ----
    $bot = Start-Process -FilePath $talker -ArgumentList @($link) -PassThru -WindowStyle Hidden -RedirectStandardOutput "$work\talker.log"
    $started += $bot
    Start-Sleep -Seconds 2
    $botKey = (Select-String -Path "$work\talker.log" -Pattern '公钥 ([a-z0-9]+)').Matches[0].Groups[1].Value
    Add-Content "$work\client\gouhuo\settings.txt" "volume.$botKey=0"

    # ---- 2. 在频道里，窗口在前台 ----
    $p = Start-Client @($link)
    Start-Sleep -Seconds 8
    $rows += Measure-State $p "在频道里，前台" $Seconds

    # ---- 3. 最小化 ----
    $h = [FP]::SlintWindow([uint32]$p.Id)
    # 直接让系统最小化它（SW_MINIMIZE）。发 WM_SYSCOMMAND 的话，窗口正忙的那一下
    # 会落空 —— 实测过一次，两小时的长测开跑两分钟就停在这儿。
    for ($i = 0; $i -lt 10 -and -not [FP]::IsIconic($h); $i++) {
        [FP]::ShowWindowAsync($h, 6) | Out-Null
        Start-Sleep -Milliseconds 500
    }
    if (-not [FP]::IsIconic($h)) { throw "没最小化成" }
    Start-Sleep -Seconds 4
    $rows += Measure-State $p "在频道里，最小化" $Seconds

    # ---- 4. 收到托盘 ----
    for ($i = 0; $i -lt 10 -and [FP]::IsIconic($h); $i++) {
        [FP]::ShowWindowAsync($h, 9) | Out-Null   # SW_RESTORE
        Start-Sleep -Milliseconds 500
    }
    Start-Sleep -Seconds 1
    [FP]::PostMessage($h, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null   # WM_CLOSE = 点 ×
    Start-Sleep -Seconds 5
    if ([FP]::IsWindowVisible($h)) { throw "没收到托盘" }
    $rows += Measure-State $p "在频道里，托盘" $Seconds

    $rows | Format-Table -AutoSize | Out-String -Width 200

    # ---- 5. 挂着：每分钟一个点 ----
    if ($LongMinutes -gt 0) {
        "收在托盘里挂 $LongMinutes 分钟，每分钟一个点："
        for ($m = 1; $m -le $LongMinutes; $m++) {
            $r = Measure-State $p "第 $m 分钟" 60
            "{0,-8} 工作集 {1,-18} 私有 {2,-18} CPU {3,-7} 唤醒 {4}/s" -f $r.状态, $r.工作集MB, $r.私有MB, $r.CPU, $r.唤醒每秒
        }
    }
}
finally {
    Stop-All
}
