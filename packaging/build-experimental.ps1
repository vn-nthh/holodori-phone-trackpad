[CmdletBinding()]
param(
    [string]$Name = "HolodoriLosslessTouch-v0.4.0-alpha9",
    [string]$CargoHome = "F:\.cargo",
    [string]$JavaHome = "",
    [string]$AndroidSdk = ""
)

$ErrorActionPreference = "Stop"
$ProjectRoot = Split-Path -Parent $PSScriptRoot
$ReleaseRoot = Join-Path $ProjectRoot "release"
$BundleDir = Join-Path $ReleaseRoot $Name
$ArchivePath = Join-Path $ReleaseRoot "$Name-windows-x64.zip"

if (Test-Path $BundleDir) {
    throw "Experimental bundle already exists: $BundleDir"
}
if (Test-Path $ArchivePath) {
    throw "Experimental archive already exists: $ArchivePath"
}

if (-not $JavaHome) {
    $JavaHome = Join-Path $ProjectRoot ".android-sdk\jdk17\jdk-17.0.20+8"
}
if (-not $AndroidSdk) {
    $AndroidSdk = Join-Path $ProjectRoot ".android-sdk"
}

$env:CARGO_HOME = (Resolve-Path $CargoHome).Path
$env:JAVA_HOME = (Resolve-Path $JavaHome).Path
$env:ANDROID_HOME = (Resolve-Path $AndroidSdk).Path
$env:ANDROID_SDK_ROOT = $env:ANDROID_HOME
$env:RUSTFLAGS = "-C target-feature=+crt-static"

Push-Location (Join-Path $ProjectRoot "native-host")
try {
    cargo test --all-targets
    if ($LASTEXITCODE -ne 0) {
        throw "Native tests failed."
    }
    cargo build --release
    if ($LASTEXITCODE -ne 0) {
        throw "Native release build failed."
    }
}
finally {
    Pop-Location
}

$AndroidDir = Join-Path $ProjectRoot "android-app"
& (Join-Path $AndroidDir "gradlew.bat") `
    --project-dir $AndroidDir `
    --no-daemon `
    "-PholodoriVersionName=0.4.0-alpha9" `
    "-PholodoriVersionCode=16" `
    clean assembleRelease
if ($LASTEXITCODE -ne 0) {
    throw "Android release build failed."
}

$Apk = Join-Path $AndroidDir "app\build\outputs\apk\release\app-release.apk"
$BuildTools = Get-ChildItem (Join-Path $env:ANDROID_HOME "build-tools") -Directory |
    Sort-Object Name -Descending |
    Select-Object -First 1
$ApkSigner = Join-Path $BuildTools.FullName "apksigner.bat"
& $ApkSigner verify --verbose $Apk
if ($LASTEXITCODE -ne 0) {
    throw "The Android release APK is not installably signed."
}

$WindowsDir = Join-Path $BundleDir "Windows"
$AndroidOutputDir = Join-Path $BundleDir "Android"
$DriversDir = Join-Path $BundleDir "Drivers"
$DocsDir = Join-Path $BundleDir "Docs"
New-Item -ItemType Directory -Path @(
    $WindowsDir,
    $AndroidOutputDir,
    $DriversDir,
    $DocsDir
) | Out-Null

$NativeRelease = Join-Path $ProjectRoot "native-host\target\release"
Copy-Item (Join-Path $NativeRelease "holodori-native-host.exe") $WindowsDir
Copy-Item (Join-Path $NativeRelease "holodori-touch-probe.exe") $WindowsDir
Copy-Item (Join-Path $NativeRelease "holodori-touch-smoke.exe") $WindowsDir
Copy-Item $Apk (Join-Path $AndroidOutputDir "HolodoriLosslessTouch-v4-experimental.apk")
Copy-Item (Join-Path $ProjectRoot "packaging\third_party\usbdk\UsbDk_1.0.22_x64.msi") $DriversDir
Copy-Item (Join-Path $ProjectRoot "packaging\third_party\usbdk\LICENSE") $DriversDir
Copy-Item (Join-Path $ProjectRoot "packaging\experimental\README.txt") $BundleDir
Copy-Item (Join-Path $ProjectRoot "packaging\experimental\run-touch.cmd") $BundleDir
Copy-Item (Join-Path $ProjectRoot "packaging\experimental\run-keys.cmd") $BundleDir
Copy-Item (Join-Path $ProjectRoot "EXPERIMENTAL_ARCHITECTURE.md") $DocsDir
Copy-Item (Join-Path $ProjectRoot "PROTOCOL_V4.md") $DocsDir
Copy-Item (Join-Path $ProjectRoot "LICENSE") $DocsDir

$Branch = git -C $ProjectRoot branch --show-current
$Commit = git -C $ProjectRoot rev-parse HEAD
$Dirty = if (git -C $ProjectRoot status --porcelain) { "yes" } else { "no" }
$BuildInfo = @(
    "name=$Name",
    "android_version_name=0.4.0-alpha9",
    "android_version_code=16",
    "built_utc=$([DateTime]::UtcNow.ToString('o'))",
    "branch=$Branch",
    "base_commit=$Commit",
    "working_tree_dirty=$Dirty",
    "protocol=4",
    "windows_arch=x86_64",
    "windows_crt=static",
    "android_signing=debug-key experimental"
)
$BuildInfo | Set-Content -Encoding UTF8 (Join-Path $BundleDir "BUILD-INFO.txt")

$ChecksumPath = Join-Path $BundleDir "SHA256SUMS.txt"
$Checksums = Get-ChildItem $BundleDir -Recurse -File |
    Where-Object FullName -ne $ChecksumPath |
    Sort-Object FullName |
    ForEach-Object {
        $Relative = $_.FullName.Substring($BundleDir.Length).
            TrimStart("\").
            Replace("\", "/")
        "$((Get-FileHash -Algorithm SHA256 $_.FullName).Hash)  $Relative"
    }
$Checksums | Set-Content -Encoding ASCII $ChecksumPath

Compress-Archive -Path $BundleDir -DestinationPath $ArchivePath -CompressionLevel Optimal

Write-Host "Bundle: $BundleDir"
Write-Host "Archive: $ArchivePath"
