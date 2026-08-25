# Android companion app

This module is the phone side of Holodori Phone Trackpad. The experimental
branch uses acknowledged protocol v4 over the phone's USB tethering/RNDIS
network. Android's normal USB tethering feature provides the physical USB path;
the app only needs ordinary network access.

## Build

1. Install Android Studio with Android SDK 35 and JDK 17 or newer.
2. Open this `android-app` directory as a project.
3. Build the `app` module, or run:

   ```text
   gradlew.bat assembleDebug
   ```

4. Install `app/build/outputs/apk/debug/app-debug.apk` through a normal
   download/install flow. Installing the APK does not require USB debugging.

## Connect

1. Open the app on the phone.
2. Enable Settings > Network & internet > Hotspot & tethering > USB tethering.
3. Connect one USB data cable to the Windows PC.
4. Start the native host. The app discovers it on UDP port 42825 and begins
   sending as soon as the Windows RNDIS adapter is ready.

Discovery deliberately excludes normal Wi-Fi, cellular, VPN, and upstream
Ethernet networks. The app accepts an `HPTD` acknowledgement only from a
selected USB-tether subnet, validates the advertised port, and pins the first
host socket address for that session. Some OEMs expose tethering as `rndis`,
`ncm`, `usb`, or an otherwise hidden `eth*` interface; the fallback remains
conservative so a normal Android network is not selected.

## Protocol v4 behavior

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
and subnet confinement are the current trust boundary; cryptographic pairing
is deferred to a future wire version.

See [`../PROTOCOL_V4.md`](../PROTOCOL_V4.md) for the byte layout and
acknowledgement semantics.
