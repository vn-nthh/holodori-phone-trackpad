[CmdletBinding()]
param(
    [string]$Name
)

$ErrorActionPreference = "Stop"
$ProjectRoot = Split-Path -Parent $PSScriptRoot
$Version = (Get-Content -LiteralPath (Join-Path $ProjectRoot "VERSION") -Raw).Trim()
if ($Version -notmatch '^\d+\.\d+\.\d+$') {
    throw "Microsoft Store packaging is stable-only. VERSION is '$Version'."
}
if ($env:GITHUB_REF_NAME -and $env:GITHUB_REF_NAME -ne "v$Version") {
    throw "Tag '$($env:GITHUB_REF_NAME)' does not match VERSION '$Version'."
}
if (-not $Name) {
    $Name = "Doritrack-v$Version"
}

$StoreConfig = Join-Path $ProjectRoot "tauri-launcher\src-tauri\tauri.microsoftstore.conf.json"
$MainConfig = Get-Content -LiteralPath (Join-Path $ProjectRoot "tauri-launcher\src-tauri\tauri.conf.json") -Raw |
    ConvertFrom-Json
$Overlay = Get-Content -LiteralPath $StoreConfig -Raw | ConvertFrom-Json
if ($Overlay.bundle.publisher -eq $MainConfig.productName) {
    throw "Microsoft Store publisher cannot match productName. See packaging/MICROSOFT_STORE.md."
}

$NativeHost = Join-Path $ProjectRoot "native-host\target\release\holodori-native-host.exe"
$TauriDir = Join-Path $ProjectRoot "tauri-launcher"
$TauriExe = Join-Path $TauriDir "src-tauri\target\release\holodori-usb-controller.exe"
foreach ($Path in @($NativeHost, $TauriExe, (Join-Path $TauriDir "node_modules"))) {
    if (-not (Test-Path -LiteralPath $Path)) {
        throw "Build the Windows release bundle first; missing $Path"
    }
}

$ReleaseRoot = Join-Path $ProjectRoot "release"
$OutputPath = Join-Path $ReleaseRoot "$Name-windows-x64-setup.exe"
if (Test-Path -LiteralPath $OutputPath) {
    throw "Store installer already exists: $OutputPath"
}

Push-Location $TauriDir
try {
    npx --no-install tauri bundle --config src-tauri/tauri.microsoftstore.conf.json
    if ($LASTEXITCODE -ne 0) {
        throw "Microsoft Store NSIS bundle failed."
    }
}
finally {
    Pop-Location
}

$BundleDir = Join-Path $TauriDir "src-tauri\target\release\bundle\nsis"
$Setup = Get-ChildItem -LiteralPath $BundleDir -Filter "*-setup.exe" |
    Sort-Object LastWriteTime -Descending |
    Select-Object -First 1
if (-not $Setup) {
    throw "Tauri did not produce an NSIS setup.exe under $BundleDir"
}

New-Item -ItemType Directory -Path $ReleaseRoot -Force | Out-Null
Copy-Item -LiteralPath $Setup.FullName -Destination $OutputPath
Write-Host "Microsoft Store installer: $OutputPath"
Write-Host "Silent install argument: /S"
Write-Host "Package URL after GitHub release: https://github.com/vn-nthh/holodori-phone-trackpad/releases/download/v$Version/$Name-windows-x64-setup.exe"
