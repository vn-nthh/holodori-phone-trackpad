# Doritrack correlated diagnostics

Clustered incidents: 3. Retained frame records: 110.

## Session health

| Source | Unique/queued | OS accepted/ACK retired | Failed/discarded | Duplicates/repairs | Diagnostic records lost |
|---|---:|---:|---:|---:|---:|
| Host (synthetic-wifi) | 1003 | 1002 | 1 failed; 1 abandoned | 1000 duplicates | 0 |
| Android | 1002 | 1001 | 1 discarded; 0 not queued | 1 repairs | 0 |

- Counts/histograms are whole-run observations; retained frames are bounded forensic context, not the population.
- No missing event proves packet loss. Distinguish absent evidence, sender discard and observed sink failure.
- Phone and host timestamps retain independent clock origins. Match session/sequence and winning-attempt timestamp; do not subtract origins.
- Transport bounds depend on stated drift and timestamp precision assumptions; environmental observations do not prove a root cause.
- OS acceptance does not establish game recognition or physical input-to-display latency.

## Incident 1 — session 0000000000000007, sequences 500–501

V5 repair attempted (forward or ACK loss possible); bounded_event_to_sink_budget_violation; forward_transport_or_endpoint_stall_bounded; protocol_ordering_hole_loss_or_reorder; sender event-to-ACK budget (return path included)

- android: {"id": "degraded-android.txt:1", "at_phone_ns": 5001330000, "last_anomaly_phone_ns": 5012200000, "session": 7, "first_sequence": 500, "last_sequence": 500, "recovery_to_later_ack_ns": 10870000, "evidence_omitted": 0, "post_context_complete": true}
- host: {"id": "degraded-host.json:1", "at_host_ns": 4008000000, "last_anomaly_host_ns": 4012050000, "session": 7, "first_sequence": 501, "last_sequence": 500, "recovery_to_os_commit_ns": 4100000, "evidence_omitted": 0, "fresh_session_commit_ns": null, "reconstructed_snapshot_ns": null, "affected_frame_keys": [[7, 500], [7, 501]], "post_context_complete": true}

| Sequence | Outcome | Winning copy | Evidence |
|---|---|---|---|
| 500 | OS accepted | repair | first host delivery matched a repair attempt; sender completion tail includes ACK generation/return-path/sender scheduling; compare host outcome |
| 501 | OS accepted | first | host received a future unique frame behind an ordering hole |

## Incident 2 — session 0000000000000007, sequences 700–700

Android dispatch timestamp age; android_dispatch_budget; bounded_event_to_sink_budget_violation; event_to_sink_definite_budget_violation; sender event-to-ACK budget (return path included)

- android: {"id": "degraded-android.txt:2", "at_phone_ns": 6599220000, "last_anomaly_phone_ns": 6600200000, "session": 7, "first_sequence": 700, "last_sequence": 700, "recovery_to_later_ack_ns": 980000, "evidence_omitted": 0, "post_context_complete": true}
- host: {"id": "degraded-host.json:2", "at_host_ns": 5600000000, "last_anomaly_host_ns": 5600050000, "session": 7, "first_sequence": 700, "last_sequence": 700, "recovery_to_os_commit_ns": 50000, "evidence_omitted": 0, "fresh_session_commit_ns": null, "reconstructed_snapshot_ns": null, "affected_frame_keys": [[7, 700]], "post_context_complete": true}

| Sequence | Outcome | Winning copy | Evidence |
|---|---|---|---|
| 700 | OS accepted | first | Android callback received an already-aged current/historical sample |

## Incident 3 — session 0000000000000007, sequences 1001–1001

authenticated session failed; frame discarded/not queued or watchdog expired; os_sink_failure; os_sink_retry; protocol_observed_frame_abandoned; session_failure_boundary

- android: {"id": "degraded-android.txt:3", "at_phone_ns": 9072000000, "last_anomaly_phone_ns": 9072000001, "session": 7, "first_sequence": 1001, "last_sequence": 1001, "recovery_to_later_ack_ns": 16300000, "evidence_omitted": 0, "post_context_complete": false}
- host: {"id": "degraded-host.json:3", "at_host_ns": 8016000000, "last_anomaly_host_ns": 8016100000, "session": 7, "first_sequence": 1001, "last_sequence": 1001, "recovery_to_os_commit_ns": 72150000, "evidence_omitted": 0, "fresh_session_commit_ns": 8088150000, "reconstructed_snapshot_ns": 8096150000, "affected_frame_keys": [[7, 1001]], "post_context_complete": true}

| Sequence | Outcome | Winning copy | Evidence |
|---|---|---|---|
| 1001 | OS sink failed | unavailable | host OS sink failed: error=123, retries=300; host discarded/rejected an observed frame; sender abandoned frame; host outcome requires retained host evidence |
