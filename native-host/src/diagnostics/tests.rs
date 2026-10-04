use super::*;

fn receive(at: u64, sequence: u64, event: u64, callback: u64, send: u64, flags: u64) -> Event {
    Event([
        RECEIVE, at, 7, sequence, event, callback, send, 0, 0, flags, 0, 1,
    ])
}
fn accept(at: u64, sequence: u64, ready: u64) -> Event {
    Event([ACCEPT, at, 7, sequence, ready, 0, 0, 0, 0, 0, 0, 0])
}

#[test]
fn duplicates_heartbeats_and_rests_do_not_make_gameplay_cadence_healthy_or_bad() {
    let mut a = Analysis::new(8_333_333);
    a.observe(receive(1_000_000, 1, 100, 110, 120, 1));
    a.observe(accept(1_010_000, 1, 1_001_000));
    for seq in 2..100 {
        a.observe(Event([
            DUPLICATE,
            seq * 100_000,
            7,
            1,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ]));
        a.observe(receive(
            seq * 100_000,
            seq,
            seq * 100_000,
            seq * 100_000,
            seq * 100_000,
            4,
        ));
        a.observe(accept(seq * 100_000 + 1, seq, seq * 100_000));
    }
    a.observe(receive(30_000_000, 100, 1_000_100, 1_000_110, 1_000_120, 1));
    assert_eq!(a.series["unique_gameplay_receive_interval_ns"].n, 1);
    assert_eq!(
        a.series["unique_gameplay_cadence_excess_ns"].max_ns,
        28_000_000
    );
    // One second rest with one second event spacing is not an input stall.
    a.observe(receive(
        1_030_000_000,
        101,
        1_001_000_100,
        1_001_000_110,
        1_001_000_120,
        1,
    ));
    assert_eq!(a.series["unique_gameplay_cadence_excess_ns"].over_budget, 1);
}

#[test]
fn historical_failure_and_reset_cannot_disappear_into_accepted_latency() {
    let mut a = Analysis::new(8_333_333);
    a.observe(receive(30_000_000, 1, 1, 20_000_001, 21_000_001, 3));
    a.observe(accept(30_010_000, 1, 30_001_000));
    a.observe(receive(
        31_000_000, 2, 30_000_000, 30_010_000, 30_020_000, 1,
    ));
    a.observe(Event([
        SINK_FAILURE,
        40_000_000,
        7,
        2,
        31_001_000,
        32,
        123,
        0,
        0,
        0,
        0,
        0,
    ]));
    a.observe(Event([BOUNDARY, 40_000_001, 7, 2, 1, 0, 0, 0, 0, 0, 0, 0]));
    assert_eq!(
        a.series["historical_event_to_sink_local_lower_bound_ns"].max_ns,
        19_989_000
    );
    assert_eq!(a.counts["os_accepted"], 1);
    assert_eq!(a.counts["os_failed"], 1);
    assert_eq!(a.counts["gameplay_uncommitted_at_boundary"], 1);
    assert_eq!(a.incidents.len(), 1);
    assert!(a.incidents[0].causes.contains("os_sink_failure"));
}

#[test]
fn reorder_cluster_retains_pre_post_and_measures_hole_recovery() {
    let mut a = Analysis::new(8_333_333);
    a.observe(receive(1_000_000, 1, 1, 2, 3, 1));
    a.observe(accept(1_010_000, 1, 1_001_000));
    for sequence in [3, 4] {
        a.observe(Event([
            GAP,
            sequence * 1_000_000,
            7,
            sequence,
            2,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ]));
        a.observe(receive(
            sequence * 1_000_000,
            sequence,
            sequence,
            sequence,
            sequence,
            1,
        ));
    }
    a.observe(receive(5_000_000, 2, 2, 2, 2, 1));
    a.observe(accept(5_010_000, 2, 5_001_000));
    a.observe(accept(5_020_000, 3, 5_011_000));
    a.observe(accept(5_030_000, 4, 5_021_000));
    a.observe(Event([BEGIN, 50_000_000, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
    assert_eq!(a.counts["ordering_holes"], 1);
    assert_eq!(a.series["ordering_hole_until_commit_ns"].max_ns, 2_030_000);
    assert_eq!(a.incidents.len(), 1);
    assert_eq!(a.incidents[0].evidence[0].0[3], 1);
    assert!(a.incidents[0].post_context_complete);
}

#[test]
fn asymmetric_forward_stall_is_not_hidden_by_half_rtt() {
    let mut a = Analysis::new(8_333_333);
    // phone clock is host +1 second. Calibrate with 100us each direction.
    a.observe(Event([
        RECEIVE,
        1_300_000,
        7,
        1,
        1_001_100_000,
        1_001_100_000,
        1_001_200_000,
        1_000_000,
        1_001_100_000,
        1,
        0,
        1,
    ]));
    a.observe(accept(1_310_000, 1, 1_301_000));
    // 12ms forward, 100us reverse: RTT/2 would wrongly pass 8.333ms.
    a.observe(Event([
        RECEIVE,
        14_400_000,
        7,
        2,
        1_002_200_000,
        1_002_200_000,
        1_002_400_000,
        2_000_000,
        1_002_100_000,
        1,
        0,
        1,
    ]));
    a.observe(accept(14_410_000, 2, 14_401_000));
    assert!(a.series["forward_transit_lower_bound_ns"].max_ns > 11_000_000);
    assert!(a.incidents.iter().any(|i| {
        i.causes
            .contains("forward_transport_or_endpoint_stall_bounded")
    }));
}

#[test]
fn histograms_have_exact_thresholds_zero_full_range_and_enclosing_quantiles() {
    let mut s = Series::default();
    for ns in [0, 8_333_332, 8_333_333, 8_333_334, 900_000_000, u64::MAX] {
        s.push(ns, 8_333_333);
    }
    assert_eq!(s.over_budget, 3);
    assert_eq!(s.quantile(1, 6), Some([0, 0]));
    let bounds = s.quantile(5, 6).unwrap();
    assert!(bounds[0] <= 900_000_000 && bounds[1] >= 900_000_000);
    assert_eq!(s.quantile(6, 6).unwrap()[1], u64::MAX);
}

#[test]
fn retention_and_unknown_coverage_are_explicit_and_bounded() {
    let mut a = Analysis::new(8_333_333);
    for n in 1..1000 {
        a.observe(Event([
            INGRESS,
            n * 100_000_000,
            0,
            0,
            3,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ]));
    }
    a.finish(42);
    assert_eq!(a.incidents.len(), MAX_INCIDENTS);
    assert!(a.incidents_omitted > 0);
    assert_eq!(a.diagnostic_records_dropped, 42);
    assert!(a.incidents.iter().all(|i| i.evidence.len() <= MAX_EVIDENCE));
}

#[test]
fn slow_reconnect_remains_linked_to_failed_incident_and_snapshot() {
    let mut a = Analysis::new(8_333_333);
    a.observe(receive(1_000_000, 1, 1, 2, 3, 1));
    a.observe(Event([BOUNDARY, 32_000_000, 7, 1, 1, 0, 0, 0, 0, 0, 0, 0]));
    a.observe(Event([BEGIN, 1_000_000_000, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
    let mut start = receive(1_001_000_000, 0, 1, 2, 3, 8);
    start.0[2] = 8;
    a.observe(start);
    let mut committed = accept(1_001_010_000, 0, 1_001_001_000);
    committed.0[2] = 8;
    a.observe(committed);
    let mut snapshot = receive(1_009_000_000, 1, 1, 2, 3, 4);
    snapshot.0[2] = 8;
    a.observe(snapshot);
    let mut committed = accept(1_009_010_000, 1, 1_009_001_000);
    committed.0[2] = 8;
    a.observe(committed);
    assert_eq!(a.incidents[0].recovery_ns, Some(969_010_000));
    assert_eq!(a.incidents[0].fresh_session_commit_ns, Some(1_001_010_000));
    assert_eq!(
        a.incidents[0].reconstructed_snapshot_ns,
        Some(1_009_010_000)
    );
    assert!(
        a.incidents[0]
            .evidence
            .iter()
            .any(|e| e.0[0] == ACCEPT && e.0[2] == 8 && e.0[3] == 1)
    );
}

#[test]
fn rejected_far_future_frame_does_not_replace_an_observed_pending_frame() {
    let mut a = Analysis::new(8_333_333);
    a.observe(receive(1_000_000, 1, 1, 2, 3, 1));
    let mut rejected = receive(
        2_000_000,
        1 + crate::protocol::MAX_REORDERED_FRAMES as u64,
        1,
        2,
        3,
        1,
    );
    rejected.0[0] = REJECT;
    a.observe(rejected);
    a.observe(accept(3_000_000, 1, 2_001_000));
    assert_eq!(a.counts["gameplay_os_accepted"], 1);
    assert!(!a.counts.contains_key("outcome_without_receive_evidence"));
}

#[test]
fn reverse_stall_and_repeated_echo_have_distinct_metrics() {
    let mut a = Analysis::new(8_333_333);
    a.observe(Event([
        RECEIVE,
        1_300_000,
        7,
        1,
        1_001_100_000,
        1_001_100_000,
        1_001_200_000,
        1_000_000,
        1_001_100_000,
        1,
        0,
        1,
    ]));
    a.observe(accept(1_310_000, 1, 1_301_000));
    a.observe(Event([
        RECEIVE,
        14_400_000,
        7,
        2,
        1_014_200_000,
        1_014_200_000,
        1_014_300_000,
        2_000_000,
        1_014_100_000,
        4,
        0,
        1,
    ]));
    a.observe(accept(14_410_000, 2, 14_401_000));
    a.observe(Event([
        RECEIVE,
        15_400_000,
        7,
        3,
        1_015_200_000,
        1_015_200_000,
        1_015_300_000,
        2_000_000,
        1_014_100_000,
        4,
        0,
        1,
    ]));
    assert_eq!(a.series["forward_transit_lower_bound_ns"].n, 1);
    assert_eq!(a.series["control_frame_forward_lower_ns"].n, 2);
    assert_eq!(a.series["reverse_control_lower_bound_ns"].n, 2);
    assert!(a.series["reverse_control_lower_bound_ns"].max_ns > 11_000_000);
    assert_eq!(
        a.counts["reused_control_echo_excluded_from_reverse_distribution"],
        1
    );
}
