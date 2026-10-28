"""Standard-library image tooling for the GUI journeys (no Pillow/numpy).

Frames come from the renderer's test hook (`frame.ppm`, see src/frame_dump.rs);
OS screenshots are PNG. Everything is 8-bit RGB in memory.
"""
from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
import struct
import zlib

META_PREFIX = b"# crow-frame "


@dataclass
class Image:
    width: int
    height: int
    rgb: bytearray  # row-major, 3 bytes per pixel

    def __post_init__(self):
        if len(self.rgb) != self.width * self.height * 3:
            raise ValueError(f"{self.width}x{self.height} image needs {self.width * self.height * 3} bytes, got {len(self.rgb)}")

    def pixel(self, x, y):
        i = (y * self.width + x) * 3
        return self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]

    def crop(self, x, y, w, h):
        x0, y0 = max(0, x), max(0, y)
        x1, y1 = min(self.width, x + w), min(self.height, y + h)
        if x1 <= x0 or y1 <= y0:
            return Image(0, 0, bytearray())
        out = bytearray()
        for row in range(y0, y1):
            start = (row * self.width + x0) * 3
            out += self.rgb[start:start + (x1 - x0) * 3]
        return Image(x1 - x0, y1 - y0, out)

    def row_bytes(self, y):
        start = y * self.width * 3
        return bytes(self.rgb[start:start + self.width * 3])


# ---------------------------------------------------------------- frame dumps

def read_frame(path):
    """Return (Image, metadata dict) from the renderer's frame.ppm."""
    data = Path(path).read_bytes()
    return parse_ppm(data)


def parse_ppm(data):
    if not data.startswith(b"P6\n"):
        raise ValueError("not a binary PPM")
    pos = 3
    meta = {}
    tokens = []
    while len(tokens) < 3:
        end = data.index(b"\n", pos)
        line = data[pos:end]
        pos = end + 1
        if line.startswith(META_PREFIX):
            meta = json.loads(line[len(META_PREFIX):])
        elif line.startswith(b"#"):
            continue
        else:
            tokens += line.split()
    width, height, maximum = map(int, tokens)
    if maximum != 255:
        raise ValueError("only 8-bit PPM is supported")
    pixels = bytearray(data[pos:pos + width * height * 3])
    return Image(width, height, pixels), meta


# ----------------------------------------------------------------------- PNG

def _chunk(kind, payload):
    return struct.pack(">I", len(payload)) + kind + payload + struct.pack(">I", zlib.crc32(kind + payload) & 0xFFFFFFFF)


def encode_png(image):
    raw = bytearray()
    stride = image.width * 3
    for y in range(image.height):
        raw.append(0)
        raw += image.rgb[y * stride:(y + 1) * stride]
    header = struct.pack(">IIBBBBB", image.width, image.height, 8, 2, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + _chunk(b"IHDR", header) + _chunk(b"IDAT", zlib.compress(bytes(raw), 9)) + _chunk(b"IEND", b"")


def write_png(path, image):
    Path(path).write_bytes(encode_png(image))


def _paeth(a, b, c):
    p = a + b - c
    pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
    if pa <= pb and pa <= pc:
        return a
    return b if pb <= pc else c


def decode_png(data):
    """8/16-bit grey, RGB, grey+alpha or RGBA, non-interlaced; alpha is dropped
    (screenshots are opaque). Palette images are refused explicitly."""
    if not data.startswith(b"\x89PNG\r\n\x1a\n"):
        raise ValueError("not a PNG")
    pos = 8
    idat = bytearray()
    header = None
    while pos < len(data):
        (length,) = struct.unpack(">I", data[pos:pos + 4])
        kind = data[pos + 4:pos + 8]
        payload = data[pos + 8:pos + 8 + length]
        pos += 12 + length
        if kind == b"IHDR":
            header = struct.unpack(">IIBBBBB", payload)
        elif kind == b"IDAT":
            idat += payload
        elif kind == b"IEND":
            break
    if header is None:
        raise ValueError("PNG without IHDR")
    width, height, depth, color, _, _, interlace = header
    channels = {0: 1, 2: 3, 4: 2, 6: 4}.get(color)
    if channels is None or depth not in (8, 16) or interlace:
        raise ValueError(f"unsupported PNG (color {color}, depth {depth}, interlace {interlace})")
    bpp = channels * depth // 8
    stride = width * bpp
    raw = zlib.decompress(bytes(idat))
    previous = bytearray(stride)
    out = bytearray(width * height * 3)
    offset = 0
    for y in range(height):
        kind = raw[offset]
        line = bytearray(raw[offset + 1:offset + 1 + stride])
        offset += 1 + stride
        if kind == 1:
            for i in range(bpp, stride):
                line[i] = (line[i] + line[i - bpp]) & 0xFF
        elif kind == 2:
            for i in range(stride):
                line[i] = (line[i] + previous[i]) & 0xFF
        elif kind == 3:
            for i in range(stride):
                left = line[i - bpp] if i >= bpp else 0
                line[i] = (line[i] + ((left + previous[i]) >> 1)) & 0xFF
        elif kind == 4:
            for i in range(stride):
                left = line[i - bpp] if i >= bpp else 0
                up_left = previous[i - bpp] if i >= bpp else 0
                line[i] = (line[i] + _paeth(left, previous[i], up_left)) & 0xFF
        elif kind != 0:
            raise ValueError(f"bad PNG filter {kind}")
        step = depth // 8
        base = y * width * 3
        for x in range(width):
            px = x * bpp
            if channels in (1, 2):
                value = line[px]
                out[base + x * 3:base + x * 3 + 3] = bytes((value, value, value))
            else:
                out[base + x * 3] = line[px]
                out[base + x * 3 + 1] = line[px + step]
                out[base + x * 3 + 2] = line[px + 2 * step]
        previous = line
    return Image(width, height, out)


def read_png(path):
    return decode_png(Path(path).read_bytes())


# ------------------------------------------------------------------ analysis

def channel_delta(a, b):
    return max(abs(a[0] - b[0]), abs(a[1] - b[1]), abs(a[2] - b[2]))


def compare(actual, golden, channel_tolerance=24, max_fraction=0.003, cell=None, max_cell_fraction=0.02):
    """Tolerant comparison. Returns (ok, report dict, diff Image).

    A pixel differs when any channel moves more than `channel_tolerance`. The
    frame passes when at most `max_fraction` of pixels differ and, if `cell`
    (width, height) is given, no cell has more than `max_cell_fraction` of its
    pixels different (a whole wrong glyph fails even inside the global budget).
    """
    if (actual.width, actual.height) != (golden.width, golden.height):
        return False, {"reason": f"size {actual.width}x{actual.height} != golden {golden.width}x{golden.height}"}, None
    diff = bytearray(len(actual.rgb))
    differing = 0
    worst = 0
    cells = {}
    a, g = actual.rgb, golden.rgb
    width = actual.width
    for i in range(0, len(a), 3):
        delta = max(abs(a[i] - g[i]), abs(a[i + 1] - g[i + 1]), abs(a[i + 2] - g[i + 2]))
        if delta > channel_tolerance:
            differing += 1
            diff[i] = 255
            if cell:
                p = i // 3
                key = ((p % width) // cell[0], (p // width) // cell[1])
                cells[key] = cells.get(key, 0) + 1
        else:
            # Faded copy of the golden for context.
            grey = (g[i] + g[i + 1] + g[i + 2]) // 3
            diff[i] = diff[i + 1] = diff[i + 2] = 192 + grey // 4
        worst = max(worst, delta)
    total = actual.width * actual.height
    fraction = differing / total if total else 0.0
    worst_cell = 0.0
    if cell and cells:
        worst_cell = max(cells.values()) / (cell[0] * cell[1])
    ok = fraction <= max_fraction and worst_cell <= max_cell_fraction
    report = {
        "differing_pixels": differing,
        "fraction": round(fraction, 6),
        "max_channel_delta": worst,
        "worst_cell_fraction": round(worst_cell, 4),
        "thresholds": {"channel": channel_tolerance, "fraction": max_fraction, "cell": max_cell_fraction},
    }
    return ok, report, Image(actual.width, actual.height, diff)


def cell_box(meta, pane_index, row, col, span=1):
    """Physical-pixel rectangle (x, y, w, h) of a grid cell in the frame."""
    pane = meta["panes"][pane_index]
    cw, ch = meta["cell_width"], meta["cell_height"]
    return pane["x"] + col * cw, pane["y"] + row * ch, cw * span, ch


def ink(image, box, background, tolerance=40):
    """Fraction of pixels in `box` that differ from `background`."""
    region = image.crop(*box)
    total = region.width * region.height
    if not total:
        return 0.0
    rgb = region.rgb
    hits = sum(1 for i in range(0, len(rgb), 3) if channel_delta(rgb[i:i + 3], background) > tolerance)
    return hits / total


def colorful(image, box, spread=48):
    """Fraction of saturated pixels (max-min channel spread). The Crowquill Ink
    theme is strictly greyscale, so colour in a cell comes from a colour glyph."""
    region = image.crop(*box)
    total = region.width * region.height
    if not total:
        return 0.0
    rgb = region.rgb
    hits = sum(1 for i in range(0, len(rgb), 3) if max(rgb[i:i + 3]) - min(rgb[i:i + 3]) > spread)
    return hits / total


def same_pixels(a, b, tolerance=24):
    if (a.width, a.height) != (b.width, b.height):
        return False
    return all(abs(x - y) <= tolerance for x, y in zip(a.rgb, b.rgb))


def mean_delta(a, b):
    if (a.width, a.height) != (b.width, b.height) or not a.rgb:
        return 255.0
    return sum(abs(x - y) for x, y in zip(a.rgb, b.rgb)) / len(a.rgb)


def hstack(*images):
    images = [image for image in images if image.width]
    width = sum(image.width for image in images)
    height = max(image.height for image in images)
    out = bytearray([255]) * (width * height * 3)
    x = 0
    for image in images:
        for y in range(image.height):
            start = (y * width + x) * 3
            out[start:start + image.width * 3] = image.row_bytes(y)
        x += image.width
    return Image(width, height, out)


def find_patch(screen, patch, tolerance=0):
    """Top-left (x, y) of `patch` inside `screen`, or None.

    Exact mode uses bytes.find per row (fast; X11/Wayland/Windows screenshots are
    lossless). With a tolerance (colour-managed macOS captures) it anchors on the
    patch's first row and verifies sampled rows.
    """
    if patch.width > screen.width or patch.height > screen.height or not patch.width:
        return None
    first = patch.row_bytes(0)
    for y in range(screen.height - patch.height + 1):
        row = screen.row_bytes(y)
        if tolerance == 0:
            start = row.find(first)
            while start != -1:
                if start % 3 == 0 and all(
                    screen.row_bytes(y + dy)[start:start + len(first)] == patch.row_bytes(dy)
                    for dy in range(1, patch.height)
                ):
                    return start // 3, y
                start = row.find(first, start + 1)
        else:
            anchor = patch.pixel(0, 0)
            for x in range(screen.width - patch.width + 1):
                if channel_delta(screen.pixel(x, y), anchor) > tolerance:
                    continue
                if all(
                    channel_delta(screen.pixel(x + dx, y + dy), patch.pixel(dx, dy)) <= tolerance
                    for dy in range(0, patch.height, 2)
                    for dx in range(0, patch.width, 3)
                ):
                    return x, y
    return None
