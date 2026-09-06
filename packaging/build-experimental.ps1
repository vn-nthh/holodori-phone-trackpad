[CmdletBinding()]
param(
    [string]$Name = "Doritrack-protocol-v5-dev",
    [string]$CargoHome = "F:\.cargo",
    [string]$JavaHome = "",
    [string]$AndroidSdk = ""
)

$ErrorActionPreference = "Stop"
$ProjectRoot = Split-Path -Parent $PSScriptRoot
$Version = (Get-Content (Join-Path $ProjectRoot "VERSION") -Raw).Trim()
$ReleaseRoot = Join-Path $ProjectRoot "release"
$BundleDir = Join-Path $ReleaseRoot $Name
$ArchivePath = Join-Path $ReleaseRoot "$Name-windows-x64.zip"
$StandaloneApkPath = Join-Path $ReleaseRoot "$Name-android.apk"

if (Test-Path $BundleDir) {
    throw "Experimental bundle already exists: $BundleDir"
}
if (Test-Path $ArchivePath) {
    throw "Experimental archive already exists: $ArchivePath"
}
if (Test-Path $StandaloneApkPath) {
    throw "Experimental Android package already exists: $StandaloneApkPath"
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
    cargo test --locked --all-targets
    if ($LASTEXITCODE -ne 0) {
        throw "Native tests failed."
    }
    cargo clippy --locked --all-targets -- -D warnings
    if ($LASTEXITCODE -ne 0) {
        throw "Native clippy checks failed."
    }
    cargo test --locked --release --lib `
        network::tests::loopback_fault_recovery_stays_inside_one_120_hz_frame `
        -- --ignored --exact
    if ($LASTEXITCODE -ne 0) {
        throw "Protocol-v4 loopback recovery check failed."
    }
    cargo test --locked --release --lib `
        v5_host::gameplay_tests::production_loopback_latency `
        -- --ignored --exact --nocapture --test-threads=1
    if ($LASTEXITCODE -ne 0) {
        throw "Protocol-v5 loopback recovery check failed."
    }
    cargo build --locked --release
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
    "-PholodoriVersionName=$Version" `
    "-PholodoriVersionCode=28" `
    clean testDebugUnitTest assembleDebug assembleRelease lintDebug lintRelease
if ($LASTEXITCODE -ne 0) {
    throw "Android release build failed."
}

$TauriDir = Join-Path $ProjectRoot "tauri-launcher"
Push-Location $TauriDir
try {
    npm ci --no-audit --no-fund
    if ($LASTEXITCODE -ne 0) {
        throw "Tauri dependency install failed."
    }
    npm test
    if ($LASTEXITCODE -ne 0) {
        throw "Tauri frontend tests failed."
    }
    npm run build
    if ($LASTEXITCODE -ne 0) {
        throw "Tauri frontend build failed."
    }
    cargo test --locked --manifest-path src-tauri\Cargo.toml --all-targets
    if ($LASTEXITCODE -ne 0) {
        throw "Tauri tests failed."
    }
    cargo clippy --locked --manifest-path src-tauri\Cargo.toml --all-targets -- -D warnings
    if ($LASTEXITCODE -ne 0) {
        throw "Tauri clippy checks failed."
    }
    npx --no-install tauri build --no-bundle
    if ($LASTEXITCODE -ne 0) {
        throw "Tauri Windows build failed."
    }
}
finally {
    Pop-Location
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
$DocsDir = Join-Path $BundleDir "Docs"
New-Item -ItemType Directory -Path @(
    $WindowsDir,
    $AndroidOutputDir,
    $DocsDir
) | Out-Null

$NativeRelease = Join-Path $ProjectRoot "native-host\target\release"
$TauriRelease = Join-Path $TauriDir "src-tauri\target\release"
Copy-Item (Join-Path $NativeRelease "holodori-native-host.exe") $WindowsDir
Copy-Item (Join-Path $NativeRelease "holodori-touch-probe.exe") $WindowsDir
Copy-Item (Join-Path $NativeRelease "holodori-touch-smoke.exe") $WindowsDir
Copy-Item (Join-Path $TauriRelease "holodori-usb-controller.exe") (Join-Path $BundleDir "HolodoriUsbController.exe")
Copy-Item $Apk (Join-Path $AndroidOutputDir "Doritrack-v5.apk")
Copy-Item $Apk $StandaloneApkPath
Copy-Item (Join-Path $ProjectRoot "packaging\experimental\README.txt") $BundleDir
Copy-Item (Join-Path $ProjectRoot "packaging\experimental\run-touch.cmd") $BundleDir
Copy-Item (Join-Path $ProjectRoot "packaging\experimental\run-keys.cmd") $BundleDir
Copy-Item (Join-Path $ProjectRoot "EXPERIMENTAL_ARCHITECTURE.md") $DocsDir
Copy-Item (Join-Path $ProjectRoot "PROTOCOL_V5.md") $DocsDir
Copy-Item (Join-Path $ProjectRoot "PROTOCOL_V5_TEST_VECTORS.md") $DocsDir
Copy-Item (Join-Path $ProjectRoot "PROTOCOL_V4.md") $DocsDir
Copy-Item (Join-Path $ProjectRoot "LATENCY_VALIDATION.md") $DocsDir
Copy-Item (Join-Path $ProjectRoot "LICENSE") $DocsDir

$Branch = git -C $ProjectRoot branch --show-current
$Commit = git -C $ProjectRoot rev-parse HEAD
$TrackedChanges = git -C $ProjectRoot status --porcelain --untracked-files=no
$Dirty = if ($TrackedChanges) { "yes" } else { "no" }
$BuildInfo = @(
    "name=$Name",
    "android_version_name=$Version",
    "android_version_code=27",
    "built_utc=$([DateTime]::UtcNow.ToString('o'))",
    "branch=$Branch",
    "base_commit=$Commit",
    "working_tree_dirty=$Dirty",
    "transport=explicit-usb-tether-or-local-network-udp",
    "udp_port=42825",
    "protocol=5",
    "legacy_protocol=4-usb-explicit",
    "windows_arch=x86_64",
    "windows_crt=static",
    "windows_launcher=tauri",
    "windows_webview2=system-runtime",
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
Write-Host "Android package: $StandaloneApkPath"
