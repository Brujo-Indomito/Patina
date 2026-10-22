; Inno Setup script for Rust VCam. Build with scripts\package.ps1, which
; stages the files into target\package and passes AppVersion.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{11CB227C-E8D0-40E6-ACE4-B0B195A4A608}
AppName=Rust VCam
AppVersion={#AppVersion}
AppVerName=Rust VCam {#AppVersion}
AppPublisher=Rust VCam
DefaultDirName={autopf}\Rust VCam
DefaultGroupName=Rust VCam
DisableProgramGroupPage=yes
; Registering the camera DLL for the Windows camera service needs admin rights.
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; MFCreateVirtualCamera needs Windows 11.
MinVersion=10.0.22000
OutputDir=..\target\installer
OutputBaseFilename=RustVCam-Setup-{#AppVersion}
SetupIconFile=..\target\package\rust-vcam.ico
UninstallDisplayIcon={app}\rust-vcam.exe
UninstallDisplayName=Rust VCam
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; Ask to close a running Rust VCam (the app holds this mutex).
AppMutex=Local\RustVCam_Instance
InfoBeforeFile=..\installer\notice.txt

[Tasks]
Name: desktopicon; Description: "Create a desktop shortcut"; GroupDescription: "Shortcuts:"
Name: startup; Description: "Start Rust VCam with Windows (in the tray)"; GroupDescription: "Startup:"; Flags: unchecked

[Files]
Source: "..\target\package\rust_vcam_source.dll"; DestDir: "{app}"; Flags: ignoreversion regserver 64bit
Source: "..\target\package\*"; DestDir: "{app}"; Excludes: "rust_vcam_source.dll,rust-vcam.ico"; Flags: ignoreversion recursesubdirs

[Dirs]
; The camera service (LocalService) writes its log here; see [Run] for the ACL.
Name: "{commonappdata}\RustVCam"

[UninstallDelete]
Type: filesandordirs; Name: "{commonappdata}\RustVCam"

[Icons]
Name: "{autoprograms}\Rust VCam"; Filename: "{app}\rust-vcam.exe"
Name: "{autodesktop}\Rust VCam"; Filename: "{app}\rust-vcam.exe"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "RustVCam"; ValueData: """{app}\rust-vcam.exe"" --minimized"; Tasks: startup

[Run]
Filename: "{sys}\icacls.exe"; Parameters: """{commonappdata}\RustVCam"" /grant *S-1-5-19:(OI)(CI)M"; Flags: runhidden
Filename: "{app}\rust-vcam.exe"; Description: "Start Rust VCam"; Flags: nowait postinstall skipifsilent runasoriginaluser

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
    settings stick); unregister it while vcam-app.exe and the camera DLL are
    still here, or a dead "Rust VCam" device stays behind. }
  if not Exec(ExpandConstant('{app}\vcam-app.exe'), '--remove-camera', '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
    Log('Could not run vcam-app.exe --remove-camera')
  else
    Log(Format('vcam-app.exe --remove-camera exited with %d', [ResultCode]));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
  begin
    RemoveCamera();
    StopCameraServices();
    { Also remove the autostart entry if it was turned on from the app. }
    RegDeleteValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run', 'RustVCam');
  end;
end;
