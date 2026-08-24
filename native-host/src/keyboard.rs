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
        self.accept_with(frame, submit_inputs)
    }

    fn accept_with<F>(&mut self, frame: &TouchFrame, mut submit: F) -> io::Result<()>
    where
        F: FnMut(&[INPUT]) -> io::Result<usize>,
    {
        if frame.session_start() || frame.action == ACTION_CANCEL || !frame.locked() {
            return self.release_all_with(submit);
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

        let pending = self.pending.as_ref().unwrap();
        let first = pending.applied;
        let remaining = &pending.changes[first..];
        if remaining.is_empty() {
            let completed = self.pending.take().unwrap();
            self.state = completed.next_state;
            return Ok(());
        }

        let inputs: Vec<_> = remaining
            .iter()
            .map(|change| key_input(self.scan_codes[change.lane], change.down))
            .collect();
        let accepted = submit(&inputs)?;
        if accepted > inputs.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "input submission accepted {accepted} of {} requested events",
                    inputs.len()
                ),
            ));
        }

        let applied = first + accepted;
        for index in first..applied {
            let change = self.pending.as_ref().unwrap().changes[index];
            self.pressed[change.lane] = change.down;
        }
        self.pending.as_mut().unwrap().applied = applied;

        if accepted != inputs.len() {
            return Err(incomplete_submission_error(accepted, inputs.len()));
        }

        let completed = self.pending.take().unwrap();
        self.state = completed.next_state;
        Ok(())
    }

    pub fn release_all(&mut self) -> io::Result<()> {
        self.release_all_with(submit_inputs)
    }

    fn release_all_with<F>(&mut self, mut submit: F) -> io::Result<()>
    where
        F: FnMut(&[INPUT]) -> io::Result<usize>,
    {
        self.pending = None;
        let lanes: Vec<_> = self
            .pressed
            .iter()
            .enumerate()
            .filter_map(|(lane, pressed)| pressed.then_some(lane))
            .collect();
        if lanes.is_empty() {
            self.state.pointer_lanes.clear();
            self.state.lane_holds.fill(0);
            return Ok(());
        }

        let inputs: Vec<_> = lanes
            .iter()
            .map(|lane| key_input(self.scan_codes[*lane], false))
            .collect();
        let accepted = submit(&inputs)?;
        if accepted > inputs.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "input submission accepted {accepted} of {} requested events",
                    inputs.len()
                ),
            ));
        }
        for lane in lanes.iter().take(accepted) {
            self.pressed[*lane] = false;
        }
        if accepted != inputs.len() {
            return Err(incomplete_submission_error(accepted, inputs.len()));
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
            if contact.touching() {
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

fn submit_inputs(inputs: &[INPUT]) -> io::Result<usize> {
    if inputs.is_empty() {
        return Ok(0);
    }
    let count = u32::try_from(inputs.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many keyboard events for one SendInput submission",
        )
    })?;
    let accepted = unsafe { SendInput(count, inputs.as_ptr(), size_of::<INPUT>() as i32) } as usize;
    if accepted == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(accepted)
}

fn incomplete_submission_error(accepted: usize, requested: usize) -> io::Error {
    io::Error::other(format!(
        "input submission accepted {accepted} of {requested} requested events"
    ))
}

#[cfg(test)]
mod tests {
    use std::mem::ManuallyDrop;

    use super::*;
    use crate::protocol::{
        ACTION_CANCEL, ACTION_DOWN, ACTION_MOVE, CONTACT_FLAG_INSIDE, CONTACT_FLAG_TIP, Contact,
        FRAME_FLAG_LOCKED, FRAME_FLAG_SESSION_START,
    };

    #[test]
    fn lane_mapping_clamps_both_edges() {
        assert_eq!(lane_for(-0.5, 6), 0);
        assert_eq!(lane_for(0.49, 6), 2);
        assert_eq!(lane_for(1.0, 6), 5);
        assert_eq!(lane_for(2.0, 6), 5);
    }

    #[test]
    fn tip_without_inside_owns_the_clamped_edge_lane() {
        let sink = test_sink(4);

        let (left_state, left) = sink.plan(&frame(
            1,
            ACTION_DOWN,
            0,
            FRAME_FLAG_LOCKED,
            vec![contact(0, CONTACT_FLAG_TIP, -0.5)],
        ));
        assert_changes(&left, &[(0, true)]);
        assert_eq!(left_state.pointer_lanes.get(&0), Some(&0));
        assert_eq!(left_state.lane_holds, [1, 0, 0, 0]);

        let (right_state, right) = sink.plan(&frame(
            1,
            ACTION_DOWN,
            0,
            FRAME_FLAG_LOCKED,
            vec![contact(0, CONTACT_FLAG_TIP, 1.5)],
        ));
        assert_changes(&right, &[(3, true)]);
        assert_eq!(right_state.pointer_lanes.get(&0), Some(&3));
        assert_eq!(right_state.lane_holds, [0, 0, 0, 1]);
    }

    #[test]
    fn outside_tip_hold_releases_only_when_tip_clears_or_contact_is_missing() {
        let mut sink = test_sink(4);
        let (down_state, down) = sink.plan(&frame(
            1,
            ACTION_DOWN,
            0,
            FRAME_FLAG_LOCKED,
            vec![contact(0, CONTACT_FLAG_INSIDE | CONTACT_FLAG_TIP, 0.05)],
        ));
        assert_changes(&down, &[(0, true)]);
        sink.state = down_state;

        let (outside_state, outside) = sink.plan(&frame(
            2,
            ACTION_MOVE,
            0,
            FRAME_FLAG_LOCKED,
            vec![contact(0, CONTACT_FLAG_TIP, -0.25)],
        ));
        assert_changes(&outside, &[]);
        assert_eq!(outside_state.pointer_lanes.get(&0), Some(&0));
        assert_eq!(outside_state.lane_holds, [1, 0, 0, 0]);

        sink.state = outside_state.clone();
        let (tip_clear_state, tip_clear) = sink.plan(&frame(
            3,
            ACTION_MOVE,
            0,
            FRAME_FLAG_LOCKED,
            vec![contact(0, CONTACT_FLAG_INSIDE, 0.05)],
        ));
        assert_changes(&tip_clear, &[(0, false)]);
        assert!(tip_clear_state.pointer_lanes.is_empty());
        assert_eq!(tip_clear_state.lane_holds, [0, 0, 0, 0]);

        sink.state = outside_state;
        let (missing_state, missing) =
            sink.plan(&frame(3, ACTION_MOVE, 0, FRAME_FLAG_LOCKED, vec![]));
        assert_changes(&missing, &[(0, false)]);
        assert!(missing_state.pointer_lanes.is_empty());
        assert_eq!(missing_state.lane_holds, [0, 0, 0, 0]);
    }

    #[test]
    fn outside_contacts_reference_count_the_same_edge_lane() {
        let mut sink = test_sink(4);
        let (both_state, both) = sink.plan(&frame(
            1,
            ACTION_DOWN,
            0,
            FRAME_FLAG_LOCKED,
            vec![
                contact(0, CONTACT_FLAG_TIP, 1.1),
                contact(1, CONTACT_FLAG_TIP, 1.2),
            ],
        ));
        assert_changes(&both, &[(3, true)]);
        assert_eq!(both_state.lane_holds, [0, 0, 0, 2]);
        sink.state = both_state;

        let (one_state, one) = sink.plan(&frame(
            2,
            ACTION_MOVE,
            1,
            FRAME_FLAG_LOCKED,
            vec![contact(1, CONTACT_FLAG_TIP, 1.3)],
        ));
        assert_changes(&one, &[]);
        assert_eq!(one_state.pointer_lanes.len(), 1);
        assert_eq!(one_state.pointer_lanes.get(&1), Some(&3));
        assert_eq!(one_state.lane_holds, [0, 0, 0, 1]);
        sink.state = one_state;

        let (none_state, none) = sink.plan(&frame(
            3,
            ACTION_MOVE,
            1,
            FRAME_FLAG_LOCKED,
            vec![contact(1, 0, 1.3)],
        ));
        assert_changes(&none, &[(3, false)]);
        assert!(none_state.pointer_lanes.is_empty());
        assert_eq!(none_state.lane_holds, [0, 0, 0, 0]);
    }

    #[test]
    fn outside_slide_plan_emits_every_crossed_lane_without_a_gap() {
        let mut sink = test_sink(4);
        let (down_state, down) = sink.plan(&touch_frame(ACTION_DOWN, 0.05));
        assert_changes(&down, &[(0, true)]);
        sink.state = down_state;

        let (_, slide) = sink.plan(&frame(
            2,
            ACTION_MOVE,
            0,
            FRAME_FLAG_LOCKED,
            vec![contact(0, CONTACT_FLAG_TIP, 1.5)],
        ));
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

    #[test]
    fn cancel_unlock_and_session_start_batch_all_releases() {
        for (action, flags) in [
            (ACTION_CANCEL, FRAME_FLAG_LOCKED),
            (ACTION_MOVE, 0),
            (ACTION_MOVE, FRAME_FLAG_LOCKED | FRAME_FLAG_SESSION_START),
        ] {
            let mut sink = held_sink(4, &[(0, 0), (1, 3)]);
            let mut batches = Vec::new();

            sink.accept_with(&frame(2, action, 0, flags, vec![]), |inputs: &[INPUT]| {
                batches.push(decode_inputs(inputs));
                Ok(inputs.len())
            })
            .unwrap();

            assert_eq!(batches, [vec![(0, false), (3, false)]]);
            assert!(sink.state.pointer_lanes.is_empty());
            assert_eq!(sink.state.lane_holds, [0, 0, 0, 0]);
            assert_eq!(sink.pressed, [false, false, false, false]);
        }
    }

    #[test]
    fn accept_submits_the_whole_planned_frame_in_exact_order() {
        let mut sink = held_sink(4, &[(0, 0)]);
        let slide = frame(
            2,
            ACTION_MOVE,
            0,
            FRAME_FLAG_LOCKED,
            vec![contact(0, CONTACT_FLAG_INSIDE | CONTACT_FLAG_TIP, 0.99)],
        );
        let mut batches = Vec::new();

        sink.accept_with(&slide, |inputs: &[INPUT]| {
            batches.push(decode_inputs(inputs));
            Ok(inputs.len())
        })
        .unwrap();

        assert_eq!(
            batches,
            [vec![
                (1, true),
                (0, false),
                (2, true),
                (1, false),
                (3, true),
                (2, false),
            ]]
        );
        assert_eq!(sink.state.pointer_lanes.get(&0), Some(&3));
        assert_eq!(sink.state.lane_holds, [0, 0, 0, 1]);
        assert_eq!(sink.pressed, [false, false, false, true]);
        assert!(sink.pending.is_none());
    }

    #[test]
    fn accept_records_a_partial_prefix_and_retries_only_the_suffix() {
        let mut sink = held_sink(4, &[(0, 0)]);
        let slide = frame(
            2,
            ACTION_MOVE,
            0,
            FRAME_FLAG_LOCKED,
            vec![contact(0, CONTACT_FLAG_TIP, 1.5)],
        );
        let mut first_calls = 0;
        let mut first_batch = Vec::new();

        let error = sink
            .accept_with(&slide, |inputs: &[INPUT]| {
                first_calls += 1;
                first_batch = decode_inputs(inputs);
                Ok(2)
            })
            .unwrap_err();

        assert_eq!(first_calls, 1);
        assert_eq!(
            first_batch,
            [
                (1, true),
                (0, false),
                (2, true),
                (1, false),
                (3, true),
                (2, false),
            ]
        );
        assert!(error.to_string().contains("accepted 2 of 6"));
        assert_eq!(sink.pending.as_ref().unwrap().applied, 2);
        assert_eq!(sink.state.pointer_lanes.get(&0), Some(&0));
        assert_eq!(sink.state.lane_holds, [1, 0, 0, 0]);
        assert_eq!(sink.pressed, [false, true, false, false]);

        let mut future_submitted = false;
        let future_error = sink
            .accept_with(
                &frame(
                    3,
                    ACTION_MOVE,
                    0,
                    FRAME_FLAG_LOCKED,
                    vec![contact(0, CONTACT_FLAG_TIP, 0.5)],
                ),
                |_: &[INPUT]| {
                    future_submitted = true;
                    Ok(0)
                },
            )
            .unwrap_err();
        assert!(!future_submitted);
        assert!(future_error.to_string().contains("sequence 2"));

        let mut retry_calls = 0;
        let mut retry_batch = Vec::new();
        sink.accept_with(&slide, |inputs: &[INPUT]| {
            retry_calls += 1;
            retry_batch = decode_inputs(inputs);
            Ok(inputs.len())
        })
        .unwrap();

        assert_eq!(retry_calls, 1);
        assert_eq!(retry_batch, [(2, true), (1, false), (3, true), (2, false)]);
        assert_eq!(sink.state.pointer_lanes.get(&0), Some(&3));
        assert_eq!(sink.state.lane_holds, [0, 0, 0, 1]);
        assert_eq!(sink.pressed, [false, false, false, true]);
        assert!(sink.pending.is_none());
    }

    #[test]
    fn release_all_records_a_partial_prefix_and_retries_remaining_keys() {
        let mut sink = held_sink(5, &[(0, 0), (1, 2), (2, 4)]);
        let mut first_calls = 0;
        let mut first_batch = Vec::new();

        let error = sink
            .release_all_with(|inputs: &[INPUT]| {
                first_calls += 1;
                first_batch = decode_inputs(inputs);
                Ok(2)
            })
            .unwrap_err();

        assert_eq!(first_calls, 1);
        assert_eq!(first_batch, [(0, false), (2, false), (4, false)]);
        assert!(error.to_string().contains("accepted 2 of 3"));
        assert_eq!(sink.pressed, [false, false, false, false, true]);
        assert_eq!(sink.state.pointer_lanes.len(), 3);
        assert_eq!(sink.state.lane_holds, [1, 0, 1, 0, 1]);

        let mut retry_calls = 0;
        let mut retry_batch = Vec::new();
        sink.release_all_with(|inputs: &[INPUT]| {
            retry_calls += 1;
            retry_batch = decode_inputs(inputs);
            Ok(inputs.len())
        })
        .unwrap();

        assert_eq!(retry_calls, 1);
        assert_eq!(retry_batch, [(4, false)]);
        assert_eq!(sink.pressed, [false, false, false, false, false]);
        assert!(sink.state.pointer_lanes.is_empty());
        assert_eq!(sink.state.lane_holds, [0, 0, 0, 0, 0]);
    }

    #[test]
    fn unchanged_frame_commits_without_calling_the_submitter() {
        let mut sink = held_sink(4, &[(0, 0)]);
        let mut submitted = false;

        sink.accept_with(
            &frame(
                2,
                ACTION_MOVE,
                0,
                FRAME_FLAG_LOCKED,
                vec![contact(0, CONTACT_FLAG_TIP, -0.5)],
            ),
            |_: &[INPUT]| {
                submitted = true;
                Ok(0)
            },
        )
        .unwrap();

        assert!(!submitted);
        assert_eq!(sink.state.pointer_lanes.get(&0), Some(&0));
        assert_eq!(sink.pressed, [true, false, false, false]);
    }

    fn test_sink(lanes: usize) -> ManuallyDrop<KeyboardSink> {
        ManuallyDrop::new(KeyboardSink {
            scan_codes: (1..=lanes)
                .map(|scan_code| u16::try_from(scan_code).unwrap())
                .collect(),
            state: KeyboardState {
                pointer_lanes: BTreeMap::new(),
                lane_holds: vec![0; lanes],
            },
            pressed: vec![false; lanes],
            pending: None,
        })
    }

    fn held_sink(lanes: usize, pointers: &[(u8, usize)]) -> ManuallyDrop<KeyboardSink> {
        let mut sink = test_sink(lanes);
        for (pointer_id, lane) in pointers {
            sink.state.pointer_lanes.insert(*pointer_id, *lane);
            sink.state.lane_holds[*lane] += 1;
            sink.pressed[*lane] = true;
        }
        sink
    }

    fn touch_frame(action: u8, x: f32) -> TouchFrame {
        frame(
            1,
            action,
            0,
            FRAME_FLAG_LOCKED,
            vec![contact(0, CONTACT_FLAG_INSIDE | CONTACT_FLAG_TIP, x)],
        )
    }

    fn frame(
        sequence: u64,
        action: u8,
        action_pointer_id: u8,
        flags: u8,
        contacts: Vec<Contact>,
    ) -> TouchFrame {
        TouchFrame {
            session_id: 1,
            sequence,
            phone_event_nanos: 0,
            phone_callback_nanos: 0,
            phone_send_nanos: 0,
            echo_host_send_nanos: 0,
            phone_control_receive_nanos: 0,
            action,
            action_pointer_id,
            flags,
            contacts,
        }
    }

    fn contact(pointer_id: u8, flags: u8, x: f32) -> Contact {
        Contact {
            pointer_id,
            flags,
            x,
            y: 0.5,
            pressure: 0.5,
            touch_major: 0.1,
        }
    }

    fn assert_changes(actual: &[KeyChange], expected: &[(usize, bool)]) {
        let actual: Vec<_> = actual
            .iter()
            .map(|change| (change.lane, change.down))
            .collect();
        assert_eq!(actual, expected);
    }

    fn decode_inputs(inputs: &[INPUT]) -> Vec<(usize, bool)> {
        inputs
            .iter()
            .map(|input| {
                assert_eq!(input.r#type, INPUT_KEYBOARD);
                let keyboard = unsafe { input.Anonymous.ki };
                assert_eq!(keyboard.wVk, 0);
                assert_ne!(keyboard.dwFlags & KEYEVENTF_SCANCODE, 0);
                (
                    usize::from(keyboard.wScan) - 1,
                    keyboard.dwFlags & KEYEVENTF_KEYUP == 0,
                )
            })
            .collect()
    }
}
