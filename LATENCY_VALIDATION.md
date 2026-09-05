# V5 latency fixes and validation — 2026-09-05

The audit findings are addressed in code. V5 retains authenticated identities,
local pairing approval, independent nonces for every copy, and ACK-after-sink
semantics while removing avoidable work from the input path.
Install the updated phone and host builds together: authenticated idle
PING/PONG adds V5 control types that older development builds do not understand.

| Finding | Implemented change |
|---|---|
| First-use Android credential encryption supplied a forbidden IV | Let Android Keystore generate the GCM IV; save `Cipher.getIV()` with the ciphertext. Added an isolated device test for creation, pairing, reload, and Forget. |
| An idle phone could leave the host pinned indefinitely | Authenticated 500 ms PING/PONG, a two-second idle expiry, and a best-effort authenticated Stop. Duplicate idle IDs never extend the deadline; idle traffic never extends active-input watchdogs. |
| Live V5 recovery waited a fixed 100 ms | Removed that delay. Fresh IK and sequence-zero CANCEL remain mandatory. Failed discovery retains its separate retry backoff. |
| Latency tests bypassed production work and excluded initial encryption | Replaced the toy V5 test with the production receive/commit/ACK loop, real UDP, real crypto, lane planning, and platform event encoding. Timing starts before payload construction and encryption. |
| Host frames allocated and copied through multiple stages | Reused datagram buffers, in-place authenticated decode, inline contacts, a bounded reorder ring, and borrowed frames through sink acceptance. Removed the unused stream parser and buffered wrappers. |
| Android receive could block encryption and allocated control objects | Separate directional cipher locks and reused control/header/plaintext buffers. A concurrency regression blocks decryption while verifying that send still completes. |
| Android polled every millisecond and rescanned the retained queue | A preallocated retained-frame ring tracks immediate sends and repairs. The writer waits for notifications or the actual deadline; an overdue repair gets a turn between fresh pairs. Pairing probes use the same deadline selector. |

Additional reductions found during implementation:

- Replaced an implicit per-frame `KeyboardState` vector clone with copies into
  existing storage. Preallocated lane plans and Windows/Linux event buffers for
  maximum-contact slides; cancellation reuses the same buffers.
- Bounded metrics arrival tracking to 256 slots. Disabled metrics now allocate
  no histograms, saving 8 MiB per host instance.
- Moved Android interface revalidation to the watchdog worker. Host startup
  formatting finishes before HELLO permits the phone to send gameplay.
- The Android ACK path compares the packet's address and port directly,
  avoiding `getSocketAddress()`'s extra allocation.
- Timestamp host receipt before authentication and parsing, so host service
  metrics include those costs. A new gameplay session on an existing V5
  connection is rejected and requires fresh authentication.

## Measured Windows loopback latency

Optimized native build, 128 measured events per case after 32 warm-up events.
All times below include payload construction, phone-side Rust encryption,
kernel UDP delivery, production host authentication/decoding/ordering, lane
planning, and Windows event encoding. Only final OS acceptance is simulated.
The repair case uses an OS-scheduled wait until the 2 ms deadline, not a spin.

| Delivery case | p50 | p99 | Maximum |
|---|---:|---:|---:|
| Healthy | 0.022 ms | 0.047 ms | 0.047 ms |
| First copy corrupt | 0.024 ms | 0.081 ms | 0.126 ms |
| First copy lost | 0.022 ms | 0.039 ms | 0.049 ms |
| Both immediate copies lost | 2.524 ms | 2.951 ms | 3.166 ms |

Every measured event was below the 8.333 ms target. The existing V4 fault check
also passed, with both-copy repair at 2.0446 ms. Its narrower harness excludes
work measured by the new V5 test; those values do **not** establish a V4/V5
speed ratio or a before/after percentage improvement.

The same optimized production-loop check passed in Debian WSL. Healthy
delivery measured 0.064 ms p99 (0.104 ms maximum); both-copy loss measured
2.447 ms p99 (2.678 ms maximum). Corrupt-first and first-lost maxima were
0.190 ms and 0.068 ms respectively. These are virtualized Linux loopback
results, with uinput acceptance simulated.

The allocation regression processes 1,023 frames after connection setup,
including 16-contact chords, full-width slides, ownership replacement,
cancellation, enabled metrics, event encoding, and encrypted ACKs. It observes
**zero Rust heap allocations** on both Windows and Linux. A separate test confirms
that constructing disabled metrics allocates nothing.

## Validation performed

- Windows: `cargo test --all-targets` (83 passed), strict Clippy, optimized
  build, and both explicit V4/V5 loopback timing checks passed.
- Linux in Debian WSL: `cargo test --all-targets` (73 passed), strict Clippy,
  optimized build, and both explicit V4/V5 loopback timing checks passed.
  Tests that emit real OS input remain opt-in.
- Android: 25 JVM tests passed; debug/release builds and debug/release lint
  passed. Lint reports zero errors and 15 warnings. The Keystore
  instrumentation APK builds successfully.
- Launcher: seven frontend tests, frontend production build, three Rust tests,
  and strict Rust Clippy passed.
- Rust formatting and `git diff --check` passed. Release scripts now select
  the production V5 timing check and print its measurements.

Run the V5 timing check without competing builds, from `native-host`:

```sh
cargo test --release --lib v5_host::gameplay_tests::production_loopback_latency -- --ignored --exact --nocapture --test-threads=1
```

Run the Android device check from `android-app` with a connected device:

```sh
./gradlew connectedDebugAndroidTest
```

No Android device was connected during this work. The Keystore device test,
physical USB/Wi-Fi soak, Android writer scheduling, real SendInput/uinput or
Windows Touch acceptance latency, and game response were not measured. The
loopback results establish a software budget check, not universal physical
latency or an absolute optimum. Android queue tests exercise the actual
selector but do not turn a desktop timing result into a phone measurement.

The Keystore change follows Android's documented rule to let `Cipher` generate
the IV when randomized encryption is required:
[KeyGenParameterSpec.Builder documentation](https://developer.android.com/reference/android/security/keystore/KeyGenParameterSpec.Builder.html#setRandomizedEncryptionRequired(boolean)).
