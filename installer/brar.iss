; brar installer (Inno Setup 6). Build with installer\build.ps1, which passes AppVersion.
;
; Per-user install, no admin prompt. Adds a "brar" submenu to the Explorer right-click menu
; of every supported archive type (on Windows 11 it appears under "Show more options").

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#define AppExe "brar.exe"
#define GuiExe "brar-gui.exe"

; Extensions that get the right-click menu.
#dim Exts[13] {"zip", "7z", "rar", "tar", "gz", "tgz", "xz", "txz", "zst", "tzst", "bz2", "tbz", "tbz2"}

[Setup]
AppId={{6F1C2B9E-4E7A-4C1D-9B2E-8B7D3A5C0B11}
AppName=brar
AppVersion={#AppVersion}
AppVerName=brar {#AppVersion}
AppPublisher=brar
DefaultDirName={autopf}\brar
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\dist
OutputBaseFilename=brar-setup-{#AppVersion}
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
SetupIconFile=..\assets\brar.ico
ChangesAssociations=yes
ChangesEnvironment=yes
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName=brar {#AppVersion}

[Tasks]
Name: contextmenu; Description: "Add brar to the right-click menu of archives"
Name: addtopath; Description: "Add brar to PATH (use it from any terminal)"

[Files]
Source: "..\target\release\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\{#GuiExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "unrar-license.txt"; DestDir: "{app}"; Flags: ignoreversion

#define I
#sub EmitMenu
  #define Key "Software\Classes\SystemFileAssociations\." + Exts[I] + "\shell\brar"
Root: HKCU; Subkey: "{#Key}"; ValueType: string; ValueName: "MUIVerb"; ValueData: "brar"; Flags: uninsdeletekey; Tasks: contextmenu
Root: HKCU; Subkey: "{#Key}"; ValueType: string; ValueName: "SubCommands"; ValueData: ""; Tasks: contextmenu
Root: HKCU; Subkey: "{#Key}"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\{#GuiExe},0"; Tasks: contextmenu
Root: HKCU; Subkey: "{#Key}\shell\1ask"; ValueType: string; ValueName: ""; ValueData: "Extract files..."; Tasks: contextmenu
Root: HKCU; Subkey: "{#Key}\shell\1ask"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\{#GuiExe},0"; Tasks: contextmenu
Root: HKCU; Subkey: "{#Key}\shell\1ask\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#GuiExe}"" x ""%1"" --ask"; Tasks: contextmenu
Root: HKCU; Subkey: "{#Key}\shell\2folder"; ValueType: string; ValueName: ""; ValueData: "Extract to folder"; Tasks: contextmenu
Root: HKCU; Subkey: "{#Key}\shell\2folder"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\{#GuiExe},0"; Tasks: contextmenu
Root: HKCU; Subkey: "{#Key}\shell\2folder\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#GuiExe}"" x ""%1"""; Tasks: contextmenu
Root: HKCU; Subkey: "{#Key}\shell\3here"; ValueType: string; ValueName: ""; ValueData: "Extract here"; Tasks: contextmenu
Root: HKCU; Subkey: "{#Key}\shell\3here"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\{#GuiExe},0"; Tasks: contextmenu
Root: HKCU; Subkey: "{#Key}\shell\3here\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#GuiExe}"" x ""%1"" --here"; Tasks: contextmenu
Root: HKCU; Subkey: "{#Key}\shell\4test"; ValueType: string; ValueName: ""; ValueData: "Test archive"; Tasks: contextmenu
Root: HKCU; Subkey: "{#Key}\shell\4test"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\{#GuiExe},0"; Tasks: contextmenu
Root: HKCU; Subkey: "{#Key}\shell\4test\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#GuiExe}"" t ""%1"""; Tasks: contextmenu
#endsub

[Registry]
#for {I = 0; I < DimOf(Exts); I++} EmitMenu
Root: HKCU; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; ValueData: "{olddata};{app}"; Check: NeedsAddPath(ExpandConstant('{app}')); Tasks: addtopath

[Code]
function PathContains(Paths, Dir: string): Boolean;
begin
  Result := Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Paths) + ';') > 0;
end;

function NeedsAddPath(Dir: string): Boolean;
var
  Paths: string;
begin
  if not RegQueryStringValue(HKCU, 'Environment', 'Path', Paths) then
    Paths := '';
  Result := not PathContains(Paths, Dir);
end;

// Removes the install directory from the user PATH on uninstall.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Paths, Dir: string;
  P: Integer;
begin
  if CurUninstallStep <> usPostUninstall then
    exit;
  if not RegQueryStringValue(HKCU, 'Environment', 'Path', Paths) then
    exit;
  Dir := ExpandConstant('{app}');
  Paths := ';' + Paths + ';';
  P := Pos(';' + Uppercase(Dir) + ';', Uppercase(Paths));
  if P = 0 then
    exit;
  Delete(Paths, P, Length(Dir) + 1);
  Paths := Copy(Paths, 2, Length(Paths) - 2);
  RegWriteExpandStringValue(HKCU, 'Environment', 'Path', Paths);
end;
