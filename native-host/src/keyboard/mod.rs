use std::collections::BTreeSet;
use std::io;

use crate::protocol::{ACTION_CANCEL, ACTION_HEARTBEAT, MAX_CONTACTS, TouchFrame};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows::KeySink;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux::KeySink;

const POINTER_ID_COUNT: usize = u8::MAX as usize + 1;

// Every contact can traverse every lane, followed by releases for omitted contacts.
fn max_key_changes(lanes: usize) -> usize {
    MAX_CONTACTS * (2 * lanes + 1)
}

#[derive(Clone)]
struct KeyboardState {
    pointer_lanes: [Option<usize>; POINTER_ID_COUNT],
    lane_holds: Vec<u16>,
}

impl KeyboardState {
    fn new(lanes: usize) -> Self {
        Self {
            pointer_lanes: [None; POINTER_ID_COUNT],
            lane_holds: vec![0; lanes],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct KeyChange {
    lane: usize,
    down: bool,
}

struct PendingFrame {
    sequence: Option<u64>,
    next_state: KeyboardState,
    changes: Vec<KeyChange>,
    applied: usize,
}

/// The lane-key bridge. Holds the platform-specific OS input sink plus the
/// shared, platform-neutral slide/hold/chord interpretation state.
pub struct KeyboardSink {
    sink: KeySink,
    state: KeyboardState,
    pressed: Vec<bool>,
    pending: PendingFrame,
}

impl KeyboardSink {
    pub fn new(keys: &[String]) -> io::Result<Self> {
        if keys.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "at least one lane key is required",
            ));
        }
        validate_lane_keys(keys)?;
        let sink = KeySink::new(keys)?;
        let lanes = sink.lane_count();
        let state = KeyboardState::new(lanes);
        Ok(Self {
            state: state.clone(),
            pressed: vec![false; lanes],
            pending: PendingFrame {
                sequence: None,
                next_state: state,
                changes: Vec::with_capacity(max_key_changes(lanes)),
                applied: 0,
            },
            sink,
        })
    }

    pub fn lane_count(&self) -> u8 {
        self.pressed.len().min(u8::MAX as usize) as u8
    }

    pub fn has_active_input(&self) -> bool {
        self.pressed.iter().any(|pressed| *pressed) || self.sink.has_pending_submission()
    }

    pub fn accept(&mut self, frame: &TouchFrame) -> io::Result<()> {
        self.accept_with(frame, KeySink::submit)
    }

    #[cfg(test)]
    pub(crate) fn accept_recorded(&mut self, frame: &TouchFrame) -> io::Result<()> {
        self.accept_with(frame, KeySink::submit_recorded)
    }

    #[cfg(test)]
    pub(crate) fn cancel_recorded(&mut self) -> io::Result<()> {
        self.release_all_with(KeySink::submit_recorded)
    }

    fn accept_with<F>(&mut self, frame: &TouchFrame, mut submit: F) -> io::Result<()>
    where
        F: FnMut(&mut KeySink, &[KeyChange]) -> io::Result<usize>,
    {
        if frame.session_start() || frame.action == ACTION_CANCEL || !frame.locked() {
            self.sink.discard_pending()?;
            return self.release_all_with(submit);
        }
        if frame.action == ACTION_HEARTBEAT && frame.contacts.is_empty() {
            // Older protocol-v4 APKs sent empty heartbeats. Newer APKs include
            // the latest snapshot so a restarted host can restore held keys.
            return Ok(());
        }
        let pending = &mut self.pending;
        if let Some(sequence) = pending.sequence
            && sequence != frame.sequence
        {
            return Err(io::Error::other(format!(
                "sequence {} is still partially applied",
                sequence
            )));
        }

        if pending.sequence.is_none() {
            plan_into(
                &self.state,
                self.pressed.len(),
                frame,
                &mut pending.next_state,
                &mut pending.changes,
            );
            pending.sequence = Some(frame.sequence);
            pending.applied = 0;
        }

        let first = pending.applied;
        let remaining = &pending.changes[first..];
        if remaining.is_empty() {
            std::mem::swap(&mut self.state, &mut pending.next_state);
            pending.sequence = None;
            return Ok(());
        }

        let remaining_len = remaining.len();
        let accepted = submit(&mut self.sink, remaining)?;
        if accepted > remaining_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "input submission accepted {accepted} of {} requested events",
                    remaining_len
                ),
            ));
        }

        let applied = first + accepted;
        for index in first..applied {
            let change = pending.changes[index];
            self.pressed[change.lane] = change.down;
        }
        pending.applied = applied;

        if accepted != remaining_len {
            return Err(incomplete_submission_error(accepted, remaining_len));
        }

        std::mem::swap(&mut self.state, &mut pending.next_state);
        pending.sequence = None;
        Ok(())
    }

    pub fn release_all(&mut self) -> io::Result<()> {
        self.sink.discard_pending()?;
        self.release_all_with(KeySink::submit)
    }

    fn release_all_with<F>(&mut self, mut submit: F) -> io::Result<()>
    where
        F: FnMut(&mut KeySink, &[KeyChange]) -> io::Result<usize>,
    {
        let pending = &mut self.pending;
        pending.sequence = None;
        pending.changes.clear();
        pending.applied = 0;
        for (lane, pressed) in self.pressed.iter().enumerate() {
            if *pressed {
                pending.changes.push(KeyChange { lane, down: false });
            }
        }
        if pending.changes.is_empty() {
            self.state.pointer_lanes.fill(None);
            self.state.lane_holds.fill(0);
            return Ok(());
        }

        let accepted = submit(&mut self.sink, &pending.changes)?;
        if accepted > pending.changes.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "input submission accepted {accepted} of {} requested events",
                    pending.changes.len()
                ),
            ));
        }
        for change in pending.changes.iter().take(accepted) {
            self.pressed[change.lane] = false;
        }
        if accepted != pending.changes.len() {
            return Err(incomplete_submission_error(accepted, pending.changes.len()));
        }

        self.state.pointer_lanes.fill(None);
        self.state.lane_holds.fill(0);
        Ok(())
    }

    #[cfg(test)]
    fn plan(&self, frame: &TouchFrame) -> (KeyboardState, Vec<KeyChange>) {
        let mut next = KeyboardState::new(self.pressed.len());
        let mut changes = Vec::new();
        plan_into(
            &self.state,
            self.pressed.len(),
            frame,
            &mut next,
            &mut changes,
        );
        (next, changes)
    }
}

fn validate_lane_keys(keys: &[String]) -> io::Result<()> {
    let mut seen = BTreeSet::new();
    for key in keys {
        let normalized = key.to_ascii_uppercase();
        if !seen.insert(normalized) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("lane key {key:?} is duplicated; every lane key must be unique"),
            ));
        }
    }
    Ok(())
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

/// Interprets one wire frame against the current lane-hold state, producing
/// the next state and the ordered key-down/key-up changes needed to reach it.
/// Kept free of any OS sink so unit tests can exercise slide/hold/chord logic
/// without opening a real input device.
fn plan_into(
    state: &KeyboardState,
    lane_count: usize,
    frame: &TouchFrame,
    next: &mut KeyboardState,
    changes: &mut Vec<KeyChange>,
) {
    next.pointer_lanes.copy_from_slice(&state.pointer_lanes);
    next.lane_holds.copy_from_slice(&state.lane_holds);
    changes.clear();

    // Android's action pointer must be applied first, but sorting a copied
    // snapshot is unnecessary work on the live path. Two bounded passes keep
    // the exact ordering without allocating or sorting a temporary list.
    if let Some(contact) = frame
        .contacts
        .iter()
        .find(|contact| contact.pointer_id == frame.action_pointer_id)
    {
        apply_contact(contact, lane_count, next, changes);
    }
    for contact in &frame.contacts {
        if contact.pointer_id != frame.action_pointer_id {
            apply_contact(contact, lane_count, next, changes);
        }
    }

    let mut present = [false; POINTER_ID_COUNT];
    for contact in &frame.contacts {
        present[usize::from(contact.pointer_id)] = true;
    }
    let mut missing = [0_u8; POINTER_ID_COUNT];
    let mut missing_len = 0;
    for (pointer_id, lane) in next.pointer_lanes.iter().enumerate() {
        if lane.is_some() && !present[pointer_id] {
            missing[missing_len] = pointer_id as u8;
            missing_len += 1;
        }
    }
    for &pointer_id in &missing[..missing_len] {
        release_pointer(pointer_id, next, changes);
    }
    // A complete snapshot can transfer ownership between fingers. If a lane
    // is owned both before and after the frame, keep its asserted key state
    // continuous instead of exposing pointer-iteration order as an UP/DOWN.
    changes
        .retain(|change| state.lane_holds[change.lane] == 0 || next.lane_holds[change.lane] == 0);
}

fn apply_contact(
    contact: &crate::protocol::Contact,
    lane_count: usize,
    state: &mut KeyboardState,
    changes: &mut Vec<KeyChange>,
) {
    // Android reports a still-touching contact just outside the locked play
    // rectangle without the INSIDE flag. It still owns the clamped edge lane
    // until TIP clears or the complete snapshot omits it.
    if contact.touching() {
        let lane = lane_for(contact.x, lane_count);
        move_pointer(contact.pointer_id, lane, state, changes);
    } else {
        release_pointer(contact.pointer_id, state, changes);
    }
}

fn move_pointer(
    pointer_id: u8,
    destination: usize,
    state: &mut KeyboardState,
    changes: &mut Vec<KeyChange>,
) {
    let pointer_index = usize::from(pointer_id);
    let Some(mut current) = state.pointer_lanes[pointer_index] else {
        acquire_lane(destination, state, changes);
        state.pointer_lanes[pointer_index] = Some(destination);
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
    state.pointer_lanes[pointer_index] = Some(destination);
}

fn release_pointer(pointer_id: u8, state: &mut KeyboardState, changes: &mut Vec<KeyChange>) {
    if let Some(lane) = state.pointer_lanes[usize::from(pointer_id)].take() {
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

fn incomplete_submission_error(accepted: usize, requested: usize) -> io::Error {
    io::Error::other(format!(
        "input submission accepted {accepted} of {requested} requested events"
    ))
}
#[cfg(test)]
pub(crate) mod tests {
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
    fn duplicate_lane_keys_are_rejected_case_insensitively() {
        assert!(validate_lane_keys(&["s".to_owned(), "D".to_owned()]).is_ok());
        let error = validate_lane_keys(&["s".to_owned(), "S".to_owned()]).unwrap_err();
        assert!(error.to_string().contains("must be unique"));
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
        assert_eq!(left_state.pointer_lanes[0], Some(0));
        assert_eq!(left_state.lane_holds, [1, 0, 0, 0]);

        let (right_state, right) = sink.plan(&frame(
            1,
            ACTION_DOWN,
            0,
            FRAME_FLAG_LOCKED,
            vec![contact(0, CONTACT_FLAG_TIP, 1.5)],
        ));
        assert_changes(&right, &[(3, true)]);
        assert_eq!(right_state.pointer_lanes[0], Some(3));
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
        assert_eq!(outside_state.pointer_lanes[0], Some(0));
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
        assert_eq!(active_pointer_count(&tip_clear_state), 0);
        assert_eq!(tip_clear_state.lane_holds, [0, 0, 0, 0]);

        sink.state = outside_state;
        let (missing_state, missing) =
            sink.plan(&frame(3, ACTION_MOVE, 0, FRAME_FLAG_LOCKED, vec![]));
        assert_changes(&missing, &[(0, false)]);
        assert_eq!(active_pointer_count(&missing_state), 0);
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
        assert_eq!(active_pointer_count(&one_state), 1);
        assert_eq!(one_state.pointer_lanes[1], Some(3));
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
        assert_eq!(active_pointer_count(&none_state), 0);
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
    fn simultaneous_lane_swap_keeps_the_chord_asserted() {
        let sink = held_sink(4, &[(0, 0), (1, 1)]);

        let (next, changes) = sink.plan(&frame(
            2,
            ACTION_MOVE,
            0,
            FRAME_FLAG_LOCKED,
            vec![
                contact(0, CONTACT_FLAG_TIP, 0.3),
                contact(1, CONTACT_FLAG_TIP, 0.1),
            ],
        ));

        assert_changes(&changes, &[]);
        assert_eq!(next.pointer_lanes[0], Some(1));
        assert_eq!(next.pointer_lanes[1], Some(0));
        assert_eq!(next.lane_holds, [1, 1, 0, 0]);
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

            sink.accept_with(
                &frame(2, action, 0, flags, vec![]),
                |_, inputs: &[KeyChange]| {
                    batches.push(decode_inputs(inputs));
                    Ok(inputs.len())
                },
            )
            .unwrap();

            assert_eq!(batches, [vec![(0, false), (3, false)]]);
            assert_eq!(active_pointer_count(&sink.state), 0);
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

        sink.accept_with(&slide, |_, inputs: &[KeyChange]| {
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
        assert_eq!(sink.state.pointer_lanes[0], Some(3));
        assert_eq!(sink.state.lane_holds, [0, 0, 0, 1]);
        assert_eq!(sink.pressed, [false, false, false, true]);
        assert!(sink.pending.sequence.is_none());
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
            .accept_with(&slide, |_, inputs: &[KeyChange]| {
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
        assert_eq!(sink.pending.applied, 2);
        assert_eq!(sink.state.pointer_lanes[0], Some(0));
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
                |_, _: &[KeyChange]| {
                    future_submitted = true;
                    Ok(0)
                },
            )
            .unwrap_err();
        assert!(!future_submitted);
        assert!(future_error.to_string().contains("sequence 2"));

        let mut retry_calls = 0;
        let mut retry_batch = Vec::new();
        sink.accept_with(&slide, |_, inputs: &[KeyChange]| {
            retry_calls += 1;
            retry_batch = decode_inputs(inputs);
            Ok(inputs.len())
        })
        .unwrap();

        assert_eq!(retry_calls, 1);
        assert_eq!(retry_batch, [(2, true), (1, false), (3, true), (2, false)]);
        assert_eq!(sink.state.pointer_lanes[0], Some(3));
        assert_eq!(sink.state.lane_holds, [0, 0, 0, 1]);
        assert_eq!(sink.pressed, [false, false, false, true]);
        assert!(sink.pending.sequence.is_none());
    }

    #[test]
    fn release_all_records_a_partial_prefix_and_retries_remaining_keys() {
        let mut sink = held_sink(5, &[(0, 0), (1, 2), (2, 4)]);
        let mut first_calls = 0;
        let mut first_batch = Vec::new();

        let error = sink
            .release_all_with(|_, inputs: &[KeyChange]| {
                first_calls += 1;
                first_batch = decode_inputs(inputs);
                Ok(2)
            })
            .unwrap_err();

        assert_eq!(first_calls, 1);
        assert_eq!(first_batch, [(0, false), (2, false), (4, false)]);
        assert!(error.to_string().contains("accepted 2 of 3"));
        assert_eq!(sink.pressed, [false, false, false, false, true]);
        assert_eq!(active_pointer_count(&sink.state), 3);
        assert_eq!(sink.state.lane_holds, [1, 0, 1, 0, 1]);

        let mut retry_calls = 0;
        let mut retry_batch = Vec::new();
        sink.release_all_with(|_, inputs: &[KeyChange]| {
            retry_calls += 1;
            retry_batch = decode_inputs(inputs);
            Ok(inputs.len())
        })
        .unwrap();

        assert_eq!(retry_calls, 1);
        assert_eq!(retry_batch, [(4, false)]);
        assert_eq!(sink.pressed, [false, false, false, false, false]);
        assert_eq!(active_pointer_count(&sink.state), 0);
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
            |_, _: &[KeyChange]| {
                submitted = true;
                Ok(0)
            },
        )
        .unwrap();

        assert!(!submitted);
        assert_eq!(sink.state.pointer_lanes[0], Some(0));
        assert_eq!(sink.pressed, [true, false, false, false]);
    }

    pub(crate) fn test_sink(lanes: usize) -> ManuallyDrop<KeyboardSink> {
        let state = KeyboardState::new(lanes);
        ManuallyDrop::new(KeyboardSink {
            sink: KeySink::for_test(lanes),
            state: state.clone(),
            pressed: vec![false; lanes],
            pending: PendingFrame {
                sequence: None,
                next_state: state,
                changes: Vec::with_capacity(max_key_changes(lanes)),
                applied: 0,
            },
        })
    }

    fn held_sink(lanes: usize, pointers: &[(u8, usize)]) -> ManuallyDrop<KeyboardSink> {
        let mut sink = test_sink(lanes);
        for (pointer_id, lane) in pointers {
            sink.state.pointer_lanes[usize::from(*pointer_id)] = Some(*lane);
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
            contacts: contacts.into_iter().collect(),
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

    fn active_pointer_count(state: &KeyboardState) -> usize {
        state.pointer_lanes.iter().flatten().count()
    }

    fn decode_inputs(inputs: &[KeyChange]) -> Vec<(usize, bool)> {
        inputs
            .iter()
            .map(|input| (input.lane, input.down))
            .collect()
    }
}
