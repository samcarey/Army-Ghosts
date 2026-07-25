#!/usr/bin/env python3
"""Generate the game's placeholder texture assets (stdlib only, deterministic).

Outputs to client/assets/:
  ground.png  - 128x128 tileable grass/dirt noise tile (RGB)
  disc.png    - 128x128 white disc with soft edge (RGBA, tint via sprite color)
  ring.png    - 128x128 white ring outline (RGBA, joystick base)
  soldier.png - 39x16 grid of 72x72 frames: a 3/4-view soldier modelled in 3D
                and projected 40 degrees off top-down, always upright (head up,
                feet down). Rows are the 16 facings clockwise from
                away-from-camera; columns are three 13-column stance blocks
                (standing, crouching, prone), each 0 idle, 1-6 walk, 7-12 run.
                Grayscale (engine tints per player), low contrast, camouflaged,
                no outlines.
  tracer.png  - 32x8 white tracer streak pointing RIGHT (+x): soft capsule
                with a bright head and a tail fading to transparent. The
                engine tints it and rotates it to the bullet's flight angle.
  crosshair.png - 128x128 white ring with four inward ticks and a center dot
                (RGBA); the aim-down-sights button icon.
  chevron.png - 128x128 white chevron pointing UP (RGBA); the stance buttons
                use it as-is to stand up and flipped to get down.
  rocks.png   - 4x1 grid of 96x96 boulder variants (RGBA, grayscale): irregular
                harmonic outlines, lit from the top-left, faceted + grainy. The
                engine picks the variant/rotation/tint from each rock's seed.
  grass.png   - 128x128 tileable sward seen from above (RGB): soil under a mess
                of short blade strokes, every stroke wrapped so the tile is
                seamless. The ground mesh repeats it in WORLD space and tints it
                per area from the sim's grass depth.
  tufts.png   - 12x1 grid of 28x48 grass clumps (RGBA, COLOUR), modelled in 3D
                and projected at the same 40 degrees as everything else, ground
                line GRASS_BASE_FRAC up from the bottom edge. Small and narrow:
                the engine scatters thousands of them, scaled to the local
                depth, and y-sorts them against everything standing in the
                field.
  shade.png   - 64x64 white vertical gradient (RGBA), full at the bottom: the
                shadow the grass throws up the front of whatever is standing in
                it. Tinted dark green at draw time.
  bushes.png  - 6x1 grid of 96x96 bush variants (RGBA, COLOUR): fractal
                branches with leaf capsules, modelled in 3D and projected at the
                same 40 degrees off top-down as the soldier, so cover is seen
                from the figures' angle. The engine tints these near-white,
                draws them translucent (they stack), and must NOT rotate them.

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
#
# Three stances share the model: standing and crouching are the same figure with
# the hips dropped (and the knees driven out to match), prone is its own layout
# lying along +y. Prone is why the frame is 72px and not 64: a soldier seen
# side-on while prone is as long as they are tall standing up, with no
# foreshortening to shrink it, so the widest frame in the sheet is a crawling
# figure facing due east.

SOLDIER_TILT = math.radians(40.0)   # off straight-down
SOLDIER_DIRS = 16                   # facings, clockwise from "away from camera"
SOLDIER_FRAME = 72                  # px per frame
SOLDIER_SCALE = 38.0                # px per character unit
SOLDIER_GROUND_PY = 58.5            # where an upright figure's ground point lands
# Prone frames are centred instead: the origin is mid-body (roughly under the
# ribs), because that is where the pawn's `Pos` is and the figure has to be able
# to swing around it in any of the 16 facings without leaving the frame.
SOLDIER_PRONE_PY = 36.0
STANCE_COLS = 13                    # animation columns per stance block


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
    # Everything above the waist is authored at standing height and dropped by
    # `crouch` exactly once, in `up()`. HIP_Z is that reference height; `hip_z`
    # is where the hips actually end up.
    HIP_Z = 0.86
    hip_z = HIP_Z - crouch + bob

    # Legs. The foot swings fore/aft and lifts on the forward half of its
    # swing; the knee is the midpoint pushed slightly forward so it reads as a
    # bend rather than a straight peg. Crouching drives the knee further
    # forward and out to the side — without that the dropped hips just read as
    # a short soldier instead of a folded one.
    for side in (-1, 1):
        swing = s * side
        foot = (side * 0.11, swing * stride, 0.055 + max(0.0, swing) * 0.07)
        hip = (side * 0.10, 0.0, hip_z)
        knee = (
            (hip[0] + foot[0]) * 0.5 + side * crouch * 0.30,
            (hip[1] + foot[1]) * 0.5 + 0.055 + crouch * 0.55,
            (hip[2] + foot[2]) * 0.5,
        )
        parts.append((*hip, *knee, 0.093, 0.52))          # thigh
        parts.append((*knee, *foot, 0.075, 0.50))         # shin
        parts.append((foot[0], foot[1] - 0.055, foot[2],
                      foot[0], foot[1] + 0.085, foot[2], 0.072, 0.42))  # boot

    # Upper body, pitched forward by `lean` and dropped by `crouch`. `z` is a
    # standing height — feeding it `hip_z` (which has already paid the crouch)
    # would drop the torso twice, which reads as fine at the run cycle's 0.05
    # and puts the shoulders through the knees at a real crouch.
    def up(y, z):
        return (y + lean * (z - HIP_Z) * 1.5, z - crouch + bob)

    hy, hz = up(0.0, HIP_Z + 0.03)
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


def _prone_parts(t, amp):
    """The soldier lying down, in the same character space (x right, y forward,
    z up) — but the origin is mid-body rather than on the ground between the
    feet, because that is where the pawn's `Pos` is once it is horizontal.

    `amp` scales the crawl: 0 is lying still, 1 is a full low-crawl stroke. The
    stroke is contralateral (left knee drives while the right arm reaches),
    which is how anyone actually moves on their belly.
    """
    s = math.sin(2 * math.pi * t) * amp
    parts = []

    # Legs, splayed and drawn up one at a time. They stay flat on the ground —
    # a knee raised into the air is the tell of a soldier who is not really
    # prone.
    for side in (-1, 1):
        pull = max(0.0, s * side)
        hip = (side * 0.11, -0.20, 0.12)
        knee = (side * (0.22 + 0.16 * pull), -0.46 + 0.20 * pull, 0.11)
        foot = (side * (0.15 + 0.12 * pull), -0.70 + 0.26 * pull, 0.09)
        parts.append((*hip, *knee, 0.093, 0.52))          # thigh
        parts.append((*knee, *foot, 0.075, 0.50))         # shin
        parts.append((foot[0], foot[1] - 0.02, foot[2],
                      foot[0], foot[1] + 0.07, foot[2] + 0.02, 0.070, 0.42))  # boot

    parts.append((-0.12, -0.20, 0.13, 0.12, -0.20, 0.13, 0.125, 0.55))   # hips
    parts.append((0.0, -0.18, 0.14, 0.0, 0.20, 0.15, 0.155, 0.58))       # torso
    parts.append((0.0, -0.04, 0.25, 0.0, 0.08, 0.26, 0.115, 0.50))       # pack
    parts.append((-0.19, 0.21, 0.15, 0.19, 0.21, 0.15, 0.115, 0.57))     # shoulders

    # Arms: both hands stay on the weapon, the leading one creeping forward on
    # the reach half of the stroke.
    for side, hand in ((1, (0.06, 0.50, 0.14)), (-1, (-0.02, 0.60, 0.15))):
        reach = max(0.0, -s * side)
        parts.append((side * 0.19, 0.20, 0.15,
                      side * 0.27, 0.34 + 0.06 * reach, 0.11, 0.072, 0.55))
        parts.append((side * 0.27, 0.34 + 0.06 * reach, 0.11,
                      hand[0], hand[1] + 0.04 * reach, hand[2], 0.062, 0.53))

    # Weapon: forward, past the head, resting where a bipod would be.
    parts.append((0.07, 0.26, 0.15, 0.02, 0.74, 0.14, 0.033, 0.34))
    parts.append((0.06, 0.40, 0.12, 0.06, 0.34, 0.15, 0.048, 0.38))      # grip

    parts.append((0.0, 0.26, 0.18, 0.0, 0.31, 0.19, 0.062, 0.54))        # neck
    # Head up and looking over the sights — the one part that lifts clear of
    # the ground, and what tells a prone soldier apart from a dropped pack.
    parts.append((0.0, 0.36, 0.19, 0.0, 0.42, 0.19, 0.135, 0.60))        # helmet
    return parts


def _render_soldier_frame(parts, phi, size, ground_py=SOLDIER_GROUND_PY):
    """Rotate to facing `phi`, project, depth sort and paint one frame."""
    cos_p, sin_p = math.cos(phi), math.sin(phi)
    cos_t, sin_t = math.cos(SOLDIER_TILT), math.sin(SOLDIER_TILT)

    def place(x, y, z):
        # Clockwise about z, so the model's +y (its forward) swings to the
        # facing the sheet row stands for.
        rx = x * cos_p + y * sin_p
        ry = -x * sin_p + y * cos_p
        px = size / 2 + rx * SOLDIER_SCALE
        py = ground_py - (ry * cos_t + z * sin_t) * SOLDIER_SCALE
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


def gen_soldier(path, size=SOLDIER_FRAME):
    """Grid sheet: SOLDIER_DIRS rows of facings x three STANCE_COLS-wide stance
    blocks (standing, crouching, prone), each 0 idle, 1-6 walk, 7-12 run.

    Crouching and crawling cap out well below the run threshold in
    `render.rs`, so their run columns are never picked in play — they are
    filled with the walk frames anyway (a straight buffer reuse, no extra
    rasterising) so that a rollback correction, which can briefly read as a
    supersonic Pos delta, can't land on an empty frame.
    """
    stand = [(0.0, 0.0, 0.0, 0.0)]                          # idle
    stand += [(i / 6, 0.20, 0.0, 0.0) for i in range(6)]    # walk
    stand += [(i / 6, 0.34, 0.16, 0.05) for i in range(6)]  # run, leaning
    # Crouched: hips dropped hard, short shuffling stride, weight forward.
    crouch = [(0.0, 0.0, 0.10, 0.42)]
    crouch += [(i / 6, 0.13, 0.10, 0.42) for i in range(6)]
    prone = [0.0] + [i / 6 for i in range(6)]               # crawl phases

    def stance_frames(phi):
        """The 3 x STANCE_COLS frames of one facing, left to right."""
        upright = [
            _render_soldier_frame(_soldier_parts(*c), phi, size) for c in stand
        ]
        low = [_render_soldier_frame(_soldier_parts(*c), phi, size) for c in crouch]
        flat = [
            _render_soldier_frame(_prone_parts(t, 0.0 if i == 0 else 1.0), phi, size,
                                  SOLDIER_PRONE_PY)
            for i, t in enumerate(prone)
        ]
        # Walk frames stand in for the (unreachable) run columns.
        low += low[1:STANCE_COLS - len(low) + 1]
        flat += flat[1:STANCE_COLS - len(flat) + 1]
        return upright + low + flat

    cols = 3 * STANCE_COLS
    rows = [bytearray() for _ in range(size * SOLDIER_DIRS)]
    for d in range(SOLDIER_DIRS):
        frames = stance_frames(2 * math.pi * d / SOLDIER_DIRS)
        assert len(frames) == cols, f'{len(frames)} frames, expected {cols}'
        for y in range(size):
            row = rows[d * size + y]
            for shade_buf, alpha_buf in frames:
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


def gen_chevron(path, size=128):
    """Stance-button icon: a fat chevron pointing UP. The engine flips it
    vertically for the get-down button, so there is only ever one of these."""
    t = 9.0  # half-stroke width
    apex = (size / 2, size * 0.34)
    left = (size * 0.20, size * 0.68)
    right = (size * 0.80, size * 0.68)
    rows = []
    for y in range(size):
        row = bytearray()
        for x in range(size):
            px, py = x + 0.5, y + 0.5
            d = min(
                _seg_dist2(px, py, *left, *apex) ** 0.5,
                _seg_dist2(px, py, *apex, *right) ** 0.5,
            )
            a = max(0.0, min(1.0, t - d + 0.5))
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
# Like the soldier, a bush is modelled ONCE in 3D (x right, y forward, z up,
# origin on the ground at the stem) and projected at the same `SOLDIER_TILT`,
# so cover is seen from the same 3/4 angle the figures are: canopy above, a
# little trunk showing below, and a silhouette with real vertical extent rather
# than a top-down splat. Because the projection is orthonormal a sphere stays a
# circle, so a canopy of radius 1 still fills a circle of `BUSH_SCALE` px around
# the frame centre — which is what lets the sprite keep matching the sim's
# collision/concealment circle.
#
# The structure is a small L-system: stems from the base recursively split into
# shorter, thinner, paler, more transparent children, with leaf capsules
# scattered over the last two generations. Everything is COLOUR here (unlike
# the grayscale rock/soldier sheets) — the engine only applies a near-white
# per-bush tint — because the point of the fractal is the variation *within* a
# bush: bark against leaf, lit top against shaded underside.
#
# Knock-on: these frames have a definite up, so bushes must NEVER be spun by a
# per-seed rotation the way boulders are. Variety comes from more variants plus
# a horizontal flip.

BUSH_SCALE = 34.0        # px per bush unit; canopy radius ≈ 1 unit
BUSH_CANOPY_Z = 0.98     # canopy centre height — the frame is centred on it
BUSH_LEAF_Z = 0.38       # no leaves below this: bare stems make the angle read
BUSH_DEPTH = 4           # branch generations below the stem

# Bark: thick lower wood is dark and warm, twigs pale toward the leaf greens.
_BARK_THICK = (0.25, 0.20, 0.14)
_BARK_THIN = (0.38, 0.35, 0.21)
# Leaf greens, deliberately spread in value AND hue so the canopy never reads
# as one flat colour once it's shrunk to ~30 px on screen.
_LEAF_GREENS = [
    (0.20, 0.31, 0.12),
    (0.26, 0.39, 0.15),
    (0.33, 0.47, 0.18),
    (0.40, 0.53, 0.22),
    (0.44, 0.50, 0.20),
]


def _norm3(v):
    m = math.sqrt(v[0] * v[0] + v[1] * v[1] + v[2] * v[2]) or 1.0
    return (v[0] / m, v[1] / m, v[2] / m)


def _cross3(a, b):
    return (a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0])


def _spread(d, tilt, azim):
    """`d` tipped `tilt` radians away from itself, rolled to `azim` around it."""
    ref = (0.0, 0.0, 1.0) if abs(d[2]) < 0.9 else (1.0, 0.0, 0.0)
    u = _norm3(_cross3(d, ref))
    v = _cross3(d, u)
    st, ct = math.sin(tilt), math.cos(tilt)
    ca, sa = math.cos(azim), math.sin(azim)
    return _norm3(tuple(d[i] * ct + (u[i] * ca + v[i] * sa) * st for i in range(3)))


def _mix(a, b, t):
    return tuple(a[i] + (b[i] - a[i]) * t for i in range(3))


def _bush_parts(rng):
    """One bush in bush space: capsules `(a, b, radius, rgb, alpha, leaf)`."""
    parts = []

    def leaves(base, direction, count, size):
        for _ in range(count):
            # Splay hard off the twig and let gravity pull the tip down, so
            # leaves fan and droop instead of bristling straight out.
            d = _spread(direction, rng.uniform(0.6, 1.6), rng.uniform(0, 2 * math.pi))
            d = _norm3((d[0], d[1], d[2] - rng.uniform(0.15, 0.75)))
            length = size * rng.uniform(0.75, 1.5)
            tip = tuple(base[i] + d[i] * length for i in range(3))
            if tip[2] < BUSH_LEAF_Z:              # bare wood below the canopy
                continue
            colour = _LEAF_GREENS[rng.randrange(len(_LEAF_GREENS))]
            # Sunlit crown, shaded skirt: a straight value ramp with height
            # does more for the dome read than any per-leaf shading trick.
            dome = 0.66 + 0.52 * max(0.0, min(1.0, (tip[2] - BUSH_LEAF_Z) / 1.05))
            colour = tuple(
                max(0.0, min(1.0, c * dome + rng.uniform(-0.02, 0.02))) for c in colour
            )
            parts.append((
                base, tip, size * rng.uniform(0.30, 0.46),
                colour, rng.uniform(0.55, 0.95), True,
            ))

    def grow(p, d, length, radius, depth):
        # A touch of droop per generation keeps branches from looking welded.
        d = _norm3((d[0], d[1], d[2] - 0.10 * (BUSH_DEPTH - depth)))
        end = tuple(p[i] + d[i] * length for i in range(3))
        thin = 1.0 - depth / BUSH_DEPTH
        parts.append((
            p, end, radius,
            _mix(_BARK_THICK, _BARK_THIN, thin * rng.uniform(0.7, 1.2)),
            0.95 - 0.35 * thin,                   # twigs read as wisps, not wire
            False,
        ))
        if depth <= 1:
            leaves(end, d, rng.randint(4, 7), 0.105)
            at = tuple(p[i] + (end[i] - p[i]) * 0.55 for i in range(3))
            leaves(at, d, rng.randint(2, 4), 0.092)
        if depth == 0:
            return
        children = rng.randint(2, 3)
        roll = rng.uniform(0, 2 * math.pi)
        for i in range(children):
            azim = roll + 2 * math.pi * i / children + rng.uniform(-0.5, 0.5)
            child = _spread(d, rng.uniform(0.35, 0.85), azim)
            grow(end, child, length * rng.uniform(0.62, 0.80),
                 radius * rng.uniform(0.55, 0.70), depth - 1)

    stems = rng.randint(3, 5)
    roll = rng.uniform(0, 2 * math.pi)
    for i in range(stems):
        azim = roll + 2 * math.pi * i / stems + rng.uniform(-0.4, 0.4)
        foot = (math.cos(azim) * 0.05, math.sin(azim) * 0.05, 0.0)
        d = _spread((0.0, 0.0, 1.0), rng.uniform(0.16, 0.42), azim)
        grow(foot, d, rng.uniform(0.46, 0.62), rng.uniform(0.045, 0.068), BUSH_DEPTH)
    return parts


def _render_bush_frame(parts, size):
    """Project, depth sort and composite one bush into (rgb, alpha) buffers."""
    cos_t, sin_t = math.cos(SOLDIER_TILT), math.sin(SOLDIER_TILT)
    ground_py = size / 2 + BUSH_CANOPY_Z * sin_t * BUSH_SCALE

    def place(p):
        px = size / 2 + p[0] * BUSH_SCALE
        py = ground_py - (p[1] * cos_t + p[2] * sin_t) * BUSH_SCALE
        return px, py, p[1] * sin_t - p[2] * cos_t   # bigger = further away

    flat = []
    for a, b, r, colour, alpha, leaf in parts:
        pax, pay, da = place(a)
        pbx, pby, db = place(b)
        flat.append((max(da, db), pax, pay, pbx, pby, r * BUSH_SCALE, colour, alpha, leaf))
    flat.sort(key=lambda p: -p[0])                    # far to near

    # Premultiplied accumulation, so parts of different opacity composite
    # correctly instead of the near one simply winning.
    prem = [[[0.0, 0.0, 0.0] for _ in range(size)] for _ in range(size)]
    acc = [[0.0] * size for _ in range(size)]
    light = (-0.42, -0.50, 0.76)                      # screen x right, y down, z out

    for idx, (_, ax, ay, bx, by, r, colour, alpha, leaf) in enumerate(flat):
        x0 = max(0, int(min(ax, bx) - r - 2))
        x1 = min(size - 1, int(max(ax, bx) + r + 2))
        y0 = max(0, int(min(ay, by) - r - 2))
        y1 = min(size - 1, int(max(ay, by) + r + 2))
        seed = idx * 7.0
        wobble = min(1.4, r * 0.55)                   # leaves are tiny — scale it
        for py in range(y0, y1 + 1):
            for px in range(x0, x1 + 1):
                fx, fy = px + 0.5, py + 0.5
                d2, along = _seg_nearest(fx, fy, ax, ay, bx, by)
                d = math.sqrt(d2)
                jitter = (_sample(_JITTER, fx * 0.6 + seed, fy * 0.6) - 0.5) * wobble
                geom = max(0.0, min(1.0, r + jitter - d + 0.5))
                if geom <= 0.0:
                    continue
                nx = (fx - (ax + (bx - ax) * along)) / r
                ny = (fy - (ay + (by - ay) * along)) / r
                nz = math.sqrt(max(0.0, 1.0 - nx * nx - ny * ny))
                lam = nx * light[0] + ny * light[1] + nz * light[2]
                # Leaves are near-flat blades; wood is round. Same normal, very
                # different amount of it.
                shade = (0.93 + 0.14 * lam) if leaf else (0.82 + 0.34 * lam)
                if leaf:
                    shade += 0.10 * (_sample(_CAMO_FINE, fx * 0.5, fy * 0.5) - 0.5)
                cov = geom * alpha
                for c in range(3):
                    lit = max(0.0, min(1.0, colour[c] * shade))
                    prem[py][px][c] = prem[py][px][c] * (1 - cov) + lit * cov
                acc[py][px] = acc[py][px] * (1 - cov) + cov
    return prem, acc


def gen_bushes(path, frame=96, variants=6):
    """Bush sheet: one row of `variants` fractal bushes, each modelled in 3D and
    projected at `SOLDIER_TILT`. Colour, not grayscale (see the note above)."""
    rows = [bytearray() for _ in range(frame)]
    for v in range(variants):
        prem, acc = _render_bush_frame(_bush_parts(random.Random(1300 + v)), frame)
        for y in range(frame):
            row = rows[y]
            for x in range(frame):
                a = acc[y][x]
                if a <= 0.004:
                    row += b'\x00\x00\x00\x00'
                    continue
                px = prem[y][x]
                row += bytes(
                    tuple(max(0, min(255, int(px[c] / a * 255))) for c in range(3))
                    + (int(min(1.0, a) * 255),)
                )
    write_png(path, frame * variants, frame, rows, color_type=6)  # RGBA


# ── Grass ────────────────────────────────────────────────────────────────────
# Grass is two assets that have to agree with each other, because the engine
# scales both off ONE number: the sim's `grass_height` in world units.
#
#   grass.png  the sward seen from above — the detail texture on the ground mesh
#   tufts.png  small clumps standing in it, scattered over the arena in their
#              thousands and y-sorted, so the ones SOUTH of a soldier cover his
#              legs and the ones north of him don't. (There used to be a third
#              sheet, a band of blades drawn over each pawn. It moved with them:
#              you wore the grass rather than stood in it.)
#
# The tuft sheet is modelled and projected exactly like the soldier and the
# bushes (x right, y forward, z up, `SOLDIER_TILT` off straight-down), so a
# blade, a bush and a rifle all lean the same way. Its frame layout lets the
# engine size a clump with one formula: the clump's ground point sits
# `GRASS_BASE_FRAC` up from the bottom edge (there is room below it for blades
# drooping toward the camera) and a blade of model height 1.0 rises
# `GRASS_RISE_FRAC` of the frame. So drawing grass of world height H means a
# sprite `H * sin(TILT) / GRASS_RISE_FRAC` tall, anchored `GRASS_BASE_FRAC` up
# from its bottom — see `client/src/grass.rs`.
#
# Colour is in the sheets (like the bushes, unlike the rocks); the engine's tint
# is near-white and only drifts the hue from area to area.

GRASS_BASE_FRAC = 0.15   # ground line, up from the bottom edge
GRASS_RISE_FRAC = 0.82   # screen rise of a 1.0-unit blade, as a frame fraction

# Blade colours: green through olive to dry straw. Real grass is never one
# colour, and a lawn-green field reads as felt.
_GRASS_GREENS = [
    (0.21, 0.33, 0.13),
    (0.27, 0.40, 0.15),
    (0.33, 0.45, 0.17),
    (0.38, 0.47, 0.20),
    (0.45, 0.48, 0.23),
    (0.51, 0.50, 0.27),  # dry
]
# Bare earth showing between the blades, and the light the tips catch.
_GRASS_SOIL = (0.16, 0.17, 0.11)
_GRASS_SUN = (0.70, 0.74, 0.48)


def _blade(parts, base, azim, height, bend, rng):
    """One blade of grass: a chain of tapering capsules arcing over.

    Straight blades read as bristles, so the lateral offset grows with the
    square of the height — the blade leaves the ground vertical and tips over
    near the top, which is what makes a patch look soft.
    """
    ux, uy = math.cos(azim), math.sin(azim)
    # Weighted toward the green end. A standing blade is seen whole, unlike the
    # strokes in the ground tile that average against the soil between them, so
    # an even draw from the palette makes every clump read as straw.
    colour = _GRASS_GREENS[rng.choice((0, 0, 1, 1, 1, 2, 2, 3, 4, 5))]
    pale = rng.uniform(0.03, 0.14)
    segs = 3
    nodes = []
    for i in range(segs + 1):
        t = i / segs
        lat = bend * t * t * height
        nodes.append((base[0] + ux * lat, base[1] + uy * lat, height * t * (1.06 - 0.06 * t)))
    for i in range(segs):
        t = (i + 0.5) / segs
        z = nodes[i][2]
        # Dark at the roots, lit at the tips: the sward shades itself, and that
        # vertical ramp does more for depth than any per-blade trick.
        dome = 0.52 + 0.40 * min(1.0, z / 0.85)
        lit = tuple(max(0.0, min(1.0, c * dome)) for c in _mix(colour, _GRASS_SUN, pale * t))
        parts.append((
            nodes[i], nodes[i + 1],
            0.050 * max(0.5, height) * (1.0 - 0.72 * t),
            lit, 1.0,
        ))


def _render_grass_frame(parts, w, h, scale):
    """Project, depth sort and composite blades into (premultiplied rgb, alpha).

    Same projection as the bushes; the frame is `w x h` with the ground line at
    `GRASS_BASE_FRAC` up from the bottom.
    """
    cos_t, sin_t = math.cos(SOLDIER_TILT), math.sin(SOLDIER_TILT)
    base_py = h * (1.0 - GRASS_BASE_FRAC)

    def place(p):
        return (
            w / 2 + p[0] * scale,
            base_py - (p[1] * cos_t + p[2] * sin_t) * scale,
            p[1] * sin_t - p[2] * cos_t,          # bigger = further away
        )

    flat = []
    for a, b, r, colour, alpha in parts:
        pax, pay, da = place(a)
        pbx, pby, db = place(b)
        flat.append((max(da, db), pax, pay, pbx, pby, r * scale, colour, alpha))
    flat.sort(key=lambda p: -p[0])

    prem = [[[0.0, 0.0, 0.0] for _ in range(w)] for _ in range(h)]
    acc = [[0.0] * w for _ in range(h)]
    light = (-0.42, -0.50, 0.76)

    for idx, (_, ax, ay, bx, by, r, colour, alpha) in enumerate(flat):
        x0 = max(0, int(min(ax, bx) - r - 2))
        x1 = min(w - 1, int(max(ax, bx) + r + 2))
        y0 = max(0, int(min(ay, by) - r - 2))
        y1 = min(h - 1, int(max(ay, by) + r + 2))
        seed = idx * 5.0
        wobble = min(1.1, r * 0.7)
        for py in range(y0, y1 + 1):
            for px in range(x0, x1 + 1):
                fx, fy = px + 0.5, py + 0.5
                d2, along = _seg_nearest(fx, fy, ax, ay, bx, by)
                d = math.sqrt(d2)
                jitter = (_sample(_JITTER, fx * 0.7 + seed, fy * 0.7) - 0.5) * wobble
                geom = max(0.0, min(1.0, r + jitter - d + 0.5))
                if geom <= 0.0:
                    continue
                nx = (fx - (ax + (bx - ax) * along)) / r
                ny = (fy - (ay + (by - ay) * along)) / r
                nz = math.sqrt(max(0.0, 1.0 - nx * nx - ny * ny))
                lam = nx * light[0] + ny * light[1] + nz * light[2]
                shade = 0.90 + 0.16 * lam
                cov = geom * alpha
                for c in range(3):
                    lit = max(0.0, min(1.0, colour[c] * shade))
                    prem[py][px][c] = prem[py][px][c] * (1 - cov) + lit * cov
                acc[py][px] = acc[py][px] * (1 - cov) + cov
    return prem, acc


def _write_grass_sheet(path, frames, w, h):
    """One row of `(prem, acc)` frames, RGBA."""
    rows = [bytearray() for _ in range(h)]
    for prem, acc in frames:
        for y in range(h):
            row = rows[y]
            for x in range(w):
                a = acc[y][x]
                if a <= 0.004:
                    row += b'\x00\x00\x00\x00'
                    continue
                p = prem[y][x]
                row += bytes(
                    tuple(max(0, min(255, int(p[c] / a * 255))) for c in range(3))
                    + (int(min(1.0, a) * 255),)
                )
    write_png(path, w * len(frames), h, rows, color_type=6)  # RGBA


def gen_tufts(path, w=28, h=48, variants=12):
    """Small clumps of grass standing on the ground, one row of `variants`.

    Deliberately NARROW and only a few blades each: the arena is covered by
    thousands of these rather than a few hundred big ones, so that walking north
    takes you through the grass a clump at a time instead of stepping over
    obvious tussocks. They keep the full model height, though — a clump's height
    is the depth of the grass where it stands, and the whole occlusion model
    rests on that being honest.
    """
    scale = h * GRASS_RISE_FRAC / math.sin(SOLDIER_TILT)
    frames = []
    for v in range(variants):
        rng = random.Random(4400 + v)
        parts = []
        for _ in range(rng.randint(4, 8)):
            azim = rng.uniform(0, 2 * math.pi)
            rad = rng.uniform(0.0, 0.07)
            base = (math.cos(azim) * rad, math.sin(azim) * rad, 0.0)
            # Bend is capped by the narrow frame: a blade that arcs further than
            # this leaves the quad and gets cut off mid-leaf.
            _blade(parts, base, rng.uniform(0, 2 * math.pi),
                   rng.uniform(0.50, 1.0), rng.uniform(0.05, 0.17), rng)
        frames.append(_render_grass_frame(parts, w, h, scale))
    _write_grass_sheet(path, frames, w, h)


def gen_grass_tex(path, size=128):
    """Tileable sward seen from above: the detail texture on the ground mesh.

    Soil showing through a mess of short blades. Every stroke wraps, so the tile
    is seamless in both axes; the engine repeats it in world space and multiplies
    it by a near-white per-area tint.
    """
    rng = random.Random(9090)
    prem = [[list(_GRASS_SOIL) for _ in range(size)] for _ in range(size)]
    # Short strokes on purpose. This layer is the *base* — the sward you see
    # even on thin ground — and the depth of the grass is carried by the tufts
    # standing in it. Draw it with long blades and the whole arena reads as
    # waist-high meadow no matter what `grass_height` says.
    for _ in range(2600):
        x0, y0 = rng.uniform(0, size), rng.uniform(0, size)
        # Blades lean up-screen (away from the camera) far more often than down.
        ang = -math.pi / 2 + rng.uniform(-0.85, 0.85)
        length = rng.uniform(2.2, 5.5)
        x1, y1 = x0 + math.cos(ang) * length, y0 + math.sin(ang) * length
        r = rng.uniform(0.45, 0.85)
        colour = _GRASS_GREENS[rng.randrange(len(_GRASS_GREENS))]
        pale = rng.uniform(0.0, 0.18)
        for py in range(int(min(y0, y1) - r - 2), int(max(y0, y1) + r + 3)):
            for px in range(int(min(x0, x1) - r - 2), int(max(x0, x1) + r + 3)):
                d2, along = _seg_nearest(px + 0.5, py + 0.5, x0, y0, x1, y1)
                cov = max(0.0, min(1.0, r - math.sqrt(d2) + 0.5))
                if cov <= 0.0:
                    continue
                # Tips (the far end of the stroke) catch the light.
                lit = _mix(colour, _GRASS_SUN, pale * along)
                for c in range(3):
                    prem[py % size][px % size][c] = (
                        prem[py % size][px % size][c] * (1 - cov) + lit[c] * cov
                    )
    rows = []
    for y in range(size):
        row = bytearray()
        for x in range(size):
            row += bytes(max(0, min(255, int(c * 255))) for c in prem[y][x])
        rows.append(row)
    write_png(path, size, size, rows, color_type=2)  # RGB


def gen_shade(path, w=64, h=64):
    """The shadow the grass throws on whatever is standing in it: opaque at the
    ground line, gone by the top. Tinted dark and drawn over the sprite, so the
    part of a soldier that isn't hidden outright is still down in the gloom."""
    rows = []
    for y in range(h):
        row = bytearray()
        # Full at the bottom of the frame, gone by the top, with the falloff
        # biased low — grass shade is deepest right down among the roots.
        a = (y / (h - 1)) ** 1.7
        for x in range(w):
            u = abs((x + 0.5) / w * 2 - 1)
            edge = max(0.0, min(1.0, (1.0 - u) / 0.22))
            row += bytes((255, 255, 255, int(a * edge * edge * (3 - 2 * edge) * 255)))
        rows.append(row)
    write_png(path, w, h, rows, color_type=6)  # RGBA


if __name__ == '__main__':
    out = os.path.join(os.path.dirname(__file__), '..', 'client', 'assets')
    os.makedirs(out, exist_ok=True)
    gen_ground(os.path.join(out, 'ground.png'))
    gen_disc(os.path.join(out, 'disc.png'))
    gen_disc(os.path.join(out, 'ring.png'), inner=0.82)
    gen_soldier(os.path.join(out, 'soldier.png'))
    gen_tracer(os.path.join(out, 'tracer.png'))
    gen_crosshair(os.path.join(out, 'crosshair.png'))
    gen_chevron(os.path.join(out, 'chevron.png'))
    gen_rocks(os.path.join(out, 'rocks.png'))
    gen_bushes(os.path.join(out, 'bushes.png'))
    gen_grass_tex(os.path.join(out, 'grass.png'))
    gen_tufts(os.path.join(out, 'tufts.png'))
    gen_shade(os.path.join(out, 'shade.png'))
