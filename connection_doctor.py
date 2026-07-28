"""Connection Doctor: structured diagnosis for the AOA connection flow.

The doctor models the connection as explicit states and records bounded,
thread-safe diagnostic events with stable codes. User-facing text comes only
from the static code catalog below; raw exceptions stay in the ``native_error``
field for the technical appendix of the report. Everything here is local and
offline: no telemetry, analytics, or network behavior, and reports are
redacted (no USB serials, user names, computer names, full paths, IP
addresses, touch coordinates, or keystrokes).
"""

from __future__ import annotations

import ctypes
import os
import platform
import re
import threading
import time
from collections import deque
from ctypes import wintypes
from dataclasses import dataclass
from enum import Enum
from typing import Callable, Optional


WINDOWS_APP_VERSION = "0.1.3"
TOUCH_PROTOCOL_VERSION = 1
DEFAULT_HISTORY_CAPACITY = 256
DEFAULT_DEDUP_WINDOW_SECONDS = 5.0


class ConnectionState(str, Enum):
    IDLE = "idle"
    DISCOVERY = "device-discovery"
    AOA_CAPABILITY_CHECK = "aoa-capability-check"
    AOA_NEGOTIATION = "aoa-negotiation"
    REENUMERATION = "usb-reenumeration"
    WINUSB_OPEN = "winusb-open"
    USBDK_FALLBACK = "usbdk-fallback"
    APP_HANDSHAKE = "android-app-handshake"
    CONNECTED_STREAM = "connected-stream"
    STALLED_STREAM = "stalled-stream"
    DISCONNECT = "disconnect"
    FAILURE = "failure"


class Severity(str, Enum):
    INFO = "info"
    WARNING = "warning"
    ERROR = "error"


# ---------------------------------------------------------------------------
# Stable public codes. Text lives here only; transport code picks codes and
# supplies structured detail parameters, never raw exception text.
# Catalog value: (summary, recommended user action, severity, transient).
# ---------------------------------------------------------------------------

HPT_DISC_NO_PHONE = "HPT-DISC-100"
HPT_DISC_SCAN_ERROR = "HPT-DISC-101"
HPT_AOA_UNSUPPORTED = "HPT-AOA-200"
HPT_AOA_CAPABILITY_FAILED = "HPT-AOA-201"
HPT_AOA_NEGOTIATION_FAILED = "HPT-AOA-210"
HPT_AOA_NEGOTIATION_ACCEPTED = "HPT-AOA-220"
HPT_USB_REENUM_TIMEOUT = "HPT-USB-300"
HPT_USB_REENUMERATED = "HPT-USB-301"
HPT_USB_WINUSB_UNAVAILABLE = "HPT-USB-310"
HPT_USB_WINUSB_TRANSIENT = "HPT-USB-311"
HPT_USB_WINUSB_NOT_SUPPORTED = "HPT-USB-312"
HPT_USB_USBDK_FALLBACK_START = "HPT-USB-320"
HPT_USB_USBDK_FALLBACK_OK = "HPT-USB-321"
HPT_USB_USBDK_FALLBACK_FAILED = "HPT-USB-322"
HPT_USB_USBDK_UNAVAILABLE = "HPT-USB-323"
HPT_USB_BACKEND_SELECTED = "HPT-USB-330"
HPT_APP_HANDSHAKE_OK = "HPT-APP-400"
HPT_APP_NOT_RESPONDING = "HPT-APP-401"
HPT_APP_PROTOCOL_MISMATCH = "HPT-APP-410"
HPT_STREAM_CONNECTED = "HPT-STR-500"
HPT_STREAM_STALLED = "HPT-STR-501"
HPT_STREAM_RESUMED = "HPT-STR-502"
HPT_LINK_DISCONNECTED = "HPT-LINK-600"
HPT_LINK_DISCONNECT_ACTIVE_INPUT = "HPT-LINK-601"
HPT_LINK_RECONNECTING = "HPT-LINK-602"
HPT_INPUT_PRIVILEGE_MISMATCH = "HPT-INPUT-700"
HPT_SESSION_RETRY_REQUESTED = "HPT-SESS-800"
HPT_SYS_UNEXPECTED = "HPT-SYS-900"


@dataclass(frozen=True)
class _CatalogEntry:
    summary: str
    action: str
    severity: Severity
    transient: bool


_CATALOG: dict[str, _CatalogEntry] = {}


def _register(
    code: str, summary: str, action: str,
    severity: Severity, transient: bool,
) -> str:
    _CATALOG[code] = _CatalogEntry(summary, action, severity, transient)
    return code


_register(
    HPT_DISC_NO_PHONE,
    "No Android phone was found on USB.",
    "Connect the phone with a data-capable USB cable and unlock it.",
    Severity.ERROR, False,
)
_register(
    HPT_DISC_SCAN_ERROR,
    "USB device enumeration failed.",
    "Reconnect the cable or try a different USB port.",
    Severity.WARNING, True,
)
_register(
    HPT_AOA_UNSUPPORTED,
    "The connected device does not support Android Open Accessory.",
    "Use an AOA-capable Android phone, or run the legacy ADB transport "
    "(--transport adb).",
    Severity.ERROR, False,
)
_register(
    HPT_AOA_CAPABILITY_FAILED,
    "The phone did not answer the AOA capability check.",
    "Reconnect the cable and approve the USB accessory prompt on the phone.",
    Severity.WARNING, True,
)
_register(
    HPT_AOA_NEGOTIATION_FAILED,
    "The AOA handshake with the phone failed.",
    "Install the companion Android app, then reconnect the cable and accept "
    "its USB prompt.",
    Severity.ERROR, False,
)
_register(
    HPT_AOA_NEGOTIATION_ACCEPTED,
    "The phone accepted AOA mode and is re-enumerating.",
    "Wait for the accessory to reappear; approve the Android app prompt.",
    Severity.INFO, True,
)
_register(
    HPT_USB_REENUM_TIMEOUT,
    "The phone accepted AOA mode but did not reappear as an accessory.",
    "Reconnect the cable and accept the app prompt on the phone.",
    Severity.ERROR, False,
)
_register(
    HPT_USB_REENUMERATED,
    "The phone re-enumerated as an Android Accessory.",
    "",
    Severity.INFO, True,
)
_register(
    HPT_USB_WINUSB_UNAVAILABLE,
    "WinUSB is not bound to the AOA interface.",
    "Install the WinUSB data support from the Holodori Windows setup, or let "
    "the app use the UsbDk fallback.",
    Severity.WARNING, False,
)
_register(
    HPT_USB_WINUSB_TRANSIENT,
    "WinUSB could not open the accessory yet.",
    "Usually resolves during re-enumeration; no action needed unless it "
    "persists.",
    Severity.WARNING, True,
)
_register(
    HPT_USB_WINUSB_NOT_SUPPORTED,
    "The driver bound to the AOA device is not usable by the WinUSB path.",
    "Provisioning for WinUSB may be missing or bound to the wrong interface; "
    "the UsbDk fallback will be tried.",
    Severity.WARNING, False,
)
_register(
    HPT_USB_USBDK_FALLBACK_START,
    "Trying the UsbDk compatibility backend.",
    "",
    Severity.INFO, True,
)
_register(
    HPT_USB_USBDK_FALLBACK_OK,
    "The UsbDk backend took over the AOA data stream.",
    "",
    Severity.INFO, False,
)
_register(
    HPT_USB_USBDK_FALLBACK_FAILED,
    "The UsbDk backend could not open the accessory either.",
    "Reinstall UsbDk (or WinUSB support) from the Holodori Windows setup and "
    "restart Windows, then reconnect the phone.",
    Severity.ERROR, False,
)
_register(
    HPT_USB_USBDK_UNAVAILABLE,
    "UsbDk is not active on this PC.",
    "Install UsbDk from the Holodori Windows setup and restart Windows once.",
    Severity.WARNING, False,
)
_register(
    HPT_USB_BACKEND_SELECTED,
    "A USB backend is ready for the touch stream.",
    "",
    Severity.INFO, False,
)
_register(
    HPT_APP_HANDSHAKE_OK,
    "The Android app is streaming touch data.",
    "",
    Severity.INFO, False,
)
_register(
    HPT_APP_NOT_RESPONDING,
    "USB is open but the Android app sent no data.",
    "Open the Holodori Trackpad app on the phone and accept its USB prompt.",
    Severity.ERROR, True,
)
_register(
    HPT_APP_PROTOCOL_MISMATCH,
    "The phone and PC speak incompatible touch protocol versions.",
    "Update both the Windows app and the Android app to the latest release.",
    Severity.ERROR, False,
)
_register(
    HPT_STREAM_CONNECTED,
    "The touch stream is live.",
    "",
    Severity.INFO, False,
)
_register(
    HPT_STREAM_STALLED,
    "Touch packets stopped arriving mid-session.",
    "Check the cable and the phone app; the PC reconnects automatically.",
    Severity.ERROR, True,
)
_register(
    HPT_STREAM_RESUMED,
    "The touch stream recovered after an interruption.",
    "",
    Severity.INFO, True,
)
_register(
    HPT_LINK_DISCONNECTED,
    "The USB link to the phone was lost.",
    "Reconnect the cable; the PC keeps retrying automatically.",
    Severity.ERROR, True,
)
_register(
    HPT_LINK_DISCONNECT_ACTIVE_INPUT,
    "The link dropped while keys were held; every key was released.",
    "Reconnect the cable; no key stays stuck.",
    Severity.ERROR, True,
)
_register(
    HPT_LINK_RECONNECTING,
    "Attempting to restore the connection.",
    "",
    Severity.INFO, True,
)
_register(
    HPT_INPUT_PRIVILEGE_MISMATCH,
    "The focused app appears elevated but the trackpad is not.",
    "Close and relaunch Holodori Phone Trackpad as Administrator so key "
    "events reach the game.",
    Severity.WARNING, False,
)
_register(
    HPT_SESSION_RETRY_REQUESTED,
    "A manual reconnect was requested.",
    "",
    Severity.INFO, True,
)
_register(
    HPT_SYS_UNEXPECTED,
    "An unexpected error occurred in the connection flow.",
    "Copy the diagnostic report and share it with the project issue tracker.",
    Severity.ERROR, True,
)


def catalog_entry(code: str) -> _CatalogEntry:
    return _CATALOG.get(code, _CATALOG[HPT_SYS_UNEXPECTED])


# Codes that imply a state transition when no explicit state is passed.
_CODE_IMPLIED_STATE: dict[str, ConnectionState] = {
    HPT_STREAM_STALLED: ConnectionState.STALLED_STREAM,
    HPT_LINK_DISCONNECTED: ConnectionState.DISCONNECT,
    HPT_LINK_DISCONNECT_ACTIVE_INPUT: ConnectionState.DISCONNECT,
    HPT_STREAM_CONNECTED: ConnectionState.CONNECTED_STREAM,
    HPT_STREAM_RESUMED: ConnectionState.CONNECTED_STREAM,
    HPT_APP_HANDSHAKE_OK: ConnectionState.CONNECTED_STREAM,
}


@dataclass
class DiagnosticEvent:
    code: str
    state: ConnectionState
    severity: Severity
    elapsed_s: float
    summary: str
    action: str
    detail: str = ""
    native_error: str = ""
    transient: bool = False
    repeats: int = 0


# ---------------------------------------------------------------------------
# Error classification: raw exceptions -> stable public codes.
# ---------------------------------------------------------------------------

_WINUSB_UNAVAILABLE_CODES = {
    2,      # ERROR_FILE_NOT_FOUND — no WinUSB-bound interface present
    1167,   # ERROR_DEVICE_NOT_CONNECTED
}
_WINUSB_TRANSIENT_CODES = {
    5,      # ERROR_ACCESS_DENIED — driver attach grace window
    31,     # ERROR_GEN_FAILURE
    121,    # ERROR_SEM_TIMEOUT
    1168,   # ERROR_NOT_FOUND
}
_ACTIVE_LINK_STATES = {
    ConnectionState.APP_HANDSHAKE,
    ConnectionState.CONNECTED_STREAM,
    ConnectionState.STALLED_STREAM,
    ConnectionState.DISCONNECT,
}
_USB_OPEN_STATES = {
    ConnectionState.REENUMERATION,
    ConnectionState.WINUSB_OPEN,
    ConnectionState.USBDK_FALLBACK,
}


def _native_error_text(exc: BaseException) -> str:
    parts = []
    current: Optional[BaseException] = exc
    while current is not None:
        name = getattr(current, "native_name", None)
        code = getattr(current, "native_code", None)
        if name or code is not None:
            parts.append(
                f"{type(current).__name__}({name or code})"
            )
        else:
            parts.append(type(current).__name__)
        current = current.__cause__ or current.__context__
    return " <- ".join(parts)


def classify_error(
    exc: BaseException,
    state: Optional[ConnectionState] = None,
) -> str:
    """Map an internal exception to a stable public code.

    Uses structured fields and the ``__cause__`` chain; never matches on
    user-visible message text. The connection state disambiguates native
    codes whose meaning changes between device discovery/open and an active
    stream.
    """
    current: Optional[BaseException] = exc
    while current is not None:
        diag_code = getattr(current, "diag_code", None)
        if diag_code:
            return diag_code
        native_name = getattr(current, "native_name", None)
        native_code = getattr(current, "native_code", None)
        if (
            state == ConnectionState.DISCOVERY
            and (native_name or native_code is not None)
        ):
            return HPT_DISC_SCAN_ERROR
        if (
            state in _ACTIVE_LINK_STATES
            and (native_name or native_code is not None)
        ):
            return HPT_LINK_DISCONNECTED
        if native_name:
            if native_name == "LIBUSB_ERROR_NOT_SUPPORTED":
                if state == ConnectionState.USBDK_FALLBACK:
                    return HPT_USB_USBDK_FALLBACK_FAILED
                return HPT_USB_WINUSB_NOT_SUPPORTED
            if native_name == "LIBUSB_ERROR_NO_DEVICE":
                return HPT_LINK_DISCONNECTED
            if native_name in ("LIBUSB_ERROR_IO", "LIBUSB_ERROR_PIPE"):
                if state in _USB_OPEN_STATES:
                    return HPT_USB_WINUSB_TRANSIENT
                return HPT_LINK_DISCONNECTED
            if native_name in (
                "LIBUSB_ERROR_ACCESS",
                "LIBUSB_ERROR_BUSY",
                "LIBUSB_ERROR_TIMEOUT",
            ):
                return HPT_USB_WINUSB_TRANSIENT
            return HPT_SYS_UNEXPECTED
        if native_code is not None:
            if native_code in _WINUSB_UNAVAILABLE_CODES:
                return HPT_USB_WINUSB_UNAVAILABLE
            if native_code in _WINUSB_TRANSIENT_CODES:
                return HPT_USB_WINUSB_TRANSIENT
            return HPT_USB_WINUSB_TRANSIENT
        current = current.__cause__ or current.__context__
    return HPT_SYS_UNEXPECTED


# ---------------------------------------------------------------------------
# Privacy redaction for rendered output.
# ---------------------------------------------------------------------------

_RE_DEVICE_PATH = re.compile(
    r"\\\\\?\\usb#vid_([0-9a-fA-F]{4})&pid_([0-9a-fA-F]{4})"
    r"(?:&mi_([0-9a-fA-F]{2}))?#[^#\s]+(?:#[^\s]*)?",
    re.IGNORECASE,
)
_RE_DEVICE_INSTANCE = re.compile(
    r"\bUSB[\\/]+VID_([0-9a-fA-F]{4})&PID_([0-9a-fA-F]{4})"
    r"(?:&MI_([0-9a-fA-F]{2}))?[\\/]+[^\s\"']+",
    re.IGNORECASE,
)
_RE_DRIVE_PATH = re.compile(r"\b[A-Za-z]:[\\/][^\r\n]*")
_RE_UNC_PATH = re.compile(r"\\\\[^\\\r\n\"']+\\[^\r\n]*")
_RE_POSIX_PATH = re.compile(
    r"(?<![:/\w])/(?:[^/\s\"']+/)+[^\r\n\"']*"
)
_RE_IPV4 = re.compile(r"\b\d{1,3}(?:\.\d{1,3}){3}\b")
_RE_IPV6 = re.compile(
    r"(?<![0-9a-fA-F:])"
    r"(?:[0-9a-fA-F]{0,4}:){2,7}[0-9a-fA-F]{0,4}"
    r"(?![0-9a-fA-F:])"
)


def _redact_device_path(match: re.Match) -> str:
    vid, pid, mi = match.groups()
    text = f"usb#vid_{vid.lower()}&pid_{pid.lower()}"
    if mi:
        text += f"&mi_{mi.lower()}"
    return f"{text}#<serial-redacted>"


def _redact_device_instance(match: re.Match) -> str:
    vid, pid, mi = match.groups()
    text = f"usb#vid_{vid.lower()}&pid_{pid.lower()}"
    if mi:
        text += f"&mi_{mi.lower()}"
    return f"{text}#<serial-redacted>"


def redact(text: str) -> str:
    """Strip identifiers that must never appear in a shared report."""
    if not text:
        return text
    text = _RE_DEVICE_PATH.sub(_redact_device_path, text)
    text = _RE_DEVICE_INSTANCE.sub(_redact_device_instance, text)
    text = _RE_DRIVE_PATH.sub("<path>", text)
    text = _RE_UNC_PATH.sub("<path>", text)
    text = _RE_POSIX_PATH.sub("<path>", text)
    text = _RE_IPV4.sub("<ip>", text)
    text = _RE_IPV6.sub("<ip>", text)
    for var in ("USERNAME", "COMPUTERNAME"):
        value = os.environ.get(var)
        if value and len(value) >= 3:
            text = re.sub(
                re.escape(value), "<redacted>", text,
                flags=re.IGNORECASE,
            )
    return text


# ---------------------------------------------------------------------------
# Snapshot
# ---------------------------------------------------------------------------

@dataclass(frozen=True)
class DoctorSnapshot:
    state: ConnectionState
    state_elapsed_s: float
    backend: str
    winusb_status: str
    usbdk_status: str
    aoa_ids: str
    aoa_interface: Optional[int]
    windows_version: str
    android_app_version: str
    aoa_protocol_version: Optional[int]
    touch_protocol_version: Optional[int]
    os_version: str
    touch_packets: int
    heartbeats: int
    packets_flowing: bool
    reconnect_attempts: int
    last_code: str
    last_summary: str
    last_action: str
    revision: int


# ---------------------------------------------------------------------------
# The doctor
# ---------------------------------------------------------------------------

class ConnectionDoctor:
    """Bounded, thread-safe diagnostic event recorder.

    Emission is a lock + deque append plus cheap field stores, so it stays off
    the latency-critical path: states change a handful of times per session
    and repeated polling events are merged instead of logged.
    """

    def __init__(
        self,
        capacity: int = DEFAULT_HISTORY_CAPACITY,
        dedup_window_s: float = DEFAULT_DEDUP_WINDOW_SECONDS,
        now: Callable[[], float] = time.monotonic,
    ) -> None:
        self._now = now
        self._start = now()
        self._dedup_window_s = dedup_window_s
        self._events: deque[DiagnosticEvent] = deque(maxlen=capacity)
        self._recent: dict[tuple[str, ConnectionState], DiagnosticEvent] = {}
        self._listeners: list[Callable[[DiagnosticEvent], None]] = []
        self._lock = threading.Lock()
        self._state = ConnectionState.IDLE
        self._state_since = self._start
        self._backend = "unknown"
        self._winusb_status = "unknown"
        self._usbdk_status = "unknown"
        self._aoa_ids = "unknown"
        self._aoa_interface: Optional[int] = None
        self._aoa_protocol_version: Optional[int] = None
        self._touch_protocol_version: Optional[int] = None
        self._android_app_version = "not reported (touch protocol v1)"
        self._touch_packets = 0
        self._heartbeats = 0
        self._last_packet_at: Optional[float] = None
        self._reconnect_attempts = 0
        self._last_code = ""
        self._last_summary = ""
        self._last_action = ""
        self._revision = 0
        self._privilege_checked = False

    # -- subscription ------------------------------------------------------

    def subscribe(
        self, listener: Callable[[DiagnosticEvent], None]
    ) -> None:
        with self._lock:
            self._listeners.append(listener)

    # -- emission ----------------------------------------------------------

    def transition(self, state: ConnectionState) -> None:
        with self._lock:
            if self._state != state:
                self._state = state
                self._state_since = self._now()
                self._revision += 1

    def emit(
        self,
        code: str,
        state: Optional[ConnectionState] = None,
        detail: str = "",
        exc: Optional[BaseException] = None,
        severity: Optional[Severity] = None,
        transient: Optional[bool] = None,
    ) -> DiagnosticEvent:
        entry = catalog_entry(code)
        now = self._now()
        merged: Optional[DiagnosticEvent] = None
        notify: Optional[DiagnosticEvent] = None
        with self._lock:
            if state is None:
                state = _CODE_IMPLIED_STATE.get(code, self._state)
            if state != self._state:
                self._state = state
                self._state_since = now
            # Rate limit: merge repeats of the same code+state arriving
            # inside the dedup window (device polling, reconnect retries).
            key = (code, state)
            elapsed = now - self._start
            previous = self._recent.get(key)
            if (
                previous is not None
                and elapsed - previous.elapsed_s < self._dedup_window_s
            ):
                previous.repeats += 1
                previous.elapsed_s = elapsed
                self._revision += 1
                merged = previous
                # Backoff: notify again only at powers-of-two repeat counts.
                repeat_count = merged.repeats + 1
                if repeat_count & (repeat_count - 1) == 0:
                    notify = merged
            event = merged
            if merged is None:
                event = DiagnosticEvent(
                    code=code,
                    state=state,
                    severity=severity or entry.severity,
                    elapsed_s=now - self._start,
                    summary=entry.summary,
                    action=entry.action,
                    detail=detail,
                    native_error=(
                        _native_error_text(exc) if exc else ""
                    ),
                    transient=(
                        entry.transient if transient is None else transient
                    ),
                )
                self._events.append(event)
                self._recent[key] = event
                if len(self._recent) > 64:
                    cutoff = elapsed - self._dedup_window_s
                    stale = [
                        k
                        for k, v in self._recent.items()
                        if v.elapsed_s < cutoff
                    ]
                    for k in stale:
                        del self._recent[k]
                if entry.severity in (Severity.WARNING, Severity.ERROR):
                    self._last_code = code
                    self._last_summary = entry.summary
                    self._last_action = entry.action
                self._revision += 1
                notify = event
        # Listeners run outside the lock so they may query the doctor.
        if notify is not None:
            self._notify(notify)
        return event

    def fail(
        self,
        exc: BaseException,
        state: Optional[ConnectionState] = None,
        detail: str = "",
        code: Optional[str] = None,
    ) -> DiagnosticEvent:
        """Emit an event for a raw exception, mapped to a stable code."""
        with self._lock:
            context_state = self._state
        return self.emit(
            code or classify_error(exc, state=context_state),
            state=state,
            detail=detail,
            exc=exc,
        )

    def _notify(self, event: DiagnosticEvent) -> None:
        listeners = list(self._listeners)
        for listener in listeners:
            try:
                listener(event)
            except Exception:
                # Diagnostics must never disturb the transport.
                pass

    # -- status fields (cheap stores; no events on the packet path) --------

    def set_state(self, state: ConnectionState) -> None:
        self.transition(state)

    def note_backend(self, backend: str) -> None:
        with self._lock:
            self._backend = backend
            self._revision += 1

    def note_winusb_status(self, status: str) -> None:
        with self._lock:
            self._winusb_status = status
            self._revision += 1

    def note_usbdk_status(self, status: str) -> None:
        with self._lock:
            self._usbdk_status = status
            self._revision += 1

    def note_accessory(
        self,
        vid: Optional[int],
        pid: Optional[int],
        interface: Optional[int] = None,
    ) -> None:
        with self._lock:
            if vid is not None and pid is not None:
                self._aoa_ids = f"{vid:04X}:{pid:04X}"
            elif vid is not None:
                self._aoa_ids = f"{vid:04X}:unknown"
            if interface is not None:
                self._aoa_interface = interface
            self._revision += 1

    def note_aoa_protocol_version(self, version: int) -> None:
        with self._lock:
            self._aoa_protocol_version = version
            self._revision += 1

    def note_touch_protocol_version(self, version: int) -> None:
        with self._lock:
            self._touch_protocol_version = version
            self._revision += 1

    def note_reconnect_attempt(self) -> None:
        with self._lock:
            self._reconnect_attempts += 1
            self._revision += 1

    def note_packet(
        self,
        heartbeat: bool,
        observed_at: Optional[float] = None,
    ) -> None:
        # Hot path: bounded lock-free bookkeeping only. The receiver passes
        # its existing monotonic sample to avoid an extra clock syscall.
        # Plain int/float stores are atomic under the GIL; diagnostics
        # tolerate a raced read and never block the touch pipeline.
        if heartbeat:
            self._heartbeats += 1
        else:
            self._touch_packets += 1
        self._last_packet_at = (
            self._now() if observed_at is None else observed_at
        )

    # -- queries -----------------------------------------------------------

    def events(self) -> list[DiagnosticEvent]:
        with self._lock:
            return list(self._events)

    def snapshot(self) -> DoctorSnapshot:
        with self._lock:
            now = self._now()
            last_packet_at = self._last_packet_at
            return DoctorSnapshot(
                state=self._state,
                state_elapsed_s=now - self._state_since,
                backend=self._backend,
                winusb_status=self._winusb_status,
                usbdk_status=self._usbdk_status,
                aoa_ids=self._aoa_ids,
                aoa_interface=self._aoa_interface,
                windows_version=WINDOWS_APP_VERSION,
                android_app_version=self._android_app_version,
                aoa_protocol_version=self._aoa_protocol_version,
                touch_protocol_version=self._touch_protocol_version,
                os_version=(
                    f"{platform.system()} {platform.release()}"
                ),
                touch_packets=self._touch_packets,
                heartbeats=self._heartbeats,
                packets_flowing=bool(
                    last_packet_at is not None
                    and now - last_packet_at < 1.5
                ),
                reconnect_attempts=self._reconnect_attempts,
                last_code=self._last_code,
                last_summary=self._last_summary,
                last_action=self._last_action,
                revision=self._revision,
            )

    # -- one-shot helpers --------------------------------------------------

    def check_input_privilege(self) -> None:
        """Warn once if the focused window is elevated and we are not."""
        with self._lock:
            if self._privilege_checked:
                return
            self._privilege_checked = True
        try:
            mismatch = _foreground_is_elevated_and_we_are_not()
        except Exception:
            mismatch = False
        if mismatch:
            self.emit(
                HPT_INPUT_PRIVILEGE_MISMATCH,
                state=ConnectionState.CONNECTED_STREAM,
            )

    # -- rendering ---------------------------------------------------------

    def render_view(self) -> str:
        snap = self.snapshot()
        interface = (
            str(snap.aoa_interface)
            if snap.aoa_interface is not None
            else "unknown"
        )
        stream = (
            f"yes ({snap.touch_packets} touch packets)"
            if snap.packets_flowing
            else "no"
        )
        lines = [
            "Connection Doctor",
            f"  stage: {snap.state.value} "
            f"({snap.state_elapsed_s:.1f}s in this stage)",
            f"  windows app: {snap.windows_version}   android app: "
            f"{snap.android_app_version}",
            f"  protocol: touch v"
            f"{snap.touch_protocol_version or 'unknown'}   AOA protocol: "
            f"{snap.aoa_protocol_version or 'unknown'}",
            f"  AOA device: {snap.aoa_ids}   interface: {interface}",
            f"  WinUSB: {snap.winusb_status}   UsbDk: {snap.usbdk_status}",
            f"  selected backend: {snap.backend}",
            f"  touch packets arriving: {stream}",
            f"  reconnect attempts: {snap.reconnect_attempts}",
        ]
        if snap.last_code:
            lines.append(f"  issue: {snap.last_code}: {snap.last_summary}")
            if snap.last_action:
                lines.append(f"  recommended action: {snap.last_action}")
        else:
            lines.append("  issue: none recorded")
        return "\n".join(lines)

    def render_report(self) -> str:
        events = self.events()
        lines = [
            "-----BEGIN HOLODORI DIAGNOSTIC REPORT-----",
            redact(self.render_view()),
            "",
            "events (most recent last; times relative to session start):",
        ]
        if not events:
            lines.append("  (no events recorded)")
        for event in events:
            repeats = (
                f" x{event.repeats + 1}" if event.repeats else ""
            )
            transient = "transient" if event.transient else "persistent"
            lines.append(
                f"  +{event.elapsed_s:8.3f}s [{event.state.value}] "
                f"{event.code} {event.severity.value}/{transient}: "
                f"{redact(event.summary)}{repeats}"
            )
            if event.detail:
                lines.append(f"      detail: {redact(event.detail)}")
            if event.action:
                lines.append(f"      action: {redact(event.action)}")
            if event.native_error:
                lines.append(
                    f"      native: {redact(event.native_error)}"
                )
        lines += [
            "",
            "privacy: contains no USB serial numbers, user or computer names,",
            "full paths, IP addresses, touch coordinates, or keystrokes.",
            "-----END HOLODORI DIAGNOSTIC REPORT-----",
        ]
        return "\n".join(lines)


# ---------------------------------------------------------------------------
# Input privilege check (Windows, best effort, no admin required to read).
# ---------------------------------------------------------------------------

def _process_is_elevated(process_handle: wintypes.HANDLE) -> Optional[bool]:
    kernel32 = ctypes.windll.kernel32
    advapi32 = ctypes.windll.advapi32
    kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel32.CloseHandle.restype = wintypes.BOOL
    advapi32.OpenProcessToken.argtypes = [
        wintypes.HANDLE,
        wintypes.DWORD,
        ctypes.POINTER(wintypes.HANDLE),
    ]
    advapi32.OpenProcessToken.restype = wintypes.BOOL
    advapi32.GetTokenInformation.argtypes = [
        wintypes.HANDLE,
        ctypes.c_int,
        ctypes.c_void_p,
        wintypes.DWORD,
        ctypes.POINTER(wintypes.DWORD),
    ]
    advapi32.GetTokenInformation.restype = wintypes.BOOL
    token = wintypes.HANDLE()
    TOKEN_QUERY = 0x0008
    if not advapi32.OpenProcessToken(
        process_handle, TOKEN_QUERY, ctypes.byref(token)
    ):
        return None
    try:
        elevation = wintypes.DWORD()
        length = wintypes.DWORD(ctypes.sizeof(elevation))
        returned = wintypes.DWORD(0)
        TokenElevation = 20
        if not advapi32.GetTokenInformation(
            token,
            TokenElevation,
            ctypes.byref(elevation),
            length,
            ctypes.byref(returned),
        ):
            return None
        return bool(elevation.value)
    finally:
        kernel32.CloseHandle(token)


def _foreground_is_elevated_and_we_are_not() -> bool:
    if os.name != "nt":
        return False
    user32 = ctypes.windll.user32
    kernel32 = ctypes.windll.kernel32
    user32.GetForegroundWindow.argtypes = []
    user32.GetForegroundWindow.restype = wintypes.HWND
    user32.GetWindowThreadProcessId.argtypes = [
        wintypes.HWND,
        ctypes.POINTER(wintypes.DWORD),
    ]
    user32.GetWindowThreadProcessId.restype = wintypes.DWORD
    kernel32.OpenProcess.argtypes = [
        wintypes.DWORD,
        wintypes.BOOL,
        wintypes.DWORD,
    ]
    kernel32.OpenProcess.restype = wintypes.HANDLE
    kernel32.GetCurrentProcess.argtypes = []
    kernel32.GetCurrentProcess.restype = wintypes.HANDLE
    foreground = user32.GetForegroundWindow()
    if not foreground:
        return False
    pid = wintypes.DWORD()
    user32.GetWindowThreadProcessId(foreground, ctypes.byref(pid))
    if not pid.value:
        return False
    PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
    remote = kernel32.OpenProcess(
        PROCESS_QUERY_LIMITED_INFORMATION, False, pid.value
    )
    if not remote:
        # We cannot even query the other process; treat as elevated.
        remote_elevated: Optional[bool] = True
    else:
        try:
            remote_elevated = _process_is_elevated(remote)
        finally:
            kernel32.CloseHandle(remote)
    self_process = kernel32.GetCurrentProcess()
    self_elevated = _process_is_elevated(self_process)
    if remote_elevated is None or self_elevated is None:
        return False
    return bool(remote_elevated and not self_elevated)
