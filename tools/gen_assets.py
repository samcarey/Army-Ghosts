#!/usr/bin/env python3
"""Generate the game's placeholder texture assets (stdlib only, deterministic).

Outputs to client/assets/:
  ground.png  - 128x128 tileable grass/dirt noise tile (RGB)
  disc.png    - 128x128 white disc with soft edge (RGBA, tint via sprite color)
  ring.png    - 128x128 white ring outline (RGBA, joystick base)
  soldier.png - 13x16 grid of 64x64 frames: a 3/4-view soldier modelled in 3D
                and projected 40 degrees off top-down, always upright (head up,
                feet down). Columns are animation (0 idle, 1-6 walk, 7-12 run),
                rows are the 16 facings clockwise from away-from-camera.
                Grayscale (engine tints per player), low contrast, camouflaged,
                no outlines.
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
# The soldier is modelled once in 3D — capsules in character space, x right,
# y forward, z up, origin on the ground between the feet — then rotated about z
# per facing and projected at SOLDIER_TILT off straight-down. That gives the
# 3/4 view these games traditionally use: head up, feet down, always upright on
# screen, with a different silhouette per direction rather than one sprite spun
# around. So the sheet is a grid: one row per facing, one column per animation
# frame, and the engine picks the row from `Facing` instead of rotating.
#
# Orthographic projection means a sphere stays a circle, so a 3D capsule
# projects to a 2D capsule and the rasteriser stays cheap. Parts are painted
# far-to-near by their depth along the view axis.
#
# Everything is grayscale (the engine tints per player) and deliberately low
# contrast: shades sit in a narrow band, there are no dark outlines separating
# parts, camouflage breaks up each part in muted bands, and the silhouette is
# jittered by noise so nothing reads as a clean analytic curve.

SOLDIER_TILT = math.radians(40.0)   # off straight-down
SOLDIER_DIRS = 16                   # facings, clockwise from "away from camera"
SOLDIER_SCALE = 38.0                # px per character unit
SOLDIER_GROUND_PY = 52.0            # where the character's ground point lands


def _seg_dist2(px, py, ax, ay, bx, by):
    return _seg_nearest(px, py, ax, ay, bx, by)[0]


def _seg_nearest(px, py, ax, ay, bx, by):
    """Squared distance to the segment, plus how far along it the foot lies."""
    vx, vy = bx - ax, by - ay
    wx, wy = px - ax, py - ay
    denom = vx * vx + vy * vy
    t = 0.0 if denom == 0 else max(0.0, min(1.0, (wx * vx + wy * vy) / denom))
    dx, dy = px - (ax + vx * t), py - (ay + vy * t)
    return dx * dx + dy * dy, t


def _noise_field(size, seed):
    """Tileable value-noise lattice. Precomputed because the sheet is ~200
    frames and hashing per pixel would dominate the runtime."""
    rng = random.Random(seed)
    return [[rng.random() for _ in range(size)] for _ in range(size)]


def _sample(field, x, y):
    n = len(field)
    xi, yi = math.floor(x), math.floor(y)
    xf, yf = x - xi, y - yi
    x0, y0 = xi % n, yi % n
    x1, y1 = (xi + 1) % n, (yi + 1) % n
    u = xf * xf * (3 - 2 * xf)
    v = yf * yf * (3 - 2 * yf)
    a = field[y0][x0] * (1 - u) + field[y0][x1] * u
    b = field[y1][x0] * (1 - u) + field[y1][x1] * u
    return a * (1 - v) + b * v


_CAMO = _noise_field(32, 20260725)
_CAMO_FINE = _noise_field(32, 777)
_JITTER = _noise_field(32, 31337)


def _soldier_parts(t, stride, lean, crouch):
    """The soldier in character space for one animation phase.

    Each part is (ax, ay, az, bx, by, bz, radius, shade). `stride` is the leg
    swing amplitude, `lean` pitches the upper body forward for the run cycle,
    `crouch` drops it.
    """
    s = math.sin(2 * math.pi * t)
    bob = abs(math.sin(4 * math.pi * t)) * 0.012
    parts = []
    hip_z = 0.86 - crouch + bob

    # Legs. The foot swings fore/aft and lifts on the forward half of its
    # swing; the knee is the midpoint pushed slightly forward so it reads as a
    # bend rather than a straight peg.
    for side in (-1, 1):
        swing = s * side
        foot = (side * 0.11, swing * stride, 0.055 + max(0.0, swing) * 0.07)
        hip = (side * 0.10, 0.0, hip_z)
        knee = (
            (hip[0] + foot[0]) * 0.5,
            (hip[1] + foot[1]) * 0.5 + 0.055,
            (hip[2] + foot[2]) * 0.5,
        )
        parts.append((*hip, *knee, 0.093, 0.52))          # thigh
        parts.append((*knee, *foot, 0.075, 0.50))         # shin
        parts.append((foot[0], foot[1] - 0.055, foot[2],
                      foot[0], foot[1] + 0.085, foot[2], 0.072, 0.42))  # boot

    # Upper body, pitched forward by `lean`.
    def up(y, z):
        return (y + lean * (z - hip_z) * 1.5, z - crouch + bob)

    hy, hz = up(0.0, hip_z + 0.03)
    parts.append((-0.11, hy, hz, 0.11, hy, hz, 0.125, 0.55))          # hips
    ty, tz = up(-0.01, 1.28)
    parts.append((0.0, hy, hz, 0.0, ty, tz, 0.170, 0.58))             # torso
    by, bz = up(-0.17, 1.20)
    parts.append((0.0, by, bz - 0.08, 0.0, by, bz + 0.08, 0.125, 0.50))  # pack
    sy, sz = up(0.0, 1.30)
    parts.append((-0.20, sy, sz, 0.20, sy, sz, 0.122, 0.57))          # shoulders

    # Arms bring both hands onto the weapon; it stays up at the ready rather
    # than swinging, which is what you want in a shooter.
    for side, hand in ((1, (0.05, 0.30, 1.16)), (-1, (-0.03, 0.43, 1.19))):
        shy, shz = up(0.0, 1.29)
        elbow_y, elbow_z = up(0.15, 1.13)
        hand_y, hand_z = up(hand[1], hand[2])
        parts.append((side * 0.21, shy, shz, side * 0.19, elbow_y, elbow_z, 0.072, 0.55))
        parts.append((side * 0.19, elbow_y, elbow_z, hand[0], hand_y, hand_z, 0.062, 0.53))

    # Weapon: stock back at the shoulder, barrel forward. Darkest thing on the
    # figure, but only just — high contrast is what we're avoiding.
    sty, stz = up(0.10, 1.12)
    bry, brz = up(0.64, 1.20)
    parts.append((0.06, sty, stz, 0.01, bry, brz, 0.033, 0.34))
    gy, gz = up(0.05, 1.10)
    parts.append((0.06, gy, gz, 0.05, sty + 0.04, stz + 0.02, 0.048, 0.38))

    ny, nz = up(0.0, 1.36)
    parts.append((0.0, ny, nz, 0.0, ny, nz + 0.06, 0.065, 0.54))      # neck
    hly, hlz = up(0.01, 1.53)
    parts.append((0.0, hly, hlz, 0.0, hly + 0.01, hlz + 0.035, 0.140, 0.60))  # helmet
    return parts


def _render_soldier_frame(parts, phi, size):
    """Rotate to facing `phi`, project, depth sort and paint one frame."""
    cos_p, sin_p = math.cos(phi), math.sin(phi)
    cos_t, sin_t = math.cos(SOLDIER_TILT), math.sin(SOLDIER_TILT)

    def place(x, y, z):
        # Clockwise about z, so the model's +y (its forward) swings to the
        # facing the sheet row stands for.
        rx = x * cos_p + y * sin_p
        ry = -x * sin_p + y * cos_p
        px = size / 2 + rx * SOLDIER_SCALE
        py = SOLDIER_GROUND_PY - (ry * cos_t + z * sin_t) * SOLDIER_SCALE
        depth = ry * sin_t - z * cos_t          # bigger = further from camera
        return px, py, depth

    flat = []
    for ax, ay, az, bx, by, bz, r, shade in parts:
        pax, pay, da = place(ax, ay, az)
        pbx, pby, db = place(bx, by, bz)
        flat.append((max(da, db), pax, pay, pbx, pby, r * SOLDIER_SCALE, shade))
    flat.sort(key=lambda p: -p[0])              # far to near

    shade_buf = [[0.0] * size for _ in range(size)]
    alpha_buf = [[0.0] * size for _ in range(size)]
    light = (-0.42, -0.50, 0.76)                # screen x right, y down, z out

    for idx, (_, ax, ay, bx, by, r, base) in enumerate(flat):
        x0 = max(0, int(min(ax, bx) - r - 2))
        x1 = min(size - 1, int(max(ax, bx) + r + 2))
        y0 = max(0, int(min(ay, by) - r - 2))
        y1 = min(size - 1, int(max(ay, by) + r + 2))
        seed = idx * 13.0
        for py in range(y0, y1 + 1):
            for px in range(x0, x1 + 1):
                fx, fy = px + 0.5, py + 0.5
                d2, along = _seg_nearest(fx, fy, ax, ay, bx, by)
                d = math.sqrt(d2)
                # Ragged silhouette: nothing on a soldier is a clean curve.
                jitter = (_sample(_JITTER, fx * 0.45 + seed, fy * 0.45) - 0.5) * 1.7
                cover = max(0.0, min(1.0, r + jitter - d + 0.5))
                if cover <= 0.0:
                    continue
                # Fake the surface normal off the capsule cross-section so the
                # part reads as round without any outline to define it.
                nx = (fx - (ax + (bx - ax) * along)) / r
                ny = (fy - (ay + (by - ay) * along)) / r
                nz = math.sqrt(max(0.0, 1.0 - nx * nx - ny * ny))
                lam = nx * light[0] + ny * light[1] + nz * light[2]
                # Camouflage: two octaves quantised into patches, in part-local
                # coordinates so the pattern travels with the limb.
                u = along * 24.0 + seed
                v = (nx * 0.7 + 0.5) * 9.0
                n = 0.62 * _sample(_CAMO, u * 0.5, v * 0.5) + 0.38 * _sample(_CAMO_FINE, u, v)
                if n < 0.42:
                    camo = -0.062
                elif n < 0.60:
                    camo = 0.004
                else:
                    camo = 0.070
                shade = base * (0.90 + 0.20 * lam) + camo
                shade = max(0.0, min(1.0, shade))
                shade_buf[py][px] = shade_buf[py][px] * (1 - cover) + shade * cover
                alpha_buf[py][px] = alpha_buf[py][px] + cover * (1 - alpha_buf[py][px])
    return shade_buf, alpha_buf


def gen_soldier(path, size=64):
    """Grid sheet: SOLDIER_DIRS rows of facings x 13 columns of animation
    (0 idle, 1-6 walk, 7-12 run)."""
    cycles = [(0.0, 0.0, 0.0, 0.0)]                        # idle
    cycles += [(i / 6, 0.20, 0.0, 0.0) for i in range(6)]  # walk
    cycles += [(i / 6, 0.34, 0.16, 0.05) for i in range(6)]  # run, leaning
    cols = len(cycles)

    rows = [bytearray() for _ in range(size * SOLDIER_DIRS)]
    for d in range(SOLDIER_DIRS):
        phi = 2 * math.pi * d / SOLDIER_DIRS
        for t, stride, lean, crouch in cycles:
            shade_buf, alpha_buf = _render_soldier_frame(
                _soldier_parts(t, stride, lean, crouch), phi, size
            )
            for y in range(size):
                row = rows[d * size + y]
                for x in range(size):
                    a = alpha_buf[y][x]
                    if a <= 0.004:
                        row += b'\x00\x00\x00\x00'
                    else:
                        v = max(0, min(255, int(shade_buf[y][x] * 255)))
                        row += bytes((v, v, v, int(a * 255)))
    write_png(path, size * cols, size * SOLDIER_DIRS, rows, color_type=6)  # RGBA


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
