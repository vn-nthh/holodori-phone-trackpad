#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(target_os = "linux")]
mod linux_network_manager;

#[cfg(windows)]
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
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum HostPhase {
    #[default]
    Ready,
    Waiting,
    Connected,
    Recovering,
    Stopping,
    Pairing,
    #[cfg(windows)]
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
            Self::Pairing => "pairing",
            #[cfg(windows)]
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
            Self::Pairing => "Pairing window open...",
            #[cfg(windows)]
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
    kind: RuntimeKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeKind {
    Controller,
    Pairing,
}

#[derive(Default)]
struct HostOutput {
    phase: Option<HostPhase>,
    fatal_message: Option<String>,
    pattern: Option<Vec<u8>>,
    can_approve: Option<bool>,
    pair_complete: bool,
    quality: Option<String>,
}

#[cfg(windows)]
const NATIVE_HOST_EXECUTABLE: &str = "holodori-native-host.exe";
#[cfg(not(windows))]
const NATIVE_HOST_EXECUTABLE: &str = "holodori-native-host";

#[derive(Default)]
struct HostState {
    runtime: Option<HostRuntime>,
    phase: HostPhase,
    stopping: bool,
    message: String,
    fatal_message: Option<String>,
    recovery_needs_admin: bool,
    paired: bool,
    pattern: Option<Vec<u8>>,
    can_approve: bool,
    quality: Option<String>,
}

#[derive(Debug, Serialize)]
struct HostStatus {
    running: bool,
    stopping: bool,
    phase: String,
    message: String,
    recovery_needs_admin: bool,
    pairing: bool,
    paired: bool,
    pattern: Option<Vec<u8>>,
    can_approve: bool,
    quality: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct LocalOnlyTetherStatus {
    available: bool,
    enabled: bool,
    configured: bool,
    mixed: bool,
    interface_name: Option<String>,
    message: String,
}

#[cfg(target_os = "linux")]
impl From<linux_network_manager::LinuxLocalOnlyStatus> for LocalOnlyTetherStatus {
    fn from(status: linux_network_manager::LinuxLocalOnlyStatus) -> Self {
        Self {
            available: status.available,
            enabled: status.enabled,
            configured: status.configured,
            mixed: status.mixed,
            interface_name: status.interface_name,
            message: status.message,
        }
    }
}

#[tauri::command]
fn start_host(
    state: State<'_, Mutex<HostState>>,
    keys: String,
    metrics: bool,
    local_only_tether: bool,
    transport: String,
    legacy_v4: bool,
) -> Result<HostStatus, String> {
    let mut state = state
        .lock()
        .map_err(|_| "controller state is unavailable")?;
    reap_child(&mut state)?;
    if state.runtime.is_some() {
        return Ok(status(&state));
    }
    let transport = validate_transport(&transport)?;
    if local_only_tether && transport != "usb" {
        return Err("Local-only tethering requires the USB transport.".to_owned());
    }
    if legacy_v4 && transport != "usb" {
        return Err("Protocol v4 is available only over USB.".to_owned());
    }
    if !legacy_v4 && !state.paired {
        state.paired =
            holodori_native_host::credentials::is_paired().map_err(|error| error.to_string())?;
        if !state.paired {
            return Err("Pair this host and phone before starting protocol v5.".to_owned());
        }
    }
    ensure_route_recovery(&mut state)?;
    #[cfg(target_os = "linux")]
    if local_only_tether {
        linux_network_manager::ensure_enabled()?;
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    if local_only_tether {
        return Err("Local-only tethering is unavailable on this platform.".to_owned());
    }

    let host = find_host().ok_or_else(|| {
        "The controller is missing. Build native-host or re-extract the portable bundle.".to_owned()
    })?;
    UdpSocket::bind(("0.0.0.0", USB_TETHER_UDP_PORT)).map_err(|error| {
        format!(
            "UDP port {USB_TETHER_UDP_PORT} is already in use. Close any other running Holodori host, then try again: {error}"
        )
    })?;
    // On Windows the launcher elevates itself (see `restart_as_admin`), so a
    // plain child process already inherits admin rights when needed.
    // Linux delegates local-only configuration to NetworkManager and never
    // elevates this launcher or the native gameplay host.
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
        .arg("--transport")
        .arg(transport)
        .arg("--warn-ms")
        .arg(DEFAULT_WARNING_BUDGET_MS);
    if legacy_v4 {
        command.arg("--legacy-v4");
    }
    if metrics {
        command.arg("--metrics");
    }
    #[cfg(any(windows, target_os = "linux"))]
    if local_only_tether {
        command.arg("--local-only-tether");
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    state.runtime = Some(spawn_runtime(
        command,
        RuntimeKind::Controller,
        local_only_tether,
    )?);
    state.phase = HostPhase::Waiting;
    state.stopping = false;
    state.message.clear();
    state.fatal_message = None;
    state.recovery_needs_admin = false;
    state.pattern = None;
    state.can_approve = false;
    state.quality = None;
    Ok(status(&state))
}

#[tauri::command]
fn begin_pairing(
    state: State<'_, Mutex<HostState>>,
    transport: String,
) -> Result<HostStatus, String> {
    let mut state = state
        .lock()
        .map_err(|_| "controller state is unavailable")?;
    reap_child(&mut state)?;
    if state.runtime.is_some() {
        return Err("Stop the current controller or pairing window first.".to_owned());
    }
    ensure_route_recovery(&mut state)?;
    let transport = validate_transport(&transport)?;
    let host = find_host().ok_or_else(|| {
        "The controller is missing. Build native-host or re-extract the portable bundle.".to_owned()
    })?;
    UdpSocket::bind(("0.0.0.0", USB_TETHER_UDP_PORT)).map_err(|error| {
        format!(
            "UDP port {USB_TETHER_UDP_PORT} is already in use. Stop another Holodori host, then try again: {error}"
        )
    })?;
    let mut command = Command::new(&host);
    command
        .current_dir(host.parent().unwrap_or_else(|| std::path::Path::new(".")))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .arg("--pair")
        .arg("--transport")
        .arg(transport)
        .arg("--udp-port")
        .arg(USB_TETHER_UDP_PORT.to_string());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    state.runtime = Some(spawn_runtime(command, RuntimeKind::Pairing, false)?);
    state.phase = HostPhase::Pairing;
    state.stopping = false;
    state.message.clear();
    state.fatal_message = None;
    state.pattern = None;
    state.can_approve = false;
    state.quality = None;
    Ok(status(&state))
}

#[tauri::command]
fn approve_pairing(state: State<'_, Mutex<HostState>>) -> Result<HostStatus, String> {
    let mut state = state
        .lock()
        .map_err(|_| "controller state is unavailable")?;
    reap_child(&mut state)?;
    if !state.can_approve {
        return Err("Wait until the phone says Pattern matched before approving.".to_owned());
    }
    let Some(runtime) = state.runtime.as_mut() else {
        return Err("No pairing window is open.".to_owned());
    };
    if runtime.kind != RuntimeKind::Pairing {
        return Err("Wait until the phone says Pattern matched before approving.".to_owned());
    }
    let stdin = runtime
        .child
        .stdin
        .as_mut()
        .ok_or("Pairing input is unavailable.")?;
    stdin
        .write_all(b"approve\n")
        .and_then(|_| stdin.flush())
        .map_err(|error| format!("Could not approve pairing: {error}"))?;
    state.can_approve = false;
    state.message = "Approval sent; finishing secure pairing...".to_owned();
    Ok(status(&state))
}

#[tauri::command]
fn forget_device(state: State<'_, Mutex<HostState>>) -> Result<HostStatus, String> {
    let mut state = state
        .lock()
        .map_err(|_| "controller state is unavailable")?;
    reap_child(&mut state)?;
    if state.runtime.is_some() {
        return Err("Stop the controller before forgetting the phone.".to_owned());
    }
    holodori_native_host::credentials::forget_phone().map_err(|error| error.to_string())?;
    state.paired = false;
    state.pattern = None;
    state.can_approve = false;
    state.quality = None;
    state.phase = HostPhase::Ready;
    state.message = "Paired phone forgotten.".to_owned();
    Ok(status(&state))
}

fn validate_transport(transport: &str) -> Result<&str, String> {
    match transport {
        "usb" | "wifi" => Ok(transport),
        _ => Err("Choose USB or Wi-Fi / local network.".to_owned()),
    }
}

fn spawn_runtime(
    mut command: Command,
    kind: RuntimeKind,
    local_only_tether: bool,
) -> Result<HostRuntime, String> {
    let mut child = command
        .spawn()
        .map_err(|error| format!("Could not start the controller: {error}"))?;
    let output = Arc::new(Mutex::new(HostOutput::default()));
    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        let output = Arc::clone(&output);
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(mut latest) = output.lock() {
                    parse_host_output_line(&line, &mut latest);
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
    Ok(HostRuntime {
        child,
        output,
        readers,
        local_only_tether,
        kind,
    })
}

fn parse_host_output_line(line: &str, output: &mut HostOutput) {
    if let Some(phase) = parse_status_token(line) {
        output.phase = Some(phase);
        return;
    }
    if let Some(value) = line.strip_prefix("HPT_PAIR PATTERN ") {
        let pattern: Option<Vec<u8>> = value
            .split(',')
            .map(|lane| {
                lane.parse::<u8>()
                    .ok()
                    .filter(|lane| (1..=6).contains(lane))
            })
            .collect();
        if pattern.as_ref().is_some_and(|pattern| pattern.len() == 8) {
            output.pattern = pattern;
        }
    } else if line == "HPT_PAIR CONFIRMED" {
        output.can_approve = Some(true);
    } else if line == "HPT_PAIR COMPLETE" {
        output.pair_complete = true;
        output.can_approve = Some(false);
    } else if let Some(summary) = line.strip_prefix("HPT_QUALITY ") {
        output.quality = Some(summary.chars().take(1_024).collect());
    }
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

/// Tells the frontend how privileged actions (currently: local-only
/// tethering) get their elevation on this platform:
///
/// - `"launcher"`: the whole launcher process is elevated up front (UAC via
///   `restart_as_admin`), and every child it spawns inherits that token.
///   This is the Windows model.
/// - `"network-manager"`: NetworkManager owns the Linux profile and polkit
///   authorizes only that settings change; the launcher and gameplay host
///   do not request elevation.
/// - `"unsupported"`: no safe platform integration is available.
#[tauri::command]
fn elevation_model() -> &'static str {
    #[cfg(windows)]
    {
        "launcher"
    }
    #[cfg(target_os = "linux")]
    {
        "network-manager"
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        "unsupported"
    }
}

#[cfg(target_os = "linux")]
#[tauri::command]
fn linux_local_only_tether_status() -> Result<LocalOnlyTetherStatus, String> {
    linux_network_manager::status().map(Into::into)
}

#[cfg(not(target_os = "linux"))]
#[tauri::command]
fn linux_local_only_tether_status() -> Result<LocalOnlyTetherStatus, String> {
    Ok(LocalOnlyTetherStatus {
        available: false,
        enabled: false,
        configured: false,
        mixed: false,
        interface_name: None,
        message: "NetworkManager profile control is Linux-only.".to_owned(),
    })
}

#[cfg(target_os = "linux")]
#[tauri::command]
fn set_linux_local_only_tether(
    state: State<'_, Mutex<HostState>>,
    enabled: bool,
) -> Result<LocalOnlyTetherStatus, String> {
    let mut state = state
        .lock()
        .map_err(|_| "controller state is unavailable")?;
    reap_child(&mut state)?;
    if state.runtime.is_some() {
        return Err("Stop the controller before changing the tether profile.".to_owned());
    }
    linux_network_manager::set_enabled(enabled).map(Into::into)
}

#[cfg(not(target_os = "linux"))]
#[tauri::command]
fn set_linux_local_only_tether(
    _state: State<'_, Mutex<HostState>>,
    _enabled: bool,
) -> Result<LocalOnlyTetherStatus, String> {
    Err("NetworkManager profile control is Linux-only.".to_owned())
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
        pairing: state
            .runtime
            .as_ref()
            .is_some_and(|runtime| runtime.kind == RuntimeKind::Pairing),
        paired: state.paired,
        pattern: state.pattern.clone(),
        can_approve: state.can_approve,
        quality: state.quality.clone(),
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
        let runtime_kind = runtime.kind;
        drop(runtime);

        if exit_status.success() {
            state.phase = HostPhase::Ready;
            state.message = if runtime_kind == RuntimeKind::Pairing && !expected_stop {
                "Pairing complete. Ready to start.".to_owned()
            } else {
                String::new()
            };
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
    let (phase, fatal_message, pattern, can_approve, pair_complete, quality) = match output.lock() {
        Ok(mut output) => (
            output.phase.take(),
            output.fatal_message.take(),
            output.pattern.take(),
            output.can_approve.take(),
            std::mem::take(&mut output.pair_complete),
            output.quality.take(),
        ),
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
    if let Some(pattern) = pattern {
        state.pattern = Some(pattern);
        state.message = "Replicate this pattern on the phone's six lanes.".to_owned();
    }
    if let Some(can_approve) = can_approve {
        state.can_approve = can_approve;
        if can_approve {
            state.message =
                "Phone reports Pattern matched. Confirm that on the real phone, then approve."
                    .to_owned();
        }
    }
    if let Some(quality) = quality {
        state.quality = Some(quality);
    }
    if pair_complete {
        state.paired = true;
        state.can_approve = false;
        state.message = "Pairing complete. Ready to start.".to_owned();
    }
}

fn parse_status_token(line: &str) -> Option<HostPhase> {
    match line {
        "HPT_STATUS WAITING" => Some(HostPhase::Waiting),
        "HPT_STATUS CONNECTED" => Some(HostPhase::Connected),
        "HPT_STATUS RECOVERING" => Some(HostPhase::Recovering),
        "HPT_STATUS STOPPING" => Some(HostPhase::Stopping),
        "HPT_STATUS PAIRING" => Some(HostPhase::Pairing),
        _ => None,
    }
}

#[cfg(windows)]
fn ensure_route_recovery(state: &mut HostState) -> Result<(), String> {
    apply_route_recovery(state, recover_orphaned_policy())
}

#[cfg(windows)]
fn apply_route_recovery(
    state: &mut HostState,
    outcome: std::io::Result<RecoveryOutcome>,
) -> Result<(), String> {
    match outcome {
        Ok(RecoveryOutcome::NothingToDo | RecoveryOutcome::Restored { .. }) => {
            state.recovery_needs_admin = false;
            if state.phase == HostPhase::RecoveryNeedsAdmin {
                state.phase = HostPhase::Ready;
                state.message.clear();
            }
            Ok(())
        }
        Ok(RecoveryOutcome::Deferred { .. }) => {
            state.recovery_needs_admin = false;
            if state.phase == HostPhase::RecoveryNeedsAdmin {
                state.phase = HostPhase::Ready;
            }
            if state.phase == HostPhase::Ready {
                state.message = "Ready. USB route cleanup is pending. Reconnect the phone, enable USB tethering, then Pair or Start to retry."
                    .to_owned();
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

#[cfg(not(windows))]
fn ensure_route_recovery(state: &mut HostState) -> Result<(), String> {
    state.recovery_needs_admin = false;
    Ok(())
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
    let mut candidates = Vec::new();
    // Portable bundles ship the native host in a platform-named
    // subdirectory next to the launcher (see packaging/build-experimental.ps1
    // for the Windows layout); mirror that with a Linux equivalent.
    #[cfg(windows)]
    candidates.push(base.join("Windows").join(NATIVE_HOST_EXECUTABLE));
    #[cfg(not(windows))]
    candidates.push(base.join("Linux").join(NATIVE_HOST_EXECUTABLE));
    candidates.push(base.join(NATIVE_HOST_EXECUTABLE));
    if let Some(workspace) = base
        .ancestors()
        .find(|path| path.join("native-host").join("Cargo.toml").is_file())
    {
        let native_target = workspace.join("native-host").join("target");
        candidates.push(native_target.join("release").join(NATIVE_HOST_EXECUTABLE));
        candidates.push(native_target.join("debug").join(NATIVE_HOST_EXECUTABLE));
    }
    candidates.into_iter().find(|path| path.is_file())
}

/// Works around a Wayland + NVIDIA crash seen at launch:
///
/// ```text
/// Gdk-Message: Error 71 (Protocol error) dispatching to Wayland display.
/// ```
///
/// caused by the compositor killing the connection with
/// `wl_display.error(wp_linux_drm_syncobj_surface_v1, 4, "explicit sync is
/// used, but no acquire point is set")`.
///
/// Tauri's own Linux graphics guidance
/// (<https://v2.tauri.app/develop/debug/linux-graphics/>) recommends setting
/// `__NV_DISABLE_EXPLICIT_SYNC=1` for exactly this crash, and says it "often
/// fixes the Wayland Error 71 crash without a performance cost." That page
/// also warns: "Only ship an unconditional override like this if you have
/// verified your app is affected. It disables a faster path for everyone,
/// including users on working setups." So this only fires when all of the
/// following hold, to avoid degrading anyone whose setup already works:
///
/// - Linux only.
/// - The session is actually Wayland (`WAYLAND_DISPLAY` is set and
///   non-empty).
/// - The proprietary NVIDIA driver is loaded (`/sys/module/nvidia_drm`
///   exists); this crash is specific to NVIDIA's explicit-sync handling.
/// - The user has not already set `__NV_DISABLE_EXPLICIT_SYNC` themselves
///   (to `1` or to `0`) -- their choice always wins.
///
/// Users on other graphics stacks who still hit this crash can set
/// `WEBKIT_DISABLE_DMABUF_RENDERER=1` themselves; see the README's
/// troubleshooting section.
#[cfg(target_os = "linux")]
fn apply_linux_graphics_workarounds() {
    let is_wayland = std::env::var("WAYLAND_DISPLAY").is_ok_and(|value| !value.is_empty());
    let has_nvidia = std::path::Path::new("/sys/module/nvidia_drm").exists();
    let already_set = std::env::var_os("__NV_DISABLE_EXPLICIT_SYNC").is_some();

    if is_wayland && has_nvidia && !already_set {
        // SAFETY: called at the very start of `main`, before
        // `tauri::Builder` spawns any threads, so no other code can be
        // concurrently reading or writing the environment.
        unsafe {
            std::env::set_var("__NV_DISABLE_EXPLICIT_SYNC", "1");
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn apply_linux_graphics_workarounds() {}

/// Grows the window to fit its measured content, once, at startup.
///
/// GTK honors the desktop's Xft.dpi text-scaling setting, so WebKitGTK can
/// render the fixed size chosen for Windows at 96 DPI far too small on a
/// high-DPI Linux desktop.
///
/// All of `current_width`/`current_height`/`wanted_width`/`wanted_height`
/// are **physical** pixels, computed by the frontend as
/// `cssPixels * window.devicePixelRatio`. This deliberately avoids
/// `window.inner_size()` and `window.scale_factor()`: on this GTK/Wayland
/// stack, `inner_size()` was measured to include a constant phantom offset
/// -- tens of logical pixels larger than the webview's real content box,
/// stable regardless of window size -- which made the old comparison
/// conclude the window was already big enough when it visibly was not.
/// `window.devicePixelRatio` and `set_size(PhysicalSize)`, by contrast, were
/// verified to agree precisely (`CSS px == physical px / devicePixelRatio`
/// in both directions), so the frontend's own measurements are the source
/// of truth here.
///
/// `scale` is `devicePixelRatio`, used only to convert the desktop-panel
/// margin below into physical pixels. This only ever grows the window
/// (never fights a user who already resized it smaller: the frontend only
/// calls this when its content actually overflows the current viewport) and
/// never exceeds the current monitor's work area.
#[cfg(target_os = "linux")]
#[tauri::command]
fn fit_window_to_content(
    window: tauri::WebviewWindow,
    current_width: u32,
    current_height: u32,
    wanted_width: u32,
    wanted_height: u32,
    scale: f64,
) -> Result<(), String> {
    let Ok(Some(monitor)) = window.current_monitor() else {
        return Ok(());
    };
    // Leave room for the desktop's panel/titlebar instead of filling the
    // work area edge-to-edge.
    const MARGIN_LOGICAL: f64 = 48.0;
    let margin_physical = MARGIN_LOGICAL * scale;
    let work_area = monitor.work_area().size;
    let max_width = (work_area.width as f64 - margin_physical).max(current_width as f64);
    let max_height = (work_area.height as f64 - margin_physical).max(current_height as f64);

    let target_width = (wanted_width as f64).min(max_width);
    let target_height = (wanted_height as f64).min(max_height);

    if target_width > current_width as f64 || target_height > current_height as f64 {
        let _ = window.set_size(tauri::PhysicalSize::new(
            target_width.round() as u32,
            target_height.round() as u32,
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
#[tauri::command]
fn fit_window_to_content(
    _window: tauri::WebviewWindow,
    _current_width: u32,
    _current_height: u32,
    _wanted_width: u32,
    _wanted_height: u32,
    _scale: f64,
) -> Result<(), String> {
    Ok(())
}

fn main() {
    apply_linux_graphics_workarounds();
    tauri::Builder::default()
        .manage(Mutex::new(HostState::default()))
        .setup(|app| {
            if let Ok(mut state) = app.state::<Mutex<HostState>>().lock() {
                let _ = ensure_route_recovery(&mut state);
                match holodori_native_host::credentials::is_paired() {
                    Ok(paired) => state.paired = paired,
                    Err(error) => {
                        state.phase = HostPhase::Fatal;
                        state.message = error.to_string();
                    }
                }
            }
            #[cfg(windows)]
            if let Some(window) = app.get_webview_window("main") {
                style_windows_titlebar(&window);
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            start_host,
            begin_pairing,
            approve_pairing,
            forget_device,
            stop_host,
            host_status,
            restart_as_admin,
            launcher_is_elevated,
            elevation_model,
            linux_local_only_tether_status,
            set_linux_local_only_tether,
            fit_window_to_content
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
    use super::{
        parse_host_output_line, parse_status_token, user_facing_native_error, HostOutput, HostPhase,
    };

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
        assert_eq!(
            parse_status_token("HPT_STATUS PAIRING"),
            Some(HostPhase::Pairing),
        );
        assert_eq!(parse_status_token("UDP link ready"), None);
        assert_eq!(parse_status_token("HPT_STATUS CONNECTED extra"), None);
    }

    #[test]
    fn parses_pairing_pattern_without_accepting_invalid_lanes() {
        let mut output = HostOutput::default();
        parse_host_output_line("HPT_PAIR PATTERN 1,2,3,4,5,6,1,2", &mut output);
        assert_eq!(output.pattern, Some(vec![1, 2, 3, 4, 5, 6, 1, 2]));
        parse_host_output_line("HPT_PAIR PATTERN 1,2,3,4,5,7,1,2", &mut output);
        assert_eq!(output.pattern, Some(vec![1, 2, 3, 4, 5, 6, 1, 2]));
    }

    #[test]
    fn keeps_native_fatal_detail_for_the_launcher() {
        assert_eq!(
            user_facing_native_error("route restore failed".to_owned()),
            "route restore failed",
        );
    }

    #[cfg(windows)]
    #[test]
    fn absent_tether_allows_startup_without_hiding_other_failures() {
        use super::{apply_route_recovery, HostState, RecoveryOutcome};

        let deferred = RecoveryOutcome::Deferred {
            restored: 0,
            pending: 2,
        };
        for phase in [HostPhase::Ready, HostPhase::RecoveryNeedsAdmin] {
            let mut state = HostState {
                phase,
                recovery_needs_admin: phase == HostPhase::RecoveryNeedsAdmin,
                ..Default::default()
            };
            apply_route_recovery(&mut state, Ok(deferred)).unwrap();
            assert_eq!(state.phase, HostPhase::Ready);
            assert!(!state.recovery_needs_admin);
            assert!(state.message.contains("cleanup is pending"));
        }

        let mut state = HostState {
            phase: HostPhase::Fatal,
            message: "input sink failed".to_owned(),
            ..Default::default()
        };
        apply_route_recovery(&mut state, Ok(deferred)).unwrap();
        assert_eq!(state.phase, HostPhase::Fatal);
        assert_eq!(state.message, "input sink failed");

        let error = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "restore denied");
        assert!(apply_route_recovery(&mut state, Err(error)).is_err());
        assert_eq!(state.phase, HostPhase::RecoveryNeedsAdmin);
        assert!(state.recovery_needs_admin);
        assert!(apply_route_recovery(&mut state, Ok(RecoveryOutcome::OwnerStillRunning)).is_err());
        assert_eq!(state.phase, HostPhase::Fatal);
    }
}
