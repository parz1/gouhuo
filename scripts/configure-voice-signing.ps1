# Windows/PowerShell 7: provision a release key without printing its private seed.
[CmdletBinding()]
param([ValidatePattern('^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$')][string]$Repository = 'parz1/gouhuo')
$ErrorActionPreference = 'Stop'
if ($PSVersionTable.PSVersion.Major -lt 7 -or -not $IsWindows) { throw 'Requires Windows and PowerShell 7.' }
$repoRoot = Split-Path -Parent $PSScriptRoot
$gh = (Get-Command gh -ErrorAction Stop).Source
$node = (Get-Command node -ErrorAction Stop).Source
function Invoke-Captured([string]$File, [string[]]$Arguments, [string]$InputText = '', [hashtable]$Environment = @{}) {
    $start = [Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $File
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardInput = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    foreach ($argument in $Arguments) { $start.ArgumentList.Add($argument) }
    foreach ($name in $Environment.Keys) { $start.Environment[$name] = $Environment[$name] }
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $start
    try {
        if (-not $process.Start()) { throw 'Subprocess did not start.' }
        $output = $process.StandardOutput.ReadToEndAsync()
        $errorOutput = $process.StandardError.ReadToEndAsync()
        $process.StandardInput.Write($InputText)
        $process.StandardInput.Close()
        if (-not $process.WaitForExit(60000)) {
            $process.Kill($true)
            $process.WaitForExit()
            throw 'Subprocess timed out.'
        }
        if ($process.ExitCode -ne 0) { throw 'Subprocess failed; secret-bearing output withheld.' }
        $null = $errorOutput.GetAwaiter().GetResult()
        return $output.GetAwaiter().GetResult()
    } finally { $process.Dispose() }
}
$backupDirectory = Join-Path $env:LOCALAPPDATA ('gouhuo-release-signing/' + $Repository.Replace('/', '-'))
$backupPath = Join-Path $backupDirectory 'voice-signing-key.dpapi'
$metadataPath = Join-Path $backupDirectory 'public-key.json'
$privateSeed = $null
$pair = $null
$secureSeed = $null
$generationOutput = $null
try {
    $secrets = (Invoke-Captured $gh @('api', "repos/$Repository/actions/secrets")) | ConvertFrom-Json
    $variables = (Invoke-Captured $gh @('api', "repos/$Repository/actions/variables")) | ConvertFrom-Json
    if ($secrets.secrets.name -contains 'GOUHUO_VOICE_SIGNING_KEY' -or $variables.variables.name -contains 'GOUHUO_VOICE_PUBLIC_KEY') {
        throw 'Signing configuration already exists; refusing to replace or rotate it.'
    }
    if (Test-Path -LiteralPath $backupPath) {
        $secureSeed = ConvertTo-SecureString ([IO.File]::ReadAllText($backupPath))
        $pointer = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secureSeed)
        try { $privateSeed = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($pointer) }
        finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($pointer) }
    }
    $keyCode = @'
const c = require('node:crypto');
const input = require('node:fs').readFileSync(0, 'utf8');
let privateKey;
if (input) {
  if (!/^[0-9a-f]{64}$/.test(input)) process.exit(1);
  privateKey = c.createPrivateKey({key:Buffer.concat([Buffer.from('302e020100300506032b657004220420','hex'),Buffer.from(input,'hex')]),format:'der',type:'pkcs8'});
} else { privateKey = c.generateKeyPairSync('ed25519').privateKey; }
const secret = Buffer.from(privateKey.export({format:'jwk'}).d,'base64url');
const publicKey = c.createPublicKey(privateKey);
const pub = Buffer.from(publicKey.export({format:'jwk'}).x,'base64url');
const message = Buffer.from('gouhuo-release-key-check');
if (secret.length !== 32 || pub.length !== 32 || !c.verify(null,message,publicKey,c.sign(null,message,privateKey))) process.exit(1);
process.stdout.write(JSON.stringify({seed:secret.toString('hex'),public:pub.toString('hex')}));
secret.fill(0);
'@
    $generationOutput = Invoke-Captured $node @('-e', $keyCode) $privateSeed
    $pair = $generationOutput | ConvertFrom-Json
    $privateSeed = $pair.seed
    if ($privateSeed -notmatch '^[0-9a-f]{64}$' -or $pair.public -notmatch '^[0-9a-f]{64}$') { throw 'Invalid key format.' }
    $null = New-Item -ItemType Directory -Path $backupDirectory -Force
    $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User
    $acl = [Security.AccessControl.DirectorySecurity]::new()
    $acl.SetOwner($sid)
    $acl.SetAccessRuleProtection($true, $false)
    $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($sid, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow'))
    Set-Acl -LiteralPath $backupDirectory -AclObject $acl
    if (-not (Test-Path -LiteralPath $backupPath)) {
        $secureSeed = ConvertTo-SecureString $privateSeed -AsPlainText -Force
        [IO.File]::WriteAllText($backupPath, (ConvertFrom-SecureString $secureSeed))
    }
    # Verify the DPAPI backup before provisioning a key that cannot be recovered.
    $restored = ConvertTo-SecureString ([IO.File]::ReadAllText($backupPath))
    $pointer = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($restored)
    try {
        if ([Runtime.InteropServices.Marshal]::PtrToStringBSTR($pointer) -cne $privateSeed) { throw 'Backup verification failed.' }
    } finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($pointer); $restored.Dispose() }
    $checkDirectory = Join-Path $repoRoot ('target/voice-production-key-check/' + [Guid]::NewGuid().ToString('N'))
    $version = [regex]::Match([IO.File]::ReadAllText((Join-Path $repoRoot 'crates/voice-engine/Cargo.toml')), '(?m)^version = "([^"]+)"').Groups[1].Value
    $null = Invoke-Captured (Join-Path $repoRoot 'target/debug/gouhuo-voice-release.exe') @('sign', (Join-Path $repoRoot 'target/dist/gouhuo-voice.exe'), $version, '0.3.1', $checkDirectory) '' @{
        GOUHUO_VOICE_SIGNING_KEY = $privateSeed
        GOUHUO_VOICE_PUBLIC_KEY = $pair.public
    }
    $verifyCode = @'
const c=require('node:crypto'), fs=require('node:fs'), path=require('node:path');
const pub=Buffer.from(process.argv[1],'hex');
const key=c.createPublicKey({key:Buffer.concat([Buffer.from('302a300506032b6570032100','hex'),pub]),format:'der',type:'spki'});
const manifest=fs.readFileSync(path.join(process.argv[2],'manifest.json'));
const signature=Buffer.from(fs.readFileSync(path.join(process.argv[2],'manifest.sig'),'utf8'),'hex');
if (!c.verify(null,manifest,key,signature)) process.exit(1);
'@
    $null = Invoke-Captured $node @('-e', $verifyCode, $pair.public, $checkDirectory)
    $null = Invoke-Captured $gh @('secret', 'set', 'GOUHUO_VOICE_SIGNING_KEY', '--repo', $Repository) $privateSeed
    $null = Invoke-Captured $gh @('variable', 'set', 'GOUHUO_VOICE_PUBLIC_KEY', '--repo', $Repository, '--body', $pair.public)
    $remotePublic = (Invoke-Captured $gh @('api', "repos/$Repository/actions/variables/GOUHUO_VOICE_PUBLIC_KEY")) | ConvertFrom-Json
    $remoteSecrets = (Invoke-Captured $gh @('api', "repos/$Repository/actions/secrets")) | ConvertFrom-Json
    if ($remotePublic.value -cne $pair.public -or $remoteSecrets.secrets.name -notcontains 'GOUHUO_VOICE_SIGNING_KEY') { throw 'Remote configuration verification failed.' }
    [IO.File]::WriteAllText($metadataPath, (@{repository=$Repository; public_key=$pair.public; backup='Windows current-user DPAPI'; created_at=[DateTime]::UtcNow.ToString('o')} | ConvertTo-Json))
    Write-Output "Configured $Repository signing Secret and public-key Variable."
    Write-Output "Public key: $($pair.public)"
    Write-Output "Encrypted backup: $backupPath"
    Write-Output "Local signing verification: $checkDirectory"
} catch {
    # Never forward subprocess diagnostics or parser errors containing key material.
    throw 'Signing configuration failed. Private data withheld; any encrypted backup has been retained. Inspect repository configuration names before retrying.'
} finally {
    if ($null -ne $secureSeed) { $secureSeed.Dispose() }
    $privateSeed = $null
    $pair = $null
    $generationOutput = $null
}
