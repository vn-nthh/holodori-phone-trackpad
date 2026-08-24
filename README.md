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

The current version is **v0.4.1**.

- [Windows app](https://github.com/vn-nthh/holodori-phone-trackpad/releases/download/v0.4.1/HolodoriUsbTetheredUdp-v0.4.1-windows-x64.zip)
- Linux app: a ready-made download is not available yet. See the
  [Linux setup guide](LINUX_SETUP.md) if you want to build and use it now.
- [Android app](https://github.com/vn-nthh/holodori-phone-trackpad/releases/download/v0.4.1/HolodoriUsbTetheredUdp-v0.4.1-android.apk)
- [Release notes](https://github.com/vn-nthh/holodori-phone-trackpad/releases/tag/v0.4.1)

You need:

- a Windows 10 or 11 PC, or a Linux PC;
- an Android phone with USB tethering;
- one USB data cable.

## Quick start

### Windows

1. Download and unzip the Windows app.
2. Install the Android app on the phone.
3. Connect the phone to the PC with a USB data cable.
4. Turn on **USB tethering** in the phone's settings.
5. Open `HolodoriUsbController.exe` on Windows.
6. Change the six lane keys if needed, then press **Start**.
7. Open the Android app.
8. Move, resize, or rotate the play area, then lock it.
9. Start the game and play.
10. Press **Stop** in the Windows app when finished.

### Linux

Linux needs a small amount of one-time setup before the first play session.
Follow the [Linux setup guide](LINUX_SETUP.md), then:

1. Unpack the Linux bundle and install the Android app on the phone.
2. Connect the phone with a USB data cable.
3. Turn on **USB tethering** in the phone's settings.
4. Open `HolodoriUsbController` from the bundle folder.
5. Change the six lane keys if needed, then press **Start**.
6. Open the Android app and arrange the play area.
7. Start the game and play.
8. Press **Stop** in the controller when finished.

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

## How the USB connection works

USB tethering creates a private cable connection between the phone and PC. The
Android app sends your touches through that connection, so no USB debugging,
phone root access, or special USB driver is needed.

The controller is designed to keep taps, holds, slides, and chords in the right
order. If the connection breaks, it releases held keys and avoids playing old
touches after reconnection.

The tool sends normal keyboard input to the PC. It does not open, read, or
change the game process.

The current version does not yet pair the phone and PC with a code. Use it only
on a trusted PC. If your firewall asks, allow the controller only on the phone's
USB connection—not on home or public Wi-Fi. Pairing is planned for a future
version.

Developers can read the [architecture guide](EXPERIMENTAL_ARCHITECTURE.md) and
[protocol specification](PROTOCOL_V4.md) for the transport and recovery details.

## Controller options

### Lane keys

Select each key box and press the letter or number you want to use. The default
layout is `S D F J K L`.

Test custom keys in Notepad before opening the game. If the game is running as
administrator, run the controller as administrator too.

### Stop the PC from using the phone's internet

USB tethering can make the PC use the phone as an internet connection. Turn on
**Stop the PC from using the phone's internet** if you only want the local
phone-to-PC link.

Windows may ask for administrator access. Use **Restart as admin** when the
launcher offers it. The app restores the setting when you stop. If the app was
interrupted, open it again and follow the recovery message before playing.

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
- Press **Start** before opening the phone app.
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

If the game runs as administrator, restart the controller as administrator.

### The Android app will not install

An older test build may use a different signing key. Uninstall the old
Holodori controller app, then install the new Android app.

## For developers

The live input path uses a native Rust host, an Android app, and a small Tauri
launcher. The protocol is designed around an 8.333 ms frame budget for 120 Hz
play, but real results still depend on the phone, cable, and PC.

Build the release bundle from PowerShell:

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

Run the main test suites:

```powershell
cargo test --manifest-path native-host\Cargo.toml --all-targets
python -m unittest discover -s tests
```

On Linux:

```sh
cargo test --manifest-path native-host/Cargo.toml --all-targets
python -m unittest discover -s tests
```

See the [Linux setup guide](LINUX_SETUP.md),
[architecture guide](EXPERIMENTAL_ARCHITECTURE.md), and
[protocol v4 specification](PROTOCOL_V4.md) for implementation details.

## License

[MIT](LICENSE). Use of this tool remains subject to the game's terms and
competitive rules.
