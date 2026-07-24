# Holodori Phone Trackpad

**Community support script for [hololive Dreams (holodori)](https://store.steampowered.com/app/4282500/hololive_Dreams/)** — use an Android phone as a multi-touch rhythm controller for the PC (Steam) client.

Holodori’s keyboard layout is built for hands on a desk. This tool turns your phone screen into a configurable play zone that maps touches to keys (`S D F J K L` by default), so you can play with fingers on glass the way a mobile rhythm game expects.

> **Unofficial.** Not affiliated with COVER Corp., hololive production, or QualiArts. Use at your own risk and follow the game’s Terms of Service.

## Features

- **Phone as trackpad** — raw multi-touch via ADB (`getevent`), not a soft keyboard
- **Play-zone UI on the phone** — drag, resize, and rotate the hit area; lock when ready
- **Drag notes** — seamless key transitions when sliding across lanes (no mid-slide gaps)
- **Up to 10 fingers** — multi-touch for chords and simultaneous notes
- **Configurable keys** — default 6-key Holodori layout; override with `--keys`
- **Low interruption mode** — keep screen on, immersive UI, quiet notifications during play
- **Saved layout** — play zone and device settings stored in `config.json`

## Requirements

| Requirement | Notes |
|-------------|--------|
| **Windows PC** | Uses Win32 `keybd_event` for key injection |
| **Python 3.7+** | Standard library only (no pip packages) |
| **ADB** | Android Platform Tools, or any `adb` on `PATH` |
| **Android phone** | USB Debugging enabled, USB cable |
| **hololive Dreams (PC)** | Steam client (or any app that accepts the mapped keys) |

## Quick start

1. Enable **Developer options** → **USB debugging** on your phone.
2. Connect the phone with USB and accept the debugging prompt.
3. Install [Android Platform Tools](https://developer.android.com/tools/releases/platform-tools) (or ensure `adb` is on your `PATH`).
4. On the PC, open Holodori and focus the game window when ready.
5. Run:

```bash
python phone_trackpad.py
```

6. On the phone, position the play zone over your preferred touch area, then tap **LOCK**.
7. Play. Quit with `Ctrl+C` on the PC.

### Holodori default keys

| Lanes (left → right) | Keys |
|----------------------|------|
| 6-key (default)      | `S` `D` `F` `J` `K` `L` |

Custom layout example:

```bash
python phone_trackpad.py --keys a s d f j k l
```

## CLI options

```text
python phone_trackpad.py [options]

  --keys KEY [KEY ...]   Keys left-to-right (default: s d f j k l)
  --device PATH          Touch device, e.g. /dev/input/event2
  --adb PATH             Path to adb.exe if not on PATH
  --test                 Print touch→key events; do not send keys
  --selftest             Type "hello" into the focused window
  --no-ui                Skip phone controller UI (full-screen fallback)
```

## How it works

```text
  Phone touchscreen  ──ADB getevent──►  PC script  ──keybd_event──►  Holodori (focused)
         │                                  │
         └── controller.html (HTTP) ────────┘  play zone + lock state
```

1. ADB streams multitouch events from `/dev/input/event*`.
2. Touches are mapped into columns of a user-defined play rectangle (position, size, rotation).
3. Press/release (and drag lane changes) become Windows keyboard events.
4. An optional local HTTP server serves the on-phone UI and receives zone updates.

## Files

| File | Role |
|------|------|
| `phone_trackpad.py` | Main script (ADB, touch map, key inject, HTTP server) |
| `controller.html` | Phone UI: resizable/rotatable play zone + lock |
| `blank_screen.html` | Minimal fullscreen overlay when UI is disabled |
| `config.json` | Auto-created local settings (gitignored) |

## Tips

- Run as **Administrator** if keys don’t reach the game (some titles need elevated input).
- Keep the game window **focused** while playing.
- If the touchscreen isn’t detected: `adb shell getevent -lp`, then pass `--device /dev/input/eventN`.
- Use `--test` first to verify lanes without sending keys into Holodori.
- Cable connection is recommended; wireless ADB works but adds latency.

## Disclaimer

This project is a **fan-made accessibility / input helper**. It does not modify game files or network traffic. Rhythm-game competitive integrity and ToS compliance are your responsibility. “hololive Dreams”, “holodori”, and related names are trademarks of their respective owners.

## License

MIT — see [LICENSE](LICENSE).
