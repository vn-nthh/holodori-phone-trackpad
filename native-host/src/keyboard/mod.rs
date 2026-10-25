use std::collections::BTreeSet;
use std::io;
use std::time::{Duration, Instant};

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
    pressure_suppressed: [bool; POINTER_ID_COUNT],
    lane_holds: Vec<u16>,
}

impl KeyboardState {
    fn new(lanes: usize) -> Self {
        Self {
            pointer_lanes: [None; POINTER_ID_COUNT],
            pressure_suppressed: [false; POINTER_ID_COUNT],
            lane_holds: vec![0; lanes],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct KeyChange {
    lane: usize,
    down: bool,
    /// A press caused by a finger landing (or being admitted), not by a
    /// slide entering the lane. Only re-press spacing reads it.
    touch: bool,
}

struct PendingFrame {
    sequence: Option<u64>,
    next_state: KeyboardState,
    changes: Vec<KeyChange>,
    applied: usize,
}

/// Beta: keeps same-lane edges from reaching Windows in the same instant.
///
/// The gap is a fixed 1-2 ms separation, not an attempt to reproduce the
/// phone's timing or to outlast a game frame; holding input for a full frame
/// would break the latency budget.
///
/// Without spacing, a release and re-press of one lane can reach Windows
/// microseconds apart (a repaired burst, or two fingers alternating on one
/// lane), and a second finger landing on a held lane is only a repeat
/// key-down. A state-polling game sees neither as a new press. With spacing,
/// a re-press waits until `gap` after the lane's release, an edge following
/// a spaced edge also waits `gap`, and a press on an already held lane
/// becomes release-then-press. The release after a finger-down press also
/// waits `gap`, so a tap repaired in one burst is not invisibly short; a
/// slide's crossed lanes stay unspaced. Releases never overtake an earlier
/// waiting press, so slides keep their no-gap guarantee. A lane keeps at most
/// `MAX_WAITING_EDGES` waiting edges (one extra complete tap); beyond that a
/// new edge cancels the newest waiting one, bounding queue size and delay.
struct RepressSpacer {
    gap: Duration,
    queue: Vec<ScheduledEdge>,
    last_emitted: Vec<Option<EmittedEdge>>,
    batch: Vec<KeyChange>,
    selected: Vec<usize>,
    lane_selected: Vec<bool>,
    lane_waiting: Vec<bool>,
}

#[derive(Clone, Copy)]
struct ScheduledEdge {
    change: KeyChange,
    due: Instant,
    spaced: bool,
}

#[derive(Clone, Copy)]
struct EmittedEdge {
    at: Instant,
    spaced: bool,
    touch: bool,
}

/// Whether `next` must trail the lane's previous edge by the gap: it is a
/// re-press, follows a spaced edge, or releases a finger-down press.
fn needs_gap(previous_spaced: bool, previous_touch: bool, next: KeyChange) -> bool {
    next.down || previous_spaced || previous_touch
}

const MAX_WAITING_EDGES: usize = 3;

impl RepressSpacer {
    fn new(lanes: usize, gap: Duration) -> Self {
        Self {
            gap,
            queue: Vec::with_capacity(MAX_WAITING_EDGES * lanes),
            last_emitted: vec![None; lanes],
            batch: Vec::with_capacity(MAX_WAITING_EDGES * lanes),
            selected: Vec::with_capacity(MAX_WAITING_EDGES * lanes),
            lane_selected: vec![false; lanes],
            lane_waiting: vec![false; lanes],
        }
    }

    fn enqueue(&mut self, change: KeyChange, now: Instant, os_pressed: bool) {
        let lane = change.lane;
        let mut waiting = 0;
        let mut last = None;
        for (index, edge) in self.queue.iter().enumerate() {
            if edge.change.lane == lane {
                waiting += 1;
                last = Some(index);
            }
        }
        let scheduled_down = last.map_or(os_pressed, |index| self.queue[index].change.down);
        if scheduled_down == change.down {
            return;
        }
        let (due, spaced) = match last {
            Some(index) if waiting >= MAX_WAITING_EDGES => {
                // The newest waiting edge and this opposite one cancel out,
                // leaving the lane in the state this edge asked for.
                self.queue.remove(index);
                return;
            }
            Some(index) => {
                let previous = self.queue[index];
                if needs_gap(previous.spaced, previous.change.touch, change) {
                    ((previous.due + self.gap).max(now), true)
                } else {
                    (previous.due.max(now), false)
                }
            }
            None => match self.last_emitted[lane] {
                Some(previous)
                    if needs_gap(previous.spaced, previous.touch, change)
                        && now < previous.at + self.gap =>
                {
                    (previous.at + self.gap, true)
                }
                _ => (now, false),
            },
        };
        self.queue.push(ScheduledEdge {
            change,
            due,
            spaced,
        });
    }

    fn flush<F>(
        &mut self,
        now: Instant,
        sink: &mut KeySink,
        pressed: &mut [bool],
        submit: &mut F,
    ) -> io::Result<()>
    where
        F: FnMut(&mut KeySink, &[KeyChange]) -> io::Result<usize>,
    {
        if self.queue.is_empty() {
            return Ok(());
        }
        self.batch.clear();
        self.selected.clear();
        self.lane_selected.fill(false);
        self.lane_waiting.fill(false);
        let mut press_waiting = false;
        for (index, edge) in self.queue.iter().enumerate() {
            let lane = edge.change.lane;
            let ready = edge.due <= now
                && !self.lane_waiting[lane]
                && !(edge.spaced && self.lane_selected[lane])
                && (edge.change.down || !press_waiting);
            if ready {
                self.selected.push(index);
                self.batch.push(edge.change);
                self.lane_selected[lane] = true;
            } else {
                self.lane_waiting[lane] = true;
                press_waiting |= edge.change.down;
            }
        }
        if self.batch.is_empty() {
            return Ok(());
        }

        let requested = self.batch.len();
        let accepted = submit(sink, &self.batch)?;
        if accepted > requested {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("input submission accepted {accepted} of {requested} requested events"),
            ));
        }
        for &index in &self.selected[..accepted] {
            let edge = self.queue[index];
            pressed[edge.change.lane] = edge.change.down;
            self.last_emitted[edge.change.lane] = Some(EmittedEdge {
                at: now,
                spaced: edge.spaced,
                touch: edge.change.touch,
            });
        }
        let mut position = 0;
        let mut emitted = 0;
        let selected = &self.selected[..accepted];
        self.queue.retain(|_| {
            let keep = selected.get(emitted) != Some(&position);
            emitted += usize::from(!keep);
            position += 1;
            keep
        });
        // A late flush must not collapse a spaced follower onto its
        // predecessor; restart its gap from the real emission time.
        for edge in &mut self.queue {
            if edge.spaced
                && self.last_emitted[edge.change.lane].is_some_and(|previous| previous.at == now)
            {
                edge.due = edge.due.max(now + self.gap);
            }
        }
        if accepted != requested {
            return Err(incomplete_submission_error(accepted, requested));
        }
        Ok(())
    }

    fn next_deadline(&self, now: Instant) -> Option<Instant> {
        if self.queue.is_empty() {
            return None;
        }
        // After a failed flush everything left may already be due.
        Some(
            self.queue
                .iter()
                .map(|edge| edge.due)
                .filter(|due| *due > now)
                .min()
                .unwrap_or(now),
        )
    }

    fn note_release(&mut self, lane: usize, now: Instant) {
        self.last_emitted[lane] = Some(EmittedEdge {
            at: now,
            spaced: false,
            touch: false,
        });
    }
}

/// The lane-key bridge. Holds the platform-specific OS input sink plus the
/// shared, platform-neutral slide/hold/chord interpretation state.
pub struct KeyboardSink {
    sink: KeySink,
    state: KeyboardState,
    pressed: Vec<bool>,
    pending: PendingFrame,
    spacer: Option<RepressSpacer>,
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
            spacer: None,
        })
    }

    /// Beta: space same-lane re-presses by `gap` (see `RepressSpacer`).
    pub fn set_repress_gap(&mut self, gap: Duration) {
        self.spacer = Some(RepressSpacer::new(self.pressed.len(), gap));
    }

    pub fn lane_count(&self) -> u8 {
        self.pressed.len().min(u8::MAX as usize) as u8
    }

    pub fn has_active_input(&self) -> bool {
        self.pressed.iter().any(|pressed| *pressed)
            || self.sink.has_pending_submission()
            || self
                .spacer
                .as_ref()
                .is_some_and(|spacer| !spacer.queue.is_empty())
    }

    pub fn accept(&mut self, frame: &TouchFrame) -> io::Result<()> {
        self.accept_at(frame, Instant::now(), KeySink::submit)
    }

    /// Submits deferred re-press edges that are now due.
    pub fn service(&mut self) -> io::Result<()> {
        self.service_at(Instant::now(), KeySink::submit)
    }

    /// The earliest instant `service` has deferred work, if any.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.spacer
            .as_ref()
            .and_then(|spacer| spacer.next_deadline(Instant::now()))
    }

    #[cfg(test)]
    pub(crate) fn accept_recorded(&mut self, frame: &TouchFrame) -> io::Result<()> {
        self.accept_with(frame, KeySink::submit_recorded)
    }

    #[cfg(test)]
    pub(crate) fn cancel_recorded(&mut self) -> io::Result<()> {
        self.release_all_with(Instant::now(), KeySink::submit_recorded)
    }

    fn service_at<F>(&mut self, now: Instant, mut submit: F) -> io::Result<()>
    where
        F: FnMut(&mut KeySink, &[KeyChange]) -> io::Result<usize>,
    {
        match self.spacer.as_mut() {
            Some(spacer) => spacer.flush(now, &mut self.sink, &mut self.pressed, &mut submit),
            None => Ok(()),
        }
    }

    #[cfg(test)]
    fn accept_with<F>(&mut self, frame: &TouchFrame, submit: F) -> io::Result<()>
    where
        F: FnMut(&mut KeySink, &[KeyChange]) -> io::Result<usize>,
    {
        self.accept_at(frame, Instant::now(), submit)
    }

    fn accept_at<F>(&mut self, frame: &TouchFrame, now: Instant, mut submit: F) -> io::Result<()>
    where
        F: FnMut(&mut KeySink, &[KeyChange]) -> io::Result<usize>,
    {
        if frame.session_start() || frame.action == ACTION_CANCEL || !frame.locked() {
            self.sink.discard_pending()?;
            return self.release_all_with(now, submit);
        }
        // Earlier deferred edges stay ahead of this frame. A failure here
        // leaves this frame unplanned, so its retry repeats only the flush.
        self.service_at(now, &mut submit)?;
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
                self.spacer.is_some(),
                &mut pending.next_state,
                &mut pending.changes,
            );
            pending.sequence = Some(frame.sequence);
            pending.applied = 0;
        }

        if let Some(spacer) = self.spacer.as_mut() {
            // The spacer owns OS submission: queue the planned changes once,
            // then emit whatever is due. A partial emission returns an error
            // with this frame still pending; the retry's flush finishes it.
            for change in &pending.changes[pending.applied..] {
                spacer.enqueue(*change, now, self.pressed[change.lane]);
            }
            pending.applied = pending.changes.len();
            spacer.flush(now, &mut self.sink, &mut self.pressed, &mut submit)?;
            std::mem::swap(&mut self.state, &mut pending.next_state);
            pending.sequence = None;
            return Ok(());
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
        self.release_all_with(Instant::now(), KeySink::submit)
    }

    fn release_all_with<F>(&mut self, now: Instant, mut submit: F) -> io::Result<()>
    where
        F: FnMut(&mut KeySink, &[KeyChange]) -> io::Result<usize>,
    {
        let pending = &mut self.pending;
        pending.sequence = None;
        pending.changes.clear();
        pending.applied = 0;
        // Deferred edges belong to the cancelled input; never emit them later.
        if let Some(spacer) = self.spacer.as_mut() {
            spacer.queue.clear();
        }
        for (lane, pressed) in self.pressed.iter().enumerate() {
            if *pressed {
                pending.changes.push(KeyChange {
                    lane,
                    down: false,
                    touch: false,
                });
            }
        }
        if pending.changes.is_empty() {
            self.state.pointer_lanes.fill(None);
            self.state.pressure_suppressed.fill(false);
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
            if let Some(spacer) = self.spacer.as_mut() {
                spacer.note_release(change.lane, now);
            }
        }
        if accepted != pending.changes.len() {
            return Err(incomplete_submission_error(accepted, pending.changes.len()));
        }

        self.state.pointer_lanes.fill(None);
        self.state.pressure_suppressed.fill(false);
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
            self.spacer.is_some(),
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

/// Failsafe for a host that exited without its own cleanup (killed or
/// crashed): Windows keeps injected keys down after the injecting process
/// dies, so the launcher sends one release for every lane key. Releasing an
/// already released key is harmless. Linux needs no equivalent: the kernel
/// releases a uinput device's keys when its owner exits.
#[cfg(windows)]
pub fn release_lane_keys(keys: &[String]) -> io::Result<()> {
    let mut sink = KeySink::new(keys)?;
    let changes: Vec<KeyChange> = (0..sink.lane_count())
        .map(|lane| KeyChange {
            lane,
            down: false,
            touch: false,
        })
        .collect();
    let accepted = sink.submit(&changes)?;
    if accepted != changes.len() {
        return Err(incomplete_submission_error(accepted, changes.len()));
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
    split_repress: bool,
    next: &mut KeyboardState,
    changes: &mut Vec<KeyChange>,
) {
    next.pointer_lanes.copy_from_slice(&state.pointer_lanes);
    next.pressure_suppressed.fill(false);
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
        next.pressure_suppressed[usize::from(contact.pointer_id)] =
            contact.touching() && contact.key_suppressed();
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

    // A distinct finger-down is a press even when another finger owns the
    // lane. By default do not synthesize an UP: the existing hold must remain
    // asserted, so the press is a repeat key-down. Beta re-press spacing
    // instead releases and re-presses; the spacer delays that press.
    // A pressure-gated finger may first qualify in a MOVE/history sample.
    // Commit that admission once, including when it shares an already-held lane.
    for contact in &frame.contacts {
        let pointer = usize::from(contact.pointer_id);
        let new_press = (frame.action == crate::protocol::ACTION_DOWN
            && contact.pointer_id == frame.action_pointer_id)
            || state.pressure_suppressed[pointer];
        if new_press
            && state.pointer_lanes[pointer].is_none()
            && let Some(lane) = next.pointer_lanes[pointer]
            && state.lane_holds[lane] > 0
        {
            if split_repress {
                changes.push(KeyChange {
                    lane,
                    down: false,
                    touch: false,
                });
            }
            changes.push(KeyChange {
                lane,
                down: true,
                touch: true,
            });
        }
    }
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
    if contact.touching() && !contact.key_suppressed() {
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
        acquire_lane(destination, true, state, changes);
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
        acquire_lane(next, false, state, changes);
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

fn acquire_lane(lane: usize, touch: bool, state: &mut KeyboardState, changes: &mut Vec<KeyChange>) {
    if state.lane_holds[lane] == 0 {
        changes.push(KeyChange {
            lane,
            down: true,
            touch,
        });
    }
    state.lane_holds[lane] = state.lane_holds[lane].saturating_add(1);
}

fn release_lane(lane: usize, state: &mut KeyboardState, changes: &mut Vec<KeyChange>) {
    if state.lane_holds[lane] == 0 {
        return;
    }
    state.lane_holds[lane] -= 1;
    if state.lane_holds[lane] == 0 {
        changes.push(KeyChange {
            lane,
            down: false,
            touch: false,
        });
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
    fn slide_lift_then_immediate_tap_reuses_pointer_without_losing_press() {
        let mut sink = held_sink(6, &[(0, 0)]);
        let mut emitted = Vec::new();
        for frame in [
            frame(
                2,
                ACTION_MOVE,
                0,
                FRAME_FLAG_LOCKED,
                vec![contact(0, CONTACT_FLAG_TIP, 0.4)],
            ),
            frame(
                3,
                crate::protocol::ACTION_UP,
                0,
                FRAME_FLAG_LOCKED,
                vec![contact(0, 0, 0.4)],
            ),
            frame(
                4,
                ACTION_DOWN,
                0,
                FRAME_FLAG_LOCKED,
                vec![contact(0, CONTACT_FLAG_TIP, 0.4)],
            ),
        ] {
            sink.accept_with(&frame, |_, changes| {
                emitted.extend(decode_inputs(changes));
                Ok(changes.len())
            })
            .unwrap();
        }
        assert_eq!(
            emitted,
            [
                (1, true),
                (0, false),
                (2, true),
                (1, false),
                (2, false),
                (2, true),
            ]
        );
        assert_eq!(sink.state.lane_holds, [0, 0, 1, 0, 0, 0]);
    }

    #[test]
    fn rapid_slide_lift_tap_survives_reordering_for_each_lane_and_finger() {
        use crate::input::{InputSink, commit_ready};
        use crate::metrics::HostMetrics;
        use crate::protocol::{ACTION_UP, OrderedFrames};
        use std::sync::atomic::AtomicBool;

        struct Recorder {
            sink: ManuallyDrop<KeyboardSink>,
            changes: Vec<(usize, bool)>,
        }
        impl InputSink for Recorder {
            fn accept(&mut self, frame: &TouchFrame) -> io::Result<()> {
                self.sink.accept_with(frame, |_, changes| {
                    self.changes.extend(decode_inputs(changes));
                    Ok(changes.len())
                })
            }
            fn has_active_input(&self) -> bool {
                self.sink.has_active_input()
            }
            fn cancel_all(&mut self) -> io::Result<()> {
                self.sink.cancel_recorded()
            }
        }

        for tap_pointer in [0, 1] {
            for tap_lane in 0..6 {
                for delivery in [[0, 1, 2, 3], [2, 3, 1, 0 /* repaired slide arrives last */]] {
                    let x = (tap_lane as f32 + 0.5) / 6.0;
                    let mut frames = [
                        frame(
                            2,
                            ACTION_MOVE,
                            0,
                            FRAME_FLAG_LOCKED,
                            vec![contact(0, CONTACT_FLAG_TIP, 0.4)],
                        ),
                        frame(3, ACTION_UP, 0, FRAME_FLAG_LOCKED, vec![contact(0, 0, 0.4)]),
                        frame(
                            4,
                            ACTION_DOWN,
                            tap_pointer,
                            FRAME_FLAG_LOCKED,
                            vec![contact(tap_pointer, CONTACT_FLAG_TIP, x)],
                        ),
                        frame(
                            5,
                            ACTION_UP,
                            tap_pointer,
                            FRAME_FLAG_LOCKED,
                            vec![contact(tap_pointer, 0, x)],
                        ),
                    ];
                    for (index, frame) in frames.iter_mut().enumerate() {
                        frame.phone_event_nanos = (index as u64 + 1) * 1_000_000;
                    }
                    let mut ordered = OrderedFrames::new();
                    ordered.begin_session(&frames[0]);
                    let mut recorder = Recorder {
                        sink: held_sink(6, &[(0, 0)]),
                        changes: Vec::new(),
                    };
                    let mut metrics = HostMetrics::new(false, 8.333, 5);
                    for index in delivery {
                        ordered.push(frames[index].clone());
                        // An immediate redundant copy must not repeat a press.
                        ordered.push(frames[index].clone());
                        commit_ready(
                            &mut ordered,
                            &mut recorder,
                            &mut metrics,
                            &AtomicBool::new(false),
                        )
                        .unwrap();
                    }
                    assert_eq!(
                        recorder.changes,
                        [
                            (1, true),
                            (0, false),
                            (2, true),
                            (1, false),
                            (2, false),
                            (tap_lane, true),
                            (tap_lane, false),
                        ],
                        "pointer={tap_pointer}, lane={tap_lane}, delivery={delivery:?}"
                    );
                    assert_eq!(ordered.acknowledged_sequence(), Some(5));
                    assert!(!recorder.has_active_input());
                }
            }
        }
    }

    #[test]
    fn tap_overlapping_slide_endpoint_keeps_key_held_until_both_lift() {
        let mut sink = held_sink(6, &[(0, 2)]);
        let mut emitted = Vec::new();
        for frame in [
            frame(
                2,
                ACTION_DOWN,
                1,
                FRAME_FLAG_LOCKED,
                vec![
                    contact(0, CONTACT_FLAG_TIP, 0.4),
                    contact(1, CONTACT_FLAG_TIP, 0.4),
                ],
            ),
            frame(
                3,
                crate::protocol::ACTION_UP,
                0,
                FRAME_FLAG_LOCKED,
                vec![contact(0, 0, 0.4), contact(1, CONTACT_FLAG_TIP, 0.4)],
            ),
            frame(
                4,
                crate::protocol::ACTION_UP,
                1,
                FRAME_FLAG_LOCKED,
                vec![contact(1, 0, 0.4)],
            ),
        ] {
            sink.accept_with(&frame, |_, changes| {
                emitted.extend(decode_inputs(changes));
                Ok(changes.len())
            })
            .unwrap();
        }
        assert_eq!(emitted, [(2, true), (2, false)]);
        assert!(!sink.has_active_input());
    }

    #[test]
    fn overlapping_tap_retries_once_and_lifting_tapper_preserves_slide() {
        let mut sink = held_sink(6, &[(0, 2)]);
        let mut tap = frame(
            2,
            ACTION_DOWN,
            1,
            FRAME_FLAG_LOCKED,
            vec![
                contact(0, CONTACT_FLAG_TIP, 0.4),
                contact(1, CONTACT_FLAG_TIP, 0.4),
            ],
        );
        assert!(
            sink.accept_with(&tap, |_, changes| {
                assert_changes(changes, &[(2, true)]);
                Ok(0)
            })
            .is_err()
        );
        let mut emitted = Vec::new();
        sink.accept_with(&tap, |_, changes| {
            emitted.extend(decode_inputs(changes));
            Ok(changes.len())
        })
        .unwrap();
        assert_eq!(emitted, [(2, true)]);
        assert_eq!(sink.state.lane_holds[2], 2);
        for action in [ACTION_DOWN, ACTION_MOVE, ACTION_HEARTBEAT] {
            tap.sequence += 1;
            tap.action = action;
            sink.accept_with(&tap, |_, _| panic!("snapshot repeated tap"))
                .unwrap();
        }
        tap.sequence += 1;
        tap.action = crate::protocol::ACTION_UP;
        tap.contacts = vec![contact(0, CONTACT_FLAG_TIP, 0.4), contact(1, 0, 0.4)]
            .into_iter()
            .collect();
        sink.accept_with(&tap, |_, _| panic!("tapper released slide"))
            .unwrap();
        assert_eq!(sink.state.lane_holds[2], 1);
        assert!(sink.pressed[2]);
    }

    #[test]
    fn pressure_admission_preserves_shared_holds_and_retries_once() {
        let mut sink = held_sink(6, &[(0, 2)]);
        let suppressed = CONTACT_FLAG_TIP | crate::protocol::CONTACT_FLAG_KEY_SUPPRESSED;
        let mut sample = frame(
            1,
            ACTION_DOWN,
            1,
            FRAME_FLAG_LOCKED,
            vec![
                contact(0, CONTACT_FLAG_TIP, 0.4),
                contact(1, suppressed, 0.4),
            ],
        );
        sink.accept_with(&sample, |_, _| panic!("light brush sent a key"))
            .unwrap();
        assert_eq!(sink.state.lane_holds[2], 1);
        sample.sequence += 1;
        sample.action = ACTION_MOVE;
        sample.contacts = vec![
            contact(0, CONTACT_FLAG_TIP, 0.4),
            contact(1, CONTACT_FLAG_TIP, 0.4),
        ]
        .into_iter()
        .collect();
        assert!(
            sink.accept_with(&sample, |_, changes| {
                assert_changes(changes, &[(2, true)]);
                Ok(0)
            })
            .is_err()
        );
        sink.accept_with(&sample, |_, changes| {
            assert_changes(changes, &[(2, true)]);
            Ok(changes.len())
        })
        .unwrap();
        assert_eq!(sink.state.lane_holds[2], 2);
        sample.sequence += 1;
        sample.action = ACTION_HEARTBEAT;
        sink.accept_with(&sample, |_, _| panic!("heartbeat repeated admission"))
            .unwrap();
        sample.sequence += 1;
        sample.action = crate::protocol::ACTION_UP;
        sample.contacts = vec![contact(0, CONTACT_FLAG_TIP, 0.4), contact(1, 0, 0.4)]
            .into_iter()
            .collect();
        sink.accept_with(&sample, |_, _| {
            panic!("tapper released another finger's hold")
        })
        .unwrap();
        assert_eq!(sink.state.lane_holds[2], 1);
        sink.cancel_recorded().unwrap();
        assert!(!sink.state.pressure_suppressed.iter().any(|value| *value));
        assert!(!sink.has_active_input());
    }

    #[test]
    fn pressure_recovery_reconstructs_only_admitted_contacts_then_slides_every_lane() {
        let mut sink = test_sink(6);
        let suppressed = CONTACT_FLAG_TIP | crate::protocol::CONTACT_FLAG_KEY_SUPPRESSED;
        let mut admitted = contact(0, CONTACT_FLAG_TIP, 0.01);
        admitted.pressure = 0.0; // Already admitted on the phone, now barely touching.
        let snapshot = frame(
            1,
            ACTION_HEARTBEAT,
            0,
            FRAME_FLAG_LOCKED,
            vec![admitted, contact(1, suppressed, 0.4)],
        );
        sink.accept_with(&snapshot, |_, changes| {
            assert_changes(changes, &[(0, true)]);
            Ok(changes.len())
        })
        .unwrap();
        admitted.x = 0.99;
        let slide = frame(
            2,
            ACTION_MOVE,
            0,
            FRAME_FLAG_LOCKED,
            vec![admitted, contact(1, suppressed, 0.4)],
        );
        sink.accept_with(&slide, |_, changes| {
            assert_changes(
                changes,
                &[
                    (1, true),
                    (0, false),
                    (2, true),
                    (1, false),
                    (3, true),
                    (2, false),
                    (4, true),
                    (3, false),
                    (5, true),
                    (4, false),
                ],
            );
            Ok(changes.len())
        })
        .unwrap();
        assert_eq!(sink.state.lane_holds, [0, 0, 0, 0, 0, 1]);
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
            .release_all_with(Instant::now(), |_, inputs: &[KeyChange]| {
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
        sink.release_all_with(Instant::now(), |_, inputs: &[KeyChange]| {
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

    const GAP: Duration = Duration::from_millis(2);

    fn spaced_sink(lanes: usize) -> ManuallyDrop<KeyboardSink> {
        let mut sink = test_sink(lanes);
        sink.set_repress_gap(GAP);
        sink
    }

    fn lane_x(lane: usize) -> f32 {
        (lane as f32 + 0.5) / 6.0
    }

    fn step(sink: &mut KeyboardSink, at: Instant, frame: &TouchFrame) -> Vec<(usize, bool)> {
        let mut emitted = Vec::new();
        sink.accept_at(frame, at, |_, changes| {
            emitted.extend(decode_inputs(changes));
            Ok(changes.len())
        })
        .unwrap();
        emitted
    }

    fn tick(sink: &mut KeyboardSink, at: Instant) -> Vec<(usize, bool)> {
        let mut emitted = Vec::new();
        sink.service_at(at, |_, changes| {
            emitted.extend(decode_inputs(changes));
            Ok(changes.len())
        })
        .unwrap();
        emitted
    }

    fn deadline(sink: &KeyboardSink, at: Instant) -> Option<Instant> {
        sink.spacer.as_ref().unwrap().next_deadline(at)
    }

    #[test]
    fn spaced_repress_waits_one_gap_after_release_and_stays_visible() {
        let t0 = Instant::now();
        let us = |value: u64| t0 + Duration::from_micros(value);
        let mut sink = spaced_sink(6);
        let tip = |action| {
            frame(
                1,
                action,
                0,
                FRAME_FLAG_LOCKED,
                vec![contact(0, CONTACT_FLAG_TIP, lane_x(2))],
            )
        };
        let lift = frame(
            1,
            crate::protocol::ACTION_UP,
            0,
            FRAME_FLAG_LOCKED,
            vec![contact(0, 0, lane_x(2))],
        );

        assert_eq!(step(&mut sink, us(0), &tip(ACTION_DOWN)), [(2, true)]);
        assert_eq!(step(&mut sink, us(5000), &lift), [(2, false)]);
        assert_eq!(step(&mut sink, us(6000), &tip(ACTION_DOWN)), []);
        assert!(sink.has_active_input());
        assert_eq!(deadline(&sink, us(6000)), Some(us(7000)));
        assert_eq!(tick(&mut sink, us(6999)), []);
        assert_eq!(tick(&mut sink, us(7000)), [(2, true)]);
        // The spaced press is held long enough for a polling game to see it.
        assert_eq!(step(&mut sink, us(7500), &lift), []);
        assert_eq!(tick(&mut sink, us(8999)), []);
        assert_eq!(tick(&mut sink, us(9000)), [(2, false)]);
        assert!(!sink.has_active_input());
        assert_eq!(deadline(&sink, us(9000)), None);
    }

    #[test]
    fn spaced_second_finger_on_held_lane_releases_then_represses() {
        let t0 = Instant::now();
        let us = |value: u64| t0 + Duration::from_micros(value);
        let mut sink = spaced_sink(6);
        let x = lane_x(2);
        let both = |action, pointer, first, second| {
            frame(
                1,
                action,
                pointer,
                FRAME_FLAG_LOCKED,
                vec![contact(0, first, x), contact(1, second, x)],
            )
        };
        let one = frame(
            1,
            ACTION_DOWN,
            0,
            FRAME_FLAG_LOCKED,
            vec![contact(0, CONTACT_FLAG_TIP, x)],
        );

        assert_eq!(step(&mut sink, us(0), &one), [(2, true)]);
        let second = both(ACTION_DOWN, 1, CONTACT_FLAG_TIP, CONTACT_FLAG_TIP);
        assert_eq!(step(&mut sink, us(30000), &second), [(2, false)]);
        assert_eq!(sink.state.lane_holds[2], 2);
        assert_eq!(tick(&mut sink, us(31999)), []);
        assert_eq!(tick(&mut sink, us(32000)), [(2, true)]);
        let first_lifts = both(crate::protocol::ACTION_UP, 0, 0, CONTACT_FLAG_TIP);
        assert_eq!(step(&mut sink, us(40000), &first_lifts), []);
        let second_lifts = frame(
            1,
            crate::protocol::ACTION_UP,
            1,
            FRAME_FLAG_LOCKED,
            vec![contact(1, 0, x)],
        );
        assert_eq!(step(&mut sink, us(50000), &second_lifts), [(2, false)]);
        assert!(!sink.has_active_input());
    }

    #[test]
    fn spaced_press_holds_back_a_later_release_so_slides_never_gap() {
        let t0 = Instant::now();
        let us = |value: u64| t0 + Duration::from_micros(value);
        let mut sink = spaced_sink(6);
        let snapshot = |action, pointer, slider: f32, tapper| {
            let mut contacts = vec![contact(1, CONTACT_FLAG_TIP, slider)];
            if let Some(flags) = tapper {
                contacts.push(contact(0, flags, lane_x(1)));
            }
            frame(1, action, pointer, FRAME_FLAG_LOCKED, contacts)
        };

        let slider_down = snapshot(ACTION_DOWN, 1, lane_x(0), None);
        assert_eq!(step(&mut sink, us(0), &slider_down), [(0, true)]);
        let tap = snapshot(ACTION_DOWN, 0, lane_x(0), Some(CONTACT_FLAG_TIP));
        assert_eq!(step(&mut sink, us(1000), &tap), [(1, true)]);
        let lift = snapshot(crate::protocol::ACTION_UP, 0, lane_x(0), Some(0));
        assert_eq!(step(&mut sink, us(30000), &lift), [(1, false)]);
        // The slide re-enters lane 1 within the gap: its press waits, and the
        // release of lane 0 must not leave a no-key gap before it.
        let slide = snapshot(ACTION_MOVE, 1, lane_x(1), None);
        assert_eq!(step(&mut sink, us(31000), &slide), []);
        assert!(sink.pressed[0]);
        assert_eq!(tick(&mut sink, us(31999)), []);
        assert_eq!(tick(&mut sink, us(32000)), [(1, true), (0, false)]);
    }

    #[test]
    fn spaced_burst_keeps_each_repaired_tap_visible() {
        let t0 = Instant::now();
        let us = |value: u64| t0 + Duration::from_micros(value);
        let mut sink = spaced_sink(6);
        let x = lane_x(3);
        let down = frame(
            1,
            ACTION_DOWN,
            0,
            FRAME_FLAG_LOCKED,
            vec![contact(0, CONTACT_FLAG_TIP, x)],
        );
        let up = frame(
            1,
            crate::protocol::ACTION_UP,
            0,
            FRAME_FLAG_LOCKED,
            vec![contact(0, 0, x)],
        );

        // Two complete taps arrive together after a repair.
        assert_eq!(step(&mut sink, us(0), &down), [(3, true)]);
        assert_eq!(step(&mut sink, us(0), &up), []);
        assert_eq!(step(&mut sink, us(0), &down), []);
        assert_eq!(step(&mut sink, us(0), &up), []);
        assert_eq!(tick(&mut sink, us(1999)), []);
        assert_eq!(tick(&mut sink, us(2000)), [(3, false)]);
        assert_eq!(tick(&mut sink, us(3999)), []);
        assert_eq!(tick(&mut sink, us(4000)), [(3, true)]);
        assert_eq!(tick(&mut sink, us(6000)), [(3, false)]);
        assert!(!sink.has_active_input());
    }

    #[test]
    fn spaced_waiting_edges_per_lane_are_bounded() {
        let t0 = Instant::now();
        let mut sink = spaced_sink(6);
        let x = lane_x(3);
        let down = frame(
            1,
            ACTION_DOWN,
            0,
            FRAME_FLAG_LOCKED,
            vec![contact(0, CONTACT_FLAG_TIP, x)],
        );
        let up = frame(
            1,
            crate::protocol::ACTION_UP,
            0,
            FRAME_FLAG_LOCKED,
            vec![contact(0, 0, x)],
        );
        step(&mut sink, t0, &down);
        for _ in 0..20 {
            step(&mut sink, t0, &up);
            step(&mut sink, t0, &down);
        }
        let spacer = sink.spacer.as_ref().unwrap();
        assert!(spacer.queue.len() <= MAX_WAITING_EDGES);
        // The bound keeps the logical state: the finger is still down.
        assert!(spacer.queue.last().is_some_and(|edge| edge.change.down));
        assert!(deadline(&sink, t0).unwrap() <= t0 + GAP * MAX_WAITING_EDGES as u32);
    }

    #[test]
    fn spaced_slide_crossings_are_not_delayed() {
        let t0 = Instant::now();
        let us = |value: u64| t0 + Duration::from_micros(value);
        let mut sink = spaced_sink(6);
        let at = |action, x| {
            frame(
                1,
                action,
                0,
                FRAME_FLAG_LOCKED,
                vec![contact(0, CONTACT_FLAG_TIP, x)],
            )
        };
        assert_eq!(
            step(&mut sink, us(0), &at(ACTION_DOWN, lane_x(0))),
            [(0, true)]
        );
        assert_eq!(
            step(&mut sink, us(30000), &at(ACTION_MOVE, lane_x(5))),
            [
                (1, true),
                (0, false),
                (2, true),
                (1, false),
                (3, true),
                (2, false),
                (4, true),
                (3, false),
                (5, true),
                (4, false),
            ]
        );
        assert!(sink.spacer.as_ref().unwrap().queue.is_empty());
    }

    #[test]
    fn spaced_cancel_drops_deferred_edges() {
        let t0 = Instant::now();
        let us = |value: u64| t0 + Duration::from_micros(value);
        let mut sink = spaced_sink(6);
        let x = lane_x(2);
        let down = frame(
            1,
            ACTION_DOWN,
            0,
            FRAME_FLAG_LOCKED,
            vec![contact(0, CONTACT_FLAG_TIP, x)],
        );
        let up = frame(
            1,
            crate::protocol::ACTION_UP,
            0,
            FRAME_FLAG_LOCKED,
            vec![contact(0, 0, x)],
        );
        step(&mut sink, us(0), &down);
        step(&mut sink, us(5000), &up);
        assert_eq!(step(&mut sink, us(6000), &down), []);
        let cancel = frame(1, ACTION_CANCEL, 0, FRAME_FLAG_LOCKED, vec![]);
        assert_eq!(step(&mut sink, us(6500), &cancel), []);
        assert!(!sink.has_active_input());
        assert_eq!(tick(&mut sink, us(20000)), []);
    }

    #[test]
    fn spaced_partial_submission_retries_only_the_remainder() {
        let t0 = Instant::now();
        let us = |value: u64| t0 + Duration::from_micros(value);
        let mut sink = spaced_sink(6);
        let snapshot = |action, pointer, slider: f32, tapper| {
            let mut contacts = vec![contact(1, CONTACT_FLAG_TIP, slider)];
            if let Some(flags) = tapper {
                contacts.push(contact(0, flags, lane_x(1)));
            }
            frame(1, action, pointer, FRAME_FLAG_LOCKED, contacts)
        };
        step(&mut sink, us(0), &snapshot(ACTION_DOWN, 1, lane_x(0), None));
        step(
            &mut sink,
            us(1000),
            &snapshot(ACTION_DOWN, 0, lane_x(0), Some(CONTACT_FLAG_TIP)),
        );
        step(
            &mut sink,
            us(30000),
            &snapshot(crate::protocol::ACTION_UP, 0, lane_x(0), Some(0)),
        );
        step(
            &mut sink,
            us(31000),
            &snapshot(ACTION_MOVE, 1, lane_x(1), None),
        );

        let error = sink
            .service_at(us(32000), |_, changes| {
                assert_changes(changes, &[(1, true), (0, false)]);
                Ok(1)
            })
            .unwrap_err();
        assert!(error.to_string().contains("accepted 1 of 2"));
        assert_eq!(sink.pressed[..2], [true, true]);
        assert_eq!(tick(&mut sink, us(32000)), [(0, false)]);
        assert!(sink.spacer.as_ref().unwrap().queue.is_empty());
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
            spacer: None,
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
