import struct
import unittest

from aoa_mode import AoaTouchRouter
from aoa_transport import (
    ACTION_CANCEL,
    ACTION_DOWN,
    ACTION_MOVE,
    ACTION_UP,
    FLAG_INSIDE,
    FLAG_LOCKED,
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


if __name__ == "__main__":
    unittest.main()
