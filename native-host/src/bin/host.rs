use std::env;
use std::error::Error;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
#[cfg(windows)]
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use holodori_native_host::input::{InputSink, cancel_with_deadline, commit_ready};
use holodori_native_host::keyboard::KeyboardSink;
use holodori_native_host::metrics::HostMetrics;
use holodori_native_host::network::{DEFAULT_UDP_PORT, UdpConnection, UdpHost};
use holodori_native_host::platform;
use holodori_native_host::protocol::{
    CONTROL_ACK, CONTROL_HELLO, FrameParser, OrderedFrames, ProtocolError, TouchFrame,
    encode_control,
};
#[cfg(windows)]
use holodori_native_host::tether_policy::{
    RecoveryOutcome, TetherRoutePolicy, recover_orphaned_policy,
};
#[cfg(windows)]
use holodori_native_host::touch::{PROBE_WINDOW_TITLE, TouchInjector, TouchTarget};
use holodori_native_host::v5::TransportKind;
use holodori_native_host::v5_host::{
    HostV5Error, PairCommand, PairEvent, V5Connection, accept_remembered, pair, serve_gameplay,
};

// Touch mode is Windows-only (it drives Windows Touch injection). On other
// platforms there is no probe window to attach to, so this is just a stable
// default identifier for the `--target-title` option.
#[cfg(not(windows))]
const PROBE_WINDOW_TITLE: &str = "Holodori Touch Probe";

const RECEIVE_WINDOW: u32 = 128;
const ACTIVE_INPUT_SILENCE_TIMEOUT: Duration = Duration::from_millis(32);
const IDLE_PEER_SILENCE_TIMEOUT: Duration = Duration::from_secs(2);
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
static SHUTDOWN_COMPLETE: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum HostPhase {
    Waiting = 0,
    Connected = 1,
    Recovering = 2,
    Stopping = 3,
    Pairing = 4,
}

impl HostPhase {
    fn token(self) -> &'static str {
        match self {
            Self::Waiting => "HPT_STATUS WAITING",
            Self::Connected => "HPT_STATUS CONNECTED",
            Self::Recovering => "HPT_STATUS RECOVERING",
            Self::Stopping => "HPT_STATUS STOPPING",
            Self::Pairing => "HPT_STATUS PAIRING",
        }
    }
}

struct StatusReporter {
    phase: AtomicU8,
    sender: Sender<HostPhase>,
}

impl StatusReporter {
    fn start() -> Self {
        let phase = AtomicU8::new(HostPhase::Waiting as u8);
        let (sender, receiver) = channel::<HostPhase>();
        thread::spawn(move || {
            while let Ok(current) = receiver.recv() {
                println!("{}", current.token());
                let _ = io::stdout().flush();
            }
        });
        let reporter = Self { phase, sender };
        let _ = reporter.sender.send(HostPhase::Waiting);
        reporter
    }

    fn publish(&self, phase: HostPhase) {
        if self.phase.swap(phase as u8, Ordering::AcqRel) != phase as u8 {
            let _ = self.sender.send(phase);
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Mode {
    #[cfg(windows)]
    Touch,
    Keys,
    Record,
}

struct Options {
    mode: Mode,
    lane_keys: Vec<String>,
    spawn_probe: bool,
    target_title: String,
    udp_port: u16,
    metrics: bool,
    metrics_file: Option<PathBuf>,
    warning_budget_ms: f64,
    local_only_tether: bool,
    pair: bool,
    legacy_v4: bool,
    transport: Option<TransportKind>,
}

struct ControlState {
    hello_session: Option<u64>,
    last_ack: Option<u64>,
    lane_count: u8,
    last_committed_frame: Instant,
}

enum Sink {
    #[cfg(windows)]
    Touch(Box<TouchInjector>),
    Keys(Box<KeyboardSink>),
    Record,
}

impl Sink {
    fn lane_count(&self, options: &Options) -> u8 {
        match self {
            #[cfg(windows)]
            Self::Touch(_) => options.lane_keys.len().min(u8::MAX as usize) as u8,
            Self::Keys(keys) => keys.lane_count(),
            Self::Record => options.lane_keys.len().min(u8::MAX as usize) as u8,
        }
    }
}

impl InputSink for Sink {
    fn accept(&mut self, frame: &TouchFrame) -> io::Result<()> {
        match self {
            #[cfg(windows)]
            Self::Touch(touch) => touch.accept(frame),
            Self::Keys(keys) => keys.accept(frame),
            Self::Record => {
                println!(
                    "session={:016x} seq={} action={} contacts={} locked={} event_ns={} callback_ns={} send_ns={}",
                    frame.session_id,
                    frame.sequence,
                    frame.action,
                    frame.contacts.len(),
                    frame.locked(),
                    frame.phone_event_nanos,
                    frame.phone_callback_nanos,
                    frame.phone_send_nanos
                );
                Ok(())
            }
        }
    }

    fn has_active_input(&self) -> bool {
        match self {
            #[cfg(windows)]
            Self::Touch(touch) => touch.has_active_input(),
            Self::Keys(keys) => keys.has_active_input(),
            Self::Record => false,
        }
    }

    fn cancel_all(&mut self) -> io::Result<()> {
        match self {
            #[cfg(windows)]
            Self::Touch(touch) => touch.cancel_all(),
            Self::Keys(keys) => keys.release_all(),
            Self::Record => Ok(()),
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("fatal: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    platform::raise_input_priority();
    platform::install_shutdown_handler(&SHUTDOWN_REQUESTED, &SHUTDOWN_COMPLETE)?;
    let options = parse_options()?;
    #[cfg(windows)]
    {
        match recover_orphaned_policy().map_err(|error| {
            let hint = if error.kind() == io::ErrorKind::PermissionDenied {
                "; run the launcher as administrator"
            } else {
                ""
            };
            format!("could not recover a previous local-only tether policy: {error}{hint}")
        })? {
            RecoveryOutcome::NothingToDo => {}
            RecoveryOutcome::Restored { snapshots } => {
                println!("Recovered {snapshots} orphaned USB-tether route settings.");
            }
            RecoveryOutcome::OwnerStillRunning => {
                return Err("another local-only tether policy owner is still running".into());
            }
        }
    }
    if options.pair {
        let result = run_pairing_mode(&options);
        SHUTDOWN_COMPLETE.store(true, Ordering::Release);
        return result;
    }
    if !options.legacy_v4 {
        let result = run_v5_controller(&options);
        SHUTDOWN_COMPLETE.store(true, Ordering::Release);
        return result;
    }
    let mut sink = build_sink(&options)?;
    let lane_count = sink.lane_count(&options);
    let udp = UdpHost::bind(options.udp_port)?;
    let mut metrics = HostMetrics::new(options.metrics, options.warning_budget_ms, 4);
    #[cfg(windows)]
    let mut tether_policy =
        if options.local_only_tether {
            Some(TetherRoutePolicy::new().map_err(|error| {
                format!("could not initialize local-only USB tethering: {error}")
            })?)
        } else {
            None
        };
    // The launcher owns the host's stdin and sends `q` for a graceful stop.
    // Keep this available even when the user turns report writing off so the
    // UI never has to terminate the process and risk held input.
    install_exit_command_thread()?;
    let status = StatusReporter::start();

    println!("Holodori native host - USB tethering/RNDIS + UDP protocol v4");
    println!(
        "mode={:?}, lanes={}, udp_port={}",
        options.mode,
        lane_count,
        udp.port()
    );
    println!(
        "Enable USB tethering on the unlocked Android phone, then connect one USB data cable."
    );
    println!("Press Q then Enter to stop gracefully.");
    io::stdout().flush()?;

    let mut ordered = OrderedFrames::new();
    let mut parser = FrameParser::default();
    while !SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
        #[cfg(windows)]
        if let Some(policy) = tether_policy.as_mut() {
            policy.refresh().map_err(|error| {
                let hint = if error.kind() == io::ErrorKind::PermissionDenied {
                    "; run the launcher as administrator"
                } else {
                    ""
                };
                format!("could not enable local-only USB tethering: {error}{hint}")
            })?;
        }
        if sink.has_active_input() {
            match cancel_sink_with_deadline(&mut sink, &mut metrics) {
                Ok(()) => {}
                Err(error) => {
                    eprintln!("the OS still has active injected input: {error}; retrying release");
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }
            }
        }
        println!(
            "waiting for USB-tethered phone on UDP port {}...",
            udp.port()
        );
        // Keep graceful Q/Ctrl+C shutdown bounded even while no phone is
        // present. The outer loop retries, so a one-second discovery slice is
        // sufficient without making exit wait for the old 15-second timeout.
        let connection =
            match connect_phone(&udp, Duration::from_secs(1), options.local_only_tether) {
                Ok(connection) => connection,
                Err(error) => {
                    #[cfg(target_os = "linux")]
                    if options.local_only_tether && error.kind() == io::ErrorKind::PermissionDenied
                    {
                        return Err(error.into());
                    }
                    eprintln!("{error}; retrying");
                    thread::sleep(Duration::from_millis(250));
                    continue;
                }
            };
        #[cfg(windows)]
        if let Some(policy) = tether_policy.as_mut()
            && let Err(error) = policy.protect_peer(connection.peer(), connection.tether_binding())
        {
            if error.kind() == io::ErrorKind::PermissionDenied {
                return Err(format!(
                        "could not protect the USB tether route for {}: {error}; run the launcher as administrator",
                        connection.peer(),
                    )
                    .into());
            }
            eprintln!(
                "USB tether peer or adapter changed before route protection for {}: {error}; retrying discovery",
                connection.peer(),
            );
            status.publish(HostPhase::Recovering);
            continue;
        }
        if let Err(error) = connection.revalidate_peer() {
            eprintln!(
                "USB tether peer or adapter changed after discovery for {}: {error}; retrying discovery",
                connection.peer(),
            );
            status.publish(HostPhase::Recovering);
            continue;
        }
        status.publish(HostPhase::Connected);
        println!("UDP link ready from {}", connection.peer());
        io::stdout().flush()?;
        parser.begin_connection();
        metrics.begin_connection();
        if let Err(error) = serve_connection(
            connection,
            &mut parser,
            &mut ordered,
            &mut sink,
            &mut metrics,
            lane_count,
        ) {
            if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                break;
            }
            // A read or ACK-write failure can happen after the OS accepted a
            // key/touch. Never wait in discovery with that input held, and do
            // not allow delayed frames from the failed link to reapply it.
            status.publish(HostPhase::Recovering);
            ordered.require_fresh_session();
            if let Err(release_error) = cancel_sink_with_deadline(&mut sink, &mut metrics) {
                eprintln!(
                    "UDP link interrupted: {error}; input release is still pending: {release_error}"
                );
            } else {
                eprintln!(
                    "UDP link interrupted: {error}; released input and require a fresh session"
                );
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    status.publish(HostPhase::Stopping);
    let release_result = cancel_sink_with_deadline(&mut sink, &mut metrics);

    metrics.set_parser_counters(
        parser.invalid_frames,
        parser.discarded_bytes,
        parser.connection_discarded_bytes,
    );
    let report_result = if metrics.enabled() {
        let report_path = options.metrics_file.unwrap_or_else(default_metrics_path);
        metrics.write_report(&report_path).map(|()| {
            println!("Metrics written to {}", report_path.display());
            let _ = io::stdout().flush();
        })
    } else {
        Ok(())
    };
    release_result?;
    #[cfg(windows)]
    if let Some(policy) = tether_policy.as_mut() {
        policy.restore()?;
    }
    SHUTDOWN_COMPLETE.store(true, Ordering::Release);
    report_result.map_err(Into::into)
}

fn run_pairing_mode(options: &Options) -> Result<(), Box<dyn Error>> {
    let transport = options
        .transport
        .ok_or("--transport usb|wifi is required for protocol v5 pairing")?;
    let commands = install_pair_command_thread()?;
    let status = StatusReporter::start();
    status.publish(HostPhase::Pairing);
    println!(
        "Holodori native host - protocol v5 pairing over {}",
        transport.label()
    );
    println!("HPT_PAIR WINDOW 60");
    io::stdout().flush()?;
    let result = pair(transport, options.udp_port, &commands, |event| {
        match event {
            PairEvent::Waiting => println!("HPT_PAIR WAITING"),
            PairEvent::Pattern(pattern) => println!(
                "HPT_PAIR PATTERN {}",
                pattern
                    .iter()
                    .map(u8::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            PairEvent::RemoteConfirmed => println!("HPT_PAIR CONFIRMED"),
            PairEvent::Quality(summary) => println!("HPT_QUALITY {summary}"),
            PairEvent::Complete => println!("HPT_PAIR COMPLETE"),
        }
        let _ = io::stdout().flush();
    });
    match result {
        Ok(()) => Ok(()),
        Err(HostV5Error::Cancelled) if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) => {
            status.publish(HostPhase::Stopping);
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn run_v5_controller(options: &Options) -> Result<(), Box<dyn Error>> {
    let transport = options
        .transport
        .ok_or("--transport usb|wifi is required for protocol v5")?;
    let mut sink = build_sink(options)?;
    let lane_count = sink.lane_count(options);
    let mut metrics = HostMetrics::new(options.metrics, options.warning_budget_ms, 5);
    #[cfg(windows)]
    let mut tether_policy =
        if options.local_only_tether {
            Some(TetherRoutePolicy::new().map_err(|error| {
                format!("could not initialize local-only USB tethering: {error}")
            })?)
        } else {
            None
        };
    install_exit_command_thread()?;
    let status = StatusReporter::start();
    println!(
        "Holodori native host - authenticated UDP protocol v5 over {}",
        transport.label()
    );
    println!(
        "mode={:?}, lanes={}, udp_port={}",
        options.mode, lane_count, options.udp_port
    );
    println!("Press Q then Enter to stop gracefully.");
    io::stdout().flush()?;

    let mut ordered = OrderedFrames::new();
    while !SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
        #[cfg(windows)]
        if let Some(policy) = tether_policy.as_mut() {
            policy.refresh().map_err(|error| {
                let hint = if error.kind() == io::ErrorKind::PermissionDenied {
                    "; run the launcher as administrator"
                } else {
                    ""
                };
                format!("could not enable local-only USB tethering: {error}{hint}")
            })?;
        }
        if sink.has_active_input()
            && let Err(error) = cancel_sink_with_deadline(&mut sink, &mut metrics)
        {
            eprintln!("the OS still has active injected input: {error}; retrying release");
            thread::sleep(Duration::from_millis(1));
            continue;
        }
        status.publish(HostPhase::Waiting);
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut connection = match accept_remembered(transport, options.udp_port, deadline) {
            Ok(connection) => connection,
            Err(HostV5Error::TimedOut(_)) => continue,
            Err(HostV5Error::NotPaired) => {
                return Err("no paired phone; use Pair before Start".into());
            }
            Err(error) => {
                eprintln!("{error}; retrying authenticated discovery");
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        verify_v5_local_only(&connection, options.local_only_tether)?;
        #[cfg(windows)]
        if let Some(policy) = tether_policy.as_mut() {
            let binding = connection
                .tether_binding()
                .ok_or("local-only tethering requires the selected protocol-v5 USB transport")?;
            policy
                .protect_peer(connection.peer(), binding)
                .map_err(|error| {
                    let hint = if error.kind() == io::ErrorKind::PermissionDenied {
                        "; run the launcher as administrator"
                    } else {
                        ""
                    };
                    format!(
                        "could not protect the USB tether route for {}: {error}{hint}",
                        connection.peer()
                    )
                })?;
        }
        if let Err(error) = connection.revalidate_interface() {
            status.publish(HostPhase::Recovering);
            eprintln!(
                "selected interface changed after authentication: {error}; retrying fresh IK"
            );
            continue;
        }
        metrics.begin_connection();
        status.publish(HostPhase::Connected);
        println!(
            "Authenticated v5 link ready from {} (connection {:016x})",
            connection.peer(),
            connection.connection_id()
        );
        io::stdout().flush()?;
        if let Err(error) = serve_gameplay(
            &mut connection,
            &mut ordered,
            &mut sink,
            &mut metrics,
            lane_count,
            &SHUTDOWN_REQUESTED,
        ) {
            if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                break;
            }
            status.publish(HostPhase::Recovering);
            ordered.require_fresh_session();
            if let Err(release_error) = cancel_sink_with_deadline(&mut sink, &mut metrics) {
                eprintln!(
                    "authenticated link interrupted: {error}; input release is still pending: {release_error}"
                );
            } else {
                eprintln!(
                    "authenticated link interrupted: {error}; released input and require fresh IK"
                );
            }
        }
    }

    status.publish(HostPhase::Stopping);
    let release_result = cancel_sink_with_deadline(&mut sink, &mut metrics);
    metrics.set_parser_counters(0, 0, 0);
    let report_result = if metrics.enabled() {
        let report_path = options
            .metrics_file
            .clone()
            .unwrap_or_else(default_metrics_path);
        metrics.write_report(&report_path).map(|()| {
            println!("Metrics written to {}", report_path.display());
            let _ = io::stdout().flush();
        })
    } else {
        Ok(())
    };
    release_result?;
    #[cfg(windows)]
    if let Some(policy) = tether_policy.as_mut() {
        policy.restore()?;
    }
    report_result.map_err(Into::into)
}

fn install_pair_command_thread() -> io::Result<Receiver<PairCommand>> {
    let (sender, receiver) = channel();
    thread::Builder::new()
        .name("v5 pairing command".to_owned())
        .spawn(move || {
            let stdin = io::stdin();
            for line in stdin.lock().lines() {
                match line {
                    Ok(value) if value.trim().eq_ignore_ascii_case("approve") => {
                        let _ = sender.send(PairCommand::Approve);
                    }
                    Ok(value)
                        if value.trim().eq_ignore_ascii_case("q")
                            || value.trim().eq_ignore_ascii_case("cancel") =>
                    {
                        SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
                        let _ = sender.send(PairCommand::Cancel);
                        return;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
            let _ = sender.send(PairCommand::Cancel);
        })?;
    Ok(receiver)
}

#[cfg(target_os = "linux")]
fn verify_v5_local_only(
    connection: &V5Connection,
    local_only_tether: bool,
) -> Result<(), Box<dyn Error>> {
    if !local_only_tether {
        return Ok(());
    }
    let binding = connection
        .tether_binding()
        .ok_or("local-only tethering requires the selected USB transport")?;
    let (ipv4, ipv6) = binding.default_routes_present()?;
    if ipv4 || ipv6 {
        return Err(
            "local-only routing failed closed because the USB tether owns a default route".into(),
        );
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn verify_v5_local_only(
    _connection: &V5Connection,
    _local_only_tether: bool,
) -> Result<(), Box<dyn Error>> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn connect_phone<'a>(
    udp: &'a UdpHost,
    timeout: Duration,
    local_only_tether: bool,
) -> io::Result<UdpConnection<'a>> {
    if !local_only_tether {
        return udp.connect(timeout);
    }
    udp.connect_checked(timeout, |binding| {
        let (ipv4_default, ipv6_default) = binding.default_routes_present().map_err(|error| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "could not verify local-only routing on the discovered RNDIS device: {error}"
                ),
            )
        })?;
        if ipv4_default || ipv6_default {
            let families = match (ipv4_default, ipv6_default) {
                (true, true) => "IPv4 and IPv6",
                (true, false) => "IPv4",
                (false, true) => "IPv6",
                (false, false) => unreachable!(),
            };
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "local-only routing failed closed because a {families} default route uses the discovered RNDIS device"
                ),
            ));
        }
        Ok(())
    })
}

#[cfg(not(target_os = "linux"))]
fn connect_phone<'a>(
    udp: &'a UdpHost,
    timeout: Duration,
    _local_only_tether: bool,
) -> io::Result<UdpConnection<'a>> {
    udp.connect(timeout)
}

fn install_exit_command_thread() -> io::Result<()> {
    thread::Builder::new()
        .name("metrics exit command".to_owned())
        .spawn(|| {
            let stdin = io::stdin();
            for line in stdin.lock().lines() {
                match line {
                    Ok(value) if value.trim().eq_ignore_ascii_case("q") => {
                        SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            // The GUI owns this pipe. EOF means the launcher exited or
            // crashed, so the hidden host must not survive as an orphan that
            // keeps the UDP port or injected keys alive.
            SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
        })
        .map(|_| ())
}

fn default_metrics_path() -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    metrics_log_directory().join(format!("holodori-metrics-{timestamp}.txt"))
}

#[cfg(windows)]
fn metrics_log_directory() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Logs")
}

#[cfg(not(windows))]
fn metrics_log_directory() -> PathBuf {
    // Writing next to the binary (the Windows convention) is wrong on Linux:
    // the binary directory is often read-only (e.g. /usr/bin) and is not
    // where per-user runtime state belongs. Follow the XDG base directory
    // spec instead. `write_report` creates this directory if it is missing.
    if let Ok(state_home) = env::var("XDG_STATE_HOME")
        && !state_home.is_empty()
    {
        return PathBuf::from(state_home).join("holodori").join("logs");
    }
    let home = env::var("HOME").unwrap_or_else(|_| ".".to_owned());
    PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("holodori")
        .join("logs")
}

fn serve_connection(
    mut connection: UdpConnection<'_>,
    parser: &mut FrameParser,
    ordered: &mut OrderedFrames,
    sink: &mut Sink,
    metrics: &mut HostMetrics,
    lane_count: u8,
) -> Result<(), Box<dyn Error>> {
    let mut read_buffer = [0_u8; 4096];
    let mut control = ControlState {
        hello_session: None,
        last_ack: None,
        lane_count,
        last_committed_frame: Instant::now(),
    };

    while !SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
        if idle_peer_timed_out(sink.has_active_input(), connection.peer_activity_elapsed()) {
            ordered.require_fresh_session();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "phone sent no valid peer activity for {} ms; returning to discovery",
                    IDLE_PEER_SILENCE_TIMEOUT.as_millis()
                ),
            )
            .into());
        }
        // Check before every read, not only after a socket timeout. A stream
        // of valid-but-uncommittable future frames must not keep stale input
        // held forever behind one missing or rejected sequence.
        if sink.has_active_input()
            && control.last_committed_frame.elapsed() >= ACTIVE_INPUT_SILENCE_TIMEOUT
        {
            ordered.require_fresh_session();
            cancel_sink_with_deadline(sink, metrics)?;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "phone input made no committed progress for {} ms; released held input and require a fresh session",
                    ACTIVE_INPUT_SILENCE_TIMEOUT.as_millis()
                ),
            )
            .into());
        }
        // Android repeats discovery every 500 ms. Revalidating its adapter
        // identity may enumerate interfaces/read sysfs, so do that only while
        // input is idle. During active input, cumulative control ACKs sustain
        // the phone; a real migration reaches the 32 ms committed-progress
        // watchdog, releases input, and returns to discovery cleanly.
        let count = connection.read(&mut read_buffer, 4, !sink.has_active_input())?;
        if connection.take_session_changed() {
            ordered.require_fresh_session();
            cancel_sink_with_deadline(sink, metrics)?;
            control.hello_session = None;
            control.last_ack = None;
            control.last_committed_frame = Instant::now();
        }
        if count == 0 {
            continue;
        }
        let arrival = Instant::now();
        let decoded = parser.decode_datagram(&read_buffer[..count]);
        if let Some(version) = parser.take_incompatible_version() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "phone is sending protocol v{version}; install the matching protocol-v4 alpha APK"
                ),
            )
            .into());
        }
        process_decoded_frame(
            decoded,
            &mut connection,
            ordered,
            sink,
            metrics,
            &mut control,
            arrival,
        )?;
    }
    Ok(())
}

fn cancel_sink_with_deadline(sink: &mut Sink, metrics: &mut HostMetrics) -> io::Result<()> {
    cancel_with_deadline(sink, metrics)
}

fn process_decoded_frame(
    decoded: Result<TouchFrame, ProtocolError>,
    connection: &mut UdpConnection<'_>,
    ordered: &mut OrderedFrames,
    sink: &mut Sink,
    metrics: &mut HostMetrics,
    control_state: &mut ControlState,
    arrival: Instant,
) -> Result<(), Box<dyn Error>> {
    let frame = match decoded {
        Ok(frame) => frame,
        // Parser counters retain bounded diagnostics for the stop-time
        // report. Never perform per-packet logging on the live input path.
        Err(_) => return Ok(()),
    };
    connection.note_valid_peer_activity();
    let incoming_session = frame.session_id;
    let same_session = ordered.session_id() == Some(frame.session_id);
    let expected = ordered.expected_sequence();
    let replay =
        same_session && (frame.sequence < expected || ordered.contains_sequence(frame.sequence));
    if same_session && !replay && frame.sequence > expected {
        metrics.observe_gap(frame.session_id, expected, frame.sequence);
    }
    metrics.observe_received(&frame, arrival, replay);
    ordered.push(frame);

    if commit_ready(ordered, sink, metrics, &SHUTDOWN_REQUESTED)? {
        control_state.last_committed_frame = Instant::now();
    }

    let Some(session_id) = ordered.session_id() else {
        return Ok(());
    };
    if session_id != incoming_session {
        return Ok(());
    }
    let acknowledged = ordered.acknowledged_sequence();
    let control_type = if control_state.hello_session != Some(session_id) {
        control_state.hello_session = Some(session_id);
        CONTROL_HELLO
    } else {
        CONTROL_ACK
    };
    // Re-send an ACK for duplicates too; the previous host-to-phone control
    // record may be the part of the exchange that was lost.
    let outgoing_type = if control_type == CONTROL_HELLO || acknowledged != control_state.last_ack {
        control_state.last_ack = acknowledged;
        control_type
    } else {
        CONTROL_ACK
    };
    let host_send_nanos = metrics.clock_nanos(Instant::now());
    let control = encode_control(
        outgoing_type,
        control_state.lane_count,
        session_id,
        acknowledged,
        RECEIVE_WINDOW,
        host_send_nanos,
    );
    let ack_started = Instant::now();
    connection.write(&control, 4)?;
    metrics.observe_ack_write(ack_started.elapsed());
    if control_type == CONTROL_HELLO {
        println!(
            "Lossless stream ready (session {session_id:016x}, acknowledged={acknowledged:?})"
        );
        io::stdout().flush()?;
    }
    Ok(())
}

fn build_sink(options: &Options) -> Result<Sink, Box<dyn Error>> {
    match options.mode {
        #[cfg(windows)]
        Mode::Touch => {
            if options.spawn_probe && options.target_title == PROBE_WINDOW_TITLE {
                ensure_probe()?;
            }
            let target = TouchTarget::from_window_title(&options.target_title)?;
            println!(
                "Windows Touch target {:?}: {},{} {}x{}",
                options.target_title, target.left, target.top, target.width, target.height
            );
            Ok(Sink::Touch(Box::new(TouchInjector::new(target)?)))
        }
        Mode::Keys => Ok(Sink::Keys(Box::new(KeyboardSink::new(&options.lane_keys)?))),
        Mode::Record => Ok(Sink::Record),
    }
}

#[cfg(windows)]
fn ensure_probe() -> io::Result<()> {
    if TouchTarget::from_window_title(PROBE_WINDOW_TITLE).is_ok() {
        return Ok(());
    }
    let current = env::current_exe()?;
    let executable: PathBuf = current.with_file_name("holodori-touch-probe.exe");
    Command::new(&executable).spawn().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("could not start {}: {error}", executable.display()),
        )
    })?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if TouchTarget::from_window_title(PROBE_WINDOW_TITLE).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "touch probe did not create its window",
    ))
}

fn parse_options() -> Result<Options, Box<dyn Error>> {
    let mut options = Options {
        mode: default_mode(),
        lane_keys: ["s", "d", "f", "j", "k", "l"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        spawn_probe: true,
        target_title: PROBE_WINDOW_TITLE.to_owned(),
        udp_port: DEFAULT_UDP_PORT,
        metrics: false,
        metrics_file: None,
        warning_budget_ms: 1_000.0 / 120.0,
        local_only_tether: false,
        pair: false,
        legacy_v4: false,
        transport: None,
    };
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--mode" => {
                options.mode = match arguments.next().as_deref() {
                    #[cfg(windows)]
                    Some("touch") => Mode::Touch,
                    #[cfg(not(windows))]
                    Some("touch") => return Err("--mode touch is Windows-only".into()),
                    Some("keys") => Mode::Keys,
                    Some("record") => Mode::Record,
                    Some(value) => return Err(format!("unknown mode {value:?}").into()),
                    None => return Err("--mode needs touch, keys, or record".into()),
                };
            }
            "--lanes" => {
                let value = arguments.next().ok_or("--lanes needs a comma list")?;
                options.lane_keys = value
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .collect();
                if options.lane_keys.is_empty() || options.lane_keys.len() > 16 {
                    return Err("--lanes requires 1 to 16 keys".into());
                }
            }
            "--target-title" => {
                options.target_title = arguments.next().ok_or("--target-title needs a value")?;
                options.spawn_probe = false;
            }
            "--no-probe" => options.spawn_probe = false,
            "--udp-port" => {
                options.udp_port = arguments
                    .next()
                    .ok_or("--udp-port needs a value")?
                    .parse()?;
                if options.udp_port == 0 {
                    return Err("--udp-port must be between 1 and 65535".into());
                }
            }
            "--metrics" => options.metrics = true,
            "--pair" => options.pair = true,
            "--legacy-v4" => options.legacy_v4 = true,
            "--transport" => {
                let value = arguments.next().ok_or("--transport needs usb or wifi")?;
                options.transport = Some(
                    TransportKind::parse(&value)
                        .ok_or_else(|| format!("unknown transport {value:?}"))?,
                );
            }
            "--metrics-file" => {
                options.metrics = true;
                options.metrics_file = Some(PathBuf::from(
                    arguments.next().ok_or("--metrics-file needs a path")?,
                ));
            }
            "--local-only-tether" => {
                #[cfg(windows)]
                {
                    options.local_only_tether = true;
                }
                #[cfg(target_os = "linux")]
                {
                    options.local_only_tether = true;
                }
                #[cfg(not(any(windows, target_os = "linux")))]
                {
                    return Err("--local-only-tether is supported only on Windows and Linux".into());
                }
            }
            "--warn-ms" => {
                let milliseconds: f64 = arguments
                    .next()
                    .ok_or("--warn-ms needs milliseconds")?
                    .parse()?;
                if !milliseconds.is_finite() || milliseconds <= 0.0 {
                    return Err("--warn-ms must be positive".into());
                }
                options.warning_budget_ms = milliseconds;
            }
            "--help" | "-h" => {
                println!(
                    "holodori-native-host [--mode touch|keys|record] \\\n\
                     [--lanes s,d,f,j,k,l] [--target-title TITLE] [--no-probe] \\\n\
                     --transport usb|wifi [--pair] [--legacy-v4] \
                     [--udp-port 42825] [--metrics] [--local-only-tether] \
                     [--metrics-file PATH] [--warn-ms 8.333]"
                );
                std::process::exit(0);
            }
            value => return Err(format!("unknown option {value:?}; use --help").into()),
        }
    }
    if options.pair && options.legacy_v4 {
        return Err("--pair cannot be combined with --legacy-v4".into());
    }
    if options.legacy_v4 {
        if options
            .transport
            .is_some_and(|transport| transport != TransportKind::Usb)
        {
            return Err("protocol v4 is available only over explicit USB transport".into());
        }
        options.transport = Some(TransportKind::Usb);
    } else if options.transport.is_none() {
        return Err("protocol v5 requires --transport usb or --transport wifi".into());
    }
    if options.local_only_tether && options.transport != Some(TransportKind::Usb) {
        return Err("--local-only-tether requires --transport usb".into());
    }
    Ok(options)
}

fn idle_peer_timed_out(has_active_input: bool, peer_silence: Duration) -> bool {
    !has_active_input && peer_silence >= IDLE_PEER_SILENCE_TIMEOUT
}

#[cfg(windows)]
fn default_mode() -> Mode {
    Mode::Touch
}

#[cfg(not(windows))]
fn default_mode() -> Mode {
    Mode::Keys
}

#[cfg(test)]
mod tests {
    use super::{HostPhase, IDLE_PEER_SILENCE_TIMEOUT, idle_peer_timed_out};
    use std::time::Duration;

    #[test]
    fn status_tokens_are_stable() {
        assert_eq!(HostPhase::Waiting.token(), "HPT_STATUS WAITING");
        assert_eq!(HostPhase::Connected.token(), "HPT_STATUS CONNECTED");
        assert_eq!(HostPhase::Recovering.token(), "HPT_STATUS RECOVERING");
        assert_eq!(HostPhase::Stopping.token(), "HPT_STATUS STOPPING");
        assert_eq!(HostPhase::Pairing.token(), "HPT_STATUS PAIRING");
    }

    #[test]
    fn legacy_idle_timeout_never_replaces_active_progress_watchdog() {
        assert!(!idle_peer_timed_out(
            false,
            IDLE_PEER_SILENCE_TIMEOUT - Duration::from_millis(1),
        ));
        assert!(idle_peer_timed_out(false, IDLE_PEER_SILENCE_TIMEOUT));
        assert!(!idle_peer_timed_out(
            true,
            IDLE_PEER_SILENCE_TIMEOUT + Duration::from_secs(1),
        ));
    }
}
