# V5 latency fixes and validation — 2026-09-05

## Combined v0.5.1-alpha3 pressure/diagnostics check — 2026-09-14

Pressure regression tests cover default-off behavior, inclusive cutoff,
historical-sample admission, pressure dips after admission, independent fingers,
lift/omission/CANCEL and pointer reuse. Host regressions cover a newly admitted
finger sharing a held lane, partial submission retries, reconstruction, and
every crossed slide lane. Authenticated decoding accepts the new keyboard-only
flag while retaining physical TIP and rejecting other reserved bits.

The optimized production-loopback check now also sends repeating rejected
brush/admission/lift sequences through real authentication and key planning.
Final OS submission is simulated. 160 samples per cell, including startup:

| Delivery | Diagnostics | Observed maximum ms |
|---|---|---:|
| Healthy | off / on | 0.114 / 0.446 |
| First copy corrupt | off / on | 1.854 / 1.501 |
| First copy lost | off / on | 0.541 / 0.476 |
| Both copies lost | off / on | 3.713 / 3.253 |
| Pressure admission, healthy | off / on | 0.958 / 0.181 |
| Pressure admission, both copies lost | off / on | 3.704 / 3.308 |

Every observed maximum was below 8.333 ms. The separate V4 fault check measured
0.0244 ms corrupt/redundant, 0.008 ms one-copy-loss, and 2.1202 ms both-copy-loss.
These are software loopback results, not Android sensor/pressure-filter execution,
physical USB/Wi-Fi latency, or game-observed timing. No Android device was attached;
the calibration UI and brush/tap separability still need physical validation.

## Diagnostic redesign validation — 2026-09-14

Schema-2 design, audit, overhead bounds and report examples are in
[DIAGNOSTICS.md](DIAGNOSTICS.md). This section supersedes the old diagnostic
metric interpretation; the dated measurements below remain historical results.

Validated on this Windows workspace:

- Native `cargo test --all-targets`: 100 library tests and 2 host tests passed;
  both timing tests, ignored in the ordinary suite, were run explicitly below.
- Native `cargo clippy --all-targets -- -D warnings`, formatting, and optimized
  native build passed. The real authenticated receive/commit/ACK allocation test
  still records **zero gameplay-thread allocations with diagnostics enabled**.
- Android debug/release builds, debug/release lint, and both unit-test variants
  passed (34 tests each). Lint retains existing warnings, with no errors.
- Launcher frontend: 20 tests and production build passed. Launcher Rust:
  4 tests and strict all-target Clippy passed.
- Offline report correlation: 5 tests passed, including coarse-clock ambiguous
  copy attribution. Generated healthy/degraded fixtures use the production
  Android and native analysis/reporting code.
- `git diff --check` passed.

Final optimized V5 production-loopback run, 160 current-event samples per cell,
**including all startup samples**. Encryption, datagram receipt, authentication,
reorder/commit, the six-lane planner and ACKs are real; the final OS submission is
simulated so validation does not type into another application. The table uses
observed ranks, not confidence claims about a population p99.

| Delivery | Diagnostics | Median ms | Observed rank 99% ms | Maximum ms |
|---|---|---:|---:|---:|
| Healthy | off | 0.025 | 0.497 | 0.645 |
| Corrupt first copy | off | 0.054 | 0.357 | 1.705 |
| First copy lost | off | 0.040 | 0.563 | 0.594 |
| Both immediate copies lost | off | 2.523 | 2.914 | 2.915 |
| Healthy | on | 0.020 | 0.259 | 0.621 |
| Corrupt first copy | on | 0.046 | 0.229 | 0.237 |
| First copy lost | on | 0.035 | 0.589 | 1.622 |
| Both immediate copies lost | on | 2.542 | 3.086 | 3.192 |

All observed maxima met 8.333 ms, including the 2 ms repair after both immediate
copies were lost. An earlier run during implementation also passed: enabled
healthy/corrupt-first/first-lost maxima were 0.103/0.132/0.075 ms, and enabled
both-copy-loss maximum was 3.895 ms. These variations demonstrate scheduling
noise; they do not prove that diagnostics improve latency. Median healthy/repair
timings showed no material added delay in this local test.

The separate legacy datagram fault check passed with 0.0288 ms
corrupt-plus-redundant recovery, 0.0152 ms one-copy-loss delivery and 2.0979 ms
both-copy-loss repair. Commands:

```powershell
cargo test --release --lib v5_host::gameplay_tests::production_loopback_latency -- --ignored --exact --nocapture --test-threads=1
cargo test --release --lib network::tests::loopback_fault_recovery_stays_inside_one_120_hz_frame -- --ignored --exact --nocapture --test-threads=1
python -m unittest discover -s tools -p 'test_*.py' -v
```

`adb devices -l` reported no attached device. Android instrumentation/device UI
tests, Android runtime allocation/scheduling measurement, phone/cable/PC and
phone/router/PC soaks, thermal/driver behavior, actual OS/game observation and
Linux execution were **not** validated here. Those remain physical release
validation requirements, not evidence of universal latency or root-cause proof.

---

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
