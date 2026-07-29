# Android companion app

This module is the no-debugging phone side of Holodori Phone Trackpad.

## Build

1. Install Android Studio with Android SDK 35 and JDK 17 or newer.
2. Open this `android-app` directory as a project.
3. Build the `app` module, or run:

   ```text
   gradlew.bat assembleDebug
   ```

4. Distribute `app/build/outputs/apk/debug/app-debug.apk` through a normal
   download/install flow. Installing a downloaded APK does not require USB
   debugging.

The checked-in Gradle wrapper is pinned to Gradle 8.10.2. The app uses only
Android platform APIs and has no runtime library dependencies.

## USB identity

The manifest matches this AOA identity:

| Field | Value |
|---|---|
| Manufacturer | `Holodori` |
| Model | `Phone Trackpad` |
| Version | `1.0` |

The PC sends this identity during the AOA handshake. Android then launches or
offers this app and grants access to the accessory after user confirmation.

## Input protocol

Each phone-to-PC record is 24 bytes, little-endian:

| Offset | Type | Meaning |
|---:|---|---|
| 0 | 4 bytes | `HPT1` magic |
| 4 | `u8` | Protocol version, currently `2` |
| 5 | `u8` | Action: heartbeat `0`, down `1`, move `2`, up `3`, cancel `4` |
| 6 | `u8` | Android pointer ID |
| 7 | `u8` | Flags; see below |
| 8 | `i16` | Zone-local X multiplied by 10,000 |
| 10 | `i16` | Zone-local Y multiplied by 10,000 |
| 12 | `u32` | Monotonic sequence |
| 16 | `u64` | Android input event time in nanoseconds |

Coordinates are intentionally not clamped to `0..1`, allowing the PC overlay
to show when a finger has crossed outside the configured play zone.

On Android 14 and newer, the timestamp comes from the nanosecond-precision
motion-event API. Older Android versions retain the millisecond-source fallback.
Phone and PC timestamps have different origins and must be aligned before
benchmark comparisons.

Protocol v2 adds an explicit transport epoch. The first record after Android
opens the accessory is `CANCEL` with flag `0x04` (session reset). The PC does
not accept touch input until that marker arrives. Each PC process sends one
8-byte `HPTC` attach record on the accessory OUT endpoint. A later attach to
the same Android transport makes Android drop the stale queue and send another
session reset with flag `0x08` (host recovery). This prevents records and
diagnostics from an old PC process from leaking into a new session.

The remaining low flag bits are inside zone `0x01`, play locked `0x02`,
session reset `0x04`, and host recovery `0x08`.

Heartbeat records carry backward-compatible queue diagnostics. Flag `0x80`
marks the fields as valid, pointer ID contains maximum queue depth, X contains
maximum age in 10-microsecond units, and Y contains the number of queue
resynchronizations since the previous heartbeat. Flags `0x10`, `0x20`, and
`0x40` indicate a warning, resynchronization, and 100 ms failsafe respectively.
