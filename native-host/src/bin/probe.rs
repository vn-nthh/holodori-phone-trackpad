use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::ptr::{null, null_mut};
use std::sync::{Mutex, OnceLock};

use holodori_native_host::touch::PROBE_WINDOW_TITLE;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DEFAULT_GUI_FONT, DeleteObject, Ellipse, EndPaint, FillRect,
    GetStockObject, InvalidateRect, NULL_PEN, PAINTSTRUCT, ScreenToClient, SelectObject, SetBkMode,
    SetTextColor, TRANSPARENT, TextOutW,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::Input::Pointer::{GetPointerInfo, POINTER_INFO};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DispatchMessageW,
    GetClientRect, GetMessageW, IDC_ARROW, LoadCursorW, MSG, PostQuitMessage, RegisterClassW,
    TranslateMessage, WM_DESTROY, WM_PAINT, WM_POINTERDOWN, WM_POINTERUP, WM_POINTERUPDATE,
    WNDCLASSW, WS_EX_TOPMOST, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

const CLASS_NAME: &str = "HolodoriTouchProbeV3";
const LANE_COUNT: i32 = 6;
const HEADER_HEIGHT: i32 = 96;

#[derive(Default)]
struct ProbeState {
    active: BTreeMap<u32, POINT>,
    down_count: u64,
    update_count: u64,
    up_count: u64,
    recent: VecDeque<String>,
}

static STATE: OnceLock<Mutex<ProbeState>> = OnceLock::new();

fn main() {
    if let Err(error) = run() {
        eprintln!("touch probe: {error}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    STATE.set(Mutex::new(ProbeState::default())).ok();
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let instance = unsafe { GetModuleHandleW(null()) };
    if instance.is_null() {
        return Err(io::Error::last_os_error());
    }
    let class_name = wide(CLASS_NAME);
    let window_title = wide(PROBE_WINDOW_TITLE);
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

    let window = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST,
            class_name.as_ptr(),
            window_title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1100,
            430,
            null_mut(),
            null_mut(),
            instance,
            null(),
        )
    };
    if window.is_null() {
        return Err(io::Error::last_os_error());
    }

    println!("READY: independent Win32 window listening for WM_POINTERDOWN/UPDATE/UP");
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
        WM_POINTERDOWN | WM_POINTERUPDATE | WM_POINTERUP => {
            receive_pointer(hwnd, message, wparam);
            0
        }
        WM_PAINT => {
            paint(hwnd);
            0
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn receive_pointer(hwnd: HWND, message: u32, wparam: WPARAM) {
    let pointer_id = (wparam as u32) & 0xffff;
    let mut info = POINTER_INFO::default();
    if unsafe { GetPointerInfo(pointer_id, &mut info) } == 0 {
        return;
    }
    let mut point = info.ptPixelLocation;
    if unsafe { ScreenToClient(hwnd, &mut point) } == 0 {
        return;
    }

    let Some(state) = STATE.get() else {
        return;
    };
    let mut state = state.lock().unwrap_or_else(|poison| poison.into_inner());
    let label = match message {
        WM_POINTERDOWN => {
            state.down_count += 1;
            state.active.insert(pointer_id, point);
            "DOWN"
        }
        WM_POINTERUP => {
            state.up_count += 1;
            state.active.remove(&pointer_id);
            "UP"
        }
        _ => {
            state.update_count += 1;
            state.active.insert(pointer_id, point);
            "UPDATE"
        }
    };
    state.recent.push_front(format!(
        "{label:<6} id={pointer_id:>2}  x={:>4} y={:>4}",
        point.x, point.y
    ));
    state.recent.truncate(4);
    drop(state);

    unsafe { InvalidateRect(hwnd, null(), 0) };
}

fn paint(hwnd: HWND) {
    let mut paint = PAINTSTRUCT::default();
    let dc = unsafe { BeginPaint(hwnd, &mut paint) };
    if dc.is_null() {
        return;
    }
    let mut client = RECT::default();
    unsafe { GetClientRect(hwnd, &mut client) };

    fill(dc, &client, rgb(8, 14, 20));
    fill(
        dc,
        &RECT {
            left: 0,
            top: 0,
            right: client.right,
            bottom: HEADER_HEIGHT,
        },
        rgb(13, 24, 34),
    );
    unsafe {
        SetBkMode(dc, TRANSPARENT as i32);
        SelectObject(dc, GetStockObject(DEFAULT_GUI_FONT));
    }

    text(dc, 24, 18, rgb(229, 241, 248), "WINDOWS TOUCH RECEIVER");
    let Some(state_lock) = STATE.get() else {
        unsafe { EndPaint(hwnd, &paint) };
        return;
    };
    let state = state_lock
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    text(
        dc,
        24,
        48,
        rgb(79, 203, 231),
        &format!(
            "WM_POINTER  active {}   down {}   update {}   up {}",
            state.active.len(),
            state.down_count,
            state.update_count,
            state.up_count
        ),
    );
    text(
        dc,
        24,
        70,
        rgb(130, 153, 168),
        "This window observes Windows messages; it does not read the USB stream.",
    );

    let lane_width = (client.right.max(LANE_COUNT) / LANE_COUNT).max(1);
    for lane in 0..LANE_COUNT {
        let left = lane * lane_width;
        if lane % 2 == 0 {
            fill(
                dc,
                &RECT {
                    left,
                    top: HEADER_HEIGHT,
                    right: if lane == LANE_COUNT - 1 {
                        client.right
                    } else {
                        left + lane_width
                    },
                    bottom: client.bottom,
                },
                rgb(10, 19, 27),
            );
        }
        if lane > 0 {
            fill(
                dc,
                &RECT {
                    left,
                    top: HEADER_HEIGHT,
                    right: left + 1,
                    bottom: client.bottom,
                },
                rgb(31, 54, 70),
            );
        }
        text(
            dc,
            left + 14,
            HEADER_HEIGHT + 14,
            rgb(91, 117, 133),
            &(lane + 1).to_string(),
        );
    }

    unsafe { SelectObject(dc, GetStockObject(NULL_PEN)) };
    for (pointer_id, point) in &state.active {
        let brush = unsafe { CreateSolidBrush(rgb(60, 205, 179)) };
        let previous = unsafe { SelectObject(dc, brush) };
        unsafe {
            Ellipse(dc, point.x - 25, point.y - 25, point.x + 25, point.y + 25);
            SelectObject(dc, previous);
            DeleteObject(brush);
        }
        text(
            dc,
            point.x - 5,
            point.y - 8,
            rgb(6, 30, 27),
            &pointer_id.to_string(),
        );
    }

    let mut y = client.bottom - 82;
    for entry in &state.recent {
        text(dc, 24, y, rgb(151, 171, 184), entry);
        y += 18;
    }
    drop(state);
    unsafe { EndPaint(hwnd, &paint) };
}

fn fill(dc: *mut core::ffi::c_void, rect: &RECT, color: u32) {
    let brush = unsafe { CreateSolidBrush(color) };
    unsafe {
        FillRect(dc, rect, brush);
        DeleteObject(brush);
    }
}

fn text(dc: *mut core::ffi::c_void, x: i32, y: i32, color: u32, value: &str) {
    let wide = value.encode_utf16().collect::<Vec<_>>();
    unsafe {
        SetTextColor(dc, color);
        TextOutW(dc, x, y, wide.as_ptr(), wide.len() as i32);
    }
}

const fn rgb(red: u32, green: u32, blue: u32) -> u32 {
    red | (green << 8) | (blue << 16)
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
