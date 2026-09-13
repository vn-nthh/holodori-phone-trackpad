# Packaging protocol-v5 builds

Set up the persistent [Android release signing key](ANDROID_SIGNING.md) before
building a distributable APK or bundle. `build-signed-apk.ps1 -InitializeKey`
creates the local key once and validates a signed APK; subsequent builds reuse it.

Build release bundles with `build-experimental.ps1` as documented in the root
README. Keep the native host, Tauri launcher, and APK from the same source
revision: Noise identities, discovery confinement, interoperability vectors,
status tokens, and clean recovery are coordinated across those artifacts.

From v0.5.1-alpha2 onward, the Windows ZIP contains the launcher, native Windows
tools, and documentation only. The build still validates both platforms, but
ships Android exclusively as the separate `*-android.apk` release asset. Users
download both assets from the same release. Store submissions use the
[separate MSIX package](MICROSOFT_STORE.md), which Microsoft signs for free
after certification. Unsigned submission packages stay in CI artifacts, not
public GitHub downloads. Until Partner Center identity is configured, branch
and alpha CI validate MSIX with a separate test identity.

Windows latency reports go to `%LOCALAPPDATA%\Doritrack\Logs` for portable
builds, or `%LOCALAPPDATA%\Packages\<PackageFamilyName>\LocalState\Logs` for
MSIX. Use Settings > Open report folder after Stop. Both paths survive updates;
MSIX reset/uninstall can remove its reports. Do not write into the app directory.
Old `Windows\Logs` reports are left in place. `--metrics-file PATH` still
overrides the destination.

Before publishing, run the validation commands in `AGENTS.md`. Also verify on a
real Windows PC that:

- pairing and remembered sessions stay on the explicitly selected USB or
  local-network interface;
- V5 never accepts legacy-v4 gameplay on the Wi-Fi listener;
- the launcher shows Waiting, Connected, Recovering, and Stopping;
- killing the host during local-only mode leaves a recovery snapshot, and the
  next launcher start restores it (requesting elevation when required);
- a generic USB Ethernet/NCM adapter is not changed before phone discovery;
- on a disposable test adapter, a replacement route installed after capture is
  preserved during normal stop and crash recovery;
- unplugging the disposable adapter before cleanup retains the recovery journal,
  and reconnecting it allows recovery to complete without touching another
  adapter that reused its interface index;
- a real phone/cable soak stays within the project's 8.333 ms live budget.

Published release notes and tags are immutable. Add a new release note for a
new version instead of editing an older one.
