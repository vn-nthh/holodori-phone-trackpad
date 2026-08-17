//! Linux lane-key sink backed by the kernel `uinput` virtual input device.
//!
//! This uses raw `libc` FFI directly rather than an `evdev`/`uinput` crate,
//! matching the rest of this project's minimal-dependency style. All ioctl
//! request numbers are computed from the same `_IOW`/`_IO` encoding used by
//! `<linux/uinput.h>` rather than hardcoded, and were checked against
//! `/usr/include/linux/uinput.h` and `/usr/include/linux/input.h`.

use std::io;
use std::mem::size_of;
use std::os::fd::RawFd;
use std::thread;
use std::time::Duration;

// From <linux/input-event-codes.h>.
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const SYN_REPORT: u16 = 0;

// From <linux/input.h>. A clearly-fake identity: this is a virtual keyboard,
// not a real USB HID device, so the vendor/product ids are made up.
const FAKE_BUS_USB: u16 = 0x03;
const FAKE_VENDOR_ID: u16 = 0x1209; // pid.codes test/prototype VID, not a real vendor.
const FAKE_PRODUCT_ID: u16 = 0x0001;
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
}

impl KeySink {
    pub(super) fn new(keys: &[String]) -> io::Result<Self> {
        let mut keycodes = Vec::with_capacity(keys.len());
        for key in keys {
            keycodes.push(linux_keycode(key)?);
        }

        let fd = unsafe { libc::open(c"/dev/uinput".as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            if matches!(error.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM)) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "cannot open /dev/uinput: add this user to the `input` group (or install \
                     a udev rule granting access to /dev/uinput), then re-login and try again",
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

        Ok(Self { fd, keycodes })
    }

    pub(super) fn lane_count(&self) -> usize {
        self.keycodes.len()
    }

    pub(super) fn emit(&mut self, lane: usize, down: bool) -> io::Result<()> {
        let code = self.keycodes[lane];
        let events = [
            InputEvent {
                time: unsafe { std::mem::zeroed() },
                type_: EV_KEY,
                code,
                value: i32::from(down),
            },
            InputEvent {
                time: unsafe { std::mem::zeroed() },
                type_: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
        ];
        let bytes = unsafe {
            std::slice::from_raw_parts(
                events.as_ptr().cast::<u8>(),
                size_of::<InputEvent>() * events.len(),
            )
        };
        let written = unsafe { libc::write(self.fd, bytes.as_ptr().cast(), bytes.len()) };
        if written < 0 {
            return Err(io::Error::last_os_error());
        }
        if written as usize != bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!(
                    "short uinput event write: {written}/{} bytes",
                    bytes.len()
                ),
            ));
        }
        Ok(())
    }
}

impl Drop for KeySink {
    fn drop(&mut self) {
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
    setup.id.bustype = FAKE_BUS_USB;
    setup.id.vendor = FAKE_VENDOR_ID;
    setup.id.product = FAKE_PRODUCT_ID;
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

    /// Opens a real `/dev/uinput` device and exercises it end to end. Not
    /// run by default `cargo test`: it requires `/dev/uinput` access (the
    /// `input` group or a permissive udev rule) and, on the down/up emit,
    /// delivers an `s` keypress to whatever window currently has focus.
    #[test]
    #[ignore = "opens /dev/uinput and briefly delivers a real keypress; run explicitly"]
    fn creates_and_destroys_a_real_uinput_device() {
        let keys: Vec<String> = ["s", "d", "f", "j", "k", "l"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let mut sink = KeySink::new(&keys).expect("/dev/uinput should open and set up the device");
        assert_eq!(sink.lane_count(), 6);

        let devices = std::fs::read_to_string("/proc/bus/input/devices").unwrap_or_default();
        assert!(
            devices.contains(DEVICE_NAME),
            "expected {DEVICE_NAME:?} to appear in /proc/bus/input/devices while the sink is alive"
        );

        sink.emit(0, true).expect("key down should be accepted");
        sink.emit(0, false).expect("key up should be accepted");

        drop(sink);
        let after_drop = std::fs::read_to_string("/proc/bus/input/devices").unwrap_or_default();
        assert!(
            !after_drop.contains(DEVICE_NAME),
            "expected {DEVICE_NAME:?} to be gone from /proc/bus/input/devices after the sink is dropped"
        );
    }
}
