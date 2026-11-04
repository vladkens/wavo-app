#!/usr/bin/env python3
"""Wavo Dev's icon: Wavo's in orange, so the two apps tell apart at a glance.

  scripts/dev-icon.py <icon.png> <out.png>

The icon is a palette PNG: only its PLTE chunk changes, the pixels stay, so no image library is
needed. Each colour takes the hue HUE and keeps its saturation and brightness: the gradient's
light and dark stay, the white mark and the grey shadow too.
"""

import colorsys
import struct
import sys
import zlib
from pathlib import Path

SIGNATURE = b"\x89PNG\r\n\x1a\n"
# Orange.
HUE = 0.07


def orange(plte: bytes) -> bytes:
    out = bytearray()
    for i in range(0, len(plte), 3):
        _, s, v = colorsys.rgb_to_hsv(*(c / 255 for c in plte[i : i + 3]))
        rgb = colorsys.hsv_to_rgb(HUE, s, v)
        out += bytes(round(c * 255) for c in rgb)

    return bytes(out)


def main() -> int:
    src, dst = Path(sys.argv[1]), Path(sys.argv[2])
    data = src.read_bytes()
    if not data.startswith(SIGNATURE):
        sys.exit(f"{src}: not a PNG")

    out, at, found = bytearray(SIGNATURE), len(SIGNATURE), False
    while at < len(data):
        (size,) = struct.unpack(">I", data[at : at + 4])
        kind, body = data[at + 4 : at + 8], data[at + 8 : at + 8 + size]
        if kind == b"PLTE":
            body, found = orange(body), True

        out += struct.pack(">I", size) + kind + body + struct.pack(">I", zlib.crc32(kind + body))
        at += 12 + size

    if not found:
        sys.exit(f"{src}: no palette, which this script colours")

    dst.parent.mkdir(parents=True, exist_ok=True)
    dst.write_bytes(out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
