HOLODORI LOSSLESS TOUCH - EXPERIMENTAL PROTOCOL V4
==================================================

This bundle is not compatible with the stable v0.2.1 Python host.

Hardware:
  - Android phone
  - One USB data cable
  - Windows 10/11 PC

Install:
  1. Install Drivers\UsbDk_1.0.22_x64.msi if UsbDk is not already installed.
  2. Copy Android\HolodoriLosslessTouch-v4-experimental.apk to the phone.
  3. Open the APK on the phone and approve installation from that source.
     If Android reports a signature conflict, uninstall the existing Holodori
     Controller first; this experimental APK is debug-signed.

Test Windows Touch:
  1. Run run-touch.cmd.
  2. Connect and unlock the phone.
  3. Accept Android's USB accessory prompt.
  4. Arrange and lock the play zone.
  5. Tap, hold, chord, and slide.
  6. The independent probe must count WM_POINTER messages.

Test the Windows API without a phone:
  1. Run Windows\holodori-touch-probe.exe.
  2. Run Windows\holodori-touch-smoke.exe.
  3. Expected: "Windows accepted DOWN + 48 UPDATE + UP touch frames".

Test Holodori keyboard mode:
  1. Release every finger and close the touch host.
  2. Run run-keys.cmd.
  3. Test in Notepad before testing in Holodori.
  4. If Holodori is elevated, run the host elevated too.

Connection diagnostics:
  - "AOA bulk link ready" means Windows opened the phone.
  - "Lossless stream ready" means the phone sent its session frame and the
    host returned the first acknowledgement.
  - This build uses an existing WinUSB-compatible Android Accessory driver
    before falling back to UsbDk.
  - This build can join the phone's active session after a Windows host restart;
    a cable replug is not required.
  - run-keys.cmd and run-touch.cmd collect metrics silently in memory.
  - Press Q then Enter (or Ctrl+C) to stop gracefully. One report is written under
    Windows\Logs. Nothing is formatted, sorted, printed, or written mid-play.
  - Use --metrics-file PATH to choose the output file, or --warn-ms 4.0 for a
    stricter final warning threshold.

Metrics include:
  - mean, max, p50, p90, p99, and p99.9;
  - estimated current touch event-to-Windows latency;
  - separate Android current dispatch and historical batch age;
  - Android callback-to-write, symmetric one-way USB, Windows sink, and ACK time;
  - replay, recovery incident, parser discard, and sink retry counters;
  - all-session tail counts and one correlated worst-event stage breakdown;
  - warnings at the 8.333 ms 120 Hz frame budget by default.

The USB value is half a measured duplex round trip after subtracting the
phone's measured turnaround. It assumes similar delay in both USB directions.

Safety and scope:
  - Release every finger before terminating keyboard mode.
  - The host never opens or modifies the Holodori process.
  - Touch mode uses the Windows Touch API and a separate WM_POINTER receiver.
  - Protocol reliability applies within a connected USB session. A physical
    cable removal creates a new session and old gameplay is not replayed late.

See Docs\EXPERIMENTAL_ARCHITECTURE.md and Docs\PROTOCOL_V4.md for details.
Verify every packaged file against SHA256SUMS.txt.
