use crate::diagnostics::{self as diag, Analysis, Event};
use crate::protocol::{ACTION_HEARTBEAT, TouchFrame};
use std::cell::UnsafeCell;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const QUEUE_CAPACITY: usize = 16_384;

// Private SPSC ownership: only HostMetrics (&mut self) writes, only its worker
// reads. No cloned producer API exists. Acquire/release protects slot reuse;
// there is no CAS, mutex, wakeup syscall, wait, or allocation on publication.
struct Queue {
    slots: Box<[UnsafeCell<Event>]>,
    read: AtomicUsize,
    write: AtomicUsize,
    stopped: AtomicBool,
}

// SAFETY: slots are accessed only between the single-producer/single-consumer
// index handoffs in push/pop. The producer never overwrites unread storage.
unsafe impl Sync for Queue {}

impl Queue {
    fn new() -> Self {
        Self {
            slots: (0..QUEUE_CAPACITY)
                .map(|_| UnsafeCell::new(Event::default()))
                .collect(),
            read: AtomicUsize::new(0),
            write: AtomicUsize::new(0),
            stopped: AtomicBool::new(false),
        }
    }
    fn push(&self, event: Event) -> bool {
        let write = self.write.load(Ordering::Relaxed);
        if write.wrapping_sub(self.read.load(Ordering::Acquire)) == QUEUE_CAPACITY {
            return false;
        }
        // SAFETY: this slot has been released by the only consumer.
        unsafe {
            *self.slots[write % QUEUE_CAPACITY].get() = event;
        }
        self.write.store(write.wrapping_add(1), Ordering::Release);
        true
    }
    fn pop(&self) -> Option<Event> {
        let read = self.read.load(Ordering::Relaxed);
        if read == self.write.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: publication completed, and the producer cannot reuse until read advances.
        let event = unsafe { *self.slots[read % QUEUE_CAPACITY].get() };
        self.read.store(read.wrapping_add(1), Ordering::Release);
        Some(event)
    }
}

struct Recorder {
    queue: Arc<Queue>,
    worker: Option<JoinHandle<Analysis>>,
    dropped: u64,
    drop_span: Option<[u64; 2]>,
}

pub struct HostMetrics {
    started: Instant,
    protocol_version: u8,
    recorder: Option<Recorder>,
    completed: Option<Analysis>,
    transport: String,
    environment: String,
    parser_counters: [u64; 3],
}

impl HostMetrics {
    pub fn new(enabled: bool, warning_budget_ms: f64, protocol_version: u8) -> Self {
        let recorder = enabled.then(|| {
            let queue = Arc::new(Queue::new());
            let consumer = queue.clone();
            let worker = thread::Builder::new()
                .name("diagnostic analysis".into())
                .spawn(move || {
                    crate::platform::lower_diagnostic_priority();
                    let mut analysis = Analysis::new((warning_budget_ms * 1_000_000.0) as u64);
                    loop {
                        while let Some(event) = consumer.pop() {
                            analysis.observe(event);
                        }
                        if consumer.stopped.load(Ordering::Acquire) {
                            while let Some(event) = consumer.pop() {
                                analysis.observe(event);
                            }
                            break;
                        }
                        // This delays only diagnosis. The input thread never wakes or waits for us.
                        thread::sleep(Duration::from_millis(4));
                    }
                    analysis
                })
                .expect("start diagnostic worker before gameplay");
            Recorder {
                queue,
                worker: Some(worker),
                dropped: 0,
                drop_span: None,
            }
        });
        Self {
            started: Instant::now(),
            protocol_version,
            recorder,
            completed: None,
            transport: String::new(),
            environment: String::new(),
            parser_counters: [0; 3],
        }
    }
    pub fn enabled(&self) -> bool {
        self.recorder.is_some() || self.completed.is_some()
    }
    pub fn clock_nanos(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.started)
            .as_nanos()
            .min(u64::MAX as u128) as u64
    }
    pub fn configure(&mut self, transport: &str, environment: String) {
        if self.enabled() {
            self.transport = transport.to_owned();
            self.environment = environment;
        }
    }
    pub fn record(&mut self, event: Event) {
        if let Some(recorder) = &mut self.recorder
            && !recorder.queue.push(event)
        {
            recorder.dropped += 1;
            match &mut recorder.drop_span {
                Some(span) => span[1] = event.0[1],
                None => recorder.drop_span = Some([event.0[1]; 2]),
            }
        }
    }
    pub fn note(&mut self, kind: u64, session: u64, sequence: u64, a: u64, b: u64) {
        if self.recorder.is_some() {
            self.record(Event([
                kind,
                self.clock_nanos(Instant::now()),
                session,
                sequence,
                a,
                b,
                0,
                0,
                0,
                0,
                0,
                0,
            ]));
        }
    }
    pub fn begin_connection(&mut self) {
        self.note(diag::BEGIN, 0, 0, 0, 0);
    }
    pub fn receive_event(
        &self,
        frame: &TouchFrame,
        arrival: Instant,
        duplicate: bool,
    ) -> Option<Event> {
        self.recorder.as_ref()?;
        let flags = if frame.action == ACTION_HEARTBEAT {
            4
        } else if frame.session_start() {
            8
        } else {
            1
        } | if frame.historical() { 2 } else { 0 };
        Some(Event([
            if duplicate {
                diag::DUPLICATE
            } else {
                diag::RECEIVE
            },
            self.clock_nanos(arrival),
            frame.session_id,
            frame.sequence,
            frame.phone_event_nanos,
            frame.phone_callback_nanos,
            frame.phone_send_nanos,
            frame.echo_host_send_nanos,
            frame.phone_control_receive_nanos,
            flags,
            frame.action as u64,
            frame.contacts.len() as u64,
        ]))
    }
    pub fn observe_gap(&mut self, session: u64, expected: u64, received: u64) {
        self.note(
            diag::GAP,
            session,
            received,
            expected,
            received.saturating_sub(expected),
        );
    }
    pub fn observe_sink(
        &mut self,
        frame: &TouchFrame,
        ready: Instant,
        end: Instant,
        retries: u64,
        error: Option<i32>,
    ) {
        if self.recorder.is_none() {
            return;
        }
        self.record(Event([
            if error.is_some() {
                diag::SINK_FAILURE
            } else {
                diag::ACCEPT
            },
            self.clock_nanos(end),
            frame.session_id,
            frame.sequence,
            self.clock_nanos(ready),
            retries,
            error.unwrap_or(0) as u64,
            0,
            0,
            0,
            0,
            0,
        ]));
    }
    pub fn observe_ack(
        &mut self,
        session: u64,
        sequence: Option<u64>,
        start: Instant,
        progressed: bool,
        failed: bool,
    ) {
        if self.recorder.is_none() {
            return;
        }
        let end = Instant::now();
        self.record(Event([
            diag::ACK,
            self.clock_nanos(end),
            session,
            sequence.unwrap_or(u64::MAX),
            end.saturating_duration_since(start).as_nanos() as u64,
            progressed as u64,
            failed as u64,
            self.clock_nanos(start),
            0,
            0,
            0,
            0,
        ]));
    }
    pub fn observe_ack_write(&mut self, elapsed: Duration) {
        self.note(diag::ACK, 0, u64::MAX, elapsed.as_nanos() as u64, 0);
    }
    pub fn set_parser_counters(&mut self, invalid: u64, discarded: u64, connection_discarded: u64) {
        self.parser_counters = [invalid, discarded, connection_discarded];
    }
    fn finish(&mut self) -> io::Result<()> {
        if let Some(mut recorder) = self.recorder.take() {
            recorder.queue.stopped.store(true, Ordering::Release);
            let mut analysis =
                recorder.worker.take().unwrap().join().map_err(|_| {
                    io::Error::other("diagnostic worker failed; coverage unavailable")
                })?;
            analysis.finish(recorder.dropped);
            analysis.diagnostic_drop_span_ns = recorder.drop_span;
            self.completed = Some(analysis);
        }
        Ok(())
    }
    pub fn write_report(&mut self, path: &Path) -> io::Result<()> {
        self.finish()?;
        let Some(analysis) = &self.completed else {
            return Ok(());
        };
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        let mut writer = BufWriter::new(File::create(path)?);
        writeln!(
            writer,
            "Doritrack diagnostic report schema=2 protocol={} transport={} duration_s={:.3}",
            self.protocol_version,
            self.transport,
            self.started.elapsed().as_secs_f64()
        )?;
        writeln!(writer, "environment={}", self.environment)?;
        writeln!(
            writer,
            "diagnostic_drop_span_ns={:?}",
            analysis.diagnostic_drop_span_ns
        )?;
        writeln!(
            writer,
            "environment_at_stop={}",
            crate::platform::diagnostic_environment()
        )?;
        writeln!(
            writer,
            "budget_ns={} diagnostic_records_dropped={} incidents={} omitted_incident_triggers={}",
            analysis.budget_ns,
            analysis.diagnostic_records_dropped,
            analysis.incidents.len(),
            analysis.incidents_omitted
        )?;
        writeln!(
            writer,
            "coverage=host-observed frames only; join Android report to account for frames never received. Any omitted evidence or queue loss makes forensic coverage incomplete."
        )?;
        writeln!(
            writer,
            "latency=OS submission, not game-observed input. No one-way point estimate. Local lower bounds exclude unknown forward transit. Duplex residual includes both directions and endpoint scheduling; never divide by two."
        )?;
        writeln!(
            writer,
            "clock=echo validity <=2s; relative drift allowance 1000ppm is an assumption, not calibration; MotionEvent precision may be 1ms on Android <34."
        )?;
        writeln!(
            writer,
            "parser_invalid={} parser_discarded_bytes={} parser_boundary_bytes={}",
            self.parser_counters[0], self.parser_counters[1], self.parser_counters[2]
        )?;
        for (key, value) in &analysis.counts {
            writeln!(writer, "{key}={value}")?;
        }
        writeln!(
            writer,
            "histogram=integer nanoseconds; 32 subdivisions/octave; full u64 range; inclusive quantile bounds; exact maximum and strict budget exceed counts; no warmup exclusion"
        )?;
        for (key, value) in &analysis.series {
            writeln!(
                writer,
                "{key}: n={} invalid={} over_budget={} mean_ns={} max_ns={} p50={:?} p90={:?} p99={:?} p99.9={:?} tail_quality={}",
                value.n,
                value.invalid,
                value.over_budget,
                if value.n == 0 {
                    "unavailable".into()
                } else {
                    (value.sum_ns / value.n as u128).to_string()
                },
                value.max_ns,
                value.quantile(50, 100),
                value.quantile(90, 100),
                if value.n >= 1000 {
                    value.quantile(99, 100)
                } else {
                    None
                },
                if value.n >= 10000 {
                    value.quantile(999, 1000)
                } else {
                    None
                },
                if value.n >= 10000 {
                    "tail ranks available; not independent samples or confidence intervals"
                } else {
                    "insufficient tail support; max/exceed counts still apply"
                }
            )?;
        }
        for incident in &analysis.incidents {
            writeln!(
                writer,
                "incident={} at_ns={} last_anomaly_ns={} frames={:?}..{:?} affected_observations={} recovery_to_os_commit_ns={:?} causes={:?} evidence={} omitted={} post_context_complete={}",
                incident.id,
                incident.started_ns,
                incident.last_anomaly_ns,
                incident.first_frame,
                incident.last_frame,
                incident.affected_frames,
                incident.recovery_ns,
                incident.causes,
                incident.evidence.len(),
                incident.evidence_omitted,
                incident.post_context_complete
            )?;
            writeln!(
                writer,
                "  affected_frame_keys={:?} omitted_keys={} fresh_session_commit_ns={:?} reconstructed_snapshot_ns={:?}",
                incident.affected_frame_keys,
                incident.affected_keys_omitted,
                incident.fresh_session_commit_ns,
                incident.reconstructed_snapshot_ns
            )?;
        }
        writer.flush()?;
        // An explicit --metrics-file ending in .json still names the readable
        // report. Never overwrite it while creating the structured companion.
        let json_path = path.with_extension(
            if path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("json"))
            {
                "events.json"
            } else {
                "json"
            },
        );
        let mut json = BufWriter::new(File::create(json_path)?);
        serde_json::to_writer_pretty(
            &mut json,
            &serde_json::json!({"source": "host", "protocol": self.protocol_version, "transport": self.transport, "environment": self.environment, "analysis": analysis, "recent_context": analysis.recent_context()}),
        )?;
        json.flush()
    }
}

impl Drop for HostMetrics {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

/// Resolve report storage only during setup or after Stop, never per frame.
#[cfg(windows)]
pub fn log_directory() -> io::Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::{APPMODEL_ERROR_NO_PACKAGE, ERROR_SUCCESS};
    use windows_sys::Win32::Storage::Packaging::Appx::{
        GetCurrentPackageFamilyName, PACKAGE_FAMILY_NAME_MAX_LENGTH,
    };

    let mut buffer = [0_u16; PACKAGE_FAMILY_NAME_MAX_LENGTH as usize + 1];
    let mut length = buffer.len() as u32;
    // The buffer is sized to the documented maximum, including its terminator.
    let result = unsafe { GetCurrentPackageFamilyName(&mut length, buffer.as_mut_ptr()) };
    let family = match result {
        APPMODEL_ERROR_NO_PACKAGE => None,
        ERROR_SUCCESS if length > 1 && length as usize <= buffer.len() => {
            Some(OsString::from_wide(&buffer[..length as usize - 1]))
        }
        ERROR_SUCCESS => return Err(io::Error::other("invalid Windows package family name")),
        code => return Err(io::Error::from_raw_os_error(code as i32)),
    };
    windows_log_directory(std::env::var_os("LOCALAPPDATA"), family.as_deref())
}

#[cfg(windows)]
fn windows_log_directory(
    base: Option<std::ffi::OsString>,
    package_family: Option<&std::ffi::OsStr>,
) -> io::Result<PathBuf> {
    let base = base
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "LOCALAPPDATA is unavailable or not absolute; use --metrics-file PATH",
            )
        })?;
    // Explicit package storage avoids relying on MSIX's redirection behavior,
    // which differs between elevated and unelevated desktop processes.
    Ok(match package_family {
        Some(family) => base
            .join("Packages")
            .join(family)
            .join("LocalState")
            .join("Logs"),
        None => base.join("Doritrack").join("Logs"),
    })
}

#[cfg(not(windows))]
pub fn log_directory() -> io::Result<PathBuf> {
    // Keep the existing XDG location for Linux users.
    if let Ok(state_home) = std::env::var("XDG_STATE_HOME")
        && !state_home.is_empty()
    {
        return Ok(PathBuf::from(state_home).join("holodori").join("logs"));
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
    Ok(PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("holodori")
        .join("logs"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_and_saturated_producers_never_allocate_or_wait() {
        let (_, allocations) = crate::allocation_check::count(|| HostMetrics::new(false, 8.333, 5));
        assert_eq!(allocations, 0);
        let queue = Queue::new();
        let (_, allocations) = crate::allocation_check::count(|| {
            for n in 0..QUEUE_CAPACITY {
                assert!(queue.push(Event([n as u64; 12])));
            }
            assert!(!queue.push(Event::default()));
            for n in 0..QUEUE_CAPACITY {
                assert_eq!(queue.pop().unwrap().0, [n as u64; 12]);
            }
        });
        assert_eq!(allocations, 0);
    }

    #[test]
    fn producer_consumer_wrap_and_stop_preserve_all_published_fields() {
        let queue = Arc::new(Queue::new());
        let consumer = queue.clone();
        let worker = thread::spawn(move || {
            for n in 0..100_000 {
                loop {
                    if let Some(e) = consumer.pop() {
                        assert_eq!(e.0, [n; 12]);
                        break;
                    }
                    thread::yield_now();
                }
            }
        });
        for n in 0..100_000 {
            while !queue.push(Event([n; 12])) {
                thread::yield_now();
            }
        }
        worker.join().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn reports_keep_portable_and_msix_user_storage() {
        use std::ffi::OsStr;
        let base = Path::new(r"C:\Users\Player One\AppData\Local");
        assert_eq!(
            windows_log_directory(Some(base.into()), None).unwrap(),
            base.join("Doritrack").join("Logs")
        );
        assert_eq!(
            windows_log_directory(Some(base.into()), Some(OsStr::new("Doritrack_Test"))).unwrap(),
            base.join("Packages/Doritrack_Test/LocalState/Logs")
        );
        assert!(windows_log_directory(Some("relative".into()), None).is_err());
    }
}
