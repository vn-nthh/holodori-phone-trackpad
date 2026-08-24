[English](README.md) | [日本語](README.ja.md)

# Doritrack

Use an Android phone as a six-lane touch controller for
[hololive Dreams (holodori)](https://store.steampowered.com/app/4282500/hololive_Dreams/)
on Windows or Linux.

Tap, hold, slide, and play chords on the phone. The app turns those touches
into lane keys. The default keys are `S D F J K L`.

> This is an unofficial community tool. It is not affiliated with COVER Corp.,
> hololive production, or QualiArts.

## Download

The current version is **v0.4.1**.

- [Windows app](https://github.com/vn-nthh/holodori-phone-trackpad/releases/download/v0.4.1/HolodoriUsbTetheredUdp-v0.4.1-windows-x64.zip)
- Linux app: no prebuilt release yet. Build `HolodoriUsbTetheredUdp-v0.4.1-linux-x64.tar.gz`
  yourself with `packaging/build-linux.sh` (see [For developers](#for-developers)).
- [Android app](https://github.com/vn-nthh/holodori-phone-trackpad/releases/download/v0.4.1/HolodoriUsbTetheredUdp-v0.4.1-android.apk)
- [Release notes](https://github.com/vn-nthh/holodori-phone-trackpad/releases/tag/v0.4.1)

You need:

- a Windows 10 or 11 PC, or a Linux PC;
- an Android phone with USB tethering;
- one USB data cable.

## Quick start

### Windows

1. Download and unzip the Windows app.
2. Install the Android APK on the phone.
3. Connect the phone to the PC with a USB data cable.
4. Turn on **USB tethering** in the phone's settings.
5. Open `HolodoriUsbController.exe` on Windows.
6. Change the six lane keys if needed, then press **Start**.
7. Open the Android app.
8. Move, resize, or rotate the play area, then lock it.
9. Start the game and play.
10. Press **Stop** in the Windows app when finished.

### Linux

1. Build the Linux bundle, or unpack one you already built (see
   [Download](#download)).
2. Install the Android APK on the phone.
3. Connect the phone to the PC with a USB data cable.
4. Turn on **USB tethering** in the phone's settings.
5. Run `./HolodoriUsbController` from the bundle folder.
6. Change the six lane keys if needed, then press **Start**.
7. Open the Android app.
8. Move, resize, or rotate the play area, then lock it.
9. Start the game and play.
10. Press **Stop** in the app when finished.

See [Linux setup](#linux-setup) below for one-time setup: `/dev/uinput` access
and the firewall rule for UDP port 42825.

The current Linux host deliberately accepts only Android tether interfaces
using the kernel's `rndis_host` driver. Generic USB Ethernet/NCM adapters fail
closed because protocol v4 does not yet authenticate the phone.

The name and location of the USB tethering setting depend on the phone. It is
usually under **Network**, **Connections**, **Hotspot**, or **Tethering**.

## What it supports

- Taps become quick key presses.
- Holds stay pressed until the finger lifts, even if it moves just outside the
  play area; the nearest edge lane stays held.
- Slides pass through every crossed lane in order.
- Multiple fingers can hold different lanes at the same time.
- A disconnect releases held keys instead of leaving them stuck.
- Lane keys can be changed in the app.

## How the USB connection works

USB tethering creates a small network link between the phone and the PC. The
Android app sends touch updates over that link with UDP. This keeps setup
simple and avoids special USB drivers. Discovery is accepted only on a
recognized USB-tether subnet, and each session pins one phone and one host
endpoint.

Each update is numbered and checked for damage. Important updates are sent
twice right away. If both copies are lost, the phone tries again after about
2 milliseconds. The PC confirms an update only after the operating system
accepts the key or touch action.

The app keeps the full state of every finger. This lets it preserve holds,
chords, and fast slides, and safely rebuild a hold after a short reconnect.
Old touches are discarded after a broken connection so they do not play late.

The tool sends normal keyboard input to the PC. It does not open, read, or
change the game process.

Protocol v4 does not yet use cryptographic pairing. Treat the accepted USB
network as trusted and do not expose UDP port `42825` to LAN or Wi-Fi. Any
device able to reach that port from the accepted tether subnet can impersonate
a phone and inject lane keys. Authenticated pairing is deferred to protocol v5.

## Windows options

### Lane keys

Select each key box and press the letter or number you want to use. The default
layout is `S D F J K L`.

Test custom keys in Notepad before opening the game. If the game is running as
administrator, run the controller as administrator too.

### Stop the PC from using the phone's internet

USB tethering can make the PC use the phone as an internet connection. Turn on
**Stop the PC from using the phone's internet** if you only want the local
phone-to-PC link.

Windows may ask for administrator access. Use **Restart as admin** when the
launcher offers it. The original network setting is restored after a normal
stop. The launcher also saves a recovery snapshot before changing routes and
repairs an interrupted session on its next start. If that repair needs
elevation, **Restart as admin** appears even when this option is unchecked.
Route protection begins only after the phone is discovered on a confirmed
tether interface. Recovery preserves any newer route Windows or the user added
instead of replacing it with an older captured gateway. If the captured tether
adapter is unplugged before cleanup finishes, its recovery snapshot is retained;
reconnect that adapter so the launcher can finish restoring its owned setting.

The status line distinguishes **Waiting**, **Phone connected**,
**Recovering**, and **Stopping**. Recovering means held input has been released
and the controller is waiting for a fresh phone session; you do not need to
press Start again.

### Save latency report when stopped

Leave this checked if you want a report after playing. Reports are saved under
`Windows\Logs` when you press **Stop**. The app does not write reports while
you are playing.

## Linux setup

### `/dev/uinput` access

Lane keys go through the kernel's `uinput` virtual keyboard. Access is
normally restricted. Do not add your account to the broad `input` group:
that group can often read physical keyboard and mouse devices. Use a
dedicated `uinput` group that grants only virtual-input creation instead.

Access to `/dev/uinput` allows software running as your account to inject
arbitrary keyboard or pointer input. Grant it only to trusted local accounts;
never make the device world-writable.

1. Create the dedicated group if it does not already exist:

   ```sh
   getent group uinput >/dev/null || sudo groupadd --system uinput
   ```

2. Install a udev rule that grants that group access to `/dev/uinput`.
   Create `/etc/udev/rules.d/99-holodori-uinput.rules` containing:

   ```
   KERNEL=="uinput", SUBSYSTEM=="misc", GROUP="uinput", MODE="0660", OPTIONS+="static_node=uinput"
   ```

   `uinput` is normally an on-demand-loaded kernel module rather than one
   built into the kernel, so `OPTIONS+="static_node=uinput"` matters: it is
   what applies this rule's ownership to the device node udev pre-creates
   at boot (before the module has actually loaded), and not only to a node
   created after a manual `modprobe uinput`. Without it, permissions can
   depend on load order and vary across boots.

3. Reload udev and re-trigger it so the rule takes effect without a reboot:

   ```sh
   sudo udevadm control --reload-rules && sudo udevadm trigger
   ```

4. Add your user to the dedicated group:

   ```sh
   sudo usermod -aG uinput "$USER"
   ```

5. Log out and back in. A group change does not apply to an already-open
   session.

6. Verify it worked:

   ```sh
   ls -l /dev/uinput
   ```

   The group should be `uinput` with group read and write, for example
   `crw-rw---- 1 root uinput 10, 223 ... /dev/uinput`.

### USB tethering

The phone appears as a new network interface, and your distribution's
network manager configures it automatically. No driver install is needed.

### Firewall

Allow inbound UDP on port 42825 on the tethered interface. With `ufw`:

```sh
sudo ufw allow in on <tether-interface> to any port 42825 proto udp
```

With `firewalld`:

```sh
sudo firewall-cmd --zone=<tether-zone> --add-port=42825/udp --permanent
sudo firewall-cmd --reload
```

### Stop the PC from using the phone's internet

This option is Windows-only; the checkbox is disabled in the Linux app.

Linux network managers may install a default route for USB tethering, and its
metric can outrank the PC's existing wired or Wi-Fi uplink. Do not assume the
existing uplink will win. If you need to guarantee that the PC never routes
through the phone, configure NetworkManager directly instead of running a
privileged process that rewrites the routing table:

```sh
nmcli connection show
nmcli connection modify <tether-connection-name> ipv4.never-default yes ipv6.never-default yes
nmcli connection up <tether-connection-name>
```

Replace `<tether-connection-name>` with the connection name for the tethered
link from the first command's output. This tells NetworkManager to never
assign that connection's route as the default, permanently, the same as any
other one-time change to a system connection profile (NetworkManager may
prompt for authentication once via polkit, the same as changing this from
its own settings GUI).

### Latency report location

Reports are saved under `$XDG_STATE_HOME/holodori/logs`, or
`~/.local/state/holodori/logs` if `XDG_STATE_HOME` is not set.

## Troubleshooting

### The phone does not connect

- Make sure the cable supports data, not charging only.
- Turn USB tethering off and on again.
- Wait for Windows to create a new Ethernet or USB network connection.
- Open the phone app after pressing **Start** on Windows.
- Allow `holodori-native-host.exe` through Windows Firewall for UDP port
  `42825`.
- Close any second copy of the controller. Only one can use port `42825`.
- On Linux, allow inbound UDP `42825` on the tethered interface instead (see
  [Linux > Firewall](#firewall)).

### The Windows app does not open

The launcher uses Microsoft Edge WebView2, which is normally included with
Windows 10 and 11. Install or update the WebView2 Runtime if the launcher stays
closed.

### The Linux app does not open

The launcher needs the system `webkit2gtk` and `gtk3` libraries. Install them
through your distribution's package manager if the launcher does not start;
check the terminal output for the missing library name.

If the terminal instead shows `Error 71 (Protocol error) dispatching to
Wayland display`, that is a Wayland + NVIDIA driver crash. The launcher
already works around it automatically on Wayland with the proprietary NVIDIA
driver. If you still hit it on another graphics stack, run the launcher with
`WEBKIT_DISABLE_DMABUF_RENDERER=1 ./HolodoriUsbController` as a manual
fallback; see Tauri's
[Linux graphics guide](https://v2.tauri.app/develop/debug/linux-graphics/)
for other options.

### Keys work outside the game but not inside it

If the game runs as administrator, restart the controller as administrator.

### Lane keys do nothing on Linux

The native host could not open `/dev/uinput`. Check `ls -l /dev/uinput`: it
should show group `uinput` with group read and write (`crw-rw----`). If it
does not, the udev rule from [Linux setup > `/dev/uinput`
access](#devuinput-access) is not installed, or was not reloaded. If it does
show that and lane keys still do nothing, confirm you are actually a member
of the `uinput` group (`groups "$USER"`) and that you logged out and back in
after joining it.

### Android refuses to install the APK

An older test build may use a different signing key. Uninstall the old
Holodori controller app, then install the new APK.

## For developers

The live input path uses a native Rust host, an Android app, and a small Tauri
launcher. The protocol is designed around an 8.333 ms frame budget for 120 Hz
play, but real results still depend on the phone, cable, and PC.

Build the release bundle from PowerShell:

```powershell
.\packaging\build-experimental.ps1 `
  -CargoHome "$env:USERPROFILE\.cargo" `
  -JavaHome $env:JAVA_HOME `
  -AndroidSdk $env:ANDROID_SDK_ROOT
```

Or from Bash on Linux:

```sh
packaging/build-linux.sh
```

Run the main test suites:

```powershell
cargo test --manifest-path native-host\Cargo.toml --all-targets
python -m unittest discover -s tests
```

On Linux:

```sh
cargo test --manifest-path native-host/Cargo.toml --all-targets
python -m unittest discover -s tests
```

See [the architecture guide](EXPERIMENTAL_ARCHITECTURE.md) and
[protocol v4 specification](PROTOCOL_V4.md) for implementation details.

## License

[MIT](LICENSE). Use of this tool remains subject to the game's terms and
competitive rules.
