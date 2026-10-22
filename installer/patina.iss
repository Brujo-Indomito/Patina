; Inno Setup script for Patina. Build with scripts\package.ps1, which
; stages the files into target\package and passes AppVersion.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{C865E49F-656A-4B4D-AAC2-065C4ABD54CC}
AppName=Patina
AppVersion={#AppVersion}
AppVerName=Patina {#AppVersion}
AppPublisher=Patina
DefaultDirName={autopf}\Patina
DefaultGroupName=Patina
DisableProgramGroupPage=yes
; Registering the camera DLL for the Windows camera service needs admin rights.
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; MFCreateVirtualCamera needs Windows 11.
MinVersion=10.0.22000
OutputDir=..\target\installer
OutputBaseFilename=Patina-Setup-{#AppVersion}
SetupIconFile=..\target\package\patina.ico
UninstallDisplayIcon={app}\patina.exe
UninstallDisplayName=Patina
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; Ask to close a running Patina (the app holds this mutex).
AppMutex=Local\Patina_Instance
InfoBeforeFile=..\installer\notice.txt

[Tasks]
Name: desktopicon; Description: "Create a desktop shortcut"; GroupDescription: "Shortcuts:"
Name: startup; Description: "Start Patina with Windows (in the tray)"; GroupDescription: "Startup:"; Flags: unchecked

[Files]
Source: "..\target\package\patina_source.dll"; DestDir: "{app}"; Flags: ignoreversion regserver 64bit
Source: "..\target\package\*"; DestDir: "{app}"; Excludes: "patina_source.dll,patina.ico"; Flags: ignoreversion recursesubdirs

[Dirs]
; The camera service (LocalService) writes its log here; see [Run] for the ACL.
Name: "{commonappdata}\Patina"

[UninstallDelete]
Type: filesandordirs; Name: "{commonappdata}\Patina"

[Icons]
Name: "{autoprograms}\Patina"; Filename: "{app}\patina.exe"
Name: "{autodesktop}\Patina"; Filename: "{app}\patina.exe"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "Patina"; ValueData: """{app}\patina.exe"" --minimized"; Tasks: startup

[Run]
Filename: "{sys}\icacls.exe"; Parameters: """{commonappdata}\Patina"" /grant *S-1-5-19:(OI)(CI)M"; Flags: runhidden
Filename: "{app}\patina.exe"; Description: "Start Patina"; Flags: nowait postinstall skipifsilent runasoriginaluser

[Code]
procedure StopCameraServices();
var
  ResultCode: Integer;
begin
  { The Windows camera service keeps the camera DLL loaded; stop it so the
    file can be replaced or removed. It restarts on demand. }
  Exec(ExpandConstant('{sys}\sc.exe'), 'stop FrameServerMonitor', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Exec(ExpandConstant('{sys}\sc.exe'), 'stop FrameServer', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Sleep(1500);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  StopCameraServices();
  Result := '';
end;

procedure RemoveCamera();
var
  ResultCode: Integer;
begin
  { The camera device outlives the app (system lifetime, so its Windows
    settings stick); unregister it while patina-cli.exe and the camera DLL are
    still here, or a dead "Patina" device stays behind. }
  if not Exec(ExpandConstant('{app}\patina-cli.exe'), '--remove-camera', '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
    Log('Could not run patina-cli.exe --remove-camera')
  else
    Log(Format('patina-cli.exe --remove-camera exited with %d', [ResultCode]));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
  begin
    RemoveCamera();
    StopCameraServices();
    { Also remove the autostart entry if it was turned on from the app. }
    RegDeleteValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run', 'Patina');
  end;
end;
