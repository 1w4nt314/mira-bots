#!/usr/bin/env python3
"""Generates the placeholder mira-bots icons (pure python: struct + zlib only).

Output in src-tauri/icons/: icon.png (512), 32x32.png, 128x128.png, 128x128@2x.png (256)
and icon.ico with uncompressed 32-bpp BMP entries (16, 32, 48, 256).

Design: dark rounded square with an amber pill near the top edge (the "island"). No letters.
Run from anywhere: python3 scripts/gen-icons.py
"""
import os
import struct
import zlib

BG = (0x1F, 0x29, 0x37)      # #1f2937
ACCENT = (0xF5, 0x9E, 0x0B)  # #f59e0b
SS = 4                       # supersampling per axis


def rrect_cov(px, py, x0, y0, x1, y1, r):
    """1.0 if the point is inside the rounded rectangle, else 0.0."""
    if px < x0 or px > x1 or py < y0 or py > y1:
        return 0.0
    cx = min(max(px, x0 + r), x1 - r)
    cy = min(max(py, y0 + r), y1 - r)
    return 1.0 if (px - cx) ** 2 + (py - cy) ** 2 <= r * r else 0.0


def render(size):
    """Returns a list of rows, each a list of (r, g, b, a) tuples."""
    s = float(size)
    bg = (0.0, 0.0, s, s, 0.22 * s)
    pill = (0.22 * s, 0.15 * s, 0.78 * s, 0.27 * s, 0.06 * s)
    rows = []
    for y in range(size):
        row = []
        for x in range(size):
            acc_a = acc_r = acc_g = acc_b = 0.0
            for sy in range(SS):
                for sx in range(SS):
                    px = x + (sx + 0.5) / SS
                    py = y + (sy + 0.5) / SS
                    if rrect_cov(px, py, *bg) == 0.0:
                        continue
                    col = ACCENT if rrect_cov(px, py, *pill) else BG
                    acc_a += 1.0
                    acc_r += col[0]
                    acc_g += col[1]
                    acc_b += col[2]
            n = SS * SS
            if acc_a == 0:
                row.append((0, 0, 0, 0))
            else:
                row.append((round(acc_r / acc_a), round(acc_g / acc_a),
                            round(acc_b / acc_a), round(255 * acc_a / n)))
        rows.append(row)
    return rows


def png_bytes(rows):
    h = len(rows)
    w = len(rows[0])
    raw = bytearray()
    for row in rows:
        raw.append(0)  # filter: none
        for r, g, b, a in row:
            raw += bytes((r, g, b, a))

    def chunk(tag, data):
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
            + chunk(b"IEND", b""))


def bmp_entry(rows):
    """ICO image entry: BITMAPINFOHEADER + bottom-up BGRA pixels + 1-bpp AND mask."""
    h = len(rows)
    w = len(rows[0])
    header = struct.pack("<IiiHHIIiiII", 40, w, h * 2, 1, 32, 0, 0, 0, 0, 0, 0)
    pixels = bytearray()
    for row in reversed(rows):
        for r, g, b, a in row:
            pixels += bytes((b, g, r, a))
    mask_row = ((w + 31) // 32) * 4
    mask = bytes(mask_row * h)  # all zero: alpha channel decides transparency
    return header + bytes(pixels) + mask


def ico_bytes(sizes):
    entries = [(sz, bmp_entry(render(sz))) for sz in sizes]
    out = bytearray(struct.pack("<HHH", 0, 1, len(entries)))
    offset = 6 + 16 * len(entries)
    for sz, data in entries:
        dim = 0 if sz >= 256 else sz
        out += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    for _, data in entries:
        out += data
    return bytes(out)


def main():
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    out_dir = os.path.join(root, "src-tauri", "icons")
    os.makedirs(out_dir, exist_ok=True)
    files = {
        "icon.png": png_bytes(render(512)),
        "32x32.png": png_bytes(render(32)),
        "128x128.png": png_bytes(render(128)),
        "128x128@2x.png": png_bytes(render(256)),
        "icon.ico": ico_bytes([16, 32, 48, 256]),
    }
    for name, data in files.items():
        with open(os.path.join(out_dir, name), "wb") as f:
            f.write(data)
        print("wrote", os.path.join(out_dir, name), len(data), "bytes")


if __name__ == "__main__":
    main()
