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
New-Item -ItemType Directory -Force -Path $ReleaseDir | Out-Null

if ($Target -in @("All", "Windows")) {
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
