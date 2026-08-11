HOLODORI LOSSLESS TOUCH - USB TETHERING / RNDIS / UDP
========================================================

This experimental bundle uses the phone's normal USB tethering network. It
does not use ADB, root, Android USB accessory mode, WinUSB, UsbDk, or a custom
Windows driver. Windows supplies its inbox RNDIS network driver.

Install:
  1. Copy Android\HolodoriUsbTetheredUdp-v4.apk to the phone.
  2. Open the APK on the phone and approve installation from that source.
     If Android reports a signature conflict, uninstall the existing Holodori
     Controller first; this experimental APK is debug-signed.
  3. On the phone, enable Settings > Network & internet > Hotspot & tethering
     > USB tethering. The exact labels vary by Android vendor.
  4. Connect the phone to Windows with one USB data cable and wait for Windows
     to finish creating the RNDIS/Ethernet adapter.

Start the Windows app:
  1. Double-click HolodoriUsbController.exe.
  2. Set the lane keys if needed. Protocol-v4 discovery uses UDP port 42825.
  3. Leave "Save latency report when stopped" checked unless you do not want
     a report.
  4. Optional: check "Stop the PC from using the phone's internet" to remove
     the phone's temporary internet gateway while Holodori is running. Use
     "Restart as admin" if Windows needs elevation.
  5. Press Start.
  6. Unlock the phone and open the APK.
  7. Arrange and lock the play zone, then tap, hold, chord, and slide.
  8. Press Stop in the app when finished. Held input is released safely and
     the original tether routes are restored.

Portable Windows app:
  - The folder is self-contained for Holodori: no installer, ADB, root,
    UsbDk, WinUSB, or custom driver is included or used.
  - The Tauri UI uses the Microsoft Edge WebView2 runtime supplied by supported
    Windows 10/11 installations. If the app does not open, install or update
    the WebView2 Runtime once from Microsoft.

Test the Windows API without a phone:
  1. Run Windows\holodori-touch-probe.exe.
  2. Run Windows\holodori-touch-smoke.exe.
  3. Expected: "Windows accepted DOWN + 48 UPDATE + UP touch frames".

Keyboard mode:
  - Edit the lane keys in HolodoriUsbController.exe. Test in Notepad before
    testing in Holodori.
  - If Holodori is elevated, run the launcher elevated too.

Connection diagnostics:
  - "waiting for USB-tethered phone on UDP port 42825" means discovery is
    listening on the RNDIS adapter.
  - If Windows Firewall blocks discovery, permit inbound UDP 42825 for
    Windows\holodori-native-host.exe on the USB-tethered network only.
  - "UDP link ready" means the host received the phone's discovery hello.
  - "Lossless UDP over USB tethering connected" means HPA4 control is active.
  - HPT4 frames are one UDP datagram each. Immediate redundant HPT4/HPA4 sends,
    cumulative acknowledgements, and 2 ms replay preserve ordering across a
    lost or corrupt datagram inside one 120 Hz frame.
  - The host can join a phone session after a host restart; a cable replug is
    not required if USB tethering remains enabled.
  - A host read/ACK failure releases injected input before reconnecting. During
    active play the phone abandons a link after 64 ms without cumulative ACK
    progress, starts socket recovery with a 4 ms backoff, and restores
    still-held contacts from its latest snapshot.
  - HolodoriUsbController.exe collects metrics silently in memory when the
    report option is checked. One report is written under Windows\Logs after
    Stop. Nothing is formatted, sorted, or written mid-play.
  - Protocol-v4 discovery uses fixed UDP port 42825. The 8.333 ms 120 Hz
    warning budget is applied automatically; normal users do not need a
    terminal.

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
  - The separately shipped touch probe uses the Windows Touch API and a
    separate WM_POINTER receiver for diagnostics only; the GUI launches keys
    mode.
  - Protocol reliability applies within a connected tethering session. During
    gameplay, 64 ms without cumulative ACK advancement makes the phone drop
    queued gameplay, start a new session, and send CANCEL so old input is not
    replayed late. Duplicate controls do not hide an ordering stall.

See Docs\EXPERIMENTAL_ARCHITECTURE.md and Docs\PROTOCOL_V4.md for details.
Verify every packaged file against SHA256SUMS.txt.
