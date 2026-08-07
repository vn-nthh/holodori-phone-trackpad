# Holodori lossless touch protocol v4

Protocol v4 is the duplex Android Open Accessory protocol used by the native
Windows experiment. Android retains each input frame until the host confirms
that the selected Windows sink accepted it. Version 4 adds stage-separated
benchmark timestamps and a duplex clock exchange; it is not wire-compatible
with protocol v3 or the stable Python host.

## Phone-to-host frame

All integers are little-endian. Frames are variable length.

| Offset | Type | Meaning |
|---:|---|---|
| 0 | `u8[4]` | `HPT4` |
| 4 | `u8` | Protocol version, `4` |
| 5 | `u8` | Message type, `1` for a touch frame |
| 6 | `u16` | Complete frame length including CRC |
| 8 | `u64` | Transport session ID |
| 16 | `u64` | Session-local frame sequence |
| 24 | `u64` | Original Android `MotionEvent` sample time |
| 32 | `u64` | Time the Android UI touch callback began |
| 40 | `u64` | Time the writer began this USB send attempt |
| 48 | `u64` | Echoed host-send timestamp from the latest control record |
| 56 | `u64` | Phone time when that control record was received |
| 64 | `u8` | Heartbeat `0`, down `1`, move `2`, up `3`, cancel `4` |
| 65 | `u8` | Pointer ID associated with down/up |
| 66 | `u8` | Number of contact records |
| 67 | `u8` | Locked `0x01`, session start `0x02`, historical `0x04` |
| 68 | contact records | Ten bytes per contact |
| final 4 | `u32` | IEEE CRC-32 of every preceding byte |

The writer patches offsets 40, 48, and 56 and computes the CRC immediately
before every send attempt. A retransmission therefore describes the copy that
actually reached the host rather than retaining the first attempt's time.

Each contact record contains a pointer ID, inside/tip flags, signed normalized
X/Y coordinates, normalized pressure, and normalized touch-major size. As in
v3, every record is a complete simultaneous contact snapshot.

## Host-to-phone control record

Control records are fixed at 40 bytes.

| Offset | Type | Meaning |
|---:|---|---|
| 0 | `u8[4]` | `HPA4` |
| 4 | `u8` | Protocol version, `4` |
| 5 | `u8` | HELLO `1`, cumulative ACK `2` |
| 6 | `u16` | Low byte is requested lane count |
| 8 | `u64` | Session ID |
| 16 | `u64` | Highest contiguous frame accepted by the host |
| 24 | `u32` | Host receive window |
| 28 | `u64` | Host monotonic time immediately before this send |
| 36 | `u32` | IEEE CRC-32 of bytes `0..35` |

## Benchmark timing

The same-clock stages are exact monotonic differences:

- Android input dispatch: callback minus original event time;
- Android app/queue: USB-write time minus callback time;
- Windows service: sink acceptance minus host receipt;
- ACK write: completion minus start of the host bulk write.

The USB estimate uses four timestamps from a duplex exchange:

```text
host control send H1 -> phone control receive P2
phone frame send P3  -> host frame receive H4
```

The phone turnaround `P3 - P2` is subtracted from `H4 - H1`. Half of the
remainder is reported as estimated one-way USB time. This cancels the unrelated
clock origins and makes no clock-rate regression. It assumes both USB
directions have equal delay, so the report labels the result as an estimate.

Current samples and historical Android batch samples have separate statistics.
Historical sample age therefore cannot inflate the live current-event result.
The estimated current-event end-to-end value is the sum of Android current
dispatch, callback-to-write, estimated USB one-way, and Windows service for the
same accepted frame.

The host aggregates the complete session into fixed 4 microsecond histograms.
It retains exact counts, mean, and maximum while avoiding per-frame allocation,
sorting, or file output. One worst current event keeps the correlated stage
breakdown needed to locate a rare stall.

## Reliability

- Android has an ordered queue with no age or capacity eviction.
- Frames carry session, sequence, length, and CRC protection.
- The host buffers future sequences and applies every sequence exactly once.
- Android retransmits an unacknowledged frame after 4 ms.
- The host acknowledges only after the selected Windows sink accepts a frame.
- A fresh host can bootstrap from the oldest replay in an active phone session.
- An 8 ms acknowledged heartbeat sustains stationary contacts.
- A cable removal is a session boundary; old gameplay is not replayed late.

On a libusb timeout the host preserves any bytes reported as transferred. This
is required by libusb's synchronous bulk-transfer contract and prevents a
partial timeout from manufacturing a sequence hole.
