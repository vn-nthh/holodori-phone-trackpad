; Holodori Phone Trackpad Windows installer.
; WinUSB and UsbDk support are installed separately from the app so
; uninstalling Holodori never removes USB support another application may use.

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
Name: "winusb"; Description: "Install WinUSB low-latency data support (recommended)"; GroupDescription: "USB connection support:"; Flags: checkedonce
Name: "usbdk"; Description: "Install UsbDk handshake and fallback support (recommended)"; GroupDescription: "USB connection support:"; Flags: checkedonce
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked

[Files]
Source: "..\..\release\HolodoriPhoneTrackpad.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\third_party\usbdk\UsbDk_1.0.22_x64.msi"; DestDir: "{tmp}"; Flags: deleteafterinstall
Source: "..\third_party\usbdk\LICENSE"; DestDir: "{app}\licenses"; DestName: "UsbDk-Apache-2.0.txt"; Flags: ignoreversion
Source: "..\third_party\libwdi\wdi-simple.exe"; DestDir: "{tmp}"; Flags: deleteafterinstall
Source: "..\third_party\libwdi\COPYING-LGPL"; DestDir: "{app}\licenses\libwdi"; DestName: "COPYING-LGPL.txt"; Flags: ignoreversion
Source: "..\third_party\libwdi\libwdi-v1.5.1-source.zip"; DestDir: "{app}\licenses\libwdi"; Flags: ignoreversion
Source: "..\third_party\libwdi\BUILDING.md"; DestDir: "{app}\licenses\libwdi"; Flags: ignoreversion
Source: "..\third_party\libwdi\holodori-build.patch"; DestDir: "{app}\licenses\libwdi"; Flags: ignoreversion
Source: "..\third_party\libwdi\Microsoft-WDK-License.rtf"; DestDir: "{app}\licenses\libwdi"; Flags: ignoreversion
Source: "..\third_party\libwdi\Microsoft-WDK-redist.txt"; DestDir: "{app}\licenses\libwdi"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{autoprograms}\{#MyAppName} (with Touch Overlay)"; Filename: "{app}\{#MyAppExeName}"; Parameters: "--overlay"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
Filename: "{sys}\msiexec.exe"; Parameters: "/i ""{tmp}\UsbDk_1.0.22_x64.msi"" /passive /norestart"; StatusMsg: "Installing UsbDk handshake and fallback support..."; Flags: waituntilterminated; Tasks: usbdk; Check: NeedsUsbDk
Filename: "{app}\{#MyAppExeName}"; Description: "Launch {#MyAppName}"; Flags: nowait postinstall skipifsilent

[Code]
function InstallWinUsbPackage(
  ProductId: String;
  InterfaceArguments: String;
  InfName: String;
  DeviceName: String
): Boolean;
var
  Destination: String;
  Parameters: String;
  ResultCode: Integer;
begin
  Destination := ExpandConstant('{tmp}\holodori-winusb-' + ProductId);
  Parameters :=
    '--silent --log 1 --type 0 --manufacturer "Holodori" --name "' +
    DeviceName + '" --vid 0x18D1 --pid 0x' + ProductId +
    InterfaceArguments + ' --inf "' + InfName + '" --dest "' +
    Destination + '"';
  Log('Provisioning WinUSB package for 18D1:' + ProductId +
    InterfaceArguments);
  Result :=
    Exec(
      ExpandConstant('{tmp}\wdi-simple.exe'),
      Parameters,
      ExpandConstant('{tmp}'),
      SW_HIDE,
      ewWaitUntilTerminated,
      ResultCode
    ) and (ResultCode = 0);
  if not Result then
    Log(
      'WinUSB provisioning for 18D1:' + ProductId +
      ' failed with exit code ' + IntToStr(ResultCode) + '.'
    );
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  StandardAccessoryInstalled: Boolean;
  AdbAccessoryInstalled: Boolean;
begin
  if (CurStep <> ssPostInstall) or
    (not WizardIsTaskSelected('winusb')) then
    Exit;

  WizardForm.StatusLabel.Caption :=
    'Installing WinUSB low-latency data support...';
  StandardAccessoryInstalled :=
    InstallWinUsbPackage(
      '2D00',
      '',
      'holodori-aoa.inf',
      'Holodori Android Accessory'
    );
  AdbAccessoryInstalled :=
    InstallWinUsbPackage(
      '2D01',
      ' --iid 0',
      'holodori-aoa-adb.inf',
      'Holodori Android Accessory (ADB)'
    );

  if StandardAccessoryInstalled and AdbAccessoryInstalled then
    Log('WinUSB support was provisioned for both Android Accessory modes.')
  else
    SuppressibleMsgBox(
      'WinUSB low-latency support could not be fully installed.' +
      Chr(13) + Chr(10) + Chr(13) + Chr(10) +
      'Holodori will continue with UsbDk compatibility mode when UsbDk ' +
      'is installed.',
      mbError,
      MB_OK,
      IDOK
    );
end;

function IsUsbDkInstalled(): Boolean;
begin
  Result := RegKeyExists(HKLM, 'SYSTEM\CurrentControlSet\Services\UsbDk');
end;

function NeedsUsbDk(): Boolean;
begin
  Result := not IsUsbDkInstalled();
end;
