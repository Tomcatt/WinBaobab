; WinBaobab installer. Build: ISCC /DAppVersion=x.y.z installer\winbaobab.iss
; (scripts\build-release.ps1 does this.)
#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif

[Setup]
; Same AppId across versions: a newer setup upgrades in place.
AppId={{6B0C5E0A-7B6D-4E52-9D0B-3F1A5C7E2B41}
AppName=WinBaobab
AppVersion={#AppVersion}
AppVerName=WinBaobab {#AppVersion}
AppPublisher=WinBaobab (unofficial; not affiliated with GNOME)
DefaultDirName={autopf}\WinBaobab
DefaultGroupName=WinBaobab
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
LicenseFile=..\COPYING
OutputDir=..\dist
OutputBaseFilename=WinBaobab-Setup-{#AppVersion}
SetupIconFile=..\assets\winbaobab.ico
UninstallDisplayIcon={app}\winbaobab.exe
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
WizardStyle=modern

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "explorermenu"; Description: "Add 'Analyze with WinBaobab' to the Explorer right-click menu"; GroupDescription: "Explorer integration:"; Flags: unchecked

[Files]
Source: "..\target\release\winbaobab.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\COPYING"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\WinBaobab"; Filename: "{app}\winbaobab.exe"
Name: "{autodesktop}\WinBaobab"; Filename: "{app}\winbaobab.exe"; Tasks: desktopicon

[Registry]
Root: HKA; Subkey: "Software\Classes\Directory\shell\WinBaobab"; ValueType: string; ValueName: ""; ValueData: "Analyze with WinBaobab"; Flags: uninsdeletekey; Tasks: explorermenu
Root: HKA; Subkey: "Software\Classes\Directory\shell\WinBaobab"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\winbaobab.exe"""; Tasks: explorermenu
Root: HKA; Subkey: "Software\Classes\Directory\shell\WinBaobab\command"; ValueType: string; ValueName: ""; ValueData: """{app}\winbaobab.exe"" ""%1"""; Tasks: explorermenu
Root: HKA; Subkey: "Software\Classes\Drive\shell\WinBaobab"; ValueType: string; ValueName: ""; ValueData: "Analyze with WinBaobab"; Flags: uninsdeletekey; Tasks: explorermenu
Root: HKA; Subkey: "Software\Classes\Drive\shell\WinBaobab"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\winbaobab.exe"""; Tasks: explorermenu
Root: HKA; Subkey: "Software\Classes\Drive\shell\WinBaobab\command"; ValueType: string; ValueName: ""; ValueData: """{app}\winbaobab.exe"" ""%1"""; Tasks: explorermenu

[Run]
Filename: "{app}\winbaobab.exe"; Description: "Launch WinBaobab"; Flags: nowait postinstall skipifsilent

[Code]
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  // Silent uninstalls always keep settings (no prompt, nothing deleted).
  if (CurUninstallStep = usPostUninstall) and not UninstallSilent then
  begin
    if MsgBox('Also remove your WinBaobab settings?', mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
      DelTree(ExpandConstant('{userappdata}\WinBaobab'), True, True, True);
  end;
end;
