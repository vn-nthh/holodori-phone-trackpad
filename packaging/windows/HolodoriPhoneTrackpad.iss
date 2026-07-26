; Holodori Phone Trackpad Windows installer.
; UsbDk is installed separately from the app so uninstalling Holodori never
; removes a USB driver another application may still use.

#define MyAppName "Holodori Phone Trackpad"
#define MyAppVersion "0.1.1"
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
Name: "usbdk"; Description: "Install UsbDk USB connection support (recommended)"; GroupDescription: "USB connection support:"; Flags: checkedonce
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked

[Files]
Source: "..\..\release\HolodoriPhoneTrackpad.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\third_party\usbdk\UsbDk_1.0.22_x64.msi"; DestDir: "{tmp}"; Flags: deleteafterinstall dontcopy
Source: "..\third_party\usbdk\LICENSE"; DestDir: "{app}\licenses"; DestName: "UsbDk-Apache-2.0.txt"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
Filename: "{sys}\msiexec.exe"; Parameters: "/i ""{tmp}\UsbDk_1.0.22_x64.msi"" /passive /norestart"; StatusMsg: "Installing UsbDk USB connection support…"; Flags: waituntilterminated; Tasks: usbdk; Check: NeedsUsbDk
Filename: "{app}\{#MyAppExeName}"; Description: "Launch {#MyAppName}"; Flags: nowait postinstall skipifsilent

[Code]
function IsUsbDkInstalled(): Boolean;
begin
  Result := RegKeyExists(HKLM, 'SYSTEM\CurrentControlSet\Services\UsbDk');
end;

function NeedsUsbDk(): Boolean;
begin
  Result := not IsUsbDkInstalled();
end;
