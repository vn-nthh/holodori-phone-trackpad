# Android companion app

This module is the no-debugging phone side of Holodori Phone Trackpad. On the
experimental branch it speaks acknowledged AOA protocol v4.

## Build

1. Install Android Studio with Android SDK 35 and JDK 17 or newer.
2. Open this `android-app` directory as a project.
3. Build the `app` module, or run:

   ```text
   gradlew.bat assembleDebug
   ```

4. Install `app/build/outputs/apk/debug/app-debug.apk` through a normal
   download/install flow. Installing the APK does not require USB debugging.

The checked-in Gradle wrapper is pinned to Gradle 8.10.2. The app uses only
Android platform APIs and has no runtime library dependencies.

## USB identity

The experimental PC host sends this AOA identity:

| Field | Value |
|---|---|
| Manufacturer | `Holodori` |
| Model | `Phone Trackpad` |
| Version | `4.0` |

Android launches or offers this app after the user approves accessory access.
The only physical connection is the phone's normal USB data cable to the PC.

## Protocol v4 behavior

Phone-to-host frames are variable-size `HPT4` records containing:

- a transport session and 64-bit sequence;
- Android event, UI-callback, and USB-write timestamps;
- the latest duplex host timestamp echo for clock-origin-independent timing;
- action and action-pointer ID;
- a complete contact snapshot with pointer ID, in-zone/tip flags, X/Y,
  pressure, and touch-major size;
- an IEEE CRC-32.

The PC returns fixed-size `HPA4` HELLO and cumulative ACK records. Android keeps
each encoded frame in an ordered queue until its sequence is acknowledged.
There is no stale-age reset or queue-capacity eviction. An unacknowledged frame
is replayed after 4 ms.

When the live queue is otherwise empty, an 8 ms acknowledged keepalive lets the
Windows host sustain a stationary contact above the game's 120 Hz maximum.

The locked touch path reuses primitive pointer arrays and precomputed zone
transforms. It enqueues USB input before changing visual state, while visual
updates remain outside the transport path. Android `MotionEvent` history is
serialized chronologically as complete multi-contact snapshots.

See [`../PROTOCOL_V4.md`](../PROTOCOL_V4.md) for the byte layout and
acknowledgement semantics.
