"""Draws dash.ico (16-256 px): the tray icon's blue disc with a white ring
and bar. Standard library only. Run: python -I make_icon.py <out.ico>"""
import struct, sys, zlib


def disc(n):
    rows = []
    c = (n - 1) / 2
    s = n / 32  # the tray icon is designed at 32 px
    for y in range(n):
        row = bytearray([0])  # PNG filter: none
        for x in range(n):
            dx, dy = (x - c) / s, (y - c) / s
            d = (dx * dx + dy * dy) ** 0.5
            inside = d <= 14.5
            ring = 9.0 <= d <= 12.0 or (abs(dx) <= 1.5 and abs(dy) <= 6.0 and dx > -1.6)
            if inside and ring:
                px = (0xFF, 0xFF, 0xFF, 0xFF)
            elif inside:
                px = (0x4F, 0x8C, 0xFF, 0xFF)
            else:
                px = (0, 0, 0, 0)
            row += bytes(px)
        rows.append(bytes(row))
    return b"".join(rows)


def png(n):
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
    header = struct.pack(">IIBBBBB", n, n, 8, 6, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(disc(n), 9)) + chunk(b"IEND", b"")


def ico(sizes):
    images = [png(n) for n in sizes]
    out = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    for n, data in zip(sizes, images):
        out += struct.pack("<BBBBHHII", n % 256, n % 256, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    return out + b"".join(images)


if __name__ == "__main__":
    with open(sys.argv[1], "wb") as f:
        f.write(ico([16, 24, 32, 48, 64, 128, 256]))
