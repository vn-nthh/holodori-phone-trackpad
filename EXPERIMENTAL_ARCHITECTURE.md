# Experimental lossless touch architecture

This branch is a clean experiment, not a wire-compatible update to the current
Python release. It has two outputs from one lossless phone stream:

1. a keyboard-lane sink for the heuristics Holodori supports today;
2. a Windows Touch sink plus an independent receiver that demonstrates
   real-time `WM_POINTER` delivery outside Holodori.

No external controller, USB adapter, microcontroller, or second cable is
required. The PC is the USB host and the Android phone enters Android Open
Accessory (AOA) mode over a normal data cable.

## Data path

```text
Android MotionEvent
  -> complete chronological contact frame
  -> retained v4 queue with benchmark timestamps
  -> AOA bulk OUT over USB
  -> Rust parser + CRC + sequence reorder/dedup
  -> selected Windows OS sink
  -> cumulative ACK over AOA bulk IN
  -> Android retires acknowledged frames
```

The ACK is deliberately after the sink. A valid packet that Windows has not
accepted remains unacknowledged and is retried; parsing alone is not success.

## Why this does not lose a slide to packet loss or jitter

- Android copies every `MotionEvent` history sample before the current sample.
- Each sample contains the complete simultaneous contact set.
- The queue has no age-based or capacity-based gameplay eviction.
- Every frame has a 64-bit session ID, 64-bit sequence, length, and CRC.
- Windows buffers a future sequence until the missing sequence arrives.
- Duplicate replays are acknowledged without being applied twice.
- Android retires only the highest contiguous sequence acknowledged by Windows.
- The lane sink interpolates every lane between old and new positions, even if
  a phone or OS reports a large coordinate jump.
- A lane transition presses the new lane before releasing the old one.
- Stationary touch contacts receive an 8 ms keepalive.

A physical cable removal is a visible session boundary, not packet jitter.
Protocol v4 does not replay old gameplay after a reconnect because doing so
would create late notes. Persisting disconnected input and playing it later is
lossless archival, but it is not a valid live controller.

## Windows Touch proof

`holodori-native-host.exe --mode touch` starts two independent processes:

- the host consumes USB frames and calls `InitializeTouchInjection` /
  `InjectTouchInput` with complete `POINTER_TOUCH_INFO` frames;
- `holodori-touch-probe.exe` knows nothing about USB and handles only
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

The native hot path has no Python interpreter, JSON, sockets, polling bridge,
or UI work. It uses synchronous USB bulk reads, fixed binary frames, stack
buffers, and direct User32 calls. It accepts and sustains at least 120 updates
per second; the keepalive period is 8 ms.

An absolute promise that every phone is faster than every physical keyboard is
not physically testable or universally true: phone touch scan rate and USB
controller scheduling vary, while gaming keyboards range from sub-millisecond
to multi-frame latency. The enforceable target for this branch is:

- no intentional buffering or frame-age reset;
- no loss caused by transport jitter, CRC failure, duplicate delivery, or a
  lost ACK;
- host processing and sink submission remain below one 120 Hz frame under
  normal operation;
- performance must be measured on each target phone/PC pair before release.

Protocol v4 preserves the original event, UI callback, and USB-write times. The
phone also echoes the host's previous control-send time and its local receipt
time. This four-timestamp exchange subtracts measured phone turnaround from a
duplex round trip, avoiding direct subtraction of unrelated clock origins.

### Instrumented metrics

Pass `--metrics` to collect bounded in-memory samples. No metrics are formatted,
sorted, printed, or written while input is active. Press Q then Enter, Ctrl+C,
or close the console to request graceful shutdown; the host then writes one timestamped file
under `Windows\Logs`. `--metrics-file PATH` selects an explicit destination and
`--warn-ms MS` changes the default 8.333 ms final warning budget.

The report contains one set of mean, max, p50, p90, p99, and p99.9 values for
current-event-to-Windows estimated latency, Android current input dispatch,
Android historical batch age, Android callback-to-write, symmetric one-way USB,
Windows receive-to-sink, and ACK write. Current and historical MotionEvent
samples are separate populations. Recovery incidents, out-of-order frames,
replays, unresolved frames, parser discards, and sink retries are counted once
at exit. No cross-device clocks are directly subtracted.

Percentiles use fixed 4 microsecond histograms that cover the complete session;
long runs are not truncated to a recent sample window. The report also gives
tail counts and one correlated worst-current-event record with every stage of
that same frame. Parser bytes skipped while attaching to an already-active USB
stream are kept separate from bytes discarded after valid framing begins.

## Build and operation

Requirements:

- Windows 10 or 11;
- Rust stable MSVC toolchain;
- JDK 17 and Android SDK Platform 35 for the APK;
- UsbDk installed, or a compatible WinUSB binding for the AOA interface.

Build:

```text
cd native-host
cargo test --all-targets
cargo build --release

cd ..\android-app
gradlew.bat assembleDebug
```

The native host requests AOA identity `Holodori / Phone Trackpad / 4.0`; the
experimental APK filter matches that exact identity.
