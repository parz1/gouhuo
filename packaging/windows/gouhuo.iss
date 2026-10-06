; SPDX-License-Identifier: GPL-3.0-or-later
;
; 篝火客户端的 Windows 安装包。别直接用 ISCC 编它，跑 packaging/windows/build.ps1：
; 它先编 dist 版的 exe，再把版本号传进来。
;
; 取舍（见 #9）：
; - **按用户装**，装到 %LOCALAPPDATA%\Programs\gouhuo，不弹 UAC。网吧、公司电脑上
;   没管理员权限也能装；gouhuo:// 协议也注册在 HKCU 里
; - **没签名**：第一次运行 SmartScreen 会拦一下，README 里写了怎么过
; - 卸载**不删** %APPDATA%\gouhuo：那里是身份密钥，删了就再也找不回这个身份

#ifndef AppVersion
  #error 要用 /DAppVersion=x.y.z 传版本号（build.ps1 会传）
#endif
#ifndef ExePath
  #define ExePath "..\..\target\dist\gouhuo.exe"
#endif
#ifndef OutputDir
  #define OutputDir "..\..\target\installer"
#endif
#ifndef VoiceExePath
  #define VoiceExePath "..\..\target\dist\gouhuo-voice.exe"
#endif

[Setup]
; 这个 GUID 永远别改：Windows 靠它认出「这是同一个程序的新版本」，改了就会装出两份。
AppId={{0C88191F-C5FF-472E-B19E-B30DDF7F5280}
AppName=篝火
AppVersion={#AppVersion}
AppVerName=篝火 {#AppVersion}
AppPublisher=篝火
AppPublisherURL=https://github.com/parz1/gouhuo
AppSupportURL=https://github.com/parz1/gouhuo/issues
AppUpdatesURL=https://github.com/parz1/gouhuo/releases
VersionInfoVersion={#AppVersion}

PrivilegesRequired=lowest
; {autopf} 在按用户装时就是 %LOCALAPPDATA%\Programs
DefaultDirName={autopf}\gouhuo
DisableDirPage=auto
DisableProgramGroupPage=yes
DefaultGroupName=篝火
UninstallDisplayName=篝火
UninstallDisplayIcon={app}\gouhuo.exe

; 客户端是 x64 的；Windows 10 起才有深色标题栏、按显示器的 DPI 这些
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0

; 篝火开着的时候没法覆盖 exe：先请用户关掉。名字跟 src/single_instance.rs 的一致。
AppMutex=GouhuoClientRunning
CloseApplications=yes
RestartApplications=no

SetupIconFile=..\..\crates\client\ui\icons\gouhuo.ico
WizardStyle=modern
Compression=lzma2/ultra64
SolidCompression=yes
OutputDir={#OutputDir}
OutputBaseFilename=gouhuo-setup-{#AppVersion}
; 协议注册改了 HKCU\Software\Classes，让资源管理器刷新一下关联
ChangesAssociations=yes

[Languages]
Name: "zh"; MessagesFile: "ChineseSimplified.isl"

[Tasks]
Name: "desktopicon"; Description: "在桌面上放一个快捷方式"; GroupDescription: "快捷方式："

[Files]
Source: "{#ExePath}"; DestDir: "{app}"; DestName: "gouhuo.exe"; Flags: ignoreversion
Source: "{#VoiceExePath}"; DestDir: "{app}"; DestName: "gouhuo-voice.exe"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\篝火"; Filename: "{app}\gouhuo.exe"
Name: "{autodesktop}\篝火"; Filename: "{app}\gouhuo.exe"; Tasks: desktopicon

[Registry]
; gouhuo:// 链接点一下就打开篝火。卸载时整个键删掉。
Root: HKCU; Subkey: "Software\Classes\gouhuo"; ValueType: string; ValueName: ""; ValueData: "URL:篝火邀请链接"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\gouhuo"; ValueType: string; ValueName: "URL Protocol"; ValueData: ""
Root: HKCU; Subkey: "Software\Classes\gouhuo\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: """{app}\gouhuo.exe"",0"
Root: HKCU; Subkey: "Software\Classes\gouhuo\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\gouhuo.exe"" ""%1"""

[Run]
Filename: "{app}\gouhuo.exe"; Description: "现在打开篝火"; Flags: nowait postinstall skipifsilent
