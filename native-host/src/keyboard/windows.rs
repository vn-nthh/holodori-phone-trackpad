//! Win32 lane-key sink: `SendInput` with `KEYEVENTF_SCANCODE`, i.e. physical
//! key position rather than the logical VK the user's keyboard layout would
//! produce.

use std::io;
use std::mem::size_of;

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
    MAPVK_VK_TO_VSC, MapVirtualKeyW, SendInput,
};

pub(super) struct KeySink {
    scan_codes: Vec<u16>,
}

impl KeySink {
    pub(super) fn new(keys: &[String]) -> io::Result<Self> {
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
        Ok(Self { scan_codes })
    }

    pub(super) fn lane_count(&self) -> usize {
        self.scan_codes.len()
    }

    pub(super) fn emit(&mut self, lane: usize, down: bool) -> io::Result<()> {
        send_one(key_input(self.scan_codes[lane], down))
    }
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
