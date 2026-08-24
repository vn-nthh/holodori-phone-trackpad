# Experimental lossless touch architecture

This branch is a USB-tethering/RNDIS + UDP experiment for Windows and Linux. It
is deliberately not wire-compatible with the stable Python release and does
not use ADB, root, Android USB accessory mode, WinUSB, UsbDk, or a custom host
driver. The only physical link is the phone's normal USB cable; the operating
system's USB-network driver carries the UDP traffic.

## Data path

```text
Android MotionEvent
  -> complete chronological contact frame
  -> retained v4 queue with benchmark timestamps
  -> HPT4 one-datagram UDP send over USB tethering/RNDIS
  -> Rust UDP receiver + CRC + sequence reorder/dedup
  -> selected host OS sink (Windows Touch/keys or Linux uinput keys)
  -> HPA4 cumulative ACK datagram
  -> Android retires acknowledged frames
```

The ACK is deliberately after the sink. A valid packet that the operating
system has not accepted remains unacknowledged and is retried; parsing alone
is not success.
UDP is used as a low-overhead datagram framing layer, not as an invitation to
drop input: the retained queue and cumulative ACK make the live path
loss-aware.

## Discovery and packet boundaries

The Android app binds one ephemeral UDP socket and broadcasts a 32-byte,
CRC-protected `HPTD` discovery hello to port 42825 only through conservative
USB-tether candidates. The host listens on `0.0.0.0:42825` so a tether adapter
can appear dynamically, but replies only when the source is in one recognized,
unambiguous USB-tether prefix. Both hosts use `IP_PKTINFO` receive metadata to
require discovery and gameplay datagrams to arrive on that exact interface.
Linux additionally accepts only the kernel's `rndis_host` driver, so generic
CDC Ethernet/NCM devices and cross-interface source spoofing fail closed while
protocol v4 cannot authenticate identity. The phone independently verifies
that the acknowledgement came from its selected tether subnet and pins the
first host IP/port for the session. No fixed phone IP is assumed.

Discovery repeats so a host restart does not require toggling USB tethering.
A same-IP source-port change is adopted in place only after the host reconfirms
the peer's unambiguous tether prefix, route, and exact original adapter. Failed
revalidation aborts to discovery through a clean input boundary. A changed
discovery session also releases injected input and requires a fresh
session-start `CANCEL`. Different-IP hellos and wrong-session gameplay cannot
retarget an established link.

Protocol v4 has no cryptographic pairing. The interface/subnet checks narrow
the intended trust boundary, but any sender able to reach the host from the
accepted tether subnet can impersonate the phone and inject input. CRC protects
integrity against corruption, not identity. Authenticated pairing remains a
protocol-v5 task.

## Optional local-only tethering

On Windows, the Tauri launcher exposes **Stop the PC from using the phone's
internet** as an opt-in setting. When enabled, the native host recognizes the Android/RNDIS
adapter only after a valid discovery hello arrives from an unambiguous private
or local tether prefix and Windows confirms that the peer is routed through
that same interface. It does not mutate every adapter whose display name looks
like RNDIS/NCM while waiting. The host then records the confirmed interface's
current default routes, disables future default-route installation on that
adapter, and removes only the routes recorded in that journal. The connected
phone subnet remains available for the Holodori UDP link, while the PC's other
applications continue using their normal interfaces.

Linux deliberately performs no route mutation. The launcher disables this
option and the backend rejects it; users who need the same policy configure
both `ipv4.never-default` and `ipv6.never-default` on the NetworkManager tether
profile.

The privileged mutation consumes the immutable adapter identity accepted by
discovery instead of performing an unrelated second selection. Every queried
or submitted interface/route row must carry that adapter's exact Windows LUID;
an adapter replacement aborts cleanup or protection without touching the
replacement, even if Windows reuses its interface index.

Before its first route mutation, the policy persists and flushes a versioned
snapshot in the 64-bit machine registry at
`HKLM\SOFTWARE\Holodori\PhoneTrackpad\RoutePolicy`. The key inherits the
machine-wide administrative write boundary, so an unelevated process cannot
forge state that a later elevated recovery would apply. It is also visible to
an administrator account used for over-the-shoulder UAC. Each entry records
the owner PID/creation time, adapter GUID/LUID identity, original per-family
flags, and reconstructable routes. A normal **Stop**, launcher close, or
console shutdown restores the owned state.
After a crash, launcher startup (and every new Start) restores only a still
enforced flag and missing original routes on the same adapter instance. Normal
rollback follows the same rule: it never deletes newer routes, and it restores
missing originals only when the current route set is a subset of the captured
state. The captured route set is compared again immediately before deletion;
if it changed, no replacement route is deleted. If an unjournaled default route
appears while protection is being re-asserted, the host fails closed and
performs cleanup instead of deleting that route. Failed rollback or recovery,
including an unplugged captured adapter, keeps the snapshot for retry rather
than forgetting potentially unrestored Windows state.

Windows protects route-table and interface-policy changes, so the launcher
offers **Restart as admin** whenever recovery or the option needs elevation,
even if the checkbox is currently clear. With the checkbox cleared and no
orphaned snapshot, the host does not modify Windows routing.

Every HPT4 frame is one datagram and is smaller than the Ethernet MTU. HPA4
HELLO and ACK records are also one datagram each. A malformed datagram cannot
be concatenated with a later frame; the host resets the datagram parser at
each boundary.

## Why this does not lose a slide to packet loss or jitter

- Android copies every `MotionEvent` history sample before the current sample.
- Each sample contains the complete simultaneous contact set.
- The queue has no age-based or capacity-based gameplay eviction.
- Every frame has a 64-bit session ID, 64-bit sequence, length, and CRC.
- The host buffers a future sequence until the missing sequence arrives.
- Duplicate replays are acknowledged without being applied twice.
- Every frame, discovery record, and acknowledgement has an immediate
  redundant copy; a second replay round starts after 2 ms.
- Android retires only the highest contiguous sequence acknowledged by the host.
- The lane sink interpolates every lane between old and new positions, even if
  a phone or OS reports a large coordinate jump.
- A lane transition presses the new lane before releasing the old one.
- Stationary touch contacts receive an 8 ms keepalive containing the latest
  complete contact snapshot, so a restarted host can reconstruct a hold.

A physical cable removal is a visible session boundary, not packet jitter.
The host releases active injected input after 32 ms without an ordered frame
being accepted by the OS sink and committed. Valid duplicates or future frames
behind an ordering hole do not mask that stall. The host then refuses delayed
gameplay until a fresh session-start `CANCEL`.
Protocol v4 does not replay old gameplay after a multi-second reconnect
because doing so would create late notes. During gameplay Android starts a
fresh session after 64 ms without cumulative ACK advancement, drops queued
gameplay, and sends a new session-start `CANCEL`; duplicate or invalid ACKs do
not count as progress. The first gameplay frame receives a fresh response
window so an older idle-discovery timestamp cannot cause an immediate false
timeout. Idle discovery retains the two-second timeout. Socket restart begins
with a 4 ms backoff. The latest complete contact snapshot survives that restart,
so a still-held finger is reconstructed after the fresh `CANCEL` without
replaying stale actions.

## Windows Touch proof

`holodori-native-host.exe --mode touch` starts two independent processes:

- the host consumes UDP frames and calls `InitializeTouchInjection` /
  `InjectTouchInput` with complete `POINTER_TOUCH_INFO` frames;
- `holodori-touch-probe.exe` knows nothing about UDP and handles only
  `WM_POINTERDOWN`, `WM_POINTERUPDATE`, and `WM_POINTERUP`.

If the probe displays the contact, Windows' pointer stack received it. This is
a sanctioned desktop Windows Touch injection path and is not a physical-origin
digitizer claim; Windows can still identify injected input. It does not read or
modify Holodori memory, protocol, or packets.

For a local Windows API smoke test without a phone:

```text
target\release\holodori-touch-probe.exe
target\release\holodori-touch-smoke.exe
```

The smoke process sends a DOWN, 48 UPDATE frames, and an UP across the probe.
The probe must count them as Windows pointer messages.

## Holodori keyboard mode

`--mode keys` converts the same ordered touch frames into Windows keyboard scan
codes or Linux `uinput` key events. Multiple fingers use per-lane reference
counts, so one finger
cannot release a key still held by another. A CANCEL, session start, host exit,
or sink drop releases all held keys. A physical tip remains owned when it
leaves the painted phone rectangle; normalized X is clamped to the nearest
edge lane until lift. Each protocol frame's ordered press-before-release
changes are submitted as one platform batch, with partial-prefix acceptance
retrying only the unaccepted suffix. Linux counts a transition as accepted only
after both its `EV_KEY` and following `SYN_REPORT` have reached `uinput`.

The native host reports stable Waiting, Connected, Recovering, and Stopping
transitions from a background reporter. The launcher drains that stream and
shows the matching phase without doing UI, logging, or formatting on the
gameplay path. An idle peer disappears from Connected after two seconds
without a valid pinned-peer hello or frame; active input still uses the
32 ms committed-progress watchdog.

This mode sends ordinary OS input only. It never opens the game process,
patches memory, hooks rendering/input code, or constructs game/network packets.

## Latency contract

The native hot path has no Python interpreter, JSON, polling bridge, or UI
work. It uses fixed binary datagrams, stack buffers, direct native OS input calls,
immediate redundant sends, and a 2 ms replay threshold. It accepts and sustains
at least 120 updates per second; the keepalive period is 8 ms.

An absolute promise that every phone is faster than every physical keyboard is
not physically testable or universally true: phone touch scan rate and USB
controller scheduling vary. The enforceable target for this branch is:

- no intentional buffering or frame-age reset;
- no loss caused by transport jitter, CRC failure, duplicate delivery, or a
  lost ACK;
- host processing and sink submission remain below one 120 Hz frame under
  normal operation;
- performance must be measured on each target phone/PC pair before release.

Protocol v4 preserves the original event, UI callback, and network-write
times. The phone also echoes the host's previous control-send time and its
local receipt time. This four-timestamp exchange subtracts measured phone
turnaround from a duplex round trip, avoiding direct subtraction of unrelated
clock origins.

### Instrumented metrics

Pass `--metrics` to collect bounded in-memory samples. No metrics are formatted,
sorted, printed, or written while input is active. Press Q then Enter, Ctrl+C,
or close the console to request graceful shutdown; the host then writes one
timestamped file under `Windows\Logs` on Windows or
`$XDG_STATE_HOME/holodori/logs` (falling back to
`~/.local/state/holodori/logs`) on Linux. `--metrics-file PATH` selects an
explicit destination and `--warn-ms MS` changes the default 8.333 ms final
warning budget.

The report contains mean, max, p50, p90, p99, and p99.9 values for current-event
to-host-input estimated latency, Android current input dispatch, Android historical
batch age, Android callback-to-write, symmetric one-way network, host
receive-to-sink, and ACK write. Recovery incidents, out-of-order frames,
replays, unresolved frames, parser discards, and sink retries are counted once
at exit. No cross-device clocks are directly subtracted.

## Build and operation

Requirements:

- Windows 10/11 or x86_64 Linux;
- Rust stable (MSVC on Windows, GNU on the current Linux bundle);
- JDK 17 and Android SDK Platform 35 for the APK;
- a phone and PC that support ordinary USB tethering/RNDIS.

Build:

```text
cd native-host
cargo test --all-targets
cargo build --release

cd ..\android-app
gradlew.bat assembleRelease
```

On the phone, enable USB tethering before launching the host. The app uses
normal `INTERNET` and `ACCESS_NETWORK_STATE` permissions only.
