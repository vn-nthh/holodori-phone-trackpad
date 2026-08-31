use std::io;
use std::ptr::null;
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{ERROR_NOT_READY, GetLastError, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
use windows_sys::Win32::UI::Input::Pointer::{
    InitializeTouchInjection, InjectTouchInput, POINTER_FLAG_CANCELED, POINTER_FLAG_DOWN,
    POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE, POINTER_FLAG_UP, POINTER_FLAG_UPDATE,
    POINTER_TOUCH_INFO, TOUCH_FEEDBACK_NONE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetClientRect, PT_TOUCH, TOUCH_MASK_CONTACTAREA, TOUCH_MASK_ORIENTATION,
    TOUCH_MASK_PRESSURE,
};

use crate::protocol::{
    ACTION_CANCEL, ACTION_HEARTBEAT, ACTION_UP, Contact, MAX_CONTACTS, TouchFrame,
};

pub const PROBE_WINDOW_TITLE: &str = "Holodori Touch Probe";

const POINTER_ID_COUNT: usize = u8::MAX as usize + 1;

#[derive(Clone, Debug)]
pub struct TouchTarget {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
    window_title: Vec<u16>,
}

impl TouchTarget {
    pub fn from_window_title(title: &str) -> io::Result<Self> {
        let mut target = Self {
            left: 0,
            top: 0,
            width: 1,
            height: 1,
            window_title: wide(title),
        };
        target.refresh()?;
        Ok(target)
    }

    fn refresh(&mut self) -> io::Result<()> {
        let hwnd = unsafe { FindWindowW(null(), self.window_title.as_ptr()) };
        if hwnd.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "touch target window was closed",
            ));
        }
        let mut client = RECT::default();
        if unsafe { GetClientRect(hwnd, &mut client) } == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut origin = POINT { x: 0, y: 0 };
        if unsafe { ClientToScreen(hwnd, &mut origin) } == 0 {
            return Err(io::Error::last_os_error());
        }

        self.left = origin.x;
        self.top = origin.y;
        self.width = (client.right - client.left).max(1);
        self.height = (client.bottom - client.top).max(1);
        Ok(())
    }

    fn map(&self, contact: &Contact) -> POINT {
        let x = contact.x.clamp(0.0, 1.0);
        let y = contact.y.clamp(0.0, 1.0);
        POINT {
            x: self.left + (x * (self.width - 1) as f32).round() as i32,
            y: self.top + (y * (self.height - 1) as f32).round() as i32,
        }
    }
}

#[derive(Clone, Copy)]
struct ActiveContact {
    location: POINT,
    pressure: u32,
    radius: i32,
}

pub struct TouchInjector {
    target: TouchTarget,
    active: [Option<ActiveContact>; POINTER_ID_COUNT],
    active_count: usize,
    injection: Vec<POINTER_TOUCH_INFO>,
}

impl TouchInjector {
    pub fn new(target: TouchTarget) -> io::Result<Self> {
        if unsafe { InitializeTouchInjection(MAX_CONTACTS as u32, TOUCH_FEEDBACK_NONE) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            target,
            active: [None; POINTER_ID_COUNT],
            active_count: 0,
            injection: Vec::with_capacity(MAX_CONTACTS * 2),
        })
    }

    pub fn has_active_input(&self) -> bool {
        self.active_count > 0
    }

    pub fn accept(&mut self, frame: &TouchFrame) -> io::Result<()> {
        if frame.session_start() || frame.action == ACTION_CANCEL || !frame.locked() {
            return self.cancel_all();
        }
        if frame.action == ACTION_HEARTBEAT && frame.contacts.is_empty() {
            // Protocol-v4 APKs before snapshot heartbeats sent an empty
            // keepalive. Preserve compatibility with those sessions.
            fill_update_contacts(&self.active, &self.active, &mut self.injection);
            if self.injection.is_empty() {
                return Ok(());
            }
            return inject_retry(&self.injection);
        }

        if self.active_count == 0 {
            self.target.refresh()?;
        }
        let mut proposed = [None; POINTER_ID_COUNT];
        let mut present = [false; POINTER_ID_COUNT];
        for contact in &frame.contacts {
            let index = usize::from(contact.pointer_id);
            proposed[index] = Some(self.contact_state(contact));
            present[index] = true;
        }

        if frame.action == ACTION_UP
            && let Some(previous) = self.active[usize::from(frame.action_pointer_id)]
            && let Some(next) = proposed[usize::from(frame.action_pointer_id)]
            && (previous.location.x != next.location.x || previous.location.y != next.location.y)
        {
            fill_update_contacts(&self.active, &proposed, &mut self.injection);
            inject_retry(&self.injection)?;
            self.active = proposed;
            self.active_count = self.active.iter().flatten().count();
        }

        self.injection.clear();
        for (pointer_id, state) in self.active.iter().enumerate() {
            if let Some(state) = state
                && !present[pointer_id]
            {
                self.injection
                    .push(to_touch_info(pointer_id as u8, *state, POINTER_FLAG_UP));
            }
        }
        for contact in &frame.contacts {
            let state = proposed[usize::from(contact.pointer_id)].unwrap();
            let Some(flags) = transition_flags(
                self.active[usize::from(contact.pointer_id)].is_some(),
                contact.touching(),
            ) else {
                // A host can restart after the original UP was accepted by the
                // old process. Treat that replay as already satisfied.
                continue;
            };
            self.injection
                .push(to_touch_info(contact.pointer_id, state, flags));
        }

        if !self.injection.is_empty() {
            inject_retry(&self.injection)?;
        }
        self.active.fill(None);
        self.active_count = 0;
        for contact in &frame.contacts {
            if contact.touching() {
                self.active[usize::from(contact.pointer_id)] =
                    proposed[usize::from(contact.pointer_id)];
                self.active_count += 1;
            }
        }
        Ok(())
    }

    pub fn cancel_all(&mut self) -> io::Result<()> {
        if self.active_count == 0 {
            return Ok(());
        }
        self.injection.clear();
        for (pointer_id, state) in self.active.iter().enumerate() {
            if let Some(state) = state {
                self.injection.push(to_touch_info(
                    pointer_id as u8,
                    *state,
                    POINTER_FLAG_UP | POINTER_FLAG_CANCELED,
                ));
            }
        }
        inject_retry(&self.injection)?;
        self.active.fill(None);
        self.active_count = 0;
        Ok(())
    }

    fn contact_state(&self, contact: &Contact) -> ActiveContact {
        let location = self.target.map(contact);
        let pressure = (contact.pressure.clamp(0.0, 1.0) * 1023.0).round() as u32 + 1;
        let radius = ((contact.touch_major.clamp(0.0, 1.0)
            * self.target.width.min(self.target.height) as f32
            * 0.5)
            .round() as i32)
            .clamp(2, 32);
        ActiveContact {
            location,
            pressure,
            radius,
        }
    }
}

fn fill_update_contacts(
    active: &[Option<ActiveContact>; POINTER_ID_COUNT],
    proposed: &[Option<ActiveContact>; POINTER_ID_COUNT],
    contacts: &mut Vec<POINTER_TOUCH_INFO>,
) {
    contacts.clear();
    for (pointer_id, state) in proposed.iter().enumerate() {
        if active[pointer_id].is_some()
            && let Some(state) = state
        {
            contacts.push(to_touch_info(
                pointer_id as u8,
                *state,
                POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT,
            ));
        }
    }
}

fn transition_flags(was_active: bool, touching: bool) -> Option<u32> {
    match (was_active, touching) {
        (false, true) => Some(POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT),
        (true, true) => Some(POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT),
        (true, false) => Some(POINTER_FLAG_UP),
        (false, false) => None,
    }
}

impl Drop for TouchInjector {
    fn drop(&mut self) {
        let deadline = Instant::now() + Duration::from_millis(8);
        while self.has_active_input() && Instant::now() < deadline {
            if self.cancel_all().is_ok() {
                break;
            }
            thread::yield_now();
        }
    }
}

fn to_touch_info(pointer_id: u8, state: ActiveContact, flags: u32) -> POINTER_TOUCH_INFO {
    let mut info = POINTER_TOUCH_INFO::default();
    info.pointerInfo.pointerType = PT_TOUCH;
    info.pointerInfo.pointerId = u32::from(pointer_id);
    info.pointerInfo.pointerFlags = flags;
    info.pointerInfo.ptPixelLocation = state.location;
    info.touchMask = TOUCH_MASK_CONTACTAREA | TOUCH_MASK_ORIENTATION | TOUCH_MASK_PRESSURE;
    info.rcContact = RECT {
        left: state.location.x - state.radius,
        top: state.location.y - state.radius,
        right: state.location.x + state.radius,
        bottom: state.location.y + state.radius,
    };
    info.orientation = 90;
    info.pressure = state.pressure;
    info
}

fn inject_retry(contacts: &[POINTER_TOUCH_INFO]) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_millis(3);
    loop {
        if unsafe { InjectTouchInput(contacts.len() as u32, contacts.as_ptr()) } != 0 {
            return Ok(());
        }
        let error = unsafe { GetLastError() };
        if error != ERROR_NOT_READY || Instant::now() >= deadline {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        thread::yield_now();
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replayed_move_reestablishes_an_unknown_contact() {
        assert_eq!(
            transition_flags(false, true),
            Some(POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT)
        );
        assert_eq!(
            transition_flags(true, true),
            Some(POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT)
        );
    }

    #[test]
    fn a_replayed_up_for_an_unknown_contact_is_already_satisfied() {
        assert_eq!(transition_flags(false, false), None);
        assert_eq!(transition_flags(true, false), Some(POINTER_FLAG_UP));
    }
}
