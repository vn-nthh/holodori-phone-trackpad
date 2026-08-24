//! OS-specific process hooks used by the `holodori-native-host` binary:
//! graceful shutdown signal handling and best-effort input scheduling
//! priority. Kept out of `bin/host.rs` so the binary's control flow stays
//! platform-neutral.

use std::io;
use std::sync::atomic::AtomicBool;

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

/// Best-effort attempt to raise this process's/thread's scheduling priority
/// so input injection is not delayed behind unrelated desktop work. Never
/// fails outright; a denial is common (e.g. no elevated privileges) and must
/// not block the tool from running.
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
