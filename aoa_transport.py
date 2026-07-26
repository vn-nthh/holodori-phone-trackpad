"""Android Open Accessory (AOA) USB transport.

The AOA handshake uses libusb directly through ctypes.  On Windows it can opt
into libusb's UsbDk backend so the handshake does not replace the phone's MTP
driver, then switches to the native WinUSB API for the latency-sensitive touch
stream.  The companion Android app emits fixed-size TOUCH_PACKET records.
"""

from __future__ import annotations

import ctypes
import os
import struct
import sys
import threading
import time
from dataclasses import dataclass
from typing import Callable, Iterable, Optional

from winusb_transport import WinUsbConnection, WinUsbError


AOA_VENDOR_ID = 0x18D1
AOA_PRODUCT_IDS = {0x2D00, 0x2D01}
AOA_GET_PROTOCOL = 51
AOA_SEND_IDENT = 52
AOA_START = 53
LIBUSB_OPTION_USE_USBDK = 1
LIBUSB_TRANSFER_TYPE_BULK = 2
LIBUSB_ENDPOINT_IN = 0x80
LIBUSB_ERROR_TIMEOUT = -7

TOUCH_MAGIC = b"HPT1"
TOUCH_PACKET = struct.Struct("<4sBBBBhhIQ")
ACTION_HEARTBEAT = 0
ACTION_DOWN = 1
ACTION_MOVE = 2
ACTION_UP = 3
ACTION_CANCEL = 4
FLAG_INSIDE = 0x01
FLAG_LOCKED = 0x02

# Common Android OEM vendor IDs.  --usb-vid can add an unlisted device without
# probing unrelated USB hardware with vendor-specific control requests.
ANDROID_VENDOR_IDS = {
    0x0409,  # NEC
    0x0421,  # Nokia
    0x04E8,  # Samsung
    0x0502,  # Acer
    0x054C,  # Sony
    0x05C6,  # Qualcomm
    0x0B05,  # ASUS
    0x0BB4,  # HTC
    0x0FCE,  # Sony Ericsson
    0x1004,  # LG
    0x12D1,  # Huawei
    0x17EF,  # Lenovo
    0x18D1,  # Google
    0x19D2,  # ZTE
    0x1BBB,  # Alcatel
    0x22B8,  # Motorola
    0x22D9,  # OPPO / realme
    0x2717,  # Xiaomi
    0x2A70,  # OnePlus
    0x2D95,  # vivo
}


class LibusbDeviceDescriptor(ctypes.Structure):
    _fields_ = [
        ("bLength", ctypes.c_uint8),
        ("bDescriptorType", ctypes.c_uint8),
        ("bcdUSB", ctypes.c_uint16),
        ("bDeviceClass", ctypes.c_uint8),
        ("bDeviceSubClass", ctypes.c_uint8),
        ("bDeviceProtocol", ctypes.c_uint8),
        ("bMaxPacketSize0", ctypes.c_uint8),
        ("idVendor", ctypes.c_uint16),
        ("idProduct", ctypes.c_uint16),
        ("bcdDevice", ctypes.c_uint16),
        ("iManufacturer", ctypes.c_uint8),
        ("iProduct", ctypes.c_uint8),
        ("iSerialNumber", ctypes.c_uint8),
        ("bNumConfigurations", ctypes.c_uint8),
    ]


class LibusbEndpointDescriptor(ctypes.Structure):
    _fields_ = [
        ("bLength", ctypes.c_uint8),
        ("bDescriptorType", ctypes.c_uint8),
        ("bEndpointAddress", ctypes.c_uint8),
        ("bmAttributes", ctypes.c_uint8),
        ("wMaxPacketSize", ctypes.c_uint16),
        ("bInterval", ctypes.c_uint8),
        ("bRefresh", ctypes.c_uint8),
        ("bSynchAddress", ctypes.c_uint8),
        ("extra", ctypes.POINTER(ctypes.c_ubyte)),
        ("extra_length", ctypes.c_int),
    ]


class LibusbInterfaceDescriptor(ctypes.Structure):
    _fields_ = [
        ("bLength", ctypes.c_uint8),
        ("bDescriptorType", ctypes.c_uint8),
        ("bInterfaceNumber", ctypes.c_uint8),
        ("bAlternateSetting", ctypes.c_uint8),
        ("bNumEndpoints", ctypes.c_uint8),
        ("bInterfaceClass", ctypes.c_uint8),
        ("bInterfaceSubClass", ctypes.c_uint8),
        ("bInterfaceProtocol", ctypes.c_uint8),
        ("iInterface", ctypes.c_uint8),
        ("endpoint", ctypes.POINTER(LibusbEndpointDescriptor)),
        ("extra", ctypes.POINTER(ctypes.c_ubyte)),
        ("extra_length", ctypes.c_int),
    ]


class LibusbInterface(ctypes.Structure):
    _fields_ = [
        ("altsetting", ctypes.POINTER(LibusbInterfaceDescriptor)),
        ("num_altsetting", ctypes.c_int),
    ]


class LibusbConfigDescriptor(ctypes.Structure):
    _fields_ = [
        ("bLength", ctypes.c_uint8),
        ("bDescriptorType", ctypes.c_uint8),
        ("wTotalLength", ctypes.c_uint16),
        ("bNumInterfaces", ctypes.c_uint8),
        ("bConfigurationValue", ctypes.c_uint8),
        ("iConfiguration", ctypes.c_uint8),
        ("bmAttributes", ctypes.c_uint8),
        ("MaxPower", ctypes.c_uint8),
        ("interface", ctypes.POINTER(LibusbInterface)),
        ("extra", ctypes.POINTER(ctypes.c_ubyte)),
        ("extra_length", ctypes.c_int),
    ]


class AoaError(RuntimeError):
    pass


@dataclass(frozen=True)
class TouchEvent:
    action: int
    pointer_id: int
    flags: int
    x: float
    y: float
    sequence: int
    phone_event_nanos: int

    @property
    def inside(self) -> bool:
        return bool(self.flags & FLAG_INSIDE)

    @property
    def locked(self) -> bool:
        return bool(self.flags & FLAG_LOCKED)


class TouchPacketParser:
    """Resynchronizing parser for the fixed-size AOA touch stream."""

    def __init__(self) -> None:
        self._buffer = bytearray()

    def feed(self, data: bytes) -> Iterable[TouchEvent]:
        self._buffer.extend(data)
        size = TOUCH_PACKET.size
        while len(self._buffer) >= size:
            if self._buffer[:4] != TOUCH_MAGIC:
                offset = self._buffer.find(TOUCH_MAGIC, 1)
                if offset < 0:
                    del self._buffer[:-3]
                    return
                del self._buffer[:offset]
                if len(self._buffer) < size:
                    return

            magic, version, action, pointer_id, flags, x, y, seq, event_ns = (
                TOUCH_PACKET.unpack_from(self._buffer)
            )
            del self._buffer[:size]
            if magic != TOUCH_MAGIC or version != 1:
                continue
            yield TouchEvent(
                action=action,
                pointer_id=pointer_id,
                flags=flags,
                x=x / 10000.0,
                y=y / 10000.0,
                sequence=seq,
                phone_event_nanos=event_ns,
            )


class Libusb:
    """Small, purpose-built libusb binding for AOA."""

    def __init__(self, use_usbdk: bool = True) -> None:
        try:
            import libusb_package
        except ImportError as exc:
            raise AoaError(
                "Missing USB runtime. Install it with: "
                "python -m pip install -r requirements.txt"
            ) from exc

        self.dll_path = str(libusb_package.get_library_path())
        self.lib = ctypes.CDLL(self.dll_path)
        self._bind()
        self.context = ctypes.c_void_p()
        rc = self.lib.libusb_init(ctypes.byref(self.context))
        self.check(rc, "initialize libusb")
        self.using_usbdk = False

        if use_usbdk and os.name == "nt":
            # Current libusb Windows builds detect UsbDk while initializing,
            # then allow an initialized context to switch backends. Supplying
            # USE_USBDK to init_context returns NOT_FOUND on these builds even
            # when UsbDkController can enumerate the device.
            rc = self.lib.libusb_set_option(
                self.context, LIBUSB_OPTION_USE_USBDK
            )
            self.using_usbdk = rc == 0

    def _bind(self) -> None:
        lib = self.lib
        lib.libusb_init.argtypes = [ctypes.POINTER(ctypes.c_void_p)]
        lib.libusb_init.restype = ctypes.c_int
        lib.libusb_exit.argtypes = [ctypes.c_void_p]
        lib.libusb_set_option.restype = ctypes.c_int
        lib.libusb_get_device_list.argtypes = [
            ctypes.c_void_p,
            ctypes.POINTER(ctypes.POINTER(ctypes.c_void_p)),
        ]
        lib.libusb_get_device_list.restype = ctypes.c_ssize_t
        lib.libusb_free_device_list.argtypes = [
            ctypes.POINTER(ctypes.c_void_p),
            ctypes.c_int,
        ]
        lib.libusb_get_device_descriptor.argtypes = [
            ctypes.c_void_p,
            ctypes.POINTER(LibusbDeviceDescriptor),
        ]
        lib.libusb_get_device_descriptor.restype = ctypes.c_int
        lib.libusb_open.argtypes = [
            ctypes.c_void_p,
            ctypes.POINTER(ctypes.c_void_p),
        ]
        lib.libusb_open.restype = ctypes.c_int
        lib.libusb_close.argtypes = [ctypes.c_void_p]
        lib.libusb_control_transfer.argtypes = [
            ctypes.c_void_p,
            ctypes.c_uint8,
            ctypes.c_uint8,
            ctypes.c_uint16,
            ctypes.c_uint16,
            ctypes.POINTER(ctypes.c_ubyte),
            ctypes.c_uint16,
            ctypes.c_uint,
        ]
        lib.libusb_control_transfer.restype = ctypes.c_int
        lib.libusb_set_configuration.argtypes = [ctypes.c_void_p, ctypes.c_int]
        lib.libusb_set_configuration.restype = ctypes.c_int
        lib.libusb_claim_interface.argtypes = [ctypes.c_void_p, ctypes.c_int]
        lib.libusb_claim_interface.restype = ctypes.c_int
        lib.libusb_release_interface.argtypes = [ctypes.c_void_p, ctypes.c_int]
        lib.libusb_release_interface.restype = ctypes.c_int
        lib.libusb_get_active_config_descriptor.argtypes = [
            ctypes.c_void_p,
            ctypes.POINTER(ctypes.POINTER(LibusbConfigDescriptor)),
        ]
        lib.libusb_get_active_config_descriptor.restype = ctypes.c_int
        lib.libusb_get_config_descriptor.argtypes = [
            ctypes.c_void_p,
            ctypes.c_uint8,
            ctypes.POINTER(ctypes.POINTER(LibusbConfigDescriptor)),
        ]
        lib.libusb_get_config_descriptor.restype = ctypes.c_int
        lib.libusb_free_config_descriptor.argtypes = [
            ctypes.POINTER(LibusbConfigDescriptor)
        ]
        lib.libusb_bulk_transfer.argtypes = [
            ctypes.c_void_p,
            ctypes.c_ubyte,
            ctypes.POINTER(ctypes.c_ubyte),
            ctypes.c_int,
            ctypes.POINTER(ctypes.c_int),
            ctypes.c_uint,
        ]
        lib.libusb_bulk_transfer.restype = ctypes.c_int
        lib.libusb_error_name.argtypes = [ctypes.c_int]
        lib.libusb_error_name.restype = ctypes.c_char_p

    def close(self) -> None:
        if self.context:
            self.lib.libusb_exit(self.context)
            self.context = ctypes.c_void_p()

    def error_name(self, code: int) -> str:
        value = self.lib.libusb_error_name(code)
        return value.decode("ascii", "replace") if value else str(code)

    def check(self, code: int, operation: str) -> None:
        if code < 0:
            raise AoaError(f"Could not {operation}: {self.error_name(code)}")

    def devices(self) -> list[tuple[ctypes.c_void_p, LibusbDeviceDescriptor]]:
        raw_list = ctypes.POINTER(ctypes.c_void_p)()
        count = self.lib.libusb_get_device_list(
            self.context, ctypes.byref(raw_list)
        )
        self.check(int(count), "enumerate USB devices")
        result: list[tuple[ctypes.c_void_p, LibusbDeviceDescriptor]] = []
        try:
            for index in range(int(count)):
                device = ctypes.c_void_p(raw_list[index])
                descriptor = LibusbDeviceDescriptor()
                if (
                    self.lib.libusb_get_device_descriptor(
                        device, ctypes.byref(descriptor)
                    )
                    == 0
                ):
                    result.append((device, descriptor))
        finally:
            # Keep the references owned by the list. Callers release them after
            # they finish opening or inspecting each retained device.
            self.lib.libusb_free_device_list(raw_list, 0)
        return result

    def unref_device(self, device: ctypes.c_void_p) -> None:
        # Bind lazily because this helper is only needed for retained list refs.
        self.lib.libusb_unref_device.argtypes = [ctypes.c_void_p]
        self.lib.libusb_unref_device(device)

    def open(self, device: ctypes.c_void_p) -> ctypes.c_void_p:
        handle = ctypes.c_void_p()
        rc = self.lib.libusb_open(device, ctypes.byref(handle))
        self.check(rc, "open the Android USB device")
        return handle

    def control_in(
        self,
        handle: ctypes.c_void_p,
        request: int,
        size: int,
        timeout_ms: int = 1000,
    ) -> bytes:
        buffer = (ctypes.c_ubyte * size)()
        rc = self.lib.libusb_control_transfer(
            handle,
            0xC0,
            request,
            0,
            0,
            buffer,
            size,
            timeout_ms,
        )
        self.check(rc, f"send AOA request {request}")
        return bytes(buffer[:rc])

    def control_out(
        self,
        handle: ctypes.c_void_p,
        request: int,
        index: int = 0,
        payload: bytes = b"",
        timeout_ms: int = 1000,
    ) -> None:
        raw = (ctypes.c_ubyte * len(payload)).from_buffer_copy(payload)
        pointer = (
            ctypes.cast(raw, ctypes.POINTER(ctypes.c_ubyte))
            if payload
            else ctypes.POINTER(ctypes.c_ubyte)()
        )
        rc = self.lib.libusb_control_transfer(
            handle,
            0x40,
            request,
            0,
            index,
            pointer,
            len(payload),
            timeout_ms,
        )
        self.check(rc, f"send AOA request {request}")


class AoaConnection:
    def __init__(
        self,
        usb: Libusb,
        handle: ctypes.c_void_p,
        device: ctypes.c_void_p,
        interface_number: int,
        endpoint_in: int,
        endpoint_out: int,
    ) -> None:
        self.usb = usb
        self.handle = handle
        self.device = device
        self.interface_number = interface_number
        self.endpoint_in = endpoint_in
        self.endpoint_out = endpoint_out
        self._closed = False

    def read(self, size: int = 4096, timeout_ms: int = 500) -> bytes:
        buffer = (ctypes.c_ubyte * size)()
        transferred = ctypes.c_int()
        rc = self.usb.lib.libusb_bulk_transfer(
            self.handle,
            self.endpoint_in,
            buffer,
            size,
            ctypes.byref(transferred),
            timeout_ms,
        )
        if rc == LIBUSB_ERROR_TIMEOUT:
            return b""
        self.usb.check(rc, "read the AOA touch stream")
        return bytes(buffer[: transferred.value])

    def write(self, payload: bytes, timeout_ms: int = 1000) -> None:
        buffer = (ctypes.c_ubyte * len(payload)).from_buffer_copy(payload)
        transferred = ctypes.c_int()
        rc = self.usb.lib.libusb_bulk_transfer(
            self.handle,
            self.endpoint_out,
            buffer,
            len(payload),
            ctypes.byref(transferred),
            timeout_ms,
        )
        self.usb.check(rc, "write AOA configuration")
        if transferred.value != len(payload):
            raise AoaError("AOA configuration write was incomplete")

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        try:
            self.usb.lib.libusb_release_interface(
                self.handle, self.interface_number
            )
        finally:
            self.usb.lib.libusb_close(self.handle)
            self.usb.unref_device(self.device)


class AoaHost:
    IDENT = (
        b"Holodori",
        b"Phone Trackpad",
        b"Low-latency multi-touch rhythm controller",
        b"1.0",
        b"https://github.com/holodori/phone-trackpad/releases",
        b"holodori-phone-trackpad",
    )

    def __init__(
        self,
        use_usbdk: bool = True,
        extra_vendor_id: Optional[int] = None,
    ) -> None:
        self.prefer_usbdk = bool(use_usbdk and os.name == "nt")
        self.usb = Libusb(use_usbdk=use_usbdk)
        self.vendor_ids = set(ANDROID_VENDOR_IDS)
        if extra_vendor_id is not None:
            self.vendor_ids.add(extra_vendor_id)

    def close(self) -> None:
        self.usb.close()

    def connect(
        self, switch_timeout: float = 12.0
    ) -> AoaConnection | WinUsbConnection:
        # UsbDk can temporarily detach Samsung's normal MTP/ADB drivers for
        # the AOA control handshake, but WinUSB is more stable for the long
        # bulk stream. Never hold the accessory data session on UsbDk.
        if self.usb.using_usbdk and self._accessory_present():
            self._replace_usb(use_usbdk=False)

        connection = self._find_data_accessory()
        if connection:
            return connection

        if self.prefer_usbdk and not self.usb.using_usbdk:
            self._replace_usb(use_usbdk=True)
            if self._accessory_present():
                self._replace_usb(use_usbdk=False)
                connection = self._find_data_accessory()
                if connection:
                    return connection

        switched = self._request_accessory_mode()
        if not switched:
            driver_note = (
                " UsbDk is not active; install UsbDk or bind WinUSB to the "
                "AOA interface. A new UsbDk installation requires a Windows "
                "restart."
                if os.name == "nt" and not self.usb.using_usbdk
                else ""
            )
            raise AoaError(
                "No accessible Android phone supports the AOA handshake."
                + driver_note
            )

        if self.usb.using_usbdk:
            # Let UsbDk finish releasing its redirect before WinUSB starts
            # opening the newly enumerated accessory interface.
            time.sleep(0.1)
            self._replace_usb(use_usbdk=False)

        deadline = time.monotonic() + switch_timeout
        last_open_error: Optional[AoaError] = None
        while time.monotonic() < deadline:
            time.sleep(0.15)
            try:
                connection = self._find_data_accessory()
            except AoaError as error:
                # Windows can enumerate the new composite device before
                # WinUSB has finished attaching to interface 0. Treat that
                # brief ACCESS/IO window as part of re-enumeration.
                last_open_error = error
                continue
            if connection:
                return connection
        if last_open_error is not None:
            raise last_open_error
        raise AoaError(
            "The phone accepted AOA mode but did not reappear as an "
            "Android Accessory. Reconnect the cable and accept the app prompt."
        )

    def _replace_usb(self, use_usbdk: bool) -> None:
        previous = self.usb
        self.usb = Libusb(use_usbdk=use_usbdk)
        previous.close()

    def _accessory_present(self) -> bool:
        devices = self.usb.devices()
        try:
            return any(
                descriptor.idVendor == AOA_VENDOR_ID
                and descriptor.idProduct in AOA_PRODUCT_IDS
                for _, descriptor in devices
            )
        finally:
            for device, _ in devices:
                self.usb.unref_device(device)

    def _find_data_accessory(
        self,
    ) -> Optional[AoaConnection | WinUsbConnection]:
        if os.name == "nt" and not self.usb.using_usbdk:
            try:
                connection = WinUsbConnection.open_first()
            except WinUsbError as error:
                raise AoaError(str(error)) from error
            if connection is not None:
                return connection
        return self._find_accessory()

    def _find_accessory(self) -> Optional[AoaConnection]:
        devices = self.usb.devices()
        keep: Optional[ctypes.c_void_p] = None
        last_open_error: Optional[AoaError] = None
        try:
            for device, descriptor in devices:
                if (
                    descriptor.idVendor == AOA_VENDOR_ID
                    and descriptor.idProduct in AOA_PRODUCT_IDS
                ):
                    try:
                        connection = self._open_accessory(device)
                    except AoaError as error:
                        # Windows can expose both the composite parent and its
                        # WinUSB accessory interface with the same VID/PID.
                        # The parent keeps Samsung's composite driver and is
                        # expected to reject libusb_open; continue until the
                        # actual WinUSB interface is found.
                        last_open_error = error
                        continue
                    keep = device
                    return connection
        finally:
            for device, _ in devices:
                if not keep or device.value != keep.value:
                    self.usb.unref_device(device)
        if last_open_error is not None:
            raise last_open_error
        return None

    def _request_accessory_mode(self) -> bool:
        devices = self.usb.devices()
        try:
            for device, descriptor in devices:
                if descriptor.idVendor not in self.vendor_ids:
                    continue
                if (
                    descriptor.idVendor == AOA_VENDOR_ID
                    and descriptor.idProduct in AOA_PRODUCT_IDS
                ):
                    continue
                handle = None
                try:
                    handle = self.usb.open(device)
                    raw_version = self.usb.control_in(
                        handle, AOA_GET_PROTOCOL, 2
                    )
                    if len(raw_version) != 2:
                        continue
                    version = int.from_bytes(raw_version, "little")
                    if version < 1:
                        continue
                    for index, value in enumerate(self.IDENT):
                        self.usb.control_out(
                            handle,
                            AOA_SEND_IDENT,
                            index=index,
                            payload=value + b"\0",
                        )
                        # A short setup-only pace is harmless to touch latency
                        # and accommodates older Android gadget stacks.
                        time.sleep(0.02)
                    time.sleep(0.05)
                    self.usb.control_out(handle, AOA_START)
                    # AOA_START is asynchronous. Keep the control handle alive
                    # briefly while Android begins USB re-enumeration.
                    time.sleep(0.25)
                    return True
                except AoaError:
                    continue
                finally:
                    if handle:
                        self.usb.lib.libusb_close(handle)
        finally:
            for device, _ in devices:
                self.usb.unref_device(device)
        return False

    def _open_accessory(
        self, device: ctypes.c_void_p
    ) -> AoaConnection:
        handle = self.usb.open(device)
        claimed = False
        active_configuration = False
        config = ctypes.POINTER(LibusbConfigDescriptor)()
        try:
            rc = self.usb.lib.libusb_get_active_config_descriptor(
                device, ctypes.byref(config)
            )
            if rc >= 0:
                active_configuration = True
            else:
                rc = self.usb.lib.libusb_get_config_descriptor(
                    device, 0, ctypes.byref(config)
                )
                self.usb.check(rc, "read AOA USB descriptors")

            descriptor = config.contents
            interface_number = -1
            endpoint_in = 0
            endpoint_out = 0

            for interface_index in range(descriptor.bNumInterfaces):
                interface = descriptor.interface[interface_index]
                for alt_index in range(interface.num_altsetting):
                    alt = interface.altsetting[alt_index]
                    candidate_in = 0
                    candidate_out = 0
                    for endpoint_index in range(alt.bNumEndpoints):
                        endpoint = alt.endpoint[endpoint_index]
                        if (
                            endpoint.bmAttributes & 0x03
                        ) != LIBUSB_TRANSFER_TYPE_BULK:
                            continue
                        if endpoint.bEndpointAddress & LIBUSB_ENDPOINT_IN:
                            candidate_in = endpoint.bEndpointAddress
                        else:
                            candidate_out = endpoint.bEndpointAddress
                    if candidate_in and candidate_out:
                        interface_number = alt.bInterfaceNumber
                        endpoint_in = candidate_in
                        endpoint_out = candidate_out
                        break
                if interface_number >= 0:
                    break

            if interface_number < 0:
                raise AoaError("The AOA bulk endpoints were not found")

            # Reapplying an already active configuration can reset WinUSB
            # pipes and make the first bulk OUT fail with LIBUSB_ERROR_IO.
            if not active_configuration:
                rc = self.usb.lib.libusb_set_configuration(
                    handle, descriptor.bConfigurationValue or 1
                )
                self.usb.check(rc, "activate the AOA USB configuration")
            rc = self.usb.lib.libusb_claim_interface(
                handle, interface_number
            )
            self.usb.check(rc, "claim the AOA USB interface")
            claimed = True
            return AoaConnection(
                usb=self.usb,
                handle=handle,
                device=device,
                interface_number=interface_number,
                endpoint_in=endpoint_in,
                endpoint_out=endpoint_out,
            )
        except Exception:
            if claimed:
                self.usb.lib.libusb_release_interface(handle, interface_number)
            self.usb.lib.libusb_close(handle)
            raise
        finally:
            if config:
                self.usb.lib.libusb_free_config_descriptor(config)


class AoaReceiver:
    """Reconnect-capable background receiver."""

    HEARTBEAT_TIMEOUT_SECONDS = 1.5

    def __init__(
        self,
        on_event: Callable[[TouchEvent], None],
        on_status: Callable[[str, bool], None],
        on_disconnect: Callable[[], None],
        lane_count: int,
        use_usbdk: bool = True,
        extra_vendor_id: Optional[int] = None,
    ) -> None:
        self.on_event = on_event
        self.on_status = on_status
        self.on_disconnect = on_disconnect
        self.lane_count = max(1, min(16, lane_count))
        self.use_usbdk = use_usbdk
        self.extra_vendor_id = extra_vendor_id
        self._stop = threading.Event()
        self.finished = threading.Event()
        self._thread: Optional[threading.Thread] = None
        self._connection: Optional[AoaConnection | WinUsbConnection] = None

    def start(self) -> None:
        self._thread = threading.Thread(
            target=self._run, name="AOA receiver", daemon=True
        )
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        if self._thread and self._thread is not threading.current_thread():
            self._thread.join(timeout=2)

    def _run(self) -> None:
        try:
            if os.name == "nt":
                try:
                    ctypes.windll.kernel32.SetThreadPriority(
                        ctypes.windll.kernel32.GetCurrentThread(), 2
                    )
                except Exception:
                    pass

            host = AoaHost(
                use_usbdk=self.use_usbdk,
                extra_vendor_id=self.extra_vendor_id,
            )
            backend = "UsbDk" if host.usb.using_usbdk else "WinUSB"
            self.on_status(f"USB backend ready: {backend}", False)
        except Exception as exc:
            self.on_status(str(exc), False)
            self.finished.set()
            return

        last_error = ""
        try:
            while not self._stop.is_set():
                try:
                    self.on_status("Looking for Android accessory…", False)
                    self._connection = host.connect()
                    backend = "UsbDk" if host.usb.using_usbdk else "WinUSB"
                    self.on_status(f"Connected over AOA ({backend})", True)
                    # Keep the accessory path one-way. The phone streams touch
                    # packets to the host; avoiding an idle reverse reader also
                    # avoids composite-driver teardown on affected devices.
                    last_error = ""
                    parser = TouchPacketParser()
                    last_packet_at = time.monotonic()
                    while not self._stop.is_set():
                        chunk = self._connection.read()
                        events = list(parser.feed(chunk))
                        now = time.monotonic()
                        if events:
                            last_packet_at = now
                            for event in events:
                                # Heartbeats participate in sequence tracking.
                                # The router ignores them after verifying that
                                # no wire record was skipped.
                                self.on_event(event)
                        elif (
                            now - last_packet_at
                            >= self.HEARTBEAT_TIMEOUT_SECONDS
                        ):
                            raise AoaError(
                                "AOA heartbeat timed out; reconnecting"
                            )
                except Exception as exc:
                    if self._stop.is_set():
                        break
                    message = str(exc)
                    if message != last_error:
                        self.on_status(message, False)
                        last_error = message
                    self.on_disconnect()
                    self._stop.wait(1)
                finally:
                    if self._connection:
                        try:
                            self._connection.close()
                        except Exception:
                            pass
                        self._connection = None
        finally:
            host.close()
            self.finished.set()
