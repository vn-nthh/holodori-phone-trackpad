# Microsoft Store: MSIX

MSIX is the Windows Store packaging standard. Microsoft signs the package for
free after certification and distributes updates through the Store. No purchased
code-signing certificate is needed for this submission route. See Microsoft's
[signing FAQ](https://learn.microsoft.com/en-us/windows/apps/publish/faq/get-started-with-the-microsoft-store).

GitHub continues to offer a Windows-only portable ZIP and a separate Android
APK, starting with v0.5.1-alpha2. Install phone and host from the same release.
The unsigned MSIX is a developer submission artifact, not a public GitHub
installer. Store signing does not sign the separate portable executables.

## Package contents and behavior

`build-microsoft-store.ps1` uses the Windows SDK's MakeAppx tool to package the
checksum-verified Windows bundle. It includes the launcher, native host under
`Windows/`, existing app icons, license, and build information. No Android APK,
touch probe, installer stub, or development certificate is included.

The installed app's taskbar and Start menu icons use `PC Logo Taskbar.png`.
The package logo uses the separate `PC Logo - App Store.png` artwork. In Partner
Center's **Store listings > Store logos**, upload
[`assets/icons/store-listing-300.png`](../assets/icons/store-listing-300.png) as
the **1:1 App tile icon (300 x 300 pixels)**. Microsoft prioritizes that image
over the package image on Store pages. See Microsoft's
[Store image requirements](https://learn.microsoft.com/en-us/windows/apps/publish/publish-your-app/msix/screenshots-and-images)
and the [icon regeneration instructions](../assets/icons/README.md).

The package targets x64 Windows 10 version 2004 (build 19041) and later, including
Windows 11. It retains the launcher's `requireAdministrator` manifest and
declares `runFullTrust` and `allowElevation`. This preserves elevated-game input
and USB-tether route recovery; it does not redesign the input path or remove
features to make packaging pass.

**Store publication depends on capability approval.** Microsoft requires advance
contact at `reportapp@microsoft.com` with justification for `allowElevation`.
Successful MakeAppx validation is not Store certification. See Microsoft's
[capability reference](https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/app-capability-declarations).

Suggested certification explanation: Doritrack is an unofficial Android phone
controller. Its Rust host submits ordinary Windows keyboard input, including
when the target game is elevated. The optional local-only USB mode temporarily
changes routes on the discovered tether adapter and restores them on Stop or
crash recovery. The app uses no custom driver, game-process hooks, or game-memory
access. Include the phone setup instructions and demonstrate safe release on
disconnect. Do not claim Microsoft has approved these capabilities in advance.

## WebView2

The launcher uses the system Evergreen WebView2 Runtime, as the portable build
does. The MSIX declares `Microsoft.WebView2` as an external dependency for
Microsoft App Installer. That declaration is ignored by Store, PowerShell,
and other installation mechanisms; it is not a guarantee that those routes
install WebView2. Keep WebView2 listed as a prerequisite and verify it on clean
Store test machines. If absent, install the
[Evergreen Runtime](https://developer.microsoft.com/en-us/microsoft-edge/webview2/).
See Microsoft's [external dependency rules](https://learn.microsoft.com/en-us/uwp/schemas/appxpackage/uapmanifestschema/element-win32dependencies-externaldependency).

## Build and validate before reserving the Store name

Install the Windows SDK including `MakeAppx.exe`, plus the normal project build
tools and Android release signing setup. From the repository root:

```powershell
$version = (Get-Content VERSION -Raw).Trim()
.\packaging\build-experimental.ps1 -Name "Doritrack-v$version"
.\packaging\build-microsoft-store.ps1 -Development
```

Use `-BundlePath <validated-bundle-directory>` if the bundle has another name.
`-Name <unique-name>` selects a fresh MSIX output directory; builds refuse to
overwrite an existing one. Neither command changes VERSION or published tags.

Development mode always uses `Doritrack.PackagingTest`, publisher
`CN=Doritrack Packaging Test`, and a visibly marked app name. It accepts alpha
versions and overrides any production identity environment variables. Its output
is under `build/msix/<name>-msix-test/`, ending in `-msix-test-unsigned.msix`.
Never submit this identity to Partner Center.

For a machine already configured for Windows developer mode, register the
generated loose layout with `Add-AppxPackage -Register <layout/AppxManifest.xml>`
and launch its Start menu entry. This avoids purchasing or trusting a certificate.
The package's normal administrator prompt still applies. Remove only the
`Doritrack.PackagingTest` registration when testing is finished. An unsigned MSIX
cannot be installed by ordinary double-click; sideloading a packed MSIX requires
appropriate signing and trust even when the eventual Store signing is free.

Run the Windows App Certification Kit and test the installed package's launch,
pairing, Stop, report folder, USB route recovery, update, and uninstall on Windows
10/11 before submission. MakeAppx validates package structure; it does not test
those behaviors or physical input latency. The checks in `AGENTS.md` still apply.

## Production identity and submissions

1. Reserve the app name as an **MSIX app** in Partner Center.
2. Copy the exact Product identity values into these GitHub Actions repository
   variables (they are identifiers, not signing secrets):

   | Repository variable | Partner Center field |
   | --- | --- |
   | `DORITRACK_MSIX_PACKAGE_NAME` | Package/Identity/Name |
   | `DORITRACK_MSIX_PUBLISHER` | Package/Identity/Publisher, including `CN=` |
   | `DORITRACK_MSIX_PUBLISHER_DISPLAY_NAME` | Publisher display name |

3. Request the required capability review and complete the listing.
4. On a stable `vX.Y.Z` tag, CI builds the production MSIX and retains it in the
   `Doritrack-MSIX-submission-<commit>` Actions artifact. Download that artifact
   and upload the MSIX directly to Partner Center's Packages page. No installer
   URL or `/S` argument is used. Microsoft signs it after certification.

Locally, the same values can be passed with `-PackageName`, `-Publisher`, and
`-PublisherDisplayName`, or set as the environment variables above. Then run
`build-microsoft-store.ps1` without `-Development` after the matching stable
bundle has passed validation. Missing identity or an alpha VERSION fails the
production build instead of creating a falsely labeled Store submission.

Production output is under `build/msix/<name>-store/` with an
`-store-unsigned.msix` suffix and SHA-256 sidecar. CI keeps MSIX artifacts separate
from the ZIP/APK assets published on GitHub. Branch and alpha CI builds validate
the test identity; only stable tags use the production identity.

MSIX version numbers use `(app major + 1).minor.patch.0`: app `0.5.1` maps to
package `1.5.1.0`, and app `1.0.0` maps to package `2.0.0.0`. The app still displays
its original version. This keeps the first component nonzero, the Store-reserved
fourth component zero, and upgrades increasing across the 0.x to 1.x transition.
Alpha suffixes are ignored only for test-identity packages. See Microsoft's
[package requirements](https://learn.microsoft.com/en-us/windows/apps/publish/publish-your-app/msix/app-package-requirements).

## Debug and latency reports

After Stop, use **Settings > Open report folder** in either distribution:

- Portable: `%LOCALAPPDATA%\Doritrack\Logs`.
- MSIX: `%LOCALAPPDATA%\Packages\<PackageFamilyName>\LocalState\Logs`.

The host queries its actual Windows package identity; the folder does not contain
the package version or depend on the installation directory. Reports survive
updates. MSIX reset/uninstall can remove package data, so copy reports elsewhere
before either operation if they need to be retained. Local data belongs to the
account running the app, including when UAC uses another account's credentials.

Old portable reports remain under `Windows\Logs` in their old extracted folders.
`--metrics-file PATH` still overrides the default. The app keeps metrics in memory
and writes reports after Stop; report browsing is disabled while the controller
is running.
