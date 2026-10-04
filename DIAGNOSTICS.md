# Doritrack diagnostics, schema 2

Diagnostics observe gameplay; they never control delivery, ACK progress, replay,
watchdogs, pairing, input ownership, or an OS sink. V5 wire records, authentication,
nonce allocation, immediate copies, 2 ms repair, MTU limits and failure boundaries
are unchanged. Keyboard mode remains the gameplay baseline.

## Use

Enable the launcher's report option (or native `--metrics`). On Android, enable
**Preferences → Diagnostics** before Start. Stop both applications after the run.
The host writes a readable `.txt` and a schema-2 `.json` beside it in the existing
report folder. Android saves its summary and incident CSV privately after Stop;
**Preferences → Diagnostic reports → Export** saves a ZIP through Android's file
picker. Allow report saving to finish before exporting. Android retains eight
runs; host reports remain under the user's control.

Join the stopped reports without contacting either device:

```powershell
python tools/diagnostic_report.py --host path/to/host.json --android path/to/android.zip --output build/joined.json
```

The command also writes `joined.md`. Multiple host JSONs, Android CSVs or ZIPs can
be supplied. A CSV's companion `.txt` supplies whole-run Android counts. No upload
occurs. Session IDs and sequence numbers join devices; the phone's actual
encryption-start timestamp identifies the winning send attempt. A copy that
arrived before its writer finished recording is still matched after Stop.

Examples generated through the production analyzers:

- [Healthy host session](docs/diagnostic-examples/healthy-host.txt),
  [Android session](docs/diagnostic-examples/healthy-android.txt),
  [correlated report](docs/diagnostic-examples/healthy-correlated.md).
- [Degraded host session](docs/diagnostic-examples/degraded-host.txt),
  [Android session](docs/diagnostic-examples/degraded-android.txt),
  [correlated incidents](docs/diagnostic-examples/degraded-correlated.md).

These are synthetic examples, not measurements of a phone or the game. Regenerate:

```powershell
python tools/generate_diagnostic_examples.py --java-home .android-sdk/jdk17/jdk-17.0.20+8
```

## Pipeline audit and ownership

The audit covered `TrackpadView`, `TouchTransport`, both Android transports,
`V5SendQueue`, `V5NetworkBinding`, V5 encryption/control handling, Rust datagram
receive/authentication, `OrderedFrames`, `input::commit_ready`, keyboard planning
and partial native submission, the touch sink, the controller/report lifecycle,
and pairing quality reporting.

| Original weakness | Replacement and owner |
|---|---|
| All received datagrams improved receive cadence | Host cadence uses unique admitted gameplay samples. Logical duplicates, AEAD packet replay, heartbeats, and session-start CANCEL are distinct populations. |
| RTT/2 hid directional stalls | Worker computes feasible clock-offset intervals with explicit drift assumptions. Reports carry directional lower/upper bounds and uncertainty; no symmetric point estimate. Pairing no longer displays RTT/2. |
| Historical samples only had dispatch age | Every historical sample retains event/callback/winning-send timestamps and receives its own sink-bound and sender-ACK distribution. Current and historical populations remain separate. |
| Repaired send timestamp appeared to be first-send queue latency | Host label is callback-to-**winning attempt**. Android owns queue admission, first selection, each actual encryption/send invocation and its local completion, attempt count, and repair-deadline lateness. |
| Accepted-only statistics hid failed frames | Android counts offered-but-not-queued, queued, cumulative-ACK-retired and discarded records. Host counts admitted, rejected, sink-failed, accepted and observed-uncommitted-at-boundary frames. Failed sink durations enter outcome statistics. Missing host evidence is never called packet loss. |
| Pending ring slots/reset erased failure evidence | Admission is classified before diagnostic pending storage. Rejected distant frames cannot replace a pending observation. Boundaries materialize unresolved outcomes and preserve their original frame keys. |
| ACK repeats looked like progress | Android records advancing, duplicate/nonadvancing and invalid-future ACK decisions, pending depth/oldest age/window and every retirement. Host distinguishes ACK progress/repeat submissions and send failures. |
| One worst frame / flat counters lacked context | Worker captures clustered incidents with pre/post evidence, causal stage tags, affected keys, source-local time, first resumed progress, fresh authenticated boundary and reconstructed snapshot. |
| Fixed histogram silently clipped ~524 ms | Integer logarithmic histograms span the duration type's range; exact maxima and strict budget counts are independent of bins. Quantiles are intervals with explicit sample support. |
| USB labels on Wi-Fi and unverified performance hints | Reports identify selected transport and interface. Android samples selected-network signal, held lock, power save, foreground, interactive state, thermal severity, and suspend-clock difference off the input threads. Host records priority/HighQoS readback at setup and Stop. |
| Per-frame histograms on the native receive thread | The input thread publishes numeric events. Worker owns histograms, correlation, clock model, incident classification and storage. Report formatting and I/O occur after Stop. |

Legacy USB v4 uses the new host reporting path and local clock caveats, including
parser counters. Android's new sender telemetry is V5-only; a v4 report must not
be interpreted as having sender-side coverage. Existing v4 CRC/interface behavior
is preserved.

## What latency means

The host timestamp is **userspace receive completion before authentication**,
not NIC arrival. Android attempt time is immediately before encryption; socket
return is local OS submission, not over-the-air transmission. Forward-path
bounds therefore include encryption, send/receive scheduling and kernel queueing.
Host receive-to-ready includes authentication, decoding, ordering and scheduling.
Sink duration includes lane planning, native API submission, partial acceptance
and retries. A successful unchanged-state frame can require no new syscall.

The sink outcome is ordinary OS acceptance, never game recognition or
input-to-display latency. A rapid release/press can be correctly submitted while
the game still fails to sample it. This diagnostic cannot inspect the game,
distinguish a driver scheduling delay from airtime with certainty, or prove an
AP's QoS behavior.

For host timestamps H and phone timestamps P, the exchange yields a feasible
offset `H − P` interval:

```text
lower = host_control_send − phone_control_receive − drift_allowance
upper = host_frame_receive − phone_attempt_send + drift_allowance
forward interval = host_frame_receive − phone_attempt_send − [upper, lower]
reverse interval = phone_control_receive − host_control_send + [lower, upper]
```

Negative durations are not fabricated into healthy samples. Missing, invalid or
older-than-2-second echoes have separate counts. The model intersects recent
constraints, expands them by an assumed **1000 ppm relative rate bound**, and
expires the previous interval after one second without a sample. Inconsistent
constraints invalidate that observation and produce an incident. A feasible
interval is not a measured clock synchronization accuracy or confidence interval.
Persistent asymmetry can remain unresolved; initial directional bounds can be
wide. Repeated control echoes are excluded from the reverse-path distribution.
Heartbeats may inform the clock model, but their forward timing is in separate
control-frame distributions, never gameplay latency/cadence.

Event-to-sink intervals add the phone-local event-to-winning-attempt duration and
host-local receive-to-sink duration to these forward bounds. Since V5 carries no
Android timestamp-resolution field, the host conservatively allows 1 ms event
quantization plus the stated phone rate allowance. The report separates:

- definite budget violations **under those assumptions**;
- upper bounds within budget under those assumptions;
- intervals crossing the budget (uncertain);
- accepted gameplay with unavailable bounds;
- rejected, failed, discarded, and unseen-at-host input, outside successful latency populations.

The Android event-to-cumulative-ACK duration is entirely in the phone clock and
provides an upper bound on OS acceptance, including host feedback and the return
path. A large ACK duration alone is not proof of late host input. A host commit
plus sender discard suggests feedback/recovery trouble, not that the note was
never injected. Repair alone cannot distinguish forward loss from ACK loss.

Dispatch durations on Android below API 34 inherit millisecond MotionEvent
precision. Android documents this timestamp's uptime time base; nanoseconds do
not imply nanosecond touch-sensor precision. See the
[MotionEvent API](https://developer.android.com/reference/android/view/MotionEvent)
and [SystemClock clocks](https://developer.android.com/reference/android/os/SystemClock).

## Incidents and recovery

The default threshold is one 120 Hz frame, 8.333 ms (`--warn-ms` customizes the host
threshold). A single outlier matters even when percentiles look good. Dispatch,
queue/send, transport bounds, sink outcomes, ordering holes, repair, protocol
discard, watchdog/reset, ingress rejection, environment constraints and recording
gaps are evidence, with different certainty. An ordering hole means missing **at
that moment**, not permanent packet loss. Sparse taps and rests do not trigger a
receive-gap failure merely because they are sparse: gameplay cadence excess is
compared to source-event spacing. The gameplay liveness watchdog remains based
on committed sequence progress.

Related anomalies remain one incident until 32 ms of quiet. A pending host hole
keeps its incident open through the affected buffered range. Retained history is
128 preceding **records**, followed by surrounding records until the quiet
boundary; it is not a guaranteed number of milliseconds. One long failure does
not emit a warning per retry/frame. Recovery after a long reconnect is linked
back to the original failed incident, even after quiet-window closure. Reports
distinguish first resumed OS commit, fresh-session CANCEL commit, reconstructed
contact snapshot, and sender-observed ACK completion. Resumed progress is not a
claim that all subsequent timing is healthy.

The joiner combines cross-device incidents only when affected sequence ranges
overlap in the same session. Shared pre-context alone is insufficient. A failure
with no assigned sequence (Android input rejected before admission) retains its
event time and an unassigned sentinel, not a fabricated logical frame. Standalone
reports and raw events remain authoritative when evidence is insufficient to
merge incidents.

## Bounded storage and cost

| Location | Bound / behavior |
|---|---|
| Native producer queue | 16,384 × 96-byte numeric records; SPSC acquire/release slot ownership, no CAS, mutex, wakeup, wait, allocation or formatting during publication. |
| Android producer queues | Two 8,192 × 96-byte primitive rings. Queue-domain producers use the already-held transport lock. Writer owns its own ring; one ownership CAS at writer startup, none per frame. Overlapping old/new writers can decline diagnostic ownership, explicitly counted. |
| Native worker | Fixed protocol-sized pending slots, 128-record history, at most 256 incidents × 512 evidence records; bounded affected-key sets and histogram key space. Worker priority is lowered. |
| Android worker | Preallocated numeric analysis/retention arrays; 128 history records, 256 incident summaries, 32,768 total evidence records, 512 per incident. No per-record worker allocations. Environmental object/API work is at most once every two seconds. |
| Full-session distributions | 2,049 integer bins per metric, 32 subdivisions per power of two, including exact zero/small values. No half-second clipping. Native sum uses u128; Android sum saturates at signed-long maximum for extraordinarily long runs. |
| Saturation | Drop **diagnostic** records only. Total drops and first/last dropped local timestamps are reported; omitted incidents, omitted detail and unavailable writer generations stay visible. No diagnostics backpressure on gameplay. |
| Stop/export | Join/drain workers only after input stops. Format reports then. Android stages `.tmp` files before finalizing exports and expires only its own old report files. Host detailed storage per report is bounded; users control report-file retention. |

The worker's 4 ms sleep only schedules diagnostics. It never batches, polls for,
or delays gameplay delivery. Disabled host diagnostics allocate no storage and
spawn no worker. Android diagnostics default off. The enabled writer adds one
conditional post-send timestamp; existing callback, enqueue, selection, receive
and sink-ready timestamps are reused. No V5 extension, telemetry datagram, live
JSON, network probe, UI update, file write or synchronous analysis is added to
gameplay. Histograms/counts cover all **observed** records, not just retained
incident context. If recording overflows, whole-run coverage is explicitly
incomplete and cannot establish absence of hitches.

Quantiles use nearest ranks and report the enclosing histogram interval. P99 is
suppressed below 1,000 samples, p99.9 below 10,000; these gates provide nominal
tail-rank support, not independence or statistical confidence. Every metric has
its own denominator, invalid count, exact maximum and exact strict-over-budget
count. No startup/warmup samples are silently removed. Lower- and upper-bound
percentiles are not a point estimate of the real latency distribution.

## Environment verification and limits

`WifiLock.isHeld()` verifies the application's lock acquisition. It cannot verify
driver support or effective radio policy. Android restricts low-latency mode to
an AP connection, foreground app and screen-on state; these conditions are
reported separately. No scan/SSID/BSSID/location permission is introduced.
Signal comes from the selected Android Network where the API supplies it;
otherwise it is unavailable. An MLO device may expose one link. See
[WifiManager's documented restrictions](https://developer.android.com/reference/android/net/wifi/WifiManager#WIFI_MODE_FULL_LOW_LATENCY).

Windows setup/Stop reads process priority, input-thread priority and the process
power-throttling masks. This verifies API state, not scheduling guarantees;
[Microsoft documents the QoS request](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-setprocessinformation).
Linux priority readback, host thermal state and host NIC RSSI are explicitly
unavailable in these reports. DSCP is not requested; Android socket traffic-class
readback is reported. AP WMM, driver power state, channel congestion and hidden
radio queues remain unverified. Environmental coincidence is supporting evidence,
not automatic root-cause attribution.

## Numeric evidence schema

Every record is 12 integers: `kind, local_ns, session, sequence, a,b,c,d,e,f,g,h`.
The joiner normalizes Android's signed 64-bit session IDs. All timestamps preserve
their source clock. Host frame flags: gameplay=1, historical=2, heartbeat=4,
session start=8. No touch coordinates, keys, credentials, SSID/BSSID or addresses
are stored in these records.

| Kind | Owner | Payload fields |
|---|---|---|
| 1 / 13 | Host unique / logical duplicate | event, callback, winning send, echoed host send, phone control receive, sample flags, action, contact count |
| 2 / 8 | Host accepted / sink failed | ready time, retry count, numeric OS error (−1 if unavailable) |
| 3 | Host ordering hole | expected sequence, distance |
| 4 | Host rejected/abandoned | rejected frame evidence; use original kind-1 record for receive timing |
| 5 / 10 | Host boundary / connection begin | failed flag on boundary |
| 6 | Host ACK submission | duration, advanced flag, failed flag, submission start |
| 9 | Host ingress discard | reason: 1 peer/interface, 2 AEAD replay, 3 tag, 4 header/connection |
| 11 / 12 | Host cancellation / watchdog | release duration+error / watchdog reason |
| 20 | Android queued | event, callback, sample flags, pending depth, window |
| 21 | Android actual attempt | encryption start, queued time, attempt ordinal, failed flag, socket return, repair-deadline lateness, selection time, wire flags |
| 22 | Android ACK decision | progress (0 repeat, 1 advance, 2 invalid future), previous progress time, depth, oldest queue time, window, current host control send |
| 23 / 24 | Android retired / discarded | queue, event, callback, sample flags, first selection, total selections, pending depth, cumulative ACK |
| 25 | Android not queued | event, historical flag, reason (1 inactive/session boundary, 2 overflow), optional depth/window; sequence unassigned |
| 26 | Android boundary | failed flag, pending count, failure class (0 unspecified, 1 socket timeout, 2 IO, 3 other) |
| 27 | Android environment | thermal severity, power save, interactive, foreground, held Wi-Fi lock, selected RSSI, frequency, elapsed-realtime minus monotonic clock |
| 28 / 29 | Android control discard / watchdog | category+count / reason (1 ACK-progress timeout, 2 backlog age, 3 overflow, 4 idle expiry), last progress, depth, oldest queue, window |

## Validation

Production tests cover admission/rejection, duplicate/heartbeat cadence isolation,
historical and failed outcomes, asymmetric forward stalls, recovery after long
reconnects, sink failures and partial submission, ACK loss, corruption/reorder,
strict threshold boundaries, multi-second tails, bounded continuous incidents,
SPSC concurrency/wrap/saturation and zero native gameplay allocations. Android
unit tests exercise the actual repair selector and sender recorder. The offline
join tests cover signed identifiers, winning-repair matching, ACK/discard
ambiguity and missing evidence. [Latency validation](LATENCY_VALIDATION.md)
records the measured environment and outstanding physical tests.

Before a hardware release, perform both phone/cable/PC and phone/router/PC soaks:
include stationary holds, rapid same/different-finger taps, historical bursts,
slides/chords, sustained congestion, directional packet/ACK loss, reconnect,
screen/foreground changes, battery saver and thermal throttling. Collect both
reports and independently observe the OS/game. Do not turn loopback timing or a
held lock into a universal physical-latency claim.
