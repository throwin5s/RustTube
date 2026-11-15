#!/usr/bin/env python3
"""Generate RustTube's app icons without any third-party imaging library.

Draws a rounded-square tile with a warm gradient and a play glyph, then writes
the PNG sizes Tauri needs plus .ico and .icns containers.
"""

import math
import os
import struct
import subprocess
import shutil
import zlib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ICONS = os.path.join(ROOT, "icons")

# Palette: RustTube accent (ember orange) on a deep charcoal tile.
BG_TOP = (26, 27, 32)
BG_BOTTOM = (17, 18, 22)
ACCENT_A = (255, 138, 76)
ACCENT_B = (226, 60, 48)


def _lerp(a, b, t):
    return tuple(int(round(a[i] + (b[i] - a[i]) * t)) for i in range(3))


def _inside_rounded(x, y, size, radius):
    """True when point (x, y) is inside a rounded square inset by 0."""
    if x < 0 or y < 0 or x > size or y > size:
        return False
    cx = min(max(x, radius), size - radius)
    cy = min(max(y, radius), size - radius)
    return (x - cx) ** 2 + (y - cy) ** 2 <= radius ** 2


def _triangle(x, y, size):
    """Play glyph: isosceles triangle pointing right, centered."""
    # Barycentric-ish test against three vertices.
    ax, ay = size * 0.38, size * 0.30
    bx, by = size * 0.38, size * 0.70
    cx, cy = size * 0.73, size * 0.50
    d1 = (x - bx) * (ay - by) - (ax - bx) * (y - by)
    d2 = (x - cx) * (by - cy) - (bx - cx) * (y - cy)
    d3 = (x - ax) * (cy - ay) - (cx - ax) * (y - ay)
    has_neg = d1 < 0 or d2 < 0 or d3 < 0
    has_pos = d1 > 0 or d2 > 0 or d3 > 0
    return not (has_neg and has_pos)


def render(size, supersample=4):
    radius = size * 0.222
    rows = []
    inv = 1.0 / supersample
    for py in range(size):
        row = []
        for px in range(size):
            bg_hits = 0
            glyph_hits = 0
            for sy in range(supersample):
                for sx in range(supersample):
                    x = px + (sx + 0.5) * inv
                    y = py + (sy + 0.5) * inv
                    if not _inside_rounded(x, y, size, radius):
                        continue
                    bg_hits += 1
                    if _triangle(x, y, size):
                        glyph_hits += 1
            total = supersample * supersample
            if bg_hits == 0:
                row.append((0, 0, 0, 0))
                continue
            t = py / max(1, size - 1)
            base = _lerp(BG_TOP, BG_BOTTOM, t)
            # Accent glow behind the glyph, stronger near the center.
            cx, cy = size * 0.55, size * 0.5
            dist = math.hypot(px - cx, py - cy) / (size * 0.75)
            glow = max(0.0, 1.0 - dist) ** 2
            accent = _lerp(ACCENT_A, ACCENT_B, min(1.0, dist * 1.4))
            base = _lerp(base, accent, glow * 0.30)
            cover = glyph_hits / total
            color = _lerp(base, (255, 255, 255), cover)
            alpha = int(round(255 * bg_hits / total))
            row.append((color[0], color[1], color[2], alpha))
        rows.append(row)
    return rows


def write_png(path, size, rows):
    raw = bytearray()
    for row in rows:
        raw.append(0)
        for px in row:
            raw.extend(px)

    def chunk(kind, data):
        out = struct.pack(">I", len(data)) + kind + data
        return out + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + chunk(b"IEND", b"")
    )
    with open(path, "wb") as fh:
        fh.write(png)


def write_ico(path, png_bytes, size):
    """Vista-style .ico holding one PNG-compressed square image."""
    header = struct.pack("<HHH", 0, 1, 1)
    entry = struct.pack("<BBBBHHII", 0 if size >= 256 else size, 0 if size >= 256 else size, 0, 0, 1, 32, len(png_bytes), 22)
    with open(path, "wb") as fh:
        fh.write(header + entry + png_bytes)


def write_icns(path, files):
    """ICNS with modern PNG element types, built from generated PNGs."""
    mapping = [
        (b"icp4", 16),
        (b"icp5", 32),
        (b"icp6", 64),
        (b"ic07", 128),
        (b"ic08", 256),
        (b"ic09", 512),
        (b"ic10", 1024),
        (b"ic11", 32),
        (b"ic12", 64),
        (b"ic13", 256),
        (b"ic14", 512),
    ]
    body = b""
    for kind, size in mapping:
        data = files[size]
        body += kind + struct.pack(">I", len(data) + 8) + data
    with open(path, "wb") as fh:
        fh.write(b"icns" + struct.pack(">I", len(body) + 8) + body)


def main():
    os.makedirs(ICONS, exist_ok=True)
    sizes = [16, 32, 48, 64, 128, 256, 512, 1024]
    blobs = {}
    for size in sizes:
        rows = render(size, supersample=4 if size <= 256 else 2)
        blobs[size] = None
        tmp = os.path.join(ICONS, "icon_%d.png" % size)
        write_png(tmp, size, rows)
        with open(tmp, "rb") as fh:
            blobs[size] = fh.read()
        os.remove(tmp)
        write_png(os.path.join(ICONS, "%dx%d.png" % (size, size)), size, rows)

    write_png(os.path.join(ICONS, "icon.png"), 1024, render(1024, supersample=2))
    write_ico(os.path.join(ICONS, "icon.ico"), blobs[64], 64)
    write_icns(os.path.join(ICONS, "icon.icns"), blobs)
    # Tauri's conventional names.
    write_png(os.path.join(ICONS, "32x32.png"), 32, render(32))
    write_png(os.path.join(ICONS, "128x128.png"), 128, render(128))
    write_png(os.path.join(ICONS, "128x128@2x.png"), 256, render(256))
    print("icons written to", ICONS)


if __name__ == "__main__":
    main()
