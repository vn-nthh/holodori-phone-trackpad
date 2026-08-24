HOLODORI LOSSLESS TOUCH - USB TETHERING / RNDIS / UDP (LINUX)
========================================================

This experimental bundle uses the phone's normal USB tethering network. It
does not use ADB, root, Android USB accessory mode, or a custom Linux driver.
Your distribution's kernel and network manager supply the USB network device.
The current host deliberately accepts only the kernel's rndis_host driver;
generic USB Ethernet/NCM adapters fail closed because protocol v4 cannot
authenticate them.

Protocol v4 has no cryptographic pairing. Treat the accepted USB network as
trusted and do not expose UDP 42825 to LAN or Wi-Fi. Any device able to reach
that port from the accepted tether subnet can impersonate a phone and inject
lane keys. Authenticated pairing is deferred to protocol v5.

Lane keys are delivered through the kernel's uinput virtual keyboard. Keys
mode is the only mode on Linux; touch mode is Windows-only.

One-time setup:
  1. Give your user access to /dev/uinput through a dedicated group. Do not
     use the broad `input` group: it can often read physical keyboard and
     mouse devices. /dev/uinput access still permits arbitrary input
     injection, so grant it only to trusted local accounts and never use
     MODE="0666".
       getent group uinput >/dev/null || sudo groupadd --system uinput
     Create /etc/udev/rules.d/99-holodori-uinput.rules containing:
       KERNEL=="uinput", SUBSYSTEM=="misc", GROUP="uinput", MODE="0660", OPTIONS+="static_node=uinput"
     Then run:
       sudo udevadm control --reload-rules && sudo udevadm trigger
       sudo usermod -aG uinput "$USER"
     Log out and back in for the group change to take effect.
  2. Allow inbound UDP 42825 on the USB-tethered interface. Example with
     ufw:
       sudo ufw allow in on <tether-interface> to any port 42825 proto udp
     On firewalld:
       sudo firewall-cmd --zone=<tether-zone> --add-port=42825/udp --permanent
       sudo firewall-cmd --reload

Install:
  1. Copy Android/HolodoriUsbTetheredUdp-v4.apk to the phone (if this bundle
     includes an Android build; the experimental build script skips the APK
     when no Android SDK is available).
  2. Open the APK on the phone and approve installation from that source.
     If Android reports a signature conflict, uninstall the existing Holodori
     Controller first; this experimental APK is debug-signed.
  3. On the phone, enable Settings > Network & internet > Hotspot & tethering
     > USB tethering. The exact labels vary by Android vendor.
  4. Connect the phone to the PC with one USB data cable. Your network
     manager (NetworkManager, systemd-networkd, ...) creates a new network
     interface for the tethered link automatically; no driver install is
     needed.

Start the Linux app:
  1. Run ./HolodoriUsbController from this folder.
  2. Set the lane keys if needed. Protocol-v4 discovery uses UDP port 42825.
  3. Leave "Save latency report when stopped" checked unless you do not want
     a report.
  4. "Stop the PC from using the phone's internet" is Windows-only; the
     checkbox is disabled in this build. Linux network managers may install
     a default route for USB tethering, and its metric can outrank the PC's
     wired or Wi-Fi uplink. Do not assume the existing uplink will win. To
     guarantee that the PC never routes through the phone, configure
     NetworkManager directly instead of running a privileged process that
     rewrites the routing table:
       nmcli connection show
       nmcli connection modify <tether-connection-name> ipv4.never-default yes ipv6.never-default yes
       nmcli connection up <tether-connection-name>
     Replace <tether-connection-name> with the connection name for the
     tethered link from the first command's output.
  5. Press Start.
  6. Unlock the phone and open the APK.
  7. Arrange and lock the play zone, then tap, hold, chord, and slide.
  8. Press Stop in the app when finished. Held input is released safely.
      Holodori does not change Linux routes; any NetworkManager setting from
      step 4 remains in place until you change it yourself.

Portable Linux app:
  - The folder is self-contained for Holodori: no installer, ADB, root, or
    custom driver is included or used.
  - The Tauri UI uses the system webkit2gtk/GTK libraries. Install them with
    your distribution's package manager if the app does not start.

Keyboard mode:
  - Edit the lane keys in HolodoriUsbController. Test in a text editor before
    testing in Holodori.
  - Touch mode and the touch-probe/touch-smoke diagnostic tools are
    Windows-only and are not included in this bundle.

Connection diagnostics:
  - "waiting for USB-tethered phone on UDP port 42825" means discovery is
    listening on the tethered adapter.
  - If a firewall blocks discovery, allow inbound UDP 42825 on the
    USB-tethered interface (see one-time setup above).
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
  - HolodoriUsbController collects metrics silently in memory when the report
    option is checked. One report is written under
    $XDG_STATE_HOME/holodori/logs (or ~/.local/state/holodori/logs) after
    Stop. Nothing is formatted, sorted, or written mid-play.
  - Protocol-v4 discovery uses fixed UDP port 42825. The 8.333 ms 120 Hz
    warning budget is applied automatically; normal users do not need a
    terminal.

Metrics include:
  - mean, max, p50, p90, p99, and p99.9;
  - estimated current touch event-to-host-input latency;
  - separate Android current dispatch and historical batch age;
  - Android callback-to-write, symmetric one-way network, keyboard sink, and
    ACK time;
  - replay, recovery incident, parser discard, and sink retry counters;
  - all-session tail counts and one correlated worst-event stage breakdown;
  - warnings at the 8.333 ms 120 Hz frame budget by default.

Safety and scope:
  - Release every finger before terminating keyboard mode.
  - The host never opens or modifies the Holodori process.
  - Protocol reliability applies within a connected tethering session. During
    gameplay, 64 ms without cumulative ACK advancement makes the phone drop
    queued gameplay, start a new session, and send CANCEL so old input is not
    replayed late. Duplicate controls do not hide an ordering stall.

See Docs/EXPERIMENTAL_ARCHITECTURE.md and Docs/PROTOCOL_V4.md for details.
Verify every packaged file against SHA256SUMS.txt.
