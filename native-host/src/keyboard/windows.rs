//! Win32 lane-key sink: `SendInput` with `KEYEVENTF_SCANCODE`, i.e. physical
//! key position rather than the logical VK the user's keyboard layout would
//! produce.

use std::io;
use std::mem::size_of;

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
    MAPVK_VK_TO_VSC, MapVirtualKeyW, SendInput,
};

use super::KeyChange;

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

    pub(super) fn has_pending_submission(&self) -> bool {
        false
    }

    pub(super) fn discard_pending(&mut self) -> io::Result<()> {
        Ok(())
    }

    pub(super) fn submit(&mut self, changes: &[KeyChange]) -> io::Result<usize> {
        if changes.is_empty() {
            return Ok(0);
        }
        let inputs: Vec<_> = changes
            .iter()
            .map(|change| key_input(self.scan_codes[change.lane], change.down))
            .collect();
        let count = u32::try_from(inputs.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "too many keyboard events for one SendInput submission",
            )
        })?;
        let accepted =
            unsafe { SendInput(count, inputs.as_ptr(), size_of::<INPUT>() as i32) } as usize;
        if accepted == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(accepted)
    }

    #[cfg(test)]
    pub(super) fn for_test(lanes: usize) -> Self {
        Self {
            scan_codes: (1..=lanes)
                .map(|scan_code| u16::try_from(scan_code).expect("test lane fits u16"))
                .collect(),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_lane_key_text() {
        assert!(parse_virtual_key("").is_err());
        assert!(parse_virtual_key("ab").is_err());
        assert!(parse_virtual_key("!").is_err());
        assert_eq!(parse_virtual_key("s").unwrap(), u16::from(b'S'));
    }

    #[test]
    fn encodes_physical_scan_code_down_and_up() {
        let down = key_input(31, true);
        let up = key_input(31, false);
        let down_key = unsafe { down.Anonymous.ki };
        let up_key = unsafe { up.Anonymous.ki };

        assert_eq!(down.r#type, INPUT_KEYBOARD);
        assert_eq!(down_key.wVk, 0);
        assert_eq!(down_key.wScan, 31);
        assert_ne!(down_key.dwFlags & KEYEVENTF_SCANCODE, 0);
        assert_eq!(down_key.dwFlags & KEYEVENTF_KEYUP, 0);
        assert_ne!(up_key.dwFlags & KEYEVENTF_SCANCODE, 0);
        assert_ne!(up_key.dwFlags & KEYEVENTF_KEYUP, 0);
    }
}
