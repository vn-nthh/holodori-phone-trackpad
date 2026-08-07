# Holodori lossless touch protocol v3

Protocol v3 is the experimental, duplex AOA protocol used by the native
Windows host. Its two invariants are:

1. Android never removes a touch frame because it became old.
2. Android removes a frame only after the host cumulatively acknowledges it.

USB already preserves byte order. Sequence numbers, CRCs, acknowledgements,
and replay extend that guarantee across application queues and delayed host
reads.

## Phone-to-host frame

All integers are little-endian. Frames are variable length.

| Offset | Type | Meaning |
|---:|---|---|
| 0 | `u8[4]` | `HPT3` |
| 4 | `u8` | Protocol version, `3` |
| 5 | `u8` | Message type, `1` for a touch frame |
| 6 | `u16` | Complete frame length including CRC |
| 8 | `u64` | Transport session ID |
| 16 | `u64` | Session-local frame sequence |
| 24 | `u64` | Android monotonic event time in nanoseconds |
| 32 | `u8` | Action: heartbeat `0`, down `1`, move `2`, up `3`, cancel `4` |
| 33 | `u8` | Pointer ID associated with down/up |
| 34 | `u8` | Number of contact records |
| 35 | `u8` | Frame flags: locked `0x01`, session start `0x02` |
| 36 | contact records | Ten bytes per contact |
| final 4 | `u32` | IEEE CRC-32 of every preceding byte |

Each contact record is:

| Offset | Type | Meaning |
|---:|---|---|
| 0 | `u8` | Stable Android pointer ID |
| 1 | `u8` | Inside play zone `0x01`, touching `0x02` |
| 2 | `i16` | Zone-local X multiplied by 10,000 |
| 4 | `i16` | Zone-local Y multiplied by 10,000 |
| 6 | `u16` | Normalized pressure |
| 8 | `u16` | Normalized touch-major size |

A MOVE with Android history becomes multiple protocol frames. Historical time
is the outer loop and contacts are the inner loop, so each protocol record is a
complete, chronological multi-contact snapshot.

Coordinates are not clamped. Keyboard mode can retain the extended outer-lane
hitboxes. Windows Touch mode clamps only when mapping to the target desktop
rectangle because `InjectTouchInput` requires desktop coordinates.

## Host-to-phone control record

Control records are fixed at 32 bytes.

| Offset | Type | Meaning |
|---:|---|---|
| 0 | `u8[4]` | `HPA3` |
| 4 | `u8` | Protocol version, `3` |
| 5 | `u8` | HELLO `1`, cumulative ACK `2` |
| 6 | `u16` | Low byte is requested lane count |
| 8 | `u64` | Session ID |
| 16 | `u64` | Highest contiguous frame accepted by the host |
| 24 | `u32` | Host receive window |
| 28 | `u32` | IEEE CRC-32 of bytes `0..27` |

The host sends HELLO after receiving the session-start frame. An ACK means the
frame passed parsing and CRC validation and was accepted by the selected sink.
For Windows Touch, acceptance means `InjectTouchInput` succeeded. Duplicate
frames are not reinjected; they cause the current cumulative ACK to be sent
again.

## Reliability and timing semantics

- Android uses an ordered, non-dropping queue. Queue age is diagnostic only.
- The sender keeps at most the host-advertised window in flight.
- An unacknowledged frame is replayed after 4 ms.
- When no motion frame is pending, Android emits an acknowledged keepalive
  every 8 ms. Touch mode converts it into a Windows contact UPDATE; keyboard
  mode treats it as a no-op.
- The receiver accepts frames exactly once per session and buffers unexpected
  future sequences until a gap is filled.
- A fresh host process may bootstrap from the oldest replayed frame of an
  already-running Android session. This permits host restart recovery without
  requiring a cable replug.
- Session start and cancel are ordinary acknowledged frames.
- A physical disconnect creates a session boundary. Protocol v3 proves
  lossless delivery during a connected session; cross-disconnect persistence
  is deliberately a separate archival concern because replaying old gameplay
  input after reconnect would violate the latency contract.

## Windows Touch demonstration

The native host maps every accepted frame to a complete
`POINTER_TOUCH_INFO[]` frame and calls `InjectTouchInput`. A separate native
probe process receives `WM_POINTERDOWN`, `WM_POINTERUPDATE`, and
`WM_POINTERUP`. The probe is intentionally independent of Holodori and exists
to demonstrate that Windows receives real multi-contact Windows Touch events
from the live phone stream.
