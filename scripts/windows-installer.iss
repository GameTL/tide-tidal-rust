; The Windows installer, TIDE-<version>-windows-x64-setup.exe (Inno Setup 6.6+).
;
;   iscc /DAppVersion=1.2.3 /DSourceDir=target\release /DOutputDir=dist scripts\windows-installer.iss
;
; Installs for the current user (no admin prompt) into
; %LOCALAPPDATA%\Programs\TIDE, adds TIDE to the Start menu and to
; Settings › Apps for uninstalling, and offers to start it. Installing a newer
; version over an older one replaces it in place (same AppId).

#ifndef AppVersion
  #error Pass the version: /DAppVersion=1.2.3
#endif
#ifndef SourceDir
  #define SourceDir "..\target\release"
#endif
#ifndef OutputDir
  #define OutputDir "..\dist"
#endif

[Setup]
; Never change the AppId: Windows uses it to recognise TIDE on upgrade and uninstall.
AppId={{98E169D9-A14C-4FAC-81D8-5D5FB6FC781E}
AppName=TIDE
AppVersion={#AppVersion}
AppVerName=TIDE {#AppVersion}
AppPublisher=Tinapat Game Limsila
AppPublisherURL=https://gametl.github.io/tide-tidal-rust/
AppSupportURL=https://github.com/GameTL/tide-tidal-rust/issues
VersionInfoVersion={#AppVersion}
DefaultDirName={autopf}\TIDE
; Per user, no UAC prompt; {autopf} then means %LOCALAPPDATA%\Programs.
PrivilegesRequired=lowest
; x64 Windows, and Windows 11 on Arm (which runs the x64 build).
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
; The fewest pages: Ready (Install), progress, Finish (with "Launch TIDE").
DisableWelcomePage=yes
DisableDirPage=yes
DisableProgramGroupPage=yes
; Light or dark, following the Windows setting.
WizardStyle=modern dynamic
SetupIconFile=..\crates\tide-app\assets\TIDE.ico
WizardSmallImageFile=..\crates\tide-app\assets\installer-58.png,..\crates\tide-app\assets\installer-116.png
UninstallDisplayIcon={app}\TIDE.exe
UninstallDisplayName=TIDE
; A running TIDE is closed for the update and started again afterwards.
CloseApplications=yes
RestartApplications=yes
OutputDir={#OutputDir}
OutputBaseFilename=TIDE-{#AppVersion}-windows-x64-setup
Compression=lzma2/max
SolidCompression=yes

[Files]
Source: "{#SourceDir}\TIDE.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\TIDE"; Filename: "{app}\TIDE.exe"

[Run]
Filename: "{app}\TIDE.exe"; Description: "Launch TIDE"; Flags: nowait postinstall skipifsilent
