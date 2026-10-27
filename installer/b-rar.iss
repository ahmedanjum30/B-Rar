; Inno Setup script for B-Rar's installer, dist\B-Rar-Setup-<version>.exe. Build it with
; installer\build-setup.ps1, which passes the version and the folder with the release binaries.
;
; Installs for the current user (no admin prompt), into %LOCALAPPDATA%\Programs\B-Rar, or for
; everyone if chosen. Adds "Open with > B-Rar" and a B-Rar submenu to the right-click menu of
; archives (on Windows 11 under "Show more options": the short menu needs the Microsoft Store
; version's package). It doesn't take over the default app for any file type.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef BinDir
  #define BinDir "..\target\release"
#endif

; The archive types B-Rar registers for (the same as installer\AppxManifest.xml).
#dim Ext[15]
#define Ext[0] "zip"
#define Ext[1] "7z"
#define Ext[2] "rar"
#define Ext[3] "tar"
#define Ext[4] "gz"
#define Ext[5] "tgz"
#define Ext[6] "xz"
#define Ext[7] "txz"
#define Ext[8] "zst"
#define Ext[9] "tzst"
#define Ext[10] "bz2"
#define Ext[11] "tbz"
#define Ext[12] "tbz2"
#define Ext[13] "001"
#define Ext[14] "r00"
#define I

[Setup]
; Never change AppId: Windows uses it to find the existing installation on updates.
AppId={{3B7F0E52-6C1D-4A8E-9F25-B0D4E7A61C38}
AppName=B-Rar
AppVersion={#AppVersion}
AppVerName=B-Rar {#AppVersion}
AppPublisher=Ahmad Anjum
AppPublisherURL=https://ahmedanjum30.github.io/B-Rar/
AppSupportURL=https://github.com/ahmedanjum30/B-Rar/issues
AppUpdatesURL=https://github.com/ahmedanjum30/B-Rar/releases
DefaultDirName={autopf}\B-Rar
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
LicenseFile=..\LICENSE
SetupIconFile=..\assets\brar.ico
UninstallDisplayIcon={app}\B-Rar-GUI.exe
UninstallDisplayName=B-Rar
WizardStyle=modern
WizardSizePercent=100
Compression=lzma2/max
SolidCompression=yes
OutputDir=..\dist
OutputBaseFilename=B-Rar-Setup-{#AppVersion}
; Closes a running B-Rar before replacing its files.
CloseApplications=force
RestartApplications=no
ChangesAssociations=yes
ChangesEnvironment=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "addtopath"; Description: "Add B-Rar to PATH (run B-Rar from any terminal)"
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#BinDir}\B-Rar.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\B-Rar-GUI.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "unrar-license.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion
Source: "..\PRIVACY.md"; DestDir: "{app}"; Flags: ignoreversion

[InstallDelete]
; What an update made from inside B-Rar leaves (see crates\brar-gui\src\update.rs).
Type: files; Name: "{app}\*.old"

[Icons]
Name: "{autoprograms}\B-Rar"; Filename: "{app}\B-Rar-GUI.exe"
Name: "{autodesktop}\B-Rar"; Filename: "{app}\B-Rar-GUI.exe"; Tasks: desktopicon

[Registry]
; "Open with > B-Rar": a ProgID listed for each type, so the default app stays as it is.
Root: HKA; Subkey: "Software\Classes\B-Rar.Archive"; ValueType: string; ValueData: "Archive"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\B-Rar.Archive\DefaultIcon"; ValueType: string; ValueData: "{app}\B-Rar-GUI.exe,0"
Root: HKA; Subkey: "Software\Classes\B-Rar.Archive\shell\open\command"; ValueType: string; ValueData: """{app}\B-Rar-GUI.exe"" ""%1"""
Root: HKA; Subkey: "Software\Classes\Applications\B-Rar-GUI.exe"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "B-Rar"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Applications\B-Rar-GUI.exe\shell\open\command"; ValueType: string; ValueData: """{app}\B-Rar-GUI.exe"" ""%1"""
; The right-click submenu, the same commands as the Store version's. Subkeys are listed in name
; order, hence the numbers.
#sub ArchiveType
Root: HKA; Subkey: "Software\Classes\.{#Ext[I]}\OpenWithProgids"; ValueType: string; ValueName: "B-Rar.Archive"; ValueData: ""; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\SystemFileAssociations\.{#Ext[I]}\shell\B-Rar"; ValueType: string; ValueName: "MUIVerb"; ValueData: "B-Rar"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\SystemFileAssociations\.{#Ext[I]}\shell\B-Rar"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\B-Rar-GUI.exe,0"
Root: HKA; Subkey: "Software\Classes\SystemFileAssociations\.{#Ext[I]}\shell\B-Rar"; ValueType: string; ValueName: "SubCommands"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\SystemFileAssociations\.{#Ext[I]}\shell\B-Rar\shell\1open"; ValueType: string; ValueName: "MUIVerb"; ValueData: "Open in B-Rar"
Root: HKA; Subkey: "Software\Classes\SystemFileAssociations\.{#Ext[I]}\shell\B-Rar\shell\1open\command"; ValueType: string; ValueData: """{app}\B-Rar-GUI.exe"" ""%1"""
Root: HKA; Subkey: "Software\Classes\SystemFileAssociations\.{#Ext[I]}\shell\B-Rar\shell\2ask"; ValueType: string; ValueName: "MUIVerb"; ValueData: "Extract files..."
Root: HKA; Subkey: "Software\Classes\SystemFileAssociations\.{#Ext[I]}\shell\B-Rar\shell\2ask\command"; ValueType: string; ValueData: """{app}\B-Rar-GUI.exe"" x ""%1"" --ask"
Root: HKA; Subkey: "Software\Classes\SystemFileAssociations\.{#Ext[I]}\shell\B-Rar\shell\3folder"; ValueType: string; ValueName: "MUIVerb"; ValueData: "Extract to folder"
Root: HKA; Subkey: "Software\Classes\SystemFileAssociations\.{#Ext[I]}\shell\B-Rar\shell\3folder\command"; ValueType: string; ValueData: """{app}\B-Rar-GUI.exe"" x ""%1"""
Root: HKA; Subkey: "Software\Classes\SystemFileAssociations\.{#Ext[I]}\shell\B-Rar\shell\4here"; ValueType: string; ValueName: "MUIVerb"; ValueData: "Extract here"
Root: HKA; Subkey: "Software\Classes\SystemFileAssociations\.{#Ext[I]}\shell\B-Rar\shell\4here\command"; ValueType: string; ValueData: """{app}\B-Rar-GUI.exe"" x ""%1"" --here"
Root: HKA; Subkey: "Software\Classes\SystemFileAssociations\.{#Ext[I]}\shell\B-Rar\shell\5test"; ValueType: string; ValueName: "MUIVerb"; ValueData: "Test archive"
Root: HKA; Subkey: "Software\Classes\SystemFileAssociations\.{#Ext[I]}\shell\B-Rar\shell\5test\command"; ValueType: string; ValueData: """{app}\B-Rar-GUI.exe"" t ""%1"""
#endsub
#for {I = 0; I < DimOf(Ext); I++} ArchiveType
; PATH, for `B-Rar` in a terminal: the user's, or everyone's for an install for all users.
Root: HKCU; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; ValueData: "{olddata};{app}"; Tasks: addtopath; Check: (not IsAdminInstallMode) and NeedsAddPath('HKCU', 'Environment')
Root: HKLM; Subkey: "SYSTEM\CurrentControlSet\Control\Session Manager\Environment"; ValueType: expandsz; ValueName: "Path"; ValueData: "{olddata};{app}"; Tasks: addtopath; Check: IsAdminInstallMode and NeedsAddPath('HKLM', 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment')

[Run]
Filename: "{app}\B-Rar-GUI.exe"; Description: "{cm:LaunchProgram,B-Rar}"; Flags: nowait postinstall skipifsilent
; An update started from B-Rar (/UPDATE=1) opens it again afterwards.
Filename: "{app}\B-Rar-GUI.exe"; Flags: nowait; Check: UpdateMode('1')

[Code]
function UpdateMode(Mode: String): Boolean;
begin
  Result := ExpandConstant('{param:UPDATE|0}') = Mode;
end;

function RootKey(Root: String): Integer;
begin
  if Root = 'HKLM' then Result := HKEY_LOCAL_MACHINE else Result := HKEY_CURRENT_USER;
end;

{ Whether PATH under Root\Key lacks the install folder. }
function NeedsAddPath(Root, Key: String): Boolean;
var
  Path: String;
begin
  if not RegQueryStringValue(RootKey(Root), Key, 'Path', Path) then
  begin
    Result := True;
    exit;
  end;
  Result := Pos(';' + Uppercase(ExpandConstant('{app}')) + ';', ';' + Uppercase(Path) + ';') = 0;
end;

{ Takes the install folder back out of PATH when B-Rar is uninstalled. }
procedure RemoveFromPath(Root, Key: String);
var
  Path, App: String;
  P: Integer;
begin
  if not RegQueryStringValue(RootKey(Root), Key, 'Path', Path) then exit;
  App := ExpandConstant('{app}');
  P := Pos(';' + Uppercase(App) + ';', ';' + Uppercase(Path) + ';');
  if P = 0 then exit;
  Delete(Path, P - 1, Length(App) + 1);
  if (Length(Path) > 0) and (Path[1] = ';') then Delete(Path, 1, 1);
  RegWriteExpandStringValue(RootKey(Root), Key, 'Path', Path);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
  begin
    if IsAdminInstallMode then
      RemoveFromPath('HKLM', 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment')
    else
      RemoveFromPath('HKCU', 'Environment');
  end;
end;

[UninstallRun]
; B-Rar may be open; stop it so its files can be removed.
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM B-Rar-GUI.exe"; Flags: runhidden; RunOnceId: "StopApp"
