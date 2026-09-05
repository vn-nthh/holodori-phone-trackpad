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
    let deadline = Instant::now() + SINK_STALL_TIMEOUT;
    loop {
        match sink.cancel_all() {
            Ok(()) => return Ok(()),
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => {
                metrics.observe_sink_retry();
                std::thread::yield_now();
            }
        }
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
        loop {
            match sink.accept(frame) {
                Ok(()) => break,
                Err(error) => {
                    metrics.observe_sink_retry();
                    if stopping.load(Ordering::Relaxed) {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "controller stopped",
                        ));
                    }
                    if retry_started.elapsed() >= SINK_STALL_TIMEOUT {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, error));
                    }
                    std::thread::yield_now();
                }
            }
        }
        metrics.observe_accepted(frame, Instant::now());
        if !ordered.commit_ready() {
            return Err(io::Error::other(
                "accepted frame was missing from the receive window",
            ));
        }
        progressed = true;
    }
    Ok(progressed)
}
