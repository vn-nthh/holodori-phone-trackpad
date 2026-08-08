use std::collections::BTreeMap;
use std::io;
use std::mem::size_of;

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
    MAPVK_VK_TO_VSC, MapVirtualKeyW, SendInput,
};

use crate::protocol::{ACTION_CANCEL, ACTION_HEARTBEAT, TouchFrame};

#[derive(Clone)]
struct KeyboardState {
    pointer_lanes: BTreeMap<u8, usize>,
    lane_holds: Vec<u16>,
}

#[derive(Clone, Copy)]
struct KeyChange {
    lane: usize,
    down: bool,
}

struct PendingFrame {
    sequence: u64,
    next_state: KeyboardState,
    changes: Vec<KeyChange>,
    applied: usize,
}

pub struct KeyboardSink {
    scan_codes: Vec<u16>,
    state: KeyboardState,
    pressed: Vec<bool>,
    pending: Option<PendingFrame>,
}

impl KeyboardSink {
    pub fn new(keys: &[String]) -> io::Result<Self> {
        if keys.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "at least one lane key is required",
            ));
        }
        let mut scan_codes = Vec::with_capacity(keys.len());
        for key in keys {
            let vk = parse_virtual_key(key)?;
            let scan_code = unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC) };
            if scan_code == 0 || scan_code > u32::from(u16::MAX) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("cannot map lane key {key:?} to a scan code"),
                ));
            }
            scan_codes.push(scan_code as u16);
        }
        Ok(Self {
            state: KeyboardState {
                pointer_lanes: BTreeMap::new(),
                lane_holds: vec![0; scan_codes.len()],
            },
            pressed: vec![false; scan_codes.len()],
            pending: None,
            scan_codes,
        })
    }

    pub fn lane_count(&self) -> u8 {
        self.scan_codes.len().min(u8::MAX as usize) as u8
    }

    pub fn has_active_input(&self) -> bool {
        self.pressed.iter().any(|pressed| *pressed)
    }

    pub fn accept(&mut self, frame: &TouchFrame) -> io::Result<()> {
        if frame.session_start() || frame.action == ACTION_CANCEL || !frame.locked() {
            return self.release_all();
        }
        if frame.action == ACTION_HEARTBEAT && frame.contacts.is_empty() {
            // Older protocol-v4 APKs sent empty heartbeats. Newer APKs include
            // the latest snapshot so a restarted host can restore held keys.
            return Ok(());
        }
        if let Some(pending) = &self.pending
            && pending.sequence != frame.sequence
        {
            return Err(io::Error::other(format!(
                "sequence {} is still partially applied",
                pending.sequence
            )));
        }

        if self.pending.is_none() {
            let (next_state, changes) = self.plan(frame);
            self.pending = Some(PendingFrame {
                sequence: frame.sequence,
                next_state,
                changes,
                applied: 0,
            });
        }

        loop {
            let Some(pending) = &self.pending else {
                break;
            };
            let Some(change) = pending.changes.get(pending.applied).copied() else {
                let completed = self.pending.take().unwrap();
                self.state = completed.next_state;
                break;
            };
            send_one(key_input(self.scan_codes[change.lane], change.down))?;
            self.pressed[change.lane] = change.down;
            self.pending.as_mut().unwrap().applied += 1;
        }
        Ok(())
    }

    pub fn release_all(&mut self) -> io::Result<()> {
        self.pending = None;
        let mut first_error = None;
        for lane in 0..self.pressed.len() {
            if self.pressed[lane] {
                match send_one(key_input(self.scan_codes[lane], false)) {
                    Ok(()) => self.pressed[lane] = false,
                    Err(error) if first_error.is_none() => first_error = Some(error),
                    Err(_) => {}
                }
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        self.state.pointer_lanes.clear();
        self.state.lane_holds.fill(0);
        Ok(())
    }

    fn plan(&self, frame: &TouchFrame) -> (KeyboardState, Vec<KeyChange>) {
        let mut next = self.state.clone();
        let mut changes = Vec::new();

        let mut contacts = frame.contacts.iter().collect::<Vec<_>>();
        contacts.sort_by_key(|contact| {
            if contact.pointer_id == frame.action_pointer_id {
                0
            } else {
                1
            }
        });
        for contact in contacts {
            if contact.touching() && contact.inside() {
                let lane = lane_for(contact.x, self.scan_codes.len());
                move_pointer(contact.pointer_id, lane, &mut next, &mut changes);
            } else {
                release_pointer(contact.pointer_id, &mut next, &mut changes);
            }
        }

        let present: Vec<u8> = frame
            .contacts
            .iter()
            .map(|contact| contact.pointer_id)
            .collect();
        let missing: Vec<u8> = next
            .pointer_lanes
            .keys()
            .filter(|pointer_id| !present.contains(pointer_id))
            .copied()
            .collect();
        for pointer_id in missing {
            release_pointer(pointer_id, &mut next, &mut changes);
        }
        (next, changes)
    }
}

impl Drop for KeyboardSink {
    fn drop(&mut self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(8);
        while self.has_active_input() && std::time::Instant::now() < deadline {
            if self.release_all().is_ok() {
                break;
            }
            std::thread::yield_now();
        }
    }
}

fn move_pointer(
    pointer_id: u8,
    destination: usize,
    state: &mut KeyboardState,
    changes: &mut Vec<KeyChange>,
) {
    let Some(mut current) = state.pointer_lanes.get(&pointer_id).copied() else {
        acquire_lane(destination, state, changes);
        state.pointer_lanes.insert(pointer_id, destination);
        return;
    };
    while current != destination {
        let next = if destination > current {
            current + 1
        } else {
            current - 1
        };
        // A slide never has a no-key gap.
        acquire_lane(next, state, changes);
        release_lane(current, state, changes);
        current = next;
    }
    state.pointer_lanes.insert(pointer_id, destination);
}

fn release_pointer(pointer_id: u8, state: &mut KeyboardState, changes: &mut Vec<KeyChange>) {
    if let Some(lane) = state.pointer_lanes.remove(&pointer_id) {
        release_lane(lane, state, changes);
    }
}

fn acquire_lane(lane: usize, state: &mut KeyboardState, changes: &mut Vec<KeyChange>) {
    if state.lane_holds[lane] == 0 {
        changes.push(KeyChange { lane, down: true });
    }
    state.lane_holds[lane] = state.lane_holds[lane].saturating_add(1);
}

fn release_lane(lane: usize, state: &mut KeyboardState, changes: &mut Vec<KeyChange>) {
    if state.lane_holds[lane] == 0 {
        return;
    }
    state.lane_holds[lane] -= 1;
    if state.lane_holds[lane] == 0 {
        changes.push(KeyChange { lane, down: false });
    }
}

fn lane_for(x: f32, lane_count: usize) -> usize {
    ((x.clamp(0.0, 1.0) * lane_count as f32).floor() as usize).min(lane_count - 1)
}

fn parse_virtual_key(key: &str) -> io::Result<u16> {
    let mut chars = key.chars();
    let Some(character) = chars.next() else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty key"));
    };
    if chars.next().is_some() || !character.is_ascii_alphanumeric() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("lane key {key:?} must be one ASCII letter or digit"),
        ));
    }
    Ok(character.to_ascii_uppercase() as u16)
}

fn key_input(scan_code: u16, down: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: 0,
                wScan: scan_code,
                dwFlags: KEYEVENTF_SCANCODE | if down { 0 } else { KEYEVENTF_KEYUP },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send_one(event: INPUT) -> io::Result<()> {
    let accepted = unsafe { SendInput(1, &event, size_of::<INPUT>() as i32) };
    if accepted != 1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        ACTION_DOWN, ACTION_MOVE, CONTACT_FLAG_INSIDE, CONTACT_FLAG_TIP, Contact, FRAME_FLAG_LOCKED,
    };

    #[test]
    fn lane_mapping_clamps_both_edges() {
        assert_eq!(lane_for(-0.5, 6), 0);
        assert_eq!(lane_for(0.49, 6), 2);
        assert_eq!(lane_for(1.0, 6), 5);
        assert_eq!(lane_for(2.0, 6), 5);
    }

    #[test]
    fn slide_plan_emits_every_crossed_lane_without_a_gap() {
        let mut sink = test_sink(4);
        let (down_state, down) = sink.plan(&touch_frame(ACTION_DOWN, 0.05));
        assert_changes(&down, &[(0, true)]);
        sink.state = down_state;

        let (_, slide) = sink.plan(&touch_frame(ACTION_MOVE, 0.99));
        assert_changes(
            &slide,
            &[
                (1, true),
                (0, false),
                (2, true),
                (1, false),
                (3, true),
                (2, false),
            ],
        );
    }

    fn test_sink(lanes: usize) -> KeyboardSink {
        KeyboardSink {
            scan_codes: vec![1; lanes],
            state: KeyboardState {
                pointer_lanes: BTreeMap::new(),
                lane_holds: vec![0; lanes],
            },
            pressed: vec![false; lanes],
            pending: None,
        }
    }

    fn touch_frame(action: u8, x: f32) -> TouchFrame {
        TouchFrame {
            session_id: 1,
            sequence: 1,
            phone_event_nanos: 0,
            phone_callback_nanos: 0,
            phone_send_nanos: 0,
            echo_host_send_nanos: 0,
            phone_control_receive_nanos: 0,
            action,
            action_pointer_id: 0,
            flags: FRAME_FLAG_LOCKED,
            contacts: vec![Contact {
                pointer_id: 0,
                flags: CONTACT_FLAG_INSIDE | CONTACT_FLAG_TIP,
                x,
                y: 0.5,
                pressure: 0.5,
                touch_major: 0.1,
            }],
        }
    }

    fn assert_changes(actual: &[KeyChange], expected: &[(usize, bool)]) {
        let actual: Vec<_> = actual
            .iter()
            .map(|change| (change.lane, change.down))
            .collect();
        assert_eq!(actual, expected);
    }
}
