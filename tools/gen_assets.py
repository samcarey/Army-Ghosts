#!/usr/bin/env python3
"""Generate the game's placeholder texture assets (stdlib only, deterministic).

Outputs to client/assets/:
  ground.png  - 128x128 tileable grass/dirt noise tile (RGB)
  disc.png    - 128x128 white disc with soft edge (RGBA, tint via sprite color)
  ring.png    - 128x128 white ring outline (RGBA, joystick base)

Run from anywhere: python3 tools/gen_assets.py
Committed outputs are canonical; rerun only when tweaking the look.
"""
import os
import random
import struct
import zlib


def write_png(path, width, height, rows, color_type):
    """rows: list of bytes-like scanlines WITHOUT filter byte."""

    def chunk(tag, data):
        c = struct.pack('!I', len(data)) + tag + data
        return c + struct.pack('!I', zlib.crc32(tag + data) & 0xFFFFFFFF)

    raw = b''.join(b'\x00' + bytes(r) for r in rows)
    png = (
        b'\x89PNG\r\n\x1a\n'
        + chunk(b'IHDR', struct.pack('!IIBBBBB', width, height, 8, color_type, 0, 0, 0))
        + chunk(b'IDAT', zlib.compress(raw, 9))
        + chunk(b'IEND', b'')
    )
    with open(path, 'wb') as f:
        f.write(png)
    print(f'wrote {path} ({len(png)} bytes)')


def gen_ground(path, size=128):
    rng = random.Random(42)
    base = (62, 74, 42)  # muted army green
    # Low-frequency blotches for variation, wrapped so the tile is seamless.
    blotches = [
        (rng.randrange(size), rng.randrange(size), rng.randint(12, 28), rng.randint(-10, 8))
        for _ in range(24)
    ]
    rows = []
    for y in range(size):
        row = bytearray()
        for x in range(size):
            n = rng.randint(-7, 7)  # per-pixel grain
            d = 0
            for bx, by, r, amt in blotches:
                # toroidal distance => seamless tiling
                dx = min(abs(x - bx), size - abs(x - bx))
                dy = min(abs(y - by), size - abs(y - by))
                if dx * dx + dy * dy < r * r:
                    d += amt
            d = max(-16, min(14, d))
            row += bytes(
                max(0, min(255, c + n + d + (dc * d // 12)))
                for c, dc in zip(base, (2, 3, 1))
            )
        rows.append(row)
    write_png(path, size, size, rows, color_type=2)  # RGB


def gen_disc(path, size=128, inner=None):
    """White disc (inner=None) or ring (inner = inner radius fraction)."""
    r_out = size / 2 - 2
    rows = []
    for y in range(size):
        row = bytearray()
        for x in range(size):
            dx, dy = x + 0.5 - size / 2, y + 0.5 - size / 2
            dist = (dx * dx + dy * dy) ** 0.5
            a = max(0.0, min(1.0, r_out - dist + 0.5))  # soft outer edge
            if inner is not None:
                r_in = r_out * inner
                a *= max(0.0, min(1.0, dist - r_in + 0.5))  # soft inner edge
            row += bytes((255, 255, 255, int(a * 255)))
        rows.append(row)
    write_png(path, size, size, rows, color_type=6)  # RGBA


if __name__ == '__main__':
    out = os.path.join(os.path.dirname(__file__), '..', 'client', 'assets')
    os.makedirs(out, exist_ok=True)
    gen_ground(os.path.join(out, 'ground.png'))
    gen_disc(os.path.join(out, 'disc.png'))
    gen_disc(os.path.join(out, 'ring.png'), inner=0.82)
