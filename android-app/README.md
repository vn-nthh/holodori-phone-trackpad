# Android companion app

This module is the phone side of Holodori Phone Trackpad. It defaults to
authenticated protocol v5 over an explicitly selected USB-tether or
Wi-Fi/local-network path. First use compares an eight-step six-lane pattern and
later sessions authenticate the remembered host with Noise IK. Protocol v4
remains available only through the explicit legacy USB option.

Android's normal USB tethering feature provides the physical USB path; neither
transport needs USB debugging, root, a custom driver, location, SSID/BSSID
access, or nearby-network scanning. See
[`../PROTOCOL_V5.md`](../PROTOCOL_V5.md) and the
[`../PROTOCOL_V5_TEST_VECTORS.md`](../PROTOCOL_V5_TEST_VECTORS.md) byte vectors.

## Build

1. Install Android Studio with Android SDK 35 and JDK 17 or newer.
2. Open this `android-app` directory as a project.
3. Build the `app` module, or run:

   ```text
   gradlew.bat assembleDebug
   ```

4. Install `app/build/outputs/apk/debug/app-debug.apk` through a normal
   download/install flow. Installing the APK does not require USB debugging.

## Pair and connect

1. Choose the same **USB tethering** or **Wi-Fi / local network** transport on
   the phone and host.
2. For USB, connect one data cable and enable Android USB tethering. For Wi-Fi,
   put the phone and PC on the same private subnet.
3. Press **Pair** on the host and phone. Replicate the host's eight numbered
   lanes on the phone, confirm **Pattern matched**, then approve locally on the
   real host.
4. Press **Start** on both sides with the intended transport. A fresh Noise IK
   session authenticates before any touch can reach the OS input sink.

Legacy-v4 discovery deliberately excludes normal Wi-Fi, cellular, VPN, and
upstream Ethernet networks. It accepts an `HPTD` acknowledgement only from a
selected USB-tether subnet, validates the advertised port, and pins the first
host socket address for that session. V5 instead confines discovery to the
transport selected before Pair or Start; its Wi-Fi path must be a physical
Wi-Fi `Network`, never cellular or a VPN.

## Protocol v5 behavior

Every touch snapshot is carried by one HPT5 ChaCha20-Poly1305 datagram. Android
sends two independently encrypted immediate copies; repair begins after 2 ms
with a fresh packet number and nonce. The host reorders by logical sequence,
submits each snapshot exactly once, and sends an authenticated cumulative ACK
only after the OS sink accepts it. A 1,024-packet replay window rejects reused
packet numbers without allowing a bad tag to consume a window position.

Stationary holds use acknowledged 8 ms full-state heartbeats. A 64 ms pending
boundary abandons the entire session, performs a fresh IK handshake and
session-start `CANCEL`, then reconstructs only the latest still-held snapshot.
Idle sessions exchange authenticated PING/PONG every 500 ms and expire after
two seconds without a fresh response. Stop sends an authenticated abort before
closing. Idle controls never extend a held-input or pending-frame watchdog.
Wi-Fi pairing measures the authenticated path and reports band, signal, delay,
loss, jitter, and deliberate 2 ms repair completion; the result warns but never
changes the cryptographic pairing decision. Thumb mode transforms every
historical and current sample into the same logical six-lane surface.

## Legacy protocol v4 behavior

Phone-to-host frames are variable-size `HPT4` records containing:

- a transport session and 64-bit sequence;
- Android event, UI-callback, and network-write timestamps;
- the latest duplex host timestamp echo for clock-origin-independent timing;
- action and action-pointer ID;
- a complete contact snapshot with pointer ID, in-zone/tip flags, X/Y,
  pressure, and touch-major size;
- an IEEE CRC-32.

Each HPT4 frame is one UDP datagram, so it is well below the USB-tethered
Ethernet MTU. The PC returns fixed-size `HPA4` HELLO and cumulative ACK
datagrams. Android keeps each encoded frame in an ordered queue until its
sequence is acknowledged. Each frame and control record is sent twice
immediately; if both copies disappear, Android begins replay after 2 ms.

When a contact is active and the live queue is otherwise empty, an 8 ms
acknowledged keepalive carries the latest complete contact snapshot. This lets
the Windows host sustain a stationary contact above the game's 120 Hz maximum
and reconstruct a hold after a quick host restart. Idle sessions send no
synthetic touch frames; discovery acknowledgements keep the connection alive.

If cumulative ACK progress is absent for 64 ms during gameplay, the app drops
queued gameplay, starts a new session after an initial 4 ms backoff, and sends
a session-start `CANCEL`. Duplicate or invalid ACKs cannot hide an ordering
stall, and the first active frame receives a fresh response window instead of
inheriting an old idle timestamp. Idle discovery keeps the two-second timeout.
The latest contact snapshot continues to update during the socket restart, so
a stationary finger is restored in the fresh session without replaying stale
transitions.

Protocol v4 uses CRC for corruption detection, not authentication. Interface
and subnet confinement are its trust boundary, so it is USB-only and requires
an explicit legacy selection.

See [`../PROTOCOL_V4.md`](../PROTOCOL_V4.md) for the byte layout and
acknowledgement semantics, and [`../PROTOCOL_V5.md`](../PROTOCOL_V5.md) for the
default authenticated transport.

The JVM tests exercise replay authentication, independent send/receive locks,
and the production retained-frame scheduler, including burst fairness and the
2 ms repair deadline. Run them with `./gradlew testDebugUnitTest`.
`./gradlew connectedDebugAndroidTest` additionally verifies first-use identity
creation, pairing persistence, reload, and Forget against Android's real
Keystore. That test uses isolated preferences and a temporary key alias; it
preserves the installed app's pairing. Its APK can be built without a device
using `./gradlew assembleDebugAndroidTest`.
