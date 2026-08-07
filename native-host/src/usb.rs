use libusb1_sys::constants::{
    LIBUSB_ENDPOINT_IN, LIBUSB_ERROR_TIMEOUT, LIBUSB_OPTION_USE_USBDK, LIBUSB_TRANSFER_TYPE_BULK,
    LIBUSB_TRANSFER_TYPE_MASK,
};
use libusb1_sys::{
    libusb_bulk_transfer, libusb_claim_interface, libusb_close, libusb_config_descriptor,
    libusb_context, libusb_control_transfer, libusb_device, libusb_device_descriptor,
    libusb_device_handle, libusb_error_name, libusb_exit, libusb_free_config_descriptor,
    libusb_free_device_list, libusb_get_active_config_descriptor, libusb_get_config_descriptor,
    libusb_get_device_descriptor, libusb_get_device_list, libusb_init, libusb_open,
    libusb_release_interface, libusb_set_configuration, libusb_set_option,
};
use std::collections::HashSet;
use std::ffi::CStr;
use std::fmt;
use std::ptr;
use std::thread;
use std::time::{Duration, Instant};

const AOA_VENDOR_ID: u16 = 0x18d1;
const AOA_PRODUCT_IDS: [u16; 2] = [0x2d00, 0x2d01];
const AOA_GET_PROTOCOL: u8 = 51;
const AOA_SEND_IDENT: u8 = 52;
const AOA_START: u8 = 53;

const IDENT: [&[u8]; 6] = [
    b"Holodori\0",
    b"Phone Trackpad\0",
    b"Lossless multi-touch rhythm controller\0",
    b"4.0\0",
    b"https://github.com/vn-nthh/holodori-phone-trackpad\0",
    b"holodori-lossless-touch\0",
];

struct ConfigDescriptor(*const libusb_config_descriptor);

impl Drop for ConfigDescriptor {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { libusb_free_config_descriptor(self.0) };
        }
    }
}

#[derive(Debug)]
pub struct UsbError {
    pub operation: &'static str,
    pub code: i32,
    pub name: String,
}

impl UsbError {
    fn new(operation: &'static str, code: i32) -> Self {
        let name = unsafe {
            let pointer = libusb_error_name(code);
            if pointer.is_null() {
                code.to_string()
            } else {
                CStr::from_ptr(pointer).to_string_lossy().into_owned()
            }
        };
        Self {
            operation,
            code,
            name,
        }
    }

    fn check(operation: &'static str, code: i32) -> Result<i32, Self> {
        if code < 0 {
            Err(Self::new(operation, code))
        } else {
            Ok(code)
        }
    }
}

impl fmt::Display for UsbError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "could not {}: {} ({})",
            self.operation, self.name, self.code
        )
    }
}

impl std::error::Error for UsbError {}

pub struct UsbContext {
    raw: *mut libusb_context,
    usbdk_raw: *mut libusb_context,
    pub using_usbdk: bool,
    vendor_ids: HashSet<u16>,
}

unsafe impl Send for UsbContext {}
unsafe impl Sync for UsbContext {}

impl UsbContext {
    pub fn new(use_usbdk: bool, extra_vendor_id: Option<u16>) -> Result<Self, UsbError> {
        let mut raw = ptr::null_mut();
        UsbError::check("initialize libusb", unsafe { libusb_init(&mut raw) })?;
        let mut usbdk_raw = ptr::null_mut();
        let using_usbdk = if use_usbdk {
            let initialized = unsafe { libusb_init(&mut usbdk_raw) };
            if initialized < 0 {
                unsafe { libusb_exit(raw) };
                return Err(UsbError::new(
                    "initialize the UsbDk libusb context",
                    initialized,
                ));
            }
            let selected = unsafe { libusb_set_option(usbdk_raw, LIBUSB_OPTION_USE_USBDK) };
            if selected < 0 {
                unsafe { libusb_exit(usbdk_raw) };
                usbdk_raw = ptr::null_mut();
                false
            } else {
                true
            }
        } else {
            false
        };
        let mut vendor_ids: HashSet<u16> = [
            0x0409, 0x0421, 0x04e8, 0x0502, 0x054c, 0x05c6, 0x0b05, 0x0bb4, 0x0fce, 0x1004, 0x12d1,
            0x17ef, 0x18d1, 0x19d2, 0x1bbb, 0x22b8, 0x22d9, 0x2717, 0x2a70, 0x2d95,
        ]
        .into_iter()
        .collect();
        if let Some(vendor_id) = extra_vendor_id {
            vendor_ids.insert(vendor_id);
        }
        Ok(Self {
            raw,
            usbdk_raw,
            using_usbdk,
            vendor_ids,
        })
    }

    pub fn connect(&self, timeout: Duration) -> Result<AccessoryConnection<'_>, UsbError> {
        let deadline = Instant::now() + timeout;
        let mut negotiation_requested = false;
        loop {
            if let Some(connection) = self.find_accessory()? {
                return Ok(connection);
            }
            if !negotiation_requested {
                negotiation_requested = self.request_accessory_mode()?;
            }
            if Instant::now() >= deadline {
                return Err(UsbError {
                    operation: "find the Android accessory",
                    code: -7,
                    name: "connection timeout".to_owned(),
                });
            }
            thread::sleep(Duration::from_millis(if negotiation_requested {
                100
            } else {
                250
            }));
        }
    }

    fn with_devices_on<T>(
        &self,
        context: *mut libusb_context,
        mut callback: impl FnMut(
            *mut libusb_device,
            &libusb_device_descriptor,
        ) -> Result<Option<T>, UsbError>,
    ) -> Result<Option<T>, UsbError> {
        let mut list: *const *mut libusb_device = ptr::null();
        let count = unsafe { libusb_get_device_list(context, &mut list) };
        if count < 0 {
            return Err(UsbError::new("enumerate USB devices", count as i32));
        }
        let result = (|| {
            for index in 0..count as usize {
                let device = unsafe { *list.add(index) };
                if device.is_null() {
                    continue;
                }
                let mut descriptor: libusb_device_descriptor = unsafe { std::mem::zeroed() };
                if unsafe { libusb_get_device_descriptor(device, &mut descriptor) } < 0 {
                    continue;
                }
                if let Some(value) = callback(device, &descriptor)? {
                    return Ok(Some(value));
                }
            }
            Ok(None)
        })();
        unsafe { libusb_free_device_list(list, 1) };
        result
    }

    fn find_accessory(&self) -> Result<Option<AccessoryConnection<'_>>, UsbError> {
        // Prefer the normal Windows backend. If an AOA device already has a
        // WinUSB-compatible driver, opening it directly avoids a slow or
        // impossible UsbDk redirect (notably with SuperDisplay's AOA driver).
        // UsbDk remains the fallback for machines without an AOA driver.
        for context in [self.raw, self.usbdk_raw] {
            if context.is_null() {
                continue;
            }
            let found = self.with_devices_on(context, |device, descriptor| {
                if descriptor.idVendor != AOA_VENDOR_ID
                    || !AOA_PRODUCT_IDS.contains(&descriptor.idProduct)
                {
                    return Ok(None);
                }
                match self.open_accessory(device) {
                    Ok(connection) => Ok(Some(connection)),
                    // Composite parents can share the AOA VID/PID while
                    // rejecting an open. Continue until the real interface.
                    Err(_) => Ok(None),
                }
            })?;
            if found.is_some() {
                return Ok(found);
            }
        }
        Ok(None)
    }

    fn request_accessory_mode(&self) -> Result<bool, UsbError> {
        // Try WinUSB-capable interfaces first, then use UsbDk to reach phones
        // whose ordinary USB personality is owned by a vendor driver.
        for context in [self.raw, self.usbdk_raw] {
            if context.is_null() {
                continue;
            }
            let requested = self.with_devices_on(context, |device, descriptor| {
                if !self.vendor_ids.contains(&descriptor.idVendor)
                    || (descriptor.idVendor == AOA_VENDOR_ID
                        && AOA_PRODUCT_IDS.contains(&descriptor.idProduct))
                {
                    return Ok(None);
                }
                let mut handle = ptr::null_mut();
                if unsafe { libusb_open(device, &mut handle) } < 0 || handle.is_null() {
                    return Ok(None);
                }
                let result = self.negotiate_accessory(handle);
                unsafe { libusb_close(handle) };
                match result {
                    Ok(true) => Ok(Some(true)),
                    Ok(false) | Err(_) => Ok(None),
                }
            })?;
            if requested.unwrap_or(false) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn negotiate_accessory(&self, handle: *mut libusb_device_handle) -> Result<bool, UsbError> {
        let mut version = [0_u8; 2];
        let count = unsafe {
            libusb_control_transfer(
                handle,
                0xc0,
                AOA_GET_PROTOCOL,
                0,
                0,
                version.as_mut_ptr(),
                version.len() as u16,
                1_000,
            )
        };
        if count != 2 || u16::from_le_bytes(version) < 1 {
            return Ok(false);
        }
        for (index, identity) in IDENT.iter().enumerate() {
            UsbError::check("send AOA identity", unsafe {
                libusb_control_transfer(
                    handle,
                    0x40,
                    AOA_SEND_IDENT,
                    0,
                    index as u16,
                    identity.as_ptr() as *mut u8,
                    identity.len() as u16,
                    1_000,
                )
            })?;
            thread::sleep(Duration::from_millis(20));
        }
        thread::sleep(Duration::from_millis(50));
        UsbError::check("start AOA mode", unsafe {
            libusb_control_transfer(handle, 0x40, AOA_START, 0, 0, ptr::null_mut(), 0, 1_000)
        })?;
        thread::sleep(Duration::from_millis(250));
        Ok(true)
    }

    fn open_accessory(
        &self,
        device: *mut libusb_device,
    ) -> Result<AccessoryConnection<'_>, UsbError> {
        let mut handle = ptr::null_mut();
        UsbError::check("open the AOA device", unsafe {
            libusb_open(device, &mut handle)
        })?;

        let result = (|| {
            let mut config: *const libusb_config_descriptor = ptr::null();
            let active_result = unsafe { libusb_get_active_config_descriptor(device, &mut config) };
            let active_configuration = active_result >= 0;
            if !active_configuration {
                UsbError::check("read AOA descriptors", unsafe {
                    libusb_get_config_descriptor(device, 0, &mut config)
                })?;
            }
            if config.is_null() {
                return Err(UsbError {
                    operation: "read AOA descriptors",
                    code: -99,
                    name: "empty configuration".to_owned(),
                });
            }
            let _config_guard = ConfigDescriptor(config);

            let descriptor = unsafe { &*config };
            let mut interface_number = None;
            let mut endpoint_in = 0_u8;
            let mut endpoint_out = 0_u8;
            for interface_index in 0..descriptor.bNumInterfaces as usize {
                let interface = unsafe { &*descriptor.interface.add(interface_index) };
                for alternate_index in 0..interface.num_altsetting as usize {
                    let alternate = unsafe { &*interface.altsetting.add(alternate_index) };
                    let mut candidate_in = 0_u8;
                    let mut candidate_out = 0_u8;
                    for endpoint_index in 0..alternate.bNumEndpoints as usize {
                        let endpoint = unsafe { &*alternate.endpoint.add(endpoint_index) };
                        if endpoint.bmAttributes & LIBUSB_TRANSFER_TYPE_MASK
                            != LIBUSB_TRANSFER_TYPE_BULK
                        {
                            continue;
                        }
                        if endpoint.bEndpointAddress & LIBUSB_ENDPOINT_IN != 0 {
                            candidate_in = endpoint.bEndpointAddress;
                        } else {
                            candidate_out = endpoint.bEndpointAddress;
                        }
                    }
                    if candidate_in != 0 && candidate_out != 0 {
                        interface_number = Some(alternate.bInterfaceNumber as i32);
                        endpoint_in = candidate_in;
                        endpoint_out = candidate_out;
                        break;
                    }
                }
                if interface_number.is_some() {
                    break;
                }
            }

            let interface_number = interface_number.ok_or_else(|| UsbError {
                operation: "find AOA bulk endpoints",
                code: -5,
                name: "bulk endpoint pair not found".to_owned(),
            })?;
            if !active_configuration {
                UsbError::check("activate the AOA configuration", unsafe {
                    libusb_set_configuration(
                        handle,
                        i32::from(descriptor.bConfigurationValue.max(1)),
                    )
                })?;
            }
            UsbError::check("claim the AOA interface", unsafe {
                libusb_claim_interface(handle, interface_number)
            })?;
            Ok(AccessoryConnection {
                _context: self,
                handle,
                interface_number,
                endpoint_in,
                endpoint_out,
            })
        })();
        if result.is_err() {
            unsafe { libusb_close(handle) };
        }
        result
    }
}

impl Drop for UsbContext {
    fn drop(&mut self) {
        if !self.usbdk_raw.is_null() {
            unsafe { libusb_exit(self.usbdk_raw) };
            self.usbdk_raw = ptr::null_mut();
        }
        if !self.raw.is_null() {
            unsafe { libusb_exit(self.raw) };
            self.raw = ptr::null_mut();
        }
    }
}

pub struct AccessoryConnection<'a> {
    _context: &'a UsbContext,
    handle: *mut libusb_device_handle,
    interface_number: i32,
    pub endpoint_in: u8,
    pub endpoint_out: u8,
}

impl AccessoryConnection<'_> {
    pub fn read(&mut self, buffer: &mut [u8], timeout_ms: u32) -> Result<usize, UsbError> {
        let mut transferred = 0_i32;
        let result = unsafe {
            libusb_bulk_transfer(
                self.handle,
                self.endpoint_in,
                buffer.as_mut_ptr(),
                buffer.len() as i32,
                &mut transferred,
                timeout_ms,
            )
        };
        completed_read(result, transferred)
    }

    pub fn write(&mut self, bytes: &[u8], timeout_ms: u32) -> Result<(), UsbError> {
        let mut offset = 0;
        while offset < bytes.len() {
            let mut transferred = 0_i32;
            let result = unsafe {
                libusb_bulk_transfer(
                    self.handle,
                    self.endpoint_out,
                    bytes[offset..].as_ptr() as *mut u8,
                    (bytes.len() - offset) as i32,
                    &mut transferred,
                    timeout_ms,
                )
            };
            let progress = transferred.max(0) as usize;
            offset = offset.saturating_add(progress).min(bytes.len());
            if result == LIBUSB_ERROR_TIMEOUT && progress > 0 {
                continue;
            }
            UsbError::check("write an AOA acknowledgement", result)?;
            if progress == 0 && offset < bytes.len() {
                return Err(UsbError {
                    operation: "write a complete AOA acknowledgement",
                    code: -1,
                    name: format!("short write {offset}/{}", bytes.len()),
                });
            }
        }
        Ok(())
    }
}

fn completed_read(result: i32, transferred: i32) -> Result<usize, UsbError> {
    let progress = transferred.max(0) as usize;
    if result == LIBUSB_ERROR_TIMEOUT {
        // libusb explicitly permits a timeout after one or more OS transfer
        // chunks completed. Those bytes belong to the stream and must not be
        // discarded; doing so manufactures a sequence hole and forces replay.
        return Ok(progress);
    }
    UsbError::check("read the AOA touch stream", result)?;
    Ok(progress)
}

impl Drop for AccessoryConnection<'_> {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                libusb_release_interface(self.handle, self.interface_number);
                libusb_close(self.handle);
            }
            self.handle = ptr::null_mut();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_preserves_partially_transferred_bytes() {
        assert_eq!(completed_read(LIBUSB_ERROR_TIMEOUT, 173).unwrap(), 173);
        assert_eq!(completed_read(LIBUSB_ERROR_TIMEOUT, 0).unwrap(), 0);
    }
}
