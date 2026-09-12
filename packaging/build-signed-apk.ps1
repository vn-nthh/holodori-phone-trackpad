[CmdletBinding()]
param(
    [switch]$InitializeKey,
    [ValidatePattern('^\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?$')]
    [string]$VersionName,
    [ValidateRange(1, 2100000000)]
    [int]$VersionCode,
    [string]$JavaHome = $env:JAVA_HOME,
    [string]$AndroidSdk = $env:ANDROID_HOME,
    [string]$OutputPath
)

$ErrorActionPreference = "Stop"
$ProjectRoot = Split-Path -Parent $PSScriptRoot
$AndroidDir = Join-Path $ProjectRoot "android-app"
$SigningDir = Join-Path $ProjectRoot ".signing"
$KeyStore = Join-Path $SigningDir "doritrack-release.p12"
$PropertiesFile = Join-Path $SigningDir "android-release.properties"
if (-not $VersionName) {
    $VersionName = (Get-Content -LiteralPath (Join-Path $ProjectRoot "VERSION") -Raw).Trim()
}
if (-not $OutputPath) {
    $OutputPath = Join-Path $ProjectRoot "release\Doritrack-v$VersionName-release-signed.apk"
}
$OutputPath = [IO.Path]::GetFullPath($OutputPath)
foreach ($Path in @($OutputPath, "$OutputPath.sha256", "$OutputPath.signing.txt")) {
    if (Test-Path -LiteralPath $Path) {
        throw "Output already exists; choose another -OutputPath: $Path"
    }
}
if (-not $JavaHome) {
    $JavaHome = Join-Path $ProjectRoot ".android-sdk\jdk17\jdk-17.0.20+8"
}
if (-not $AndroidSdk) {
    $AndroidSdk = if ($env:ANDROID_SDK_ROOT) { $env:ANDROID_SDK_ROOT } else {
        Join-Path $ProjectRoot ".android-sdk"
    }
}
$JavaHome = (Resolve-Path -LiteralPath $JavaHome).Path
$AndroidSdk = (Resolve-Path -LiteralPath $AndroidSdk).Path
$BuildTools = Get-ChildItem -LiteralPath (Join-Path $AndroidSdk "build-tools") -Directory |
    Where-Object Name -Match '^\d+\.\d+\.\d+$' |
    Sort-Object { [version]$_.Name } -Descending | Select-Object -First 1
if (-not $BuildTools) { throw "No Android SDK build-tools found." }
$ApkSigner = Join-Path $BuildTools.FullName "apksigner.bat"
$PreviousJavaHome = $env:JAVA_HOME
$PreviousAndroidHome = $env:ANDROID_HOME
$PreviousAndroidSdkRoot = $env:ANDROID_SDK_ROOT
try {
    $env:JAVA_HOME = $JavaHome
    $env:ANDROID_HOME = $AndroidSdk
    $env:ANDROID_SDK_ROOT = $AndroidSdk

    if ($InitializeKey) {
        if ((Test-Path -LiteralPath $KeyStore) -or (Test-Path -LiteralPath $PropertiesFile)) {
            throw "Signing files already exist. Reuse them without -InitializeKey; never replace a release key."
        }
        if ($env:DORITRACK_KEYSTORE -or $env:DORITRACK_STORE_PASSWORD -or
            $env:DORITRACK_KEY_ALIAS -or $env:DORITRACK_KEY_PASSWORD) {
            throw "Clear DORITRACK signing environment variables before initializing a local key."
        }
        New-Item -ItemType Directory -Path $SigningDir -Force | Out-Null
        # Remove inherited access before writing any secrets; keep only this Windows user.
        $Acl = New-Object Security.AccessControl.DirectorySecurity
        $Acl.SetAccessRuleProtection($true, $false)
        $Identity = [Security.Principal.WindowsIdentity]::GetCurrent().User
        $Acl.SetOwner($Identity)
        $Acl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule(
            $Identity, "FullControl", "ContainerInherit,ObjectInherit", "None", "Allow")))
        Set-Acl -LiteralPath $SigningDir -AclObject $Acl
        $Random = [Security.Cryptography.RandomNumberGenerator]::Create()
        $Bytes = New-Object byte[] 32
        try { $Random.GetBytes($Bytes) } finally { $Random.Dispose() }
        $Password = [Convert]::ToBase64String($Bytes)
        $PreviousKeyPassword = $env:DORITRACK_NEW_KEY_PASSWORD
        try {
            $env:DORITRACK_NEW_KEY_PASSWORD = $Password
            & (Join-Path $JavaHome "bin\keytool.exe") -genkeypair -noprompt `
                -keystore $KeyStore -storetype PKCS12 -alias doritrack `
                -keyalg RSA -keysize 3072 -sigalg SHA256withRSA -validity 10000 `
                -dname "CN=Doritrack Release" `
                -storepass:env DORITRACK_NEW_KEY_PASSWORD -keypass:env DORITRACK_NEW_KEY_PASSWORD
            if ($LASTEXITCODE -ne 0) { throw "Release key generation failed." }
            @(
                "storeFile=../.signing/doritrack-release.p12"
                "storePassword=$Password"
                "keyAlias=doritrack"
                "keyPassword=$Password"
            ) | Set-Content -LiteralPath $PropertiesFile -Encoding ASCII
        }
        finally {
            $env:DORITRACK_NEW_KEY_PASSWORD = $PreviousKeyPassword
            $Password = $null
        }
        Write-Host "Created private signing files in $SigningDir. Back up both files securely."
    }

    $VersionArgs = @("-PholodoriVersionName=$VersionName")
    if ($PSBoundParameters.ContainsKey("VersionCode")) {
        $VersionArgs += "-PholodoriVersionCode=$VersionCode"
    }
    & (Join-Path $AndroidDir "gradlew.bat") --project-dir $AndroidDir --no-daemon `
        @VersionArgs requireReleaseSigning testDebugUnitTest assembleDebug assembleRelease lintDebug lintRelease
    if ($LASTEXITCODE -ne 0) { throw "Android release validation/build failed." }

    $Apk = Join-Path $AndroidDir "app\build\outputs\apk\release\app-release.apk"
    $Verification = & $ApkSigner verify --verbose --print-certs $Apk
    if ($LASTEXITCODE -ne 0) { throw "APK signature verification failed." }
    if ($Verification -match 'certificate DN:.*CN=Android Debug') {
        throw "Refusing to distribute an APK signed with the Android debug key."
    }
    & (Join-Path $BuildTools.FullName "zipalign.exe") -c -P 16 4 $Apk
    if ($LASTEXITCODE -ne 0) { throw "APK alignment verification failed." }
    $Metadata = Get-Content -LiteralPath (Join-Path (Split-Path $Apk) "output-metadata.json") -Raw |
        ConvertFrom-Json
    $ApkVersion = $Metadata.elements[0]
    if ($ApkVersion.versionName -ne $VersionName -or
        ($PSBoundParameters.ContainsKey("VersionCode") -and $ApkVersion.versionCode -ne $VersionCode)) {
        throw "Built APK version does not match the requested version."
    }
    New-Item -ItemType Directory -Path (Split-Path -Parent $OutputPath) -Force | Out-Null
    [IO.File]::Copy($Apk, $OutputPath, $false)
    $Hash = (Get-FileHash -LiteralPath $OutputPath -Algorithm SHA256).Hash.ToLowerInvariant()
    "$Hash  $([IO.Path]::GetFileName($OutputPath))" |
        Set-Content -LiteralPath "$OutputPath.sha256" -Encoding ASCII
    @("versionName=$($ApkVersion.versionName)", "versionCode=$($ApkVersion.versionCode)") +
        $Verification | Set-Content -LiteralPath "$OutputPath.signing.txt" -Encoding ASCII
    Write-Host "Signed APK: $OutputPath"
    Write-Host "Version: $($ApkVersion.versionName) (code $($ApkVersion.versionCode))"
    $Verification | Where-Object { $_ -match 'certificate SHA-256 digest:' } | Write-Host
}
finally {
    $env:JAVA_HOME = $PreviousJavaHome
    $env:ANDROID_HOME = $PreviousAndroidHome
    $env:ANDROID_SDK_ROOT = $PreviousAndroidSdkRoot
}
