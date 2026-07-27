# Bundled libwdi helper

Holodori bundles the `wdi-simple.exe` example from libwdi 1.5.1 to generate,
sign, pre-stage, and install the two device-specific WinUSB packages used by
Android Open Accessory mode.

- Upstream: <https://github.com/pbatard/libwdi>
- Tag: `v1.5.1`
- Commit: `9b23b82a2dd1cbffc16d46c212f92c6bf8c0c602`
- License: GNU LGPL 3.0 or later (`COPYING-LGPL`)
- Bundled source archive SHA-256:
  `746547AAF927CAE44C75512D763941805928427F4BA4DF3DBB40C3F7F561821E`
- Bundled x64 executable SHA-256:
  `5EEE1919EF07989BA8B54C199D66DAC93F90811D239FC49CBB8BF9C43A07BCC8`

The source archive is the upstream tag archive. Apply the compact patch with
`git apply --unidiff-zero holodori-build.patch`, replace the placeholder
`WDK_DIR` with the local Windows Driver Kit 8.0 redistributable root, and build
the installer helpers, libwdi static library, and
`examples/wdi-simple.vcxproj` in Release/x64 mode with Visual Studio 2022 Build
Tools. Only WinUSB support and the x64 target are needed by Holodori's x64-only
installer.

The embedded WDF and WinUSB coinstallers were obtained from Microsoft's WDF
1.11 coinstaller redistributable:

<https://download.microsoft.com/download/0/5/F/05FD6919-6250-425B-86ED-9B095E54065A/wdfcoinstaller.msi>

Its SHA-256 is
`29314207814CE9D5D73695F7E9239539CF37C79E750B9D5EA5A5EF5487A583D6`.
The applicable Microsoft license and redistribution notice are included next
to this file.

The Holodori installer invokes the helper only when the user selects the
WinUSB task. libwdi creates a device-specific catalog and local signing
certificate, adds that certificate to the required Windows trust stores,
installs the driver package, and deletes the certificate private key.
