; Inno Setup script for the Windows release.
;
; The workflow stages the built binaries in ..\staging and passes the release
; version with /DVersion=YYYY.MDD.REVISION. The installer is per-machine so
; the injected rc_agent.dll sits in Program Files, which the session-0
; WUDFHost token can read; a per-user directory can make LoadLibraryW fail.
;
; The helper scheduled task is still registered from the app's Helper tab
; (one UAC), but the installer stops it before an upgrade and removes it on
; uninstall.

#ifndef Version
  #define Version "0.0.0"
#endif

#define AppName "Remote Mic Rune"
#define AppPublisher "4fuu"
#define AppURL "https://github.com/4fuu/remote-mic-rune"

[Setup]
AppId={{961AA2CD-CAE2-4F16-867F-ABD2D2CA7A05}
AppName={#AppName}
AppVersion={#Version}
AppVerName={#AppName} {#Version}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
AppSupportURL={#AppURL}
AppUpdatesURL={#AppURL}/releases
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=admin
ArchitecturesAllowed=x64
ArchitecturesInstallIn64BitMode=x64
MinVersion=10.0
OutputDir=..
OutputBaseFilename=remote-mic-rune-{#Version}-x86_64-pc-windows-msvc-setup
SetupIconFile=..\crates\rc-app\assets\app-icon.ico
UninstallDisplayName={#AppName}
UninstallDisplayIcon={app}\rc-app.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
RestartApplications=no
SetupLogging=yes
VersionInfoCompany={#AppPublisher}
VersionInfoDescription={#AppName} installer
VersionInfoProductName={#AppName}
VersionInfoProductVersion={#Version}
VersionInfoVersion={#Version}.0
VersionInfoCopyright=Copyright (c) 2026 {#AppPublisher}
LicenseFile=..\LICENSE

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional icons:"; Flags: unchecked

[Files]
Source: "..\staging\rc-app.exe"; DestDir: "{app}"; Flags: ignoreversion restartreplace
Source: "..\staging\rc-helper.exe"; DestDir: "{app}"; Flags: ignoreversion restartreplace
Source: "..\staging\rc_agent.dll"; DestDir: "{app}"; Flags: ignoreversion restartreplace
Source: "..\staging\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\staging\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\staging\THIRD-PARTY-NOTICES.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\rc-app.exe"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\rc-app.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\rc-app.exe"; Description: "Launch {#AppName}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{sys}\schtasks.exe"; Parameters: "/end /tn RemoteMicRuneHelper"; Flags: runhidden waituntilterminated; RunOnceId: "StopHelperTask"
Filename: "{app}\rc-helper.exe"; Parameters: "--remove-task"; Flags: runhidden waituntilterminated; RunOnceId: "RemoveHelperTask"

[Code]
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ResultCode: Integer;
begin
  { Stop a running helper task before replacing rc-helper.exe / rc_agent.dll.
    A missing or stopped task is fine; the app starts it again after launch. }
  Exec(ExpandConstant('{sys}\schtasks.exe'), '/end /tn RemoteMicRuneHelper', '',
    SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Result := '';
end;
