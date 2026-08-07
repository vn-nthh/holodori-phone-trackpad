HOLODORI LOSSLESS TOUCH - USB TETHERING / RNDIS / UDP
========================================================

This experimental bundle uses the phone's normal USB tethering network. It
does not use ADB, root, Android USB accessory mode, WinUSB, UsbDk, or a custom
Windows driver. Windows supplies its inbox RNDIS network driver.

Install:
  1. Copy Android\HolodoriUsbTetheredUdp-v4-alpha10.apk to the phone.
  2. Open the APK on the phone and approve installation from that source.
     If Android reports a signature conflict, uninstall the existing Holodori
     Controller first; this experimental APK is debug-signed.
  3. On the phone, enable Settings > Network & internet > Hotspot & tethering
     > USB tethering. The exact labels vary by Android vendor.
  4. Connect the phone to Windows with one USB data cable and wait for Windows
     to finish creating the RNDIS/Ethernet adapter.

Test Windows Touch:
  1. Run run-touch.cmd.
  2. Unlock the phone and open the APK.
  3. Arrange and lock the play zone.
  4. Tap, hold, chord, and slide.
  5. The independent probe must count WM_POINTER messages.

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
  - "waiting for USB-tethered phone on UDP port 42825" means discovery is
    listening on the RNDIS adapter.
  - "UDP link ready" means the host received the phone's discovery hello.
  - "Lossless UDP over USB tethering connected" means HPA4 control is active.
  - HPT4 frames are one UDP datagram each; HPA4 cumulative acknowledgements
    and 4 ms replay preserve ordering across lost datagrams.
  - The host can join a phone session after a host restart; a cable replug is
    not required if USB tethering remains enabled.
  - run-keys.cmd and run-touch.cmd collect metrics silently in memory.
  - Press Q then Enter (or Ctrl+C) to stop gracefully. One report is written
    under Windows\Logs. Nothing is formatted, sorted, printed, or written
    mid-play.
  - Use --udp-port PORT for a different port, --metrics-file PATH to choose
    the output file, or --warn-ms 4.0 for a stricter final warning threshold.

Metrics include:
  - mean, max, p50, p90, p99, and p99.9;
  - estimated current touch event-to-Windows latency;
  - separate Android current dispatch and historical batch age;
  - Android callback-to-write, symmetric one-way network, Windows sink, and
    ACK time;
  - replay, recovery incident, parser discard, and sink retry counters;
  - all-session tail counts and one correlated worst-event stage breakdown;
  - warnings at the 8.333 ms 120 Hz frame budget by default.

Safety and scope:
  - Release every finger before terminating keyboard mode.
  - The host never opens or modifies the Holodori process.
  - Touch mode uses the Windows Touch API and a separate WM_POINTER receiver.
  - Protocol reliability applies within a connected tethering session. After
    two seconds without host control, the phone drops queued gameplay, starts
    a new session, and sends CANCEL so old input is not replayed late.

See Docs\EXPERIMENTAL_ARCHITECTURE.md and Docs\PROTOCOL_V4.md for details.
Verify every packaged file against SHA256SUMS.txt.
