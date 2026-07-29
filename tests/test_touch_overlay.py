import unittest

from touch_overlay import (
    MIN_OVERLAY_HEIGHT,
    MIN_OVERLAY_WIDTH,
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
