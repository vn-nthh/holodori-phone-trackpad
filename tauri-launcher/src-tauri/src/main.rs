#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::Serialize;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use tauri::{Manager, State};

const DEFAULT_WARNING_BUDGET_MS: &str = "8.333";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Default)]
struct HostState {
    child: Option<Child>,
    stopping: bool,
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
    port: u16,
    metrics: bool,
) -> Result<HostStatus, String> {
    let mut state = state
        .lock()
        .map_err(|_| "controller state is unavailable")?;
    reap_child(&mut state)?;
    if state.child.is_some() {
        return Ok(status(&state, "Running"));
    }

    let host = find_host().ok_or_else(|| {
        "The Windows controller files are missing. Re-extract the portable bundle.".to_owned()
    })?;
    let mut command = Command::new(&host);
    command
        .current_dir(host.parent().unwrap_or_else(|| std::path::Path::new(".")))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .arg("--mode")
        .arg("keys")
        .arg("--lanes")
        .arg(keys)
        .arg("--udp-port")
        .arg(port.to_string())
        .arg("--warn-ms")
        .arg(DEFAULT_WARNING_BUDGET_MS);
    if metrics {
        command.arg("--metrics");
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    state.child = Some(
        command
            .spawn()
            .map_err(|error| format!("Could not start the controller: {error}"))?,
    );
    state.stopping = false;
    Ok(status(&state, "Running"))
}

#[tauri::command]
fn stop_host(state: State<'_, Mutex<HostState>>) -> Result<HostStatus, String> {
    let mut state = state
        .lock()
        .map_err(|_| "controller state is unavailable")?;
    reap_child(&mut state)?;
    let Some(child) = state.child.as_mut() else {
        return Ok(status(&state, "Ready"));
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
        Ok(status(&state, "Ready"))
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
    if child
        .try_wait()
        .map_err(|error| format!("Could not check controller state: {error}"))?
        .is_some()
    {
        state.child = None;
        state.stopping = false;
    }
    Ok(())
}

fn find_host() -> Option<PathBuf> {
    let base = std::env::current_exe().ok()?.parent()?.to_owned();
    [
        base.join("Windows").join("holodori-native-host.exe"),
        base.join("holodori-native-host.exe"),
    ]
    .into_iter()
    .find(|path| path.is_file())
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
        .invoke_handler(tauri::generate_handler![start_host, stop_host, host_status])
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
    let hwnd = hwnd.0 as *mut c_void;

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
