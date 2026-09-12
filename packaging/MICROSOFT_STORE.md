# Microsoft Store (stable Windows app)

GitHub releases stay the portable zip + APK flow, including alphas as
prereleases. The Microsoft Store listing is a separate EXE installer and
**only ships stable `x.y.z` versions**.

Do not convert the portable zip into an installer. Do not submit alpha tags.

## What this repository automates

On a stable `vX.Y.Z` tag, Windows CI builds
`release/Doritrack-vX.Y.Z-windows-x64-setup.exe` after the normal bundle and
attaches it to that GitHub release. The installer is:

- NSIS, silent with `/S`
- current-user (no admin required to install)
- WebView2 **offline** (Store rejects downloaders)
- self-contained with `Windows/holodori-native-host.exe` next to the launcher

Locally, after a successful `packaging/build-experimental.ps1` on a stable
`VERSION`:

```powershell
.\packaging\build-microsoft-store.ps1
```

The setup.exe on a GitHub release URL must never be replaced. Bump the version
and create a new tag instead.

## Partner Center (once)

1. Enroll at [Microsoft Partner Center](https://partner.microsoft.com/dashboard).
2. Apps and games → **New product** → **EXE or MSI app**.
3. Reserve a unique name (for example `Doritrack`).
4. Fill properties, age ratings, and the Store listing.
5. Packages page for the first stable tag:

   | Field | Value |
   | --- | --- |
   | Package URL | `https://github.com/vn-nthh/holodori-phone-trackpad/releases/download/vX.Y.Z/Doritrack-vX.Y.Z-windows-x64-setup.exe` |
   | Architecture | `x64` |
   | Installer parameters | `/S` |
   | App type | `EXE` |

6. Notes for certification: the installer is current-user and silent; the
   installed app then requests administrator once at launch so it can inject
   keys into an elevated game and recover USB-tether routes. There is no custom
   driver. Local-only USB tethering may change routes on the phone's RNDIS
   adapter and restores them on Stop.

Later stables are a **new submission** with the new versioned GitHub URL. Leave
alphas on GitHub only.

A Windows code-signing certificate is recommended, not required for the first
listing. Store MSIX packaging is not used: this app's release binary embeds
`requireAdministrator`, which a Store MSIX cannot preserve.
