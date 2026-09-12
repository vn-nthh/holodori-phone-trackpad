[English](README.md) | [日本語](README.ja.md)

# Doritrack

Use an Android phone as a six-lane touch controller for
[hololive Dreams (holodori)](https://store.steampowered.com/app/4282500/hololive_Dreams/)
on Windows or Linux.

Tap, hold, slide, and play chords on the phone. The app turns those touches
into lane keys. The default keys are `S D F J K L`.

> This is an unofficial community tool. It is not affiliated with COVER Corp.,
> hololive production, or QualiArts.

## Download

The stable version is **v0.5.0**, with authenticated USB/Wi-Fi sessions,
pairing, thumb mode, and new Android and PC icons.
Install the phone and host from the same release.

- [Windows app](https://github.com/vn-nthh/holodori-phone-trackpad/releases/download/v0.5.0/Doritrack-v0.5.0-windows-x64.zip)
- Linux app: a ready-made download is not available yet. See the
  [Linux setup guide](LINUX_SETUP.md) if you want to build and use it now.
- [Android app](https://github.com/vn-nthh/holodori-phone-trackpad/releases/download/v0.5.0/Doritrack-v0.5.0-android.apk)
- [Release notes](https://github.com/vn-nthh/holodori-phone-trackpad/releases/tag/v0.5.0)

**Upgrading from an alpha or v0.4.1:** the Android app now uses a persistent
private release key. Uninstall the old APK once, install v0.5.0, and pair again.
Uninstalling clears the phone app's settings and pairing. Future releases using
this key can update normally.

You need:

- a Windows 10 or 11 PC, or a Linux PC;
- an Android phone;
- USB tethering and a USB data cable, or both devices on the same private local
  network with the phone connected over Wi-Fi.

## Quick start

### Windows

1. Download and unzip the Windows app.
2. Install the Android app on the phone.
3. For USB, connect a data cable and enable **USB tethering** in the phone's
   settings. For Wi-Fi, connect both devices to the same private local network.
4. Open `HolodoriUsbController.exe`, accept the administrator prompt, and open
   Doritrack on the phone.
5. Select the same connection type on both devices.
6. On first use, press **Pair** on both devices. Tap the host's eight numbered
   lanes on the phone in order, confirm **Pattern matched**, then click
   **Approve** on the PC. Later sessions remember this pairing.
7. Change the six lane keys in **Preferences** if needed, then press **Start**
   on both devices.
8. Arrange and lock the phone's play area, or select thumb mode, then play.
9. Press **Stop** when finished.

### Linux

Linux needs a small amount of one-time setup before the first play session.
Follow the [Linux setup guide](LINUX_SETUP.md), then:

1. Unpack the Linux bundle and install the Android app on the phone.
2. Connect by USB tethering or the same private local network.
3. Open `HolodoriUsbController` from the bundle folder and open the phone app.
4. Select the same connection type and complete the first-use **Pair** flow
   described above, including local **Approve** on the PC.
5. Change lane keys if needed, press **Start** on both devices, and arrange the
   phone's play area.
6. Start the game and play. Press **Stop** when finished.

The name and location of the USB tethering setting depend on the phone. It is
usually under **Network**, **Connections**, **Hotspot**, or **Tethering**.

## What it supports

- Taps become quick key presses.
- Holds stay pressed until the finger lifts, even if it moves just outside the
  play area; the nearest edge lane stays held.
- Slides pass through every crossed lane in order.
- Multiple fingers can hold different lanes at the same time.
- A disconnect releases held keys instead of leaving them stuck.
- Lane keys can be changed in the app.

## How the connection works

USB tethering creates a private cable connection between the phone and PC. The
Android app sends your touches through that connection, so no USB debugging,
phone root access, or special USB driver is needed.
Alternatively, select Wi-Fi to use the same private local network. Pair and
Start use only the connection type you select.

The controller is designed to keep taps, holds, slides, and chords in the right
order. If the connection breaks, it releases held keys and avoids playing old
touches after reconnection.

The tool sends normal keyboard input to the PC. It does not open, read, or
change the game process.

Protocol V5 authenticates the paired devices and encrypts input. The first-use
lane pattern must match, and pairing requires approval on the PC. Allow the
controller through the firewall on your selected USB or private local network.
Use a trusted PC and keep the UDP listener off public networks. Legacy protocol
v4 is available only through the explicit USB option and has no authenticated
pairing.

Developers can read the [architecture guide](EXPERIMENTAL_ARCHITECTURE.md) and
[protocol v5 specification](PROTOCOL_V5.md) and
[interoperability vectors](PROTOCOL_V5_TEST_VECTORS.md) for the shipping transport.
The [protocol v4 specification](PROTOCOL_V4.md) documents the legacy USB mode.

## Controller options

### Lane keys

Select each key box and press the letter or number you want to use. The default
layout is `S D F J K L`.

Test custom keys in Notepad before opening the game. On Windows the launcher
always asks for administrator access when it opens, so it can send keys to a
game that runs as administrator.

### Stop the PC from using the phone's internet

USB tethering can make the PC use the phone as an internet connection. Turn on
**Stop the PC from using the phone's internet** if you only want the local
phone-to-PC link.

The app restores the setting when you stop. If the app was interrupted, open it
again and follow the recovery message before playing.

If the phone is disconnected, Windows route cleanup stays pending while the
launcher remains usable. Reconnect the phone, enable **USB tethering**, then
click **Pair** or **Start** to retry recovery. The app keeps the saved settings
until it can restore the same adapter safely; pending cleanup for an absent
adapter does not block local-network mode.

On Linux, your desktop may ask for permission to change this network setting.
The app checks the result before it lets a protected session start. See the
[Linux setup guide](LINUX_SETUP.md#keeping-the-pc-off-the-phones-internet) for
requirements and troubleshooting.

The status line distinguishes **Waiting**, **Phone connected**,
**Recovering**, and **Stopping**. Recovering means held input has been released
and the controller is waiting for a fresh phone session; you do not need to
press Start again.

### Save latency report when stopped

Leave this checked if you want a report after playing. Reports are saved under
`Windows\Logs` on Windows. Linux locations are listed in the
[Linux setup guide](LINUX_SETUP.md#latency-reports). The app writes the report
only after you press **Stop**, not while you are playing.

## Linux users

Linux needs one-time permission and firewall setup. The dedicated
[Linux setup guide](LINUX_SETUP.md) has the commands, distribution notes,
internet-routing option, and Linux troubleshooting in one place.

Use the safe permission steps in the guide. Avoid shortcuts that give every
account on the PC control over keyboard input.

## Troubleshooting

### The phone does not connect

- Make sure the cable supports data, not charging only.
- Turn USB tethering off and on again.
- Wait for the PC to show a new USB network connection.
- Select the same transport on both devices, complete pairing, and press
  **Start** on both.
- For Wi-Fi, use the same private subnet and check that the router allows
  devices to communicate with each other.
- On Windows, allow the controller when Windows Firewall asks.
- Close any second copy of the controller.
- On Linux, follow the connection checks in the
  [Linux setup guide](LINUX_SETUP.md#the-phone-does-not-connect).

### The Windows app does not open

The launcher uses Microsoft Edge WebView2, which is normally included with
Windows 10 and 11. Install or update the WebView2 Runtime if the launcher stays
closed.

### The Linux app does not open

See [Linux troubleshooting](LINUX_SETUP.md#troubleshooting) for missing desktop
libraries, graphics problems, input permissions, firewall setup, and tether
policy messages.

### Keys work outside the game but not inside it

The launcher runs as administrator on Windows for exactly this case. If you
declined the elevation prompt, close the launcher and open it again.

### The Android app will not install

Older releases use a different signing key. Uninstall the old Holodori
controller app once, install v0.5.0, then pair again. This clears the phone
app's settings and pairing.

## For developers

The live input path uses a native Rust host, an Android app, and a small Tauri
launcher. The protocol is designed around an 8.333 ms frame budget for 120 Hz
play, but real results still depend on the phone, cable, and PC.

Build the release bundle from PowerShell:

Set up the [Android release signing key](packaging/ANDROID_SIGNING.md) first.

```powershell
.\packaging\build-experimental.ps1 `
  -CargoHome "$env:USERPROFILE\.cargo" `
  -JavaHome $env:JAVA_HOME `
  -AndroidSdk $env:ANDROID_SDK_ROOT
```

Or from Bash on Linux:

```sh
packaging/build-linux.sh
```

Run the native tests:

```powershell
cargo test --manifest-path native-host\Cargo.toml --all-targets
```

On Linux:

```sh
cargo test --manifest-path native-host/Cargo.toml --all-targets
```

See the [Linux setup guide](LINUX_SETUP.md),
[architecture guide](EXPERIMENTAL_ARCHITECTURE.md), and
[protocol v5 specification](PROTOCOL_V5.md) for current source-build details.
The [protocol v4 specification](PROTOCOL_V4.md) documents the published v0.4.1
transport and the explicit legacy migration mode.

## License

[MIT](LICENSE). Use of this tool remains subject to the game's terms and
competitive rules.
