#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use holodori_native_host::tether_policy::{recover_orphaned_policy, RecoveryOutcome};
use serde::Serialize;
use std::io::{BufRead, BufReader, Write};
use std::net::UdpSocket;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use tauri::{AppHandle, Manager, State};

const DEFAULT_WARNING_BUDGET_MS: &str = "8.333";
const USB_TETHER_UDP_PORT: u16 = 42_825;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum HostPhase {
    #[default]
    Ready,
    Waiting,
    Connected,
    Recovering,
    Stopping,
    RecoveryNeedsAdmin,
    Fatal,
}

impl HostPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Waiting => "waiting",
            Self::Connected => "connected",
            Self::Recovering => "recovering",
            Self::Stopping => "stopping",
            Self::RecoveryNeedsAdmin => "recovery-needs-admin",
            Self::Fatal => "fatal",
        }
    }

    fn default_message(self) -> &'static str {
        match self {
            Self::Ready => "Ready",
            Self::Waiting => "Waiting for phone...",
            Self::Connected => "Phone connected",
            Self::Recovering => "Connection lost — recovering...",
            Self::Stopping => "Stopping safely...",
            Self::RecoveryNeedsAdmin => {
                "Administrator access is required to recover USB-tether routes."
            }
            Self::Fatal => "The controller stopped unexpectedly.",
        }
    }
}

struct HostRuntime {
    child: Child,
    output: Arc<Mutex<HostOutput>>,
    readers: Vec<JoinHandle<()>>,
    local_only_tether: bool,
}

#[derive(Default)]
struct HostOutput {
    phase: Option<HostPhase>,
    fatal_message: Option<String>,
}

#[derive(Default)]
struct HostState {
    runtime: Option<HostRuntime>,
    phase: HostPhase,
    stopping: bool,
    message: String,
    fatal_message: Option<String>,
    recovery_needs_admin: bool,
}

#[derive(Debug, Serialize)]
struct HostStatus {
    running: bool,
    stopping: bool,
    phase: String,
    message: String,
    recovery_needs_admin: bool,
}

#[tauri::command]
fn start_host(
    state: State<'_, Mutex<HostState>>,
    keys: String,
    metrics: bool,
    local_only_tether: bool,
) -> Result<HostStatus, String> {
    let mut state = state
        .lock()
        .map_err(|_| "controller state is unavailable")?;
    reap_child(&mut state)?;
    if state.runtime.is_some() {
        return Ok(status(&state));
    }
    ensure_route_recovery(&mut state)?;

    let host = find_host().ok_or_else(|| {
        "The Windows controller is missing. Build native-host or re-extract the portable bundle."
            .to_owned()
    })?;
    UdpSocket::bind(("0.0.0.0", USB_TETHER_UDP_PORT)).map_err(|error| {
        format!(
            "UDP port {USB_TETHER_UDP_PORT} is already in use. Close any older Holodori host in Task Manager, then try again: {error}"
        )
    })?;
    let mut command = Command::new(&host);
    command
        .current_dir(host.parent().unwrap_or_else(|| std::path::Path::new(".")))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .arg("--mode")
        .arg("keys")
        .arg("--lanes")
        .arg(keys)
        .arg("--udp-port")
        .arg(USB_TETHER_UDP_PORT.to_string())
        .arg("--warn-ms")
        .arg(DEFAULT_WARNING_BUDGET_MS);
    if metrics {
        command.arg("--metrics");
    }
    if local_only_tether {
        command.arg("--local-only-tether");
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = command
        .spawn()
        .map_err(|error| format!("Could not start the controller: {error}"))?;
    let output = Arc::new(Mutex::new(HostOutput::default()));
    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        let output = Arc::clone(&output);
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(phase) = parse_status_token(&line) {
                    if let Ok(mut latest) = output.lock() {
                        latest.phase = Some(phase);
                    }
                }
            }
        }));
    }
    if let Some(stderr) = child.stderr.take() {
        let output = Arc::clone(&output);
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Some(message) = line.strip_prefix("fatal: ") {
                    if let Ok(mut latest) = output.lock() {
                        latest.fatal_message = Some(message.to_owned());
                    }
                }
            }
        }));
    }

    state.runtime = Some(HostRuntime {
        child,
        output,
        readers,
        local_only_tether,
    });
    state.phase = HostPhase::Waiting;
    state.stopping = false;
    state.message.clear();
    state.fatal_message = None;
    state.recovery_needs_admin = false;
    Ok(status(&state))
}

#[tauri::command]
fn restart_as_admin(app: AppHandle) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        let executable = std::env::current_exe()
            .map_err(|error| format!("Could not find the launcher executable: {error}"))?;
        let file: Vec<u16> = executable
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let verb: Vec<u16> = OsStr::new("runas")
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let result = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                file.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        if (result as usize) <= 32 {
            return Err(
                "Could not restart as admin. Approve the UAC prompt and try again.".to_owned(),
            );
        }
        app.exit(0);
        Ok(())
    }

    #[cfg(not(windows))]
    {
        let _ = app;
        Err("Restart as admin is only available on Windows.".to_owned())
    }
}

#[tauri::command]
fn launcher_is_elevated() -> Result<bool, String> {
    #[cfg(windows)]
    {
        use std::mem::size_of;
        use std::ptr::null_mut;
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::Security::{
            GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        let mut token: HANDLE = null_mut();
        let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
        if opened == 0 {
            return Err(format!(
                "Could not inspect launcher elevation: {}",
                std::io::Error::last_os_error()
            ));
        }

        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned_length = 0_u32;
        let queried = unsafe {
            GetTokenInformation(
                token,
                TokenElevation,
                (&mut elevation as *mut TOKEN_ELEVATION).cast(),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut returned_length,
            )
        };
        let query_error = if queried == 0 {
            Some(std::io::Error::last_os_error())
        } else {
            None
        };
        unsafe { CloseHandle(token) };

        if let Some(error) = query_error {
            Err(format!("Could not inspect launcher elevation: {error}"))
        } else {
            Ok(elevation.TokenIsElevated != 0)
        }
    }

    #[cfg(not(windows))]
    {
        Ok(false)
    }
}

#[tauri::command]
fn stop_host(state: State<'_, Mutex<HostState>>) -> Result<HostStatus, String> {
    let mut state = state
        .lock()
        .map_err(|_| "controller state is unavailable")?;
    reap_child(&mut state)?;
    let Some(runtime) = state.runtime.as_mut() else {
        return Ok(status(&state));
    };
    if let Some(stdin) = runtime.child.stdin.as_mut() {
        stdin
            .write_all(b"q\n")
            .and_then(|_| stdin.flush())
            .map_err(|error| format!("Could not stop the controller safely: {error}"))?;
    }
    state.stopping = true;
    state.phase = HostPhase::Stopping;
    state.message.clear();
    Ok(status(&state))
}

#[tauri::command]
fn host_status(state: State<'_, Mutex<HostState>>) -> Result<HostStatus, String> {
    let mut state = state
        .lock()
        .map_err(|_| "controller state is unavailable")?;
    reap_child(&mut state)?;
    Ok(status(&state))
}

fn status(state: &HostState) -> HostStatus {
    HostStatus {
        running: state.runtime.is_some(),
        stopping: state.stopping,
        phase: state.phase.as_str().to_owned(),
        message: if state.message.is_empty() {
            state.phase.default_message().to_owned()
        } else {
            state.message.clone()
        },
        recovery_needs_admin: state.recovery_needs_admin,
    }
}

fn reap_child(state: &mut HostState) -> Result<(), String> {
    drain_runtime_events(state);
    let Some(runtime) = state.runtime.as_mut() else {
        return Ok(());
    };
    if let Some(exit_status) = runtime
        .child
        .try_wait()
        .map_err(|error| format!("Could not check controller state: {error}"))?
    {
        let mut runtime = state.runtime.take().expect("runtime checked above");
        for reader in runtime.readers.drain(..) {
            let _ = reader.join();
        }
        apply_output(state, &runtime.output);
        let expected_stop = state.stopping;
        let local_only_tether = runtime.local_only_tether;
        drop(runtime);

        if expected_stop && exit_status.success() {
            state.phase = HostPhase::Ready;
            state.message.clear();
            state.fatal_message = None;
        } else {
            state.phase = HostPhase::Fatal;
            state.message = state
                .fatal_message
                .clone()
                .map(user_facing_native_error)
                .unwrap_or_else(|| {
                    if local_only_tether && expected_stop {
                        "The controller did not stop cleanly while local-only tethering was enabled."
                            .to_owned()
                    } else if expected_stop {
                        "The controller did not stop cleanly. Try starting it again.".to_owned()
                    } else {
                        "The controller stopped unexpectedly. Try starting it again.".to_owned()
                    }
                });
        }
        state.stopping = false;
        let _ = ensure_route_recovery(state);
    }
    Ok(())
}

fn drain_runtime_events(state: &mut HostState) {
    let Some(runtime) = state.runtime.as_ref() else {
        return;
    };
    let output = Arc::clone(&runtime.output);
    apply_output(state, &output);
}

fn apply_output(state: &mut HostState, output: &Arc<Mutex<HostOutput>>) {
    let (phase, fatal_message) = match output.lock() {
        Ok(mut output) => (output.phase.take(), output.fatal_message.take()),
        Err(_) => return,
    };
    if !state.stopping {
        if let Some(phase) = phase {
            state.phase = phase;
            state.message.clear();
        }
    }
    if let Some(message) = fatal_message {
        state.phase = HostPhase::Fatal;
        state.fatal_message = Some(message.clone());
        state.message = user_facing_native_error(message);
    }
}

fn parse_status_token(line: &str) -> Option<HostPhase> {
    match line {
        "HPT_STATUS WAITING" => Some(HostPhase::Waiting),
        "HPT_STATUS CONNECTED" => Some(HostPhase::Connected),
        "HPT_STATUS RECOVERING" => Some(HostPhase::Recovering),
        "HPT_STATUS STOPPING" => Some(HostPhase::Stopping),
        _ => None,
    }
}

fn ensure_route_recovery(state: &mut HostState) -> Result<(), String> {
    match recover_orphaned_policy() {
        Ok(RecoveryOutcome::NothingToDo | RecoveryOutcome::Restored { .. }) => {
            state.recovery_needs_admin = false;
            if state.phase == HostPhase::RecoveryNeedsAdmin {
                state.phase = HostPhase::Ready;
                state.message.clear();
            }
            Ok(())
        }
        Ok(RecoveryOutcome::OwnerStillRunning) => {
            let message =
                "A previous controller still owns USB-tether route settings. Close it first."
                    .to_owned();
            state.phase = HostPhase::Fatal;
            state.message = message.clone();
            Err(message)
        }
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            let message =
                format!("Administrator access is required to recover USB-tether routes: {error}");
            state.phase = HostPhase::RecoveryNeedsAdmin;
            state.message = message.clone();
            state.recovery_needs_admin = true;
            Err(message)
        }
        Err(error) => {
            let message = format!("Could not recover USB-tether routes safely: {error}");
            state.phase = HostPhase::Fatal;
            state.message = message.clone();
            Err(message)
        }
    }
}

fn user_facing_native_error(message: String) -> String {
    if message.contains("recognized Android/RNDIS adapter")
        || message.contains("not on an unambiguous Android/RNDIS subnet")
    {
        "The phone was discovered outside USB tethering. Enable USB tethering and try again."
            .to_owned()
    } else {
        message
    }
}

fn find_host() -> Option<PathBuf> {
    let base = std::env::current_exe().ok()?.parent()?.to_owned();
    let mut candidates = vec![
        base.join("Windows").join("holodori-native-host.exe"),
        base.join("holodori-native-host.exe"),
    ];
    if let Some(workspace) = base
        .ancestors()
        .find(|path| path.join("native-host").join("Cargo.toml").is_file())
    {
        let native_target = workspace.join("native-host").join("target");
        candidates.push(
            native_target
                .join("release")
                .join("holodori-native-host.exe"),
        );
        candidates.push(native_target.join("debug").join("holodori-native-host.exe"));
    }
    candidates.into_iter().find(|path| path.is_file())
}

fn main() {
    tauri::Builder::default()
        .manage(Mutex::new(HostState::default()))
        .setup(|app| {
            if let Ok(mut state) = app.state::<Mutex<HostState>>().lock() {
                let _ = ensure_route_recovery(&mut state);
            }
            #[cfg(windows)]
            if let Some(window) = app.get_webview_window("main") {
                style_windows_titlebar(&window);
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            start_host,
            stop_host,
            host_status,
            restart_as_admin,
            launcher_is_elevated
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                let _ = window
                    .state::<Mutex<HostState>>()
                    .lock()
                    .map(|mut state| request_stop(&mut state));
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running Holodori USB Controller");
}

#[cfg(windows)]
fn style_windows_titlebar(window: &tauri::WebviewWindow) {
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::ptr::null_mut;
    use windows_sys::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SendMessageW, SetClassLongPtrW, SetWindowLongPtrW, SetWindowPos,
        GCLP_HICON, GCLP_HICONSM, GWL_EXSTYLE, ICON_BIG, ICON_SMALL, SWP_FRAMECHANGED, SWP_NOMOVE,
        SWP_NOSIZE, SWP_NOZORDER, WM_SETICON, WS_EX_DLGMODALFRAME,
    };

    let Ok(hwnd) = window.hwnd() else {
        return;
    };
    let hwnd = hwnd.0;

    unsafe {
        // Keep the native minimize/maximize/close buttons while suppressing
        // the app icon in the caption.
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex_style | WS_EX_DLGMODALFRAME as isize);

        // Remove the executable icon from both caption slots without
        // removing the native minimize/maximize/close actions.
        let _ = SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, 0);
        let _ = SendMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, 0);
        let _ = SetClassLongPtrW(hwnd, GCLP_HICON, 0);
        let _ = SetClassLongPtrW(hwnd, GCLP_HICONSM, 0);

        // Match the native caption and border to the blackout app surface.
        let dark_mode: i32 = 1;
        let black: u32 = 0;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            &dark_mode as *const _ as *const c_void,
            size_of::<i32>() as u32,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_COLOR as u32,
            &black as *const _ as *const c_void,
            size_of::<u32>() as u32,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR as u32,
            &black as *const _ as *const c_void,
            size_of::<u32>() as u32,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_TEXT_COLOR as u32,
            &black as *const _ as *const c_void,
            size_of::<u32>() as u32,
        );
        let _ = SetWindowPos(
            hwnd,
            null_mut(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
        );
    }
}

fn request_stop(state: &mut HostState) -> Result<(), String> {
    let Some(runtime) = state.runtime.as_mut() else {
        return Ok(());
    };
    if let Some(stdin) = runtime.child.stdin.as_mut() {
        stdin
            .write_all(b"q\n")
            .and_then(|_| stdin.flush())
            .map_err(|error| format!("Could not stop the controller safely: {error}"))?;
    }
    state.stopping = true;
    state.phase = HostPhase::Stopping;
    state.message.clear();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse_status_token, user_facing_native_error, HostPhase};

    #[test]
    fn parses_only_stable_status_tokens() {
        assert_eq!(
            parse_status_token("HPT_STATUS WAITING"),
            Some(HostPhase::Waiting),
        );
        assert_eq!(
            parse_status_token("HPT_STATUS CONNECTED"),
            Some(HostPhase::Connected),
        );
        assert_eq!(
            parse_status_token("HPT_STATUS RECOVERING"),
            Some(HostPhase::Recovering),
        );
        assert_eq!(
            parse_status_token("HPT_STATUS STOPPING"),
            Some(HostPhase::Stopping),
        );
        assert_eq!(parse_status_token("UDP link ready"), None);
        assert_eq!(parse_status_token("HPT_STATUS CONNECTED extra"), None);
    }

    #[test]
    fn keeps_native_fatal_detail_for_the_launcher() {
        assert_eq!(
            user_facing_native_error("route restore failed".to_owned()),
            "route restore failed",
        );
    }
}
