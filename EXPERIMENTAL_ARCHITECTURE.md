# Experimental lossless touch architecture

This branch is a USB-tethering/RNDIS + UDP experiment. It is deliberately
not wire-compatible with the stable Python release and does not use ADB,
root, Android USB accessory mode, WinUSB, UsbDk, or a custom Windows driver.
The only physical link is the phone's normal USB cable; Windows' inbox RNDIS
network driver carries the UDP traffic.

## Data path

```text
Android MotionEvent
  -> complete chronological contact frame
  -> retained v4 queue with benchmark timestamps
  -> HPT4 one-datagram UDP send over USB tethering/RNDIS
  -> Rust UDP receiver + CRC + sequence reorder/dedup
  -> selected Windows OS sink
  -> HPA4 cumulative ACK datagram
  -> Android retires acknowledged frames
```

The ACK is deliberately after the sink. A valid packet that Windows has not
accepted remains unacknowledged and is retried; parsing alone is not success.
UDP is used as a low-overhead datagram framing layer, not as an invitation to
drop input: the retained queue and cumulative ACK make the live path
loss-aware.

## Discovery and packet boundaries

The Android app binds one ephemeral UDP socket and broadcasts a 32-byte,
CRC-protected `HPTD` discovery hello to port 42825 on the RNDIS interface.
The host listens on `0.0.0.0:42825`, replies to the phone's source address,
and the phone then sends HPT4 frames to that host address. No fixed phone IP
is assumed. Discovery repeats while the link is not acknowledged, allowing a
host restart without toggling USB tethering.

Every HPT4 frame is one datagram and is smaller than the Ethernet MTU. HPA4
HELLO and ACK records are also one datagram each. A malformed datagram cannot
be concatenated with a later frame; the host resets the datagram parser at
each boundary.

## Why this does not lose a slide to packet loss or jitter

- Android copies every `MotionEvent` history sample before the current sample.
- Each sample contains the complete simultaneous contact set.
- The queue has no age-based or capacity-based gameplay eviction.
- Every frame has a 64-bit session ID, 64-bit sequence, length, and CRC.
- Windows buffers a future sequence until the missing sequence arrives.
- Duplicate replays are acknowledged without being applied twice.
- Every frame, discovery record, and acknowledgement has an immediate
  redundant copy; a second replay round starts after 2 ms.
- Android retires only the highest contiguous sequence acknowledged by Windows.
- The lane sink interpolates every lane between old and new positions, even if
  a phone or OS reports a large coordinate jump.
- A lane transition presses the new lane before releasing the old one.
- Stationary touch contacts receive an 8 ms keepalive containing the latest
  complete contact snapshot, so a restarted host can reconstruct a hold.

A physical cable removal is a visible session boundary, not packet jitter.
Windows releases active injected input after 32 ms without an ordered frame
being accepted by the OS sink and committed. Valid duplicates or future frames
behind an ordering hole do not mask that stall. Windows then refuses delayed
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
code input. Multiple fingers use per-lane reference counts, so one finger
cannot release a key still held by another. A CANCEL, session start, host exit,
or sink drop releases all held keys.

This mode sends ordinary OS input only. It never opens the game process,
patches memory, hooks rendering/input code, or constructs game/network packets.

## Latency contract

The native hot path has no Python interpreter, JSON, polling bridge, or UI
work. It uses fixed binary datagrams, stack buffers, direct User32 calls,
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
timestamped file under `Windows\Logs`. `--metrics-file PATH` selects an explicit
destination and `--warn-ms MS` changes the default 8.333 ms final warning budget.

The report contains mean, max, p50, p90, p99, and p99.9 values for current-event
to-Windows estimated latency, Android current input dispatch, Android historical
batch age, Android callback-to-write, symmetric one-way network, Windows
receive-to-sink, and ACK write. Recovery incidents, out-of-order frames,
replays, unresolved frames, parser discards, and sink retries are counted once
at exit. No cross-device clocks are directly subtracted.

## Build and operation

Requirements:

- Windows 10 or 11;
- Rust stable MSVC toolchain;
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
