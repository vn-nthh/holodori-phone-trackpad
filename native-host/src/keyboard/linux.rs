//! Linux lane-key sink backed by the kernel `uinput` virtual input device.
//!
//! This uses raw `libc` FFI directly rather than an `evdev`/`uinput` crate,
//! matching the rest of this project's minimal-dependency style. All ioctl
//! request numbers are computed from the same `_IOW`/`_IO` encoding used by
//! `<linux/uinput.h>` rather than hardcoded, and were checked against
//! `/usr/include/linux/uinput.h` and `/usr/include/linux/input.h`.

use std::io;
use std::mem::{size_of, size_of_val};
use std::os::fd::RawFd;
use std::thread;
use std::time::Duration;

use super::KeyChange;

// From <linux/input-event-codes.h>.
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const SYN_REPORT: u16 = 0;

// From <linux/input.h>. Identify the device honestly as virtual rather than
// claiming it is a physical USB HID keyboard.
const BUS_VIRTUAL: u16 = 0x06;
const DEVICE_NAME: &str = "Holodori lane keys";

// `_IOC(dir, type, nr, size)` from <asm-generic/ioctl.h>: 2-bit dir, 8-bit
// type, 8-bit nr, 14-bit size, packed as dir<<30 | type<<8 | nr | size<<16.
const fn ioc(dir: u32, ty: u8, nr: u8, size: u32) -> libc::Ioctl {
    ((dir << 30) | ((ty as u32) << 8) | (nr as u32) | (size << 16)) as libc::Ioctl
}

const fn iow<T>(ty: u8, nr: u8) -> libc::Ioctl {
    ioc(1, ty, nr, size_of::<T>() as u32)
}

const fn io_none(ty: u8, nr: u8) -> libc::Ioctl {
    ioc(0, ty, nr, 0)
}

const UI_SET_EVBIT: libc::Ioctl = iow::<libc::c_int>(b'U', 100);
const UI_SET_KEYBIT: libc::Ioctl = iow::<libc::c_int>(b'U', 101);
const UI_DEV_SETUP: libc::Ioctl = iow::<libc::uinput_setup>(b'U', 3);
const UI_DEV_CREATE: libc::Ioctl = io_none(b'U', 1);
const UI_DEV_DESTROY: libc::Ioctl = io_none(b'U', 2);

// Verified against the headers on this machine:
//   UI_SET_EVBIT  = 0x40045564
//   UI_SET_KEYBIT = 0x40045565
//   UI_DEV_CREATE = 0x5501
//   UI_DEV_DESTROY = 0x5502
const _: () = assert!(UI_SET_EVBIT == 0x4004_5564);
const _: () = assert!(UI_SET_KEYBIT == 0x4004_5565);
const _: () = assert!(UI_DEV_CREATE == 0x5501);
const _: () = assert!(UI_DEV_DESTROY == 0x5502);

// `libc` does not expose `struct input_event` on every target the same way
// as the kernel header does when it's unavailable; fall back to a local
// definition matching <linux/input.h> if that ever happens. On the glibc
// Linux targets this crate is built for, `libc::input_event` already exists
// and is layout-compatible, so we use it directly.
type InputEvent = libc::input_event;

pub(super) struct KeySink {
    fd: RawFd,
    keycodes: Vec<u16>,
    pending: Option<PendingWrite>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingWrite {
    /// The EV_KEY was accepted, but its SYN_REPORT was not. The shared state
    /// must not count this transition until a later call completes the sync.
    Change(KeyChange),
    /// Cancellation accepted the compensating key event but still owes the
    /// final SYN_REPORT.
    CleanupSync,
}

impl KeySink {
    pub(super) fn new(keys: &[String]) -> io::Result<Self> {
        let mut keycodes = Vec::with_capacity(keys.len());
        for key in keys {
            keycodes.push(linux_keycode(key)?);
        }

        let fd = unsafe {
            libc::open(
                c"/dev/uinput".as_ptr(),
                libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let error = io::Error::last_os_error();
            if matches!(error.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM)) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "cannot open /dev/uinput: install a udev rule for a dedicated `uinput` \
                     group, add this user to that group, then re-login and try again",
                ));
            }
            return Err(error);
        }

        if let Err(error) = setup_device(fd, &keycodes) {
            unsafe { libc::close(fd) };
            return Err(error);
        }

        // Events written immediately after UI_DEV_CREATE are not dropped by
        // the kernel; the uinput device and its input_event queue exist as
        // soon as the ioctl returns. The problem is downstream: the
        // compositor/libinput has not opened the new device node yet (opening
        // it happens asynchronously, after udev enumerates it and creates
        // /dev/input/eventN), so events written before that open simply have
        // no reader and are missed. This happens exactly once, at startup,
        // not on the per-frame hot path.
        thread::sleep(Duration::from_millis(300));

        Ok(Self {
            fd,
            keycodes,
            pending: None,
        })
    }

    pub(super) fn lane_count(&self) -> usize {
        self.keycodes.len()
    }

    pub(super) fn submit(&mut self, changes: &[KeyChange]) -> io::Result<usize> {
        let fd = self.fd;
        self.submit_with(changes, |events| write_events(fd, events))
    }

    pub(super) fn discard_pending(&mut self) -> io::Result<()> {
        let fd = self.fd;
        self.discard_pending_with(|events| write_events(fd, events))
    }

    pub(super) fn has_pending_submission(&self) -> bool {
        self.pending.is_some()
    }

    #[cfg(test)]
    pub(super) fn for_test(lanes: usize) -> Self {
        Self {
            fd: -1,
            keycodes: (1..=lanes)
                .map(|code| u16::try_from(code).expect("test lane fits u16"))
                .collect(),
            pending: None,
        }
    }

    fn submit_with<F>(&mut self, changes: &[KeyChange], mut write: F) -> io::Result<usize>
    where
        F: FnMut(&[InputEvent]) -> io::Result<usize>,
    {
        let mut accepted = 0;
        match self.pending {
            Some(PendingWrite::CleanupSync) => {
                return Err(io::Error::other(
                    "uinput cancellation sync is still pending",
                ));
            }
            Some(PendingWrite::Change(pending)) => {
                if changes.first() != Some(&pending) {
                    return Err(io::Error::other(
                        "uinput retry does not match the unsynchronized transition",
                    ));
                }
                match write(&[sync_event()]) {
                    Ok(1) => {
                        self.pending = None;
                        accepted = 1;
                    }
                    Ok(0) => return Err(write_zero_error()),
                    Ok(count) => return Err(invalid_event_count(count, 1)),
                    Err(error) => return Err(error),
                }
            }
            None => {}
        }

        let remaining = &changes[accepted..];
        if remaining.is_empty() {
            return Ok(accepted);
        }
        let events = encode_changes(&self.keycodes, remaining);
        let written = match write(&events) {
            Ok(0) if accepted == 0 => return Err(write_zero_error()),
            Ok(0) | Err(_) if accepted != 0 => return Ok(accepted),
            Err(error) => return Err(error),
            Ok(count) if count > events.len() => {
                return Err(invalid_event_count(count, events.len()));
            }
            Ok(count) => count,
        };

        let complete = written / 2;
        accepted += complete;
        if written % 2 != 0 {
            self.pending = Some(PendingWrite::Change(remaining[complete]));
        }
        Ok(accepted)
    }

    fn discard_pending_with<F>(&mut self, mut write: F) -> io::Result<()>
    where
        F: FnMut(&[InputEvent]) -> io::Result<usize>,
    {
        let events = match self.pending {
            None => return Ok(()),
            Some(PendingWrite::Change(change)) if change.down => [
                Some(key_event(self.keycodes[change.lane], false)),
                Some(sync_event()),
            ],
            Some(PendingWrite::Change(_)) | Some(PendingWrite::CleanupSync) => {
                [Some(sync_event()), None]
            }
        };
        let events: Vec<_> = events.into_iter().flatten().collect();
        match write(&events)? {
            0 => Err(write_zero_error()),
            count if count > events.len() => Err(invalid_event_count(count, events.len())),
            count if count == events.len() => {
                self.pending = None;
                Ok(())
            }
            1 if events.len() == 2 => {
                self.pending = Some(PendingWrite::CleanupSync);
                Err(io::Error::other(
                    "uinput cancellation accepted the key event but not its SYN_REPORT",
                ))
            }
            count => Err(io::Error::other(format!(
                "uinput cancellation accepted {count} of {} events",
                events.len()
            ))),
        }
    }
}

fn encode_changes(keycodes: &[u16], changes: &[KeyChange]) -> Vec<InputEvent> {
    let mut events = Vec::with_capacity(changes.len() * 2);
    for change in changes {
        events.push(key_event(keycodes[change.lane], change.down));
        events.push(sync_event());
    }
    events
}

fn key_event(code: u16, down: bool) -> InputEvent {
    InputEvent {
        time: unsafe { std::mem::zeroed() },
        type_: EV_KEY,
        code,
        value: i32::from(down),
    }
}

fn sync_event() -> InputEvent {
    InputEvent {
        time: unsafe { std::mem::zeroed() },
        type_: EV_SYN,
        code: SYN_REPORT,
        value: 0,
    }
}

fn write_events(fd: RawFd, events: &[InputEvent]) -> io::Result<usize> {
    if events.is_empty() {
        return Ok(0);
    }
    let byte_length = size_of_val(events);
    let bytes = unsafe { std::slice::from_raw_parts(events.as_ptr().cast::<u8>(), byte_length) };
    let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    if written < 0 {
        return Err(io::Error::last_os_error());
    }
    let written = written as usize;
    if written > byte_length || !written.is_multiple_of(size_of::<InputEvent>()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid uinput event write length: {written}/{byte_length} bytes"),
        ));
    }
    Ok(written / size_of::<InputEvent>())
}

fn write_zero_error() -> io::Error {
    io::Error::new(io::ErrorKind::WriteZero, "uinput accepted no events")
}

fn invalid_event_count(accepted: usize, requested: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("uinput reported {accepted} of {requested} events"),
    )
}

impl Drop for KeySink {
    fn drop(&mut self) {
        if self.fd < 0 {
            return;
        }
        unsafe {
            libc::ioctl(self.fd, UI_DEV_DESTROY);
            libc::close(self.fd);
        }
    }
}

fn setup_device(fd: RawFd, keycodes: &[u16]) -> io::Result<()> {
    checked(unsafe { libc::ioctl(fd, UI_SET_EVBIT, libc::c_int::from(EV_KEY)) })?;
    checked(unsafe { libc::ioctl(fd, UI_SET_EVBIT, libc::c_int::from(EV_SYN)) })?;

    let mut distinct: Vec<u16> = Vec::new();
    for &code in keycodes {
        if !distinct.contains(&code) {
            checked(unsafe { libc::ioctl(fd, UI_SET_KEYBIT, libc::c_int::from(code)) })?;
            distinct.push(code);
        }
    }

    let mut setup: libc::uinput_setup = unsafe { std::mem::zeroed() };
    setup.id.bustype = BUS_VIRTUAL;
    // Zero IDs avoid borrowing a real or community-assigned hardware vendor
    // identity. BUS_VIRTUAL plus the explicit name keep the injected origin
    // visible to software that inspects the device.
    setup.id.vendor = 0;
    setup.id.product = 0;
    setup.id.version = 1;
    setup.ff_effects_max = 0;
    let name_bytes = DEVICE_NAME.as_bytes();
    for (destination, byte) in setup.name.iter_mut().zip(name_bytes) {
        *destination = *byte as libc::c_char;
    }

    checked(unsafe { libc::ioctl(fd, UI_DEV_SETUP, &raw const setup) })?;
    checked(unsafe { libc::ioctl(fd, UI_DEV_CREATE) })?;
    Ok(())
}

fn checked(result: libc::c_int) -> io::Result<()> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Maps an ASCII letter/digit lane key to its Linux evdev keycode, i.e. the
/// physical key position (`<linux/input-event-codes.h>`). This mirrors the
/// Windows sink's use of `KEYEVENTF_SCANCODE`: both are layout-independent
/// physical positions, so the same lane keys mean the same physical keys on
/// both platforms. Keycodes are not contiguous with QWERTY row order, so an
/// explicit table is required rather than an offset computation.
fn linux_keycode(key: &str) -> io::Result<u16> {
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
    let code = match character.to_ascii_uppercase() {
        '1' => 2,
        '2' => 3,
        '3' => 4,
        '4' => 5,
        '5' => 6,
        '6' => 7,
        '7' => 8,
        '8' => 9,
        '9' => 10,
        '0' => 11,
        'Q' => 16,
        'W' => 17,
        'E' => 18,
        'R' => 19,
        'T' => 20,
        'Y' => 21,
        'U' => 22,
        'I' => 23,
        'O' => 24,
        'P' => 25,
        'A' => 30,
        'S' => 31,
        'D' => 32,
        'F' => 33,
        'G' => 34,
        'H' => 35,
        'J' => 36,
        'K' => 37,
        'L' => 38,
        'Z' => 44,
        'X' => 45,
        'C' => 46,
        'V' => 47,
        'B' => 48,
        'N' => 49,
        'M' => 50,
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("lane key {other:?} has no known Linux keycode"),
            ));
        }
    };
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_the_default_lane_keys_to_documented_evdev_keycodes() {
        assert_eq!(linux_keycode("s").unwrap(), 31);
        assert_eq!(linux_keycode("d").unwrap(), 32);
        assert_eq!(linux_keycode("f").unwrap(), 33);
        assert_eq!(linux_keycode("j").unwrap(), 36);
        assert_eq!(linux_keycode("k").unwrap(), 37);
        assert_eq!(linux_keycode("l").unwrap(), 38);
    }

    #[test]
    fn rejects_input_the_windows_sink_also_rejects() {
        assert!(linux_keycode("").is_err());
        assert!(linux_keycode("ab").is_err());
        assert!(linux_keycode("!").is_err());
    }

    #[test]
    fn batches_each_transition_with_its_sync_boundary() {
        let mut sink = KeySink::for_test(2);
        let changes = [
            KeyChange {
                lane: 0,
                down: true,
            },
            KeyChange {
                lane: 1,
                down: false,
            },
        ];
        let mut observed = Vec::new();

        let accepted = sink
            .submit_with(&changes, |events| {
                observed = decode_events(events);
                Ok(events.len())
            })
            .unwrap();

        assert_eq!(accepted, 2);
        assert_eq!(
            observed,
            [
                (EV_KEY, 1, 1),
                (EV_SYN, SYN_REPORT, 0),
                (EV_KEY, 2, 0),
                (EV_SYN, SYN_REPORT, 0),
            ]
        );
        assert!(!sink.has_pending_submission());
    }

    #[test]
    fn odd_short_write_withholds_the_unsynchronized_transition() {
        let mut sink = KeySink::for_test(2);
        let changes = [
            KeyChange {
                lane: 0,
                down: true,
            },
            KeyChange {
                lane: 1,
                down: true,
            },
        ];

        assert_eq!(sink.submit_with(&changes, |_| Ok(3)).unwrap(), 1);
        assert!(sink.has_pending_submission());

        let mut retry = Vec::new();
        assert_eq!(
            sink.submit_with(&changes[1..], |events| {
                retry.push(decode_events(events));
                Ok(events.len())
            })
            .unwrap(),
            1
        );
        assert_eq!(retry, [vec![(EV_SYN, SYN_REPORT, 0)]]);
        assert!(!sink.has_pending_submission());
    }

    #[test]
    fn cancellation_neutralizes_an_unsynchronized_key_down() {
        let mut sink = KeySink::for_test(1);
        let down = [KeyChange {
            lane: 0,
            down: true,
        }];
        assert_eq!(sink.submit_with(&down, |_| Ok(1)).unwrap(), 0);

        let mut first_cleanup = Vec::new();
        let error = sink
            .discard_pending_with(|events| {
                first_cleanup = decode_events(events);
                Ok(1)
            })
            .unwrap_err();
        assert!(error.to_string().contains("not its SYN_REPORT"));
        assert_eq!(first_cleanup, [(EV_KEY, 1, 0), (EV_SYN, SYN_REPORT, 0)]);

        let mut final_cleanup = Vec::new();
        sink.discard_pending_with(|events| {
            final_cleanup = decode_events(events);
            Ok(1)
        })
        .unwrap();
        assert_eq!(final_cleanup, [(EV_SYN, SYN_REPORT, 0)]);
        assert!(!sink.has_pending_submission());
    }

    /// Opens a real `/dev/uinput` device without emitting input. Not run by
    /// default because it requires the dedicated `uinput` group/udev rule.
    #[test]
    #[ignore = "opens /dev/uinput but emits no input; requires explicit device access"]
    fn creates_and_destroys_a_real_uinput_device() {
        let keys: Vec<String> = ["s", "d", "f", "j", "k", "l"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let sink = KeySink::new(&keys).expect("/dev/uinput should open and set up the device");
        assert_eq!(sink.lane_count(), 6);

        let devices = std::fs::read_to_string("/proc/bus/input/devices").unwrap_or_default();
        let device = devices
            .split("\n\n")
            .find(|block| block.contains(DEVICE_NAME))
            .expect("the uinput device should appear in /proc/bus/input/devices");
        assert!(device.contains("Bus=0006 Vendor=0000 Product=0000 Version=0001"));

        drop(sink);
        let after_drop = std::fs::read_to_string("/proc/bus/input/devices").unwrap_or_default();
        assert!(
            !after_drop.contains(DEVICE_NAME),
            "expected {DEVICE_NAME:?} to be gone from /proc/bus/input/devices after the sink is dropped"
        );
    }

    /// Exercises real down/up delivery. This can type into the focused
    /// application, so automated validation must never run it implicitly.
    #[test]
    #[ignore = "briefly delivers a real keypress; run only during supervised physical testing"]
    fn real_uinput_device_accepts_a_key_transition() {
        let keys: Vec<String> = ["s", "d", "f", "j", "k", "l"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let mut sink = KeySink::new(&keys).expect("/dev/uinput should open and set up the device");
        assert_eq!(
            sink.submit(&[KeyChange {
                lane: 0,
                down: true,
            }])
            .expect("key down should be accepted"),
            1
        );
        assert_eq!(
            sink.submit(&[KeyChange {
                lane: 0,
                down: false,
            }])
            .expect("key up should be accepted"),
            1
        );
    }

    fn decode_events(events: &[InputEvent]) -> Vec<(u16, u16, i32)> {
        events
            .iter()
            .map(|event| (event.type_, event.code, event.value))
            .collect()
    }
}
