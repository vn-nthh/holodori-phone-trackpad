# Holodori lossless touch protocol v4

> **Implementation status:** the current release implements this USB-only
> protocol. [Protocol v5](PROTOCOL_V5.md) supersedes it as the default current
> source implementation. This document remains the exact reference for the
> published v0.4.1 build and explicit legacy migration tests.

Protocol v4 is the duplex binary protocol used by the native Windows and Linux
hosts over Android USB tethering/RNDIS and UDP. Android retains each input
frame until the host confirms that the selected operating-system sink accepted
it.
Version 4 adds stage-separated benchmark timestamps and a duplex clock
exchange; it is not wire-compatible with earlier versions.

## USB-tethered UDP transport

The app sends a 32-byte `HPTD` discovery hello to UDP port `42825` only on
conservatively classified USB-tether interfaces. The host listens on
`0.0.0.0:42825` so an adapter can appear after startup, but it accepts a hello
only when the source belongs to one unambiguous recognized USB-tether prefix.
Both hosts require `IP_PKTINFO` receive metadata to report that the datagram
actually arrived on the accepted interface. Linux additionally requires the
kernel's `rndis_host` driver; generic CDC Ethernet/NCM adapters and spoofed
packets arriving on another interface fail closed while protocol v4 has no
authenticated pairing. The host replies to that source and receives HPT4
datagrams from the phone's ephemeral source port. The discovery record is
little-endian and uses this layout:

| Offset | Type | Meaning |
|---:|---|---|
| 0 | `u8[4]` | `HPTD` |
| 4 | `u8` | Discovery version, `1` |
| 5 | `u8` | Hello `1`, acknowledgement `2` |
| 6 | `u16` | Reserved, zero |
| 8 | `u64` | Discovery nonce |
| 16 | `u64` | Touch session ID |
| 24 | `u16` | Discovery port, `42825` |
| 26 | `u16` | Reserved, zero |
| 28 | `u32` | IEEE CRC-32 of bytes `0..27` |

New senders put the actual listening destination port in offset 24. A receiver
accepts zero only for compatibility with earlier protocol-v4 builds; every
other value must equal `42825` (or the host's explicitly configured test port).
The phone accepts an acknowledgement only from the selected tether subnet,
then pins the first complete host IP/port for that session.
The host accepts a hello only when its source belongs to an unambiguous
private/local prefix on a conservatively recognized phone-tether adapter. If
local-only routing is requested, Windows must also route that peer through the
same interface before any route setting is changed.

On Linux, local-only mode is launcher configuration rather than a protocol or
native-host route mutation. Before starting the host with that option selected,
the launcher requires one active `rndis_host` device and verifies that its exact
NetworkManager profile UUID has both IPv4 and IPv6 `never-default` enabled in
the persistent profile and the currently applied connection. Persistent writes
and rollbacks use NetworkManager's profile version guard. It also requires
that trusted iproute2 find no IPv4 or IPv6 default route for that interface in
any routing table. The native host repeats the read-only route check before it
sends the discovery ACK, so no Linux local-only gameplay session starts on an
unsafe tether.

The host pins the phone IP, source port, discovery nonce, session, and exact
tether adapter identity. A same-IP hello from a new source port migrates in
place only after the host reconfirms the peer's unambiguous prefix, route, and
original adapter identity. A failed revalidation aborts the connection and
returns to discovery through a clean input boundary. If the session is
unchanged, asserted input remains owned; if its session changes, the host first
releases input and requires a sequence-zero session-start `CANCEL`. Hellos from
a different IP and HPT4 frames with a different session are ignored without an
acknowledgement.

This is network confinement, not cryptographic authentication. CRC-32 detects
corruption but does not prove identity. Any sender able to reach the host from
the accepted USB-tether subnet can impersonate the phone and inject input.
Pairing and authenticated framing are defined by
[protocol v5](PROTOCOL_V5.md) and are intentionally outside this version.

Each HPT4 frame and HPA4 control record is one UDP datagram. A datagram is
never concatenated with the next datagram for parsing, and the largest HPT4
frame is 232 bytes, below the USB-tethered Ethernet MTU.

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
| 40 | `u64` | Time the writer began this network send attempt |
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
X/Y coordinates, normalized pressure, and normalized touch-major size. Every
record is a complete simultaneous contact snapshot.

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
- host input service: sink acceptance minus host receipt;
- ACK write: completion minus start of the host bulk write.

The USB-tethered network estimate uses four timestamps from a duplex exchange:

```text
host control send H1 -> phone control receive P2
phone frame send P3  -> host frame receive H4
```

The phone turnaround `P3 - P2` is subtracted from `H4 - H1`. Half of the
remainder is reported as estimated one-way network time. This cancels the unrelated
clock origins and makes no clock-rate regression. It assumes both USB
directions have equal delay, so the report labels the result as an estimate.

Current samples and historical Android batch samples have separate statistics.
Historical sample age therefore cannot inflate the live current-event result.
The estimated current-event end-to-end value is the sum of Android current
dispatch, callback-to-write, estimated USB-tethered network one-way, and host
input service for the same accepted frame.

The host aggregates the complete session into fixed 4 microsecond histograms.
It retains exact counts, mean, and maximum while avoiding per-frame allocation,
sorting, or file output. One worst current event keeps the correlated stage
breakdown needed to locate a rare stall.

## Reliability

- Android never coalesces or individually evicts ordered gameplay frames. A
  64 ms oldest-frame boundary abandons the whole session instead of replaying
  stale transitions.
- Frames carry session, sequence, length, and CRC protection.
- The host buffers future sequences and applies every sequence exactly once.
- Android sends every frame twice immediately and begins replay after 2 ms if
  neither copy produces a cumulative acknowledgement. The host likewise sends
  every discovery and control record twice. One lost or corrupt datagram is
  therefore covered without waiting for the replay timer.
- The host acknowledges only after the selected OS sink accepts a frame.
- A fresh host can bootstrap from the oldest replay in an active phone session.
- During gameplay, either 64 ms without cumulative ACK advancement or a 64 ms
  oldest pending frame makes Android drop queued gameplay and start a fresh
  session with a `CANCEL`. Duplicate, stale, or out-of-range controls do not
  count as ordered progress. The first active
  frame starts a fresh response window rather than inheriting an older idle
  discovery timestamp. An idle search may wait two seconds. The initial
  socket-restart backoff is 4 ms.
- An 8 ms acknowledged heartbeat carries the latest complete contact snapshot,
  sustains active stationary contacts, and lets a restarted host reconstruct a
  held contact. Hosts still accept the empty heartbeat emitted by earlier v4
  APKs. Idle sessions send no synthetic touch frames; discovery
  acknowledgements keep the host liveness check alive. With no active input,
  two seconds without a valid pinned-peer hello or CRC-valid frame returns the
  host to discovery so the launcher cannot remain falsely connected.
- A cable removal is a session boundary; old gameplay is not replayed late.
- If no ordered frame reaches the host OS sink and commits for 32 ms while
  input is active, the host releases injected input, rejects delayed gameplay
  from that session, and waits for a fresh session-start `CANCEL`. Merely
  decoding duplicates or future frames behind a gap does not refresh liveness.
- Any host read or acknowledgement-write failure also releases injected input
  before discovery resumes. Android retains the latest contact snapshot across
  its socket restart, including updates received while offline, so a stationary
  hold is reconstructed in the fresh session without replaying stale actions.

UDP datagrams are atomic at the application boundary. If one datagram is lost
or corrupt, its immediate redundant copy carries the same sequence. If both
copies disappear, the 2 ms replay sends the sequence again; a corrupt copy is
rejected and remains unacknowledged.
