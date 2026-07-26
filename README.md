# Holodori Phone Trackpad

**Community support tool for [hololive Dreams (holodori)](https://store.steampowered.com/app/4282500/hololive_Dreams/)**: use an Android phone as a multi-touch rhythm controller for the PC game.

Holodori's keyboard layout is built for hands on a desk. This tool turns a phone screen into a configurable play zone that maps touches to keys (`S D F J K L` by default), so players can use fingers on glass as they would in a mobile rhythm game.

> **Unofficial.** Not affiliated with COVER Corp., hololive production, or QualiArts. Use at your own risk and follow the game's Terms of Service.

## Download v0.1.2

Install the Windows setup and Android companion app, connect the phone with a
data-capable USB cable, and approve Android's accessory prompt. The Windows
setup includes the signed UsbDk USB driver needed for the normal AOA connection;
approve its administrator prompt and restart only if setup asks you to.
The PC touch overlay is off by default, keeping play unobstructed. Launch the
separate **with Touch Overlay** Start-menu shortcut when you want it.

- [Download the Windows setup (`HolodoriPhoneTrackpadSetup.exe`)](https://github.com/vn-nthh/holodori-phone-trackpad/releases/download/v0.1.2/HolodoriPhoneTrackpadSetup.exe)
- [Download the Android app (`HolodoriPhoneTrackpad.apk`)](https://github.com/vn-nthh/holodori-phone-trackpad/releases/download/v0.1.2/HolodoriPhoneTrackpad.apk)
- [Portable Windows app (`HolodoriPhoneTrackpad.exe`)](https://github.com/vn-nthh/holodori-phone-trackpad/releases/download/v0.1.2/HolodoriPhoneTrackpad.exe)
- [View the v0.1.2 release notes](https://github.com/vn-nthh/holodori-phone-trackpad/releases/tag/v0.1.2)

The original ADB transport remains available if you prefer it. ADB mode still
requires USB debugging; AOA mode does not.

This release also includes an experimental PC touch overlay. The phone sends
live touch positions rather than virtual button presses, and the overlay
mirrors those positions in real time. It is an early proof of concept that
direct touch transfer is practical; the PC maps the received coordinates to
game keys only after they arrive.

The project is free and open source under the [MIT License](LICENSE).

## Features

- **No USB debugging required**: the native Android app communicates through Android Open Accessory (AOA)
- **Low-latency native touch capture**: requests unbuffered Android input dispatch and sends compact binary events
- **Phone play-zone editor**: drag to position, pinch to resize or rotate, then lock for play
- **PC touch overlay**: mirrors fingertips inside a custom click-through zone over the game
- **Drag notes**: presses the new lane before releasing the old lane during transitions
- **Multi-touch chords**: tracks independent fingers and reference-counts fingers sharing a lane
- **Configurable keys**: defaults to six Holodori lanes but supports custom layouts
- **Legacy ADB fallback**: the original raw `getevent` transport remains available

## Requirements

| Requirement | Notes |
|---|---|
| **Windows PC** | Uses Win32 key injection and a topmost transparent overlay |
| **Windows setup** | Bundles the PC app and UsbDk; no Python installation required |
| **Android phone** | AOA-capable phone, companion APK, and data-capable USB cable |
| **Windows USB access** | The setup bundles UsbDk for the initial handshake, or use an AOA interface already bound to WinUSB |
| **hololive Dreams (PC)** | Steam client, or another focused app accepting the mapped keys |

## Quick start

1. Download the [Windows setup](https://github.com/vn-nthh/holodori-phone-trackpad/releases/download/v0.1.1/HolodoriPhoneTrackpadSetup.exe) and [Android app](https://github.com/vn-nthh/holodori-phone-trackpad/releases/download/v0.1.1/HolodoriPhoneTrackpad.apk).
2. Run the Windows setup. Leave **Install UsbDk USB connection support** selected unless you already use WinUSB for the Android Accessory interface. Approve the administrator prompt, then restart Windows only if setup asks you to.
3. Install the APK on the phone, then launch Holodori Phone Trackpad from the Start menu or desktop shortcut.
4. When running from source instead, install the PC dependency:

   ```text
   python -m pip install -r requirements.txt
   ```

5. Connect the phone with a data-capable USB cable. Accept the Android USB-access prompt and choose Holodori Trackpad if Android asks which app to open.
6. Open Holodori in your preferred display mode.
7. When running from source, launch:

   ```text
   python phone_trackpad.py
   ```

8. On the phone, position the play zone, pinch to resize or rotate, then tap the lock button.
9. The PC overlay is off by default. To enable it, launch **Holodori Phone Trackpad (with Touch Overlay)** from the Start menu, or run `HolodoriPhoneTrackpad.exe --overlay`. Position and resize it from any edge or corner, then press `Enter`. During play, click **Edit zone** or use `Ctrl+Shift+O` to edit it later.

## Build distributable packages

Run the packaging script from PowerShell:

```text
.\packaging\build.ps1 -Target All
```

The Windows build creates `release\HolodoriPhoneTrackpad.exe` plus
`release\HolodoriPhoneTrackpadSetup.exe`, which bundles the signed UsbDk 1.0.22
x64 installer. Building the setup requires [Inno Setup 6](https://jrsoftware.org/isinfo.php).
Its settings are stored in `%LOCALAPPDATA%\Holodori Phone Trackpad`, so they
survive the temporary extraction used by the single-file executable.

The Android build requires JDK 17 or newer and an Android SDK containing
Platform 35. Set `JAVA_HOME` and `ANDROID_SDK_ROOT`, or pass `-JavaHome` and
`-AndroidSdk` to the script. It creates the installable, development-signed
`release\HolodoriPhoneTrackpad.apk`.

Build only one package with `-Target Windows` or `-Target Android`.

### Default keys

| Lanes, left to right | Keys |
|---|---|
| 6-key default | `S` `D` `F` `J` `K` `L` |

Custom layout:

```text
python phone_trackpad.py --keys a s d f j k l
```

## CLI options

```text
python phone_trackpad.py [options]

  --keys KEY [KEY ...]   Keys left-to-right (default: s d f j k l)
  --transport aoa|adb    USB transport (default: aoa)
  --overlay              Show the PC touch-position overlay (off by default)
  --no-overlay           Keep the PC touch-position overlay disabled (the default)
  --overlay-edit         Open the saved PC overlay zone for editing
  --usb-vid VID          Add an unlisted Android USB vendor ID
  --no-usbdk             Use an installed WinUSB driver instead of UsbDk
  --test                 Show input events without sending keys

Legacy ADB options:

  --device PATH          Touch device, for example /dev/input/event2
  --adb PATH             Path to adb.exe if it is not on PATH
  --no-ui                Skip the legacy browser controller
```

## How it works

```text
Native Android app ──AOA USB bulk──► PC input router ──key event──► game
        │                                  │
        │ normalized touch coordinates    └──────────────► PC overlay
        └── native play-zone editor
```

1. The Android view requests unbuffered touch dispatch and maps screen coordinates into its rotated play zone.
2. Fixed 24-byte records carry finger ID, action, normalized coordinates, sequence, and source timestamp over AOA.
3. The PC immediately maps records to key presses and releases.
4. A separate queue mirrors coordinates to the visual overlay, so drawing never blocks the input path.
5. If USB disconnects, the PC releases every held key and automatically looks for the phone again.

## Legacy ADB fallback

Use the original raw-event transport while testing AOA compatibility:

```text
python phone_trackpad.py --transport adb
```

ADB mode still requires Android Platform Tools, USB debugging, and the original browser controller.

## Files

| File | Role |
|---|---|
| `phone_trackpad.py` | Main entry point, key injection, and legacy ADB mode |
| `aoa_transport.py` | AOA handshake, binary parser, and USB reconnect loop |
| `aoa_mode.py` | Multi-touch key-state routing |
| `touch_overlay.py` | Transparent, click-through PC touch overlay |
| `android-app/` | Native Android companion app |
| `controller.html` | Legacy ADB phone UI |
| `tests/` | Protocol and input-state tests |

## Troubleshooting

- The Windows setup installs UsbDk automatically when it is not already present. If you use the portable EXE, install [UsbDk 1.0.22 x64](https://www.spice-space.org/download/windows/usbdk/UsbDk_1.0.22_x64.msi) separately.
- Restart Windows once after installing or reinstalling UsbDk; its USB filter does not attach to already-running USB controllers.
- If it cannot claim `18D1:2D00`, bind WinUSB to the **Android Accessory** interface only.
- Do not replace the phone's normal MTP driver with WinUSB. UsbDk avoids that device-wide replacement.
- The overlay works over ordinary and borderless windows. True exclusive fullscreen can bypass desktop composition.
- Run as Administrator if key events do not reach an elevated game.
- Use `--test` before sending keys into the game.
- For legacy ADB touchscreen detection, run `adb shell getevent -lp`, then pass `--device /dev/input/eventN`.

## Disclaimer

This project is a fan-made accessibility and input helper. It does not modify game files or network traffic. Rhythm-game competitive integrity and Terms of Service compliance are the user's responsibility. “hololive Dreams,” “holodori,” and related names are trademarks of their respective owners.

## License

MIT, see [LICENSE](LICENSE).
