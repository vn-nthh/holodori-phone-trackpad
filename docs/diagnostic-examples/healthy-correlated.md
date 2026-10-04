# Doritrack correlated diagnostics

Clustered incidents: 0. Retained frame records: 32.

## Session health

| Source | Unique/queued | OS accepted/ACK retired | Failed/discarded | Duplicates/repairs | Diagnostic records lost |
|---|---:|---:|---:|---:|---:|
| Host (synthetic-wifi) | 1000 | 1000 | 0 failed; 0 abandoned | 1000 duplicates | 0 |
| Android | 1000 | 1000 | 0 discarded; 0 not queued | 0 repairs | 0 |

- Counts/histograms are whole-run observations; retained frames are bounded forensic context, not the population.
- No missing event proves packet loss. Distinguish absent evidence, sender discard and observed sink failure.
- Phone and host timestamps retain independent clock origins. Match session/sequence and winning-attempt timestamp; do not subtract origins.
- Transport bounds depend on stated drift and timestamp precision assumptions; environmental observations do not prove a root cause.
- OS acceptance does not establish game recognition or physical input-to-display latency.
