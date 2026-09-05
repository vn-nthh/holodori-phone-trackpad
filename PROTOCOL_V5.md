# Holodori lossless touch protocol v5

Protocol v5 is the authenticated transport implemented by current source builds
of Doritrack. It adds
explicit USB or Wi-Fi/local-network selection, first-use pairing through the
six phone lanes, authenticated encryption, transport-independent remembered
peers, Wi-Fi path measurement, and a thumb-friendly phone layout without
weakening the existing lossless input rules.

This specification is normative for current protocol work. The published
v0.4.1 release still implements [protocol v4](PROTOCOL_V4.md). Version 5 is not
wire-compatible with version 4. A v5 implementation MUST NOT silently
downgrade, MUST NOT accept v4 gameplay on a Wi-Fi listener, and MUST treat a
transport or authenticated session change as a clean input boundary.

A migration build MAY retain v4 only as a separate, explicitly selected,
USB-only **Legacy (unpaired)** mode. It does not negotiate v4 inside a v5
exchange and never exposes legacy framing on the local-network listener.

The key words MUST, MUST NOT, SHOULD, SHOULD NOT, and MAY are normative.

## Goals and non-goals

Protocol v5 MUST:

- preserve every chronological Android history sample and every complete
  simultaneous contact snapshot;
- acknowledge a gameplay sequence only after the selected operating-system
  input sink accepts it;
- keep the immediate send, immediate redundant copy, and 2 ms repair behavior;
- authenticate and encrypt every gameplay and post-handshake control record;
- pair without asking the user to type a conventional password;
- remember an authenticated phone identity so ordinary reconnects need no new
  lane pattern;
- let the user choose USB or Wi-Fi/local network before pairing or starting;
- measure the selected Wi-Fi path without making radio band or RSSI an
  authentication decision; and
- keep every datagram comfortably below the path MTU.

It does not create a hotspot, traverse NAT, use a cloud relay, scan nearby
networks, or make any claim that injected Windows Touch is physical hardware.
The initial Wi-Fi scope is one existing local network and one paired phone.

## Fixed cryptographic suite

Version 5 has one suite rather than negotiation on the unauthenticated path:

```text
Noise_XX_25519_ChaChaPoly_BLAKE2s   initial pairing
Noise_IK_25519_ChaChaPoly_BLAKE2s   remembered-peer sessions
```

The implementation MUST use a maintained Noise implementation and MUST NOT
substitute a home-grown Diffie-Hellman, cipher, or handshake. The Noise
prologue is:

```text
ASCII("holodori-phone-trackpad-v5") || 0x00 || transport || exchange_id
```

`transport` is `1` for USB tethering and `2` for Wi-Fi/local network.
`exchange_id` is the 16-byte value from the discovery envelope. Binding both
values into the transcript prevents a handshake from being moved to another
transport or pairing attempt.

Each installation owns one X25519 static identity key. Pairing records the
other side's public identity. Android MUST protect its private material with
Android Keystore-backed storage where available. Windows MUST protect it for
the current user with DPAPI. Linux MUST use the desktop Secret Service/keyring
when available and fail closed rather than write a plaintext private key.

For initial pairing, the host is the Noise XX initiator. For a remembered-peer
session, the phone is the Noise IK initiator and already knows the host static
public key. After decrypting IK message 1, the host MUST match its encrypted
initiator static key to the paired phone record. It MUST NOT expose a stable
device identifier in a discovery broadcast.

The Noise split cipher states become the two directional application keys.
They are never reused by another handshake. The initiator-to-responder split
key protects that direction and the responder-to-initiator key protects the
other direction.

## Transport choice and interface confinement

The host UI MUST ask for **USB** or **Wi-Fi / local network** before **Pair** or
**Start** becomes active. The phone MUST make the same choice. The selection is
not automatic and MUST NOT change while input is active.

USB continues to use Android USB tethering and the conservatively identified
USB-network interface. Wi-Fi/local network means that the phone uses its
current physical Wi-Fi network; the PC may use Wi-Fi or wired Ethernet. A wired
PC on the same LAN is preferred over adding another wireless hop.

The first v5 implementation is limited to peers on the same local subnet. The
host MUST bind to the selected private interface or selected local address,
not blindly expose the listener on every interface. It MUST also inspect
receive-interface metadata as defense in depth. Android MUST select a `Network`
whose `NetworkCapabilities` contains `TRANSPORT_WIFI` and bind its UDP socket
with `Network.bindSocket`; it MUST NOT fall through to cellular or a VPN.

Pairing discovery is active only for the selected interface and the 60-second
host pairing window. Remembered-peer discovery is active only after the user
starts the selected transport. An address or interface change during gameplay
ends the authenticated session, releases injected input, and requires a fresh
IK handshake and session-start `CANCEL` before input resumes.

Initial discovery sends `HPP5` probes to the selected interface's IPv4 directed
broadcast address on UDP port `42825`; the host replies by unicast to the source
address and port. The initial implementation does not require IPv6 discovery.
Guest-network or client-isolation settings may block discovery even when both
devices show the same Wi-Fi name, and the UI SHOULD explain that case rather
than suggesting a weaker listener.

Wi-Fi is NOT restricted to 5 GHz. The app accepts 2.4, 5, and 6 GHz, including
multi-link networks. It SHOULD recommend 5 or 6 GHz when available, but the
measured path decides whether a warning is useful. A healthy 2.4 GHz path is
valid; a congested 5 GHz path is not declared healthy merely because of its
band. Authenticated encryption protects confidentiality and integrity even on
an open LAN, but it does not prevent jamming, flooding, or other denial of
service.

## Pairing ceremony

Pairing never runs while a controller session is active. Pressing **Pair** MUST
first stop the controller, release all injected input, and open one 60-second
window on the selected interface.

The phone sends a fresh pairing probe. The host locks the window to the first
well-formed probe and runs one Noise XX handshake. Retransmission of the same
handshake step is still the same attempt; another exchange ID or another phone
is ignored. A timeout, malformed authenticated step, wrong lane pattern, or
explicit cancellation destroys all provisional keys and closes the window.
The user must press **Pair** again to receive a fresh exchange, keys, and
pattern.

### Pairing envelope

All integers are little-endian. Pairing and reconnect records use UDP port
`42825` and the following public envelope:

| Offset | Type | Meaning |
|---:|---|---|
| 0 | `u8[4]` | `HPP5` |
| 4 | `u8` | Protocol version, `5` |
| 5 | `u8` | Record kind |
| 6 | `u16` | Complete datagram length |
| 8 | `u8[16]` | Phone-generated random exchange ID, echoed for this exchange |
| 24 | `u32` | Handshake step or zero |
| 28 | `u16` | Payload length |
| 30 | `u8` | Transport: USB `1`, local network `2` |
| 31 | `u8` | Suite `1`, the fixed suite above |
| 32 | payload | Opaque probe or Noise message bytes |

Record kinds are pairing probe `1`, pairing offer/XX message 1 `2`, XX
handshake continuation `3`, abort `4`, remembered-peer/IK message 1 `5`, and
IK continuation `6`. The complete length MUST equal `32 + payload_length` and
MUST NOT exceed 1200 bytes. The phone generates the fresh exchange ID and an
initial probe has an empty payload. No pre-authentication record can start
input, alter a pairing database, open another interface, or keep asserted
input alive.

The initial probe uses step zero. The host offer carries XX message 1 at step
one, the phone continuation carries message 2 at step two, and the host
continuation carries message 3 at step three. A remembered phone puts IK
message 1 in record kind `5` at step one; the host returns IK message 2 in kind
`6` at step two.

Accepting an offer pins the exchange to that source socket address and ingress
interface. A source or interface change aborts rather than migrating the
handshake. Public abort kind `4` is honored only before Noise splits and only
from that pinned peer; after split, cancellation uses the authenticated abort
application record.

The envelope has no CRC. Strict framing makes corruption fail the exchange,
Noise authenticates the protected handshake payloads, and unauthenticated
discovery is handled as hostile input. Receivers MUST enforce exact lengths and
state transitions, rate-limit replies, cap simultaneous provisional state, and
silently discard unexpected records. A retransmitted Noise handshake step MUST
resend the exact previous bytes; it MUST NOT encrypt changed plaintext under a
reused Noise nonce.

### Transcript-bound lane comparison

A single nonempty chord across six lanes has only 63 states, about 5.98 bits.
Six sequential lane choices have `6^6 = 46,656` states, about 15.51 bits. Both
are too small for this ceremony. Version 5 uses eight sequential lane choices:

```text
6^8 = 1,679,616 possibilities = about 20.68 bits
```

This slightly exceeds a six-decimal-digit comparison space. The six-key
alphabet is sufficient only because the ceremony uses eight choices, permits
one attempt per Pair click, exposes no prefix result over the network, and
cryptographically binds the comparison to this handshake. Ten choices would
provide about 25.85 bits but are not the default usability tradeoff.

Noise XX alone does not authenticate first-use identities. After it completes,
the two sides perform this encrypted commitment exchange before showing or
accepting a pattern:

1. The phone generates a random 32-byte `R_phone` and sends
   `C_phone = BLAKE2s("holodori-v5-sas-commit" || 0x01 || h || R_phone)`.
2. The host generates a random 32-byte `R_host` and replies with
   `C_host = BLAKE2s("holodori-v5-sas-commit" || 0x02 || h || R_host)`.
3. Only after both commitments exist, the phone reveals `R_phone`.
4. After validating `C_phone`, the host reveals `R_host`.
5. Each side validates the other commitment and computes
   `D = BLAKE2s("holodori-v5-sas" || h || R_phone || R_host)`, where `h` is the
   Noise handshake hash/channel binding.

The commitments stop an active intermediary from choosing its final random
value after learning the other side's value. Any failed commitment aborts the
attempt. Cryptographic test vectors for this exchange and the lane mapping MUST
be frozen and independently reviewed before v5 is released.

To map `D` without modulo bias, read its eight 32-bit little-endian words in
order. Accept the first word less than
`floor(2^32 / 6^8) * 6^8`, reduce it modulo `6^8`, then encode it as exactly
eight base-6 digits, most significant first. Add one to each digit to obtain
lane numbers 1 through 6. If no word is accepted, hash
`"holodori-v5-sas-retry" || D` and repeat.

The host displays the eight numbered lane holds. The phone independently
computes the same hidden sequence and compares it locally as the user presses
and releases each lane. Repeated lane numbers are allowed. Every choice
requires a distinct down/up; chord shape, pressure, hold length, and timing add
no security entropy. The phone MAY show neutral progress and provide haptic
feedback, but MUST NOT reveal whether a prefix was correct and MUST NOT send
partial guesses. It collects exactly eight choices before producing either one
generic failure or `PAIR_CONFIRM`; it does not reset or retry within the same
Pair click. Labels and animation MUST make the pattern usable without relying
on color alone.

On an exact local match, the phone shows **Pattern matched** and sends an
encrypted `PAIR_CONFIRM`. That network message alone is not authorization: an
active intermediary controls its own provisional Noise channel and could send
one. The host instead shows **Approve pairing** and tells the user to approve
only while the real phone visibly says **Pattern matched**. This local host
action completes the human comparison and cannot be triggered by a datagram.

Only after a valid `PAIR_CONFIRM` and local host approval does the host persist
the peer identity and return encrypted `PAIR_COMPLETE`; the phone persists only
after that response. A timeout, abort, or rejected local approval leaves no
peer record. After approval, a lost final response may require the user to
forget the one-sided host record and pair again; it MUST never make an
unapproved identity usable for input.

The pattern is a short authentication string, not an encryption key. It is
never transmitted, persisted, logged, included in metrics, or reused. Pairing
does not protect against a person who can see the host display and operate the
phone during the ceremony, or a user who approves despite a failure on the
phone.

### Remembering and revoking a phone

Pairing authorizes the phone identity, not one network interface. The same
paired identity may later use USB or local Wi-Fi, but each start still requires
an explicit transport selection and a fresh Noise IK session. Enabling Wi-Fi
for the first time SHOULD have a clear local-network permission/exposure
explanation.

Both apps MUST offer **Forget device**. Forgetting deletes the peer public key,
credentials, friendly name, and cached permissions; it does not delete latency
reports. Revocation takes effect before another listener opens. Reinstalling an
app creates a new identity and therefore requires pairing again.

## Authenticated application records

Every post-handshake control or gameplay message is one AEAD-protected UDP
datagram. Phone-to-host records use `HPT5`; host-to-phone records use `HPA5`.
The fixed 48-byte public header is authenticated as associated data:

| Offset | Type | Meaning |
|---:|---|---|
| 0 | `u8[4]` | Direction magic, `HPT5` or `HPA5` |
| 4 | `u8` | Protocol version, `5` |
| 5 | `u8` | Direction-specific message type |
| 6 | `u16` | Complete datagram length including tag |
| 8 | `u64` | Connection ID derived from this handshake |
| 16 | `u64` | Gameplay session ID; zero during pairing |
| 24 | `u64` | Direction-local packet number and AEAD nonce |
| 32 | `u64` | Logical frame sequence, cumulative ACK, or control ID |
| 40 | `u32` | Message flags; zero unless defined for the type |
| 44 | `u16` | Encrypted payload length |
| 46 | `u16` | Reserved, zero |
| 48 | bytes | Encrypted payload |
| final 16 | `u8[16]` | ChaCha20-Poly1305 tag |

`complete_length` MUST equal `64 + payload_length` and MUST NOT exceed 1200.
The AEAD associated data is exactly bytes `0..47`. The nonce is four zero bytes
followed by the packet number encoded as an unsigned 64-bit little-endian
integer, matching the Noise ChaChaPoly nonce convention.

Undefined header, frame, contact, and control flag bits MUST be zero. A receiver
rejects an authenticated record with an unknown message type, nonzero reserved
field, or undefined flag rather than guessing at its meaning.

The connection ID is the first eight bytes of
`BLAKE2s("holodori-v5-connection" || h)` interpreted little-endian. A process
MUST reject a collision with another live connection. The gameplay session ID
is a fresh random nonzero `u64` created by the phone for every clean input
session. Pairing and pre-game setup records use session zero; a touch frame
with session zero is always rejected.

There is no CRC in an authenticated record. A tag failure is a silent drop and
MUST NOT receive an ACK. Header fields are visible but authenticated; contact
coordinates, actions, timestamps, pairing controls, and quality results are
encrypted.

### Packet numbers and replay defense

Each direction starts packet number zero under a fresh split key and increments
for every encrypted datagram. The sender MUST establish a fresh Noise session
before wraparound and MUST NOT reset a packet number while retaining a key.
Allocating a packet number burns it even if encryption or the socket send later
fails; a retry uses the next number. A process restart never resumes a split key
or its counter.

Crucially, logical sequence and packet number are different:

- a logical frame keeps one frame sequence across its copies and repairs;
- every immediate copy and every repair gets a new packet number, a fresh
  nonce, and a fresh AEAD operation; and
- the writer updates per-attempt timestamps before encrypting that attempt.

Changing timestamp plaintext while reusing a nonce would break AEAD security
and is forbidden. Re-sending an already encrypted application datagram is also
forbidden; generate a fresh packet number even when its logical content is a
duplicate.

The receiver maintains a bounded replay window of at least 1024 packet numbers
per direction. It commits a packet number to that window only after the tag
validates. An authenticated packet-number replay is discarded. A new packet
number carrying an already applied logical frame is authenticated, not applied
twice, and may cause the current cumulative ACK to be sent again.

### Phone touch payload

Phone message type `1` is a touch frame. Header offset 32 is its session-local
logical sequence. Its encrypted payload is:

| Offset | Type | Meaning |
|---:|---|---|
| 0 | `u64` | Original Android `MotionEvent` sample time |
| 8 | `u64` | Time the Android UI callback began |
| 16 | `u64` | Time this network send attempt began |
| 24 | `u64` | Latest authenticated host-send timestamp echo |
| 32 | `u64` | Phone time when that host record was received |
| 40 | `u8` | Heartbeat `0`, down `1`, move `2`, up `3`, cancel `4` |
| 41 | `u8` | Action pointer ID for down/up |
| 42 | `u8` | Contact count, `0..16` |
| 43 | `u8` | Locked `0x01`, session start `0x02`, historical `0x04` |
| 44 | records | Ten bytes per contact |

Every contact record is a complete member of the simultaneous snapshot:

| Offset | Type | Meaning |
|---:|---|---|
| 0 | `u8` | Pointer ID, unique in this frame |
| 1 | `u8` | Inside `0x01`, physical tip down `0x02` |
| 2 | `i16` | Normalized X multiplied by 10,000 |
| 4 | `i16` | Normalized Y multiplied by 10,000 |
| 6 | `u16` | Normalized pressure multiplied by 65,535 |
| 8 | `u16` | Normalized touch-major size multiplied by 65,535 |

The payload is exactly `44 + 10 * contact_count` bytes. Its maximum
authenticated datagram is 268 bytes. Values outside the painted rectangle are
allowed so an owned contact can remain asserted until lift.

### Host control payload

Host message type `1` is HELLO and type `2` is cumulative ACK. Header offset 32
contains the highest contiguous logical frame accepted by the OS sink, or
`UINT64_MAX` when none has been accepted. Their encrypted 16-byte payload is:

| Offset | Type | Meaning |
|---:|---|---|
| 0 | `u32` | Host logical receive window |
| 4 | `u8` | Requested lane count, `6` |
| 5 | `u8` | Control flags, initially zero |
| 6 | `u16` | Reserved, zero |
| 8 | `u64` | Host monotonic time immediately before this send |

Pairing and quality messages use the same authenticated envelope with gameplay
session zero. Direction-specific types are reserved as follows:

| Phone `HPT5` | Host `HPA5` | Meaning |
|---:|---:|---|
| `2` | `3` | Quality reply / quality probe |
| `3` | `4` | Phone / host SAS commitment |
| `4` | `5` | Phone / host SAS reveal |
| `5` | `6` | Pair confirm / pair complete |
| `6` | `7` | Authenticated abort |
| `7` | `8` | Idle PING / PONG (established gameplay session only) |

Commitment and reveal payloads are exactly 32 bytes. `PAIR_CONFIRM` and
`PAIR_COMPLETE` have empty payloads. Authenticated abort has one `u16` reason:
user cancellation `1`, comparison failure `2`, timeout `3`, or protocol failure
`4`. The reason is diagnostic only and never allows another attempt inside the
same pairing window.

Implementations MUST reject message types that are invalid for the current
handshake role, state, or session kind.

Idle PING has an empty payload and a monotonically increasing control ID in
header offset 32. PONG echoes that ID with the ordinary 16-byte host control
payload; it is never a cumulative gameplay ACK. Both carry the current nonzero
gameplay session ID and zero header flags. An idle phone sends a PING every
500 ms, with two independently encrypted copies. The host returns two PONG
copies. Both endpoints expire an idle session after two seconds without a
new committed frame or a response/request with a fresh idle control ID.
Duplicate idle IDs do not refresh this deadline, even under fresh packet
numbers. PING/PONG never refresh active-input or pending-gameplay watchdogs.

### Authenticated gameplay start

After a remembered-peer IK handshake splits, the host sends authenticated
HELLO with session zero and `UINT64_MAX` in header offset 32. The phone waits
for that ready signal, generates a random nonzero gameplay session ID, and
sends logical sequence zero as `ACTION_CANCEL` with only the session-start flag
set and an empty contact snapshot. The host releases any residual sink state
and acknowledges sequence zero only after that release is accepted. Sequence
one may then reconstruct the phone's latest still-held complete snapshot. Any
other first action, sequence, or session transition fails closed.

All later gameplay controls carry that nonzero session ID. A fresh IK handshake
is required before another gameplay session starts on the connection; session
IDs are not changed in place.

## Reliability and clean recovery

V5 keeps the v4 delivery semantics after authentication:

- Android copies historical samples first, then the current sample, without
  coalescing or individually evicting gameplay frames.
- Each sample is a complete contact snapshot with a 64-bit logical sequence.
- The host buffers up to 256 future logical frames, applies every sequence once
  in order, and deduplicates repairs.
- The phone sends the first copy immediately, a separately encrypted redundant
  copy immediately, and starts repair after 2 ms when cumulative progress does
  not cover the frame. Host controls use the same two-copy rule. An overdue
  repair gets a turn between new immediate pairs, so a burst of new samples
  cannot starve the missing sequence. The writer waits for queue notifications
  or the next repair/heartbeat deadline instead of polling every millisecond.
- The host advances its cumulative ACK only after the selected Windows or Linux
  sink accepts the complete ordered frame.
- An 8 ms acknowledged full-state heartbeat sustains active stationary
  contacts. Idle sessions do not manufacture touch frames.
- A 64 ms oldest-pending or no-ACK-progress boundary makes Android abandon the
  entire gameplay session rather than replay old notes.
- If no ordered frame commits to the OS sink for 32 ms while input is active,
  the host releases all injected input and rejects delayed gameplay from that
  session.
- Authentication success, duplicate packets, duplicate ACKs, discovery,
  malformed records, and future frames behind an ordering hole do not count as
  committed progress.
- Any transport, authentication, read, write, or sink failure releases input.
  Recovery performs a fresh IK handshake, creates a new gameplay session, and
  requires logical sequence zero to be a session-start `CANCEL`. Live failure
  recovery adds no fixed 100 ms delay. Stop sends a best-effort authenticated
  abort before closing; the idle deadline covers a lost abort or process death.
- Android retains only the latest complete contact snapshot across recovery.
  After the fresh `CANCEL`, it reconstructs still-held contacts from that
  snapshot without replaying old transitions.

The authenticated layer does not justify a longer hot-path timeout. Any change
to these values requires loopback fault injection and real phone/PC evidence
against the 8.333 ms target.

## Wi-Fi path measurement

Wi-Fi signal strength is useful diagnosis, not proof of identity and not a
compatibility gate. During the human lane-entry interval, the apps run a
bounded 3-to-5-second probe on the exact authenticated socket and selected
interfaces. The probe MUST stop before gameplay begins and MUST NOT add a wait,
timer, allocation, or report formatting step to the gameplay hot path.

The host sends authenticated quality probes. The phone sends replies using the
normal immediate-copy policy; a small labeled subset deliberately omits both
immediate copies and sends at the 2 ms repair point. The deadline selector is
the same bounded `V5SendQueue` used by gameplay, while pairing runs its own
socket loop. This measures setup-path timing without pretending that the
network itself lost those copies or proving gameplay thread scheduling.
Probe IDs use header offset 32. Quality probing runs for at least 3 and
at most 5 seconds; a fast pattern entry may therefore wait briefly for the
minimum sample, but this setup delay never enters gameplay.

Host quality-probe payloads are one `u64` host send timestamp. Authenticated
header flag `0x01` requests the deliberate repair-only reply. Phone quality
replies echo the probe ID in header offset 32 and use this fixed 32-byte
payload:

| Offset | Type | Meaning |
|---:|---|---|
| 0 | `u64` | Echoed host probe-send time |
| 8 | `u64` | Phone time when the first copy arrived |
| 16 | `u64` | Phone time this reply attempt began |
| 24 | `u8` | Normal `0`, deliberate 2 ms repair `1` |
| 25 | `i8` | Android signal level `0..4`, or `-1` unavailable |
| 26 | `i16` | RSSI dBm, or `INT16_MIN` unavailable |
| 28 | `u32` | Active frequency MHz, or zero unavailable |

Each separately encrypted reply copy updates offset 16. A deliberate repair
reply is first eligible 2 ms after it is queued, then uses the ordinary
single-copy repair rule. The host uses its send/receive clock and
subtracts the phone turnaround to estimate network delay without subtracting
unrelated clock origins. Human-readable report formatting may evolve; these
wire fields do not.

The optimized `v5_host::gameplay_tests::production_loopback_latency` check starts
its timer before payload construction and encryption, and runs UDP reception,
authentication, touch decoding, ordering, lane planning, native event encoding,
and ACK generation through the production host loop. It covers corruption,
one-copy loss, and loss of both immediate copies with a scheduled 2 ms repair.
Only OS acceptance is simulated; this check cannot establish physical Android
scheduling, cable/router latency, OS delivery, or the game's response. A
separate allocation check covers maximum-contact slides, chords, cancellations,
metrics, and encrypted ACKs without allocating after connection setup.

The result shown after pairing SHOULD include:

- phone frequency and derived 2.4/5/6 GHz band;
- raw phone RSSI in dBm and Android's system signal level;
- host RSSI when the selected PC interface is itself Wi-Fi;
- RTT p50, p95, p99, and maximum, plus jitter;
- loss, reordering, duplicate, and immediate-copy-winner counts; and
- observed completion time for the deliberate 2 ms repair probes.

The report MUST distinguish unavailable data from zero. On multi-link Wi-Fi,
the app reports the frequency exposed by Android and labels that it may
represent only one link. The UI shows the 8.333 ms target beside estimated
one-way and repair-completion metrics while reporting RTT separately. It MAY
warn on poor measured tails, but it still lets the user continue. No release
may claim universal Wi-Fi latency without a real-device soak across
representative phones, routers, wired/wireless PCs, bands, and congestion.
Missing or poor quality samples produce an unavailable result or warning; they
never change the cryptographic pairing decision.

Initial v5 MUST NOT collect SSID, BSSID, scan results, location, or nearby
network lists. It SHOULD use ordinary network and Wi-Fi-state access. It MUST
request `NEARBY_WIFI_DEVICES` only if a chosen Android API genuinely needs
Wi-Fi management rather than merely reading the active connection.

During gameplay, the apps MAY update bounded counters from normal frames and
ACKs. They MUST NOT add diagnostic probe traffic, formatting, sorting, or file
I/O to the hot path; any report is written only after Stop.

## Thumb mode

Thumb mode is an Android physical-to-logical coordinate transform, not a wire
protocol flag. The host always receives the same six-lane logical surface.

The phone divides the surface into two three-lane clusters inside the existing
movable, rotatable, and resizable controller container. The user gets one
persisted center-gap adjustment. Pairing uses the same visible split so lane
numbers mean the same thing in setup and play.

The transform MUST run on every historical and current `MotionEvent` sample in
chronological order before the complete snapshot is encoded. A new DOWN in the
center gap owns no lane, produces no gameplay contact, and remains uncaptured
until that pointer lifts. Once a pointer is captured in either cluster,
crossing the gap keeps that pointer captured: a continuous monotonic bridge
maps lane 3 to lane 4, so host interpolation still presses the next lane before
releasing the previous one. A lift in the gap releases the correct logical
contact.

Thumb mode can change only while the play area is unlocked and no contact is
active. Any layout or calibration change emits `CANCEL` before applying the new
transform. Locked gameplay remains renderless except for the existing unlock
control.

## Security and validation gates

Protocol v5 does not make network input harmless. A paired phone can inject the
ordinary OS input the user selected, and a compromised paired device has that
authority. The host must retain its existing scope: no game memory access,
hooks, anti-cheat bypass, or game-network protocol.

Before implementation is called complete, it MUST have:

- [published byte-level test vectors](PROTOCOL_V5_TEST_VECTORS.md) for XX, IK,
  the commitment exchange, lane mapping, record AEAD, nonce progression, and
  replay rejection;
- independent review of the pairing and key-storage implementation;
- tests proving `PAIR_CONFIRM` without local host approval cannot persist or
  authorize a peer;
- tests proving no changed plaintext ever reuses a packet number;
- tests for corrupt tags, replayed packet numbers, duplicate logical frames,
  lost immediate copies, both-copy loss, reordering, and session failure;
- the full transport validation listed in `AGENTS.md`; and
- real USB and Wi-Fi phone/PC soak results before any universal latency claim.

Primary specifications and platform references:

- [Noise Protocol Framework](https://noiseprotocol.org/noise.html)
- [Bluetooth Core architecture: numeric comparison](https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/Core-60/out/en/architecture,-change-history,-and-conventions/architecture.html)
- [RFC 7693: BLAKE2](https://www.rfc-editor.org/info/rfc7693/)
- [RFC 7748: X25519](https://www.rfc-editor.org/info/rfc7748/)
- [RFC 8439: ChaCha20-Poly1305](https://www.rfc-editor.org/info/rfc8439/)
- [Android `Network.bindSocket`](https://developer.android.com/reference/android/net/Network)
- [Android `WifiInfo`](https://developer.android.com/reference/android/net/wifi/WifiInfo)
- [Android Wi-Fi permissions](https://developer.android.com/develop/connectivity/wifi/wifi-permissions)
- [Android Keystore](https://developer.android.com/privacy-and-security/keystore)
- [Windows DPAPI](https://learn.microsoft.com/en-us/windows/win32/seccrypto/example-c-program-using-cryptprotectdata)
- [Secret Service API](https://specifications.freedesktop.org/secret-service/latest-single/)
