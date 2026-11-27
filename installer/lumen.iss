; Lumen-Setup.exe: Inno Setup 6 script.
;
; Build with installer\build.ps1, which finds (or installs) Inno Setup, reads
; the version from Cargo.toml and passes it here:
;   iscc /DMyAppVersion=2.1.0 /DMyAppExe=C:\path\to\Lumen.exe /Odist installer\lumen.iss
;
; Per-user install by default (no admin rights, %LOCALAPPDATA%\Programs\Lumen);
; the first page lets the user choose all users instead (Program Files).
; Lumen keeps its settings in %APPDATA%\Lumen, which uninstalling leaves alone.
; The installed copy never has a portable.txt beside it, so it always uses
; %APPDATA% (portable mode is for Lumen.exe / Lumen-portable.zip only).

#ifndef MyAppVersion
#error MyAppVersion is not defined: build with installer\build.ps1 (or pass /DMyAppVersion=x.y.z)
#endif
#ifndef MyAppExe
#define MyAppExe "..\dist\Lumen.exe"
#endif
#ifndef MyCompression
#define MyCompression "lzma2/max"
#endif

#define MyAppName "Lumen"
#define MyAppPublisher "Rahul Tarigoppula"
#define MyAppURL "https://github.com/RahulTarigoppula/lumen-image-viewer"

[Setup]
; Never change the AppId: new versions find the old install by it and upgrade
; in place. The uninstall entry is HKCU (or HKLM)\...\Uninstall\{AppId}_is1.
AppId={{F470A35E-944D-4549-8773-857A16786614}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}/issues
AppUpdatesURL={#MyAppURL}/releases/latest
AppCopyright=Copyright (c) 2026 {#MyAppPublisher}
VersionInfoVersion={#MyAppVersion}
VersionInfoProductName={#MyAppName}
VersionInfoDescription={#MyAppName} Setup
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
DefaultDirName={autopf}\{#MyAppName}
DisableProgramGroupPage=yes
LicenseFile=..\LICENSE
SetupIconFile=..\assets\lumen.ico
UninstallDisplayIcon={app}\Lumen.exe
UninstallDisplayName={#MyAppName}
WizardStyle=modern
CloseApplications=yes
Compression={#MyCompression}
SolidCompression=yes
OutputBaseFilename=Lumen-Setup

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "fileassoc"; Description: "Use Lumen to open pictures and videos"; GroupDescription: "File types (adds Lumen to 'Open with' and Windows Default apps):"
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#MyAppExe}"; DestDir: "{app}"; DestName: "Lumen.exe"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion
Source: "..\THIRD-PARTY-NOTICES.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\Lumen.exe"; Comment: "Image and video viewer"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\Lumen.exe"; Comment: "Image and video viewer"; Tasks: desktopicon

[Run]
; The same registration as Settings, Windows integration, Register. It writes
; to the user's own registry (HKCU), so it runs as the user who started Setup
; even when Setup was elevated for an all-users install.
Filename: "{app}\Lumen.exe"; Parameters: "--register-file-types"; StatusMsg: "Registering Lumen for pictures and videos..."; Flags: runhidden waituntilterminated runasoriginaluser; Tasks: fileassoc
Filename: "{app}\Lumen.exe"; Description: "{cm:LaunchProgram,{#MyAppName}}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; Removes "Open with", Default apps and "Browse with Lumen" entries (Settings,
; Windows integration, Remove all integration). Settings stay in %APPDATA%\Lumen.
Filename: "{app}\Lumen.exe"; Parameters: "--unregister"; Flags: runhidden waituntilterminated; RunOnceId: "LumenUnregister"
