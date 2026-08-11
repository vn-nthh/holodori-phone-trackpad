#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

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

#[derive(Default)]
struct HostState {
    child: Option<Child>,
    stopping: bool,
    local_only_tether: bool,
    last_message: Option<String>,
    native_error: Arc<Mutex<Option<String>>>,
    stderr_reader: Option<JoinHandle<()>>,
}

#[derive(Debug, Serialize)]
struct HostStatus {
    running: bool,
    stopping: bool,
    message: String,
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
    if state.child.is_some() {
        return Ok(status(&state, "Running"));
    }

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
        .stdout(Stdio::null())
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
    let native_error = Arc::new(Mutex::new(None));
    let stderr_reader = child.stderr.take().map(|stderr| {
        let native_error = Arc::clone(&native_error);
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Some(message) = line.strip_prefix("fatal: ") {
                    if let Ok(mut latest) = native_error.lock() {
                        *latest = Some(message.to_owned());
                    }
                }
            }
        })
    });

    state.child = Some(child);
    state.stopping = false;
    state.local_only_tether = local_only_tether;
    state.last_message = None;
    state.native_error = native_error;
    state.stderr_reader = stderr_reader;
    Ok(status(&state, "Running"))
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
    let Some(child) = state.child.as_mut() else {
        let message = state
            .last_message
            .take()
            .unwrap_or_else(|| "Ready".to_owned());
        return Ok(status(&state, &message));
    };
    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(b"q\n")
            .and_then(|_| stdin.flush())
            .map_err(|error| format!("Could not stop the controller safely: {error}"))?;
    }
    state.stopping = true;
    Ok(status(&state, "Stopping safely..."))
}

#[tauri::command]
fn host_status(state: State<'_, Mutex<HostState>>) -> Result<HostStatus, String> {
    let mut state = state
        .lock()
        .map_err(|_| "controller state is unavailable")?;
    reap_child(&mut state)?;
    if state.child.is_some() {
        let message = if state.stopping {
            "Stopping safely..."
        } else {
            "Running"
        };
        Ok(status(&state, message))
    } else {
        let message = state
            .last_message
            .take()
            .unwrap_or_else(|| "Ready".to_owned());
        Ok(status(&state, &message))
    }
}

fn status(state: &HostState, message: &str) -> HostStatus {
    HostStatus {
        running: state.child.is_some(),
        stopping: state.stopping,
        message: message.to_owned(),
    }
}

fn reap_child(state: &mut HostState) -> Result<(), String> {
    let Some(child) = state.child.as_mut() else {
        return Ok(());
    };
    if let Some(exit_status) = child
        .try_wait()
        .map_err(|error| format!("Could not check controller state: {error}"))?
    {
        state.child = None;
        if let Some(reader) = state.stderr_reader.take() {
            let _ = reader.join();
        }
        let native_error = state
            .native_error
            .lock()
            .ok()
            .and_then(|message| message.clone());
        if !exit_status.success() {
            state.last_message = Some(
                native_error
                    .map(user_facing_native_error)
                    .unwrap_or_else(|| {
                        if state.local_only_tether && state.stopping {
                            "The controller did not stop cleanly while local-only tethering was enabled."
                                .to_owned()
                        } else if state.stopping {
                            "The controller did not stop cleanly. Try starting it again.".to_owned()
                        } else {
                            "The controller stopped unexpectedly. Try starting it again.".to_owned()
                        }
                    }),
            );
        }
        state.stopping = false;
        state.local_only_tether = false;
        state.native_error = Arc::new(Mutex::new(None));
    }
    Ok(())
}

fn user_facing_native_error(message: String) -> String {
    if message.contains("recognized Android/RNDIS adapter") {
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
    let Some(child) = state.child.as_mut() else {
        return Ok(());
    };
    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(b"q\n")
            .and_then(|_| stdin.flush())
            .map_err(|error| format!("Could not stop the controller safely: {error}"))?;
    }
    state.stopping = true;
    Ok(())
}
