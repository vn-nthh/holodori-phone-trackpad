# Doritrack Privacy Policy

Last updated: September 13, 2026

This policy describes the privacy practices of CattWorks ("we", "us") for
Doritrack and its Android companion app. Doritrack lets you use your phone as a
touch controller for your computer.

## Information sent to CattWorks

Doritrack does not automatically send personal information, controller input,
usage analytics, or diagnostic reports to CattWorks. We do not operate a cloud
service for the app. Doritrack does not require an account and does not include
advertising, advertising trackers, or automatic analytics or crash-reporting
services added by CattWorks. We do not sell personal information.

## Information used on your devices

Doritrack processes information locally to provide its controller features:

- **Controller input:** touch positions, contact states, timing, and related
  input information are processed on your phone and computer to generate the
  configured controller input.
- **Connection information:** local network addresses, network interface
  details, and connection-quality measurements are used to discover devices,
  establish the selected connection, and diagnose connection problems.
- **Settings and pairing:** preferences, selected lane keys, and cryptographic
  installation identities, paired-device names, and pairing credentials are
  stored on your devices to remember your configuration and recognize paired
  devices.
- **Optional diagnostic reports:** if you enable report saving, the app writes
  local performance and connection statistics, session identifiers, and
  diagnostic information. These reports are not uploaded automatically.
- **USB network recovery records:** when you enable the option that prevents
  your computer from using your phone's internet connection, the app may save
  network adapter identifiers and routing settings locally so it can restore
  the connection settings after stopping or recovering from an interruption.

## Communication and security

The apps communicate directly over your selected USB-tethering connection or
local network. Device discovery may use broadcasts on that local network.
Controller input is sent between your phone and computer, not through a
CattWorks server.

The default protocol V5 authenticates paired devices and encrypts session
traffic. Pairing credentials are protected using operating-system security
features. The optional legacy USB protocol does not provide V5 authentication
or encryption.

On Android 10 and newer, the app uses the `WAKE_LOCK` permission to request
low-latency Wi-Fi during pairing and controller sessions. The request ends when
the connection closes. It does not collect additional information or acquire a
CPU wake lock, and it is not used for USB connections.

## Your choices and retention

You can stop the controller connection at any time. Use the app's **Forget**
control on each device to remove its remembered pairing. Forgetting a peer does
not remove the installation's own cryptographic identity or all app settings.

You can disable latency-report saving and delete existing report files.
Settings and credentials remain locally until you clear the relevant app data
or remove those files. Uninstalling the desktop app may leave local settings,
credentials, or reports behind. Network recovery records are retained as needed
to restore settings, including when the affected adapter is temporarily absent.

CattWorks cannot remotely access or delete data that remains only on your
devices.

## Support and external services

If you choose to contact us or share a diagnostic report, we receive the
information you submit and use it to answer your request and troubleshoot the
app. GitHub issues are public; avoid posting personal information, passwords,
or private keys. Information submitted through GitHub is also subject to
GitHub's privacy policy and retention controls.

Microsoft Store, Windows, Microsoft Edge WebView2, Android, and other platform
services may separately process information under their own privacy policies.
This policy describes CattWorks' practices for Doritrack and does not replace
those providers' policies.

## Changes and contact

We will update this policy when the app's privacy practices change and revise
the date above.

For privacy questions, contact CattWorks through the
[Doritrack project issues page](https://github.com/vn-nthh/holodori-phone-trackpad/issues).
