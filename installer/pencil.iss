; Inno Setup script for Pencil.
;
; Per-user install (no admin rights), so updates can run silently without a UAC prompt.
; Build: ISCC.exe /DAppVersion=1.2.3 installer\pencil.iss  -> installer\Output\PencilSetup.exe

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

#define AppName "Pencil"
#define AppExe "pencil.exe"
; Must match UNINSTALL_KEY in src/updater.rs.
#define AppId "{950E1D4D-F50A-497C-846C-8EBDC0033C1E}"
#define RunKey "Software\Microsoft\Windows\CurrentVersion\Run"

[Setup]
AppId={{#AppId}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=vlazaroes
AppPublisherURL=https://github.com/vlazaroes/pencil
AppUpdatesURL=https://github.com/vlazaroes/pencil/releases
VersionInfoVersion={#AppVersion}
DefaultDirName={localappdata}\Programs\{#AppName}
DefaultGroupName={#AppName}
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Fixed per-user folder: a protected folder (e.g. Program Files) would need admin
; rights, and then updates could no longer install silently.
DisableDirPage=yes
DisableProgramGroupPage=yes
DisableReadyPage=yes
SetupIconFile=..\assets\pencil.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
OutputDir=Output
OutputBaseFilename=PencilSetup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; Pencil is closed by the [Code] section below, before files are replaced.
CloseApplications=no

[Tasks]
; Only offered on a fresh install: updates keep whatever the user chose in Settings.
Name: "startup"; Description: "Start {#AppName} with Windows"; Check: not IsUpgrade

[Files]
Source: "..\target\release\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"

[Registry]
Root: HKCU; Subkey: "{#RunKey}"; ValueType: string; ValueName: "{#AppName}"; ValueData: """{app}\{#AppExe}"""; Tasks: startup

[Run]
; Also runs after a silent update, so Pencil comes back on its own.
Filename: "{app}\{#AppExe}"; Description: "Launch {#AppName}"; Flags: postinstall nowait

[Code]
var
  Upgrade: Boolean;

function IsUpgrade: Boolean;
begin
  Result := Upgrade;
end;

{ Asks a running Pencil to quit (WM_CLOSE to its hidden control window) and waits. }
procedure ClosePencil;
var
  Wnd: HWND;
  I: Integer;
begin
  Wnd := FindWindowByClassName('PencilCtrl');
  if Wnd = 0 then
    Exit;
  PostMessage(Wnd, 16 { WM_CLOSE }, 0, 0);
  for I := 1 to 50 do
  begin
    Sleep(100);
    if FindWindowByClassName('PencilCtrl') = 0 then
      Exit;
  end;
end;

function InitializeSetup: Boolean;
begin
  Upgrade := RegValueExists(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{#AppId}_is1', 'UninstallString');
  Result := True;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  ClosePencil;
  Result := '';
end;

function InitializeUninstall: Boolean;
begin
  ClosePencil;
  Result := True;
end;

{ Removing the program also removes its logon entry and settings. }
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
  begin
    RegDeleteValue(HKCU, '{#RunKey}', '{#AppName}');
    RegDeleteKeyIncludingSubkeys(HKCU, 'Software\{#AppName}');
  end;
end;
