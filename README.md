# Holodori Phone Trackpad

Use an Android phone as a multi-touch controller for [hololive Dreams (holodori)](https://store.steampowered.com/app/4282500/hololive_Dreams/). Touches map to keyboard lanes (`S D F J K L` by default) and travel over USB without requiring USB debugging.

> Unofficial community tool. Not affiliated with COVER Corp., hololive production, or QualiArts.

## Download v0.2.1

- [Windows installer](https://github.com/vn-nthh/holodori-phone-trackpad/releases/download/v0.2.1/HolodoriPhoneTrackpadSetup.exe)
- [Android app](https://github.com/vn-nthh/holodori-phone-trackpad/releases/download/v0.2.1/HolodoriPhoneTrackpad.apk)
- [Portable Windows app](https://github.com/vn-nthh/holodori-phone-trackpad/releases/download/v0.2.1/HolodoriPhoneTrackpad.exe)
- [Release notes](https://github.com/vn-nthh/holodori-phone-trackpad/releases/tag/v0.2.1)

Version 0.2.1 fixes fast slides occasionally skipping a lane and makes diagnostic logs shorter.

## Quick start

1. Install the Windows app and Android APK.
2. Keep the WinUSB and UsbDk options selected during Windows setup.
3. Connect the phone with a data-capable USB cable.
4. Approve Android's accessory prompt.
5. Move and resize the phone's play zone, then lock it.
6. Start the game and play.

The PC touch overlay is off by default. Use the **with Touch Overlay** shortcut or run with `--overlay` to enable it.

## Features

- Low-latency Android Open Accessory input; no USB debugging required
- Reliable fast slides, multi-touch chords, and configurable keys
- WinUSB data transport with automatic UsbDk fallback
- Movable, resizable, and rotatable phone play zone
- Optional click-through PC touch overlay
- Safe reconnects that release held keys
- Queue and latency diagnostics
- Legacy ADB mode

## Useful commands

```text
python phone_trackpad.py
python phone_trackpad.py --keys a s d f j k l
python phone_trackpad.py --overlay
python phone_trackpad.py --aoa-benchmark
python phone_trackpad.py --diagnose
python phone_trackpad.py --test
python phone_trackpad.py --transport adb
```

Run `python phone_trackpad.py --help` for every option.

## How it works

The Android app sends touch positions over AOA USB, and the PC maps them to keyboard presses. Android's bundled movement history is replayed in order so fast slides cannot skip lane crossings. WinUSB handles the live stream when available; UsbDk performs the accessory handshake and acts as a fallback. Disconnecting or reconnecting releases every held key.

## Diagnostics

Use `--aoa-benchmark` for queue, stage, percentile, and jitter numbers. Use `--diagnose` for connection status, safe retry steps, and a privacy-preserving report.

Queue warnings start at 8 ms, stale input resets at 25 ms, and the final failsafe is 100 ms. Benchmark latency is relative jitter against the fastest recent sample, not absolute one-way latency.

## Build

Install Python dependencies, then build all packages:

```text
python -m pip install -r requirements.txt
.\packaging\build.ps1 -Target All
```

The Windows installer build requires Inno Setup 6. The Android build requires JDK 17 and Android SDK Platform 35. Outputs are written to `release\`.

Run the tests with:

```text
python -m unittest discover -s tests
```

## Troubleshooting

- Use a data-capable USB cable, not a charge-only cable.
- Restart Windows once if the installer asks after installing UsbDk.
- Do not replace the phone's normal MTP driver with WinUSB.
- Run as Administrator if an elevated game ignores key presses.
- Use `--test` before sending keys into the game.
- ADB mode requires Android Platform Tools and USB debugging.
- If the APK will not update from 0.2.0, uninstall the old phone app once and reinstall.

## License

[MIT](LICENSE). Use of this tool remains subject to the game's terms and competitive rules.
