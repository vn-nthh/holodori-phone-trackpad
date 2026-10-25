//! OS-specific process hooks used by the `holodori-native-host` binary:
//! graceful shutdown signal handling and best-effort input scheduling
//! priority. Kept out of `bin/host.rs` so the binary's control flow stays
//! platform-neutral.

use std::io;
use std::sync::atomic::AtomicBool;

/// Setup/stop-time readback. A successful request is not proof of scheduler,
/// driver, AP QoS, or physical latency. No environmental queries per input.
#[cfg(windows)]
pub fn diagnostic_environment() -> String {
    use windows_sys::Win32::System::Threading::*;
    let mut state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ..Default::default()
    };
    let ok = unsafe {
        GetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            std::ptr::from_mut(&mut state).cast(),
            std::mem::size_of_val(&state) as u32,
        )
    };
    let priority = unsafe { GetPriorityClass(GetCurrentProcess()) };
    let thread = unsafe { GetThreadPriority(GetCurrentThread()) };
    format!(
        "os=windows process_priority_readback={priority} input_thread_priority_readback={thread} qos_query_ok={} qos_control_mask={} qos_state_mask={} timer_period_1ms_granted={} socket_1ms_timeout_us={} thermal=unavailable nic_rssi=unavailable dscp=not_requested ap_wmm=unverified",
        ok != 0,
        state.ControlMask,
        state.StateMask,
        TIMER_PERIOD_GRANTED.load(std::sync::atomic::Ordering::Relaxed),
        socket_timeout_probe_us(),
    )
}

#[cfg(not(windows))]
pub fn diagnostic_environment() -> String {
    format!(
        "os={} priority_readback=unavailable socket_1ms_timeout_us={} thermal=unavailable nic_rssi=unavailable dscp={} ap_wmm=unverified",
        std::env::consts::OS,
        socket_timeout_probe_us(),
        if cfg!(target_os = "linux") {
            "ef_requested_v5"
        } else {
            "not_requested"
        },
    )
}

/// Setup-time evidence of how long a 1 ms socket read timeout really takes.
/// Gameplay watchdogs and deferred key edges wake through such timeouts, so
/// a coarse OS timer stretches them. Median of three loopback waits, or -1
/// when the probe socket is unavailable. Never runs per input.
fn socket_timeout_probe_us() -> i64 {
    use std::time::{Duration, Instant};
    let Ok(socket) = std::net::UdpSocket::bind(("127.0.0.1", 0)) else {
        return -1;
    };
    if socket
        .set_read_timeout(Some(Duration::from_millis(1)))
        .is_err()
    {
        return -1;
    }
    let mut buffer = [0_u8; 1];
    let mut samples = [0_i64; 3];
    for sample in &mut samples {
        let started = Instant::now();
        let _ = socket.recv(&mut buffer);
        *sample = i64::try_from(started.elapsed().as_micros()).unwrap_or(i64::MAX);
    }
    samples.sort_unstable();
    samples[1]
}

#[cfg(windows)]
static TIMER_PERIOD_GRANTED: AtomicBool = AtomicBool::new(false);

/// Holds a 1 ms Windows timer period while the controller runs.
///
/// Socket read timeouts drive the active-input watchdog, HELLO repair, and
/// deferred key edges. At the default 15.6 ms tick a "4 ms" timeout can take
/// a whole tick, stretching the 32 ms stuck-key watchdog toward 48 ms.
/// Received datagrams wake the input thread immediately either way.
pub struct TimerResolution {
    #[cfg(windows)]
    granted: bool,
}

#[cfg(windows)]
pub fn request_timer_resolution() -> TimerResolution {
    use std::sync::atomic::Ordering;
    use windows_sys::Win32::Media::{TIMERR_NOERROR, timeBeginPeriod};
    let granted = unsafe { timeBeginPeriod(1) } == TIMERR_NOERROR;
    if !granted {
        eprintln!("warning: Windows did not grant a 1 ms timer period");
    }
    TIMER_PERIOD_GRANTED.store(granted, Ordering::Relaxed);
    TimerResolution { granted }
}

/// Linux socket timeouts already use high-resolution timers.
#[cfg(not(windows))]
pub fn request_timer_resolution() -> TimerResolution {
    TimerResolution {}
}

#[cfg(windows)]
impl Drop for TimerResolution {
    fn drop(&mut self) {
        if self.granted {
            unsafe { windows_sys::Win32::Media::timeEndPeriod(1) };
            TIMER_PERIOD_GRANTED.store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

pub fn lower_diagnostic_priority() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Threading::*;
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST);
    }
    #[cfg(target_os = "linux")]
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
    }
}

/// Installs a shutdown signal handler.
///
/// `requested` is set as soon as a shutdown signal arrives; the main loop
/// polls it and performs cleanup (releasing held input, writing metrics)
/// itself before exiting normally. `complete` is set by the caller once that
/// cleanup has finished.
///
/// On Windows, `CTRL_CLOSE`/`CTRL_LOGOFF`/`CTRL_SHUTDOWN` additionally block
/// inside the console control callback for up to 4 seconds waiting for
/// `complete`, because Windows may terminate the process as soon as the
/// callback returns and a rhythm game cannot tolerate a key staying held
/// after that. On Linux, terminating signals don't have that immediate-kill
/// behavior once a handler is installed, so `complete` is unused there.
#[cfg(windows)]
pub fn install_shutdown_handler(
    requested: &'static AtomicBool,
    complete: &'static AtomicBool,
) -> io::Result<()> {
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
        SetConsoleCtrlHandler,
    };

    static REQUESTED: std::sync::OnceLock<&'static AtomicBool> = std::sync::OnceLock::new();
    static COMPLETE: std::sync::OnceLock<&'static AtomicBool> = std::sync::OnceLock::new();
    REQUESTED.set(requested).ok();
    COMPLETE.set(complete).ok();

    unsafe extern "system" fn console_control_handler(event: u32) -> i32 {
        let Some(requested) = REQUESTED.get() else {
            return 0;
        };
        match event {
            CTRL_C_EVENT | CTRL_BREAK_EVENT => {
                requested.store(true, Ordering::Relaxed);
                1
            }
            CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => {
                requested.store(true, Ordering::Relaxed);
                if let Some(complete) = COMPLETE.get() {
                    let deadline = Instant::now() + Duration::from_secs(4);
                    while !complete.load(Ordering::Acquire) && Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(10));
                    }
                }
                1
            }
            _ => 0,
        }
    }

    if unsafe { SetConsoleCtrlHandler(Some(console_control_handler), 1) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn install_shutdown_handler(
    requested: &'static AtomicBool,
    _complete: &'static AtomicBool,
) -> io::Result<()> {
    use std::mem::zeroed;
    use std::sync::atomic::{AtomicPtr, Ordering};

    static REQUESTED: AtomicPtr<AtomicBool> = AtomicPtr::new(std::ptr::null_mut());
    REQUESTED.store(std::ptr::from_ref(requested).cast_mut(), Ordering::Release);

    extern "C" fn handle_signal(_signal: libc::c_int) {
        // Only atomic operations: this runs as a signal handler, and nothing
        // else is guaranteed async-signal-safe. The pointer targets the
        // caller-provided `static` AtomicBool and therefore remains valid for
        // the lifetime of every installed handler.
        let requested = REQUESTED.load(Ordering::Acquire);
        if !requested.is_null() {
            // SAFETY: `install_shutdown_handler` stores a pointer obtained
            // from an explicit `&'static AtomicBool` before installing any
            // handler, and never frees or mutates the pointed-to allocation.
            unsafe { (*requested).store(true, Ordering::Relaxed) };
        }
    }

    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        let mut action: libc::sigaction = unsafe { zeroed() };
        action.sa_sigaction = handle_signal as *const () as usize;
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        action.sa_flags = 0;
        if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Best-effort input priority and HighQoS for the windowless host process.
/// Failure must not prevent play; these are scheduling requests, not guarantees.
#[cfg(windows)]
pub fn raise_input_priority() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentThread, HIGH_PRIORITY_CLASS, SetPriorityClass,
        SetThreadPriority, THREAD_PRIORITY_HIGHEST,
    };
    let process_ok = unsafe { SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS) } != 0;
    let thread_ok = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) } != 0;
    if !process_ok || !thread_ok {
        eprintln!("warning: Windows did not grant the requested high input priority");
    }
    if let Err(error) = request_high_qos() {
        eprintln!("warning: Windows did not grant input HighQoS: {error}");
    }
}

#[cfg(windows)]
fn request_high_qos() -> io::Result<()> {
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling, SetProcessInformation,
    };
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
            | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        StateMask: 0,
    };
    // Opt out of execution-speed throttling and keep Windows 11 honoring this
    // windowless process's timer-period request (`request_timer_resolution`),
    // without changing affinity or the user's system-wide power configuration.
    let ok = unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            std::ptr::from_ref(&state).cast(),
            std::mem::size_of_val(&state) as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn raise_input_priority() {
    // Deliberately not SCHED_FIFO: that needs root and would make the common
    // case (an unprivileged user running the host) noisy for no benefit.
    //
    // A negative nice value needs `CAP_SYS_NICE` or a raised `RLIMIT_NICE`,
    // neither of which an unprivileged user has on stock distros, so
    // `EACCES`/`EPERM` here is the expected, common outcome for most Linux
    // users, not a problem to report on every single launch. Any other errno
    // is unexpected and still worth a warning.
    let result = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, -10) };
    if result == -1 {
        let error = io::Error::last_os_error();
        if !matches!(error.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM)) {
            eprintln!("warning: the OS did not grant the requested high input priority: {error}");
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn high_qos_is_applied_to_the_host_process() {
        const CHILD: &str = "DORITRACK_HIGH_QOS_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // Process-wide priority must not affect other parallel tests.
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "platform::tests::high_qos_is_applied_to_the_host_process",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, GetProcessInformation, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
            ProcessPowerThrottling,
        };
        super::raise_input_priority();
        let mut state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ..Default::default()
        };
        let ok = unsafe {
            GetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                std::ptr::from_mut(&mut state).cast(),
                std::mem::size_of_val(&state) as u32,
            )
        };
        assert_ne!(ok, 0, "{}", std::io::Error::last_os_error());
        assert_ne!(
            state.ControlMask & PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            0
        );
        assert_eq!(
            state.StateMask & PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            0
        );
        assert_ne!(
            state.ControlMask & PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            0
        );
        assert_eq!(
            state.StateMask & PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            0
        );
    }

    #[test]
    fn timer_period_request_is_scoped_to_its_guard() {
        use std::sync::atomic::Ordering;
        let guard = super::request_timer_resolution();
        assert!(super::TIMER_PERIOD_GRANTED.load(Ordering::Relaxed));
        drop(guard);
        assert!(!super::TIMER_PERIOD_GRANTED.load(Ordering::Relaxed));
    }
}
