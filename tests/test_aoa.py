import struct
import time
import unittest
from unittest import mock

from aoa_mode import AoaTouchRouter
from aoa_transport import (
    ACTION_CANCEL,
    ACTION_DOWN,
    ACTION_HEARTBEAT,
    ACTION_MOVE,
    ACTION_UP,
    AoaError,
    AoaHost,
    AoaReceiver,
    ClockNormalizedLatency,
    FLAG_INSIDE,
    FLAG_LOCKED,
    FLAG_QUEUE_DIAGNOSTICS,
    FLAG_QUEUE_FAILSAFE,
    FLAG_QUEUE_RESYNC,
    FLAG_QUEUE_WARNING,
    QueueTelemetry,
    TOUCH_MAGIC,
    TOUCH_PACKET,
    TouchEvent,
    TouchPacketParser,
)


def event(action, pointer_id, x, y, sequence=0, flags=None):
    if flags is None:
        flags = FLAG_INSIDE | FLAG_LOCKED
    return TouchEvent(action, pointer_id, flags, x, y, sequence, 0)


class PacketParserTests(unittest.TestCase):
    def test_fragmented_and_concatenated_packets(self):
        first = TOUCH_PACKET.pack(
            TOUCH_MAGIC, 1, ACTION_DOWN, 3, 3, 2500, 7500, 10, 123
        )
        second = TOUCH_PACKET.pack(
            TOUCH_MAGIC, 1, ACTION_UP, 3, 2, 2500, 7500, 11, 456
        )
        parser = TouchPacketParser()
        self.assertEqual(list(parser.feed(first[:9])), [])
        parsed = list(parser.feed(first[9:] + second))
        self.assertEqual(len(parsed), 2)
        self.assertAlmostEqual(parsed[0].x, 0.25)
        self.assertAlmostEqual(parsed[0].y, 0.75)
        self.assertEqual(parsed[1].action, ACTION_UP)

    def test_parser_resynchronizes_after_noise(self):
        packet = TOUCH_PACKET.pack(
            TOUCH_MAGIC, 1, ACTION_MOVE, 1, 3, 5000, 5000, 2, 0
        )
        parser = TouchPacketParser()
        parsed = list(parser.feed(b"garbage" + packet))
        self.assertEqual(len(parsed), 1)
        self.assertEqual(parsed[0].pointer_id, 1)

    def test_heartbeat_carries_queue_telemetry(self):
        packet = TOUCH_PACKET.pack(
            TOUCH_MAGIC,
            1,
            ACTION_HEARTBEAT,
            7,
            FLAG_QUEUE_DIAGNOSTICS
            | FLAG_QUEUE_WARNING
            | FLAG_QUEUE_RESYNC
            | FLAG_QUEUE_FAILSAFE,
            1234,
            2,
            9,
            456,
        )
        parsed = list(TouchPacketParser().feed(packet))
        self.assertEqual(len(parsed), 1)
        heartbeat = parsed[0]
        self.assertTrue(heartbeat.has_queue_diagnostics)
        self.assertEqual(heartbeat.queue_depth, 7)
        self.assertEqual(heartbeat.queue_age_nanos, 12_340_000)
        self.assertEqual(heartbeat.queue_resyncs, 2)

        telemetry = QueueTelemetry()
        telemetry.observe(heartbeat)
        snapshot = telemetry.snapshot()
        self.assertEqual(snapshot.reports, 1)
        self.assertAlmostEqual(snapshot.max_age_ms, 12.34)
        self.assertEqual(snapshot.max_depth, 7)
        self.assertEqual(snapshot.warning_reports, 1)
        self.assertEqual(snapshot.resyncs, 2)
        self.assertEqual(snapshot.failsafe_reports, 1)


class LatencyTests(unittest.TestCase):
    def test_clock_normalization_uses_fastest_offset_as_baseline(self):
        latency = ClockNormalizedLatency()
        latency.observe(100_000_000, 1_100_000_000, True)
        latency.observe(200_000_000, 1_210_000_000, True)
        # A heartbeat can improve clock alignment without being counted as a
        # touch-latency sample.
        latency.observe(300_000_000, 1_290_000_000, False)

        snapshot = latency.snapshot()
        self.assertEqual(snapshot.samples, 2)
        self.assertAlmostEqual(snapshot.mean_excess_ms, 15.0)
        self.assertAlmostEqual(snapshot.max_excess_ms, 20.0)


class AoaHostTests(unittest.TestCase):
    def test_data_connection_prefers_winusb_without_enabling_fallback(self):
        connection = object()
        host = object.__new__(AoaHost)
        host.prefer_usbdk = True
        host.usb = type("Usb", (), {"using_usbdk": False})()
        host._find_data_accessory = mock.Mock(return_value=connection)
        host._replace_usb = mock.Mock()

        self.assertIs(host._wait_for_data_accessory(0.1), connection)
        host._replace_usb.assert_not_called()

    def test_data_connection_falls_back_to_usbdk_after_winusb_error(self):
        connection = object()
        host = object.__new__(AoaHost)
        host.prefer_usbdk = True
        host.usb = type("Usb", (), {"using_usbdk": False})()

        def replace_usb(use_usbdk):
            host.usb = type(
                "Usb", (), {"using_usbdk": bool(use_usbdk)}
            )()

        host._replace_usb = mock.Mock(side_effect=replace_usb)
        host._find_data_accessory = mock.Mock(
            side_effect=[AoaError("LIBUSB_ERROR_NOT_SUPPORTED"), connection]
        )

        self.assertIs(host._wait_for_data_accessory(0.1), connection)
        host._replace_usb.assert_called_once_with(use_usbdk=True)
        self.assertTrue(host.usb.using_usbdk)

    def test_data_connection_falls_back_after_winusb_attach_grace(self):
        connection = object()
        host = object.__new__(AoaHost)
        host.prefer_usbdk = True
        host.usb = type("Usb", (), {"using_usbdk": False})()

        def replace_usb(use_usbdk):
            host.usb = type(
                "Usb", (), {"using_usbdk": bool(use_usbdk)}
            )()

        host._replace_usb = mock.Mock(side_effect=replace_usb)
        host._find_data_accessory = mock.Mock(
            side_effect=[None, connection]
        )

        with (
            mock.patch(
                "aoa_transport.time.monotonic",
                side_effect=[0.0, 0.0, 0.0, 1.5, 1.5],
            ),
            mock.patch("aoa_transport.time.sleep"),
        ):
            self.assertIs(host._wait_for_data_accessory(2.0), connection)

        host._replace_usb.assert_called_once_with(use_usbdk=True)

    def test_data_fallback_is_disabled_when_usbdk_was_opted_out(self):
        host = object.__new__(AoaHost)
        host.prefer_usbdk = False
        host.usb = type("Usb", (), {"using_usbdk": False})()
        host._replace_usb = mock.Mock()

        self.assertFalse(host._enable_usbdk_data_fallback())
        host._replace_usb.assert_not_called()


class RouterTests(unittest.TestCase):
    def setUp(self):
        self.down = []
        self.up = []
        self.router = AoaTouchRouter(
            ["a", "b"],
            False,
            lambda key: self.down.append(key) or True,
            lambda key: self.up.append(key) or True,
            None,
        )

    def test_press_drag_release(self):
        self.router.handle(event(ACTION_DOWN, 1, 0.2, 0.5, 1))
        self.router.handle(event(ACTION_MOVE, 1, 0.8, 0.5, 2))
        self.router.handle(event(ACTION_UP, 1, 0.8, 0.5, 3))
        self.assertEqual(self.down, ["a", "b"])
        self.assertEqual(self.up, ["a", "b"])

    def test_two_fingers_in_one_lane_use_reference_count(self):
        self.router.handle(event(ACTION_DOWN, 1, 0.2, 0.5, 1))
        self.router.handle(event(ACTION_DOWN, 2, 0.3, 0.5, 2))
        self.router.handle(event(ACTION_UP, 1, 0.2, 0.5, 3))
        self.assertEqual(self.down, ["a"])
        self.assertEqual(self.up, [])
        self.router.handle(event(ACTION_UP, 2, 0.3, 0.5, 4))
        self.assertEqual(self.up, ["a"])

    def test_outside_and_cancel_release_keys(self):
        self.router.handle(event(ACTION_DOWN, 1, 0.2, 0.5, 1))
        self.router.handle(
            event(ACTION_MOVE, 1, -0.1, 0.5, 2, FLAG_LOCKED)
        )
        self.assertEqual(self.up, ["a"])
        self.router.handle(event(ACTION_DOWN, 2, 0.8, 0.5, 3))
        self.router.handle(event(ACTION_CANCEL, 0, 0, 0, 4, 0))
        self.assertEqual(self.up, ["a", "b"])

    def test_heartbeat_participates_in_sequence_without_touching_keys(self):
        self.router.handle(event(ACTION_DOWN, 1, 0.2, 0.5, 1))
        self.router.handle(event(ACTION_HEARTBEAT, 0, 0, 0, 2, 0))
        self.router.handle(event(ACTION_UP, 1, 0.2, 0.5, 3))

        self.assertEqual(self.router.sequence_gaps, 0)
        self.assertEqual(self.down, ["a"])
        self.assertEqual(self.up, ["a"])
        self.assertEqual(self.router.stats["events"], 2)

    def test_sequence_gap_releases_stale_keys_before_latest_sample(self):
        self.router.handle(event(ACTION_DOWN, 1, 0.2, 0.5, 1))
        self.router.handle(event(ACTION_DOWN, 2, 0.8, 0.5, 3))

        self.assertEqual(self.router.sequence_gaps, 1)
        self.assertEqual(self.down, ["a", "b"])
        self.assertEqual(self.up, ["a"])
        self.assertEqual(self.router.active_keys, {2: "b"})
        self.assertEqual(self.router.key_counts, {"b": 1})


class ReceiverTests(unittest.TestCase):
    def test_silent_connection_times_out_and_disconnects(self):
        class SilentConnection:
            def __init__(self):
                self.closed = False

            def read(self):
                time.sleep(0.003)
                return b""

            def close(self):
                self.closed = True

        connection = SilentConnection()

        class FakeHost:
            instance = None

            def __init__(self, **_kwargs):
                self.usb = type("Usb", (), {"using_usbdk": False})()
                self.closed = False
                FakeHost.instance = self

            def connect(self):
                return connection

            def close(self):
                self.closed = True

        statuses = []
        disconnects = []
        receiver = None

        def disconnect():
            disconnects.append(True)
            receiver._stop.set()

        receiver = AoaReceiver(
            on_event=lambda _event: None,
            on_status=lambda text, connected: statuses.append(
                (text, connected)
            ),
            on_disconnect=disconnect,
            lane_count=2,
        )
        receiver.HEARTBEAT_TIMEOUT_SECONDS = 0.01

        with mock.patch("aoa_transport.AoaHost", FakeHost):
            receiver._run()

        self.assertEqual(disconnects, [True])
        self.assertTrue(connection.closed)
        self.assertTrue(FakeHost.instance.closed)
        self.assertTrue(receiver.finished.is_set())
        self.assertIn(
            ("AOA heartbeat timed out; reconnecting", False),
            statuses,
        )


if __name__ == "__main__":
    unittest.main()
