"""Android Open Accessory (AOA) USB transport.

The AOA handshake uses libusb directly through ctypes.  On Windows it can opt
into libusb's UsbDk backend so the handshake does not replace the phone's MTP
driver, then prefers the native WinUSB API for the latency-sensitive touch
stream and falls back to UsbDk when WinUSB is unavailable.  The companion
Android app emits fixed-size TOUCH_PACKET records.
"""

from __future__ import annotations

import ctypes
import os
import struct
import threading
import time
from collections import deque
from dataclasses import dataclass
from typing import Callable, Iterable, Optional

from connection_doctor import (
    ConnectionDoctor,
    ConnectionState,
)
import connection_doctor as doctor_codes
from winusb_transport import (
    DEFAULT_READ_PIPELINE_DEPTH,
    WinUsbConnection,
    WinUsbError,
)


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
HOST_CONTROL_MAGIC = b"HPTC"
HOST_CONTROL_PACKET = struct.Struct("<4sBBH")
HOST_CONTROL_ATTACH = 1
HOST_ATTACH_REQUEST = HOST_CONTROL_PACKET.pack(
    HOST_CONTROL_MAGIC,
    doctor_codes.TOUCH_PROTOCOL_VERSION,
    HOST_CONTROL_ATTACH,
    0,
)
ACTION_HEARTBEAT = 0
ACTION_DOWN = 1
ACTION_MOVE = 2
ACTION_UP = 3
ACTION_CANCEL = 4
FLAG_INSIDE = 0x01
FLAG_LOCKED = 0x02
FLAG_SESSION_RESET = 0x04
FLAG_HOST_RECOVERY = 0x08
FLAG_QUEUE_WARNING = 0x10
FLAG_QUEUE_RESYNC = 0x20
FLAG_QUEUE_FAILSAFE = 0x40
FLAG_QUEUE_DIAGNOSTICS = 0x80
QUEUE_AGE_REPORT_UNIT_NANOS = 10_000
WINUSB_ATTACH_GRACE_SECONDS = 1.5
INTERRUPTIBLE_READ_TIMEOUT_MS = 100

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
    """AOA transport failure with structured fields for the doctor.

    ``diag_code`` (when set) is the exact public code to report; otherwise the
    doctor classifies from ``native_name``/``native_code`` or the ``__cause__``
    chain. The message stays available internally but is never shown as
    user-facing text when a doctor is attached.
    """

    def __init__(
        self,
        message: str,
        operation: Optional[str] = None,
        native_name: Optional[str] = None,
        native_code: Optional[int] = None,
        diag_code: Optional[str] = None,
    ) -> None:
        super().__init__(message)
        self.operation = operation
        self.native_name = native_name
        self.native_code = native_code
        self.diag_code = diag_code


class TouchProtocolMismatch(AoaError):
    """Confirmed unsupported touch protocol for one connection attempt."""

    def __init__(self, received_version: int) -> None:
        self.received_version = int(received_version)
        self.expected_version = doctor_codes.TOUCH_PROTOCOL_VERSION
        self.diagnostic_metadata = {
            "received_touch_protocol_version": self.received_version,
            "expected_touch_protocol_version": self.expected_version,
        }
        super().__init__(
            "The phone and PC use incompatible touch protocol versions",
            diag_code=doctor_codes.HPT_APP_PROTOCOL_MISMATCH,
        )


class _AoaOperationCancelled(RuntimeError):
    """Internal control-flow signal for retry or shutdown."""


@dataclass(frozen=True)
class TouchEvent:
    action: int
    pointer_id: int
    flags: int
    x: float
    y: float
    sequence: int
    phone_event_nanos: int
    raw_x: int = 0
    raw_y: int = 0

    @property
    def inside(self) -> bool:
        return bool(self.flags & FLAG_INSIDE)

    @property
    def locked(self) -> bool:
        return bool(self.flags & FLAG_LOCKED)

    @property
    def session_reset(self) -> bool:
        return bool(
            self.action == ACTION_CANCEL
            and self.flags & FLAG_SESSION_RESET
        )

    @property
    def host_recovery(self) -> bool:
        return bool(
            self.session_reset and self.flags & FLAG_HOST_RECOVERY
        )

    @property
    def has_queue_diagnostics(self) -> bool:
        return bool(
            self.action == ACTION_HEARTBEAT
            and self.flags & FLAG_QUEUE_DIAGNOSTICS
        )

    @property
    def queue_age_nanos(self) -> int:
        if not self.has_queue_diagnostics:
            return 0
        return max(0, self.raw_x) * QUEUE_AGE_REPORT_UNIT_NANOS

    @property
    def queue_depth(self) -> int:
        return self.pointer_id if self.has_queue_diagnostics else 0

    @property
    def queue_resyncs(self) -> int:
        if not self.has_queue_diagnostics:
            return 0
        return max(0, self.raw_y)


@dataclass(frozen=True)
class LatencySnapshot:
    samples: int
    mean_excess_ms: float
    max_excess_ms: float
    p50_excess_ms: float
    p90_excess_ms: float
    p95_excess_ms: float
    p99_excess_ms: float
    p99_9_excess_ms: float
    window_seconds: float
    session_samples: int


@dataclass(frozen=True)
class QueueTelemetrySnapshot:
    reports: int
    max_age_ms: float
    max_depth: int
    warning_reports: int
    resyncs: int
    failsafe_reports: int
    host_recoveries: int
    first_warning_from_first_stroke_s: Optional[float]


class ClockNormalizedLatency:
    """Measure recent excess delay across independent, drifting clocks.

    Phone event time and ``perf_counter_ns`` have unrelated origins. The
    lower envelope of recent host-minus-phone offsets estimates clock skew.
    Residual delay is then reported relative to the fastest recent sample.
    This remains a relative jitter benchmark, not absolute one-way latency.
    """

    WINDOW_NANOS = 60_000_000_000
    MAX_OBSERVATIONS = 65_536
    SKEW_BUCKET_NANOS = 5_000_000_000
    MIN_SKEW_SPAN_NANOS = 10_000_000_000
    MIN_SKEW_BUCKETS = 3
    MAX_ABS_SKEW_MS_PER_SECOND = 0.5

    def __init__(self) -> None:
        self.reset()

    def reset(self) -> None:
        self._observations = deque(maxlen=self.MAX_OBSERVATIONS)
        self._latest_phone_nanos = 0
        self._session_samples = 0

    def observe(
        self,
        phone_event_nanos: int,
        host_arrival_nanos: int,
        include_sample: bool,
    ) -> None:
        if phone_event_nanos <= 0:
            return
        offset = host_arrival_nanos - phone_event_nanos
        self._observations.append(
            (phone_event_nanos, offset, include_sample)
        )
        self._latest_phone_nanos = max(
            self._latest_phone_nanos, phone_event_nanos
        )
        if include_sample:
            self._session_samples += 1

        cutoff = self._latest_phone_nanos - self.WINDOW_NANOS
        while (
            self._observations
            and self._observations[0][0] < cutoff
        ):
            self._observations.popleft()

    def _skew_trend(
        self, observations: list[tuple[int, int, bool]]
    ) -> Optional[tuple[int, int, float, float]]:
        """Fit clock-rate drift to minimum-delay points in 5-second buckets."""
        if not observations:
            return None
        first_phone = observations[0][0]
        buckets: dict[int, tuple[int, int]] = {}
        for phone_nanos, offset_nanos, _ in observations:
            bucket = (
                phone_nanos - first_phone
            ) // self.SKEW_BUCKET_NANOS
            current = buckets.get(bucket)
            if current is None or offset_nanos < current[1]:
                buckets[bucket] = (phone_nanos, offset_nanos)

        anchors = sorted(buckets.values(), key=lambda value: value[0])
        if (
            len(anchors) < self.MIN_SKEW_BUCKETS
            or anchors[-1][0] - anchors[0][0]
            < self.MIN_SKEW_SPAN_NANOS
        ):
            return None

        reference_phone, reference_offset = anchors[0]
        x_seconds = [
            (phone - reference_phone) / 1_000_000_000.0
            for phone, _ in anchors
        ]
        y_millis = [
            (offset - reference_offset) / 1_000_000.0
            for _, offset in anchors
        ]
        mean_x = sum(x_seconds) / len(x_seconds)
        mean_y = sum(y_millis) / len(y_millis)
        denominator = sum(
            (value - mean_x) ** 2 for value in x_seconds
        )
        if denominator <= 0:
            return None
        slope = sum(
            (x_value - mean_x) * (y_value - mean_y)
            for x_value, y_value in zip(x_seconds, y_millis)
        ) / denominator
        limit = self.MAX_ABS_SKEW_MS_PER_SECOND
        slope = max(-limit, min(limit, slope))
        intercept = mean_y - slope * mean_x
        return reference_phone, reference_offset, intercept, slope

    @staticmethod
    def _percentile(
        sorted_values: list[float], percentile: float
    ) -> float:
        """Return a linearly interpolated percentile from sorted values."""
        if not sorted_values:
            return 0.0
        position = (len(sorted_values) - 1) * percentile / 100.0
        lower = int(position)
        upper = min(lower + 1, len(sorted_values) - 1)
        fraction = position - lower
        return (
            sorted_values[lower] * (1.0 - fraction)
            + sorted_values[upper] * fraction
        )

    def snapshot(self) -> LatencySnapshot:
        observations = list(self._observations)
        if not observations:
            return LatencySnapshot(
                samples=0,
                mean_excess_ms=0.0,
                max_excess_ms=0.0,
                p50_excess_ms=0.0,
                p90_excess_ms=0.0,
                p95_excess_ms=0.0,
                p99_excess_ms=0.0,
                p99_9_excess_ms=0.0,
                window_seconds=0.0,
                session_samples=self._session_samples,
            )

        trend = self._skew_trend(observations)
        residuals: list[tuple[float, bool]] = []
        for phone_nanos, offset_nanos, include_sample in observations:
            if trend is None:
                residual = float(offset_nanos)
            else:
                (
                    reference_phone,
                    reference_offset,
                    intercept,
                    slope,
                ) = trend
                elapsed_seconds = (
                    phone_nanos - reference_phone
                ) / 1_000_000_000.0
                predicted_offset = reference_offset + (
                    intercept + slope * elapsed_seconds
                ) * 1_000_000.0
                residual = offset_nanos - predicted_offset
            residuals.append((residual, include_sample))

        baseline = min(residual for residual, _ in residuals)
        excess_nanos = [
            max(0.0, residual - baseline)
            for residual, include_sample in residuals
            if include_sample
        ]
        samples = len(excess_nanos)
        mean_ns = sum(excess_nanos) / samples if samples else 0.0
        sorted_excess_nanos = sorted(excess_nanos)
        max_ns = sorted_excess_nanos[-1] if sorted_excess_nanos else 0.0
        window_seconds = max(
            0.0,
            (
                max(value[0] for value in observations)
                - min(value[0] for value in observations)
            )
            / 1_000_000_000.0,
        )
        return LatencySnapshot(
            samples=samples,
            mean_excess_ms=mean_ns / 1_000_000.0,
            max_excess_ms=max_ns / 1_000_000.0,
            p50_excess_ms=(
                self._percentile(sorted_excess_nanos, 50.0) / 1_000_000.0
            ),
            p90_excess_ms=(
                self._percentile(sorted_excess_nanos, 90.0) / 1_000_000.0
            ),
            p95_excess_ms=(
                self._percentile(sorted_excess_nanos, 95.0) / 1_000_000.0
            ),
            p99_excess_ms=(
                self._percentile(sorted_excess_nanos, 99.0) / 1_000_000.0
            ),
            p99_9_excess_ms=(
                self._percentile(sorted_excess_nanos, 99.9) / 1_000_000.0
            ),
            window_seconds=window_seconds,
            session_samples=self._session_samples,
        )


class QueueTelemetry:
    def __init__(self) -> None:
        self.reset()

    def reset(self, preserve_host_recoveries: bool = False) -> None:
        host_recoveries = (
            getattr(self, "host_recoveries", 0)
            if preserve_host_recoveries
            else 0
        )
        self.reports = 0
        self.max_age_nanos = 0
        self.max_depth = 0
        self.warning_reports = 0
        self.resyncs = 0
        self.failsafe_reports = 0
        self.host_recoveries = host_recoveries
        self.first_stroke_nanos: Optional[int] = None
        self.first_warning_report_nanos: Optional[int] = None

    def begin_epoch(self, recovered: bool) -> None:
        self.reset(preserve_host_recoveries=True)
        if recovered:
            self.host_recoveries += 1

    def observe(self, event: TouchEvent) -> None:
        if (
            event.action == ACTION_DOWN
            and event.phone_event_nanos > 0
            and self.first_stroke_nanos is None
        ):
            self.first_stroke_nanos = event.phone_event_nanos
        if not event.has_queue_diagnostics:
            return
        self.reports += 1
        self.max_age_nanos = max(
            self.max_age_nanos, event.queue_age_nanos
        )
        self.max_depth = max(self.max_depth, event.queue_depth)
        self.resyncs += event.queue_resyncs
        if event.flags & FLAG_QUEUE_WARNING:
            self.warning_reports += 1
            if (
                event.phone_event_nanos > 0
                and self.first_warning_report_nanos is None
            ):
                self.first_warning_report_nanos = event.phone_event_nanos
        if event.flags & FLAG_QUEUE_FAILSAFE:
            self.failsafe_reports += 1

    def snapshot(self) -> QueueTelemetrySnapshot:
        first_warning_offset = None
        if (
            self.first_stroke_nanos is not None
            and self.first_warning_report_nanos is not None
        ):
            first_warning_offset = (
                self.first_warning_report_nanos - self.first_stroke_nanos
            ) / 1_000_000_000.0
        return QueueTelemetrySnapshot(
            reports=self.reports,
            max_age_ms=self.max_age_nanos / 1_000_000.0,
            max_depth=self.max_depth,
            warning_reports=self.warning_reports,
            resyncs=self.resyncs,
            failsafe_reports=self.failsafe_reports,
            host_recoveries=self.host_recoveries,
            first_warning_from_first_stroke_s=first_warning_offset,
        )


class TouchPacketParser:
    """Resynchronizing parser for the fixed-size AOA touch stream."""

    def __init__(
        self, on_bad_version: Optional[Callable[[int], None]] = None
    ) -> None:
        self._buffer = bytearray()
        self._on_bad_version = on_bad_version
        self.bad_version_packets = 0

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
            if (
                magic != TOUCH_MAGIC
                or version != doctor_codes.TOUCH_PROTOCOL_VERSION
            ):
                if version != doctor_codes.TOUCH_PROTOCOL_VERSION:
                    self.bad_version_packets += 1
                    if self._on_bad_version is not None:
                        self._on_bad_version(version)
                continue
            yield TouchEvent(
                action=action,
                pointer_id=pointer_id,
                flags=flags,
                x=x / 10000.0,
                y=y / 10000.0,
                sequence=seq,
                phone_event_nanos=event_ns,
                raw_x=x,
                raw_y=y,
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
        interrupt = getattr(lib, "libusb_interrupt_event_handler", None)
        if interrupt is not None:
            interrupt.argtypes = [ctypes.c_void_p]
            interrupt.restype = None

    def close(self) -> None:
        if self.context:
            self.lib.libusb_exit(self.context)
            self.context = ctypes.c_void_p()

    def error_name(self, code: int) -> str:
        value = self.lib.libusb_error_name(code)
        return value.decode("ascii", "replace") if value else str(code)

    def check(self, code: int, operation: str) -> None:
        if code < 0:
            raise AoaError(
                f"Could not {operation}: {self.error_name(code)}",
                operation=operation,
                native_name=self.error_name(code),
                native_code=int(code),
            )

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
        self._cancel_requested = threading.Event()

    def read(
        self,
        size: int = 4096,
        timeout_ms: int = INTERRUPTIBLE_READ_TIMEOUT_MS,
    ) -> bytes:
        if self._cancel_requested.is_set():
            return b""
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
        if self._cancel_requested.is_set():
            return b""
        if rc == LIBUSB_ERROR_TIMEOUT:
            return b""
        self.usb.check(rc, "read the AOA touch stream")
        return bytes(buffer[: transferred.value])

    def cancel_pending_read(self) -> None:
        """Wake or bound an in-progress synchronous libusb read.

        libusb's synchronous transfer is timeout-bounded above. Its event-loop
        interrupt shortens that wait on runtimes which export the helper.
        """
        self._cancel_requested.set()
        interrupt = getattr(
            self.usb.lib, "libusb_interrupt_event_handler", None
        )
        if interrupt is not None and self.usb.context:
            try:
                interrupt(self.usb.context)
            except Exception:
                pass

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
        self.usb.check(rc, "write AOA host attach")
        if transferred.value != len(payload):
            raise AoaError("AOA host attach write was incomplete")

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
        winusb_read_depth: int = DEFAULT_READ_PIPELINE_DEPTH,
        doctor: Optional[ConnectionDoctor] = None,
        cancel_event: Optional[threading.Event] = None,
    ) -> None:
        self.prefer_usbdk = bool(use_usbdk and os.name == "nt")
        self.usb = Libusb(use_usbdk=use_usbdk)
        self._usbdk_data_fallback_attempted = False
        self._usbdk_data_fallback_pending = False
        self.winusb_read_depth = max(1, min(2, winusb_read_depth))
        self.vendor_ids = set(ANDROID_VENDOR_IDS)
        self.doctor = doctor
        self._cancel_event = cancel_event
        if doctor is not None:
            doctor.note_usbdk_status(
                "active" if self.usb.using_usbdk else "not active"
            )
            doctor.note_winusb_status("not tried yet")
        if extra_vendor_id is not None:
            self.vendor_ids.add(extra_vendor_id)

    def _diag_emit(self, code: str, **kwargs) -> None:
        doctor = getattr(self, "doctor", None)
        if doctor is not None:
            doctor.emit(code, **kwargs)

    def _check_cancelled(self) -> None:
        cancel_event = getattr(self, "_cancel_event", None)
        if cancel_event is not None and cancel_event.is_set():
            raise _AoaOperationCancelled()

    def _interruptible_wait(self, timeout: float) -> None:
        cancel_event = getattr(self, "_cancel_event", None)
        if cancel_event is None:
            time.sleep(timeout)
        elif cancel_event.wait(timeout):
            raise _AoaOperationCancelled()

    def close(self) -> None:
        self.usb.close()

    def connect(
        self, switch_timeout: float = 12.0
    ) -> AoaConnection | WinUsbConnection:
        # UsbDk can temporarily detach Samsung's normal MTP/ADB drivers for
        # the AOA control handshake. Prefer WinUSB for the long bulk stream,
        # while retaining UsbDk as a compatibility fallback for systems where
        # the optional WinUSB package was skipped or could not be installed.
        doctor = getattr(self, "doctor", None)
        self._check_cancelled()
        if doctor is not None:
            doctor.transition(ConnectionState.DISCOVERY)
        accessory_present = (
            self.usb.using_usbdk and self._accessory_present()
        )
        if accessory_present:
            self._check_cancelled()
            if doctor is not None:
                doctor.transition(ConnectionState.REENUMERATION)
            self._replace_usb(use_usbdk=False)
            return self._wait_for_data_accessory(switch_timeout)

        try:
            connection = self._find_data_accessory()
        except AoaError:
            if not self._enable_usbdk_data_fallback():
                raise
            connection = self._find_data_accessory()
        if connection:
            try:
                self._check_cancelled()
            except _AoaOperationCancelled:
                connection.close()
                raise
            self._record_usbdk_fallback_success()
            return connection

        if self.prefer_usbdk and not self.usb.using_usbdk:
            self._enable_usbdk_data_fallback()
            connection = self._find_data_accessory()
            if connection:
                try:
                    self._check_cancelled()
                except _AoaOperationCancelled:
                    connection.close()
                    raise
                self._record_usbdk_fallback_success()
                return connection

        switched = self._request_accessory_mode()
        if not switched:
            outcome = getattr(self, "_last_handshake_outcome", "no-candidate")
            terminal_code = {
                "no-candidate": doctor_codes.HPT_DISC_NO_PHONE,
                "unsupported": doctor_codes.HPT_AOA_UNSUPPORTED,
                "capability-failed": (
                    doctor_codes.HPT_AOA_CAPABILITY_FAILED
                ),
                "negotiation-failed": (
                    doctor_codes.HPT_AOA_NEGOTIATION_FAILED
                ),
            }.get(outcome, doctor_codes.HPT_DISC_NO_PHONE)
            if doctor is not None:
                doctor.emit(terminal_code, state=ConnectionState.FAILURE)
            driver_note = (
                " UsbDk is not active; install UsbDk or bind WinUSB to the "
                "AOA interface. A new UsbDk installation requires a Windows "
                "restart."
                if os.name == "nt" and not self.usb.using_usbdk
                else ""
            )
            raise AoaError(
                "No accessible Android phone supports the AOA handshake."
                + driver_note,
                diag_code=terminal_code,
            )

        self._diag_emit(
            doctor_codes.HPT_AOA_NEGOTIATION_ACCEPTED,
            state=ConnectionState.REENUMERATION,
        )

        if self.usb.using_usbdk:
            # Let UsbDk finish releasing its redirect before WinUSB starts
            # opening the newly enumerated accessory interface.
            self._interruptible_wait(0.1)
            self._replace_usb(use_usbdk=False)

        return self._wait_for_data_accessory(switch_timeout)

    def _wait_for_data_accessory(
        self, switch_timeout: float
    ) -> AoaConnection | WinUsbConnection:
        doctor = getattr(self, "doctor", None)
        if doctor is not None:
            doctor.transition(ConnectionState.REENUMERATION)
            doctor.note_winusb_status("probing")
        deadline = time.monotonic() + switch_timeout
        fallback_at = time.monotonic() + min(
            WINUSB_ATTACH_GRACE_SECONDS, switch_timeout
        )
        last_open_error: Optional[AoaError] = None
        while time.monotonic() < deadline:
            self._check_cancelled()
            try:
                connection = self._find_data_accessory()
            except AoaError as error:
                # Windows can enumerate the new composite device before
                # WinUSB has finished attaching to interface 0. Treat that
                # brief ACCESS/IO window as part of re-enumeration. If UsbDk
                # is available, an unusable WinUSB path is also a definite
                # signal to activate the compatibility backend immediately.
                last_open_error = error
                if doctor is not None:
                    attempt_state = (
                        ConnectionState.USBDK_FALLBACK
                        if self.usb.using_usbdk
                        else ConnectionState.WINUSB_OPEN
                    )
                    classified = doctor_codes.classify_error(
                        error, state=attempt_state
                    )
                    if self.usb.using_usbdk:
                        doctor.note_usbdk_status(
                            "probing (last error: " + classified + ")"
                        )
                    else:
                        doctor.emit(
                            classified,
                            state=attempt_state,
                            detail="opening the re-enumerated accessory",
                            exc=error,
                        )
                        doctor.note_winusb_status("failed: " + classified)
                if self._enable_usbdk_data_fallback():
                    continue
            else:
                if connection:
                    try:
                        self._check_cancelled()
                    except _AoaOperationCancelled:
                        connection.close()
                        raise
                    self._record_usbdk_fallback_success()
                    if doctor is not None:
                        doctor.emit(
                            doctor_codes.HPT_USB_REENUMERATED,
                            state=(
                                ConnectionState.USBDK_FALLBACK
                                if self.usb.using_usbdk
                                else ConnectionState.WINUSB_OPEN
                            ),
                        )
                        if self.usb.using_usbdk:
                            doctor.note_usbdk_status("active")
                        else:
                            doctor.note_winusb_status("ok")
                    return connection

            if (
                time.monotonic() >= fallback_at
                and self._enable_usbdk_data_fallback()
            ):
                continue
            self._interruptible_wait(0.15)
        if last_open_error is not None:
            if doctor is not None:
                final_code = doctor_codes.classify_error(
                    last_open_error,
                    state=(
                        ConnectionState.USBDK_FALLBACK
                        if self.usb.using_usbdk
                        else ConnectionState.WINUSB_OPEN
                    ),
                )
                if self.usb.using_usbdk and final_code in (
                    doctor_codes.HPT_USB_WINUSB_NOT_SUPPORTED,
                    doctor_codes.HPT_USB_WINUSB_UNAVAILABLE,
                    doctor_codes.HPT_USB_WINUSB_TRANSIENT,
                ):
                    # The compatibility backend owned the last open attempt
                    # and could not use the device either.
                    final_code = doctor_codes.HPT_USB_USBDK_FALLBACK_FAILED
                doctor.emit(
                    final_code,
                    state=ConnectionState.FAILURE,
                    detail="the re-enumerated accessory never opened",
                    exc=last_open_error,
                )
                if self.usb.using_usbdk:
                    doctor.note_usbdk_status("failed: " + final_code)
                else:
                    doctor.note_winusb_status("failed: " + final_code)
            raise last_open_error
        timeout_error = AoaError(
            "The phone accepted AOA mode but did not reappear as an "
            "Android Accessory. Reconnect the cable and accept the app prompt.",
            diag_code=doctor_codes.HPT_USB_REENUM_TIMEOUT,
        )
        if doctor is not None:
            doctor.emit(
                doctor_codes.HPT_USB_REENUM_TIMEOUT,
                state=ConnectionState.FAILURE,
                detail=f"waited {switch_timeout:.0f}s for the accessory",
                exc=timeout_error,
            )
        raise timeout_error

    def _enable_usbdk_data_fallback(self) -> bool:
        self._check_cancelled()
        if (
            not self.prefer_usbdk
            or self.usb.using_usbdk
            or getattr(self, "_usbdk_data_fallback_attempted", False)
        ):
            return False
        doctor = getattr(self, "doctor", None)
        self._diag_emit(
            doctor_codes.HPT_USB_USBDK_FALLBACK_START,
            state=ConnectionState.USBDK_FALLBACK,
        )
        self._usbdk_data_fallback_attempted = True
        self._replace_usb(use_usbdk=True)
        if doctor is not None:
            if self.usb.using_usbdk:
                self._usbdk_data_fallback_pending = True
                doctor.note_usbdk_status("probing (fallback)")
            else:
                doctor.emit(
                    doctor_codes.HPT_USB_USBDK_UNAVAILABLE,
                    state=ConnectionState.USBDK_FALLBACK,
                )
                doctor.note_usbdk_status("unavailable")
        return self.usb.using_usbdk

    def _record_usbdk_fallback_success(self) -> None:
        """Mark fallback success only after an accessory actually opens."""
        if not (
            self.usb.using_usbdk
            and getattr(self, "_usbdk_data_fallback_pending", False)
        ):
            return
        self._usbdk_data_fallback_pending = False
        doctor = getattr(self, "doctor", None)
        if doctor is not None:
            doctor.emit(
                doctor_codes.HPT_USB_USBDK_FALLBACK_OK,
                state=ConnectionState.USBDK_FALLBACK,
            )
            doctor.note_usbdk_status("active (fallback)")

    def _replace_usb(self, use_usbdk: bool) -> None:
        previous = self.usb
        self.usb = Libusb(use_usbdk=use_usbdk)
        if not use_usbdk and previous.using_usbdk:
            # A successful handshake starts a fresh WinUSB-first data phase.
            # Re-arm the same known-good UsbDk context as its fallback.
            self._usbdk_data_fallback_attempted = False
            self._usbdk_data_fallback_pending = False
        previous.close()

    def _accessory_present(self) -> bool:
        self._check_cancelled()
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
        self._check_cancelled()
        if os.name == "nt" and not self.usb.using_usbdk:
            try:
                connection = WinUsbConnection.open_first(
                    read_depth=self.winusb_read_depth
                )
            except WinUsbError as error:
                raise AoaError(str(error)) from error
            if connection is not None:
                return connection
        return self._find_accessory()

    def _find_accessory(self) -> Optional[AoaConnection]:
        self._check_cancelled()
        devices = self.usb.devices()
        keep: Optional[ctypes.c_void_p] = None
        last_open_error: Optional[AoaError] = None
        try:
            for device, descriptor in devices:
                self._check_cancelled()
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
                    doctor = getattr(self, "doctor", None)
                    if doctor is not None:
                        doctor.note_accessory(
                            descriptor.idVendor,
                            descriptor.idProduct,
                            connection.interface_number,
                        )
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
        self._check_cancelled()
        devices = self.usb.devices()
        doctor = getattr(self, "doctor", None)
        self._last_handshake_outcome = "no-candidate"
        try:
            for device, descriptor in devices:
                self._check_cancelled()
                if descriptor.idVendor not in self.vendor_ids:
                    continue
                if (
                    descriptor.idVendor == AOA_VENDOR_ID
                    and descriptor.idProduct in AOA_PRODUCT_IDS
                ):
                    continue
                handle = None
                phase = ConnectionState.AOA_CAPABILITY_CHECK
                try:
                    if doctor is not None:
                        doctor.transition(phase)
                    handle = self.usb.open(device)
                    raw_version = self.usb.control_in(
                        handle, AOA_GET_PROTOCOL, 2
                    )
                    if len(raw_version) != 2:
                        self._last_handshake_outcome = "capability-failed"
                        self._diag_emit(
                            doctor_codes.HPT_AOA_CAPABILITY_FAILED,
                            state=ConnectionState.AOA_CAPABILITY_CHECK,
                            detail="empty answer to the AOA protocol request",
                        )
                        continue
                    version = int.from_bytes(raw_version, "little")
                    if doctor is not None:
                        doctor.note_aoa_protocol_version(version)
                    if version < 1:
                        self._last_handshake_outcome = "unsupported"
                        self._diag_emit(
                            doctor_codes.HPT_AOA_UNSUPPORTED,
                            state=ConnectionState.AOA_CAPABILITY_CHECK,
                            detail=f"AOA protocol version {version}",
                        )
                        continue
                    phase = ConnectionState.AOA_NEGOTIATION
                    if doctor is not None:
                        doctor.transition(phase)
                    for index, value in enumerate(self.IDENT):
                        self._check_cancelled()
                        self.usb.control_out(
                            handle,
                            AOA_SEND_IDENT,
                            index=index,
                            payload=value + b"\0",
                        )
                        # A short setup-only pace is harmless to touch latency
                        # and accommodates older Android gadget stacks.
                        self._interruptible_wait(0.02)
                    self._interruptible_wait(0.05)
                    self.usb.control_out(handle, AOA_START)
                    # AOA_START is asynchronous. Keep the control handle alive
                    # briefly while Android begins USB re-enumeration.
                    self._interruptible_wait(0.25)
                    return True
                except AoaError as error:
                    capability_failure = (
                        phase == ConnectionState.AOA_CAPABILITY_CHECK
                    )
                    self._last_handshake_outcome = (
                        "capability-failed"
                        if capability_failure
                        else "negotiation-failed"
                    )
                    self._diag_emit(
                        (
                            doctor_codes.HPT_AOA_CAPABILITY_FAILED
                            if capability_failure
                            else doctor_codes.HPT_AOA_NEGOTIATION_FAILED
                        ),
                        state=phase,
                        detail=error.operation or "AOA control transfer",
                        exc=error,
                    )
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
    RECONNECT_BACKOFF_SECONDS = 1.0

    def __init__(
        self,
        on_event: Callable[[TouchEvent], None],
        on_status: Callable[[str, bool], None],
        on_disconnect: Callable[[], None],
        lane_count: int,
        use_usbdk: bool = True,
        extra_vendor_id: Optional[int] = None,
        winusb_read_depth: int = DEFAULT_READ_PIPELINE_DEPTH,
        benchmark: bool = False,
        doctor: Optional[ConnectionDoctor] = None,
    ) -> None:
        self.on_event = on_event
        self.on_status = on_status
        self.on_disconnect = on_disconnect
        self.lane_count = max(1, min(16, lane_count))
        self.use_usbdk = use_usbdk
        self.extra_vendor_id = extra_vendor_id
        self.winusb_read_depth = max(1, min(2, winusb_read_depth))
        self.benchmark = benchmark
        self.doctor = doctor
        self._latency = ClockNormalizedLatency()
        self._queue_telemetry = QueueTelemetry()
        self._stop = threading.Event()
        self._control_event = threading.Event()
        self._lifecycle_lock = threading.Lock()
        self._retry_generation = 0
        self._handled_retry_generation = 0
        self.finished = threading.Event()
        self.transport_open = threading.Event()
        self.session_ready = threading.Event()
        self.connected = self.session_ready
        self._thread: Optional[threading.Thread] = None
        self._connection: Optional[AoaConnection | WinUsbConnection] = None
        self._host: Optional[AoaHost] = None
        self._app_handshake_done = False
        self._had_stream_failure = False

    def start(self) -> None:
        self._thread = threading.Thread(
            target=self._run, name="AOA receiver", daemon=True
        )
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        self._control_event.set()
        self._interrupt_active_read()
        if self._thread and self._thread is not threading.current_thread():
            self._thread.join(timeout=2)

    def request_retry(self) -> bool:
        """Force the current attempt to tear down and reconnect immediately."""
        with self._lifecycle_lock:
            if self._stop.is_set() or self.finished.is_set():
                return False
            self._retry_generation += 1
            self._control_event.set()
        if self.doctor is not None:
            self.doctor.emit(doctor_codes.HPT_SESSION_RETRY_REQUESTED)
        self._interrupt_active_read()
        return True

    def latency_snapshot(self) -> LatencySnapshot:
        return self._latency.snapshot()

    def queue_telemetry_snapshot(self) -> QueueTelemetrySnapshot:
        return self._queue_telemetry.snapshot()

    def _interrupt_active_read(self) -> None:
        with self._lifecycle_lock:
            connection = self._connection
        if connection is None:
            return
        cancel = getattr(connection, "cancel_pending_read", None)
        try:
            if callable(cancel):
                cancel()
            else:
                # Built-in backends expose cancellation. This close fallback
                # keeps simple third-party/test backends interruptible.
                connection.close()
        except Exception:
            pass

    def _retry_pending(self) -> bool:
        with self._lifecycle_lock:
            return self._retry_generation != self._handled_retry_generation

    def _consume_retry(self) -> bool:
        with self._lifecycle_lock:
            pending = (
                self._retry_generation != self._handled_retry_generation
            )
            self._handled_retry_generation = self._retry_generation
            if not self._stop.is_set():
                self._control_event.clear()
            return pending

    def _set_connection(
        self, connection: Optional[AoaConnection | WinUsbConnection]
    ) -> None:
        with self._lifecycle_lock:
            self._connection = connection

    def _take_connection(
        self,
    ) -> Optional[AoaConnection | WinUsbConnection]:
        with self._lifecycle_lock:
            connection = self._connection
            self._connection = None
            return connection

    def _set_host(self, host: Optional[AoaHost]) -> None:
        with self._lifecycle_lock:
            self._host = host

    def _new_host(self) -> AoaHost:
        host = AoaHost(
            use_usbdk=self.use_usbdk,
            extra_vendor_id=self.extra_vendor_id,
            winusb_read_depth=self.winusb_read_depth,
            doctor=self.doctor,
            cancel_event=self._control_event,
        )
        self._set_host(host)
        backend = "UsbDk" if host.usb.using_usbdk else "WinUSB"
        try:
            self.on_status(f"USB backend available: {backend}", False)
        except Exception:
            self._close_host(host)
            raise
        return host

    def _close_connection(self) -> None:
        connection = self._take_connection()
        if connection is None:
            return
        try:
            connection.close()
        except Exception:
            pass

    def _close_host(self, host: Optional[AoaHost]) -> None:
        self._set_host(None)
        if host is None:
            return
        try:
            host.close()
        except Exception:
            pass

    def _release_attempt(self) -> None:
        self.session_ready.clear()
        self.transport_open.clear()
        try:
            self.on_disconnect()
        finally:
            self._close_connection()

    def _report_host_start_failure(self, exc: BaseException) -> None:
        if self.doctor is not None:
            event = self.doctor.fail(exc, state=ConnectionState.FAILURE)
            message = f"{event.summary} {event.action}".strip()
        else:
            message = str(exc)
        self.on_status(message, False)

    def _run(self) -> None:
        host: Optional[AoaHost] = None
        try:
            if os.name == "nt":
                try:
                    ctypes.windll.kernel32.SetThreadPriority(
                        ctypes.windll.kernel32.GetCurrentThread(), 2
                    )
                except Exception:
                    pass

            try:
                host = self._new_host()
            except Exception as exc:
                self._report_host_start_failure(exc)
                self._release_attempt()
                return

            last_error = ""
            while not self._stop.is_set():
                if self._retry_pending():
                    self._consume_retry()
                    self._release_attempt()
                    self._close_host(host)
                    try:
                        host = self._new_host()
                    except Exception as exc:
                        self._report_host_start_failure(exc)
                        break

                retry_attempt = False
                connection_failed = False
                terminal_mismatch: Optional[TouchProtocolMismatch] = None
                try:
                    if self.doctor is not None and last_error:
                        self.doctor.note_reconnect_attempt()
                        self.doctor.emit(
                            doctor_codes.HPT_LINK_RECONNECTING,
                            state=ConnectionState.DISCOVERY,
                        )
                    self.on_status("Looking for Android accessory…", False)
                    if self._control_event.is_set():
                        raise _AoaOperationCancelled()
                    connection = host.connect()
                    self._set_connection(connection)
                    if self._control_event.is_set():
                        raise _AoaOperationCancelled()

                    backend = "UsbDk" if host.usb.using_usbdk else "WinUSB"
                    self.transport_open.set()
                    self.session_ready.clear()
                    doctor = self.doctor
                    if doctor is not None:
                        doctor.note_backend(backend)
                        doctor.note_accessory(
                            getattr(connection, "device_vid", None),
                            getattr(connection, "device_pid", None),
                            getattr(connection, "interface_number", None),
                        )
                        doctor.emit(
                            doctor_codes.HPT_USB_BACKEND_SELECTED,
                            state=ConnectionState.USB_TRANSPORT_OPEN,
                            detail=(
                                f"backend={backend} "
                                f"interface="
                                f"{getattr(connection, 'interface_number', '?')} "
                                f"in=0x{connection.endpoint_in:02x} "
                                f"out=0x{connection.endpoint_out:02x}"
                            ),
                        )
                        doctor.transition(
                            ConnectionState.WAITING_FOR_ANDROID_APP
                        )
                    self.on_status(
                        f"USB connected over {backend} — "
                        "waiting for Android app…",
                        False,
                    )
                    # Touch traffic remains phone-to-host. One small attach
                    # control record gives Android a reliable host-process
                    # boundary without adding work to the steady-state path.
                    last_error = ""
                    handshake_done = False
                    attach_requested = False
                    self._app_handshake_done = False

                    def bad_version(version: int) -> None:
                        raise TouchProtocolMismatch(version)

                    parser = TouchPacketParser(on_bad_version=bad_version)
                    self._latency.reset()
                    self._queue_telemetry.reset()
                    last_packet_at = time.monotonic()
                    epoch_wait_started_at = last_packet_at
                    while not self._stop.is_set():
                        if self._control_event.is_set():
                            raise _AoaOperationCancelled()
                        chunk = connection.read()
                        if self._control_event.is_set():
                            raise _AoaOperationCancelled()
                        arrival_nanos = time.perf_counter_ns()
                        now = time.monotonic()
                        parsed_events = list(parser.feed(chunk))
                        if parsed_events and not attach_requested:
                            connection.write(
                                HOST_ATTACH_REQUEST, timeout_ms=500
                            )
                            attach_requested = True
                        received_epoch_packet = False
                        became_ready = False
                        for event in parsed_events:
                            if not handshake_done and not event.session_reset:
                                # A restarted host may first drain records that
                                # Android queued for the previous process. They
                                # do not belong to this transport epoch.
                                continue

                            if event.session_reset:
                                self._latency.reset()
                                self._queue_telemetry.begin_epoch(
                                    event.host_recovery
                                )

                            last_packet_at = now
                            received_epoch_packet = True
                            if doctor is not None:
                                doctor.note_packet(
                                    event.action == ACTION_HEARTBEAT,
                                    observed_at=now,
                                )
                            if not handshake_done:
                                handshake_done = True
                                self._app_handshake_done = True
                                self.session_ready.set()
                                became_ready = True
                                if doctor is not None:
                                    doctor.note_touch_protocol_version(
                                        doctor_codes.TOUCH_PROTOCOL_VERSION
                                    )
                                    doctor.emit(
                                        doctor_codes.HPT_APP_HANDSHAKE_OK,
                                        state=(
                                            ConnectionState
                                            .PROTOCOL_HANDSHAKE_COMPLETE
                                        ),
                                    )
                                    doctor.emit(
                                        doctor_codes.HPT_STREAM_RESUMED
                                        if self._had_stream_failure
                                        else doctor_codes.HPT_STREAM_CONNECTED,
                                        state=(
                                            ConnectionState
                                            .TOUCH_STREAM_ACTIVE
                                        ),
                                    )
                            elif event.session_reset:
                                # A write that resumed after the host reader
                                # disappeared defines a fresh epoch. Route its
                                # CANCEL so no pre-stall key state survives.
                                self.on_event(event)

                            self._queue_telemetry.observe(event)
                            if self.benchmark:
                                self._latency.observe(
                                    event.phone_event_nanos,
                                    arrival_nanos,
                                    include_sample=(
                                        event.action != ACTION_HEARTBEAT
                                        and not event.session_reset
                                    ),
                                )
                            if event.session_reset:
                                # The initial marker only establishes the
                                # epoch. A later recovery marker was already
                                # routed above to clear host input state.
                                continue
                            # Heartbeats participate in sequence tracking. The
                            # router ignores them after verifying no wire record
                            # was skipped.
                            self.on_event(event)
                        if became_ready:
                            self.on_status(
                                f"Android app responded over {backend} — "
                                "touch stream active",
                                True,
                            )
                        if (
                            not handshake_done
                            and now - epoch_wait_started_at
                            >= self.HEARTBEAT_TIMEOUT_SECONDS
                        ):
                            raise AoaError(
                                "AOA session reset marker timed out; "
                                "reconnecting",
                                diag_code=doctor_codes.HPT_APP_NOT_RESPONDING,
                            )
                        if (
                            handshake_done
                            and not received_epoch_packet
                            and now - last_packet_at
                            >= self.HEARTBEAT_TIMEOUT_SECONDS
                        ):
                            raise AoaError(
                                "AOA heartbeat timed out; reconnecting",
                                diag_code=doctor_codes.HPT_STREAM_STALLED,
                            )
                except _AoaOperationCancelled:
                    retry_attempt = not self._stop.is_set()
                except Exception as exc:
                    if self._stop.is_set() or self._control_event.is_set():
                        retry_attempt = not self._stop.is_set()
                    else:
                        connection_failed = True
                        if self._app_handshake_done:
                            self._had_stream_failure = True
                        if isinstance(exc, TouchProtocolMismatch):
                            terminal_mismatch = exc
                        else:
                            message = str(exc)
                            doctor = self.doctor
                            if doctor is not None:
                                event = doctor.fail(
                                    exc, detail="connection attempt"
                                )
                                message = (
                                    f"{event.summary} {event.action}".strip()
                                )
                            if message != last_error:
                                self.on_status(message, False)
                                last_error = message
                finally:
                    self._release_attempt()

                if terminal_mismatch is not None:
                    message = str(terminal_mismatch)
                    doctor = self.doctor
                    if doctor is not None:
                        doctor.note_touch_protocol_version(
                            terminal_mismatch.received_version
                        )
                        event = doctor.fail(
                            terminal_mismatch,
                            state=ConnectionState.FAILURE,
                            detail=(
                                "phone sent touch protocol "
                                f"v{terminal_mismatch.received_version}; "
                                "PC expects "
                                f"v{terminal_mismatch.expected_version}"
                            ),
                        )
                        message = f"{event.summary} {event.action}".strip()
                    if message != last_error:
                        self.on_status(message, False)
                        last_error = message

                if self._stop.is_set():
                    break

                if retry_attempt or self._retry_pending():
                    self._consume_retry()
                    self._close_host(host)
                    try:
                        host = self._new_host()
                    except Exception as exc:
                        self._report_host_start_failure(exc)
                        break
                    continue

                if connection_failed and self._control_event.wait(
                    self.RECONNECT_BACKOFF_SECONDS
                ):
                    if self._stop.is_set():
                        break
                    if self._retry_pending():
                        self._consume_retry()
                        self._close_host(host)
                        try:
                            host = self._new_host()
                        except Exception as exc:
                            self._report_host_start_failure(exc)
                            break
        finally:
            self.session_ready.clear()
            self.transport_open.clear()
            self._close_connection()
            self._close_host(host)
            self.finished.set()
