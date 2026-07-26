import unittest

from touch_overlay import (
    MIN_OVERLAY_HEIGHT,
    MIN_OVERLAY_WIDTH,
    resize_geometry,
)


class ResizeGeometryTests(unittest.TestCase):
    def test_resize_southeast_grows_without_moving_origin(self):
        self.assertEqual(
            resize_geometry("se", (100, 200, 600, 180), 80, 40),
            (100, 200, 680, 220),
        )

    def test_resize_northwest_keeps_opposite_corner_fixed(self):
        self.assertEqual(
            resize_geometry("nw", (100, 200, 600, 180), 50, 30),
            (150, 230, 550, 150),
        )

    def test_resize_clamps_at_minimum_size(self):
        self.assertEqual(
            resize_geometry("nw", (100, 200, 600, 180), 1000, 1000),
            (380, 280, MIN_OVERLAY_WIDTH, MIN_OVERLAY_HEIGHT),
        )

    def test_resize_west_expands_left_and_preserves_right_edge(self):
        self.assertEqual(
            resize_geometry("w", (100, 200, 600, 180), -75, 999),
            (25, 200, 675, 180),
        )
