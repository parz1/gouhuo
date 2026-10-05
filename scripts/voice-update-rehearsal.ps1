# Build a different engine in an isolated source copy, keeping the UI unchanged.
[CmdletBinding()]
param([switch]$Hardware, [string]$UiPath = 'target/dist/gouhuo.exe')
$ErrorActionPreference = 'Stop'
$repo = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$rehearsal = Join-Path $repo ("target/voice-update-rehearsal/" + [Guid]::NewGuid().ToString('N'))
$source = Join-Path $rehearsal 'source'
$ui = if ([IO.Path]::IsPathRooted($UiPath)) { $UiPath } else { Join-Path $repo $UiPath }
if (-not (Test-Path -LiteralPath $ui)) { throw 'Build the client artifact before rehearsal.' }
$uiHash = (Get-FileHash -LiteralPath $ui -Algorithm SHA256).Hash
New-Item -ItemType Directory -Path $source -Force | Out-Null
$files = & git -C $repo ls-files
if ($LASTEXITCODE -ne 0) { throw 'Cannot list workspace source.' }
foreach ($file in $files) {
    if ($file -notmatch '^(Cargo\.(toml|lock)|crates/|\.cargo/)') { continue }
    $destination = [IO.Path]::GetFullPath((Join-Path $source $file))
    if (-not $destination.StartsWith($source + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Source path escaped rehearsal directory.'
    }
    $original = Join-Path $repo $file
    if (-not (Test-Path -LiteralPath $original -PathType Leaf)) { continue }
    New-Item -ItemType Directory -Path (Split-Path -Parent $destination) -Force | Out-Null
    Copy-Item -LiteralPath $original -Destination $destination
}
$manifest = Join-Path $source 'crates/voice-engine/Cargo.toml'
$text = [IO.File]::ReadAllText($manifest)
$versionMatch = [regex]::Match($text, '(?m)^version = "(\d+)\.(\d+)\.(\d+)"\r?$')
if (-not $versionMatch.Success) { throw 'Engine version must have three numeric segments.' }
$nextPatch = [uint64]$versionMatch.Groups[3].Value + 1
$updatedVersion = '{0}.{1}.{2}' -f $versionMatch.Groups[1].Value, $versionMatch.Groups[2].Value, $nextPatch
$text = $text -replace '(?m)^version = "[^"]+"', "version = `"$updatedVersion`""
# A distinct binary name prevents the original workspace artifact from
# overwriting this output while Cargo still considers the newer build fresh.
$text = $text -replace '(?m)^name = "gouhuo-voice"', 'name = "gouhuo-voice-rehearsal"'
[IO.File]::WriteAllText($manifest, $text)
$previousPolicy = $env:CMAKE_POLICY_VERSION_MINIMUM
$previousRoot = $env:GOUHUO_REHEARSAL_ROOT
$previousEngine = $env:GOUHUO_REHEARSAL_ENGINE
$previousUi = $env:GOUHUO_REHEARSAL_UI
$previousHardware = $env:GOUHUO_REHEARSAL_HARDWARE
Push-Location $repo
try {
    $env:CMAKE_POLICY_VERSION_MINIMUM = '3.5'
    # The copy's lockfile alone changes; the real manifest and lock stay intact.
    & cargo build --offline --manifest-path (Join-Path $source 'Cargo.toml') --target-dir (Join-Path $repo 'target') -p voice-engine --bin gouhuo-voice-rehearsal
    if ($LASTEXITCODE -ne 0) { throw 'Independent engine build failed.' }
    $updated = Join-Path $rehearsal "gouhuo-voice-$updatedVersion.exe"
    Copy-Item -LiteralPath (Join-Path $repo 'target/debug/gouhuo-voice-rehearsal.exe') -Destination $updated
    $env:GOUHUO_REHEARSAL_ROOT = $rehearsal
    $env:GOUHUO_REHEARSAL_ENGINE = $updated
    $env:GOUHUO_REHEARSAL_UI = $ui
    $env:GOUHUO_REHEARSAL_HARDWARE = if ($Hardware) { '1' } else { '0' }
    & cargo test --offline --locked -p voice-engine --test process independent_update_rehearsal -- --ignored --exact --nocapture
    if ($LASTEXITCODE -ne 0) { throw "Rehearsal failed; artifacts retained in $rehearsal" }
    if ((Get-FileHash -LiteralPath $ui -Algorithm SHA256).Hash -ne $uiHash) { throw 'UI changed during rehearsal.' }
    Write-Output "Rehearsal passed. Evidence: $rehearsal/report.json"
    Get-Content -LiteralPath (Join-Path $rehearsal 'report.json')
} finally {
    Pop-Location
    $env:CMAKE_POLICY_VERSION_MINIMUM = $previousPolicy
    $env:GOUHUO_REHEARSAL_ROOT = $previousRoot
    $env:GOUHUO_REHEARSAL_ENGINE = $previousEngine
    $env:GOUHUO_REHEARSAL_UI = $previousUi
    $env:GOUHUO_REHEARSAL_HARDWARE = $previousHardware
}
