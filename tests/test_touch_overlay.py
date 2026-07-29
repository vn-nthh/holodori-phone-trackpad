import unittest

from touch_overlay import (
    MIN_OVERLAY_HEIGHT,
    MIN_OVERLAY_WIDTH,
    _OverlayMailbox,
    resize_geometry,
)


class ResizeGeometryTests(unittest.TestCase):
    def test_resize_geometry(self):
        geometry = (100, 200, 600, 180)
        cases = (
            ("se", 80, 40, (100, 200, 680, 220)),
            ("nw", 50, 30, (150, 230, 550, 150)),
            (
                "nw",
                1000,
                1000,
                (380, 280, MIN_OVERLAY_WIDTH, MIN_OVERLAY_HEIGHT),
            ),
            ("w", -75, 999, (25, 200, 675, 180)),
        )
        for edge, dx, dy, expected in cases:
            with self.subTest(edge=edge, dx=dx, dy=dy):
                self.assertEqual(
                    resize_geometry(edge, geometry, dx, dy),
                    expected,
                )


class OverlayMailboxTests(unittest.TestCase):
    def test_moves_are_coalesced_to_latest_pointer_state(self):
        mailbox = _OverlayMailbox()
        for index in range(100_000):
            mailbox.publish_touch(
                index % 2,
                index / 100_000,
                0.5,
                True,
                False,
            )

        cancelled, touches, status = mailbox.drain()
        self.assertFalse(cancelled)
        self.assertIsNone(status)
        self.assertEqual(set(touches), {0, 1})
        self.assertAlmostEqual(touches[0][0], 0.99998)
        self.assertAlmostEqual(touches[1][0], 0.99999)

    def test_cancel_discards_older_touches_but_keeps_newer_state(self):
        mailbox = _OverlayMailbox()
        mailbox.publish_touch(1, 0.1, 0.2, True, False)
        mailbox.publish_cancel()
        mailbox.publish_touch(2, 0.8, 0.7, True, False)

        cancelled, touches, _ = mailbox.drain()
        self.assertTrue(cancelled)
        self.assertEqual(touches, {2: (0.8, 0.7, True, False)})

    def test_status_is_coalesced_and_drain_clears_pending_state(self):
        mailbox = _OverlayMailbox()
        mailbox.publish_status("Looking", False)
        mailbox.publish_status("Connected", True)

        _, _, status = mailbox.drain()
        self.assertEqual(status, ("Connected", True))
        self.assertEqual(mailbox.drain(), (False, {}, None))
