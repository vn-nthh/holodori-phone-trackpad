"""Connection Doctor tests: structured diagnosis of the AOA flow."""

import os
import threading
import time
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

from app_metadata import APP_VERSION
from aoa_mode import AoaTouchRouter, make_disconnect_handler
from aoa_transport import (
    ACTION_CANCEL,
    ACTION_DOWN,
    ACTION_HEARTBEAT,
    FLAG_HOST_RECOVERY,
    FLAG_INSIDE,
    FLAG_LOCKED,
    FLAG_QUEUE_DIAGNOSTICS,
    FLAG_QUEUE_FAILSAFE,
    FLAG_QUEUE_RESYNC,
    FLAG_QUEUE_WARNING,
    FLAG_SESSION_RESET,
    HOST_ATTACH_REQUEST,
    AOA_GET_PROTOCOL,
    AOA_SEND_IDENT,
    AoaError,
    AoaHost,
    AoaReceiver,
    TOUCH_MAGIC,
    TOUCH_PACKET,
)
import connection_doctor as dc
from connection_doctor import (
    ConnectionDoctor,
    ConnectionState,
    Severity,
    redact,
)


def packet(action, pointer_id=0, flags=0, sequence=0, version=None,
           x=0, y=0, event_nanos=0):
    if version is None:
        version = dc.TOUCH_PROTOCOL_VERSION
    return TOUCH_PACKET.pack(
        TOUCH_MAGIC,
        version,
        action,
        pointer_id,
        flags,
        x,
        y,
        sequence,
        event_nanos,
    )


def session_reset(sequence=0, recovered=False):
    return packet(
        ACTION_CANCEL,
        flags=(
            FLAG_SESSION_RESET
            | (FLAG_HOST_RECOVERY if recovered else 0)
        ),
        sequence=sequence,
    )


def codes(doctor):
    return [event.code for event in doctor.events()]


def wait_until(predicate, timeout=1.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.005)
    return bool(predicate())


def host_skeleton(doctor, using_usbdk=False, prefer_usbdk=True):
    host = object.__new__(AoaHost)
    host.prefer_usbdk = prefer_usbdk
    host.usb = SimpleNamespace(using_usbdk=using_usbdk)
    host.doctor = doctor
    host._usbdk_data_fallback_attempted = False
    host._usbdk_data_fallback_pending = False
    host.vendor_ids = {0x04E8, 0x18D1}
    return host


class ClassificationTests(unittest.TestCase):
    def test_native_error_mapping(self):
        not_supported = AoaError(
            "Could not open the Android USB device",
            operation="open the Android USB device",
            native_name="LIBUSB_ERROR_NOT_SUPPORTED",
            native_code=-12,
        )
        self.assertEqual(
            dc.classify_error(not_supported),
            dc.HPT_USB_WINUSB_NOT_SUPPORTED,
        )
        for name in (
            "LIBUSB_ERROR_NO_DEVICE",
            "LIBUSB_ERROR_IO",
            "LIBUSB_ERROR_PIPE",
        ):
            self.assertEqual(
                dc.classify_error(AoaError("x", native_name=name)),
                dc.HPT_LINK_DISCONNECTED,
            )
        for name in (
            "LIBUSB_ERROR_ACCESS",
            "LIBUSB_ERROR_BUSY",
            "LIBUSB_ERROR_TIMEOUT",
        ):
            self.assertEqual(
                dc.classify_error(AoaError("x", native_name=name)),
                dc.HPT_USB_WINUSB_TRANSIENT,
            )
        self.assertEqual(
            dc.classify_error(AoaError("x")),
            dc.HPT_SYS_UNEXPECTED,
        )

    def test_native_errors_are_classified_by_connection_stage(self):
        from winusb_transport import WinUsbError

        io_error = AoaError("x", native_name="LIBUSB_ERROR_IO")
        self.assertEqual(
            dc.classify_error(
                io_error, state=ConnectionState.WINUSB_OPEN
            ),
            dc.HPT_USB_WINUSB_TRANSIENT,
        )
        self.assertEqual(
            dc.classify_error(
                io_error, state=ConnectionState.CONNECTED_STREAM
            ),
            dc.HPT_LINK_DISCONNECTED,
        )
        disconnected = WinUsbError("x", native_code=1167)
        self.assertEqual(
            dc.classify_error(
                disconnected, state=ConnectionState.CONNECTED_STREAM
            ),
            dc.HPT_LINK_DISCONNECTED,
        )
        self.assertEqual(
            dc.classify_error(
                disconnected, state=ConnectionState.WINUSB_OPEN
            ),
            dc.HPT_USB_WINUSB_UNAVAILABLE,
        )
        self.assertEqual(
            dc.classify_error(
                AoaError("x", native_name="LIBUSB_ERROR_IO"),
                state=ConnectionState.DISCOVERY,
            ),
            dc.HPT_DISC_SCAN_ERROR,
        )

    def test_winusb_error_codes_map_through_cause(self):
        from winusb_transport import WinUsbError

        wrapped = AoaError("wrapped")
        wrapped.__cause__ = WinUsbError(
            "Could not open the AOA WinUSB interface (2)", native_code=2
        )
        try:
            raise wrapped
        except AoaError as error:
            self.assertEqual(
                dc.classify_error(error), dc.HPT_USB_WINUSB_UNAVAILABLE
            )
        try:
            raise AoaError("wrapped") from WinUsbError(
                "transient (5)", native_code=5
            )
        except AoaError as error:
            self.assertEqual(
                dc.classify_error(error), dc.HPT_USB_WINUSB_TRANSIENT
            )

    def test_explicit_diag_code_wins(self):
        error = AoaError(
            "AOA heartbeat timed out; reconnecting",
            diag_code=dc.HPT_APP_NOT_RESPONDING,
        )
        self.assertEqual(
            dc.classify_error(error), dc.HPT_APP_NOT_RESPONDING
        )


class DoctorCoreTests(unittest.TestCase):
    def test_bounded_history_keeps_newest(self):
        doctor = ConnectionDoctor(
            capacity=10, dedup_window_s=0, now=lambda: 0.0
        )
        for index in range(50):
            doctor.emit(
                dc.HPT_STREAM_STALLED,
                state=ConnectionState.STALLED_STREAM,
                detail=f"stall {index}",
            )
        events = doctor.events()
        self.assertEqual(len(events), 10)
        self.assertEqual(events[-1].detail, "stall 49")
        self.assertEqual(events[0].detail, "stall 40")

    def test_event_dedup_merges_repeats_inside_window(self):
        clock = [0.0]
        doctor = ConnectionDoctor(now=lambda: clock[0], dedup_window_s=5.0)
        first = doctor.emit(dc.HPT_DISC_NO_PHONE)
        merged = doctor.emit(dc.HPT_DISC_NO_PHONE)
        self.assertIs(first, merged)
        self.assertEqual(first.repeats, 1)
        clock[0] = 10.0
        doctor.emit(dc.HPT_DISC_NO_PHONE)
        self.assertEqual(len(doctor.events()), 2)

    def test_redaction_strips_private_fragments(self):
        with mock.patch.dict(
            os.environ,
            {"USERNAME": "AliceUser", "COMPUTERNAME": "DevBox"},
        ):
            text = redact(
                r"open \\?\usb#vid_18d1&pid_2d01&mi_00#"
                "7&2f3a4b5c&0#{guid} "
                "from C:\\Users\\bob\\Desktop to 10.0.0.8\n"
                "C:/Users/Alice User/Desktop/private.log\n"
                "/home/alice/private/report.log\n"
                "2001:db8::1\nALICEUSER\ndevbox\n"
                r"USB\VID_18D1&PID_2D01\SERIAL123"
            )
        for private in (
            "2f3a4b5c",
            "bob",
            "C:\\",
            "10.0.0.8",
            "Alice User",
            "/home/alice",
            "2001:db8::1",
            "ALICEUSER",
            "devbox",
            "SERIAL123",
        ):
            self.assertNotIn(private, text)
        self.assertIn("vid_18d1&pid_2d01", text)

    def test_report_contains_no_private_fragments(self):
        import os

        username = os.environ.get("USERNAME", "")
        doctor = ConnectionDoctor(now=lambda: 0.0)
        doctor.emit(
            dc.HPT_USB_WINUSB_TRANSIENT,
            detail=(
                r"device \\?\usb#vid_18d1&pid_2d01#K9X7SERIAL#{810c} at "
                r"C:\Users\somewhere\logs via 192.168.1.20 "
                f"for {username}"
            ),
            exc=AoaError("x", native_name="LIBUSB_ERROR_ACCESS"),
        )
        report = doctor.render_report()
        self.assertNotIn("K9X7SERIAL", report)
        self.assertNotIn("C:\\Users\\somewhere", report)
        self.assertNotIn("192.168.1.20", report)
        if len(username) >= 3:
            self.assertNotIn(username, report)
        self.assertNotIn("vid_18d1&pid_2d01", report)
        self.assertNotIn("native:", report)
        self.assertNotIn("LIBUSB_ERROR_ACCESS", report)
        self.assertNotIn("detail:", report)
        self.assertTrue(
            report.startswith("-----BEGIN HOLODORI DIAGNOSTIC REPORT-----")
        )
        self.assertTrue(
            report.strip().endswith(
                "-----END HOLODORI DIAGNOSTIC REPORT-----"
            )
        )

    def test_report_uses_allowlisted_protocol_metadata_only(self):
        doctor = ConnectionDoctor(now=lambda: 0.0)
        doctor.emit(
            dc.HPT_APP_PROTOCOL_MISMATCH,
            state=ConnectionState.FAILURE,
            detail=r"private developer detail C:\Users\alice\trace.log",
            exc=AoaError("raw failure", native_name="LIBUSB_ERROR_IO"),
            metadata={
                "received_touch_protocol_version": 7,
                "expected_touch_protocol_version": 1,
                "untrusted_field": "must not be copied",
            },
        )

        report = doctor.render_report()

        self.assertIn("received touch protocol: 7", report)
        self.assertIn("expected touch protocol: 1", report)
        self.assertNotIn("private developer detail", report)
        self.assertNotIn("raw failure", report)
        self.assertNotIn("LIBUSB_ERROR_IO", report)
        self.assertNotIn("untrusted_field", report)
        self.assertNotIn("must not be copied", report)

    def test_runtime_version_comes_from_authoritative_version_file(self):
        project_root = Path(__file__).resolve().parents[1]
        version_file = project_root / "VERSION"
        self.assertEqual(APP_VERSION, version_file.read_text().strip())
        self.assertEqual(
            ConnectionDoctor(now=lambda: 0.0).snapshot().windows_version,
            APP_VERSION,
        )

        android_build = (
            project_root / "android-app" / "app" / "build.gradle"
        ).read_text()
        installer = (
            project_root
            / "packaging"
            / "windows"
            / "HolodoriPhoneTrackpad.iss"
        ).read_text()
        pyinstaller_spec = (
            project_root
            / "packaging"
            / "windows"
            / "HolodoriPhoneTrackpad.spec"
        ).read_text()
        self.assertIn('rootProject.file("../VERSION")', android_build)
        self.assertNotIn(f'versionName "{APP_VERSION}"', android_build)
        self.assertIn(r'..\..\VERSION', installer)
        self.assertNotIn(
            f'#define MyAppVersion "{APP_VERSION}"', installer
        )
        self.assertIn('project_root / "VERSION"', pyinstaller_spec)

    def test_snapshot_reports_status_fields(self):
        doctor = ConnectionDoctor(now=lambda: 100.0)
        doctor.transition(ConnectionState.CONNECTED_STREAM)
        doctor.note_backend("UsbDk")
        doctor.note_winusb_status("failed: HPT-USB-312")
        doctor.note_usbdk_status("active (fallback)")
        doctor.note_accessory(0x18D1, 0x2D01, 0)
        doctor.note_aoa_protocol_version(2)
        doctor.note_touch_protocol_version(1)
        doctor.note_packet(heartbeat=False)
        snapshot = doctor.snapshot()
        self.assertEqual(snapshot.state, ConnectionState.CONNECTED_STREAM)
        self.assertEqual(snapshot.backend, "UsbDk")
        self.assertEqual(snapshot.aoa_ids, "18D1:2D01")
        self.assertEqual(snapshot.aoa_interface, 0)
        self.assertEqual(snapshot.aoa_protocol_version, 2)
        self.assertTrue(snapshot.packets_flowing)
        self.assertTrue(snapshot.os_name)
        if snapshot.os_name == "Windows":
            self.assertRegex(snapshot.os_release, r"^\d+")
            self.assertRegex(snapshot.os_build, r"^\d+(?:\.\d+)?$")
        for private_name in (
            os.environ.get("USERNAME", ""),
            os.environ.get("COMPUTERNAME", ""),
        ):
            if len(private_name) >= 3:
                self.assertNotIn(private_name, snapshot.os_version)

    def test_privilege_check_emits_once(self):
        doctor = ConnectionDoctor(now=lambda: 0.0)
        with mock.patch(
            "connection_doctor._foreground_is_elevated_and_we_are_not",
            return_value=True,
        ):
            doctor.check_input_privilege()
            doctor.check_input_privilege()
        events = [
            e
            for e in doctor.events()
            if e.code == dc.HPT_INPUT_PRIVILEGE_MISMATCH
        ]
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0].severity, Severity.WARNING)


class DoctorNotificationTests(unittest.TestCase):
    def test_blocked_subscriber_does_not_delay_emit(self):
        doctor = ConnectionDoctor(notification_capacity=4)
        callback_started = threading.Event()
        release_callback = threading.Event()
        callback_threads = []

        def blocked_listener(_event):
            callback_threads.append(threading.get_ident())
            callback_started.set()
            release_callback.wait(1.0)

        doctor.subscribe(blocked_listener)
        emitter_thread = threading.get_ident()
        started = time.monotonic()
        doctor.emit(dc.HPT_SESSION_RETRY_REQUESTED)
        first_elapsed = time.monotonic() - started
        self.assertTrue(callback_started.wait(0.5))

        started = time.monotonic()
        doctor.emit(
            dc.HPT_USB_REENUMERATED,
            state=ConnectionState.IDLE,
        )
        second_elapsed = time.monotonic() - started

        self.assertEqual(len(doctor.events()), 2)
        self.assertLess(first_elapsed, 0.1)
        self.assertLess(second_elapsed, 0.1)
        self.assertNotEqual(callback_threads[0], emitter_thread)
        release_callback.set()
        self.assertTrue(doctor.close(timeout=1.0))

    def test_callback_exceptions_are_isolated(self):
        doctor = ConnectionDoctor(dedup_window_s=0)
        delivered = []
        complete = threading.Event()

        def broken(_event):
            raise RuntimeError("subscriber failed")

        def healthy(event):
            delivered.append(event.code)
            if len(delivered) == 2:
                complete.set()

        doctor.subscribe(broken)
        doctor.subscribe(healthy)
        doctor.emit(dc.HPT_SESSION_RETRY_REQUESTED)
        doctor.emit(
            dc.HPT_USB_REENUMERATED,
            state=ConnectionState.IDLE,
        )

        self.assertTrue(complete.wait(0.5))
        self.assertEqual(
            delivered,
            [
                dc.HPT_SESSION_RETRY_REQUESTED,
                dc.HPT_USB_REENUMERATED,
            ],
        )
        self.assertTrue(doctor.close(timeout=1.0))

    def test_queue_overflow_preserves_failure_and_retained_order(self):
        doctor = ConnectionDoctor(
            dedup_window_s=0,
            notification_capacity=4,
        )
        callback_started = threading.Event()
        release_callback = threading.Event()
        delivered = []

        def listener(event):
            delivered.append(event.code)
            if event.code == dc.HPT_SESSION_RETRY_REQUESTED:
                callback_started.set()
                release_callback.wait(1.0)

        doctor.subscribe(listener)
        doctor.emit(
            dc.HPT_SESSION_RETRY_REQUESTED,
            state=ConnectionState.IDLE,
        )
        self.assertTrue(callback_started.wait(0.5))
        doctor.emit(
            dc.HPT_AOA_NEGOTIATION_ACCEPTED,
            state=ConnectionState.IDLE,
        )
        doctor.emit(
            dc.HPT_USB_REENUMERATED,
            state=ConnectionState.IDLE,
        )
        doctor.emit(
            dc.HPT_USB_USBDK_FALLBACK_START,
            state=ConnectionState.IDLE,
        )
        doctor.emit(
            dc.HPT_USB_USBDK_FALLBACK_OK,
            state=ConnectionState.IDLE,
        )
        doctor.emit(
            dc.HPT_APP_PROTOCOL_MISMATCH,
            state=ConnectionState.IDLE,
        )
        doctor.emit(
            dc.HPT_STREAM_RESUMED,
            state=ConnectionState.TOUCH_STREAM_ACTIVE,
        )

        release_callback.set()
        self.assertTrue(doctor.close(timeout=1.0))
        self.assertEqual(
            delivered,
            [
                dc.HPT_SESSION_RETRY_REQUESTED,
                dc.HPT_USB_USBDK_FALLBACK_START,
                dc.HPT_USB_USBDK_FALLBACK_OK,
                dc.HPT_APP_PROTOCOL_MISMATCH,
                dc.HPT_STREAM_RESUMED,
            ],
        )
        self.assertEqual(doctor.notification_drops(), 2)

    def test_shutdown_is_bounded_idempotent_and_stops_delivery(self):
        doctor = ConnectionDoctor(notification_capacity=2)
        callback_started = threading.Event()
        release_callback = threading.Event()
        delivered = []

        def listener(event):
            delivered.append(event.code)
            callback_started.set()
            release_callback.wait(1.0)

        doctor.subscribe(listener)
        doctor.emit(dc.HPT_SESSION_RETRY_REQUESTED)
        self.assertTrue(callback_started.wait(0.5))

        started = time.monotonic()
        self.assertFalse(
            doctor.close(drain=False, timeout=0.02)
        )
        self.assertLess(time.monotonic() - started, 0.2)
        doctor.emit(dc.HPT_APP_PROTOCOL_MISMATCH)
        self.assertIn(dc.HPT_APP_PROTOCOL_MISMATCH, codes(doctor))

        release_callback.set()
        self.assertTrue(doctor.close(timeout=1.0))
        self.assertTrue(doctor.close(timeout=0.01))
        self.assertEqual(delivered, [dc.HPT_SESSION_RETRY_REQUESTED])


class FakeUsb:
    """Minimal libusb stand-in for handshake tests."""

    def __init__(
        self,
        descriptor,
        protocol_version=1,
        fail_control_out=False,
        using_usbdk=False,
    ):
        self.using_usbdk = using_usbdk
        self.lib = SimpleNamespace(libusb_close=lambda handle: None)
        self._descriptor = descriptor
        self._protocol_version = protocol_version
        self._fail_control_out = fail_control_out
        self.failed_out_requests = []

    def devices(self):
        return [("device-token", self._descriptor)]

    def open(self, device):
        return "handle-token"

    def control_in(self, handle, request, size, timeout_ms=1000):
        assert request == AOA_GET_PROTOCOL
        return self._protocol_version.to_bytes(2, "little")

    def control_out(self, handle, request, index=0, payload=b"", **kwargs):
        if self._fail_control_out and request == AOA_SEND_IDENT:
            raise AoaError(
                "Could not send AOA request 52: LIBUSB_ERROR_IO",
                operation="send AOA request 52",
                native_name="LIBUSB_ERROR_IO",
                native_code=-1,
            )

    def unref_device(self, device):
        pass


class HandshakeDiagnosticTests(unittest.TestCase):
    def test_no_phone_detected(self):
        doctor = ConnectionDoctor(now=lambda: 0.0)
        host = host_skeleton(doctor, prefer_usbdk=False)
        host._find_data_accessory = mock.Mock(return_value=None)
        host._request_accessory_mode = mock.Mock(return_value=False)

        with self.assertRaises(AoaError):
            host.connect()

        events = doctor.events()
        self.assertIn(dc.HPT_DISC_NO_PHONE, codes(doctor))
        event = events[-1]
        self.assertEqual(event.state, ConnectionState.FAILURE)
        self.assertIn("data-capable USB cable", event.action)

    def test_aoa_unsupported_phone(self):
        doctor = ConnectionDoctor(now=lambda: 0.0)
        host = host_skeleton(doctor)
        host.usb = FakeUsb(
            SimpleNamespace(idVendor=0x04E8, idProduct=0x6860),
            protocol_version=0,
        )

        self.assertFalse(host._request_accessory_mode())

        self.assertEqual(host._last_handshake_outcome, "unsupported")
        self.assertIn(dc.HPT_AOA_UNSUPPORTED, codes(doctor))
        event = [
            e
            for e in doctor.events()
            if e.code == dc.HPT_AOA_UNSUPPORTED
        ][0]
        self.assertEqual(event.state, ConnectionState.AOA_CAPABILITY_CHECK)
        self.assertEqual(doctor.snapshot().aoa_protocol_version, 0)

    def test_aoa_negotiation_failure(self):
        doctor = ConnectionDoctor(now=lambda: 0.0)
        host = host_skeleton(doctor)
        host.usb = FakeUsb(
            SimpleNamespace(idVendor=0x04E8, idProduct=0x6860),
            protocol_version=2,
            fail_control_out=True,
        )

        self.assertFalse(host._request_accessory_mode())

        self.assertEqual(host._last_handshake_outcome, "negotiation-failed")
        self.assertIn(dc.HPT_AOA_NEGOTIATION_FAILED, codes(doctor))
        self.assertEqual(doctor.snapshot().aoa_protocol_version, 2)
        event = [
            e
            for e in doctor.events()
            if e.code == dc.HPT_AOA_NEGOTIATION_FAILED
        ][0]
        self.assertEqual(event.detail, "send AOA request 52")
        self.assertIn("LIBUSB_ERROR_IO", event.native_error)
        # User-facing text is catalog-driven, not raw exception text.
        self.assertEqual(event.summary, "The AOA handshake with the phone failed.")

    def test_capability_open_failure_stays_in_capability_stage(self):
        doctor = ConnectionDoctor(now=lambda: 0.0)
        host = host_skeleton(doctor)
        host.usb = FakeUsb(
            SimpleNamespace(idVendor=0x04E8, idProduct=0x6860),
            protocol_version=2,
        )
        host.usb.open = mock.Mock(
            side_effect=AoaError(
                "Could not open the Android USB device",
                operation="open the Android USB device",
                native_name="LIBUSB_ERROR_ACCESS",
            )
        )

        self.assertFalse(host._request_accessory_mode())

        self.assertEqual(host._last_handshake_outcome, "capability-failed")
        event = doctor.events()[-1]
        self.assertEqual(event.code, dc.HPT_AOA_CAPABILITY_FAILED)
        self.assertEqual(event.state, ConnectionState.AOA_CAPABILITY_CHECK)

    def test_reenumeration_timeout(self):
        doctor = ConnectionDoctor(now=lambda: 0.0)
        host = host_skeleton(doctor, prefer_usbdk=False)
        host._find_data_accessory = mock.Mock(return_value=None)
        host._replace_usb = mock.Mock()

        with (
            mock.patch(
                "aoa_transport.time.monotonic",
                side_effect=[0.0, 0.0, 0.0, 2.5, 2.5],
            ),
            mock.patch("aoa_transport.time.sleep"),
            self.assertRaises(AoaError) as raised,
        ):
            host._wait_for_data_accessory(2.0)

        self.assertIn("did not reappear", str(raised.exception))
        self.assertIn(dc.HPT_USB_REENUM_TIMEOUT, codes(doctor))
        event = [
            e
            for e in doctor.events()
            if e.code == dc.HPT_USB_REENUM_TIMEOUT
        ][0]
        self.assertEqual(event.state, ConnectionState.FAILURE)
        self.assertFalse(event.transient)

    def test_reenumeration_wait_is_interruptible(self):
        doctor = ConnectionDoctor()
        host = host_skeleton(doctor, prefer_usbdk=False)
        host._cancel_event = threading.Event()
        scan_started = threading.Event()

        def scan():
            scan_started.set()
            return None

        host._find_data_accessory = mock.Mock(side_effect=scan)

        def cancel():
            scan_started.wait(0.5)
            host._cancel_event.set()

        thread = threading.Thread(target=cancel, daemon=True)
        thread.start()
        started = time.monotonic()
        with self.assertRaises(RuntimeError):
            host._wait_for_data_accessory(5.0)

        self.assertLess(time.monotonic() - started, 0.5)
        self.assertNotIn(dc.HPT_USB_REENUM_TIMEOUT, codes(doctor))

    def test_both_backends_fail_reports_fallback_failure(self):
        doctor = ConnectionDoctor(now=lambda: 0.0)
        host = host_skeleton(doctor)

        def replace_usb(use_usbdk):
            host.usb = SimpleNamespace(using_usbdk=bool(use_usbdk))

        host._replace_usb = mock.Mock(side_effect=replace_usb)
        both_fail = [
            AoaError(
                "Could not open the Android USB device: "
                "LIBUSB_ERROR_NOT_SUPPORTED",
                operation="open the Android USB device",
                native_name="LIBUSB_ERROR_NOT_SUPPORTED",
                native_code=-12,
            ),
            AoaError(
                "Could not open the Android USB device: "
                "LIBUSB_ERROR_NOT_SUPPORTED",
                operation="open the Android USB device",
                native_name="LIBUSB_ERROR_NOT_SUPPORTED",
                native_code=-12,
            ),
        ]
        host._find_data_accessory = mock.Mock(side_effect=both_fail)

        with (
            mock.patch(
                "aoa_transport.time.monotonic",
                side_effect=[0.0, 0.0, 0.0, 0.0, 1.0, 5.0],
            ),
            mock.patch("aoa_transport.time.sleep"),
            self.assertRaises(AoaError),
        ):
            host._wait_for_data_accessory(2.0)

        seen = codes(doctor)
        self.assertIn(dc.HPT_USB_USBDK_FALLBACK_START, seen)
        self.assertNotIn(dc.HPT_USB_USBDK_FALLBACK_OK, seen)
        last = doctor.events()[-1]
        self.assertEqual(last.code, dc.HPT_USB_USBDK_FALLBACK_FAILED)
        self.assertEqual(last.state, ConnectionState.FAILURE)
        self.assertIn("restart windows", last.action.lower())
        self.assertEqual(
            doctor.snapshot().usbdk_status,
            "failed: " + dc.HPT_USB_USBDK_FALLBACK_FAILED,
        )


class ReproducedFailureTests(unittest.TestCase):
    """The observed failure:

        Could not open the Android USB device:
        LIBUSB_ERROR_NOT_SUPPORTED

    Re-enumeration succeeded; the WinUSB open path rejected the device; the
    UsbDk fallback carried the stream.
    """

    def test_not_supported_after_reenumeration_uses_usbdk(self):
        doctor = ConnectionDoctor(now=lambda: 0.0)
        connection = SimpleNamespace(
            interface_number=0,
            endpoint_in=0x81,
            endpoint_out=0x01,
            device_vid=0x18D1,
            device_pid=0x2D01,
        )
        host = host_skeleton(doctor)

        def replace_usb(use_usbdk):
            host.usb = SimpleNamespace(using_usbdk=bool(use_usbdk))

        host._replace_usb = mock.Mock(side_effect=replace_usb)
        host._find_data_accessory = mock.Mock(
            side_effect=[
                AoaError(
                    "Could not open the Android USB device: "
                    "LIBUSB_ERROR_NOT_SUPPORTED",
                    operation="open the Android USB device",
                    native_name="LIBUSB_ERROR_NOT_SUPPORTED",
                    native_code=-12,
                ),
                connection,
            ]
        )

        self.assertIs(host._wait_for_data_accessory(0.5), connection)

        seen = codes(doctor)
        self.assertEqual(
            [dc.HPT_USB_WINUSB_NOT_SUPPORTED,
             dc.HPT_USB_USBDK_FALLBACK_START,
             dc.HPT_USB_USBDK_FALLBACK_OK,
             dc.HPT_USB_REENUMERATED],
            seen,
        )
        winusb_event = doctor.events()[0]
        self.assertEqual(winusb_event.state, ConnectionState.WINUSB_OPEN)
        self.assertNotIn("LIBUSB_ERROR_NOT_SUPPORTED", winusb_event.summary)
        self.assertNotIn("LIBUSB_ERROR_NOT_SUPPORTED", winusb_event.action)
        self.assertIn("LIBUSB_ERROR_NOT_SUPPORTED", winusb_event.native_error)
        self.assertIn("fallback", winusb_event.action)
        snapshot = doctor.snapshot()
        self.assertEqual(snapshot.usbdk_status, "active")
        self.assertEqual(snapshot.winusb_status, "failed: HPT-USB-312")


class StreamingConnection:
    """Fake USB connection delivering scripted chunks then failing."""

    def __init__(self, backend="winusb", script=(), error=None, idle=False):
        self.interface_number = 0
        self.endpoint_in =0x81
        self.endpoint_out = 0x01
        if backend == "winusb":
            self.device_vid = 0x18D1
            self.device_pid = 0x2D01
        self._script = list(script)
        self._error = error
        self._idle = idle
        self.closed = False
        self.reads = 0
        self.writes = []

    def read(self):
        self.reads += 1
        if self._script:
            return self._script.pop(0)
        if self._error is not None:
            raise self._error
        if self._idle:
            time.sleep(0.002)
        return b""

    def write(self, payload, timeout_ms=1000):
        self.writes.append((payload, timeout_ms))

    def close(self):
        self.closed = True


class RetryConnection:
    """Controllable connection for retry and shutdown tests."""

    def __init__(self, script=(), block_after_script=False):
        self.interface_number = 0
        self.endpoint_in = 0x81
        self.endpoint_out = 0x01
        self.device_vid = 0x18D1
        self.device_pid = 0x2D01
        self._script = list(script)
        self._block_after_script = block_after_script
        self.read_waiting = threading.Event()
        self.cancelled = threading.Event()
        self.closed = threading.Event()
        self.cancel_calls = 0
        self.writes = []

    def read(self):
        if self._script:
            return self._script.pop(0)
        self.read_waiting.set()
        if self._block_after_script:
            self.cancelled.wait(2.0)
        else:
            time.sleep(0.002)
        return b""

    def write(self, payload, timeout_ms=1000):
        self.writes.append((payload, timeout_ms))

    def cancel_pending_read(self):
        self.cancel_calls += 1
        self.cancelled.set()

    def close(self):
        self.closed.set()
        self.cancelled.set()


def make_retry_host(plans):
    remaining = list(plans)

    class RetryHost:
        instances = []

        def __init__(self, **kwargs):
            self.usb = SimpleNamespace(using_usbdk=False)
            self.cancel_event = kwargs["cancel_event"]
            self.plan = remaining.pop(0) if remaining else "discovery"
            self.connect_started = threading.Event()
            self.closed = threading.Event()
            self.connect_calls = 0
            self.__class__.instances.append(self)

        def connect(self):
            self.connect_calls += 1
            self.connect_started.set()
            if self.plan == "discovery":
                self.cancel_event.wait(2.0)
                raise RuntimeError("discovery interrupted")
            if isinstance(self.plan, BaseException):
                raise self.plan
            return self.plan

        def close(self):
            self.closed.set()

    return RetryHost


def run_receiver(connection_or_error=None, connections=None, doctor=None,
                 stop_after_events=3, heartbeat_timeout=None,
                 stop_codes=(), deadline_s=8.0,
                 stop_delay_after_code_s=0.0, benchmark=False):
    """Drive AoaReceiver._run synchronously with a fake host.

    Stops once enough touch events were handled, or once the doctor records
    any of ``stop_codes``, or at ``deadline_s`` (which fails the test through
    unfinished receiver assertions instead of hanging).
    """
    events_received = []
    statuses = []
    disconnects = []
    router_down = []
    router_up = []
    router = AoaTouchRouter(
        ["a", "b"],
        False,
        lambda key: router_down.append(key) or True,
        lambda key: router_up.append(key) or True,
        None,
    )
    disconnect_handler = make_disconnect_handler(router, doctor)

    def on_disconnect():
        disconnects.append(True)
        disconnect_handler()

    receiver = AoaReceiver(
        on_event=router.handle,
        on_status=lambda text, connected: statuses.append((text, connected)),
        on_disconnect=on_disconnect,
        lane_count=2,
        benchmark=benchmark,
        doctor=doctor,
    )
    if heartbeat_timeout is not None:
        receiver.HEARTBEAT_TIMEOUT_SECONDS = heartbeat_timeout

    queue = list(connections or [connection_or_error])

    class FakeHost:
        def __init__(self, **_kwargs):
            self.usb = SimpleNamespace(using_usbdk=False)
            self.closed = False

        def connect(self):
            item = queue.pop(0) if queue else None
            if isinstance(item, Exception):
                raise item
            return item

        def close(self):
            self.closed = True

    original_handle = router.handle

    def counted(event):
        events_received.append(event)
        original_handle(event)

    receiver.on_event = counted

    def watchdog():
        deadline = time.monotonic() + deadline_s
        while not receiver.finished.is_set():
            if len(events_received) >= stop_after_events:
                break
            if doctor is not None and stop_codes:
                if any(
                    event.code in stop_codes for event in doctor.events()
                ):
                    if stop_delay_after_code_s:
                        time.sleep(stop_delay_after_code_s)
                    break
            if time.monotonic() > deadline:
                break
            time.sleep(0.005)
        receiver.stop()

    with mock.patch("aoa_transport.AoaHost", FakeHost):
        thread = threading.Thread(target=watchdog, daemon=True)
        thread.start()
        receiver._run()

    return receiver, statuses, disconnects, router, router_down, router_up


class ReceiverDiagnosticTests(unittest.TestCase):
    def test_transport_open_waits_for_app_before_session_ready(self):
        doctor = ConnectionDoctor()
        connection = RetryConnection(block_after_script=True)
        host_type = make_retry_host([connection])
        statuses = []
        receiver = AoaReceiver(
            on_event=lambda _event: None,
            on_status=lambda text, connected: statuses.append(
                (text, connected)
            ),
            on_disconnect=lambda: None,
            lane_count=2,
            doctor=doctor,
        )

        with mock.patch("aoa_transport.AoaHost", host_type):
            receiver.start()
            try:
                self.assertTrue(receiver.transport_open.wait(0.5))
                self.assertTrue(connection.read_waiting.wait(0.5))
                self.assertFalse(receiver.session_ready.is_set())
                self.assertFalse(any(ready for _, ready in statuses))
                self.assertEqual(doctor.snapshot().backend, "WinUSB")
                self.assertEqual(
                    doctor.snapshot().state,
                    ConnectionState.WAITING_FOR_ANDROID_APP,
                )
                self.assertTrue(
                    any(
                        "USB connected over WinUSB" in text
                        and "waiting for Android app" in text
                        and not ready
                        for text, ready in statuses
                    )
                )
            finally:
                receiver.stop()

    def test_winusb_success_reaches_connected_stream(self):
        doctor = ConnectionDoctor()
        connection = StreamingConnection(
            script=[
                session_reset(sequence=0),
                packet(ACTION_HEARTBEAT, sequence=1),
                packet(ACTION_HEARTBEAT, sequence=2),
            ],
            idle=True,
        )
        receiver, statuses, _, _, _, _ = run_receiver(
            connection, doctor=doctor, stop_after_events=2
        )

        self.assertTrue(receiver.finished.is_set())
        seen = codes(doctor)
        self.assertIn(dc.HPT_USB_BACKEND_SELECTED, seen)
        self.assertIn(dc.HPT_APP_HANDSHAKE_OK, seen)
        self.assertIn(dc.HPT_STREAM_CONNECTED, seen)
        snapshot = doctor.snapshot()
        self.assertEqual(
            snapshot.state, ConnectionState.TOUCH_STREAM_ACTIVE
        )
        self.assertEqual(snapshot.backend, "WinUSB")
        self.assertEqual(snapshot.aoa_ids, "18D1:2D01")
        self.assertGreaterEqual(snapshot.heartbeats, 1)
        self.assertTrue(snapshot.packets_flowing)
        self.assertTrue(
            any(connected for _, connected in statuses)
        )
        waiting_index = next(
            index
            for index, (text, connected) in enumerate(statuses)
            if "waiting for Android app" in text and not connected
        )
        ready_index = next(
            index
            for index, (_, connected) in enumerate(statuses)
            if connected
        )
        self.assertLess(waiting_index, ready_index)

    def test_new_host_epoch_discards_old_android_backlog(self):
        stale_report = packet(
            ACTION_HEARTBEAT,
            pointer_id=9,
            flags=(
                FLAG_QUEUE_DIAGNOSTICS
                | FLAG_QUEUE_WARNING
                | FLAG_QUEUE_RESYNC
                | FLAG_QUEUE_FAILSAFE
            ),
            sequence=50,
            x=32767,
            y=67,
            event_nanos=1_000_000_000,
        )
        stale_down = packet(
            ACTION_DOWN,
            pointer_id=2,
            flags=FLAG_INSIDE | FLAG_LOCKED,
            sequence=51,
            x=7500,
            event_nanos=1_001_000_000,
        )
        recovered_epoch = session_reset(sequence=52, recovered=True)
        fresh_report = packet(
            ACTION_HEARTBEAT,
            pointer_id=1,
            flags=FLAG_QUEUE_DIAGNOSTICS,
            sequence=53,
            x=500,
            event_nanos=1_002_000_000,
        )
        fresh_down = packet(
            ACTION_DOWN,
            pointer_id=3,
            flags=FLAG_INSIDE | FLAG_LOCKED,
            sequence=54,
            x=2500,
            event_nanos=1_003_000_000,
        )
        connection = StreamingConnection(
            script=[
                stale_report + stale_down,
                recovered_epoch + fresh_report + fresh_down,
            ],
            idle=True,
        )

        receiver, _, _, _, pressed, _ = run_receiver(
            connection,
            stop_after_events=2,
            benchmark=True,
        )

        self.assertEqual(pressed, ["a"])
        self.assertEqual(
            connection.writes,
            [(HOST_ATTACH_REQUEST, 500)],
        )
        queue = receiver.queue_telemetry_snapshot()
        self.assertEqual(queue.reports, 1)
        self.assertAlmostEqual(queue.max_age_ms, 5.0)
        self.assertEqual(queue.max_depth, 1)
        self.assertEqual(queue.warning_reports, 0)
        self.assertEqual(queue.resyncs, 0)
        self.assertEqual(queue.failsafe_reports, 0)
        self.assertEqual(queue.host_recoveries, 1)
        latency = receiver.latency_snapshot()
        self.assertEqual(latency.samples, 1)
        self.assertEqual(latency.session_samples, 1)

    def test_protocol_mismatch_is_reported(self):
        doctor = ConnectionDoctor()
        connection = StreamingConnection(
            script=[packet(ACTION_HEARTBEAT, sequence=1, version=1)],
            idle=True,
        )
        receiver, statuses, _, _, _, _ = run_receiver(
            connection,
            doctor=doctor,
            heartbeat_timeout=0.05,
            stop_codes=(dc.HPT_APP_PROTOCOL_MISMATCH,),
            # Keep the receiver alive beyond the original first-packet
            # deadline; the closed attempt must not become APP-401.
            stop_delay_after_code_s=0.12,
        )
        seen = codes(doctor)
        self.assertIn(dc.HPT_APP_PROTOCOL_MISMATCH, seen)
        self.assertNotIn(dc.HPT_APP_NOT_RESPONDING, seen)
        event = [
            e
            for e in doctor.events()
            if e.code == dc.HPT_APP_PROTOCOL_MISMATCH
        ][0]
        self.assertIn("v1", event.detail)
        self.assertEqual(
            event.metadata,
            {
                "received_touch_protocol_version": 1,
                "expected_touch_protocol_version": 2,
            },
        )
        self.assertFalse(event.transient)
        self.assertIn("Update both", event.action)
        self.assertEqual(
            doctor.snapshot().last_code,
            dc.HPT_APP_PROTOCOL_MISMATCH,
        )
        self.assertEqual(doctor.snapshot().touch_protocol_version, 1)
        self.assertFalse(any(connected for _, connected in statuses))
        self.assertFalse(receiver.session_ready.is_set())
        self.assertTrue(connection.closed)

    def test_protocol_mismatch_remains_final_after_key_release(self):
        doctor = ConnectionDoctor()
        down = TOUCH_PACKET.pack(
            TOUCH_MAGIC,
            dc.TOUCH_PROTOCOL_VERSION,
            ACTION_DOWN,
            1,
            FLAG_INSIDE | FLAG_LOCKED,
            2500,
            5000,
            1,
            0,
        )
        mismatch = packet(
            ACTION_HEARTBEAT,
            sequence=2,
            version=1,
        )
        connection = StreamingConnection(
            script=[session_reset(sequence=0), down, mismatch],
            idle=True,
        )

        _, _, _, router, pressed, released = run_receiver(
            connection,
            doctor=doctor,
            heartbeat_timeout=0.05,
            stop_after_events=99,
            stop_codes=(dc.HPT_APP_PROTOCOL_MISMATCH,),
        )

        self.assertEqual(pressed, ["a"])
        self.assertEqual(released, ["a"])
        self.assertEqual(router.key_counts, {})
        self.assertIn(
            dc.HPT_LINK_DISCONNECT_ACTIVE_INPUT,
            codes(doctor),
        )
        self.assertEqual(
            doctor.events()[-1].code,
            dc.HPT_APP_PROTOCOL_MISMATCH,
        )
        self.assertEqual(
            doctor.snapshot().last_code,
            dc.HPT_APP_PROTOCOL_MISMATCH,
        )
        self.assertNotIn(dc.HPT_APP_NOT_RESPONDING, codes(doctor))

    def test_stalled_stream_after_data(self):
        doctor = ConnectionDoctor()
        connection = StreamingConnection(
            script=[
                session_reset(sequence=0),
                packet(ACTION_HEARTBEAT, sequence=1),
            ],
            idle=True,
        )
        run_receiver(
            connection,
            doctor=doctor,
            heartbeat_timeout=0.05,
            stop_after_events=99,
            stop_codes=(dc.HPT_STREAM_STALLED,),
        )
        seen = codes(doctor)
        self.assertIn(dc.HPT_APP_HANDSHAKE_OK, seen)
        self.assertIn(dc.HPT_STREAM_STALLED, seen)
        event = [
            e for e in doctor.events() if e.code == dc.HPT_STREAM_STALLED
        ][0]
        self.assertEqual(event.state, ConnectionState.STALLED_STREAM)
        self.assertTrue(event.transient)

    def test_app_not_responding_when_never_streamed(self):
        doctor = ConnectionDoctor()
        connection = StreamingConnection(idle=True)
        _, statuses, _, _, _, _ = run_receiver(
            connection,
            doctor=doctor,
            heartbeat_timeout=0.05,
            stop_after_events=99,
            stop_codes=(dc.HPT_APP_NOT_RESPONDING,),
        )
        seen = codes(doctor)
        self.assertNotIn(dc.HPT_APP_HANDSHAKE_OK, seen)
        self.assertIn(dc.HPT_APP_NOT_RESPONDING, seen)
        self.assertFalse(any(connected for _, connected in statuses))
        self.assertTrue(
            any(
                "waiting for Android app" in text and not connected
                for text, connected in statuses
            )
        )
        self.assertEqual(doctor.snapshot().backend, "WinUSB")
        self.assertEqual(
            doctor.snapshot().state,
            ConnectionState.WAITING_FOR_ANDROID_APP,
        )
        event = [
            e
            for e in doctor.events()
            if e.code == dc.HPT_APP_NOT_RESPONDING
        ][0]
        self.assertIn("Holodori Trackpad app", event.action)

    def test_reconnection_after_disconnect(self):
        doctor = ConnectionDoctor()
        dead = AoaError(
            "read the AOA touch stream: LIBUSB_ERROR_NO_DEVICE",
            native_name="LIBUSB_ERROR_NO_DEVICE",
            native_code=-4,
        )
        live = StreamingConnection(
            script=[
                session_reset(sequence=0),
                packet(ACTION_HEARTBEAT, sequence=1),
            ],
            idle=True,
        )
        receiver, _, _, _, _, _ = run_receiver(
            connections=[dead, live],
            doctor=doctor,
            heartbeat_timeout=0.5,
            stop_after_events=1,
        )
        seen = codes(doctor)
        self.assertIn(dc.HPT_LINK_DISCONNECTED, seen)
        self.assertIn(dc.HPT_LINK_RECONNECTING, seen)
        self.assertEqual(
            doctor.snapshot().state, ConnectionState.CONNECTED_STREAM
        )
        self.assertEqual(doctor.snapshot().reconnect_attempts, 1)

    def test_disconnect_while_keys_held_releases_everything(self):
        from winusb_transport import WinUsbError

        doctor = ConnectionDoctor()
        # Place the touch in lane 1 of 2 (x=0.25 -> key "a").
        down_packet = TOUCH_PACKET.pack(
            TOUCH_MAGIC, dc.TOUCH_PROTOCOL_VERSION, ACTION_DOWN, 1,
            FLAG_INSIDE | FLAG_LOCKED, 2500, 5000, 1, 0,
        )
        dead = StreamingConnection(
            script=[session_reset(sequence=0), down_packet],
            error=WinUsbError(
                "The device is not connected",
                operation="read the AOA WinUSB stream",
                native_code=1167,
            ),
        )
        live = StreamingConnection(
            script=[
                session_reset(sequence=0),
                packet(ACTION_HEARTBEAT, sequence=1),
            ],
            idle=True,
        )
        receiver, _, _, router, down, up = run_receiver(
            connections=[dead, live],
            doctor=doctor,
            heartbeat_timeout=0.5,
            stop_after_events=2,
        )
        self.assertEqual(down, ["a"])
        self.assertEqual(up, ["a"])
        self.assertEqual(router.key_counts, {})
        self.assertIn(dc.HPT_LINK_DISCONNECTED, codes(doctor))
        self.assertIn(
            dc.HPT_LINK_DISCONNECT_ACTIVE_INPUT, codes(doctor)
        )
        self.assertNotIn(dc.HPT_USB_WINUSB_UNAVAILABLE, codes(doctor))
        event = [
            e
            for e in doctor.events()
            if e.code == dc.HPT_LINK_DISCONNECT_ACTIVE_INPUT
        ][0]
        self.assertEqual(event.state, ConnectionState.DISCONNECT)
        self.assertIn("released", event.summary)


class RetrySemanticsTests(unittest.TestCase):
    def test_retry_interrupts_discovery_and_starts_fresh_host(self):
        host_type = make_retry_host(["discovery", "discovery"])
        disconnects = []
        receiver = AoaReceiver(
            on_event=lambda _event: None,
            on_status=lambda _text, _connected: None,
            on_disconnect=lambda: disconnects.append(True),
            lane_count=2,
        )

        with mock.patch("aoa_transport.AoaHost", host_type):
            receiver.start()
            try:
                self.assertTrue(
                    wait_until(
                        lambda: host_type.instances
                        and host_type.instances[0].connect_started.is_set()
                    )
                )
                started = time.monotonic()
                self.assertTrue(receiver.request_retry())
                self.assertTrue(receiver.request_retry())
                self.assertTrue(
                    wait_until(
                        lambda: len(host_type.instances) >= 2
                        and host_type.instances[1].connect_started.is_set(),
                        timeout=0.75,
                    )
                )
                self.assertLess(time.monotonic() - started, 0.75)
                self.assertTrue(host_type.instances[0].closed.is_set())
                self.assertTrue(disconnects)
            finally:
                receiver.stop()

    def test_retry_active_connection_releases_input_and_reconnects(self):
        down = packet(
            ACTION_DOWN,
            pointer_id=1,
            flags=FLAG_INSIDE | FLAG_LOCKED,
            sequence=1,
        )
        connection = RetryConnection(
            script=[session_reset(sequence=0), down]
        )
        host_type = make_retry_host([connection, "discovery"])
        held = threading.Event()
        released = threading.Event()
        doctor = ConnectionDoctor()

        def on_event(_event):
            held.set()

        def on_disconnect():
            if held.is_set():
                held.clear()
                released.set()

        receiver = AoaReceiver(
            on_event=on_event,
            on_status=lambda _text, _connected: None,
            on_disconnect=on_disconnect,
            lane_count=2,
            doctor=doctor,
        )
        receiver.HEARTBEAT_TIMEOUT_SECONDS = 2.0

        with mock.patch("aoa_transport.AoaHost", host_type):
            receiver.start()
            try:
                self.assertTrue(receiver.session_ready.wait(0.5))
                self.assertTrue(held.is_set())
                self.assertTrue(receiver.request_retry())
                self.assertTrue(released.wait(0.5))
                self.assertTrue(
                    wait_until(
                        lambda: len(host_type.instances) >= 2
                        and host_type.instances[1].connect_started.is_set()
                    )
                )
                self.assertTrue(connection.closed.is_set())
                self.assertTrue(host_type.instances[0].closed.is_set())
                self.assertFalse(receiver.session_ready.is_set())
                self.assertIn(
                    dc.HPT_SESSION_RETRY_REQUESTED,
                    codes(doctor),
                )
            finally:
                receiver.stop()

    def test_retry_cancels_blocked_read(self):
        connection = RetryConnection(
            script=[packet(ACTION_HEARTBEAT, sequence=1)],
            block_after_script=True,
        )
        host_type = make_retry_host([connection, "discovery"])
        receiver = AoaReceiver(
            on_event=lambda _event: None,
            on_status=lambda _text, _connected: None,
            on_disconnect=lambda: None,
            lane_count=2,
        )

        with mock.patch("aoa_transport.AoaHost", host_type):
            receiver.start()
            try:
                self.assertTrue(connection.read_waiting.wait(0.5))
                started = time.monotonic()
                self.assertTrue(receiver.request_retry())
                self.assertTrue(
                    wait_until(
                        lambda: len(host_type.instances) >= 2
                        and host_type.instances[1].connect_started.is_set(),
                        timeout=0.75,
                    )
                )
                self.assertGreaterEqual(connection.cancel_calls, 1)
                self.assertTrue(connection.closed.is_set())
                self.assertLess(time.monotonic() - started, 0.75)
            finally:
                receiver.stop()

    def test_retry_skips_backoff_and_rebuilds_backend(self):
        failure = AoaError(
            "device disconnected",
            native_name="LIBUSB_ERROR_NO_DEVICE",
            native_code=-4,
        )
        host_type = make_retry_host([failure, "discovery"])
        disconnected = threading.Event()
        receiver = AoaReceiver(
            on_event=lambda _event: None,
            on_status=lambda _text, _connected: None,
            on_disconnect=disconnected.set,
            lane_count=2,
        )
        receiver.RECONNECT_BACKOFF_SECONDS = 2.0

        with mock.patch("aoa_transport.AoaHost", host_type):
            receiver.start()
            try:
                self.assertTrue(disconnected.wait(0.5))
                started = time.monotonic()
                self.assertTrue(receiver.request_retry())
                self.assertTrue(
                    wait_until(
                        lambda: len(host_type.instances) >= 2
                        and host_type.instances[1].connect_started.is_set(),
                        timeout=0.75,
                    )
                )
                self.assertLess(time.monotonic() - started, 0.75)
                self.assertTrue(host_type.instances[0].closed.is_set())
            finally:
                receiver.stop()

    def test_shutdown_cancels_read_and_rejects_late_retry(self):
        connection = RetryConnection(
            script=[packet(ACTION_HEARTBEAT, sequence=1)],
            block_after_script=True,
        )
        host_type = make_retry_host([connection])
        disconnected = threading.Event()
        doctor = ConnectionDoctor()
        receiver = AoaReceiver(
            on_event=lambda _event: None,
            on_status=lambda _text, _connected: None,
            on_disconnect=disconnected.set,
            lane_count=2,
            doctor=doctor,
        )

        with mock.patch("aoa_transport.AoaHost", host_type):
            receiver.start()
            self.assertTrue(connection.read_waiting.wait(0.5))
            receiver.stop()
            receiver.stop()

        self.assertTrue(receiver.finished.is_set())
        self.assertTrue(disconnected.is_set())
        self.assertGreaterEqual(connection.cancel_calls, 1)
        self.assertTrue(connection.closed.is_set())
        self.assertTrue(host_type.instances[0].closed.is_set())
        self.assertFalse(receiver.request_retry())
        self.assertNotIn(dc.HPT_SESSION_RETRY_REQUESTED, codes(doctor))
        self.assertEqual(len(host_type.instances), 1)

    def test_repeated_retry_racing_shutdown_is_safe(self):
        connection = RetryConnection(
            script=[packet(ACTION_HEARTBEAT, sequence=1)],
            block_after_script=True,
        )
        host_type = make_retry_host([connection, "discovery"])
        receiver = AoaReceiver(
            on_event=lambda _event: None,
            on_status=lambda _text, _connected: None,
            on_disconnect=lambda: None,
            lane_count=2,
        )
        gate = threading.Barrier(3)
        failures = []

        def retry_repeatedly():
            try:
                gate.wait()
                for _ in range(20):
                    receiver.request_retry()
            except BaseException as exc:
                failures.append(exc)

        def shutdown():
            try:
                gate.wait()
                receiver.stop()
            except BaseException as exc:
                failures.append(exc)

        with mock.patch("aoa_transport.AoaHost", host_type):
            receiver.start()
            self.assertTrue(connection.read_waiting.wait(0.5))
            retry_thread = threading.Thread(target=retry_repeatedly)
            stop_thread = threading.Thread(target=shutdown)
            retry_thread.start()
            stop_thread.start()
            gate.wait()
            retry_thread.join(1.0)
            stop_thread.join(2.0)
            receiver.stop()

        self.assertEqual(failures, [])
        self.assertFalse(retry_thread.is_alive())
        self.assertFalse(stop_thread.is_alive())
        self.assertTrue(receiver.finished.is_set())
        self.assertTrue(connection.closed.is_set())
        self.assertTrue(
            all(host.closed.is_set() for host in host_type.instances)
        )


if __name__ == "__main__":
    unittest.main()
