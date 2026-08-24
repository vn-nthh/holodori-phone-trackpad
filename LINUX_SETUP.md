# Linux setup and troubleshooting

This guide contains the Linux-specific setup that is intentionally kept out of
the main README. The current Linux build supports six-lane keyboard mode on an
x86_64 PC. Windows Touch mode remains Windows-only.

## What you need

- An x86_64 Linux PC with a desktop session.
- An Android phone that supports USB tethering.
- A USB data cable.
- Access to an account that can run `sudo` for the one-time input and firewall
  setup.
- The Linux bundle and the Android APK.

The launcher uses the system GTK 3 and WebKitGTK libraries. The input host uses
the kernel's `uinput` device. Install your distribution's GTK 3 and WebKitGTK
packages before starting. Package names differ between distributions.
NetworkManager 1.44 or newer, `nmcli`, and iproute2 are needed only for the
optional setting that keeps the PC off the phone's internet.

There is no prebuilt Linux download yet. Developers can build the bundle from
the repository root:

```sh
packaging/build-linux.sh
```

The script runs the tests and produces a `release/*-linux-x64.tar.gz` archive.
Rust, Node.js/npm, the Tauri Linux build dependencies, and the normal C build
tools must already be installed. An Android SDK and Java 17 are optional unless
you also want the script to build the APK; use `packaging/build-linux.sh --help`
for those options.

## One-time input permission

Lane keys are sent through Linux's `uinput` virtual keyboard. Access is normally
restricted. Use a dedicated `uinput` group; do not add your account to the broad
`input` group, which can often read physical keyboards and mice.

> Access to `/dev/uinput` lets software running as your account inject keyboard
> or pointer input. Grant it only to trusted local accounts. Never use a
> world-writable rule such as `MODE="0666"`.

1. Create the dedicated group if needed:

   ```sh
   getent group uinput >/dev/null || sudo groupadd --system uinput
   ```

2. Create `/etc/udev/rules.d/99-holodori-uinput.rules` with this line:

   ```text
   KERNEL=="uinput", SUBSYSTEM=="misc", GROUP="uinput", MODE="0660", OPTIONS+="static_node=uinput"
   ```

   `OPTIONS+="static_node=uinput"` ensures the permission is applied even when
   the kernel module is loaded on demand. Without it, permissions can depend on
   device load order and vary across boots.

3. Reload the rules:

   ```sh
   sudo udevadm control --reload-rules && sudo udevadm trigger
   ```

4. Add your account to the group:

   ```sh
   sudo usermod -aG uinput "$USER"
   ```

5. Log out and back in. The new group does not apply to an already-open desktop
   session.

6. Verify the device permission:

   ```sh
   ls -l /dev/uinput
   ```

   It should show the `uinput` group with group read and write permission, for
   example `crw-rw---- 1 root uinput 10, 223 ... /dev/uinput`.

## Firewall

Allow inbound UDP port 42825 only on the phone's USB-tether interface. Do not
open it broadly to LAN or Wi-Fi.

With `ufw`:

```sh
sudo ufw allow in on <tether-interface> to any port 42825 proto udp
```

With `firewalld`:

```sh
sudo firewall-cmd --zone=<tether-zone> --add-port=42825/udp --permanent
sudo firewall-cmd --reload
```

Replace the placeholders with the interface or firewall zone created when USB
tethering is enabled. Your network settings UI, `ip link`, or `nmcli device`
can show it.

## Start a play session

1. Unpack the Linux bundle.
2. Install `Android/HolodoriUsbTetheredUdp-v4.apk` on the phone if the bundle
   includes it. Otherwise use the separately supplied Android APK.
3. Connect the phone with a USB data cable and enable USB tethering.
4. Run `./HolodoriUsbController` from the bundle directory.
5. Choose the lane keys and any network or report options.
6. Press **Start**, then open the Android app.
7. Arrange and lock the phone play area.
8. Press **Stop** in the controller when finished.

No ADB, phone root access, custom kernel driver, or controller elevation is
required. Your desktop may display a NetworkManager/polkit authorization prompt
if you enable the optional internet-routing protection.

## Keeping the PC off the phone's internet

USB tethering can add a default internet route through the phone. Enable
**Stop the PC from using the phone's internet** if you want only the local
phone-to-PC controller link.

The automatic toggle requires:

- NetworkManager 1.44 or newer;
- exactly one active Android tether using the kernel `rndis_host` driver;
- `nmcli` and iproute2 in their normal root-owned system locations; and
- any requested NetworkManager/polkit authorization.

Use **Check tether** after connecting or reconnecting the phone. Start remains
disabled if the selected protection is pending, mixed, or cannot be verified.
The checkbox remains selected across a temporary disconnect so an absent phone
cannot silently remove the requested guard.

NetworkManager 1.58 fixes an earlier DHCPv6 reapply case that could leave an
IPv6 default route. On versions 1.44 through 1.56, the controller detects a
remaining route and reports the option as pending. Reconnect the tether and use
**Check tether**, turn the option off, or upgrade NetworkManager. See the
[NetworkManager 1.58 release notes](https://networkmanager.dev/blog/networkmanager-1-58/).

If your distribution does not use NetworkManager, configure the equivalent
policy in its network manager. The manual NetworkManager commands are:

```sh
nmcli connection show
nmcli connection modify <tether-connection-name> ipv4.never-default yes ipv6.never-default yes
nmcli connection up <tether-connection-name>
```

Replace `<tether-connection-name>` with the tether profile shown by the first
command.

### How the toggle is applied

The launcher resolves the exact active connection by UUID and changes only
`ipv4.never-default` and `ipv6.never-default`. Persistent updates use
NetworkManager's profile version guard. Applied updates preserve externally
managed routes.

The launcher then checks every IPv4 and IPv6 routing table and refuses to call
the policy active while a tether default route remains. The native host repeats
the read-only route and device-identity check before acknowledging phone
discovery. It does not add or delete raw Linux routes.

Rollback restores an original value only if the profile version and current
value still belong to that operation. A newer concurrent NetworkManager change
is preserved and reported instead of overwritten. The profile setting remains
across reconnects until you turn the checkbox off or change the profile.

The native host's `--local-only-tether` argument is only an internal read-only
pre-session verifier. It does not configure or persist the NetworkManager
profile by itself.

## Supported tether and pairing limits

Protocol v4 does not cryptographically pair the phone and PC. To reduce the
untrusted surface, the Linux host accepts only Android tether interfaces backed
by the kernel's `rndis_host` driver. Generic USB Ethernet and NCM adapters are
rejected. Broader authenticated device support is deferred to protocol v5.

Treat the accepted USB network as trusted. Any device that can reach UDP port
42825 from that tether network could impersonate the phone and inject lane
keys. Keep the firewall rule limited to the USB-tether interface.

## Latency reports

Reports are written only after **Stop**. They are saved under
`$XDG_STATE_HOME/holodori/logs`, or `~/.local/state/holodori/logs` when
`XDG_STATE_HOME` is not set.

## Troubleshooting

### The phone does not connect

- Confirm the cable supports data, not charging only.
- Turn USB tethering off and on again.
- Wait for the desktop network manager to create the new connection.
- Press **Start** before opening the Android app.
- Confirm UDP port 42825 is allowed on the USB-tether interface.
- Close any second controller instance; only one process can own the port.
- Check that the tether uses `rndis_host`. Unsupported USB Ethernet/NCM links
  are rejected deliberately.

### The launcher does not open

Run `./HolodoriUsbController` from a terminal and look for a missing GTK 3 or
WebKitGTK library. Install the named library through your distribution's
package manager.

If the terminal reports `Error 71 (Protocol error) dispatching to Wayland
display`, the launcher already applies the recommended workaround on Wayland
with the proprietary NVIDIA driver. On another graphics stack, try:

```sh
WEBKIT_DISABLE_DMABUF_RENDERER=1 ./HolodoriUsbController
```

See Tauri's [Linux graphics guide](https://v2.tauri.app/develop/debug/linux-graphics/)
for additional WebKitGTK workarounds.

### Lane keys do nothing

Check `ls -l /dev/uinput`. The device should belong to group `uinput` and show
group read/write permission. Also check `groups "$USER"` and confirm you logged
out and back in after joining the group.

Test the selected lane keys in a text editor before opening the game. Linux
supports keys mode only; Windows Touch mode is not available.

### The internet-protection option is unavailable

Connect exactly one supported Android tether, wait for NetworkManager to make
it active, and press **Check tether**. The control remains unavailable when no
supported profile exists, multiple RNDIS tethers are active, NetworkManager is
not running, required tools are missing or untrusted, or the controller is
already running.

### The internet-protection option stays pending

Reconnect the tether and press **Check tether**. If it remains pending, inspect
the NetworkManager version and upgrade to 1.58 or newer where possible. You can
also turn the option off, but the PC may then use the phone for internet access.

## Further technical information

- [Experimental architecture](EXPERIMENTAL_ARCHITECTURE.md)
- [Protocol v4 specification](PROTOCOL_V4.md)
