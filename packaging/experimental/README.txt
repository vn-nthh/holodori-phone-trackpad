HOLODORI LOSSLESS TOUCH - AUTHENTICATED LOCAL UDP
=================================================

This bundle defaults to authenticated protocol v5 over an explicitly selected
USB-tether or Wi-Fi/local-network path. USB uses Android's normal tethering
network and Windows' inbox RNDIS driver; Wi-Fi requires the phone and PC on the
same private subnet. Legacy protocol v4 is available only as an explicit,
unpaired USB migration option.

Install:
  1. Copy Android\Doritrack-v5.apk to the phone.
  2. Open the APK on the phone and approve installation from that source.
     If Android reports a signature conflict, uninstall the existing Holodori
     Controller first; this experimental APK is debug-signed.
  3. For USB, enable Settings > Network & internet > Hotspot & tethering > USB
     tethering, connect one data cable, and wait for the RNDIS adapter. For
     Wi-Fi, connect the phone and PC to the same private local subnet.

Start the Windows app:
  1. Double-click HolodoriUsbController.exe.
  2. Choose USB or Wi-Fi / local network on both the phone and host.
  3. Press Pair on both. Replicate all eight numbered host lanes on the phone.
     Approve on the host only while the real phone says "Pattern matched".
  4. Set the lane keys if needed. V5 uses UDP port 42825 only on the selected
     interface.
  5. Leave "Save latency report when stopped" checked unless you do not want
     a report.
  6. Optional for USB only: check "Stop the PC from using the phone's internet"
     to remove the phone's temporary internet gateway while Holodori is
     running. Use "Restart as admin" if Windows needs elevation.
  7. Press Start on the host and phone with the same transport selected.
  8. Arrange and lock the play zone, optionally enable thumb mode, then tap,
     hold, chord, and slide.
  9. Press Stop in the app when finished. Held input is released safely and
     the original tether routes are restored.

Portable Windows app:
  - The folder is self-contained for Holodori and needs no installer or custom
    driver.
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
  - "Pairing window open" means HPP5 discovery is confined to the selected
    interface for 60 seconds.
  - If Windows Firewall blocks discovery, permit inbound UDP 42825 for
    Windows\holodori-native-host.exe only on the selected private interface.
  - "Phone connected" means Noise IK authenticated the remembered phone and
    HPT5/HPA5 control is active.
  - Each immediate copy and 2 ms repair is independently encrypted with a new
    packet number; cumulative ACK advances only after Windows accepts input.
  - Wi-Fi pairing reports signal and authenticated path timing. 2.4, 5, and
    6 GHz are accepted; poor measurements warn but never decide identity.
  - A host read/ACK failure releases injected input before reconnecting. During
    active play the phone abandons a link after 64 ms without cumulative ACK
    progress, starts socket recovery with a 4 ms backoff, and restores
    still-held contacts from its latest snapshot.
  - HolodoriUsbController.exe collects metrics silently in memory when the
    report option is checked. One report is written under Windows\Logs after
    Stop. Nothing is formatted, sorted, or written mid-play.
  - V5 and explicit legacy v4 use fixed UDP port 42825. V4 never runs on the
    Wi-Fi listener. The 8.333 ms 120 Hz warning budget is applied automatically.

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
  - During gameplay, 64 ms without cumulative ACK advancement makes the phone
    drop queued gameplay and require fresh Noise IK plus session-start CANCEL,
    so old input is not replayed late. Duplicate controls do not hide a stall.

See Docs\EXPERIMENTAL_ARCHITECTURE.md, Docs\PROTOCOL_V5.md, and
Docs\PROTOCOL_V5_TEST_VECTORS.md for current details. PROTOCOL_V4.md documents
the explicit legacy migration mode.
Verify every packaged file against SHA256SUMS.txt.
