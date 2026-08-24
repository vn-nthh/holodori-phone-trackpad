# Packaging protocol-v4 builds

Build release bundles with `build-experimental.ps1` as documented in the root
README. Keep the native host, Tauri launcher, and APK from the same source
revision: discovery port validation, endpoint pinning, status tokens, and route
recovery are coordinated across those artifacts.

Before publishing, run the validation commands in `AGENTS.md`. Also verify on a
real Windows PC that:

- discovery succeeds only over the phone's USB-tether adapter;
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
