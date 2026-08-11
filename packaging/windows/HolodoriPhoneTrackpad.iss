; Holodori Phone Trackpad Windows installer.
; The UDP branch uses Windows' inbox RNDIS network driver. The installer does
; not provision any USB, WinUSB, or UsbDk package.

#define MyAppName "Holodori Phone Trackpad"
#define VersionFile FileOpen(AddBackslash(SourcePath) + "..\..\VERSION")
#define MyAppVersion Trim(FileRead(VersionFile))
#expr FileClose(VersionFile)
#define MyAppPublisher "Holodori Phone Trackpad contributors"
#define MyAppExeName "HolodoriPhoneTrackpad.exe"

[Setup]
AppId={{E4CF4E7F-5FE4-4D5D-97F4-D388A258D72F}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
OutputDir=..\..\release
OutputBaseFilename=HolodoriPhoneTrackpadSetup
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64os
ArchitecturesInstallIn64BitMode=x64os
PrivilegesRequired=admin
UninstallDisplayName={#MyAppName}

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked

[Files]
Source: "..\..\release\HolodoriPhoneTrackpad.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{autoprograms}\{#MyAppName} (with Touch Overlay)"; Filename: "{app}\{#MyAppExeName}"; Parameters: "--overlay"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "Launch {#MyAppName}"; Flags: nowait postinstall skipifsilent
