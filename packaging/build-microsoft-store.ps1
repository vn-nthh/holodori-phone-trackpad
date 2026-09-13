[CmdletBinding()]
param(
    [string]$Name,
    [string]$BundlePath,
    [string]$PackageName = $env:DORITRACK_MSIX_PACKAGE_NAME,
    [string]$Publisher = $env:DORITRACK_MSIX_PUBLISHER,
    [string]$PublisherDisplayName = $env:DORITRACK_MSIX_PUBLISHER_DISPLAY_NAME,
    [switch]$Development
)

$ErrorActionPreference = 'Stop'
$ProjectRoot = Split-Path -Parent $PSScriptRoot
$Version = (Get-Content -LiteralPath (Join-Path $ProjectRoot 'VERSION') -Raw).Trim()
$VersionMatch = [regex]::Match($Version, '^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(-alpha\d+)?$')
if (-not $VersionMatch.Success) { throw "Unsupported VERSION '$Version'." }
if (-not $Development -and $VersionMatch.Groups[4].Success) {
    throw 'Microsoft Store submissions are stable-only. Use -Development to validate alpha packaging.'
}
if ($env:GITHUB_REF_TYPE -eq 'tag' -and $env:GITHUB_REF_NAME -ne "v$Version") {
    throw "Tag '$($env:GITHUB_REF_NAME)' does not match VERSION '$Version'."
}

# Store versions need a nonzero major and a zero fourth component. The +1
# mapping also keeps future 1.x app versions above today's 0.x packages.
$Major = [int]$VersionMatch.Groups[1].Value + 1
$Minor = [int]$VersionMatch.Groups[2].Value
$Patch = [int]$VersionMatch.Groups[3].Value
if ($Major -gt 65535 -or $Minor -gt 65535 -or $Patch -gt 65535) {
    throw 'VERSION exceeds MSIX version component limits.'
}
$PackageVersion = "$Major.$Minor.$Patch.0"
if ($Development) {
    $PackageName = 'Doritrack.PackagingTest'
    $Publisher = 'CN=Doritrack Packaging Test'
    $PublisherDisplayName = 'Doritrack development'
    $DisplayName = 'Doritrack (packaging test)'
} else {
    foreach ($Field in @('PackageName', 'Publisher', 'PublisherDisplayName')) {
        if ([string]::IsNullOrWhiteSpace((Get-Variable -Name $Field -ValueOnly))) {
            throw "Missing $Field from Partner Center Product identity. See packaging/MICROSOFT_STORE.md."
        }
    }
    if ($PackageName -eq 'Doritrack.PackagingTest' -or $Publisher -eq 'CN=Doritrack Packaging Test') {
        throw 'The test identity cannot be used for a Store submission.'
    }
    $DisplayName = 'Doritrack'
}
if ($PackageName -notmatch '^[A-Za-z0-9.-]{3,50}$') { throw 'Invalid MSIX PackageName.' }

if (-not $BundlePath) { $BundlePath = Join-Path $ProjectRoot "release\Doritrack-v$Version" }
$BundlePath = (Resolve-Path -LiteralPath $BundlePath).Path
$BuildInfoPath = Join-Path $BundlePath 'BUILD-INFO.txt'
$BuildInfo = Get-Content -LiteralPath $BuildInfoPath -Raw | ConvertFrom-StringData
if ($BuildInfo.android_version_name -ne $Version -or $BuildInfo.windows_arch -ne 'x86_64') {
    throw 'Build a matching Windows x64 release bundle before packaging MSIX.'
}
# Consume the validated bundle, so Store and portable builds ship identical
# launcher/host binaries. Verify it before using its release artifacts.
foreach ($Line in (Get-Content -LiteralPath (Join-Path $BundlePath 'SHA256SUMS.txt'))) {
    $Fields = $Line -split '  ', 2
    if ($Fields.Count -ne 2) { throw 'Invalid bundle checksum manifest.' }
    $FilePath = [IO.Path]::GetFullPath((Join-Path $BundlePath $Fields[1]))
    if (-not $FilePath.StartsWith($BundlePath.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Checksum path escapes the bundle.'
    }
    if ((Get-FileHash -LiteralPath $FilePath -Algorithm SHA256).Hash -ne $Fields[0]) {
        throw "Bundle checksum mismatch: $($Fields[1])"
    }
}

$SdkBin = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
$MakeAppx = Get-ChildItem -LiteralPath $SdkBin -Directory |
    Where-Object { $_.Name -match '^10\.0\.\d+\.0$' } |
    Sort-Object { [version]$_.Name } -Descending |
    ForEach-Object { Join-Path $_.FullName 'x64\makeappx.exe' } |
    Where-Object { Test-Path -LiteralPath $_ } |
    Select-Object -First 1
if (-not $MakeAppx) { throw 'Install the Windows 10/11 SDK with MakeAppx.exe.' }

if (-not $Name) { $Name = "Doritrack-v$Version" }
if ($Name -in @('.', '..') -or $Name.IndexOfAny([IO.Path]::GetInvalidFileNameChars()) -ge 0) {
    throw 'Name must be a single filename without directory components.'
}
$Suffix = if ($Development) { '-msix-test' } else { '-store' }
$OutputDir = Join-Path $ProjectRoot "build\msix\$Name$Suffix"
$LayoutDir = Join-Path $OutputDir 'layout'
$OutputPath = Join-Path $OutputDir "$Name-windows-x64$Suffix-unsigned.msix"
if (Test-Path -LiteralPath $OutputDir) { throw "MSIX output already exists: $OutputDir" }
New-Item -ItemType Directory -Path (Join-Path $LayoutDir 'Windows'),(Join-Path $LayoutDir 'Assets') | Out-Null
Copy-Item -LiteralPath (Join-Path $BundlePath 'HolodoriUsbController.exe') -Destination $LayoutDir
Copy-Item -LiteralPath (Join-Path $BundlePath 'Windows\holodori-native-host.exe') -Destination (Join-Path $LayoutDir 'Windows')
Copy-Item -LiteralPath (Join-Path $BundlePath 'Docs\LICENSE'),$BuildInfoPath -Destination $LayoutDir
foreach ($Icon in @('Square44x44Logo.png', 'Square150x150Logo.png', 'StoreLogo.png')) {
    Copy-Item -LiteralPath (Join-Path $ProjectRoot "tauri-launcher\src-tauri\icons\$Icon") -Destination (Join-Path $LayoutDir 'Assets')
}

$Manifest = [xml](Get-Content -LiteralPath (Join-Path $PSScriptRoot 'msix\AppxManifest.xml') -Raw)
$Manifest.Package.Identity.Name = $PackageName
$Manifest.Package.Identity.Publisher = $Publisher
$Manifest.Package.Identity.Version = $PackageVersion
$Manifest.Package.Properties.DisplayName = $DisplayName
$Manifest.Package.Properties.PublisherDisplayName = $PublisherDisplayName
$Manifest.Package.Applications.Application.VisualElements.DisplayName = $DisplayName
$Manifest.Save((Join-Path $LayoutDir 'AppxManifest.xml'))

& $MakeAppx pack /d $LayoutDir /p $OutputPath /h SHA256 /o
if ($LASTEXITCODE -ne 0) { throw 'MSIX packaging/manifest validation failed.' }
& (Join-Path $PSScriptRoot 'test-microsoft-store.ps1') -PackagePath $OutputPath -BundlePath $BundlePath -ExpectedPackageName $PackageName
"$((Get-FileHash -LiteralPath $OutputPath -Algorithm SHA256).Hash)  $([IO.Path]::GetFileName($OutputPath))" |
    Set-Content -Encoding ASCII -LiteralPath "$OutputPath.sha256"
Write-Host "MSIX: $OutputPath"
Write-Host "Package version: $PackageVersion; app version: $Version"
Write-Host "Layout for development registration: $LayoutDir"
if ($Development) {
    Write-Host 'TEST IDENTITY ONLY: do not submit or distribute this unsigned package to users.'
} else {
    Write-Host 'Upload this unsigned MSIX to Partner Center; Microsoft signs it after certification.'
    Write-Host 'runFullTrust and allowElevation must be reviewed before Store publication.'
}
