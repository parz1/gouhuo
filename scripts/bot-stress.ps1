# Run release bots against a fresh local server; no microphone or speaker.
# cargo build --release -p server -p test-bot
# ./scripts/bot-stress.ps1 -Counts 8,32,64 -Seconds 30
[CmdletBinding()]
param(
    [ValidateRange(2, 120)][int]$Seconds = 30,
    [ValidateRange(1, 256)][int[]]$Counts = @(8, 32, 64),
    [ValidateRange(0, 256)][int]$Speakers = 0,
    [ValidateSet('clean', 'loss', 'outage')][string[]]$Scenarios = @('clean', 'loss', 'outage')
)
$ErrorActionPreference = 'Stop'
if ($Speakers -gt 0 -and @($Counts | Where-Object { $_ -lt $Speakers }).Count -gt 0) { throw 'Speakers must not exceed any bot count' }
if ($Scenarios -contains 'outage' -and $Seconds -lt 25) { throw 'Outage scenario requires at least 25 seconds' }
$root = Split-Path -Parent $PSScriptRoot
$serverExe = Join-Path $root 'target/release/gouhuo-server.exe'
$botExe = Join-Path $root 'target/release/gouhuo-bot.exe'
foreach ($path in @($serverExe, $botExe)) {
    if (-not (Test-Path -LiteralPath $path)) { throw "Build release binaries first: $path" }
}
$run = Join-Path $root ('target/bot-stress/' + (Get-Date -Format 'yyyyMMdd-HHmmss') + '-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $run | Out-Null
$cpu = $env:PROCESSOR_IDENTIFIER
try { $cpu = (Get-ItemProperty -LiteralPath 'HKLM:\HARDWARE\DESCRIPTION\System\CentralProcessor\0').ProcessorNameString } catch { }
$meta = [ordered]@{
    timestamp = (Get-Date).ToString('o'); platform = [System.Environment]::OSVersion.VersionString
    cpu = $cpu; logical_cpus = [System.Environment]::ProcessorCount
    seconds = $Seconds; counts = $Counts; scenarios = $Scenarios; speakers = $Speakers; ramp_ms = 100
    revision = (git -C $root rev-parse HEAD); server_sha256 = (Get-FileHash $serverExe).Hash
    working_tree_dirty = (@(git -C $root status --porcelain 2>$null).Count -gt 0)
    bot_sha256 = (Get-FileHash $botExe).Hash
    topology = 'Windows loopback; server and all bots on same host; speakers=0 means all transmit continuously'
}
$meta | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $run 'metadata.json') -Encoding utf8

function Start-Hidden([string]$exe, [string[]]$arguments, [hashtable]$environment) {
    $info = [System.Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $exe; $info.WorkingDirectory = $root
    $info.UseShellExecute = $false; $info.CreateNoWindow = $true
    $info.WindowStyle = [System.Diagnostics.ProcessWindowStyle]::Hidden
    $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
    foreach ($argument in $arguments) { $info.ArgumentList.Add($argument) }
    foreach ($key in @($info.Environment.Keys)) {
        if ($key.StartsWith('GOUHUO_')) { $info.Environment.Remove($key) | Out-Null }
    }
    foreach ($key in $environment.Keys) { $info.Environment[$key] = [string]$environment[$key] }
    return [System.Diagnostics.Process]::Start($info)
}

foreach ($count in $Counts) {
    foreach ($scenario in $Scenarios) {
        $case = Join-Path $run "$count-$scenario"
        New-Item -ItemType Directory -Path $case | Out-Null
        $srv = $null; $bots = $null; $serverTail = $null; $serverErr = $null
        $botOut = $null; $botErr = $null; $startup = ''
        $resources = [System.Collections.Generic.List[object]]::new()
        try {
            # Production invitations use the configured port; choose a nonzero
            # port free for both protocols, below Windows dynamic reservations.
            $port = $null
            for ($attempt = 0; $attempt -lt 30; $attempt++) {
                $candidate = Get-Random -Minimum 21000 -Maximum 32000
                $tcp = $null; $udp = $null
                try {
                    $tcp = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Any, $candidate)
                    $tcp.Start()
                    $udp = [System.Net.Sockets.UdpClient]::new($candidate)
                    $port = $candidate
                    break
                } catch { } finally {
                    if ($null -ne $tcp) { $tcp.Stop() }
                    if ($null -ne $udp) { $udp.Dispose() }
                }
            }
            if ($null -eq $port) { throw 'No free TCP/UDP test port found' }
            $srv = Start-Hidden $serverExe @() @{
                GOUHUO_PORT = $port; GOUHUO_HOST = '127.0.0.1'; GOUHUO_DATA = (Join-Path $case 'server-data')
                GOUHUO_MAX_USERS = ($count + 8)
            }
            $serverErr = $srv.StandardError.ReadToEndAsync()
            $deadline = [datetime]::UtcNow.AddSeconds(15)
            while ([datetime]::UtcNow -lt $deadline) {
                $line = $srv.StandardOutput.ReadLineAsync()
                if (-not $line.Wait([Math]::Max(1, [int]($deadline - [datetime]::UtcNow).TotalMilliseconds))) {
                    throw 'Server startup timed out'
                }
                if ($null -eq $line.Result) { throw 'Server exited before invitation was ready' }
                $startup += $line.Result + "`n"
                if ($line.Result -match 'gouhuo://j/[a-z0-9]+') { $invite = $Matches[0]; break }
            }
            $serverTail = $srv.StandardOutput.ReadToEndAsync()
            $inviteFile = Join-Path $case 'invite.txt'
            Set-Content -LiteralPath $inviteFile -Value $invite -Encoding utf8
            $arguments = @('--invite-file', $inviteFile, '--count', "$count", '--seconds', "$Seconds", '--log', (Join-Path $case 'bots.jsonl'))
            if ($Speakers -gt 0) { $arguments += @('--speakers', "$Speakers") }
            if ($scenario -eq 'loss') { $arguments += @('--loss', '5', '--delay-ms', '30', '--jitter-ms', '10', '--seed', '42') }
            if ($scenario -eq 'outage') {
                $arguments += @('--outage-at', '5', '--outage-seconds', '12', '--outage-direction', 'up')
            }
            $bots = Start-Hidden $botExe $arguments @{}
            $botOut = $bots.StandardOutput.ReadToEndAsync(); $botErr = $bots.StandardError.ReadToEndAsync()
            $clock = [System.Diagnostics.Stopwatch]::StartNew()
            $previousTime = 0.0; $previousServerCpu = $srv.TotalProcessorTime.TotalSeconds
            $previousBotCpu = $bots.TotalProcessorTime.TotalSeconds
            while (-not $bots.HasExited) {
                Start-Sleep -Milliseconds 500
                if ($srv.HasExited) { throw 'Server exited during load' }
                if ($clock.Elapsed.TotalSeconds -gt ($Seconds + 45)) { throw 'Bots exceeded duration plus startup/shutdown allowance' }
                $srv.Refresh(); $bots.Refresh()
                if ($bots.HasExited) { break }
                $now = $clock.Elapsed.TotalSeconds; $interval = $now - $previousTime
                $serverCpu = $srv.TotalProcessorTime.TotalSeconds; $botCpu = $bots.TotalProcessorTime.TotalSeconds
                $resources.Add([ordered]@{
                    elapsed_s = $now; interval_s = $interval
                    server_cpu_core_pct = 100 * ($serverCpu - $previousServerCpu) / $interval
                    bots_cpu_core_pct = 100 * ($botCpu - $previousBotCpu) / $interval
                    server_private_bytes = $srv.PrivateMemorySize64; server_working_set_bytes = $srv.WorkingSet64
                    bots_private_bytes = $bots.PrivateMemorySize64; bots_working_set_bytes = $bots.WorkingSet64
                    server_threads = $srv.Threads.Count; bot_threads = $bots.Threads.Count
                })
                $previousTime = $now; $previousServerCpu = $serverCpu; $previousBotCpu = $botCpu
            }
            $bots.WaitForExit()
            [ordered]@{ bot_exit_code = $bots.ExitCode; measured_s = $clock.Elapsed.TotalSeconds } |
                ConvertTo-Json | Set-Content -LiteralPath (Join-Path $case 'status.json') -Encoding utf8
            Write-Output "$count-$scenario completed (exit $($bots.ExitCode))"
        } finally {
            # Only handles created by this case are stopped; persisted data stays for inspection.
            foreach ($process in @($bots, $srv)) {
                if ($null -ne $process -and -not $process.HasExited) { $process.Kill(); $process.WaitForExit() }
            }
            [System.IO.File]::WriteAllLines((Join-Path $case 'resources.jsonl'), [string[]]@(
                $resources | ForEach-Object { $_ | ConvertTo-Json -Compress }
            ))
            if ($null -ne $serverTail) { $startup += $serverTail.Result }
            Set-Content -LiteralPath (Join-Path $case 'server.stdout.log') -Value $startup -Encoding utf8
            if ($null -ne $serverErr) { Set-Content -LiteralPath (Join-Path $case 'server.stderr.log') -Value $serverErr.Result -Encoding utf8 }
            if ($null -ne $botOut) { Set-Content -LiteralPath (Join-Path $case 'bots.stdout.log') -Value $botOut.Result -Encoding utf8 }
            if ($null -ne $botErr) { Set-Content -LiteralPath (Join-Path $case 'bots.stderr.log') -Value $botErr.Result -Encoding utf8 }
            foreach ($process in @($bots, $srv)) { if ($null -ne $process) { $process.Dispose() } }
        }
    }
}
Write-Output "Results: $run"
