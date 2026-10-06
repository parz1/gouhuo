# SPDX-License-Identifier: GPL-3.0-or-later

# Install into a marked isolated directory, restoring the user's app registration.
[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$Installer,
    [Parameter(Mandatory=$true)][string]$AppDir,
    [Parameter(Mandatory=$true)][string]$EvidenceDir
)
$ErrorActionPreference='Stop'
$package=(Resolve-Path -LiteralPath $Installer).Path
$testDir=[IO.Path]::GetFullPath($AppDir).TrimEnd('\')
$evidence=[IO.Path]::GetFullPath($EvidenceDir)
$uninstall='HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\{0C88191F-C5FF-472E-B19E-B30DDF7F5280}_is1'
$protocol='HKCU\Software\Classes\gouhuo'
$oldInstall=Get-ItemProperty ('Registry::'+$uninstall) -ErrorAction SilentlyContinue
$oldProtocolCommand=(Get-ItemProperty ('Registry::'+$protocol+'\shell\open\command') -ErrorAction SilentlyContinue).'(default)'
if ($oldInstall.InstallLocation -and $oldInstall.InstallLocation.TrimEnd('\') -eq $testDir) { throw 'Refusing to use the existing application directory.' }
$marker=Join-Path $testDir '.gouhuo-installer-validation'
if ((Test-Path -LiteralPath $testDir) -and -not (Test-Path -LiteralPath $marker)) { throw 'Existing directory is not marked as an installer test.' }
New-Item -ItemType Directory -Force -Path $testDir,$evidence | Out-Null
Set-Content -LiteralPath $marker -Value 'Isolated installer verification; not the normal installation.'
$backups=@()
foreach ($entry in @(@('uninstall',$uninstall),@('protocol',$protocol))) {
    $file=Join-Path $evidence ($entry[0]+'.reg')
    $exists=Test-Path ('Registry::'+$entry[1])
    if ($exists) { & reg.exe export $entry[1] $file /y | Out-Null; if ($LASTEXITCODE -ne 0) { throw 'Registry backup failed.' } }
    $backups += [pscustomobject]@{key=$entry[1];file=$file;exists=$exists}
}
try {
    $args=@('/VERYSILENT','/SUPPRESSMSGBOXES','/NORESTART','/NOICONS',('/DIR="'+$testDir+'"'),('/LOG="'+(Join-Path $evidence 'install.log')+'"'))
    $setup=Start-Process -FilePath $package -ArgumentList $args -WindowStyle Hidden -Wait -PassThru
    if ($setup.ExitCode -ne 0) { throw "Installer failed: $($setup.ExitCode)" }
    $installed=Get-ItemProperty ('Registry::'+$uninstall)
    if ($installed.InstallLocation.TrimEnd('\') -ne $testDir) { throw 'Installer did not honor the isolated directory.' }
    $files=@('gouhuo.exe','gouhuo-voice.exe','LICENSE.txt') | ForEach-Object {
        $path=Join-Path $testDir $_
        $item=Get-Item -LiteralPath $path
        [ordered]@{name=$item.Name;bytes=$item.Length;sha256=(Get-FileHash -LiteralPath $path).Hash;version=$item.VersionInfo.FileVersion}
    }
    [ordered]@{schema=1;installer_sha256=(Get-FileHash -LiteralPath $package).Hash;exit_code=$setup.ExitCode;display_version=$installed.DisplayVersion;files=$files} | ConvertTo-Json -Depth 5 | Set-Content (Join-Path $evidence 'installed.json') -Encoding utf8
} finally {
    foreach ($backup in $backups) {
        if ($backup.exists) { & reg.exe import $backup.file | Out-Null; if ($LASTEXITCODE -ne 0) { throw 'Registry restoration failed; backup retained.' } }
        elseif (Test-Path ('Registry::'+$backup.key)) {
            $current=Get-ItemProperty ('Registry::'+$backup.key)
            $owned=if ($backup.key -eq $uninstall) { $current.InstallLocation.TrimEnd('\') -eq $testDir } else { (Get-ItemProperty ('Registry::'+$protocol+'\shell\open\command')).'(default)' -like ('"'+$testDir+'\gouhuo.exe"*') }
            if (-not $owned) { throw 'Registration changed outside the test; refusing cleanup.' }
            Remove-Item -LiteralPath ('Registry::'+$backup.key) -Recurse
        }
    }
}
$restoredInstall=Get-ItemProperty ('Registry::'+$uninstall) -ErrorAction SilentlyContinue
$restoredProtocolCommand=(Get-ItemProperty ('Registry::'+$protocol+'\shell\open\command') -ErrorAction SilentlyContinue).'(default)'
if ($oldInstall.InstallLocation -ne $restoredInstall.InstallLocation -or $oldInstall.DisplayVersion -ne $restoredInstall.DisplayVersion -or $oldProtocolCommand -ne $restoredProtocolCommand) { throw 'Original app registration did not restore exactly.' }
[ordered]@{uninstall_location_restored=$true;display_version_restored=$true;protocol_command_restored=$true} | ConvertTo-Json | Set-Content (Join-Path $evidence 'registration-restored.json') -Encoding utf8
Get-Content (Join-Path $evidence 'installed.json')
