"""Direct WinUSB transport for the Android Open Accessory data interface.

libusb's Windows composite-device layer can open the AOA interface but return
``LIBUSB_ERROR_IO`` on its first bulk read when another function (notably ADB)
is present.  Opening the WinUSB device interface directly avoids that composite
parent path. The latency-sensitive receive path keeps a small ordered pipeline
of overlapped reads posted into reusable buffers.
"""

from __future__ import annotations

import ctypes
import os
import re
import threading
from ctypes import wintypes
from typing import Iterable, Optional

try:
    import winreg
except ImportError:  # pragma: no cover - only reached off Windows
    winreg = None


AOA_DEVICE_PREFIXES = (
    "vid_18d1&pid_2d00",
    "vid_18d1&pid_2d01",
    "vid_18d1&pid_2d04",
    "vid_18d1&pid_2d05",
)
_DEVICE_ID_PATTERN = re.compile(
    r"vid_([0-9a-f]{4})&pid_([0-9a-f]{4})", re.IGNORECASE
)


def _parse_device_ids(path: str) -> tuple[Optional[int], Optional[int]]:
    match = _DEVICE_ID_PATTERN.search(path)
    if not match:
        return None, None
    return int(match.group(1), 16), int(match.group(2), 16)

DIGCF_PRESENT = 0x00000002
DIGCF_DEVICEINTERFACE = 0x00000010
GENERIC_READ = 0x80000000
GENERIC_WRITE = 0x40000000
FILE_SHARE_READ = 0x00000001
FILE_SHARE_WRITE = 0x00000002
OPEN_EXISTING = 3
FILE_FLAG_OVERLAPPED = 0x40000000
ERROR_NO_MORE_ITEMS = 259
ERROR_SEM_TIMEOUT = 121
ERROR_IO_PENDING = 997
ERROR_NOT_FOUND = 1168
WAIT_OBJECT_0 = 0
WAIT_TIMEOUT = 258
WAIT_FAILED = 0xFFFFFFFF

PIPE_TRANSFER_TIMEOUT = 3
AUTO_CLEAR_STALL = 2
DEFAULT_READ_PIPELINE_DEPTH = 2
DEFAULT_READ_TIMEOUT_MS = 100


class WinUsbError(RuntimeError):
    """WinUSB failure with structured fields for the connection doctor."""

    def __init__(
        self,
        message: str,
        operation: Optional[str] = None,
        native_code: Optional[int] = None,
    ) -> None:
        super().__init__(message)
        self.operation = operation
        self.native_code = native_code


class Guid(ctypes.Structure):
    _fields_ = [
        ("Data1", wintypes.DWORD),
        ("Data2", wintypes.WORD),
        ("Data3", wintypes.WORD),
        ("Data4", ctypes.c_ubyte * 8),
    ]


class DeviceInterfaceData(ctypes.Structure):
    _fields_ = [
        ("cbSize", wintypes.DWORD),
        ("InterfaceClassGuid", Guid),
        ("Flags", wintypes.DWORD),
        ("Reserved", ctypes.c_size_t),
    ]


class UsbInterfaceDescriptor(ctypes.Structure):
    _fields_ = [
        ("bLength", ctypes.c_ubyte),
        ("bDescriptorType", ctypes.c_ubyte),
        ("bInterfaceNumber", ctypes.c_ubyte),
        ("bAlternateSetting", ctypes.c_ubyte),
        ("bNumEndpoints", ctypes.c_ubyte),
        ("bInterfaceClass", ctypes.c_ubyte),
        ("bInterfaceSubClass", ctypes.c_ubyte),
        ("bInterfaceProtocol", ctypes.c_ubyte),
        ("iInterface", ctypes.c_ubyte),
    ]


class WinUsbPipeInformation(ctypes.Structure):
    _fields_ = [
        ("PipeType", ctypes.c_int),
        ("PipeId", ctypes.c_ubyte),
        ("MaximumPacketSize", ctypes.c_ushort),
        ("Interval", ctypes.c_ubyte),
    ]


class Overlapped(ctypes.Structure):
    """Layout-compatible OVERLAPPED for offset-free device I/O."""

    _fields_ = [
        ("Internal", ctypes.c_size_t),
        ("InternalHigh", ctypes.c_size_t),
        ("Offset", wintypes.DWORD),
        ("OffsetHigh", wintypes.DWORD),
        ("hEvent", wintypes.HANDLE),
    ]


class _OverlappedRead:
    def __init__(self, api: "_WinUsbApi", size: int) -> None:
        self.buffer = (ctypes.c_ubyte * size)()
        self.overlapped = Overlapped()
        self.transferred = wintypes.DWORD()
        self.event = api.kernel.CreateEventW(None, True, False, None)
        if not self.event:
            raise api.last_error("create an AOA read event")
        self.active = False
        self.in_flight = False
        self.completed_inline = False
        self.completion_error: Optional[int] = None


class _WinUsbApi:
    def __init__(self) -> None:
        if os.name != "nt":
            raise WinUsbError("Native WinUSB is only available on Windows")

        self.ole32 = ctypes.WinDLL("ole32", use_last_error=True)
        self.setup = ctypes.WinDLL("setupapi", use_last_error=True)
        self.kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        self.winusb = ctypes.WinDLL("winusb", use_last_error=True)
        self._bind()

    def _bind(self) -> None:
        self.ole32.CLSIDFromString.argtypes = [
            wintypes.LPCWSTR,
            ctypes.POINTER(Guid),
        ]
        self.ole32.CLSIDFromString.restype = ctypes.c_long

        self.setup.SetupDiGetClassDevsW.argtypes = [
            ctypes.POINTER(Guid),
            wintypes.LPCWSTR,
            wintypes.HWND,
            wintypes.DWORD,
        ]
        self.setup.SetupDiGetClassDevsW.restype = ctypes.c_void_p
        self.setup.SetupDiEnumDeviceInterfaces.argtypes = [
            ctypes.c_void_p,
            ctypes.c_void_p,
            ctypes.POINTER(Guid),
            wintypes.DWORD,
            ctypes.POINTER(DeviceInterfaceData),
        ]
        self.setup.SetupDiEnumDeviceInterfaces.restype = wintypes.BOOL
        self.setup.SetupDiGetDeviceInterfaceDetailW.argtypes = [
            ctypes.c_void_p,
            ctypes.POINTER(DeviceInterfaceData),
            ctypes.c_void_p,
            wintypes.DWORD,
            ctypes.POINTER(wintypes.DWORD),
            ctypes.c_void_p,
        ]
        self.setup.SetupDiGetDeviceInterfaceDetailW.restype = wintypes.BOOL
        self.setup.SetupDiDestroyDeviceInfoList.argtypes = [ctypes.c_void_p]

        self.kernel.CreateFileW.argtypes = [
            wintypes.LPCWSTR,
            wintypes.DWORD,
            wintypes.DWORD,
            ctypes.c_void_p,
            wintypes.DWORD,
            wintypes.DWORD,
            wintypes.HANDLE,
        ]
        self.kernel.CreateFileW.restype = wintypes.HANDLE
        self.kernel.CloseHandle.argtypes = [wintypes.HANDLE]
        self.kernel.CreateEventW.argtypes = [
            ctypes.c_void_p,
            wintypes.BOOL,
            wintypes.BOOL,
            wintypes.LPCWSTR,
        ]
        self.kernel.CreateEventW.restype = wintypes.HANDLE
        self.kernel.ResetEvent.argtypes = [wintypes.HANDLE]
        self.kernel.ResetEvent.restype = wintypes.BOOL
        self.kernel.WaitForSingleObject.argtypes = [
            wintypes.HANDLE,
            wintypes.DWORD,
        ]
        self.kernel.WaitForSingleObject.restype = wintypes.DWORD
        self.kernel.CancelIoEx.argtypes = [
            wintypes.HANDLE,
            ctypes.POINTER(Overlapped),
        ]
        self.kernel.CancelIoEx.restype = wintypes.BOOL

        self.winusb.WinUsb_Initialize.argtypes = [
            wintypes.HANDLE,
            ctypes.POINTER(ctypes.c_void_p),
        ]
        self.winusb.WinUsb_Initialize.restype = wintypes.BOOL
        self.winusb.WinUsb_Free.argtypes = [ctypes.c_void_p]
        self.winusb.WinUsb_QueryInterfaceSettings.argtypes = [
            ctypes.c_void_p,
            ctypes.c_ubyte,
            ctypes.POINTER(UsbInterfaceDescriptor),
        ]
        self.winusb.WinUsb_QueryInterfaceSettings.restype = wintypes.BOOL
        self.winusb.WinUsb_QueryPipe.argtypes = [
            ctypes.c_void_p,
            ctypes.c_ubyte,
            ctypes.c_ubyte,
            ctypes.POINTER(WinUsbPipeInformation),
        ]
        self.winusb.WinUsb_QueryPipe.restype = wintypes.BOOL
        self.winusb.WinUsb_SetPipePolicy.argtypes = [
            ctypes.c_void_p,
            ctypes.c_ubyte,
            wintypes.DWORD,
            wintypes.ULONG,
            ctypes.c_void_p,
        ]
        self.winusb.WinUsb_SetPipePolicy.restype = wintypes.BOOL
        self.winusb.WinUsb_ReadPipe.argtypes = [
            ctypes.c_void_p,
            ctypes.c_ubyte,
            ctypes.POINTER(ctypes.c_ubyte),
            wintypes.ULONG,
            ctypes.POINTER(wintypes.ULONG),
            ctypes.c_void_p,
        ]
        self.winusb.WinUsb_ReadPipe.restype = wintypes.BOOL
        self.winusb.WinUsb_GetOverlappedResult.argtypes = [
            ctypes.c_void_p,
            ctypes.POINTER(Overlapped),
            ctypes.POINTER(wintypes.DWORD),
            wintypes.BOOL,
        ]
        self.winusb.WinUsb_GetOverlappedResult.restype = wintypes.BOOL
        self.winusb.WinUsb_WritePipe.argtypes = [
            ctypes.c_void_p,
            ctypes.c_ubyte,
            ctypes.POINTER(ctypes.c_ubyte),
            wintypes.ULONG,
            ctypes.POINTER(wintypes.ULONG),
            ctypes.c_void_p,
        ]
        self.winusb.WinUsb_WritePipe.restype = wintypes.BOOL

    def guid(self, text: str) -> Guid:
        value = Guid()
        if self.ole32.CLSIDFromString(text, ctypes.byref(value)) != 0:
            raise WinUsbError(f"Invalid WinUSB interface GUID: {text}")
        return value

    def interface_paths(self, guid_text: str) -> Iterable[str]:
        guid = self.guid(guid_text)
        info = self.setup.SetupDiGetClassDevsW(
            ctypes.byref(guid),
            None,
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
        invalid = ctypes.c_void_p(-1).value
        if info == invalid:
            raise self.last_error("enumerate WinUSB interfaces")

        try:
            index = 0
            while True:
                data = DeviceInterfaceData()
                data.cbSize = ctypes.sizeof(data)
                if not self.setup.SetupDiEnumDeviceInterfaces(
                    info,
                    None,
                    ctypes.byref(guid),
                    index,
                    ctypes.byref(data),
                ):
                    error = ctypes.get_last_error()
                    if error == ERROR_NO_MORE_ITEMS:
                        return
                    raise self.last_error(
                        "enumerate WinUSB interface", error
                    )

                required = wintypes.DWORD()
                self.setup.SetupDiGetDeviceInterfaceDetailW(
                    info,
                    ctypes.byref(data),
                    None,
                    0,
                    ctypes.byref(required),
                    None,
                )
                detail = ctypes.create_string_buffer(required.value)
                # SP_DEVICE_INTERFACE_DETAIL_DATA_W.cbSize is 8 on 64-bit
                # Windows (6 on 32-bit); DevicePath itself starts at offset 4.
                detail_size = 8 if ctypes.sizeof(ctypes.c_void_p) == 8 else 6
                ctypes.cast(
                    detail, ctypes.POINTER(wintypes.DWORD)
                )[0] = detail_size
                if not self.setup.SetupDiGetDeviceInterfaceDetailW(
                    info,
                    ctypes.byref(data),
                    detail,
                    required.value,
                    ctypes.byref(required),
                    None,
                ):
                    raise self.last_error("read WinUSB interface path")
                yield ctypes.wstring_at(ctypes.addressof(detail) + 4)
                index += 1
        finally:
            self.setup.SetupDiDestroyDeviceInfoList(info)

    @staticmethod
    def last_error(operation: str, code: Optional[int] = None) -> WinUsbError:
        error = ctypes.get_last_error() if code is None else code
        message = ctypes.FormatError(error).strip()
        return WinUsbError(
            f"Could not {operation}: {message or 'Windows error'} ({error})",
            operation=operation,
            native_code=error,
        )


def _accessory_interface_guids() -> list[str]:
    """Read interface GUIDs attached to the current AOA device nodes."""

    if os.name != "nt" or winreg is None:
        return []
    result: list[str] = []
    root_path = r"SYSTEM\CurrentControlSet\Enum\USB"
    try:
        with winreg.OpenKey(winreg.HKEY_LOCAL_MACHINE, root_path) as root:
            device_index = 0
            while True:
                try:
                    device_name = winreg.EnumKey(root, device_index)
                except OSError:
                    break
                device_index += 1
                lower_name = device_name.lower()
                if not any(
                    lower_name.startswith(prefix)
                    for prefix in AOA_DEVICE_PREFIXES
                ):
                    continue
                try:
                    with winreg.OpenKey(root, device_name) as device_key:
                        instance_index = 0
                        while True:
                            try:
                                instance = winreg.EnumKey(
                                    device_key, instance_index
                                )
                            except OSError:
                                break
                            instance_index += 1
                            parameters_path = (
                                f"{device_name}\\{instance}"
                                "\\Device Parameters"
                            )
                            try:
                                with winreg.OpenKey(
                                    root, parameters_path
                                ) as parameters:
                                    values, _ = winreg.QueryValueEx(
                                        parameters, "DeviceInterfaceGUIDs"
                                    )
                            except OSError:
                                continue
                            if isinstance(values, str):
                                values = [values]
                            for value in values:
                                if value not in result:
                                    result.append(value)
                except OSError:
                    continue
    except OSError:
        pass
    return result


class WinUsbConnection:
    def __init__(
        self,
        api: _WinUsbApi,
        file_handle: int,
        interface_handle: ctypes.c_void_p,
        endpoint_in: int,
        endpoint_out: int,
        read_depth: int = DEFAULT_READ_PIPELINE_DEPTH,
    ) -> None:
        self.api = api
        self.file_handle = file_handle
        self.interface_handle = interface_handle
        self.endpoint_in = endpoint_in
        self.endpoint_out = endpoint_out
        self.interface_number = 0
        self.device_vid: Optional[int] = None
        self.device_pid: Optional[int] = None
        self.read_depth = max(1, min(2, int(read_depth)))
        self._read_timeout: Optional[int] = None
        self._write_timeout: Optional[int] = None
        self._read_size = 0
        self._read_slots: list[_OverlappedRead] = []
        self._next_read_slot = 0
        self._deferred_read_error: Optional[WinUsbError] = None
        self._closed = False
        self._lifecycle_lock = threading.Lock()
        enabled = wintypes.BOOL(True)
        self.api.winusb.WinUsb_SetPipePolicy(
            self.interface_handle,
            self.endpoint_in,
            AUTO_CLEAR_STALL,
            ctypes.sizeof(enabled),
            ctypes.byref(enabled),
        )

    @classmethod
    def open_first(
        cls, read_depth: int = DEFAULT_READ_PIPELINE_DEPTH
    ) -> Optional["WinUsbConnection"]:
        if os.name != "nt":
            return None
        api = _WinUsbApi()
        last_error: Optional[WinUsbError] = None
        for guid in _accessory_interface_guids():
            try:
                paths = api.interface_paths(guid)
                for path in paths:
                    lower_path = path.lower()
                    if not any(
                        prefix in lower_path for prefix in AOA_DEVICE_PREFIXES
                    ):
                        continue
                    if "&adb" in lower_path or "&mi_01" in lower_path:
                        continue
                    try:
                        connection = cls._open_path(
                            api, path, read_depth=read_depth
                        )
                    except WinUsbError as error:
                        last_error = error
                        continue
                    if connection is not None:
                        return connection
            except WinUsbError as error:
                last_error = error
        if last_error is not None:
            raise last_error
        return None

    @classmethod
    def _open_path(
        cls,
        api: _WinUsbApi,
        path: str,
        read_depth: int = DEFAULT_READ_PIPELINE_DEPTH,
    ) -> Optional["WinUsbConnection"]:
        file_handle = api.kernel.CreateFileW(
            path,
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            None,
        )
        invalid = ctypes.c_void_p(-1).value
        if file_handle == invalid:
            raise api.last_error("open the AOA WinUSB interface")

        interface_handle = ctypes.c_void_p()
        try:
            if not api.winusb.WinUsb_Initialize(
                file_handle, ctypes.byref(interface_handle)
            ):
                raise api.last_error("initialize the AOA WinUSB interface")

            descriptor = UsbInterfaceDescriptor()
            if not api.winusb.WinUsb_QueryInterfaceSettings(
                interface_handle, 0, ctypes.byref(descriptor)
            ):
                raise api.last_error("read AOA WinUSB descriptors")

            endpoint_in = 0
            endpoint_out = 0
            for index in range(descriptor.bNumEndpoints):
                pipe = WinUsbPipeInformation()
                if not api.winusb.WinUsb_QueryPipe(
                    interface_handle, 0, index, ctypes.byref(pipe)
                ):
                    raise api.last_error("read an AOA WinUSB pipe")
                # USBD_PIPE_TYPE_BULK == 2.
                if pipe.PipeType != 2:
                    continue
                if pipe.PipeId & 0x80:
                    endpoint_in = pipe.PipeId
                else:
                    endpoint_out = pipe.PipeId

            if not endpoint_in or not endpoint_out:
                api.winusb.WinUsb_Free(interface_handle)
                api.kernel.CloseHandle(file_handle)
                return None
            connection = cls(
                api,
                file_handle,
                interface_handle,
                endpoint_in,
                endpoint_out,
                read_depth=read_depth,
            )
            connection.device_vid, connection.device_pid = _parse_device_ids(
                path
            )
            return connection
        except Exception:
            if interface_handle:
                api.winusb.WinUsb_Free(interface_handle)
            api.kernel.CloseHandle(file_handle)
            raise

    def _set_timeout(self, endpoint: int, timeout_ms: int) -> None:
        value = wintypes.ULONG(max(0, timeout_ms))
        if not self.api.winusb.WinUsb_SetPipePolicy(
            self.interface_handle,
            endpoint,
            PIPE_TRANSFER_TIMEOUT,
            ctypes.sizeof(value),
            ctypes.byref(value),
        ):
            raise self.api.last_error("set the AOA WinUSB timeout")

    def read(
        self, size: int = 4096, timeout_ms: int = DEFAULT_READ_TIMEOUT_MS
    ) -> bytes:
        if self._closed:
            raise WinUsbError("The AOA WinUSB connection is closed")
        if size <= 0:
            return b""

        if self._deferred_read_error is not None:
            error = self._deferred_read_error
            self._deferred_read_error = None
            raise error
        timeout_ms = max(0, int(timeout_ms))
        if self._read_timeout != timeout_ms:
            self._teardown_read_pipeline()
            self._set_timeout(self.endpoint_in, timeout_ms)
            self._read_timeout = timeout_ms
        if self._read_size != size:
            self._teardown_read_pipeline()
            self._read_size = size
        if not self._read_slots:
            self._start_read_pipeline()

        slot = self._read_slots[self._next_read_slot]
        payload = self._finish_read(slot, timeout_ms)
        if payload is None:
            return b""

        self._next_read_slot = (
            self._next_read_slot + 1
        ) % len(self._read_slots)
        try:
            # Repost immediately. With depth two the other slot remains ahead
            # in submission order, so no packet can be delivered out of order.
            self._post_read(slot)
        except WinUsbError as error:
            # The completed payload is still valid. Deliver it before surfacing
            # the failure on the next call.
            self._deferred_read_error = error
        return payload

    def _start_read_pipeline(self) -> None:
        slots: list[_OverlappedRead] = []
        try:
            for _ in range(self.read_depth):
                slots.append(_OverlappedRead(self.api, self._read_size))
            self._read_slots = slots
            self._next_read_slot = 0
            for slot in self._read_slots:
                self._post_read(slot)
        except Exception:
            self._read_slots = slots
            self._teardown_read_pipeline()
            raise

    def _post_read(self, slot: _OverlappedRead) -> None:
        ctypes.memset(
            ctypes.byref(slot.overlapped),
            0,
            ctypes.sizeof(slot.overlapped),
        )
        slot.overlapped.hEvent = slot.event
        if not self.api.kernel.ResetEvent(slot.event):
            raise self.api.last_error("reset an AOA read event")

        slot.active = True
        slot.in_flight = False
        slot.completed_inline = False
        slot.completion_error = None
        if self.api.winusb.WinUsb_ReadPipe(
            self.interface_handle,
            self.endpoint_in,
            slot.buffer,
            self._read_size,
            None,
            ctypes.byref(slot.overlapped),
        ):
            slot.completed_inline = True
            return

        error = ctypes.get_last_error()
        if error == ERROR_IO_PENDING:
            slot.in_flight = True
            return
        if error == ERROR_SEM_TIMEOUT:
            slot.completion_error = error
            return
        slot.active = False
        raise self.api.last_error("post an AOA touch read", error)

    def _finish_read(
        self, slot: _OverlappedRead, timeout_ms: int
    ) -> Optional[bytes]:
        if not slot.active:
            raise WinUsbError("The AOA WinUSB read pipeline is not active")

        if slot.in_flight:
            wait_result = self.api.kernel.WaitForSingleObject(
                slot.event, timeout_ms
            )
            if wait_result == WAIT_TIMEOUT:
                return None
            if wait_result == WAIT_FAILED:
                raise self.api.last_error("wait for an AOA touch read")
            if wait_result != WAIT_OBJECT_0:
                raise WinUsbError(
                    f"Unexpected AOA read wait result: {wait_result}"
                )

        error = slot.completion_error
        slot.transferred.value = 0
        if error is None:
            if not self.api.winusb.WinUsb_GetOverlappedResult(
                self.interface_handle,
                ctypes.byref(slot.overlapped),
                ctypes.byref(slot.transferred),
                False,
            ):
                error = ctypes.get_last_error()

        slot.active = False
        slot.in_flight = False
        slot.completed_inline = False
        slot.completion_error = None

        if error == ERROR_SEM_TIMEOUT:
            return b""
        if error is not None:
            raise self.api.last_error("read the AOA touch stream", error)
        return ctypes.string_at(slot.buffer, slot.transferred.value)

    def _teardown_read_pipeline(self) -> None:
        slots = self._read_slots
        self._read_slots = []
        self._next_read_slot = 0
        self._deferred_read_error = None
        if not slots:
            return

        for slot in slots:
            if slot.in_flight:
                if not self.api.kernel.CancelIoEx(
                    self.file_handle, ctypes.byref(slot.overlapped)
                ):
                    error = ctypes.get_last_error()
                    if error != ERROR_NOT_FOUND:
                        # Teardown is best-effort; closing the interface below
                        # also invalidates outstanding transfers.
                        pass

        for slot in slots:
            if slot.in_flight:
                slot.transferred.value = 0
                self.api.winusb.WinUsb_GetOverlappedResult(
                    self.interface_handle,
                    ctypes.byref(slot.overlapped),
                    ctypes.byref(slot.transferred),
                    True,
                )
            slot.active = False
            slot.in_flight = False
            if slot.event:
                self.api.kernel.CloseHandle(slot.event)
                slot.event = None

    def write(self, payload: bytes, timeout_ms: int = 1000) -> None:
        if self._write_timeout != timeout_ms:
            self._set_timeout(self.endpoint_out, timeout_ms)
            self._write_timeout = timeout_ms
        buffer = (ctypes.c_ubyte * len(payload)).from_buffer_copy(payload)
        transferred = wintypes.ULONG()
        if not self.api.winusb.WinUsb_WritePipe(
            self.interface_handle,
            self.endpoint_out,
            buffer,
            len(payload),
            ctypes.byref(transferred),
            None,
        ):
            raise self.api.last_error("write AOA configuration")
        if transferred.value != len(payload):
            raise WinUsbError("AOA configuration write was incomplete")

    def cancel_pending_read(self) -> None:
        """Cancel overlapped reads without freeing handles on this thread."""
        with self._lifecycle_lock:
            if self._closed:
                return
            try:
                self.api.kernel.CancelIoEx(self.file_handle, None)
            except Exception:
                # The receiver's bounded read wait remains the fallback.
                pass

    def close(self) -> None:
        with self._lifecycle_lock:
            if self._closed:
                return
            self._closed = True
            self._teardown_read_pipeline()
            self.api.winusb.WinUsb_Free(self.interface_handle)
            self.api.kernel.CloseHandle(self.file_handle)
