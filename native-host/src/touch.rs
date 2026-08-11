use std::collections::BTreeMap;
use std::io;
use std::ptr::null;
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{ERROR_NOT_READY, GetLastError, HWND, POINT, RECT};
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

#[derive(Clone, Copy, Debug)]
pub struct TouchTarget {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

impl TouchTarget {
    pub fn from_window_title(title: &str) -> io::Result<Self> {
        let wide_title = wide(title);
        let hwnd = unsafe { FindWindowW(null(), wide_title.as_ptr()) };
        if hwnd.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("window not found: {title:?}"),
            ));
        }
        Self::from_client(hwnd)
    }

    fn from_client(hwnd: HWND) -> io::Result<Self> {
        let mut client = RECT::default();
        if unsafe { GetClientRect(hwnd, &mut client) } == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut origin = POINT { x: 0, y: 0 };
        if unsafe { ClientToScreen(hwnd, &mut origin) } == 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            left: origin.x,
            top: origin.y,
            width: (client.right - client.left).max(1),
            height: (client.bottom - client.top).max(1),
        })
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
    active: BTreeMap<u8, ActiveContact>,
}

impl TouchInjector {
    pub fn new(target: TouchTarget) -> io::Result<Self> {
        if unsafe { InitializeTouchInjection(MAX_CONTACTS as u32, TOUCH_FEEDBACK_NONE) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            target,
            active: BTreeMap::new(),
        })
    }

    pub fn set_target(&mut self, target: TouchTarget) {
        self.target = target;
    }

    pub fn has_active_input(&self) -> bool {
        !self.active.is_empty()
    }

    pub fn accept(&mut self, frame: &TouchFrame) -> io::Result<()> {
        if frame.session_start() || frame.action == ACTION_CANCEL || !frame.locked() {
            return self.cancel_all();
        }
        if frame.action == ACTION_HEARTBEAT && frame.contacts.is_empty() {
            // Protocol-v4 APKs before snapshot heartbeats sent an empty
            // keepalive. Preserve compatibility with those sessions.
            let contacts = self.make_update_contacts(&self.active);
            if contacts.is_empty() {
                return Ok(());
            }
            return inject_retry(&contacts);
        }

        let proposed = self.proposed_contacts(frame);

        if frame.action == ACTION_UP
            && let Some(previous) = self.active.get(&frame.action_pointer_id)
            && let Some(next) = proposed.get(&frame.action_pointer_id)
            && (previous.location.x != next.location.x || previous.location.y != next.location.y)
        {
            let update = self.make_update_contacts(&proposed);
            inject_retry(&update)?;
            self.active = proposed.clone();
        }

        let mut contacts = Vec::with_capacity(proposed.len().max(1));
        for contact in &frame.contacts {
            let Some(state) = proposed.get(&contact.pointer_id).copied() else {
                continue;
            };
            let Some(flags) = transition_flags(
                self.active.contains_key(&contact.pointer_id),
                contact.touching(),
            ) else {
                // A host can restart after the original UP was accepted by the
                // old process. Treat that replay as already satisfied.
                continue;
            };
            contacts.push(to_touch_info(contact.pointer_id, state, flags));
        }

        if contacts.is_empty() {
            self.active.clear();
            return Ok(());
        }
        inject_retry(&contacts)?;

        self.active = proposed
            .into_iter()
            .filter(|(pointer_id, _)| {
                frame
                    .contacts
                    .iter()
                    .find(|contact| contact.pointer_id == *pointer_id)
                    .is_some_and(Contact::touching)
            })
            .collect();
        Ok(())
    }

    pub fn cancel_all(&mut self) -> io::Result<()> {
        if self.active.is_empty() {
            return Ok(());
        }
        let contacts: Vec<_> = self
            .active
            .iter()
            .map(|(pointer_id, state)| {
                to_touch_info(*pointer_id, *state, POINTER_FLAG_UP | POINTER_FLAG_CANCELED)
            })
            .collect();
        inject_retry(&contacts)?;
        self.active.clear();
        Ok(())
    }

    fn proposed_contacts(&self, frame: &TouchFrame) -> BTreeMap<u8, ActiveContact> {
        frame
            .contacts
            .iter()
            .map(|contact| {
                let location = self.target.map(contact);
                let pressure = (contact.pressure.clamp(0.0, 1.0) * 1023.0).round() as u32 + 1;
                let radius = ((contact.touch_major.clamp(0.0, 1.0)
                    * self.target.width.min(self.target.height) as f32
                    * 0.5)
                    .round() as i32)
                    .clamp(2, 32);
                (
                    contact.pointer_id,
                    ActiveContact {
                        location,
                        pressure,
                        radius,
                    },
                )
            })
            .collect()
    }

    fn make_update_contacts(
        &self,
        contacts: &BTreeMap<u8, ActiveContact>,
    ) -> Vec<POINTER_TOUCH_INFO> {
        contacts
            .iter()
            .filter(|(pointer_id, _)| self.active.contains_key(pointer_id))
            .map(|(pointer_id, state)| {
                to_touch_info(
                    *pointer_id,
                    *state,
                    POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT,
                )
            })
            .collect()
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
