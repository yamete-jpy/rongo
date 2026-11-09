#!/usr/bin/env python3
"""Draw rongo's app icon with the standard library only.

A tilted green leaf with a midrib and a stem on a dark rounded square, drawn
with signed distance fields for smooth edges. Writes:

  assets/icon-1024.png   source for the .icns (scripts/bundle-macos.sh)
  assets/icon-256.rgba   raw RGBA the app sets as its window/Dock icon

Usage: python3 scripts/make-icon.py
"""

import math
import os
import struct
import zlib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ASSETS = os.path.join(ROOT, "assets")


def mix(a, b, t):
    return tuple(x + (y - x) * t for x, y in zip(a, b))


def hexrgb(h):
    h = h.lstrip("#")
    return tuple(int(h[i:i + 2], 16) / 255 for i in (0, 2, 4))


BG_TOP, BG_BOTTOM = hexrgb("#24322b"), hexrgb("#121a16")
LEAF_LIGHT, LEAF_DARK = hexrgb("#7ccf7f"), hexrgb("#3e9b52")
RIB = hexrgb("#2b6f3a")

# Apple's icon grid: an 824/1024 rounded square with a ~185 corner radius.
BOX_HALF, BOX_RADIUS = 412 / 1024, 185 / 1024

# The leaf is the lens where two circles overlap, along an axis tilted 45°.
LEAF_LEN, LEAF_WIDTH = 0.50, 0.27
LEAF_CENTER = (0.53, 0.47)
ANGLE = math.radians(-45)
R = ((LEAF_LEN / 2) ** 2 + (LEAF_WIDTH / 2) ** 2) / LEAF_WIDTH
C = R - LEAF_WIDTH / 2


def rounded_box(x, y):
    qx, qy = abs(x - 0.5) - BOX_HALF + BOX_RADIUS, abs(y - 0.5) - BOX_HALF + BOX_RADIUS
    outside = math.hypot(max(qx, 0), max(qy, 0))
    return outside + min(max(qx, qy), 0) - BOX_RADIUS


def to_leaf(x, y):
    """Coordinates along (u) and across (v) the leaf's axis."""
    dx, dy = x - LEAF_CENTER[0], y - LEAF_CENTER[1]
    ca, sa = math.cos(ANGLE), math.sin(ANGLE)
    return dx * ca - dy * sa, dx * sa + dy * ca


def leaf(u, v):
    return max(math.hypot(u, v - C) - R, math.hypot(u, v + C) - R)


def segment(u, v, u0, u1, half_thickness):
    t = min(max(u, u0), u1)
    return math.hypot(u - t, v) - half_thickness


def cover(d, px):
    """Fraction of a pixel inside a shape at signed distance d."""
    return min(max(0.5 - d * px, 0.0), 1.0)


def over(dst, color, alpha):
    r, g, b, a = dst
    out_a = alpha + a * (1 - alpha)
    if out_a == 0:
        return (0.0, 0.0, 0.0, 0.0)
    blend = lambda s, d: (s * alpha + d * a * (1 - alpha)) / out_a
    return (blend(color[0], r), blend(color[1], g), blend(color[2], b), out_a)


def render(size):
    px = size
    rows = []
    for j in range(size):
        y = (j + 0.5) / size
        row = bytearray()
        for i in range(size):
            x = (i + 0.5) / size
            p = (0.0, 0.0, 0.0, 0.0)
            box = rounded_box(x, y)
            if box * px < 1:
                p = over(p, mix(BG_TOP, BG_BOTTOM, y), cover(box, px))
                u, v = to_leaf(x, y)
                # Stem: from the leaf's base outward, slightly curved.
                stem = segment(u + v * v * 0.6, v, -LEAF_LEN / 2 - 0.10, -LEAF_LEN / 2 + 0.02, 0.012)
                p = over(p, LEAF_DARK, cover(stem, px))
                d = leaf(u, v)
                if d * px < 1:
                    shade = mix(LEAF_LIGHT, LEAF_DARK, min(max((u + LEAF_LEN / 2) / LEAF_LEN * 0.5 + (v + LEAF_WIDTH / 2) / LEAF_WIDTH * 0.5, 0), 1))
                    p = over(p, shade, cover(d, px))
                    inside = cover(d + 0.025, px)
                    rib = segment(u, v, -LEAF_LEN / 2 + 0.03, LEAF_LEN / 2 - 0.05, 0.007)
                    p = over(p, RIB, cover(rib, px) * inside)
                    # Three pairs of side veins, angled toward the tip.
                    for k in range(3):
                        u0 = -LEAF_LEN / 2 + 0.11 + k * 0.09
                        for side in (1, -1):
                            vu, vv = u - u0, v * side
                            if vu > -0.01 and vv > -0.01:
                                du, dv = 0.55, 0.835  # direction of a vein
                                t = min(max(vu * du + vv * dv, 0), 0.09)
                                vein = math.hypot(vu - du * t, vv - dv * t) - 0.004
                                p = over(p, RIB, cover(vein, px) * inside * 0.9)
            row += bytes(round(c * 255) for c in p)
        rows.append(bytes(row))
    return rows


def write_png(path, size, rows):
    raw = b"".join(b"\x00" + r for r in rows)
    chunk = lambda tag, data: struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
    png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b"")
    with open(path, "wb") as f:
        f.write(png)


def main():
    os.makedirs(ASSETS, exist_ok=True)
    write_png(os.path.join(ASSETS, "icon-1024.png"), 1024, render(1024))
    with open(os.path.join(ASSETS, "icon-256.rgba"), "wb") as f:
        f.write(b"".join(render(256)))
    print("wrote assets/icon-1024.png and assets/icon-256.rgba")


if __name__ == "__main__":
    main()
