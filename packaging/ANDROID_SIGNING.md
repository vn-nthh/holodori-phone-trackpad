# Android release signing

An APK needs a signing key, not a purchased certificate. Keep the same private
key for every release so Android can install future updates. The alpha8 APK
used the Android debug certificate; release builds now use your private key.
Without signing settings, Gradle produces an unsigned release for build/lint
checks. Distribution scripts require a non-debug private key and fail closed.

## First build on Windows

From the repository root:

```powershell
.\packaging\build-signed-apk.ps1 -InitializeKey
```

This creates a 3072-bit RSA key with a 10,000-day certificate and a random
password. Both files are in the Git-ignored `.signing` directory, whose Windows
permissions are restricted to your account before any secrets are written:

- `doritrack-release.p12`: the private key and public certificate.
- `android-release.properties`: the key path, alias, and plaintext passwords.

Back up **both files** in an encrypted backup or password manager before
publishing. Losing them prevents signing compatible updates. Never commit,
attach, publish, or paste these files into chat. The script refuses to overwrite
an existing key or output APK. Do not run `-InitializeKey` again for a new version.

The script runs Android unit tests, debug/release builds, debug/release lint,
signature verification, and alignment verification. It writes the signed APK,
SHA-256 checksum, and a public signing report under `release/`. It uses `VERSION`
and the version code in `android-app/app/build.gradle` unless overridden.

## Subsequent builds and v0.5.0

```powershell
.\packaging\build-signed-apk.ps1

# When the v0.5.0 source is ready (alpha8 used version code 33):
.\packaging\build-signed-apk.ps1 -VersionName 0.5.0 -VersionCode 34
```

The second command writes `release/Doritrack-v0.5.0-release-signed.apk`.
Version overrides change only that build; they do not update `VERSION`, tags,
release notes, or the desktop bundle. Use a version code above every previously
published version. Choose a fresh `-OutputPath` to rebuild the same version.
`-JavaHome` and `-AndroidSdk` can override tool locations; otherwise the script
uses environment settings, then this repository's local SDK/JDK.

`build-experimental.ps1` and the optional Android step in `build-linux.sh` use
the same Gradle signing settings, so APKs inside bundles use the same key too.
Keep all release artifacts on the same source revision. Full release validation
and physical-device checks in `AGENTS.md` still apply before publication.

## Existing key or another machine

Restore the two `.signing` files from your backup, or configure all four
environment variables below. No secret belongs in a command-line argument or
tracked Gradle file. File paths in `android-release.properties` are relative to
`android-app/`; use forward slashes for Windows paths in this Java properties
file. Environment variables take precedence over the local properties.

| Environment variable | Value |
| --- | --- |
| `DORITRACK_KEYSTORE` | Absolute path to the keystore |
| `DORITRACK_STORE_PASSWORD` | Keystore password |
| `DORITRACK_KEY_ALIAS` | Key alias (`doritrack` for generated keys) |
| `DORITRACK_KEY_PASSWORD` | Key password (same as store password for generated keys) |

## GitHub Actions

The Windows release workflow restores this same key from GitHub Actions
repository secrets. It refuses to publish without them and deletes its temporary
keystore after the build. Linux CI only compiles an unsigned APK for validation.

Configure the following secrets in the GitHub repository's **Settings > Secrets
and variables > Actions**. This transfers a copy of the private key to GitHub;
only do so in the repository you trust to run your release workflow.

- `DORITRACK_KEYSTORE_BASE64`: Base64 encoding of `doritrack-release.p12`.
- `DORITRACK_STORE_PASSWORD`: `storePassword` from the properties file.
- `DORITRACK_KEY_ALIAS`: `doritrack`.
- `DORITRACK_KEY_PASSWORD`: `keyPassword` from the properties file.

With GitHub CLI authenticated, this PowerShell block uploads the local signing
files as repository secrets without printing their contents. Set the repository
explicitly before running:

```powershell
$SigningRepo = 'vn-nthh/holodori-phone-trackpad'
$SigningSettings = Get-Content .signing/android-release.properties -Raw | ConvertFrom-StringData
[Convert]::ToBase64String([IO.File]::ReadAllBytes(
    (Join-Path (Get-Location) '.signing/doritrack-release.p12'))) |
    gh secret set DORITRACK_KEYSTORE_BASE64 --repo $SigningRepo
if ($LASTEXITCODE -ne 0) { throw 'Keystore secret upload failed.' }
foreach ($Entry in @{
    DORITRACK_STORE_PASSWORD = $SigningSettings.storePassword
    DORITRACK_KEY_ALIAS = $SigningSettings.keyAlias
    DORITRACK_KEY_PASSWORD = $SigningSettings.keyPassword
}.GetEnumerator()) {
    $Entry.Value | gh secret set $Entry.Key --repo $SigningRepo
    if ($LASTEXITCODE -ne 0) { throw "Secret upload failed: $($Entry.Key)" }
}
$SigningSettings = $null
```

## Installing over an alpha

The new release key differs from the debug key used by existing alphas. Users
must uninstall the old app once, then install the release-signed APK and pair
again. Uninstalling clears local settings and pairing. Later releases signed
with this same key can update normally. This does not uninstall anything
automatically or replace previously published downloads.

References: [Android app signing](https://developer.android.com/studio/publish/app-signing)
and [apksigner verification](https://developer.android.com/tools/apksigner).
