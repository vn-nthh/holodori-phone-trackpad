use std::env;
use std::error::Error;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use holodori_native_host::keyboard::KeyboardSink;
use holodori_native_host::metrics::HostMetrics;
use holodori_native_host::network::{DEFAULT_UDP_PORT, UdpConnection, UdpHost};
use holodori_native_host::protocol::{
    CONTROL_ACK, CONTROL_HELLO, FrameParser, OrderedFrames, TouchFrame, encode_control,
};
use holodori_native_host::touch::{PROBE_WINDOW_TITLE, TouchInjector, TouchTarget};
use windows_sys::Win32::System::Console::{
    CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    SetConsoleCtrlHandler,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, HIGH_PRIORITY_CLASS, SetPriorityClass, SetThreadPriority,
    THREAD_PRIORITY_HIGHEST,
};

const RECEIVE_WINDOW: u32 = 128;
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
static SHUTDOWN_COMPLETE: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug)]
enum Mode {
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
}

enum Sink {
    Touch(TouchInjector),
    Keys(KeyboardSink),
    Record,
}

impl Sink {
    fn lane_count(&self, options: &Options) -> u8 {
        match self {
            Self::Keys(keys) => keys.lane_count(),
            _ => options.lane_keys.len().min(u8::MAX as usize) as u8,
        }
    }

    fn accept(&mut self, frame: &TouchFrame) -> io::Result<()> {
        match self {
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
}

fn main() {
    if let Err(error) = run() {
        eprintln!("fatal: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    raise_input_priority();
    install_shutdown_handler()?;
    let options = parse_options()?;
    let mut sink = build_sink(&options)?;
    let lane_count = sink.lane_count(&options);
    let udp = UdpHost::bind(options.udp_port)?;
    let mut metrics = HostMetrics::new(options.metrics, options.warning_budget_ms);
    // The launcher owns the host's stdin and sends `q` for a graceful stop.
    // Keep this available even when the user turns report writing off so the
    // UI never has to terminate the process and risk held input.
    install_exit_command_thread()?;

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
        println!(
            "waiting for USB-tethered phone on UDP port {}...",
            udp.port()
        );
        // Keep graceful Q/Ctrl+C shutdown bounded even while no phone is
        // present. The outer loop retries, so a one-second discovery slice is
        // sufficient without making exit wait for the old 15-second timeout.
        let connection = match udp.connect(Duration::from_secs(1)) {
            Ok(connection) => connection,
            Err(error) => {
                eprintln!("{error}; retrying");
                thread::sleep(Duration::from_millis(250));
                continue;
            }
        };
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
            eprintln!("UDP link interrupted: {error}; preserving host ordering state");
            thread::sleep(Duration::from_millis(100));
        }
    }

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
    SHUTDOWN_COMPLETE.store(true, Ordering::Release);
    report_result.map_err(Into::into)
}

fn install_shutdown_handler() -> io::Result<()> {
    if unsafe { SetConsoleCtrlHandler(Some(console_control_handler), 1) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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
        })
        .map(|_| ())
}

unsafe extern "system" fn console_control_handler(event: u32) -> i32 {
    match event {
        CTRL_C_EVENT | CTRL_BREAK_EVENT => {
            SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
            1
        }
        CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => {
            SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
            let deadline = Instant::now() + Duration::from_secs(4);
            while !SHUTDOWN_COMPLETE.load(Ordering::Acquire) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            1
        }
        _ => 0,
    }
}

fn default_metrics_path() -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let directory = env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Logs");
    directory.join(format!("holodori-metrics-{timestamp}.txt"))
}

fn raise_input_priority() {
    let process_ok = unsafe { SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS) } != 0;
    let thread_ok = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) } != 0;
    if !process_ok || !thread_ok {
        eprintln!("warning: Windows did not grant the requested high input priority");
    }
}

trait HostConnection {
    fn read(&mut self, buffer: &mut [u8], timeout_ms: u32) -> io::Result<usize>;
    fn write(&mut self, bytes: &[u8], timeout_ms: u32) -> io::Result<()>;
    fn is_datagram(&self) -> bool {
        false
    }
}

impl HostConnection for UdpConnection<'_> {
    fn read(&mut self, buffer: &mut [u8], timeout_ms: u32) -> io::Result<usize> {
        UdpConnection::read(self, buffer, timeout_ms)
    }

    fn write(&mut self, bytes: &[u8], timeout_ms: u32) -> io::Result<()> {
        UdpConnection::write(self, bytes, timeout_ms)
    }

    fn is_datagram(&self) -> bool {
        true
    }
}

fn serve_connection<C: HostConnection>(
    mut connection: C,
    parser: &mut FrameParser,
    ordered: &mut OrderedFrames,
    sink: &mut Sink,
    metrics: &mut HostMetrics,
    lane_count: u8,
) -> Result<(), Box<dyn Error>> {
    let mut read_buffer = [0_u8; 4096];
    let mut hello_session = None;
    let mut last_ack = None;

    while !SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
        let count = connection.read(&mut read_buffer, 4)?;
        if count == 0 {
            continue;
        }
        let arrival = Instant::now();
        let decoded_frames = if connection.is_datagram() {
            parser.feed_datagram(&read_buffer[..count])
        } else {
            parser.feed(&read_buffer[..count])
        };
        if let Some(version) = parser.take_incompatible_version() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "phone is sending protocol v{version}; install the matching protocol-v4 alpha APK"
                ),
            )
            .into());
        }
        for decoded in decoded_frames {
            let frame = match decoded {
                Ok(frame) => frame,
                Err(error) => {
                    eprintln!("wire frame rejected: {error}; waiting for replay");
                    continue;
                }
            };
            let incoming_session = frame.session_id;
            let same_session = ordered.session_id() == Some(frame.session_id);
            let expected = ordered.expected_sequence();
            let replay = same_session
                && (frame.sequence < expected || ordered.contains_sequence(frame.sequence));
            if same_session && !replay && frame.sequence > expected {
                metrics.observe_gap(frame.session_id, expected, frame.sequence);
            }
            metrics.observe_received(&frame, arrival, replay);
            ordered.push(frame);

            while let Some(frame) = ordered.next_ready().cloned() {
                let mut last_error_report = Instant::now() - Duration::from_secs(2);
                loop {
                    match sink.accept(&frame) {
                        Ok(()) => break,
                        Err(error) => {
                            metrics.observe_sink_retry();
                            if last_error_report.elapsed() >= Duration::from_secs(1) {
                                eprintln!(
                                    "OS sink has not accepted seq {}: {}; withholding ACK and retrying",
                                    frame.sequence, error
                                );
                                last_error_report = Instant::now();
                            }
                            thread::sleep(Duration::from_millis(1));
                        }
                    }
                }
                metrics.observe_accepted(&frame, Instant::now());
                // This commit is the protocol's durability boundary and must
                // execute in optimized builds. Never hide side effects inside
                // debug_assert!, which release compilation removes entirely.
                if !ordered.commit_ready() {
                    return Err(io::Error::other(format!(
                        "could not commit accepted sequence {}",
                        frame.sequence
                    ))
                    .into());
                }
            }

            let Some(session_id) = ordered.session_id() else {
                continue;
            };
            if session_id != incoming_session {
                continue;
            }
            let acknowledged = ordered.acknowledged_sequence();
            let control_type = if hello_session != Some(session_id) {
                hello_session = Some(session_id);
                CONTROL_HELLO
            } else {
                CONTROL_ACK
            };
            // Re-send an ACK for duplicates too; the previous host-to-phone
            // control record may be the part of the exchange that was lost.
            let outgoing_type = if control_type == CONTROL_HELLO || acknowledged != last_ack {
                last_ack = acknowledged;
                control_type
            } else {
                CONTROL_ACK
            };
            let host_send_nanos = metrics.clock_nanos(Instant::now());
            let control = encode_control(
                outgoing_type,
                lane_count,
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
        }
    }
    Ok(())
}

fn build_sink(options: &Options) -> Result<Sink, Box<dyn Error>> {
    match options.mode {
        Mode::Touch => {
            if options.spawn_probe && options.target_title == PROBE_WINDOW_TITLE {
                ensure_probe()?;
            }
            let target = TouchTarget::from_window_title(&options.target_title)?;
            println!(
                "Windows Touch target {:?}: {},{} {}x{}",
                options.target_title, target.left, target.top, target.width, target.height
            );
            Ok(Sink::Touch(TouchInjector::new(target)?))
        }
        Mode::Keys => Ok(Sink::Keys(KeyboardSink::new(&options.lane_keys)?)),
        Mode::Record => Ok(Sink::Record),
    }
}

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
        mode: Mode::Touch,
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
    };
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--mode" => {
                options.mode = match arguments.next().as_deref() {
                    Some("touch") => Mode::Touch,
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
            "--metrics-file" => {
                options.metrics = true;
                options.metrics_file = Some(PathBuf::from(
                    arguments.next().ok_or("--metrics-file needs a path")?,
                ));
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
                     [--udp-port 42825] [--metrics] \
                     [--metrics-file PATH] [--warn-ms 8.333]"
                );
                std::process::exit(0);
            }
            value => return Err(format!("unknown option {value:?}; use --help").into()),
        }
    }
    Ok(options)
}
