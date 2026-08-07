//! Friendly Windows launcher for the driver-free USB-tethered host.
//!
//! The latency-critical work remains in `holodori-native-host.exe`. This
//! process only presents a small native settings window and starts the host
//! with the equivalent command-line options for the user.

use std::io::{self, Write};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::ptr::{null, null_mut};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{DEFAULT_GUI_FONT, GetStockObject, UpdateWindow};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{BST_CHECKED, BST_UNCHECKED};
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BM_GETCHECK, BM_SETCHECK, BS_AUTOCHECKBOX, BS_AUTORADIOBUTTON, BS_DEFPUSHBUTTON, BS_GROUPBOX,
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, ES_AUTOHSCROLL, ES_LEFT, GetMessageW, GetWindowTextLengthW, GetWindowTextW,
    IDC_ARROW, KillTimer, LoadCursorW, MSG, MessageBoxW, PostQuitMessage, RegisterClassW,
    SendMessageW, SetTimer, SetWindowTextW, ShowWindow, TranslateMessage, WM_CLOSE, WM_COMMAND,
    WM_CREATE, WM_DESTROY, WM_ERASEBKGND, WM_SETFONT, WM_TIMER, WNDCLASSW, WS_CAPTION, WS_CHILD,
    WS_EX_CLIENTEDGE, WS_GROUP, WS_MINIMIZEBOX, WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};

const WINDOW_CLASS: &str = "HolodoriUsbControllerLauncher";
const TIMER_ID: usize = 1;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const ID_TOUCH_MODE: i32 = 1001;
const ID_KEYS_MODE: i32 = 1002;
const ID_LANES: i32 = 1003;
const ID_PORT: i32 = 1004;
const ID_WARN_MS: i32 = 1005;
const ID_METRICS: i32 = 1006;
const ID_START: i32 = 1007;
const ID_STOP: i32 = 1008;
const ID_STATUS: i32 = 1009;

const BN_CLICKED: u16 = 0;
const STYLE_GROUPBOX: u32 = BS_GROUPBOX as u32;
const STYLE_RADIO: u32 = BS_AUTORADIOBUTTON as u32;
const STYLE_CHECKBOX: u32 = BS_AUTOCHECKBOX as u32;
const STYLE_DEFAULT_BUTTON: u32 = BS_DEFPUSHBUTTON as u32;
const STYLE_EDIT: u32 = ES_LEFT as u32 | ES_AUTOHSCROLL as u32 | 0x0000_0100;

#[derive(Clone, Copy)]
struct Controls {
    touch_mode: HWND,
    keys_mode: HWND,
    lanes: HWND,
    port: HWND,
    warn_ms: HWND,
    metrics: HWND,
    start: HWND,
    stop: HWND,
    status: HWND,
}

// All controls are created and used on the launcher's single UI thread. The
// marker is needed only because HWND is an opaque raw pointer in windows-sys.
unsafe impl Send for Controls {}

struct UiState {
    controls: Option<Controls>,
    child: Option<Child>,
    closing: bool,
    stop_started: Option<Instant>,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            controls: None,
            child: None,
            closing: false,
            stop_started: None,
        }
    }
}

static UI_STATE: OnceLock<Mutex<UiState>> = OnceLock::new();

fn main() {
    if let Err(error) = run() {
        message_box(
            null_mut(),
            &format!("Holodori could not start.\n\n{error}"),
            "Holodori USB Controller",
        );
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    UI_STATE.set(Mutex::new(UiState::default())).ok();
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let instance = unsafe { GetModuleHandleW(null()) };
    if instance.is_null() {
        return Err(io::Error::last_os_error());
    }

    let class_name = wide(WINDOW_CLASS);
    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        hCursor: unsafe { LoadCursorW(null_mut(), IDC_ARROW) },
        lpszClassName: class_name.as_ptr(),
        ..WNDCLASSW::default()
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        return Err(io::Error::last_os_error());
    }

    let title = wide("Holodori USB Controller");
    let window = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            650,
            600,
            null_mut(),
            null_mut(),
            instance,
            null(),
        )
    };
    if window.is_null() {
        return Err(io::Error::last_os_error());
    }

    unsafe {
        ShowWindow(window, 1);
        UpdateWindow(window);
    }

    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    Ok(())
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_CREATE => {
            if let Err(error) = create_controls(hwnd) {
                message_box(
                    hwnd,
                    &format!("Could not create the settings window.\n\n{error}"),
                    "Holodori",
                );
                return -1;
            }
            0
        }
        WM_COMMAND => {
            let id = (wparam & 0xffff) as i32;
            let notification = ((wparam >> 16) & 0xffff) as u16;
            if notification == BN_CLICKED {
                match id {
                    ID_TOUCH_MODE | ID_KEYS_MODE => update_lane_editor(),
                    ID_START => start_host(hwnd),
                    ID_STOP => request_stop(hwnd),
                    _ => {}
                }
            }
            0
        }
        WM_TIMER if wparam == TIMER_ID => poll_child(hwnd),
        WM_CLOSE => {
            let should_close = {
                let Some(lock) = UI_STATE.get() else {
                    return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
                };
                let mut state = lock.lock().unwrap_or_else(|poison| poison.into_inner());
                if state.child.is_some() {
                    state.closing = true;
                    drop(state);
                    request_stop(hwnd);
                    false
                } else {
                    true
                }
            };
            if should_close {
                unsafe { DestroyWindow(hwnd) };
            }
            0
        }
        WM_DESTROY => {
            unsafe { KillTimer(hwnd, TIMER_ID) };
            unsafe { PostQuitMessage(0) };
            0
        }
        WM_ERASEBKGND => 1,
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn create_controls(parent: HWND) -> io::Result<()> {
    let instance = unsafe { GetModuleHandleW(null()) };
    let font = unsafe { GetStockObject(DEFAULT_GUI_FONT) };

    make_control(
        parent,
        "STATIC",
        "Holodori USB Controller",
        WS_CHILD | WS_VISIBLE,
        0,
        28,
        20,
        570,
        28,
        0,
        instance,
        font,
    );
    make_control(
        parent,
        "STATIC",
        "Start once, then play. USB tethering carries the input; no ADB or driver install is needed.",
        WS_CHILD | WS_VISIBLE,
        0,
        28,
        52,
        570,
        24,
        0,
        instance,
        font,
    );

    make_control(
        parent,
        "BUTTON",
        "Input mode",
        WS_CHILD | WS_VISIBLE | STYLE_GROUPBOX,
        0,
        28,
        88,
        590,
        112,
        0,
        instance,
        font,
    );
    let touch_mode = make_control(
        parent,
        "BUTTON",
        "Touch input (recommended)",
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_GROUP | STYLE_RADIO,
        0,
        48,
        116,
        250,
        26,
        ID_TOUCH_MODE,
        instance,
        font,
    );
    let keys_mode = make_control(
        parent,
        "BUTTON",
        "Keyboard lanes",
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | STYLE_RADIO,
        0,
        48,
        148,
        250,
        26,
        ID_KEYS_MODE,
        instance,
        font,
    );
    unsafe {
        SendMessageW(touch_mode, BM_SETCHECK, BST_CHECKED as usize, 0);
        SendMessageW(keys_mode, BM_SETCHECK, BST_UNCHECKED as usize, 0);
    }
    make_control(
        parent,
        "STATIC",
        "Touch mode opens the Windows touch receiver automatically.",
        WS_CHILD | WS_VISIBLE,
        0,
        322,
        116,
        270,
        24,
        0,
        instance,
        font,
    );
    make_control(
        parent,
        "STATIC",
        "Keyboard mode sends the selected lanes to the focused game.",
        WS_CHILD | WS_VISIBLE,
        0,
        322,
        148,
        270,
        24,
        0,
        instance,
        font,
    );

    make_control(
        parent,
        "BUTTON",
        "Settings",
        WS_CHILD | WS_VISIBLE | STYLE_GROUPBOX,
        0,
        28,
        218,
        590,
        132,
        0,
        instance,
        font,
    );
    make_control(
        parent,
        "STATIC",
        "Keys, left to right",
        WS_CHILD | WS_VISIBLE,
        0,
        48,
        247,
        140,
        24,
        0,
        instance,
        font,
    );
    let lanes = make_control(
        parent,
        "EDIT",
        "s,d,f,j,k,l",
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | STYLE_EDIT,
        WS_EX_CLIENTEDGE,
        190,
        243,
        180,
        26,
        ID_LANES,
        instance,
        font,
    );
    make_control(
        parent,
        "STATIC",
        "Used only in keyboard mode",
        WS_CHILD | WS_VISIBLE,
        0,
        380,
        247,
        200,
        24,
        0,
        instance,
        font,
    );
    make_control(
        parent,
        "STATIC",
        "USB port",
        WS_CHILD | WS_VISIBLE,
        0,
        48,
        283,
        90,
        24,
        0,
        instance,
        font,
    );
    let port = make_control(
        parent,
        "EDIT",
        "42825",
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | STYLE_EDIT,
        WS_EX_CLIENTEDGE,
        140,
        279,
        90,
        26,
        ID_PORT,
        instance,
        font,
    );
    make_control(
        parent,
        "STATIC",
        "Warning budget (ms)",
        WS_CHILD | WS_VISIBLE,
        0,
        260,
        247,
        145,
        24,
        0,
        instance,
        font,
    );
    let warn_ms = make_control(
        parent,
        "EDIT",
        "8.333",
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | STYLE_EDIT,
        WS_EX_CLIENTEDGE,
        410,
        243,
        90,
        26,
        ID_WARN_MS,
        instance,
        font,
    );
    let metrics = make_control(
        parent,
        "BUTTON",
        "Save latency report when stopped",
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | STYLE_CHECKBOX,
        0,
        48,
        318,
        300,
        26,
        ID_METRICS,
        instance,
        font,
    );
    unsafe {
        SendMessageW(metrics, BM_SETCHECK, BST_CHECKED as usize, 0);
    }

    make_control(
        parent,
        "BUTTON",
        "Before you start",
        WS_CHILD | WS_VISIBLE | STYLE_GROUPBOX,
        0,
        28,
        370,
        590,
        78,
        0,
        instance,
        font,
    );
    make_control(
        parent,
        "STATIC",
        "1. Enable USB tethering on the phone.  2. Connect a USB data cable.  3. Open the phone app.",
        WS_CHILD | WS_VISIBLE,
        0,
        48,
        394,
        540,
        24,
        0,
        instance,
        font,
    );
    make_control(
        parent,
        "STATIC",
        "Keep this window open while playing. Stop here when you are finished.",
        WS_CHILD | WS_VISIBLE,
        0,
        48,
        418,
        540,
        24,
        0,
        instance,
        font,
    );

    let status = make_control(
        parent,
        "STATIC",
        "Ready. Enable USB tethering, connect the phone, then press Start.",
        WS_CHILD | WS_VISIBLE,
        0,
        28,
        464,
        360,
        28,
        ID_STATUS,
        instance,
        font,
    );
    let start = make_control(
        parent,
        "BUTTON",
        "Start",
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | STYLE_DEFAULT_BUTTON,
        0,
        410,
        458,
        98,
        34,
        ID_START,
        instance,
        font,
    );
    let stop = make_control(
        parent,
        "BUTTON",
        "Stop",
        WS_CHILD | WS_VISIBLE | WS_TABSTOP,
        0,
        516,
        458,
        98,
        34,
        ID_STOP,
        instance,
        font,
    );
    unsafe { EnableWindow(stop, 0) };

    let controls = Controls {
        touch_mode,
        keys_mode,
        lanes,
        port,
        warn_ms,
        metrics,
        start,
        stop,
        status,
    };
    let lock = UI_STATE.get().expect("UI state initialized");
    let mut state = lock.lock().unwrap_or_else(|poison| poison.into_inner());
    state.controls = Some(controls);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn make_control(
    parent: HWND,
    class_name: &str,
    text: &str,
    style: u32,
    ex_style: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    id: i32,
    instance: *mut core::ffi::c_void,
    font: *mut core::ffi::c_void,
) -> HWND {
    let class_name = wide(class_name);
    let text = wide(text);
    let control = unsafe {
        CreateWindowExW(
            ex_style,
            class_name.as_ptr(),
            text.as_ptr(),
            style,
            x,
            y,
            width,
            height,
            parent,
            id as usize as _,
            instance,
            null(),
        )
    };
    if !control.is_null() {
        unsafe { SendMessageW(control, WM_SETFONT, font as usize, 1) };
    }
    control
}

fn update_lane_editor() {
    let Some(controls) = controls() else {
        return;
    };
    let touch =
        unsafe { SendMessageW(controls.touch_mode, BM_GETCHECK, 0, 0) } == BST_CHECKED as isize;
    unsafe { EnableWindow(controls.lanes, if touch { 0 } else { 1 }) };
}

fn start_host(hwnd: HWND) {
    let Some(controls) = controls() else {
        return;
    };
    let touch =
        unsafe { SendMessageW(controls.touch_mode, BM_GETCHECK, 0, 0) } == BST_CHECKED as isize;
    let lanes = read_text(controls.lanes);
    let port_text = read_text(controls.port);
    let warn_text = read_text(controls.warn_ms);
    let port = match port_text.parse::<u16>() {
        Ok(value) if value > 0 => value,
        _ => {
            message_box(
                hwnd,
                "USB port must be a number from 1 to 65535.",
                "Check settings",
            );
            return;
        }
    };
    let warning_budget = match warn_text.parse::<f64>() {
        Ok(value) if value.is_finite() && value > 0.0 => value,
        _ => {
            message_box(
                hwnd,
                "Warning budget must be a positive number, for example 8.333.",
                "Check settings",
            );
            return;
        }
    };
    if !touch && !valid_lane_list(&lanes) {
        message_box(
            hwnd,
            "Enter one letter or number per lane, separated by commas.\n\nExample: s,d,f,j,k,l",
            "Check keyboard lanes",
        );
        return;
    }

    let metrics =
        unsafe { SendMessageW(controls.metrics, BM_GETCHECK, 0, 0) } == BST_CHECKED as isize;
    let host = match find_host() {
        Ok(path) => path,
        Err(error) => {
            message_box(
                hwnd,
                &format!("The Windows host executable is missing.\n\n{error}"),
                "Holodori files missing",
            );
            return;
        }
    };

    let mut command = Command::new(&host);
    command
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .arg("--mode")
        .arg(if touch { "touch" } else { "keys" })
        .arg("--udp-port")
        .arg(port.to_string())
        .arg("--warn-ms")
        .arg(warning_budget.to_string());
    if !touch {
        command.arg("--lanes").arg(&lanes);
    }
    if metrics {
        command.arg("--metrics");
    }

    match command.spawn() {
        Ok(child) => {
            let lock = UI_STATE.get().expect("UI state initialized");
            let mut state = lock.lock().unwrap_or_else(|poison| poison.into_inner());
            state.child = Some(child);
            state.closing = false;
            state.stop_started = None;
            set_running(&controls, true);
            set_status(
                controls.status,
                "Running. Enable USB tethering and open the phone app if you have not already.",
            );
            unsafe { SetTimer(hwnd, TIMER_ID, 250, None) };
        }
        Err(error) => message_box(
            hwnd,
            &format!("Could not start the controller.\n\n{error}"),
            "Holodori could not start",
        ),
    }
}

fn request_stop(hwnd: HWND) {
    let Some(controls) = controls() else {
        return;
    };
    let lock = UI_STATE.get().expect("UI state initialized");
    let mut state = lock.lock().unwrap_or_else(|poison| poison.into_inner());
    let Some(child) = state.child.as_mut() else {
        if state.closing {
            unsafe { DestroyWindow(hwnd) };
        }
        return;
    };
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(b"q\n");
        let _ = stdin.flush();
    }
    state.stop_started.get_or_insert_with(Instant::now);
    set_running(&controls, false);
    unsafe { EnableWindow(controls.start, 0) };
    set_status(
        controls.status,
        "Stopping safely. Releasing any held input...",
    );
    unsafe { SetTimer(hwnd, TIMER_ID, 100, None) };
}

fn poll_child(hwnd: HWND) -> LRESULT {
    let Some(controls) = controls() else {
        return 0;
    };
    let lock = UI_STATE.get().expect("UI state initialized");
    let mut state = lock.lock().unwrap_or_else(|poison| poison.into_inner());
    let Some(child) = state.child.as_mut() else {
        unsafe { KillTimer(hwnd, TIMER_ID) };
        return 0;
    };

    match child.try_wait() {
        Ok(Some(_status)) => {
            state.child = None;
            let closing = state.closing;
            state.stop_started = None;
            if closing {
                unsafe { DestroyWindow(hwnd) };
            } else {
                set_running(&controls, false);
                set_status(
                    controls.status,
                    "Stopped. You can start again when the phone is ready.",
                );
                unsafe { KillTimer(hwnd, TIMER_ID) };
            }
        }
        Ok(None) => {
            if let Some(started) = state.stop_started
                && started.elapsed() > Duration::from_secs(5)
            {
                set_status(
                    controls.status,
                    "Still stopping. The host is finishing its safe input cleanup.",
                );
            }
        }
        Err(error) => set_status(
            controls.status,
            &format!("Could not check controller state: {error}"),
        ),
    }
    0
}

fn set_running(controls: &Controls, running: bool) {
    unsafe {
        EnableWindow(controls.touch_mode, if running { 0 } else { 1 });
        EnableWindow(controls.keys_mode, if running { 0 } else { 1 });
        EnableWindow(controls.lanes, if running { 0 } else { 1 });
        EnableWindow(controls.port, if running { 0 } else { 1 });
        EnableWindow(controls.warn_ms, if running { 0 } else { 1 });
        EnableWindow(controls.metrics, if running { 0 } else { 1 });
        EnableWindow(controls.start, if running { 0 } else { 1 });
        EnableWindow(controls.stop, if running { 1 } else { 0 });
    }
}

fn set_status(control: HWND, text: &str) {
    let text = wide(text);
    unsafe { SetWindowTextW(control, text.as_ptr()) };
}

fn controls() -> Option<Controls> {
    let lock = UI_STATE.get()?;
    let state = lock.lock().unwrap_or_else(|poison| poison.into_inner());
    state.controls
}

fn find_host() -> io::Result<PathBuf> {
    let current = std::env::current_exe()?;
    let directory = current
        .parent()
        .ok_or_else(|| io::Error::other("launcher has no parent directory"))?;
    let candidates = [
        directory.join("holodori-native-host.exe"),
        directory.join("Windows").join("holodori-native-host.exe"),
    ];
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "holodori-native-host.exe was not found beside the launcher",
            )
        })
}

fn read_text(control: HWND) -> String {
    let length = unsafe { GetWindowTextLengthW(control) };
    if length <= 0 {
        return String::new();
    }
    let mut buffer = vec![0_u16; length as usize + 1];
    let read = unsafe { GetWindowTextW(control, buffer.as_mut_ptr(), buffer.len() as i32) };
    String::from_utf16_lossy(&buffer[..read.max(0) as usize])
        .trim()
        .to_owned()
}

fn valid_lane_list(value: &str) -> bool {
    let lanes = value.split(',').map(str::trim).collect::<Vec<_>>();
    !lanes.is_empty()
        && lanes.len() <= 16
        && lanes.iter().all(|lane| {
            let mut chars = lane.chars();
            chars
                .next()
                .is_some_and(|character| character.is_ascii_alphanumeric())
                && chars.next().is_none()
        })
}

fn message_box(parent: HWND, message: &str, title: &str) {
    let message = wide(message);
    let title = wide(title);
    unsafe { MessageBoxW(parent, message.as_ptr(), title.as_ptr(), 0x0000_0010) };
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::valid_lane_list;

    #[test]
    fn accepts_the_default_lane_layout() {
        assert!(valid_lane_list("s,d,f,j,k,l"));
    }

    #[test]
    fn rejects_empty_or_multi_character_lanes() {
        assert!(!valid_lane_list(""));
        assert!(!valid_lane_list("s,down,d"));
        assert!(!valid_lane_list("s,,d"));
    }

    #[test]
    fn rejects_more_than_sixteen_lanes() {
        let lanes = (0..17).map(|_| "a").collect::<Vec<_>>().join(",");
        assert!(!valid_lane_list(&lanes));
    }
}
