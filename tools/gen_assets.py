#!/usr/bin/env python3
"""Generate the game's placeholder texture assets (stdlib only, deterministic).

Outputs to client/assets/:
  ground.png  - 128x128 tileable grass/dirt noise tile (RGB)
  disc.png    - 128x128 white disc with soft edge (RGBA, tint via sprite color)
  ring.png    - 128x128 white ring outline (RGBA, joystick base)
  soldier.png - 13x1 grid of 64x64 frames: top-down soldier facing UP,
                grayscale (engine tints per player). Frame 0 idle,
                1-6 walk cycle, 7-12 run cycle (longer stride + lean).
  tracer.png  - 32x8 white tracer streak pointing RIGHT (+x): soft capsule
                with a bright head and a tail fading to transparent. The
                engine tints it and rotates it to the bullet's flight angle.
  crosshair.png - 128x128 white ring with four inward ticks and a center dot
                (RGBA); the aim-down-sights button icon.
  rocks.png   - 4x1 grid of 96x96 boulder variants (RGBA, grayscale): irregular
                harmonic outlines, lit from the top-left, faceted + grainy. The
                engine picks the variant/rotation/tint from each rock's seed.
  bushes.png  - 4x1 grid of 96x96 bush variants (RGBA, grayscale): overlapping
                leaf lobes with rimmed edges. Flat interior alpha on purpose —
                the engine draws them translucent, so overlapping bushes stack
                into cleanly denser cover.

Run from anywhere: python3 tools/gen_assets.py
Committed outputs are canonical; rerun only when tweaking the look.
"""
import math
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


# ── Soldier sprite sheet ─────────────────────────────────────────────────────
# Shapes are analytic (ellipses + capsules) rasterized with 3x3 subsampling;
# grayscale shades multiply against the per-player sprite tint in the engine,
# so the whole figure reads as one plastic color — the army-men look.


def _seg_dist2(px, py, ax, ay, bx, by):
    vx, vy = bx - ax, by - ay
    wx, wy = px - ax, py - ay
    denom = vx * vx + vy * vy
    t = 0.0 if denom == 0 else max(0.0, min(1.0, (wx * vx + wy * vy) / denom))
    dx, dy = px - (ax + vx * t), py - (ay + vy * t)
    return dx * dx + dy * dy


def _hit(shape, x, y):
    if shape[0] == 'ellipse':
        _, cx, cy, rx, ry, _ = shape
        dx, dy = (x - cx) / rx, (y - cy) / ry
        return dx * dx + dy * dy <= 1.0
    _, ax, ay, bx, by, r, _ = shape  # capsule
    return _seg_dist2(x, y, ax, ay, bx, by) <= r * r


def _render(shapes, size):
    """Rasterize shapes (painter's order, last on top) to an RGBA pixel grid."""
    sub = (1 / 6, 3 / 6, 5 / 6)
    grid = []
    for y in range(size):
        row = []
        for x in range(size):
            cover, shade_sum = 0, 0.0
            for oy in sub:
                for ox in sub:
                    shade = None
                    for sh in shapes:
                        if _hit(sh, x + ox, y + oy):
                            shade = sh[-1]
                    if shade is not None:
                        cover += 1
                        shade_sum += shade
            if cover:
                v = int(round(shade_sum / cover * 255))
                row.append((v, v, v, int(round(cover / 9 * 255))))
            else:
                row.append((0, 0, 0, 0))
        grid.append(row)
    return grid


def _soldier_pose(size, stride_amp, sway_amp, lean, t):
    """One frame's shape list. Texture y is DOWN; the soldier faces UP.

    stride_amp: leg swing amplitude (px). sway_amp: torso/helmet lateral sway.
    lean: forward body shift (run frames). t: 0..1 phase through the cycle.
    """
    stride = math.sin(2 * math.pi * t) * stride_amp
    sway = math.sin(2 * math.pi * t) * sway_amp
    cx, cy = size / 2, size / 2 + 3 - lean
    shapes = []

    def add(shape, outline=True):
        # A dark rim under each part separates it from what it overlaps
        # (helmet from shoulders, boots from ground) — the toy-figure read.
        if outline:
            grow, rim = 1.6, 0.14
            if shape[0] == 'ellipse':
                _, ex, ey, rx, ry, _ = shape
                shapes.append(('ellipse', ex, ey, rx + grow, ry + grow, rim))
            else:
                _, ax, ay, bx, by, r, _ = shape
                shapes.append(('capsule', ax, ay, bx, by, r + grow, rim))
        shapes.append(shape)

    # Boots: swing fore/aft in opposite phase; visible past the torso mid-stride.
    for side, s in ((-1, -stride), (1, stride)):
        bx, by = cx + side * 7.0, cy + 6 + s
        add(('capsule', bx, by - 3.5, bx, by + 3.5, 4.2, 0.38))
    # Torso (shoulders wide across x).
    add(('ellipse', cx + sway * 0.5, cy, 16, 8.5, 0.62))
    # Arms reaching to the rifle (left hand on foregrip, right on the stock).
    add(('capsule', cx - 13, cy - 1, cx - 4, cy - 12, 3.0, 0.50))
    add(('capsule', cx + 13, cy, cx + 4, cy - 7, 3.0, 0.50))
    # Rifle: dark barrel pointing up (slightly right of center, right-handed).
    add(('capsule', cx + 1.5, cy - 5, cx + 1.5, cy - 26, 2.0, 0.22))
    # Hands on top of the rifle.
    add(('ellipse', cx - 4, cy - 12, 2.8, 2.8, 0.55), outline=False)
    add(('ellipse', cx + 4, cy - 7, 2.8, 2.8, 0.55), outline=False)
    # Helmet with an off-center highlight.
    add(('ellipse', cx + sway, cy - 2, 8.5, 8.5, 0.90))
    add(('ellipse', cx + sway - 2.2, cy - 4.2, 4.2, 4.2, 1.0), outline=False)
    return shapes


def gen_soldier(path, size=64):
    poses = [_soldier_pose(size, 0, 0, 0, 0)]  # frame 0: idle, legs together
    for i in range(6):  # frames 1-6: walk
        poses.append(_soldier_pose(size, 6.5, 1.0, 0, i / 6))
    for i in range(6):  # frames 7-12: run (longer stride, forward lean)
        poses.append(_soldier_pose(size, 9.5, 2.0, 3, i / 6))
    grids = [_render(shapes, size) for shapes in poses]
    rows = []
    for y in range(size):
        row = bytearray()
        for g in grids:
            for px in g[y]:
                row += bytes(px)
        rows.append(row)
    write_png(path, size * len(grids), size, rows, color_type=6)  # RGBA


def gen_tracer(path, w=32, h=8):
    """Tracer streak pointing +x: bright rounded head, tail fades out."""
    cy = h / 2
    r = h / 2 - 1
    tail_x, head_x = r + 1, w - r - 1
    rows = []
    for y in range(h):
        row = bytearray()
        for x in range(w):
            px, py = x + 0.5, y + 0.5
            d = _seg_dist2(px, py, tail_x, cy, head_x, cy) ** 0.5
            edge = max(0.0, min(1.0, r - d + 0.5))  # soft capsule edge
            f = max(0.0, min(1.0, (px - tail_x) / (head_x - tail_x)))
            row += bytes((255, 255, 255, int(edge * f ** 1.5 * 255)))
        rows.append(row)
    write_png(path, w, h, rows, color_type=6)  # RGBA


def gen_crosshair(path, size=128):
    """ADS icon: thin ring, four ticks reaching inward, small center dot."""
    c = size / 2
    r_out = size / 2 - 5
    t = 2.6  # half-stroke width
    rows = []
    for y in range(size):
        row = bytearray()
        for x in range(size):
            px, py = x + 0.5, y + 0.5
            d = ((px - c) ** 2 + (py - c) ** 2) ** 0.5
            a = max(0.0, min(1.0, t - abs(d - r_out) + 0.5))  # ring
            for dx, dy in ((0, 1), (0, -1), (1, 0), (-1, 0)):
                # Tick from just inside the ring toward (but short of) center.
                seg = _seg_dist2(
                    px, py,
                    c + dx * (r_out - 4), c + dy * (r_out - 4),
                    c + dx * 16, c + dy * 16,
                ) ** 0.5
                a = max(a, max(0.0, min(1.0, t - seg + 0.5)))
            a = max(a, max(0.0, min(1.0, 3.5 - d + 0.5)))  # center dot
            row += bytes((255, 255, 255, int(a * 255)))
        rows.append(row)
    write_png(path, size, size, rows, color_type=6)  # RGBA


# ── Boulders ─────────────────────────────────────────────────────────────────


def gen_rocks(path, frame=96, variants=4, fill=40.0):
    """Boulder sheet: one row of `variants` frames, each an irregular blob whose
    mean radius is `fill` px (the engine scales 2r/fill to match the sim's
    collision circle). Grayscale — rocks are tinted per-seed at draw time."""
    half = frame / 2
    rows = [bytearray() for _ in range(frame)]
    for v in range(variants):
        rng = random.Random(900 + v)
        # Outline: mean radius modulated by a few low-frequency harmonics, so
        # every variant is lumpy in its own way but still roughly circular.
        harmonics = [
            (k, rng.uniform(0.03, 0.09), rng.uniform(0, 2 * math.pi))
            for k in (2, 3, 5, 7)
        ]
        # Interior facets: wedges of slightly different shade, so the blob
        # reads as chipped stone rather than a ball.
        facets = [
            (rng.uniform(0, 2 * math.pi), rng.uniform(0.3, 0.8), rng.uniform(-18, 18))
            for _ in range(3)
        ]
        for y in range(frame):
            for x in range(frame):
                px, py = x + 0.5 - half, y + 0.5 - half
                dist = math.hypot(px, py)
                theta = math.atan2(py, px)
                r = fill * (1 + sum(a * math.cos(k * theta + p) for k, a, p in harmonics))
                alpha = max(0.0, min(1.0, r - dist + 0.5))
                if alpha <= 0.0:
                    rows[y] += b'\x00\x00\x00\x00'
                    continue
                # Lit from the top-left (image y is down, so -x-y faces the sun).
                shade = 132 + 54 * max(-1.0, min(1.0, (-px - py) / (fill * 1.6)))
                for center, width, amount in facets:
                    if abs(((theta - center + math.pi) % (2 * math.pi)) - math.pi) < width:
                        shade += amount
                shade += rng.randint(-9, 9)  # grain
                # Dark rim: separates the boulder from the ground underneath.
                shade -= 48 * max(0.0, min(1.0, 3.5 - (r - dist)))
                c = max(0, min(255, int(shade)))
                rows[y] += bytes((c, c, c, int(alpha * 255)))
    write_png(path, frame * variants, frame, rows, color_type=6)  # RGBA


# ── Bushes ───────────────────────────────────────────────────────────────────


def gen_bushes(path, frame=96, variants=4, fill=40.0):
    """Bush sheet: one row of `variants` frames, each a clump of leaf lobes
    covering a `fill`-radius canopy. Painter's order with a dark rim per lobe so
    the clumps stay legible; interior alpha is flat (1.0) because the engine
    draws bushes translucent and overlapping canopies must stack evenly."""
    half = frame / 2
    rows = [bytearray() for _ in range(frame)]
    for v in range(variants):
        rng = random.Random(1300 + v)
        lobes = []
        # Many small clumps rather than a few big ones: a handful of large
        # lobes reads as a pile of balloons, a scatter of small ones reads as
        # leaves. Bias placement outward (sqrt) so the canopy fills its circle.
        for _ in range(34):
            angle = rng.uniform(0, 2 * math.pi)
            d = fill * 0.62 * math.sqrt(rng.random())
            lobes.append((
                math.cos(angle) * d,
                math.sin(angle) * d,
                rng.uniform(fill * 0.17, fill * 0.30),
                rng.uniform(-26, 26),  # per-lobe tone
            ))
        for y in range(frame):
            for x in range(frame):
                px, py = x + 0.5 - half, y + 0.5 - half
                alpha = 0.0
                shade = None
                for lx, ly, lr, tone in lobes:  # painter's order: last on top
                    d = math.hypot(px - lx, py - ly)
                    a = max(0.0, min(1.0, lr - d + 0.5))
                    if a <= 0.0:
                        continue
                    alpha = max(alpha, a)
                    # A gentle top-left lift plus a dark rim on this clump's
                    # edge. Keep the gradient shallow — a strong one turns each
                    # lobe back into a sphere.
                    lit = 16 * max(-1.0, min(1.0, (-(px - lx) - (py - ly)) / lr))
                    shade = 140 + tone + lit - 40 * max(0.0, min(1.0, 2.0 - (lr - d)))
                if shade is None:
                    rows[y] += b'\x00\x00\x00\x00'
                    continue
                c = max(0, min(255, int(shade + rng.randint(-10, 10))))
                rows[y] += bytes((c, c, c, int(alpha * 255)))
    write_png(path, frame * variants, frame, rows, color_type=6)  # RGBA


if __name__ == '__main__':
    out = os.path.join(os.path.dirname(__file__), '..', 'client', 'assets')
    os.makedirs(out, exist_ok=True)
    gen_ground(os.path.join(out, 'ground.png'))
    gen_disc(os.path.join(out, 'disc.png'))
    gen_disc(os.path.join(out, 'ring.png'), inner=0.82)
    gen_soldier(os.path.join(out, 'soldier.png'))
    gen_tracer(os.path.join(out, 'tracer.png'))
    gen_crosshair(os.path.join(out, 'crosshair.png'))
    gen_rocks(os.path.join(out, 'rocks.png'))
    gen_bushes(os.path.join(out, 'bushes.png'))
