#!/usr/bin/env python3
"""Unit tests for the GUI journey tooling (no display needed): PPM/PNG codecs,
tolerant comparison, patch search and grid lookup."""
from pathlib import Path
import struct
import sys
import unittest
import zlib

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent / "gui"))
import harness  # noqa: E402
import imaging  # noqa: E402


def solid(width, height, color):
    return imaging.Image(width, height, bytearray(bytes(color) * (width * height)))


def paint(image, x, y, w, h, color):
    for row in range(y, y + h):
        for col in range(x, x + w):
            i = (row * image.width + col) * 3
            image.rgb[i:i + 3] = bytes(color)


class Codecs(unittest.TestCase):
    def test_ppm_metadata_round_trip(self):
        data = b'P6\n# crow-frame {"seq": 4, "cell_width": 11}\n2 1\n255\n' + bytes([1, 2, 3, 4, 5, 6])
        image, meta = imaging.parse_ppm(data)
        self.assertEqual((image.width, image.height, meta["seq"]), (2, 1, 4))
        self.assertEqual(image.pixel(1, 0), (4, 5, 6))

    def test_png_round_trip_and_filtered_rgba(self):
        image = solid(5, 4, (10, 20, 30))
        paint(image, 1, 1, 2, 2, (200, 100, 50))
        self.assertEqual(imaging.decode_png(imaging.encode_png(image)).rgb, image.rgb)
        # RGBA with Sub/Up/Average/Paeth filters, as screenshot tools write them.
        rows = [bytes([1, 2, 3, 255, 4, 5, 6, 255]), bytes([7, 8, 9, 255, 10, 11, 12, 255])]
        raw = bytearray()
        previous = bytes(8)
        for kind, row in zip((1, 4), rows):
            raw.append(kind)
            for i, value in enumerate(row):
                left = row[i - 4] if i >= 4 else 0
                up = previous[i]
                up_left = previous[i - 4] if i >= 4 else 0
                predictor = left if kind == 1 else imaging._paeth(left, up, up_left)
                raw.append((value - predictor) & 0xFF)
            previous = row
        def chunk(kind, payload):
            return struct.pack(">I", len(payload)) + kind + payload + struct.pack(">I", zlib.crc32(kind + payload))
        png = (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", 2, 2, 8, 6, 0, 0, 0))
               + chunk(b"IDAT", zlib.compress(bytes(raw))) + chunk(b"IEND", b""))
        decoded = imaging.decode_png(png)
        self.assertEqual(list(decoded.rgb), [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12])

    def test_palette_png_is_refused(self):
        png = b"\x89PNG\r\n\x1a\n" + struct.pack(">I", 13) + b"IHDR" + struct.pack(">IIBBBBB", 1, 1, 8, 3, 0, 0, 0) + b"\0\0\0\0"
        with self.assertRaises(ValueError):
            imaging.decode_png(png)


class Comparison(unittest.TestCase):
    def test_tolerant_global_and_per_cell_budget(self):
        golden = solid(100, 100, (255, 255, 255))
        noisy = solid(100, 100, (240, 240, 240))  # within channel tolerance
        self.assertTrue(imaging.compare(noisy, golden)[0])
        one_glyph = solid(100, 100, (255, 255, 255))
        paint(one_glyph, 0, 0, 5, 5, (0, 0, 0))  # 25 px: 0.25% globally
        ok, report, _ = imaging.compare(one_glyph, golden)
        self.assertTrue(ok, report)
        ok, report, diff = imaging.compare(one_glyph, golden, cell=(10, 10))
        self.assertFalse(ok, "a wrong glyph fails its cell even inside the global budget")
        self.assertEqual(report["worst_cell_fraction"], 0.25)
        self.assertEqual(diff.pixel(0, 0)[0], 255)
        self.assertFalse(imaging.compare(solid(2, 2, (0, 0, 0)), solid(2, 3, (0, 0, 0)))[0])

    def test_ink_colour_and_patch_search(self):
        screen = solid(40, 30, (20, 20, 20))
        frame = solid(12, 8, (255, 255, 255))
        paint(frame, 2, 2, 3, 3, (0, 0, 0))
        for y in range(8):
            for x in range(12):
                i = ((y + 9) * 40 + x + 17) * 3
                screen.rgb[i:i + 3] = bytes(frame.pixel(x, y))
        self.assertEqual(imaging.find_patch(screen, frame), (17, 9))
        shifted = imaging.Image(screen.width, screen.height, bytearray(min(255, v + 10) for v in screen.rgb))
        self.assertIsNone(imaging.find_patch(shifted, frame))
        self.assertEqual(imaging.find_patch(shifted, frame, tolerance=12), (17, 9))
        self.assertAlmostEqual(imaging.ink(frame, (0, 0, 6, 6), (255, 255, 255)), 9 / 36)
        paint(frame, 8, 0, 2, 2, (220, 40, 40))
        self.assertEqual(imaging.colorful(frame, (8, 0, 4, 2)), 0.5)
        self.assertEqual(imaging.colorful(frame, (0, 0, 6, 6)), 0.0)


class Grid(unittest.TestCase):
    def cells(self, texts):
        return [{"text": t, "wide_spacer": t == ""} for t in texts]

    def test_locate_and_wide_columns(self):
        screen = {"cells": [self.cells(list("LIGA ->")), self.cells(["C", "J", "K", " ", "界", "", "x"])]}
        row, cells = harness.locate(screen, "CJK")
        self.assertEqual(row, 1)
        self.assertEqual(harness.column_of(cells, "界"), (4, 2))
        self.assertEqual(harness.column_of(screen["cells"][0], "->"), (5, 2))
        with self.assertRaises(AssertionError):
            harness.locate(screen, "EMOJI")


if __name__ == "__main__":
    unittest.main(verbosity=2)
