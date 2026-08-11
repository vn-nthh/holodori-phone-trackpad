# Holodori Phone Trackpad

Use an Android phone as a six-lane touch controller for
[hololive Dreams (holodori)](https://store.steampowered.com/app/4282500/hololive_Dreams/)
on Windows.

Tap, hold, slide, and play chords on the phone. The Windows app turns those
touches into lane keys. The default keys are `S D F J K L`.

> This is an unofficial community tool. It is not affiliated with COVER Corp.,
> hololive production, or QualiArts.

## Download

The current version is **v0.4.1**.

- [Windows app](https://github.com/vn-nthh/holodori-phone-trackpad/releases/download/v0.4.1/HolodoriUsbTetheredUdp-v0.4.1-windows-x64.zip)
- [Android app](https://github.com/vn-nthh/holodori-phone-trackpad/releases/download/v0.4.1/HolodoriUsbTetheredUdp-v0.4.1-android.apk)
- [Release notes](https://github.com/vn-nthh/holodori-phone-trackpad/releases/tag/v0.4.1)

You need:

- a Windows 10 or 11 PC;
- an Android phone with USB tethering;
- one USB data cable.

## Quick start

1. Download and unzip the Windows app.
2. Install the Android APK on the phone.
3. Connect the phone to the PC with a USB data cable.
4. Turn on **USB tethering** in the phone's settings.
5. Open `HolodoriUsbController.exe` on Windows.
6. Change the six lane keys if needed, then press **Start**.
7. Open the Android app.
8. Move, resize, or rotate the play area, then lock it.
9. Start the game and play.
10. Press **Stop** in the Windows app when finished.

The name and location of the USB tethering setting depend on the phone. It is
usually under **Network**, **Connections**, **Hotspot**, or **Tethering**.

## What it supports

- Taps become quick key presses.
- Holds stay pressed until the finger lifts.
- Slides pass through every crossed lane in order.
- Multiple fingers can hold different lanes at the same time.
- A disconnect releases held keys instead of leaving them stuck.
- Lane keys can be changed in the Windows app.

## How the USB connection works

USB tethering creates a small network link between the phone and the PC. The
Android app sends touch updates over that link with UDP. This keeps setup
simple and avoids special USB drivers.

Each update is numbered and checked for damage. Important updates are sent
twice right away. If both copies are lost, the phone tries again after about
2 milliseconds. The PC confirms an update only after Windows accepts the key
or touch action.

The app keeps the full state of every finger. This lets it preserve holds,
chords, and fast slides, and safely rebuild a hold after a short reconnect.
Old touches are discarded after a broken connection so they do not play late.

The tool sends normal Windows input. It does not open, read, or change the game
process.

## Windows options

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
launcher offers it. The original network setting is restored after a normal
stop.

### Save latency report when stopped

Leave this checked if you want a report after playing. Reports are saved under
`Windows\Logs` when you press **Stop**. The app does not write reports while
you are playing.

## Troubleshooting

### The phone does not connect

- Make sure the cable supports data, not charging only.
- Turn USB tethering off and on again.
- Wait for Windows to create a new Ethernet or USB network connection.
- Open the phone app after pressing **Start** on Windows.
- Allow `holodori-native-host.exe` through Windows Firewall for UDP port
  `42825`.
- Close any second copy of the controller. Only one can use port `42825`.

### The Windows app does not open

The launcher uses Microsoft Edge WebView2, which is normally included with
Windows 10 and 11. Install or update the WebView2 Runtime if the launcher stays
closed.

### Keys work outside the game but not inside it

If the game runs as administrator, restart the controller as administrator.

### Android refuses to install the APK

An older test build may use a different signing key. Uninstall the old
Holodori controller app, then install the new APK.

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

Run the main test suites:

```powershell
cargo test --manifest-path native-host\Cargo.toml --all-targets
python -m unittest discover -s tests
```

See [the architecture guide](EXPERIMENTAL_ARCHITECTURE.md) and
[protocol v4 specification](PROTOCOL_V4.md) for implementation details.

## License

[MIT](LICENSE). Use of this tool remains subject to the game's terms and
competitive rules.
