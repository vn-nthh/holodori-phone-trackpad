//! Shared OS acceptance and cancellation boundary for both wire protocols.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::metrics::HostMetrics;
use crate::protocol::{OrderedFrames, TouchFrame};

pub const SINK_STALL_TIMEOUT: Duration = Duration::from_millis(8);

pub trait InputSink {
    fn accept(&mut self, frame: &TouchFrame) -> io::Result<()>;
    fn has_active_input(&self) -> bool;
    fn cancel_all(&mut self) -> io::Result<()>;
}

pub fn cancel_with_deadline(
    sink: &mut impl InputSink,
    metrics: &mut HostMetrics,
) -> io::Result<()> {
    let started = Instant::now();
    let deadline = started + SINK_STALL_TIMEOUT;
    loop {
        let result = sink.cancel_all();
        if result.is_ok() || Instant::now() >= deadline {
            let code = result
                .as_ref()
                .map_or_else(|error| error.raw_os_error().unwrap_or(-1) as u64, |()| 0);
            let elapsed = started.elapsed().as_nanos() as u64;
            metrics.note(crate::diagnostics::CANCEL, 0, 0, elapsed, code);
            return result;
        }
        std::thread::yield_now();
    }
}

/// Record a received frame and buffer it in order. A unique frame the window
/// refused (stale session or beyond the window) is recorded as rejected.
pub fn receive_ordered(
    ordered: &mut OrderedFrames,
    metrics: &mut HostMetrics,
    frame: TouchFrame,
    arrival: Instant,
) {
    let (session, sequence) = (frame.session_id, frame.sequence);
    let same_session = ordered.session_id() == Some(session);
    let expected = ordered.expected_sequence();
    let replay = same_session && (sequence < expected || ordered.contains_sequence(sequence));
    if same_session
        && !replay
        && sequence > expected
        && sequence - expected < crate::protocol::MAX_REORDERED_FRAMES as u64
    {
        metrics.observe_gap(session, expected, sequence);
    }
    let observation = metrics.receive_event(&frame, arrival, replay);
    ordered.push(frame);
    if let Some(mut event) = observation {
        if !replay
            && (!ordered.contains_sequence(sequence) || ordered.session_id() != Some(session))
        {
            event.0[0] = crate::diagnostics::REJECT;
        }
        metrics.record(event);
    }
}

/// Borrow each frame until the sink accepts it; neither copying nor early ACK is necessary.
pub fn commit_ready(
    ordered: &mut OrderedFrames,
    sink: &mut impl InputSink,
    metrics: &mut HostMetrics,
    stopping: &AtomicBool,
) -> io::Result<bool> {
    let mut progressed = false;
    while let Some(frame) = ordered.next_ready() {
        let retry_started = Instant::now();
        let mut retries = 0;
        let failure = loop {
            match sink.accept(frame) {
                Ok(()) => break None,
                Err(error) => {
                    retries += 1;
                    let stopped = stopping.load(Ordering::Relaxed);
                    if stopped || retry_started.elapsed() >= SINK_STALL_TIMEOUT {
                        break Some((error, stopped));
                    }
                    std::thread::yield_now();
                }
            }
        };
        let code = failure
            .as_ref()
            .map(|(error, _)| error.raw_os_error().unwrap_or(-1));
        metrics.observe_sink(frame, retry_started, Instant::now(), retries, code);
        if let Some((error, stopped)) = failure {
            return Err(if stopped {
                io::Error::new(io::ErrorKind::Interrupted, "controller stopped")
            } else {
                io::Error::new(io::ErrorKind::TimedOut, error)
            });
        }
        if !ordered.commit_ready() {
            return Err(io::Error::other(
                "accepted frame was missing from the receive window",
            ));
        }
        progressed = true;
    }
    Ok(progressed)
}
