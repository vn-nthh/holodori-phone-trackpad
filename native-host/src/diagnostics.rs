//! Diagnostic analysis runs on a worker, never on the input thread.
//! Numeric event schema is also used by the Android recorder and offline joiner.
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::Serialize;

#[cfg(test)]
mod tests;

pub const RECEIVE: u64 = 1;
pub const ACCEPT: u64 = 2;
pub const GAP: u64 = 3;
pub const REJECT: u64 = 4;
pub const BOUNDARY: u64 = 5;
pub const ACK: u64 = 6;
pub const SINK_FAILURE: u64 = 8;
pub const INGRESS: u64 = 9;
pub const BEGIN: u64 = 10;
pub const CANCEL: u64 = 11;
pub const STALL: u64 = 12;
pub const DUPLICATE: u64 = 13;
pub const CONTEXT: usize = 128;
pub const MAX_INCIDENTS: usize = 256;
pub const MAX_EVIDENCE: usize = 512;
pub const QUIET_NS: u64 = 32_000_000;

/// kind, local ns, session, sequence, eight kind-specific numeric fields.
/// Contains no coordinates, credentials, IPs, SSIDs, or input key values.
#[derive(Clone, Copy, Default, Debug, Serialize)]
pub struct Event(pub [u64; 12]);

#[derive(Serialize)]
pub struct Series {
    pub n: u64,
    pub invalid: u64,
    pub over_budget: u64,
    pub max_ns: u64,
    pub sum_ns: u128,
    #[serde(serialize_with = "sparse_bins")]
    pub bins: Vec<u64>,
}

fn sparse_bins<S: serde::Serializer>(bins: &[u64], serializer: S) -> Result<S::Ok, S::Error> {
    let occupied: BTreeMap<usize, u64> = bins
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, n)| *n != 0)
        .collect();
    occupied.serialize(serializer)
}

impl Default for Series {
    fn default() -> Self {
        Self {
            n: 0,
            invalid: 0,
            over_budget: 0,
            max_ns: 0,
            sum_ns: 0,
            bins: vec![0; 2049],
        }
    }
}

impl Series {
    pub fn push(&mut self, ns: u64, budget: u64) {
        // 32 subdivisions per power of two, exact integer low range and zero.
        let exponent = 63 - ns.max(1).leading_zeros() as usize;
        let shift = exponent.saturating_sub(5);
        let index = if ns < 32 {
            ns as usize
        } else {
            (exponent - 4) * 32 + ((ns >> shift) as usize - 32)
        };
        self.bins[index] += 1;
        self.n += 1;
        self.sum_ns += ns as u128;
        self.max_ns = self.max_ns.max(ns);
        self.over_budget += u64::from(ns > budget);
    }

    /// Quantile is an interval, never a falsely precise clipped scalar.
    pub fn quantile(&self, numerator: u64, denominator: u64) -> Option<[u64; 2]> {
        if self.n == 0 {
            return None;
        }
        let rank = (self.n as u128 * numerator as u128).div_ceil(denominator as u128);
        let mut count = 0_u128;
        for (i, n) in self.bins.iter().enumerate() {
            count += *n as u128;
            if count >= rank {
                let (low, high) = if i < 32 {
                    (i as u64, i as u64)
                } else {
                    let exponent = i / 32 + 4;
                    let shift = exponent - 5;
                    let low = ((32 + i % 32) as u64) << shift;
                    (low, low.saturating_add((1_u64 << shift) - 1))
                };
                return Some([low.min(self.max_ns), high.min(self.max_ns)]);
            }
        }
        None
    }
}

#[derive(Serialize)]
pub struct Incident {
    pub id: usize,
    pub started_ns: u64,
    pub last_anomaly_ns: u64,
    pub last_evidence_ns: u64,
    pub recovery_ns: Option<u64>,
    pub fresh_session_commit_ns: Option<u64>,
    pub reconstructed_snapshot_ns: Option<u64>,
    pub causes: BTreeSet<&'static str>,
    pub affected_frames: u64,
    pub affected_frame_keys: BTreeSet<[u64; 2]>,
    pub affected_keys_omitted: u64,
    pub first_frame: [u64; 2],
    pub last_frame: [u64; 2],
    pub evidence_omitted: u64,
    pub pre_context_truncated: bool,
    pub post_context_complete: bool,
    pub evidence: Vec<Event>,
}

#[derive(Serialize)]
pub struct Analysis {
    pub schema: u8,
    pub budget_ns: u64,
    pub counts: BTreeMap<&'static str, u64>,
    pub series: BTreeMap<&'static str, Series>,
    pub incidents: Vec<Incident>,
    pub incidents_omitted: u64,
    pub diagnostic_records_dropped: u64,
    pub diagnostic_drop_span_ns: Option<[u64; 2]>,
    pub last_event_ns: u64,
    #[serde(skip)]
    pending: Vec<Option<Event>>,
    #[serde(skip)]
    forward_bounds: Vec<Option<[u64; 2]>>,
    #[serde(skip)]
    clock: Option<(u64, u64, i128, i128)>,
    #[serde(skip)]
    history: VecDeque<Event>,
    #[serde(skip)]
    active: Option<usize>,
    #[serde(skip)]
    gap: Option<(u64, u64, u64, u64)>,
    #[serde(skip)]
    last_unique: Option<Event>,
    #[serde(skip)]
    last_commit: Option<Event>,
    #[serde(skip)]
    last_bad_frame: Option<[u64; 2]>,
    #[serde(skip)]
    recovering: Option<usize>,
    #[serde(skip)]
    awaiting_snapshot: Option<usize>,
    #[serde(skip)]
    last_reverse_echo: Option<(u64, u64)>,
}

impl Analysis {
    pub fn new(budget_ns: u64) -> Self {
        Self {
            schema: 2,
            budget_ns,
            counts: BTreeMap::new(),
            series: BTreeMap::new(),
            incidents: Vec::with_capacity(MAX_INCIDENTS),
            incidents_omitted: 0,
            diagnostic_records_dropped: 0,
            diagnostic_drop_span_ns: None,
            last_event_ns: 0,
            pending: vec![None; crate::protocol::MAX_REORDERED_FRAMES],
            forward_bounds: vec![None; crate::protocol::MAX_REORDERED_FRAMES],
            clock: None,
            history: VecDeque::with_capacity(CONTEXT),
            active: None,
            gap: None,
            last_unique: None,
            last_commit: None,
            last_bad_frame: None,
            recovering: None,
            awaiting_snapshot: None,
            last_reverse_echo: None,
        }
    }

    fn count(&mut self, key: &'static str, n: u64) {
        *self.counts.entry(key).or_default() += n;
    }
    fn sample(&mut self, key: &'static str, ns: u64) {
        self.series.entry(key).or_default().push(ns, self.budget_ns);
    }
    fn delta(&mut self, key: &'static str, later: u64, earlier: u64) -> Option<u64> {
        if earlier == 0 || later < earlier {
            self.series.entry(key).or_default().invalid += 1;
            return None;
        }
        let ns = later - earlier;
        self.sample(key, ns);
        Some(ns)
    }

    fn transport_bounds(&mut self, e: Event) -> Option<[u64; 2]> {
        let d = e.0;
        if d[7] == 0 || d[8] == 0 || d[6] < d[8] || d[1] < d[7] {
            self.count("clock_exchange_missing_or_invalid", 1);
            return None;
        }
        let span = d[1] - d[7];
        if span > 2_000_000_000 {
            self.count("clock_exchange_stale", 1);
            return None;
        }
        let uncertainty = (span / 1000 + 1) as i128; // explicit 1000 ppm assumption
        let mut low = d[7] as i128 - d[8] as i128 - uncertainty;
        let mut high = d[1] as i128 - d[6] as i128 + uncertainty;
        if low > high {
            self.count("clock_exchange_invalid", 1);
            return None;
        }
        self.sample("duplex_exchange_residual_upper_ns", (high - low) as u64);
        if let Some((session, at, old_low, old_high)) = self.clock
            && session == d[2]
            && d[1].saturating_sub(at) <= 1_000_000_000
        {
            let drift = (d[1].saturating_sub(at) / 1000 + 1) as i128;
            let intersection = (low.max(old_low - drift), high.min(old_high + drift));
            if intersection.0 <= intersection.1 {
                (low, high) = intersection;
            } else {
                self.count("clock_model_resets", 1);
                self.anomaly(e, "clock_rate_assumption_or_timestamps_inconsistent");
                self.clock = None;
                return None;
            }
        }
        self.clock = Some((d[2], d[1], low, high));
        let observed = d[1] as i128 - d[6] as i128;
        let forward = [
            (observed - high).max(0) as u64,
            (observed - low).max(0) as u64,
        ];
        let gameplay = d[9] & 1 != 0;
        self.sample(
            if gameplay {
                "forward_transit_lower_bound_ns"
            } else {
                "control_frame_forward_lower_ns"
            },
            forward[0],
        );
        self.sample(
            if gameplay {
                "forward_transit_upper_bound_ns"
            } else {
                "control_frame_forward_upper_ns"
            },
            forward[1],
        );
        self.sample("clock_offset_interval_width_ns", (high - low) as u64);
        let reverse = d[8] as i128 - d[7] as i128;
        if self
            .last_reverse_echo
            .is_none_or(|(session, echo)| session != d[2] || d[7] > echo)
        {
            self.last_reverse_echo = Some((d[2], d[7]));
            self.sample(
                "reverse_control_lower_bound_ns",
                (reverse + low).max(0) as u64,
            );
            self.sample(
                "reverse_control_upper_bound_ns",
                (reverse + high).max(0) as u64,
            );
        } else {
            self.count("reused_control_echo_excluded_from_reverse_distribution", 1);
        }
        if forward[0] > self.budget_ns {
            self.anomaly(e, "forward_transport_or_endpoint_stall_bounded");
        } else if forward[1] > self.budget_ns {
            self.anomaly(e, "forward_latency_budget_uncertain");
        }
        if (reverse + low).max(0) as u64 > self.budget_ns {
            self.anomaly(e, "reverse_control_or_endpoint_stall_bounded");
        }
        Some(forward)
    }

    fn anomaly(&mut self, e: Event, cause: &'static str) {
        let d = e.0;
        if self.active.is_none() {
            if self.incidents.len() == MAX_INCIDENTS {
                self.incidents_omitted += 1;
                return;
            }
            self.incidents.push(Incident {
                id: self.incidents.len() + 1,
                started_ns: d[1],
                last_anomaly_ns: d[1],
                last_evidence_ns: d[1],
                recovery_ns: None,
                fresh_session_commit_ns: None,
                reconstructed_snapshot_ns: None,
                causes: BTreeSet::new(),
                affected_frames: 0,
                affected_frame_keys: BTreeSet::new(),
                affected_keys_omitted: 0,
                first_frame: [d[2], d[3]],
                last_frame: [d[2], d[3]],
                evidence_omitted: 0,
                pre_context_truncated: self.history.len() == CONTEXT,
                post_context_complete: false,
                evidence: self.history.iter().copied().collect(),
            });
            self.active = Some(self.incidents.len() - 1);
        }
        let incident = &mut self.incidents[self.active.unwrap()];
        incident.last_anomaly_ns = d[1];
        incident.recovery_ns = None;
        incident.causes.insert(cause);
        if self.last_bad_frame != Some([d[2], d[3]]) {
            incident.affected_frames += 1;
            self.last_bad_frame = Some([d[2], d[3]]);
        }
        incident.last_frame = [d[2], d[3]];
        if d[2] != 0 && d[3] != u64::MAX {
            if incident.affected_frame_keys.len() < MAX_EVIDENCE {
                incident.affected_frame_keys.insert([d[2], d[3]]);
            } else if !incident.affected_frame_keys.contains(&[d[2], d[3]]) {
                incident.affected_keys_omitted += 1;
            }
        }
    }

    fn discard_pending(&mut self, e: Event) {
        for i in 0..self.pending.len() {
            if let Some(frame) = self.pending[i].take() {
                self.count("observed_uncommitted_at_boundary", 1);
                if frame.0[9] & 1 != 0 {
                    self.count("gameplay_uncommitted_at_boundary", 1);
                }
                let mut discarded = frame;
                discarded.0[0] = REJECT;
                discarded.0[1] = e.0[1];
                self.anomaly(discarded, "protocol_observed_frame_abandoned");
                self.retain(discarded);
            }
        }
    }

    fn retain(&mut self, e: Event) {
        if let Some(index) = self.active {
            let incident = &mut self.incidents[index];
            incident.last_evidence_ns = e.0[1];
            if incident.evidence.len() < MAX_EVIDENCE {
                incident.evidence.push(e);
            } else {
                incident.evidence_omitted += 1;
            }
        }
    }

    pub fn observe(&mut self, e: Event) {
        let d = e.0;
        let recovery_target = self.recovering.or(self.awaiting_snapshot);
        self.last_event_ns = self.last_event_ns.max(d[1]);
        if let Some(index) = self.active {
            let incident = &mut self.incidents[index];
            if self.gap.is_none() && d[1].saturating_sub(incident.last_anomaly_ns) > QUIET_NS {
                incident.post_context_complete = true;
                self.active = None;
                self.last_bad_frame = None;
            }
        }
        match d[0] {
            BEGIN => {
                self.discard_pending(e);
                self.count("connections", 1);
                self.last_unique = None;
                self.last_commit = None;
                self.gap = None;
                self.clock = None;
            }
            RECEIVE => {
                self.count("unique_logical_frames_observed", 1);
                let gameplay = d[9] & 1 != 0;
                if d[9] & 4 != 0 {
                    self.count("unique_heartbeats", 1);
                }
                if gameplay {
                    if d[4] == 0 || d[5] < d[4] || d[6] < d[5] {
                        self.anomaly(e, "android_timestamp_order_invalid");
                    }
                    self.count(
                        if d[9] & 2 != 0 {
                            "historical_frames_observed"
                        } else {
                            "current_frames_observed"
                        },
                        1,
                    );
                    let input_key = if d[9] & 2 != 0 {
                        "android_historical_dispatch_ns"
                    } else {
                        "android_current_dispatch_ns"
                    };
                    if self
                        .delta(input_key, d[5], d[4])
                        .is_some_and(|v| v > self.budget_ns)
                    {
                        self.anomaly(e, "android_dispatch_budget");
                    }
                    if self
                        .delta("android_callback_to_winning_attempt_ns", d[6], d[5])
                        .is_some_and(|v| v > self.budget_ns)
                    {
                        self.anomaly(e, "android_queue_or_repair_age_budget");
                    }
                    if let Some(previous) = self.last_unique.filter(|p| p.0[2] == d[2]) {
                        let receive_gap = d[1].saturating_sub(previous.0[1]);
                        self.sample("unique_gameplay_receive_interval_ns", receive_gap);
                        // Sparse taps/rests are not stalls. Compare to sender event spacing,
                        // and only use chronological samples from the same session.
                        if d[4] >= previous.0[4] {
                            let event_gap = d[4] - previous.0[4];
                            let excess = receive_gap.saturating_sub(event_gap);
                            self.sample("unique_gameplay_cadence_excess_ns", excess);
                            if excess > self.budget_ns {
                                self.anomaly(e, "delivery_cadence_excess_origin_unresolved");
                            }
                        }
                    }
                    self.last_unique = Some(e);
                }
                let forward = self.transport_bounds(e);
                let slot = d[3] as usize % self.pending.len();
                if self.pending[slot].is_some() {
                    self.count("diagnostic_pending_collision", 1);
                    self.anomaly(e, "diagnostic_coverage_gap");
                }
                self.pending[slot] = Some(e);
                self.forward_bounds[slot] = forward;
            }
            DUPLICATE => {
                self.count("logical_duplicate_datagrams", 1);
            }
            GAP => {
                self.count("future_unique_frames", 1);
                if self.gap.is_none_or(|g| (g.0, g.1) != (d[2], d[4])) {
                    self.count("ordering_holes", 1);
                    self.gap = Some((d[2], d[4], d[1], d[3]));
                } else if let Some(gap) = &mut self.gap {
                    gap.3 = gap.3.max(d[3]);
                }
                self.anomaly(e, "protocol_ordering_hole_loss_or_reorder");
            }
            ACCEPT | SINK_FAILURE => {
                let slot = d[3] as usize % self.pending.len();
                let accepted_flags = self.pending[slot]
                    .filter(|p| p.0[2..4] == d[2..4])
                    .map(|p| p.0[9]);
                self.count(
                    if d[0] == ACCEPT {
                        "os_accepted"
                    } else {
                        "os_failed"
                    },
                    1,
                );
                self.count("sink_retries", d[5]);
                if d[5] > 0 {
                    self.anomaly(e, "os_sink_retry");
                }
                if d[0] == SINK_FAILURE {
                    self.anomaly(e, "os_sink_failure");
                }
                if self
                    .delta("sink_attempts_including_planning_ns", d[1], d[4])
                    .is_some_and(|v| v > self.budget_ns)
                {
                    self.anomaly(e, "sink_submission_budget");
                }
                let slot = d[3] as usize % self.pending.len();
                if let Some(frame) = self.pending[slot].filter(|p| p.0[2..4] == d[2..4]) {
                    let f = frame.0;
                    self.delta("host_receive_to_ready_ns", d[4], f[1]);
                    let service = d[1].saturating_sub(f[1]);
                    self.sample("host_receive_to_sink_outcome_ns", service);
                    if d[4].saturating_sub(f[1]) > self.budget_ns {
                        self.anomaly(e, "host_decode_order_or_scheduling_budget");
                    }
                    if d[0] == ACCEPT {
                        self.pending[slot] = None;
                        if f[9] & 1 != 0 {
                            self.count("gameplay_os_accepted", 1);
                            // No cross-clock subtraction: local components are a lower
                            // bound; unknown forward leg is not quietly filled with RTT/2.
                            if f[4] > 0 && f[5] >= f[4] && f[6] >= f[5] {
                                let phone_age = f[6] - f[4];
                                let timestamp_total = phone_age.saturating_add(service);
                                // V5 carries no Android API/resolution field. Use the
                                // conservative pre-34 1ms event quantization allowance,
                                // plus the same documented phone clock-rate assumption.
                                let lower =
                                    timestamp_total.saturating_sub(1_000_000 + phone_age / 1000);
                                let key = if f[9] & 2 != 0 {
                                    "historical_event_to_sink_local_lower_bound_ns"
                                } else {
                                    "current_event_to_sink_local_lower_bound_ns"
                                };
                                self.sample(key, lower);
                                if lower > self.budget_ns {
                                    self.anomaly(e, "event_to_sink_definite_budget_violation");
                                }
                                if let Some(bounds) = self.forward_bounds[slot] {
                                    let historical = f[9] & 2 != 0;
                                    let low = lower.saturating_add(bounds[0]);
                                    let high = timestamp_total
                                        .saturating_add(phone_age / 1000)
                                        .saturating_add(bounds[1]);
                                    self.sample(
                                        if historical {
                                            "historical_event_to_sink_lower_ns"
                                        } else {
                                            "current_event_to_sink_lower_ns"
                                        },
                                        low,
                                    );
                                    self.sample(
                                        if historical {
                                            "historical_event_to_sink_upper_ns"
                                        } else {
                                            "current_event_to_sink_upper_ns"
                                        },
                                        high,
                                    );
                                    if low > self.budget_ns {
                                        self.count("gameplay_budget_definite_fail", 1);
                                        self.anomaly(e, "bounded_event_to_sink_budget_violation");
                                    } else if high > self.budget_ns {
                                        self.count("gameplay_budget_uncertain", 1);
                                        self.anomaly(e, "event_to_sink_budget_uncertain");
                                    } else {
                                        self.count(
                                            "gameplay_within_budget_under_clock_assumption",
                                            1,
                                        );
                                    }
                                } else {
                                    self.count("gameplay_budget_unmeasurable", 1);
                                }
                            } else {
                                self.count("gameplay_budget_unmeasurable", 1);
                            }
                        }
                    }
                } else {
                    self.count("outcome_without_receive_evidence", 1);
                }
                if d[0] == ACCEPT {
                    if let Some(index) = self.recovering.take() {
                        let incident = &mut self.incidents[index];
                        incident.fresh_session_commit_ns = Some(d[1]);
                        incident.recovery_ns = Some(d[1].saturating_sub(incident.started_ns));
                        self.awaiting_snapshot = Some(index);
                    }
                    if accepted_flags.is_some_and(|flags| flags & (1 | 4) != 0)
                        && let Some(index) = self.awaiting_snapshot.take()
                    {
                        self.incidents[index].reconstructed_snapshot_ns = Some(d[1]);
                    }
                    if let Some((session, _expected, start, through)) = self.gap
                        && session == d[2]
                        && d[3] >= through
                    {
                        self.sample("ordering_hole_until_commit_ns", d[1].saturating_sub(start));
                        self.gap = None;
                    }
                    if self.gap.is_none()
                        && self.pending.iter().all(Option::is_none)
                        && let Some(index) = self.active
                    {
                        let incident = &mut self.incidents[index];
                        incident
                            .recovery_ns
                            .get_or_insert(d[1].saturating_sub(incident.started_ns));
                    }
                    self.last_commit = Some(e);
                }
            }
            REJECT => {
                self.count("protocol_rejected", 1);
                let slot = d[3] as usize % self.pending.len();
                if self.pending[slot].is_some_and(|p| p.0[2..4] == d[2..4]) {
                    self.pending[slot] = None;
                }
                self.anomaly(e, "protocol_frame_rejected");
            }
            BOUNDARY => {
                if d[4] != 0 {
                    self.count("failed_boundaries", 1);
                    self.anomaly(e, "session_failure_boundary");
                    self.recovering = self.active;
                }
                self.discard_pending(e);
                self.gap = None;
                self.last_unique = None;
            }
            ACK => {
                self.count(
                    if d[5] != 0 {
                        "ack_progress_submissions"
                    } else {
                        "ack_repeat_submissions"
                    },
                    1,
                );
                self.sample("host_ack_submission_ns", d[4]);
                if d[6] != 0 {
                    self.anomaly(e, "host_ack_send_failure");
                }
                if d[4] > self.budget_ns {
                    self.anomaly(e, "host_ack_submission_budget");
                }
            }
            INGRESS => {
                self.count(
                    match d[4] {
                        1 => "ingress_wrong_peer_or_interface",
                        2 => "aead_packet_replay",
                        3 => "bad_tag",
                        _ => "malformed_or_wrong_connection",
                    },
                    1,
                );
                self.anomaly(e, "ingress_discard_not_gameplay_loss_proof");
            }
            CANCEL => {
                self.count("release_attempts", 1);
                if d[5] != 0 {
                    self.anomaly(e, "os_release_failure");
                }
                if d[4] > self.budget_ns {
                    self.anomaly(e, "os_release_budget");
                }
            }
            STALL => {
                self.count("watchdog_or_interface_failures", 1);
                self.anomaly(e, "watchdog_or_interface_boundary");
            }
            _ => {}
        }
        // Recovery can take longer than the quiet clustering window. Preserve its
        // later authenticated boundary/snapshot in the original incident too.
        if let Some(index) = recovery_target
            && self.active != Some(index)
        {
            let incident = &mut self.incidents[index];
            if incident.evidence.len() < MAX_EVIDENCE {
                incident.evidence.push(e);
            } else {
                incident.evidence_omitted += 1;
            }
        }
        self.retain(e);
        if self.history.len() == CONTEXT {
            self.history.pop_front();
        }
        self.history.push_back(e);
    }

    pub fn finish(&mut self, dropped: u64) {
        self.diagnostic_records_dropped = dropped;
        let event = Event([BOUNDARY, self.last_event_ns, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        self.discard_pending(event);
        if dropped > 0 {
            self.anomaly(event, "diagnostic_coverage_gap");
        }
    }

    pub fn recent_context(&self) -> &VecDeque<Event> {
        &self.history
    }
}
