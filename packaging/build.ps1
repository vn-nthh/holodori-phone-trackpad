[CmdletBinding()]
param(
    [ValidateSet("All", "Windows", "Android")]
    [string]$Target = "All",
    [string]$AndroidSdk = $env:ANDROID_SDK_ROOT,
    [string]$JavaHome = $env:JAVA_HOME,
    [string]$InnoSetupCompiler = ""
)

$ErrorActionPreference = "Stop"
$ProjectRoot = Split-Path -Parent $PSScriptRoot
$ReleaseDir = Join-Path $ProjectRoot "release"
$UsbDkInstaller = Join-Path $ProjectRoot "packaging\third_party\usbdk\UsbDk_1.0.22_x64.msi"
$UsbDkSha256 = "91F6F695E1E13C656024E6D3B55620BF08D8835EF05EE0496935BA6BB62466A5"
$LibwdiDir = Join-Path $ProjectRoot "packaging\third_party\libwdi"
$BundledWinUsbFiles = @(
    @{
        Path = Join-Path $LibwdiDir "wdi-simple.exe"
        Sha256 = "5EEE1919EF07989BA8B54C199D66DAC93F90811D239FC49CBB8BF9C43A07BCC8"
    },
    @{
        Path = Join-Path $LibwdiDir "libwdi-v1.5.1-source.zip"
        Sha256 = "746547AAF927CAE44C75512D763941805928427F4BA4DF3DBB40C3F7F561821E"
    },
    @{
        Path = Join-Path $LibwdiDir "COPYING-LGPL"
        Sha256 = "EA7D049C7705DC13AFC202DD18E1827F3484F8212FD3FA7B82FC4A0C363432C9"
    },
    @{
        Path = Join-Path $LibwdiDir "Microsoft-WDK-License.rtf"
        Sha256 = "68421CBF5AFF522E2660D812220458B475DFEE6D2E66363CF66C7144E956529E"
    },
    @{
        Path = Join-Path $LibwdiDir "Microsoft-WDK-redist.txt"
        Sha256 = "8D567A02B1EEC44FCE2C0FC492C8C1234CD29FADA9FB7C578BD03BF3F97885A2"
    }
)

New-Item -ItemType Directory -Force -Path $ReleaseDir | Out-Null

if ($Target -in @("All", "Windows")) {
    if (-not (Test-Path $UsbDkInstaller)) {
        throw "Missing bundled UsbDk installer: $UsbDkInstaller"
    }
    if ((Get-FileHash -Algorithm SHA256 $UsbDkInstaller).Hash -ne $UsbDkSha256) {
        throw "Bundled UsbDk installer checksum does not match the verified upstream asset."
    }
    foreach ($BundledFile in $BundledWinUsbFiles) {
        if (-not (Test-Path $BundledFile.Path)) {
            throw "Missing bundled WinUSB support file: $($BundledFile.Path)"
        }
        if (
            (Get-FileHash -Algorithm SHA256 $BundledFile.Path).Hash -ne
            $BundledFile.Sha256
        ) {
            throw "Bundled WinUSB support file checksum does not match: $($BundledFile.Path)"
        }
    }

    $VenvDir = Join-Path $ProjectRoot ".venv-package"
    $VenvPython = Join-Path $VenvDir "Scripts\python.exe"

    if (-not (Test-Path $VenvPython)) {
        python -m venv $VenvDir
        if ($LASTEXITCODE -ne 0) {
            throw "Could not create the Windows packaging environment."
        }
    }

    & $VenvPython -m pip install --disable-pip-version-check -r `
        (Join-Path $ProjectRoot "requirements-build.txt")
    if ($LASTEXITCODE -ne 0) {
        throw "Could not install the Windows packaging dependencies."
    }

    & $VenvPython -m PyInstaller --noconfirm --clean `
        --distpath $ReleaseDir `
        --workpath (Join-Path $ProjectRoot "build\pyinstaller") `
        (Join-Path $ProjectRoot "packaging\windows\HolodoriPhoneTrackpad.spec")
    if ($LASTEXITCODE -ne 0) {
        throw "PyInstaller failed to build the Windows package."
    }

    $ExePath = Join-Path $ReleaseDir "HolodoriPhoneTrackpad.exe"
    if (-not (Test-Path $ExePath)) {
        throw "Windows build completed without producing $ExePath"
    }
    Write-Host "Windows package: $ExePath"

    if (-not $InnoSetupCompiler) {
        $InnoSetupCompiler = Join-Path ${env:ProgramFiles(x86)} "Inno Setup 6\ISCC.exe"
    }
    if (-not (Test-Path $InnoSetupCompiler)) {
        throw "Inno Setup 6 is required to build the Windows installer. Install it or pass -InnoSetupCompiler."
    }

    & $InnoSetupCompiler (Join-Path $ProjectRoot "packaging\windows\HolodoriPhoneTrackpad.iss")
    if ($LASTEXITCODE -ne 0) {
        throw "Inno Setup failed to build the Windows installer."
    }

    $SetupPath = Join-Path $ReleaseDir "HolodoriPhoneTrackpadSetup.exe"
    if (-not (Test-Path $SetupPath)) {
        throw "Windows package completed without producing $SetupPath"
    }
    Write-Host "Windows setup: $SetupPath"
}

if ($Target -in @("All", "Android")) {
    if (-not $JavaHome) {
        throw "Set JAVA_HOME or pass -JavaHome with a JDK 17 or newer."
    }
    if (-not $AndroidSdk) {
        throw "Set ANDROID_SDK_ROOT or pass -AndroidSdk with an Android SDK."
    }

    $env:JAVA_HOME = (Resolve-Path $JavaHome).Path
    $env:ANDROID_HOME = (Resolve-Path $AndroidSdk).Path
    $env:ANDROID_SDK_ROOT = $env:ANDROID_HOME

    $AndroidDir = Join-Path $ProjectRoot "android-app"
    & (Join-Path $AndroidDir "gradlew.bat") `
        --project-dir $AndroidDir `
        --no-daemon `
        assembleRelease
    if ($LASTEXITCODE -ne 0) {
        throw "Gradle failed to build the Android package."
    }

    $BuiltApk = Join-Path $AndroidDir "app\build\outputs\apk\release\app-release.apk"
    if (-not (Test-Path $BuiltApk)) {
        throw "Android build completed without producing $BuiltApk"
    }

    $ApkPath = Join-Path $ReleaseDir "HolodoriPhoneTrackpad.apk"
    Copy-Item -Force $BuiltApk $ApkPath
    Write-Host "Android package: $ApkPath"
}
