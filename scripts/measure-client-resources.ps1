# SPDX-License-Identifier: GPL-3.0-or-later

# Measure UI + its voice child without injecting input or activating accessibility.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][int]$ClientPid,
    [Parameter(Mandatory = $true)][string]$State,
    [Parameter(Mandatory = $true)][string]$OutFile,
    [ValidateRange(5, 300)][int]$Seconds = 30
)
$ErrorActionPreference = 'Stop'
$children = @(Get-CimInstance Win32_Process -Filter "ParentProcessId=$ClientPid AND Name='gouhuo-voice.exe'")
if ($children.Count -ne 1) { throw 'Expected one stable voice child.' }
$processes = @((Get-Process -Id $ClientPid), (Get-Process -Id $children[0].ProcessId))
$startTimes = @($processes | ForEach-Object { $_.StartTime.ToUniversalTime().ToString('o') })
$initialCpu = @($processes | ForEach-Object { $_.TotalProcessorTime.TotalSeconds })
$started = [DateTime]::UtcNow
$clock = [Diagnostics.Stopwatch]::StartNew()
$samples = @()
for ($i = 0; $i -lt $Seconds; $i++) {
    Start-Sleep -Seconds 1
    $row = [ordered]@{ elapsed_s = $clock.Elapsed.TotalSeconds }
    for ($n = 0; $n -lt $processes.Count; $n++) {
        $process = $processes[$n]
        $process.Refresh()
        if ($process.HasExited -or $process.StartTime.ToUniversalTime().ToString('o') -ne $startTimes[$n]) { throw 'Process exited/restarted; measurement invalid.' }
        $label = if ($n -eq 0) { 'ui' } else { 'engine' }
        $row["${label}_working_set_bytes"] = $process.WorkingSet64
        $row["${label}_private_bytes"] = $process.PrivateMemorySize64
        $row["${label}_cpu_s"] = $process.TotalProcessorTime.TotalSeconds
        $row["${label}_threads"] = $process.Threads.Count
    }
    $samples += [pscustomobject]$row
}
$clock.Stop()
$cpu = @()
for ($n = 0; $n -lt $processes.Count; $n++) { $cpu += ($processes[$n].TotalProcessorTime.TotalSeconds - $initialCpu[$n]) / $clock.Elapsed.TotalSeconds * 100 }
$ws = @($samples | ForEach-Object { $_.ui_working_set_bytes + $_.engine_working_set_bytes })
$private = @($samples | ForEach-Object { $_.ui_private_bytes + $_.engine_private_bytes })
$machineCpu = Get-CimInstance Win32_Processor | Select-Object Name,NumberOfLogicalProcessors
$machineOs = Get-CimInstance Win32_OperatingSystem | Select-Object Caption,Version,TotalVisibleMemorySize
$result = [ordered]@{
    schema = 1; state = $State; started_utc = $started.ToString('o'); elapsed_s = $clock.Elapsed.TotalSeconds
    environment = [ordered]@{ processors = @($machineCpu); os = $machineOs }
    cpu_basis = 'single core percent; UI plus one unchanged child; no normalization by logical CPUs'
    ui_cpu_percent = $cpu[0]; engine_cpu_percent = $cpu[1]; total_cpu_percent = $cpu[0] + $cpu[1]
    total_working_set_mean_bytes = ($ws | Measure-Object -Average).Average
    total_working_set_peak_bytes = ($ws | Measure-Object -Maximum).Maximum
    total_private_mean_bytes = ($private | Measure-Object -Average).Average
    total_private_peak_bytes = ($private | Measure-Object -Maximum).Maximum
    processes = @($processes | ForEach-Object { [ordered]@{ pid = $_.Id; name = $_.ProcessName; sha256 = (Get-FileHash -LiteralPath $_.Path).Hash; version = $_.FileVersion } })
    samples = $samples
}
$fullOutput = [IO.Path]::GetFullPath($OutFile)
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $fullOutput) | Out-Null
$result | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $fullOutput -Encoding utf8
[pscustomobject]$result | Select-Object state,elapsed_s,total_cpu_percent,total_working_set_mean_bytes,total_private_mean_bytes | Format-List
