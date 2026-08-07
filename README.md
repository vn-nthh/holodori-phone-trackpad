# Holodori Phone Trackpad

Use an Android phone as a multi-touch controller for [hololive Dreams (holodori)](https://store.steampowered.com/app/4282500/hololive_Dreams/). Touches map to keyboard lanes (`S D F J K L` by default) and travel over USB without requiring USB debugging.

> Unofficial community tool. Not affiliated with COVER Corp., hololive production, or QualiArts.

## Experimental USB-tethered UDP branch

The `codex/usb-rndis-udp` branch replaces the latency-path PC runtime with a
native Rust host and carries duplex protocol v4 over USB tethering/RNDIS + UDP.
It is intentionally driver-free: no ADB, root, Android USB accessory mode,
WinUSB, or UsbDk is installed or opened.

- Android retains every frame until Windows acknowledges OS acceptance.
- CRC, sequence ordering, cumulative ACKs, and 4 ms replay cover corruption,
  duplicates, queue delay, and a lost host-to-phone ACK.
- Complete multi-contact snapshots preserve taps, holds, chords, and every
  historical slide sample.
- An 8 ms keepalive sustains stationary Windows contacts above 120 Hz.
- `touch` mode uses the sanctioned Windows Touch API. A separate process sees
  only `WM_POINTER` messages, proving that Windows received touch rather than
  reading the transport stream.
- `keys` mode retains the Holodori lane bridge and emits every crossed lane,
  pressing the next lane before releasing the prior lane.
- The hardware remains exactly phone + one USB data cable + PC.
- Exit-only benchmarking separates Android dispatch, phone queue, duplex
  USB-tethered network,
  and Windows sink latency without writing or sorting during play.
- A 32-byte discovery hello finds the host on the RNDIS adapter without a
  fixed phone IP.

Protocol v4 is intentionally incompatible with the stable v0.2.1 Python host.
See [the experimental architecture](EXPERIMENTAL_ARCHITECTURE.md) and
[protocol specification](PROTOCOL_V4.md).

Build and run the experiment:

```text
cd native-host
cargo build --release
target\release\holodori-native-host.exe --mode touch
```

The touch host opens the independent probe automatically. To retain the
current Holodori keyboard behavior, use:

```text
target\release\holodori-native-host.exe --mode keys --lanes s,d,f,j,k,l
```

Create a distributable experimental bundle with:

```text
.\packaging\build-experimental.ps1
```

The current experimental bundle is published on the
[v0.4.0 GitHub release](https://github.com/vn-nthh/holodori-phone-trackpad/releases/tag/v0.4.0).
The Windows launcher is a small Tauri app; it shows only lane keys, USB port,
latency-report preference, and Start/Stop. It uses the system WebView2 runtime
on supported Windows 10/11, while the latency-critical host remains native
Rust.

## Download v0.2.1

- [Windows installer](https://github.com/vn-nthh/holodori-phone-trackpad/releases/download/v0.2.1/HolodoriPhoneTrackpadSetup.exe)
- [Android app](https://github.com/vn-nthh/holodori-phone-trackpad/releases/download/v0.2.1/HolodoriPhoneTrackpad.apk)
- [Portable Windows app](https://github.com/vn-nthh/holodori-phone-trackpad/releases/download/v0.2.1/HolodoriPhoneTrackpad.exe)
- [Release notes](https://github.com/vn-nthh/holodori-phone-trackpad/releases/tag/v0.2.1)

Version 0.2.1 fixes fast slides occasionally skipping a lane and makes diagnostic logs shorter.

## Quick start

1. Install the Windows app and Android APK.
2. Enable USB tethering on the phone.
3. Connect the phone with a data-capable USB cable.
4. Open `HolodoriUsbController.exe` and press **Start**.
5. Open the Android app, move and resize the phone's play zone, then lock it.
6. Start the game and play. Press **Stop** in the Windows app when finished.

The PC touch overlay is off by default. Use the **with Touch Overlay** shortcut or run with `--overlay` to enable it.

## Features

- Low-latency USB-tethered UDP input; no USB debugging required
- Reliable fast slides, multi-touch chords, and configurable keys
- Windows inbox RNDIS data transport; no custom driver installation
- Movable, resizable, and rotatable phone play zone
- Optional click-through PC touch overlay
- Safe reconnects that release held keys
- Queue and latency diagnostics

## How it works

The Android app sends touch positions as acknowledged binary UDP datagrams over
the USB-tethered RNDIS adapter, and the PC maps them to keyboard presses.
Android's bundled movement history is replayed in order so fast slides cannot
skip lane crossings. Disconnecting or reconnecting releases every held key.

## Diagnostics

The Windows app can save queue, stage, percentile, and jitter numbers when
**Save latency report when stopped** is checked. Press **Stop** to write one
report under `Windows\Logs`. No command prompt is needed.

Queue warnings use the 8.333 ms 120 Hz budget by default. The Windows app lets
users change the UDP port without typing flags. Benchmark latency is relative
jitter against the fastest recent sample, not absolute one-way latency.

## Build

Build the driver-free experimental bundle:

```text
.\packaging\build-experimental.ps1
```

The build requires Rust, JDK 17, and Android SDK Platform 35. Outputs are
written to `release\`.

Run the tests with:

```text
python -m unittest discover -s tests
```

## Troubleshooting

- Use a data-capable USB cable, not a charge-only cable.
- If the host cannot discover the phone, confirm USB tethering is enabled and
  that Windows created an RNDIS/Ethernet adapter.
- Run as Administrator if an elevated game ignores key presses.
- Start the host after Windows creates the RNDIS/Ethernet adapter.
- If the APK will not update from 0.2.0, uninstall the old phone app once and reinstall.

## License

[MIT](LICENSE). Use of this tool remains subject to the game's terms and competitive rules.
