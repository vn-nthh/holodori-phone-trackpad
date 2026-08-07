use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::protocol::{ACTION_HEARTBEAT, TouchFrame};

const HISTOGRAM_BIN_NANOS: f64 = 4_000.0;
const HISTOGRAM_BINS: usize = 131_072;
const NANOS_PER_MILLI: f64 = 1_000_000.0;

#[derive(Clone, Copy, Debug, Default)]
pub struct Snapshot {
    pub samples: u64,
    pub mean_ms: f64,
    pub max_ms: f64,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p99_ms: f64,
    pub p99_9_ms: f64,
}

struct SampleSeries {
    bins: Box<[u64]>,
    samples: u64,
    sum_nanos: f64,
    max_nanos: f64,
}

impl SampleSeries {
    fn new() -> Self {
        Self {
            bins: vec![0; HISTOGRAM_BINS].into_boxed_slice(),
            samples: 0,
            sum_nanos: 0.0,
            max_nanos: 0.0,
        }
    }

    fn push_duration(&mut self, duration: Duration) {
        self.push_nanos(duration.as_nanos() as f64);
    }

    fn push_nanos(&mut self, nanos: f64) {
        if !nanos.is_finite() || nanos < 0.0 {
            return;
        }
        let index = ((nanos / HISTOGRAM_BIN_NANOS) as usize).min(HISTOGRAM_BINS - 1);
        self.bins[index] += 1;
        self.samples += 1;
        self.sum_nanos += nanos;
        self.max_nanos = self.max_nanos.max(nanos);
    }

    fn snapshot(&self) -> Snapshot {
        if self.samples == 0 {
            return Snapshot::default();
        }
        Snapshot {
            samples: self.samples,
            mean_ms: self.sum_nanos / self.samples as f64 / NANOS_PER_MILLI,
            max_ms: self.max_nanos / NANOS_PER_MILLI,
            p50_ms: self.percentile_nanos(50.0) / NANOS_PER_MILLI,
            p90_ms: self.percentile_nanos(90.0) / NANOS_PER_MILLI,
            p99_ms: self.percentile_nanos(99.0) / NANOS_PER_MILLI,
            p99_9_ms: self.percentile_nanos(99.9) / NANOS_PER_MILLI,
        }
    }

    fn percentile_nanos(&self, percentile: f64) -> f64 {
        if self.samples == 0 {
            return 0.0;
        }
        let target =
            ((self.samples as f64 * percentile / 100.0).ceil() as u64).clamp(1, self.samples);
        let mut cumulative = 0_u64;
        for (index, count) in self.bins.iter().enumerate() {
            cumulative += count;
            if cumulative >= target {
                return (index + 1) as f64 * HISTOGRAM_BIN_NANOS;
            }
        }
        self.max_nanos
    }

    fn count_above_ms(&self, threshold_ms: f64) -> u64 {
        let threshold_nanos = threshold_ms * NANOS_PER_MILLI;
        let first_full_bin_above = (threshold_nanos / HISTOGRAM_BIN_NANOS).floor() as usize + 1;
        self.bins
            .iter()
            .skip(first_full_bin_above.min(HISTOGRAM_BINS))
            .sum()
    }
}

struct ArrivalObservation {
    arrival: Instant,
    input_dispatch_nanos: Option<f64>,
    callback_to_write_nanos: Option<f64>,
    transport_nanos: Option<f64>,
    current_touch: bool,
}

#[derive(Clone, Copy)]
struct WorstCurrentEvent {
    session_id: u64,
    sequence: u64,
    total_nanos: f64,
    input_dispatch_nanos: f64,
    callback_to_write_nanos: f64,
    transport_nanos: f64,
    service_nanos: f64,
}

pub struct HostMetrics {
    enabled: bool,
    started: Instant,
    warning_budget_ms: f64,
    input_current: SampleSeries,
    input_historical: SampleSeries,
    callback_to_write: SampleSeries,
    transport_one_way: SampleSeries,
    service: SampleSeries,
    end_to_end_current: SampleSeries,
    ack_write: SampleSeries,
    interarrival: SampleSeries,
    worst_current: Option<WorstCurrentEvent>,
    arrivals: HashMap<(u64, u64), ArrivalObservation>,
    last_arrival: Option<Instant>,
    active_gap: Option<(u64, u64)>,
    frames_received: u64,
    unique_frames: u64,
    accepted: u64,
    replay_frames: u64,
    recovery_events: u64,
    out_of_order_frames: u64,
    max_reorder_distance: u64,
    connections: u64,
    invalid_frames: u64,
    stream_discarded_bytes: u64,
    connection_discarded_bytes: u64,
    sink_retries: u64,
}

impl HostMetrics {
    pub fn new(enabled: bool, warning_budget_ms: f64) -> Self {
        Self {
            enabled,
            started: Instant::now(),
            warning_budget_ms,
            input_current: SampleSeries::new(),
            input_historical: SampleSeries::new(),
            callback_to_write: SampleSeries::new(),
            transport_one_way: SampleSeries::new(),
            service: SampleSeries::new(),
            end_to_end_current: SampleSeries::new(),
            ack_write: SampleSeries::new(),
            interarrival: SampleSeries::new(),
            worst_current: None,
            arrivals: HashMap::with_capacity(256),
            last_arrival: None,
            active_gap: None,
            frames_received: 0,
            unique_frames: 0,
            accepted: 0,
            replay_frames: 0,
            recovery_events: 0,
            out_of_order_frames: 0,
            max_reorder_distance: 0,
            connections: 0,
            invalid_frames: 0,
            stream_discarded_bytes: 0,
            connection_discarded_bytes: 0,
            sink_retries: 0,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn begin_connection(&mut self) {
        if !self.enabled {
            return;
        }
        self.connections += 1;
        self.arrivals.clear();
        self.last_arrival = None;
        self.active_gap = None;
    }

    pub fn clock_nanos(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.started)
            .as_nanos()
            .min(u128::from(u64::MAX)) as u64
    }

    pub fn observe_received(&mut self, frame: &TouchFrame, arrival: Instant, replay: bool) {
        if !self.enabled {
            return;
        }
        self.frames_received += 1;
        if let Some(previous) = self.last_arrival {
            self.interarrival
                .push_duration(arrival.saturating_duration_since(previous));
        }
        self.last_arrival = Some(arrival);
        if replay {
            self.replay_frames += 1;
            return;
        }

        self.unique_frames += 1;
        let gameplay = frame.action != ACTION_HEARTBEAT && !frame.session_start();
        let input_dispatch_nanos =
            ordered_delta(frame.phone_callback_nanos, frame.phone_event_nanos);
        let callback_to_write_nanos =
            ordered_delta(frame.phone_send_nanos, frame.phone_callback_nanos);
        let host_arrival_nanos = self.clock_nanos(arrival);
        let transport_nanos = estimate_one_way_nanos(
            frame.echo_host_send_nanos,
            frame.phone_control_receive_nanos,
            frame.phone_send_nanos,
            host_arrival_nanos,
        );

        if gameplay {
            if let Some(value) = input_dispatch_nanos {
                if frame.historical() {
                    self.input_historical.push_nanos(value);
                } else {
                    self.input_current.push_nanos(value);
                }
            }
            if let Some(value) = callback_to_write_nanos {
                self.callback_to_write.push_nanos(value);
            }
        }
        if let Some(value) = transport_nanos {
            self.transport_one_way.push_nanos(value);
        }

        self.arrivals
            .entry((frame.session_id, frame.sequence))
            .or_insert(ArrivalObservation {
                arrival,
                input_dispatch_nanos,
                callback_to_write_nanos,
                transport_nanos,
                current_touch: gameplay && !frame.historical(),
            });
    }

    pub fn observe_accepted(&mut self, frame: &TouchFrame, now: Instant) {
        if !self.enabled {
            return;
        }
        self.accepted += 1;
        let Some(observation) = self.arrivals.remove(&(frame.session_id, frame.sequence)) else {
            return;
        };
        let service_nanos = now
            .saturating_duration_since(observation.arrival)
            .as_nanos() as f64;
        self.service.push_nanos(service_nanos);
        if observation.current_touch
            && let (Some(input), Some(queue), Some(transport)) = (
                observation.input_dispatch_nanos,
                observation.callback_to_write_nanos,
                observation.transport_nanos,
            )
        {
            let total_nanos = input + queue + transport + service_nanos;
            self.end_to_end_current.push_nanos(total_nanos);
            if self
                .worst_current
                .is_none_or(|worst| total_nanos > worst.total_nanos)
            {
                self.worst_current = Some(WorstCurrentEvent {
                    session_id: frame.session_id,
                    sequence: frame.sequence,
                    total_nanos,
                    input_dispatch_nanos: input,
                    callback_to_write_nanos: queue,
                    transport_nanos: transport,
                    service_nanos,
                });
            }
        }
    }

    pub fn observe_ack_write(&mut self, elapsed: Duration) {
        if self.enabled {
            self.ack_write.push_duration(elapsed);
        }
    }

    pub fn observe_gap(&mut self, session_id: u64, expected: u64, received: u64) {
        if !self.enabled || received <= expected {
            return;
        }
        self.out_of_order_frames += 1;
        self.max_reorder_distance = self.max_reorder_distance.max(received - expected);
        let key = (session_id, expected);
        if self.active_gap != Some(key) {
            self.active_gap = Some(key);
            self.recovery_events += 1;
        }
    }

    pub fn observe_sink_retry(&mut self) {
        if self.enabled {
            self.sink_retries += 1;
        }
    }

    pub fn set_parser_counters(
        &mut self,
        invalid_frames: u64,
        stream_discarded_bytes: u64,
        connection_discarded_bytes: u64,
    ) {
        if self.enabled {
            self.invalid_frames = invalid_frames;
            self.stream_discarded_bytes = stream_discarded_bytes;
            self.connection_discarded_bytes = connection_discarded_bytes;
        }
    }

    pub fn write_report(&self, path: &Path) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let end_to_end = self.end_to_end_current.snapshot();
        let input_current = self.input_current.snapshot();
        let input_historical = self.input_historical.snapshot();
        let callback_to_write = self.callback_to_write.snapshot();
        let transport = self.transport_one_way.snapshot();
        let service = self.service.snapshot();
        let ack = self.ack_write.snapshot();
        let interarrival = self.interarrival.snapshot();
        let pending = self.unique_frames.saturating_sub(self.accepted);
        let tail_budget = [
            self.end_to_end_current
                .count_above_ms(self.warning_budget_ms),
            self.input_current.count_above_ms(self.warning_budget_ms),
            self.callback_to_write
                .count_above_ms(self.warning_budget_ms),
            self.transport_one_way
                .count_above_ms(self.warning_budget_ms),
            self.service.count_above_ms(self.warning_budget_ms),
            self.ack_write.count_above_ms(self.warning_budget_ms),
        ];
        let severe_threshold_ms = 50.0;
        let severe = [
            self.end_to_end_current.count_above_ms(severe_threshold_ms),
            self.input_current.count_above_ms(severe_threshold_ms),
            self.callback_to_write.count_above_ms(severe_threshold_ms),
            self.transport_one_way.count_above_ms(severe_threshold_ms),
            self.service.count_above_ms(severe_threshold_ms),
            self.ack_write.count_above_ms(severe_threshold_ms),
        ];
        let mut warnings = Vec::new();

        warn_p99(
            &mut warnings,
            "estimated current-touch end-to-end",
            end_to_end,
            self.warning_budget_ms,
        );
        if end_to_end.samples > 0 && end_to_end.p99_9_ms > self.warning_budget_ms * 2.0 {
            warnings.push(format!(
                "estimated current-touch end-to-end p99.9 {:.3} ms exceeded two 120 Hz frames",
                end_to_end.p99_9_ms
            ));
        }
        if severe[0] > 0 {
            warnings.push(format!(
                "rare severe current-touch stalls: {} samples exceeded {:.0} ms; worst {:.3} ms",
                severe[0], severe_threshold_ms, end_to_end.max_ms
            ));
        }
        warn_p99(
            &mut warnings,
            "Android current-event dispatch",
            input_current,
            self.warning_budget_ms,
        );
        warn_p99(
            &mut warnings,
            "estimated USB-tethered network one-way transit",
            transport,
            self.warning_budget_ms,
        );
        warn_p99(
            &mut warnings,
            "Windows host service",
            service,
            self.warning_budget_ms,
        );
        if interarrival.max_ms > self.warning_budget_ms * 2.0 {
            warnings.push(format!(
                "maximum USB-tethered network receive gap {:.3} ms exceeded two 120 Hz frames",
                interarrival.max_ms
            ));
        }
        if self.recovery_events > 0
            || pending > 0
            || self.invalid_frames > 0
            || self.stream_discarded_bytes > 0
            || self.sink_retries > 0
        {
            warnings.push(format!(
                "recovery: events={}, out_of_order={}, pending={}, invalid={}, stream_discarded_bytes={}, sink_retry={}",
                self.recovery_events,
                self.out_of_order_frames,
                pending,
                self.invalid_frames,
                self.stream_discarded_bytes,
                self.sink_retries
            ));
        } else if self.frames_received > 0
            && self.replay_frames > 32
            && self.replay_frames as f64 / self.frames_received as f64 > 0.01
        {
            warnings.push(format!(
                "sustained acknowledgement replay: {} duplicate frames ({:.2}%)",
                self.replay_frames,
                self.replay_frames as f64 * 100.0 / self.frames_received as f64
            ));
        }

        let file = File::create(path)?;
        let mut writer = BufWriter::new(file);
        writeln!(writer, "Holodori lossless host benchmark")?;
        writeln!(
            writer,
            "protocol=4 duration_s={:.3} warning_budget_ms={:.3}",
            self.started.elapsed().as_secs_f64(),
            self.warning_budget_ms
        )?;
        writeln!(
            writer,
            "frames_received={} unique={} accepted={} pending={} replay={}",
            self.frames_received, self.unique_frames, self.accepted, pending, self.replay_frames
        )?;
        writeln!(
            writer,
            "connections={} recovery_events={} out_of_order_frames={} max_reorder_distance={} invalid={} stream_discarded_bytes={} connection_start_discarded_bytes={} sink_retry={}",
            self.connections,
            self.recovery_events,
            self.out_of_order_frames,
            self.max_reorder_distance,
            self.invalid_frames,
            self.stream_discarded_bytes,
            self.connection_discarded_bytes,
            self.sink_retries
        )?;
        write_snapshot(
            &mut writer,
            "touch_current_event_to_windows_estimated_ms",
            end_to_end,
        )?;
        write_snapshot(
            &mut writer,
            "android_current_event_to_callback_ms",
            input_current,
        )?;
        write_snapshot(
            &mut writer,
            "android_historical_event_to_callback_ms",
            input_historical,
        )?;
        write_snapshot(
            &mut writer,
            "android_callback_to_network_write_ms",
            callback_to_write,
        )?;
        write_snapshot(
            &mut writer,
            "usb_tethered_network_one_way_symmetric_estimate_ms",
            transport,
        )?;
        write_snapshot(&mut writer, "windows_receive_to_sink_ms", service)?;
        write_snapshot(&mut writer, "ack_write_ms", ack)?;
        writeln!(
            writer,
            "tail_over_{:.3}ms: end_to_end={} android_dispatch={} callback_to_write={} usb={} windows_sink={} ack={}",
            self.warning_budget_ms,
            tail_budget[0],
            tail_budget[1],
            tail_budget[2],
            tail_budget[3],
            tail_budget[4],
            tail_budget[5]
        )?;
        writeln!(
            writer,
            "tail_over_{severe_threshold_ms:.0}ms: end_to_end={} android_dispatch={} callback_to_write={} usb={} windows_sink={} ack={}",
            severe[0], severe[1], severe[2], severe[3], severe[4], severe[5]
        )?;
        if let Some(worst) = self.worst_current {
            writeln!(
                writer,
                "worst_current_event: session={:016x} seq={} total_ms={:.3} android_dispatch_ms={:.3} callback_to_write_ms={:.3} usb_ms={:.3} windows_sink_ms={:.3}",
                worst.session_id,
                worst.sequence,
                worst.total_nanos / NANOS_PER_MILLI,
                worst.input_dispatch_nanos / NANOS_PER_MILLI,
                worst.callback_to_write_nanos / NANOS_PER_MILLI,
                worst.transport_nanos / NANOS_PER_MILLI,
                worst.service_nanos / NANOS_PER_MILLI
            )?;
        }
        writeln!(
            writer,
            "usb_tethered_network_receive_cadence: mean_interval_ms={:.3} max_gap_ms={:.3}",
            interarrival.mean_ms, interarrival.max_ms
        )?;
        writeln!(
            writer,
            "note=USB-tethered network one-way is half duplex round-trip after subtracting measured phone turnaround; it assumes symmetric directions"
        )?;
        writeln!(
            writer,
            "note=current and historical Android MotionEvent samples are intentionally reported separately"
        )?;
        writeln!(
            writer,
            "note=percentiles cover the complete session in 0.004 ms bins; max remains exact"
        )?;
        writeln!(writer, "warnings={}", warnings.len())?;
        for warning in warnings {
            writeln!(writer, "WARNING: {warning}")?;
        }
        writer.flush()
    }
}

fn ordered_delta(later: u64, earlier: u64) -> Option<f64> {
    (later >= earlier && earlier > 0).then_some((later - earlier) as f64)
}

fn estimate_one_way_nanos(
    host_send: u64,
    phone_receive: u64,
    phone_send: u64,
    host_receive: u64,
) -> Option<f64> {
    if host_send == 0
        || phone_receive == 0
        || phone_send < phone_receive
        || host_receive < host_send
    {
        return None;
    }
    let host_round_trip = host_receive - host_send;
    let phone_turnaround = phone_send - phone_receive;
    (host_round_trip >= phone_turnaround)
        .then_some((host_round_trip - phone_turnaround) as f64 / 2.0)
}

fn warn_p99(warnings: &mut Vec<String>, label: &str, value: Snapshot, budget_ms: f64) {
    if value.samples > 0 && value.p99_ms > budget_ms {
        warnings.push(format!(
            "{label} p99 {:.3} ms exceeded {:.3} ms",
            value.p99_ms, budget_ms
        ));
    }
}

fn write_snapshot(writer: &mut impl Write, label: &str, value: Snapshot) -> io::Result<()> {
    writeln!(
        writer,
        "{label}: n={} mean={:.3} max={:.3} p50={:.3} p90={:.3} p99={:.3} p99.9={:.3}",
        value.samples,
        value.mean_ms,
        value.max_ms,
        value.p50_ms,
        value.p90_ms,
        value.p99_ms,
        value.p99_9_ms
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_session_histogram_reports_tail() {
        let mut series = SampleSeries::new();
        for value in 1..=200_000 {
            series.push_nanos(value as f64 * 1_000.0);
        }
        let snapshot = series.snapshot();
        assert_eq!(snapshot.samples, 200_000);
        assert!((snapshot.p50_ms - 100.0).abs() <= 0.0041);
        assert!((snapshot.p99_ms - 198.0).abs() <= 0.0041);
        assert!((snapshot.p99_9_ms - 199.8).abs() <= 0.0041);
        assert!((snapshot.max_ms - 200.0).abs() < 0.0001);
        assert!((series.count_above_ms(50.0) as i64 - 150_000).abs() <= 4);
    }

    #[test]
    fn duplex_estimate_subtracts_phone_turnaround() {
        // 400 ns total USB-tethered network round trip and 500 ns spent on the phone.
        let estimate = estimate_one_way_nanos(1_000, 11_100, 11_600, 1_900).unwrap();
        assert_eq!(estimate, 200.0);
    }

    #[test]
    fn duplex_estimate_rejects_invalid_clock_order() {
        assert!(estimate_one_way_nanos(1_000, 11_600, 11_100, 1_900).is_none());
        assert!(estimate_one_way_nanos(2_000, 11_100, 11_600, 1_900).is_none());
    }

    #[test]
    fn one_missing_sequence_is_one_recovery_event() {
        let mut metrics = HostMetrics::new(true, 8.333);
        metrics.observe_gap(7, 10, 11);
        metrics.observe_gap(7, 10, 12);
        metrics.observe_gap(7, 10, 13);
        assert_eq!(metrics.recovery_events, 1);
        assert_eq!(metrics.out_of_order_frames, 3);
        assert_eq!(metrics.max_reorder_distance, 3);

        metrics.observe_gap(7, 14, 15);
        assert_eq!(metrics.recovery_events, 2);
    }
}
