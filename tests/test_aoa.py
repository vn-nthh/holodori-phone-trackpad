import struct
import time
import unittest
from unittest import mock

import connection_doctor as doctor_codes
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
    FLAG_INCIDENT_ACTIVE_TOUCH,
    FLAG_INCIDENT_MOTION_BATCH,
    FLAG_INCIDENT_TIMING_BREAKDOWN,
    FLAG_INCIDENT_WRITER_BLOCKED,
    FLAG_INSIDE,
    FLAG_HOST_RECOVERY,
    FLAG_LOCKED,
    FLAG_QUEUE_DIAGNOSTICS,
    FLAG_QUEUE_FAILSAFE,
    FLAG_QUEUE_INCIDENT,
    FLAG_QUEUE_RESYNC,
    FLAG_QUEUE_WARNING,
    FLAG_SESSION_RESET,
    HOST_CAP_MOTION_BATCH_DIAGNOSTICS,
    HOST_CAP_TIMING_BREAKDOWN,
    HOST_CONTROL_ATTACH,
    HOST_CONTROL_MAGIC,
    HOST_CONTROL_PACKET,
    HOST_LANE_COUNT_SHIFT,
    INCIDENT_DETAIL_REASON_SHIFT,
    INCIDENT_REASON_CAPACITY,
    INCIDENT_REASON_WARNING,
    MOTION_HISTORY_SPAN_UNIT_NANOS,
    QueueTelemetry,
    TIMING_APP_SHIFT,
    TIMING_DISPATCH_SHIFT,
    TIMING_DURATION_UNIT_NANOS,
    TIMING_QUEUE_SHIFT,
    TIMING_WRITE_SHIFT,
    TOUCH_MAGIC,
    TOUCH_PACKET,
    TouchEvent,
    TouchPacketParser,
    make_host_attach_request,
)


def event(
    action,
    pointer_id,
    x,
    y,
    sequence=0,
    flags=None,
    phone_event_nanos=0,
):
    if flags is None:
        flags = FLAG_INSIDE | FLAG_LOCKED
    return TouchEvent(
        action,
        pointer_id,
        flags,
        x,
        y,
        sequence,
        phone_event_nanos,
    )


class PacketParserTests(unittest.TestCase):
    def test_host_attach_advertises_diagnostics_and_lane_count(self):
        magic, version, action, capabilities = (
            HOST_CONTROL_PACKET.unpack(make_host_attach_request(12))
        )
        self.assertEqual(magic, HOST_CONTROL_MAGIC)
        self.assertEqual(version, doctor_codes.TOUCH_PROTOCOL_VERSION)
        self.assertEqual(action, HOST_CONTROL_ATTACH)
        self.assertTrue(capabilities & HOST_CAP_TIMING_BREAKDOWN)
        self.assertTrue(
            capabilities & HOST_CAP_MOTION_BATCH_DIAGNOSTICS
        )
        self.assertEqual(
            capabilities >> HOST_LANE_COUNT_SHIFT, 12
        )

    def test_fragmented_and_concatenated_packets(self):
        first = TOUCH_PACKET.pack(
            TOUCH_MAGIC,
            doctor_codes.TOUCH_PROTOCOL_VERSION,
            ACTION_DOWN,
            3,
            3,
            2500,
            7500,
            10,
            123,
        )
        second = TOUCH_PACKET.pack(
            TOUCH_MAGIC,
            doctor_codes.TOUCH_PROTOCOL_VERSION,
            ACTION_UP,
            3,
            2,
            2500,
            7500,
            11,
            456,
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
            TOUCH_MAGIC,
            doctor_codes.TOUCH_PROTOCOL_VERSION,
            ACTION_MOVE,
            1,
            3,
            5000,
            5000,
            2,
            0,
        )
        parser = TouchPacketParser()
        parsed = list(parser.feed(b"garbage" + packet))
        self.assertEqual(len(parsed), 1)
        self.assertEqual(parsed[0].pointer_id, 1)

    def test_heartbeat_carries_queue_telemetry(self):
        packet = TOUCH_PACKET.pack(
            TOUCH_MAGIC,
            doctor_codes.TOUCH_PROTOCOL_VERSION,
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
        self.assertEqual(snapshot.host_recoveries, 0)
        self.assertEqual(
            snapshot.warning_reports_from_first_stroke_s, ()
        )
        self.assertEqual(snapshot.incidents, ())

    def test_exact_queue_incident_carries_stall_context(self):
        detail = (
            INCIDENT_REASON_WARNING << INCIDENT_DETAIL_REASON_SHIFT
        ) | 500
        packet = TOUCH_PACKET.pack(
            TOUCH_MAGIC,
            doctor_codes.TOUCH_PROTOCOL_VERSION,
            ACTION_HEARTBEAT,
            2,
            FLAG_QUEUE_DIAGNOSTICS
            | FLAG_QUEUE_INCIDENT
            | FLAG_INCIDENT_ACTIVE_TOUCH
            | FLAG_INCIDENT_WRITER_BLOCKED,
            842,
            0,
            12,
            (5_250_000_000 & ~0xFFFF) | detail,
        )
        incident = list(TouchPacketParser().feed(packet))[0]
        self.assertTrue(incident.is_queue_incident)
        self.assertAlmostEqual(
            incident.queue_age_nanos / 1_000_000.0, 8.42
        )
        self.assertEqual(incident.queue_depth, 2)
        self.assertTrue(incident.incident_active_touch)
        self.assertTrue(incident.incident_writer_blocked)
        self.assertAlmostEqual(
            incident.incident_write_age_nanos / 1_000_000.0,
            10.0,
        )

        telemetry = QueueTelemetry()
        telemetry.observe(
            event(
                ACTION_DOWN,
                1,
                0.25,
                0.5,
                phone_event_nanos=2_000_000_000,
            )
        )
        telemetry.observe(incident, delivery_excess_ms=54.862)
        snapshot = telemetry.snapshot()
        self.assertEqual(len(snapshot.incidents), 1)
        self.assertAlmostEqual(snapshot.max_age_ms, 8.42)
        self.assertEqual(snapshot.max_depth, 2)
        diagnosed = snapshot.incidents[0]
        self.assertAlmostEqual(
            diagnosed.from_first_stroke_s, 3.25, places=3
        )
        self.assertAlmostEqual(diagnosed.queue_age_ms, 8.42)
        self.assertEqual(diagnosed.queue_depth, 2)
        self.assertEqual(diagnosed.reason, INCIDENT_REASON_WARNING)
        self.assertTrue(diagnosed.active_touch)
        self.assertTrue(diagnosed.writer_blocked)
        self.assertAlmostEqual(diagnosed.write_block_ms, 10.0)
        self.assertAlmostEqual(diagnosed.delivery_excess_ms, 54.862)
        self.assertEqual(
            len(snapshot.warning_reports_from_first_stroke_s), 1
        )
        self.assertAlmostEqual(
            snapshot.warning_reports_from_first_stroke_s[0],
            3.25,
            places=3,
        )

        telemetry.begin_epoch(recovered=False)
        self.assertEqual(telemetry.snapshot().incidents, ())

    def test_queue_incident_pairs_exact_stage_timing_by_token(self):
        token = 7
        summary_detail = (
            INCIDENT_REASON_WARNING << INCIDENT_DETAIL_REASON_SHIFT
        )
        summary_packet = TOUCH_PACKET.pack(
            TOUCH_MAGIC,
            doctor_codes.TOUCH_PROTOCOL_VERSION,
            ACTION_HEARTBEAT,
            1,
            FLAG_QUEUE_DIAGNOSTICS
            | FLAG_QUEUE_INCIDENT
            | FLAG_INCIDENT_ACTIVE_TOUCH,
            925,
            token,
            41,
            (5_250_000_000 & ~0xFFFF) | summary_detail,
        )

        expected_units = {
            TIMING_DISPATCH_SHIFT: 60,
            TIMING_APP_SHIFT: 20,
            TIMING_QUEUE_SHIFT: 290,
            TIMING_WRITE_SHIFT: 12,
        }
        packed = sum(
            units << shift for shift, units in expected_units.items()
        )

        def signed_short(value):
            value &= 0xFFFF
            return value - 0x10000 if value & 0x8000 else value

        timing_packet = TOUCH_PACKET.pack(
            TOUCH_MAGIC,
            doctor_codes.TOUCH_PROTOCOL_VERSION,
            ACTION_HEARTBEAT,
            token,
            FLAG_QUEUE_DIAGNOSTICS
            | FLAG_QUEUE_INCIDENT
            | FLAG_INCIDENT_TIMING_BREAKDOWN,
            signed_short(packed),
            signed_short(packed >> 16),
            43,
            (5_260_000_000 & ~0xFFFF) | ((packed >> 32) & 0xFFFF),
        )
        history_size = 2
        crossed_lanes = 3
        history_span_units = 320
        motion_packet = TOUCH_PACKET.pack(
            TOUCH_MAGIC,
            doctor_codes.TOUCH_PROTOCOL_VERSION,
            ACTION_HEARTBEAT,
            token,
            FLAG_QUEUE_DIAGNOSTICS
            | FLAG_QUEUE_INCIDENT
            | FLAG_INCIDENT_MOTION_BATCH,
            history_size,
            crossed_lanes,
            44,
            (5_261_000_000 & ~0xFFFF) | history_span_units,
        )
        parser = TouchPacketParser()
        summary = list(parser.feed(summary_packet))[0]
        timing = list(parser.feed(timing_packet))[0]
        motion = list(parser.feed(motion_packet))[0]
        self.assertTrue(summary.is_queue_incident)
        self.assertFalse(summary.is_queue_timing_breakdown)
        self.assertEqual(summary.incident_token, token)
        self.assertFalse(timing.is_queue_incident)
        self.assertTrue(timing.is_queue_timing_breakdown)
        self.assertEqual(timing.timing_token, token)
        self.assertFalse(motion.is_queue_incident)
        self.assertFalse(motion.is_queue_timing_breakdown)
        self.assertTrue(motion.is_queue_motion_batch)
        self.assertEqual(motion.motion_token, token)
        self.assertEqual(motion.motion_history_size, history_size)
        self.assertEqual(
            motion.motion_crossed_lane_count, crossed_lanes
        )
        self.assertEqual(
            motion.motion_history_span_nanos,
            history_span_units * MOTION_HISTORY_SPAN_UNIT_NANOS,
        )

        telemetry = QueueTelemetry()
        telemetry.observe(
            event(
                ACTION_DOWN,
                1,
                0.25,
                0.5,
                phone_event_nanos=2_000_000_000,
            )
        )
        telemetry.observe(summary, delivery_excess_ms=8.2)
        telemetry.observe(timing, delivery_excess_ms=0.4)
        telemetry.observe(motion)
        snapshot = telemetry.snapshot()
        self.assertEqual(len(snapshot.incidents), 1)
        diagnosed = snapshot.incidents[0]
        self.assertAlmostEqual(diagnosed.input_dispatch_ms, 1.5)
        self.assertAlmostEqual(diagnosed.app_processing_ms, 0.5)
        self.assertAlmostEqual(diagnosed.queue_residence_ms, 7.25)
        self.assertAlmostEqual(diagnosed.usb_write_ms, 0.3)
        self.assertAlmostEqual(
            diagnosed.post_write_delivery_excess_ms, 0.4
        )
        self.assertEqual(diagnosed.history_size, 2)
        self.assertAlmostEqual(diagnosed.history_span_ms, 8.0)
        self.assertEqual(diagnosed.crossed_lane_count, 3)
        self.assertAlmostEqual(snapshot.max_input_dispatch_ms, 1.5)
        self.assertAlmostEqual(snapshot.max_app_processing_ms, 0.5)
        self.assertAlmostEqual(snapshot.max_queue_residence_ms, 7.25)
        self.assertAlmostEqual(snapshot.max_usb_write_ms, 0.3)
        self.assertEqual(snapshot.incidents_with_history, 1)
        self.assertEqual(snapshot.max_history_size, 2)
        self.assertAlmostEqual(snapshot.max_history_span_ms, 8.0)
        self.assertEqual(snapshot.max_crossed_lane_count, 3)
        self.assertEqual(
            timing.timing_input_dispatch_nanos,
            60 * TIMING_DURATION_UNIT_NANOS,
        )

    def test_queue_incident_decodes_capacity_and_saturated_write(self):
        detail = (
            INCIDENT_REASON_CAPACITY << INCIDENT_DETAIL_REASON_SHIFT
        ) | 0x3FFF
        packet = TOUCH_PACKET.pack(
            TOUCH_MAGIC,
            doctor_codes.TOUCH_PROTOCOL_VERSION,
            ACTION_HEARTBEAT,
            255,
            FLAG_QUEUE_DIAGNOSTICS
            | FLAG_QUEUE_INCIDENT
            | FLAG_INCIDENT_WRITER_BLOCKED,
            32767,
            0,
            13,
            (6_000_000_000 & ~0xFFFF) | detail,
        )

        incident = list(TouchPacketParser().feed(packet))[0]
        self.assertEqual(
            incident.incident_reason, INCIDENT_REASON_CAPACITY
        )
        self.assertAlmostEqual(
            incident.incident_write_age_nanos / 1_000_000.0,
            327.66,
        )

    def test_queue_warning_is_timed_from_first_stroke(self):
        telemetry = QueueTelemetry()
        telemetry.observe(
            event(
                ACTION_DOWN,
                1,
                0.25,
                0.5,
                phone_event_nanos=2_000_000_000,
            )
        )
        telemetry.observe(
            event(
                ACTION_HEARTBEAT,
                0,
                0.0,
                0.0,
                flags=FLAG_QUEUE_DIAGNOSTICS | FLAG_QUEUE_WARNING,
                phone_event_nanos=5_250_000_000,
            )
        )
        telemetry.observe(
            event(
                ACTION_HEARTBEAT,
                0,
                0.0,
                0.0,
                flags=FLAG_QUEUE_DIAGNOSTICS | FLAG_QUEUE_WARNING,
                phone_event_nanos=8_500_000_000,
            )
        )

        snapshot = telemetry.snapshot()
        self.assertEqual(snapshot.warning_reports, 2)
        self.assertEqual(
            snapshot.warning_reports_from_first_stroke_s,
            (3.25, 6.5),
        )

        telemetry.begin_epoch(recovered=False)
        self.assertEqual(
            telemetry.snapshot().warning_reports_from_first_stroke_s,
            (),
        )

    def test_session_reset_marker_starts_a_recovered_epoch(self):
        packet = TOUCH_PACKET.pack(
            TOUCH_MAGIC,
            doctor_codes.TOUCH_PROTOCOL_VERSION,
            ACTION_CANCEL,
            0,
            FLAG_SESSION_RESET | FLAG_HOST_RECOVERY,
            0,
            0,
            10,
            456,
        )
        parsed = list(TouchPacketParser().feed(packet))
        self.assertEqual(len(parsed), 1)
        marker = parsed[0]
        self.assertTrue(marker.session_reset)
        self.assertTrue(marker.host_recovery)

        telemetry = QueueTelemetry()
        telemetry.begin_epoch(marker.host_recovery)
        snapshot = telemetry.snapshot()
        self.assertEqual(snapshot.reports, 0)
        self.assertEqual(snapshot.host_recoveries, 1)


class LatencyTests(unittest.TestCase):
    def test_estimates_delivery_excess_for_incident_record(self):
        latency = ClockNormalizedLatency()
        latency.observe(100_000_000, 1_100_000_000, False)
        latency.observe(200_000_000, 1_205_000_000, False)

        self.assertAlmostEqual(
            latency.estimate_excess_ms(
                200_000_000, 1_205_000_000
            ),
            5.0,
        )

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
        self.assertAlmostEqual(snapshot.p50_excess_ms, 15.0)
        self.assertAlmostEqual(snapshot.p90_excess_ms, 19.0)
        self.assertAlmostEqual(snapshot.p95_excess_ms, 19.5)
        self.assertAlmostEqual(snapshot.p99_excess_ms, 19.9)
        self.assertAlmostEqual(snapshot.p99_9_excess_ms, 19.99)

    def test_clock_normalization_reports_tail_percentiles(self):
        latency = ClockNormalizedLatency()
        start_phone = 100_000_000_000
        host_epoch = 5_000_000_000_000
        for delay_ms in range(1_001):
            phone = start_phone + delay_ms * 1_000_000
            latency.observe(
                phone,
                host_epoch + phone + delay_ms * 1_000_000,
                True,
            )

        snapshot = latency.snapshot()
        self.assertEqual(snapshot.samples, 1_001)
        self.assertAlmostEqual(snapshot.p50_excess_ms, 500.0)
        self.assertAlmostEqual(snapshot.p90_excess_ms, 900.0)
        self.assertAlmostEqual(snapshot.p95_excess_ms, 950.0)
        self.assertAlmostEqual(snapshot.p99_excess_ms, 990.0)
        self.assertAlmostEqual(snapshot.p99_9_excess_ms, 999.0)

    def test_clock_normalization_removes_long_session_clock_skew(self):
        latency = ClockNormalizedLatency()
        start_phone = 100_000_000_000
        host_epoch = 5_000_000_000_000
        interval_nanos = 10_000_000
        skew = 25 / 1_000_000

        for index in range(7_000):
            elapsed = index * interval_nanos
            phone = start_phone + elapsed
            host = (
                host_epoch
                + phone
                + 4_000_000
                + int(elapsed * skew)
            )
            latency.observe(phone, host, True)

        snapshot = latency.snapshot()
        self.assertEqual(snapshot.session_samples, 7_000)
        self.assertGreater(snapshot.samples, 5_900)
        self.assertLess(snapshot.mean_excess_ms, 0.001)
        self.assertLess(snapshot.max_excess_ms, 0.001)
        self.assertAlmostEqual(snapshot.window_seconds, 60.0, places=2)

    def test_clock_normalization_keeps_recent_latency_spike(self):
        latency = ClockNormalizedLatency()
        start_phone = 100_000_000_000
        host_epoch = 5_000_000_000_000
        interval_nanos = 10_000_000
        skew = -30 / 1_000_000

        for index in range(6_001):
            elapsed = index * interval_nanos
            phone = start_phone + elapsed
            spike = 8_000_000 if index == 6_000 else 0
            host = (
                host_epoch
                + phone
                + 4_000_000
                + int(elapsed * skew)
                + spike
            )
            latency.observe(phone, host, True)

        snapshot = latency.snapshot()
        self.assertGreater(snapshot.max_excess_ms, 7.99)
        self.assertLess(snapshot.max_excess_ms, 8.01)

    def test_clock_normalization_bounds_history_to_recent_window(self):
        latency = ClockNormalizedLatency()
        start_phone = 100_000_000_000
        for second in range(90):
            phone = start_phone + second * 1_000_000_000
            early_delay = 100_000_000 if second == 0 else 0
            latency.observe(
                phone,
                5_000_000_000_000 + phone + early_delay,
                True,
            )

        snapshot = latency.snapshot()
        self.assertEqual(snapshot.session_samples, 90)
        self.assertEqual(snapshot.samples, 61)
        self.assertAlmostEqual(snapshot.window_seconds, 60.0)
        self.assertAlmostEqual(snapshot.max_excess_ms, 0.0)

    def test_clock_normalization_is_session_length_invariant(self):
        def run_session(seconds):
            latency = ClockNormalizedLatency()
            start_phone = 100_000_000_000
            interval_nanos = 100_000_000
            for index in range(seconds * 10):
                elapsed = index * interval_nanos
                phone = start_phone + elapsed
                jitter = index % 10 * 200_000
                host = (
                    5_000_000_000_000
                    + phone
                    + 4_000_000
                    + int(elapsed * 25 / 1_000_000)
                    + jitter
                )
                latency.observe(phone, host, True)
            return latency.snapshot()

        short = run_session(70)
        long = run_session(3_600)
        self.assertAlmostEqual(
            short.mean_excess_ms, long.mean_excess_ms, places=3
        )
        self.assertAlmostEqual(
            short.max_excess_ms, long.max_excess_ms, places=3
        )


class AoaHostTests(unittest.TestCase):
    @staticmethod
    def make_host(*find_results, prefer_usbdk=True):
        host = object.__new__(AoaHost)
        host.prefer_usbdk = prefer_usbdk
        host.usb = type("Usb", (), {"using_usbdk": False})()

        def replace_usb(use_usbdk=False):
            host.usb = type(
                "Usb", (), {"using_usbdk": bool(use_usbdk)}
            )()

        host._replace_usb = mock.Mock(side_effect=replace_usb)
        host._find_data_accessory = mock.Mock(
            side_effect=list(find_results),
        )
        return host

    def test_data_connection_prefers_winusb_without_enabling_fallback(self):
        connection = object()
        host = self.make_host(connection)

        self.assertIs(host._wait_for_data_accessory(0.1), connection)
        host._replace_usb.assert_not_called()

    def test_data_connection_falls_back_to_usbdk_after_winusb_error(self):
        connection = object()
        host = self.make_host(
            AoaError("LIBUSB_ERROR_NOT_SUPPORTED"),
            connection,
        )

        self.assertIs(host._wait_for_data_accessory(0.1), connection)
        host._replace_usb.assert_called_once_with(use_usbdk=True)
        self.assertTrue(host.usb.using_usbdk)

    def test_data_connection_falls_back_after_winusb_attach_grace(self):
        connection = object()
        host = self.make_host(None, connection)

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
        host = self.make_host(prefer_usbdk=False)

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

    def test_extended_hitbox_clamps_outer_lanes_and_ignores_height(self):
        self.router.handle(event(ACTION_DOWN, 1, 0.2, 0.5, 1))
        self.router.handle(
            event(ACTION_MOVE, 1, -0.1, 0.5, 2, FLAG_LOCKED)
        )
        self.router.handle(
            event(ACTION_MOVE, 1, 1.1, -2.0, 3, FLAG_LOCKED)
        )
        self.router.handle(
            event(ACTION_MOVE, 1, 0.2, 2.0, 4, FLAG_LOCKED)
        )
        self.router.handle(event(ACTION_CANCEL, 0, 0, 0, 5, 0))
        self.assertEqual(self.down, ["a", "b", "a"])
        self.assertEqual(self.up, ["a", "b", "a"])

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
            ("AOA session reset marker timed out; reconnecting", False),
            statuses,
        )


if __name__ == "__main__":
    unittest.main()
