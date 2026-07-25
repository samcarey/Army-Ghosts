//! The deterministic game core.
//!
//! Everything here is integer math on i32 fixed-point values (`FP` subunits per
//! world unit; 1 world unit == 1 screen pixel at base zoom). NO floats, no
//! randomness, no wall-clock reads — every peer must compute bit-identical
//! state from the same input stream, because rollback netcode (GGRS) replays
//! and re-simulates ticks constantly. All tick-evolving state lives in
//! rollback-registered components.
//!
//! The crate is generic over the ggrs `Config` (input type is fixed to
//! [`PlayerInput`]) so it never depends on matchbox: the client instantiates it
//! with `PeerId` addresses for p2p and a dummy address type for synctest.

use std::marker::PhantomData;

use bevy::prelude::*;
use bevy_ggrs::ggrs::Config;
use bevy_ggrs::{
    AddRollbackCommandExtension, GgrsSchedule, PlayerInputs, RollbackApp, RollbackFrameRate,
};
use serde::{Deserialize, Serialize};

/// Fixed-point scale: subunits per world unit (pixel).
pub const FP: i32 = 256;
/// Simulation tick rate (GGRS rollback schedule fps).
pub const TICK_HZ: usize = 60;
/// Sessions are built for up to this many players (`?players=N` picks the
/// actual room size, default 2).
pub const MAX_PLAYERS: usize = 8;

/// Arena half-extents in world units (pixels).
pub const ARENA_HALF_W: i32 = 400;
pub const ARENA_HALF_H: i32 = 300;

/// Player movement speed standing, subunits per tick (2 px/tick = 120 px/s).
pub const PLAYER_SPEED: i32 = 2 * FP;
/// Movement speed per stance, subunits per tick. Going lower buys a smaller
/// profile with speed: a crouch costs ~45%, a crawl ~70%. Indexed by
/// [`Stance::level`], so it must have [`STANCE_COUNT`] entries.
pub const STANCE_SPEED: [i32; STANCE_COUNT] = [
    PLAYER_SPEED,
    PLAYER_SPEED * 9 / 16,
    PLAYER_SPEED * 5 / 16,
];
/// Ticks to drop one stance level, and to climb back up one. You are rooted
/// for the whole transition (the stick still turns you) — that dead time is
/// what makes going prone in the open a commitment rather than a free hide.
/// Getting up is the slower half, as it is in life.
pub const STANCE_DOWN_TICKS: u16 = 16;
pub const STANCE_UP_TICKS: u16 = 26;
/// Bullet speed, subunits per tick (16 px/tick = 960 px/s).
pub const BULLET_SPEED: i32 = 16 * FP;
/// Ticks between shots while holding fire (12 ticks = 5 shots/s).
pub const FIRE_COOLDOWN: u16 = 12;
/// Bullet lifetime in ticks.
pub const BULLET_TTL: u16 = 90;
/// Collision radii, world units.
pub const PLAYER_R: i32 = 12;
pub const BULLET_R: i32 = 2;
pub const TARGET_R: i32 = 14;
/// Ticks a target stays "flashed" after a hit (render feedback).
pub const HIT_FLASH_TICKS: u16 = 8;

/// Stance levels, tallest first. Also indexes [`STANCE_SPEED`] and the stance
/// blocks of the sprite sheet.
pub const STANCE_STAND: u8 = 0;
pub const STANCE_CROUCH: u8 = 1;
pub const STANCE_PRONE: u8 = 2;
pub const STANCE_COUNT: usize = 3;

/// How tall a pawn stands in each stance, world units — *physical* height, not
/// the number of screen pixels the sprite covers (the 3/4 projection foreshortens
/// everything vertical by `sin(tilt)`; the client applies that). Measured off the
/// sprite sheet: 64 units is a soldier from boots to helmet, so a pawn is about
/// 2.7 times as tall as they are wide. Crouching only takes a fifth off; going
/// flat takes three quarters, which is the whole reason to do it.
///
/// The sim doesn't read these yet — they exist so that "how much of this pawn is
/// buried in the grass" is one number both peers can agree on, the same way
/// [`Bush`] is foliage the sim knows about but doesn't act on.
pub const STANCE_HEIGHT: [i32; STANCE_COUNT] = [64, 52, 15];

/// The only thing that crosses the network: one player's input for one tick.
/// Kept tiny (ggrs serializes it with serde every tick). Joystick axes are
/// quantized to i8 (-127..=127); `buttons` is a bitflag byte.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct PlayerInput {
    pub move_x: i8,
    pub move_y: i8,
    pub buttons: u8,
}

pub const BTN_FIRE: u8 = 1 << 0;
/// Aiming down sights: the shooter plants their feet (stick only turns them).
pub const BTN_ADS: u8 = 1 << 1;
/// Bits 2-3 carry the stance the player *wants* (0 stand, 1 crouch, 2 prone) —
/// an absolute level, not a "go down" edge. Edge-triggered inputs would need
/// the sim to remember last tick's buttons, which is exactly the kind of hidden
/// state rollback punishes; a level re-sent every tick re-applies identically no
/// matter how often the frame is replayed.
pub const BTN_STANCE_SHIFT: u8 = 2;
pub const BTN_STANCE_MASK: u8 = 0b11 << BTN_STANCE_SHIFT;

impl PlayerInput {
    pub fn fire(&self) -> bool {
        self.buttons & BTN_FIRE != 0
    }
    pub fn ads(&self) -> bool {
        self.buttons & BTN_ADS != 0
    }
    /// The requested stance, clamped: a peer sending 3 must not index anything
    /// out of range on our side.
    pub fn stance(&self) -> u8 {
        ((self.buttons & BTN_STANCE_MASK) >> BTN_STANCE_SHIFT).min(STANCE_PRONE)
    }
    pub fn set_stance(&mut self, level: u8) {
        self.buttons &= !BTN_STANCE_MASK;
        self.buttons |= (level.min(STANCE_PRONE)) << BTN_STANCE_SHIFT;
    }
}

// ── Components (all rollback-registered) ────────────────────────────────────

/// Fixed-point world position.
#[derive(Component, Copy, Clone, Default, Debug, Hash, PartialEq, Eq)]
pub struct Pos {
    pub x: i32,
    pub y: i32,
}

impl Pos {
    pub const fn from_units(x: i32, y: i32) -> Self {
        Self { x: x * FP, y: y * FP }
    }
    /// Render-side conversion (the ONLY place fixed-point meets floats is in
    /// the client's transform sync, via this helper).
    pub fn to_f32(self) -> (f32, f32) {
        (self.x as f32 / FP as f32, self.y as f32 / FP as f32)
    }
}

/// A player pawn, owned by the GGRS player `handle`.
#[derive(Component, Copy, Clone, Default, Debug, Hash)]
pub struct Player {
    pub handle: usize,
}

/// Last non-zero move direction, raw joystick range (-127..=127 per axis).
/// Bullets fire along this. Defaults to "up".
#[derive(Component, Copy, Clone, Debug, Hash)]
pub struct Facing {
    pub x: i32,
    pub y: i32,
}

impl Default for Facing {
    fn default() -> Self {
        Self { x: 0, y: 127 }
    }
}

/// Ticks until this player may fire again.
#[derive(Component, Copy, Clone, Default, Debug, Hash)]
pub struct Cooldown(pub u16);

/// How low the pawn is carrying itself: [`STANCE_STAND`] / [`STANCE_CROUCH`] /
/// [`STANCE_PRONE`]. Sim state (it gates movement speed), so it rolls back with
/// everything else.
///
/// `level` commits the moment the change starts — the figure drops or rises
/// straight away — and `change` is the dead time bought with it. Keeping the
/// destination in `level` means the sprite reacts on the frame you tap, which
/// is what makes the button feel connected; the cost is paid in movement, not
/// in animation lag.
#[derive(Component, Copy, Clone, Default, Debug, Hash)]
pub struct Stance {
    pub level: u8,
    /// Ticks left before the pawn can move again.
    pub change: u16,
}

impl Stance {
    /// Advance one tick toward `wanted`, one level at a time (stand → prone
    /// pays for both legs of the trip). Pure so it can be tested without a
    /// session; the caller roots movement whenever `change` is non-zero.
    pub fn advance(&mut self, wanted: u8) {
        if self.change > 0 {
            self.change -= 1;
            return;
        }
        let wanted = wanted.min(STANCE_PRONE);
        if wanted == self.level {
            return;
        }
        if wanted > self.level {
            self.level += 1;
            self.change = STANCE_DOWN_TICKS;
        } else {
            self.level -= 1;
            self.change = STANCE_UP_TICKS;
        }
    }

    pub fn speed(&self) -> i32 {
        STANCE_SPEED[(self.level as usize).min(STANCE_COUNT - 1)]
    }
}

/// A live bullet; `owner` is the firing player's handle (no self-hits).
#[derive(Component, Copy, Clone, Default, Debug, Hash)]
pub struct Bullet {
    pub owner: usize,
    pub ttl: u16,
    /// Velocity, subunits per tick.
    pub vx: i32,
    pub vy: i32,
}

/// A shootable dummy target. `hits` accumulates; `flash` counts down render
/// feedback ticks after each hit.
#[derive(Component, Copy, Clone, Default, Debug, Hash)]
pub struct Target {
    pub hits: u32,
    pub flash: u16,
}

/// Static cover: a boulder. Solid to players and bullets, and opaque to sight
/// (the client casts its shadow). Placed once by [`rock_layout`] and never
/// moved, but still rollback-registered so a session restart rebuilds the field
/// with everything else.
#[derive(Component, Copy, Clone, Default, Debug, Hash)]
pub struct Rock {
    /// Collision radius, world units.
    pub r: i32,
    /// Look seed: the client derives sprite variant / rotation / tint from it.
    /// Never read by the sim.
    pub seed: u32,
}

/// Concealment: a bush. Unlike a [`Rock`] it stops nothing — you walk and shoot
/// straight through — it only clouds sight, and the client stacks the haze
/// where bushes overlap. (Groundwork for Ghost-War style hiding: the sim knows
/// where the foliage is, it just doesn't act on it yet.)
#[derive(Component, Copy, Clone, Default, Debug, Hash)]
pub struct Bush {
    /// Canopy radius, world units — the concealment circle, not a hitbox.
    pub r: i32,
    /// Look seed: sprite variant / rotation / tint. Never read by the sim.
    pub seed: u32,
}

// ── World setup ─────────────────────────────────────────────────────────────

/// Fixed spawn points (world units), one per handle. Deterministic — every
/// peer spawns the identical world before the session starts ticking.
/// Cardinals first, then corners, all inside the arena walls.
pub const SPAWN_POINTS: [(i32, i32); MAX_PLAYERS] = [
    (-150, 0),
    (150, 0),
    (0, -150),
    (0, 150),
    (-150, -150),
    (150, 150),
    (-150, 150),
    (150, -150),
];

/// Practice dummies sit on the spawn axis: walk straight out from spawn and
/// they're dead ahead (also makes hit registration trivially testable).
pub const TARGET_POINTS: [(i32, i32); 2] = [(-300, 0), (300, 0)];

// ── Procedural rock field ───────────────────────────────────────────────────

/// How many boulders to place (fewer if the layout runs out of room).
pub const ROCK_COUNT: usize = 16;
/// Radii land in `ROCK_R_MIN ..= ROCK_R_MIN + ROCK_R_SPAN`, world units.
const ROCK_R_MIN: i32 = 16;
const ROCK_R_SPAN: i32 = 26;
/// Clear ground kept between a boulder and the arena wall, world units. Must
/// exceed the player diameter (24) or you could be pinched against the wall.
const ROCK_WALL_GAP: i32 = 34;
/// Clear ground kept between two boulders — same reason: every gap in the
/// field has to stay walkable.
const ROCK_GAP: i32 = 34;
/// Elbow room around spawns and practice dummies.
const ROCK_SPAWN_CLEAR: i32 = 40;
const ROCK_TARGET_CLEAR: i32 = 24;
/// Layout seed and the attempt budget for rejection sampling. Both fixed:
/// the field must come out identical on every peer, every run.
const ROCK_SEED: u32 = 0x5EED_0C13;
const ROCK_ATTEMPTS: usize = 800;

/// Deterministic LCG (Numerical Recipes constants). Returns the high 24 bits,
/// which are far better distributed than the low ones.
fn lcg(state: &mut u32) -> u32 {
    *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    *state >> 8
}

fn within(ax: i32, ay: i32, bx: i32, by: i32, d: i32) -> bool {
    let (dx, dy) = ((ax - bx) as i64, (ay - by) as i64);
    dx * dx + dy * dy < (d as i64) * (d as i64)
}

/// The boulder field, in world units: `(x, y, rock)`. Pure integer rejection
/// sampling from a fixed seed — no floats, no RNG crate, no time — so every
/// peer builds the identical arena before the first tick. (`Pos` is checksummed,
/// so a peer that somehow generated a different field desyncs immediately
/// rather than playing a subtly different map.)
pub fn rock_layout() -> Vec<(i32, i32, Rock)> {
    let mut state = ROCK_SEED;
    let mut rocks: Vec<(i32, i32, Rock)> = Vec::new();
    for _ in 0..ROCK_ATTEMPTS {
        if rocks.len() >= ROCK_COUNT {
            break;
        }
        let r = ROCK_R_MIN + (lcg(&mut state) % (ROCK_R_SPAN as u32 + 1)) as i32;
        let span_x = ARENA_HALF_W - r - ROCK_WALL_GAP;
        let span_y = ARENA_HALF_H - r - ROCK_WALL_GAP;
        let x = (lcg(&mut state) % (span_x as u32 * 2 + 1)) as i32 - span_x;
        let y = (lcg(&mut state) % (span_y as u32 * 2 + 1)) as i32 - span_y;
        let seed = lcg(&mut state);

        // Keep the spawn→practice-dummy lane clear: walk straight out from
        // spawn and the target is still dead ahead (and still trivially
        // testable). The middle of the map is fair game.
        if (100..=340).contains(&x.abs()) && y.abs() <= r + 22 {
            continue;
        }
        if SPAWN_POINTS
            .iter()
            .any(|&(sx, sy)| within(x, y, sx, sy, r + PLAYER_R + ROCK_SPAWN_CLEAR))
        {
            continue;
        }
        if TARGET_POINTS
            .iter()
            .any(|&(tx, ty)| within(x, y, tx, ty, r + TARGET_R + ROCK_TARGET_CLEAR))
        {
            continue;
        }
        if rocks
            .iter()
            .any(|&(ox, oy, other)| within(x, y, ox, oy, r + other.r + ROCK_GAP))
        {
            continue;
        }
        rocks.push((x, y, Rock { r, seed }));
    }
    rocks
}

// ── Procedural bush clusters ────────────────────────────────────────────────

/// How many thickets to scatter (fewer if the layout runs out of room).
pub const BUSH_CLUSTERS: usize = 8;
/// Bushes per thicket: `MIN ..= MIN + SPAN`. Generous, because the per-bush
/// rejections below (round the thicket off, stay off the boulders, stay inside
/// the walls) thin every cluster out.
const BUSH_PER_CLUSTER_MIN: u32 = 7;
const BUSH_PER_CLUSTER_SPAN: u32 = 5;
/// Canopy radii, world units.
const BUSH_R_MIN: i32 = 13;
const BUSH_R_SPAN: i32 = 13;
/// How far a bush can sit from its thicket's center. Bushes inside a thicket
/// deliberately overlap — that's what makes the concealment stack.
const BUSH_SPREAD: i32 = 42;
/// Keep thickets apart, off the spawns, and out of the practice lane.
const BUSH_CLUSTER_GAP: i32 = 2 * BUSH_SPREAD + 30;
const BUSH_SPAWN_CLEAR: i32 = 60;
const BUSH_SEED: u32 = 0x0B05_1137;
const BUSH_ATTEMPTS: usize = 3000;

/// The bush field, in world units: `(x, y, bush)`. Same integer rejection
/// sampling as [`rock_layout`] (fixed seed, no floats) — thicket centers first,
/// then a handful of overlapping canopies jittered around each one.
pub fn bush_layout() -> Vec<(i32, i32, Bush)> {
    let rocks = rock_layout();
    let mut state = BUSH_SEED;
    let mut centers: Vec<(i32, i32)> = Vec::new();
    let mut bushes: Vec<(i32, i32, Bush)> = Vec::new();

    for _ in 0..BUSH_ATTEMPTS {
        if centers.len() >= BUSH_CLUSTERS {
            break;
        }
        let margin = BUSH_SPREAD + BUSH_R_MIN + BUSH_R_SPAN + 8;
        let span_x = ARENA_HALF_W - margin;
        let span_y = ARENA_HALF_H - margin;
        let cx = (lcg(&mut state) % (span_x as u32 * 2 + 1)) as i32 - span_x;
        let cy = (lcg(&mut state) % (span_y as u32 * 2 + 1)) as i32 - span_y;
        // Same clear lane the rocks respect, widened by the thicket's reach so
        // no stray canopy drifts into it.
        if (100..=340).contains(&cx.abs()) && cy.abs() <= BUSH_SPREAD + BUSH_R_MIN + BUSH_R_SPAN + 22
        {
            continue;
        }
        if SPAWN_POINTS
            .iter()
            .any(|&(sx, sy)| within(cx, cy, sx, sy, BUSH_SPREAD + BUSH_SPAWN_CLEAR))
        {
            continue;
        }
        if centers
            .iter()
            .any(|&(ox, oy)| within(cx, cy, ox, oy, BUSH_CLUSTER_GAP))
        {
            continue;
        }
        // Thickets grow in open ground. Without this a cluster centered on a
        // boulder loses most of its canopies to the per-bush rock rejection
        // and comes out as two sad shrubs.
        if rocks
            .iter()
            .any(|&(rx, ry, rock)| within(cx, cy, rx, ry, rock.r + BUSH_SPREAD))
        {
            continue;
        }
        centers.push((cx, cy));

        let count = BUSH_PER_CLUSTER_MIN + lcg(&mut state) % (BUSH_PER_CLUSTER_SPAN + 1);
        for _ in 0..count {
            let r = BUSH_R_MIN + (lcg(&mut state) % (BUSH_R_SPAN as u32 + 1)) as i32;
            let dx = (lcg(&mut state) % (BUSH_SPREAD as u32 * 2 + 1)) as i32 - BUSH_SPREAD;
            let dy = (lcg(&mut state) % (BUSH_SPREAD as u32 * 2 + 1)) as i32 - BUSH_SPREAD;
            let seed = lcg(&mut state);
            let (x, y) = (cx + dx, cy + dy);
            // Round the thicket off, and keep foliage off the boulders (a bush
            // growing out of a rock reads as a glitch) and inside the walls.
            if dx * dx + dy * dy > BUSH_SPREAD * BUSH_SPREAD
                || x.abs() + r > ARENA_HALF_W
                || y.abs() + r > ARENA_HALF_H
                || rocks
                    .iter()
                    .any(|&(rx, ry, rock)| within(x, y, rx, ry, rock.r + r))
            {
                continue;
            }
            bushes.push((x, y, Bush { r, seed }));
        }
    }
    bushes
}

// ── Procedural grass ────────────────────────────────────────────────────────
//
// Unlike the rocks and bushes there are no grass *entities*: the whole field is
// one pure function of position, [`grass_height`], so nothing has to be spawned,
// stored, rolled back or checksummed, and any peer (or the client's renderer, or
// a test) can ask how deep the grass is anywhere for the cost of a few
// multiplies. It is integer value noise for the same reason the layouts are
// integer rejection sampling: every peer must get bit-identical answers, and
// float noise on two different machines is exactly the kind of thing that
// wouldn't.
//
// The client draws it and hides pawns in it; the sim doesn't act on it yet.

/// Deepest grass, world units — a shade over the 64-unit standing soldier of
/// [`STANCE_HEIGHT`], so the tallest patches genuinely swallow someone standing
/// up in them. Reached only where every octave peaks together, which is rare.
pub const GRASS_MAX_H: i32 = 72;
/// Octave lattice sizes, world units. The coarsest is deliberately large next
/// to the 800x600 arena — the point of it is a handful of *regions* per map, not
/// a lawn of dots — with two finer octaves for patchiness and break-up. Only a
/// few lattice points fall inside the arena at that scale, so the mix of open
/// and deep ground is as much a property of [`GRASS_SEED`] as of the weights:
/// both were picked together against the assertions in `grass_field_*`.
const GRASS_CELL: i32 = 300;
const GRASS_CELL_MID: i32 = 105;
const GRASS_CELL_FINE: i32 = 38;
const GRASS_SEED: u32 = 0x883D_58B3;

/// Hash a lattice point to a well-mixed u32 (xorshift-multiply, same family as
/// the rest of the layout code — no float, no RNG crate, no state).
fn grass_hash(ix: i32, iy: i32, salt: u32) -> u32 {
    let mut h = (ix as u32)
        .wrapping_mul(0x27D4_EB2D)
        ^ (iy as u32).wrapping_mul(0x1656_67B1)
        ^ salt;
    h ^= h >> 15;
    h = h.wrapping_mul(0x2545_F491);
    h ^ (h >> 13)
}

/// `a + (b - a) * t` with `t` in 0..=FP.
fn lerp_fp(a: i32, b: i32, t: i32) -> i32 {
    a + (b - a) * t / FP
}

/// `t * t * (3 - 2t)` in FP — the same ease the renderer uses, so the noise has
/// no lattice creases in it. Peaks at exactly FP, so nothing can overflow the
/// interpolations above it.
fn smoothstep_fp(t: i32) -> i32 {
    t * t / FP * (3 * FP - 2 * t) / FP
}

/// One octave of value noise: bilinear interpolation between hashed lattice
/// corners, eased. Returns 0..=FP.
fn value_noise(x: i32, y: i32, cell: i32, salt: u32) -> i32 {
    let (ix, iy) = (x.div_euclid(cell), y.div_euclid(cell));
    // rem_euclid, so the lattice doesn't mirror across the origin (the arena
    // straddles it — a seam down x=0 would be very visible).
    let u = smoothstep_fp(x.rem_euclid(cell) * FP / cell);
    let v = smoothstep_fp(y.rem_euclid(cell) * FP / cell);
    let corner = |dx, dy| ((grass_hash(ix + dx, iy + dy, salt) >> 8 & 0xFF) as i32) * FP / 255;
    let top = lerp_fp(corner(0, 0), corner(1, 0), u);
    let bottom = lerp_fp(corner(0, 1), corner(1, 1), u);
    lerp_fp(top, bottom, v)
}

/// How deep the grass is at a world point, in world units (0..=[`GRASS_MAX_H`]).
///
/// Three octaves of value noise — broad meadows, patchiness inside them, and a
/// fine break-up so no edge reads as a contour line — then two corrections,
/// both of which the field is bad without:
///
/// * **Contrast.** Summed octaves pile up around the middle, so the raw sum is
///   shin-deep almost everywhere: no cover *and* no open ground. Stretching the
///   middle half of the range over the whole output, eased at both ends, is what
///   turns it into meadows and clearings (the test asserts the mix).
/// * **Bias.** Even then, splitting the map evenly between deep and thin would
///   make deep grass the default. Averaging the stretched value with its own
///   square tilts the whole field toward thin ground, so the deep patches stay
///   somewhere you move *to*.
pub fn grass_height(x: i32, y: i32) -> i32 {
    let n = (value_noise(x, y, GRASS_CELL, GRASS_SEED) * 100
        + value_noise(x, y, GRASS_CELL_MID, GRASS_SEED ^ 0x9E37_79B9) * 34
        + value_noise(x, y, GRASS_CELL_FINE, GRASS_SEED ^ 0x85EB_CA6B) * 12)
        / 146;
    let stretched = smoothstep_fp(((n - FP / 4) * 2).clamp(0, FP));
    let shaped = (stretched * stretched / FP + stretched) / 2;
    GRASS_MAX_H * shaped / FP
}

/// What fraction of a pawn standing at `(x, y)` in this stance the grass
/// swallows, 0..=FP. Falls straight out of the two heights: the grass hides
/// everything below its own tips, so going flat is worth far more than any
/// stance bonus would be — 15 units of prone soldier disappears in grass that
/// barely reaches a standing one's knees.
pub fn grass_cover(x: i32, y: i32, stance: u8) -> i32 {
    let body = STANCE_HEIGHT[(stance as usize).min(STANCE_COUNT - 1)];
    (grass_height(x, y) * FP / body).min(FP)
}

/// Spawn the initial world: one pawn per player, the practice targets, and the
/// procedural rock and bush fields. Both clients run this identically before
/// the first tick.
pub fn spawn_world(commands: &mut Commands, num_players: usize) {
    for handle in 0..num_players {
        let (x, y) = SPAWN_POINTS[handle];
        commands
            .spawn((
                Player { handle },
                Pos::from_units(x, y),
                Facing::default(),
                Cooldown::default(),
                Stance::default(),
            ))
            .add_rollback();
    }
    for (x, y) in TARGET_POINTS {
        commands
            .spawn((Target::default(), Pos::from_units(x, y)))
            .add_rollback();
    }
    for (x, y, rock) in rock_layout() {
        commands
            .spawn((rock, Pos::from_units(x, y)))
            .add_rollback();
    }
    for (x, y, bush) in bush_layout() {
        commands
            .spawn((bush, Pos::from_units(x, y)))
            .add_rollback();
    }
}

// ── Integer math helpers ────────────────────────────────────────────────────

/// Deterministic integer square root (Newton's method).
pub fn isqrt(n: i64) -> i64 {
    if n <= 0 {
        return 0;
    }
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

fn dist2(a: Pos, b: Pos) -> i64 {
    let dx = (a.x - b.x) as i64;
    let dy = (a.y - b.y) as i64;
    dx * dx + dy * dy
}

fn radius_fp(r_units: i32) -> i64 {
    (r_units * FP) as i64
}

// ── The plugin ──────────────────────────────────────────────────────────────

/// Add to an app that already has `GgrsPlugin::<C>` installed. Registers all
/// sim components for rollback + checksums and installs the fixed-tick systems.
pub struct SimPlugin<C>(PhantomData<C>);

impl<C> Default for SimPlugin<C> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<C: Config<Input = PlayerInput>> Plugin for SimPlugin<C> {
    fn build(&self, app: &mut App) {
        app.insert_resource(RollbackFrameRate(TICK_HZ))
            .rollback_component_with_copy::<Pos>()
            .rollback_component_with_copy::<Player>()
            .rollback_component_with_copy::<Facing>()
            .rollback_component_with_copy::<Cooldown>()
            .rollback_component_with_copy::<Stance>()
            .rollback_component_with_copy::<Bullet>()
            .rollback_component_with_copy::<Target>()
            .rollback_component_with_copy::<Rock>()
            .rollback_component_with_copy::<Bush>()
            // Desync detection: checksum the position state every frame so a
            // nondeterminism bug surfaces as a GGRS desync event immediately,
            // not as subtly diverged worlds.
            .checksum_component_with_hash::<Pos>()
            // Stance too: a divergent stance eventually shows up in `Pos`
            // anyway (it scales movement), but only once that player moves —
            // checksumming it catches the disagreement on the tick it happens.
            .checksum_component_with_hash::<Stance>()
            .add_systems(
                GgrsSchedule,
                (
                    move_players::<C>,
                    fire_bullets::<C>,
                    move_bullets,
                    resolve_hits,
                    tick_targets,
                )
                    .chain(),
            );
    }
}

// ── Fixed-tick systems (run inside the GGRS rollback schedule) ──────────────

fn move_players<C: Config<Input = PlayerInput>>(
    inputs: Res<PlayerInputs<C>>,
    mut players: Query<(&Player, &mut Pos, &mut Facing, &mut Stance)>,
    rocks: Query<(&Rock, &Pos), Without<Player>>,
) {
    // Sorted: resolving overlaps in a different order could land a pinched
    // player on a different subunit, and query iteration order is not a
    // determinism guarantee.
    let mut cover: Vec<(i32, i32, i32)> = rocks
        .iter()
        .map(|(rock, pos)| (pos.x, pos.y, rock.r))
        .collect();
    cover.sort_unstable();

    for (player, mut pos, mut facing, mut stance) in &mut players {
        let (input, _status) = inputs[player.handle];
        // Stance first: the requested level rides in the input bits, so every
        // peer starts (and finishes) the same transition on the same tick.
        stance.advance(input.stance());
        let (mx, my) = (input.move_x as i32, input.move_y as i32);
        if mx == 0 && my == 0 {
            continue;
        }
        facing.x = mx;
        facing.y = my;
        // Aiming down sights roots the shooter in place — the stick still
        // turns them (that's the aim), it just doesn't carry them anywhere.
        // The bit rides in the input stream, so every peer agrees. Changing
        // stance roots them the same way, for as long as it takes.
        if input.ads() || stance.change > 0 {
            continue;
        }
        // Scale the joystick vector to at most the stance's speed, preserving
        // direction: v = m * SPEED / max(len, 127). Dividing by the *longer*
        // of len/127 keeps sub-max joystick deflections proportional while
        // capping diagonals at full speed.
        let speed = stance.speed();
        let len = isqrt((mx * mx + my * my) as i64).max(127) as i32;
        pos.x += mx * speed / len;
        pos.y += my * speed / len;
        push_out_of_cover(&mut pos, &cover);
        pos.x = pos.x.clamp(-(ARENA_HALF_W - PLAYER_R) * FP, (ARENA_HALF_W - PLAYER_R) * FP);
        pos.y = pos.y.clamp(-(ARENA_HALF_H - PLAYER_R) * FP, (ARENA_HALF_H - PLAYER_R) * FP);
    }
}

/// Boulders are solid: shove `pos` back out to each one's surface along the
/// normal. Cancelling only the into-the-rock component is what makes an angled
/// approach *deflect* — the along-the-surface part of the step survives, so you
/// slide around the rock instead of stopping dead against it.
///
/// `cover` is `(x, y, radius)` in the same order on every peer (see the sort in
/// [`move_players`]); resolving in a different order could settle a pinched
/// player on a different subunit.
fn push_out_of_cover(pos: &mut Pos, cover: &[(i32, i32, i32)]) {
    for &(rx, ry, r) in cover {
        let reach = ((r + PLAYER_R) * FP) as i64;
        let (dx, dy) = ((pos.x - rx) as i64, (pos.y - ry) as i64);
        let d2 = dx * dx + dy * dy;
        if d2 >= reach * reach {
            continue;
        }
        let d = isqrt(d2);
        if d == 0 {
            // Dead center (only reachable by starting inside): any fixed
            // direction will do, as long as every peer picks the same one.
            pos.y = ry + reach as i32;
            continue;
        }
        pos.x = rx + (dx * reach / d) as i32;
        pos.y = ry + (dy * reach / d) as i32;
    }
}

fn fire_bullets<C: Config<Input = PlayerInput>>(
    mut commands: Commands,
    inputs: Res<PlayerInputs<C>>,
    mut players: Query<(&Player, &Pos, &Facing, &mut Cooldown)>,
) {
    for (player, pos, facing, mut cooldown) in &mut players {
        if cooldown.0 > 0 {
            cooldown.0 -= 1;
        }
        let (input, _status) = inputs[player.handle];
        if !input.fire() || cooldown.0 > 0 {
            continue;
        }
        cooldown.0 = FIRE_COOLDOWN;
        let len = isqrt((facing.x * facing.x + facing.y * facing.y) as i64).max(1) as i32;
        let vx = facing.x * BULLET_SPEED / len;
        let vy = facing.y * BULLET_SPEED / len;
        // Spawn just outside the player's own radius so the bullet never
        // overlaps its shooter.
        let offset = (PLAYER_R + BULLET_R + 2) * FP;
        let start = Pos {
            x: pos.x + facing.x * offset / len,
            y: pos.y + facing.y * offset / len,
        };
        commands
            .spawn((
                Bullet { owner: player.handle, ttl: BULLET_TTL, vx, vy },
                start,
            ))
            .add_rollback();
    }
}

fn move_bullets(
    mut commands: Commands,
    mut bullets: Query<(Entity, &mut Bullet, &mut Pos)>,
    rocks: Query<(&Rock, &Pos), Without<Bullet>>,
) {
    for (entity, mut bullet, mut pos) in &mut bullets {
        pos.x += bullet.vx;
        pos.y += bullet.vy;
        bullet.ttl = bullet.ttl.saturating_sub(1);
        let out = pos.x.abs() > ARENA_HALF_W * FP || pos.y.abs() > ARENA_HALF_H * FP;
        // Rounds stop in cover, same as sight does. Order-independent (the
        // outcome is just "despawned"), so no sort needed here.
        let blocked = rocks.iter().any(|(rock, rock_pos)| {
            let reach = radius_fp(rock.r + BULLET_R);
            dist2(*pos, *rock_pos) <= reach * reach
        });
        if bullet.ttl == 0 || out || blocked {
            commands.entity(entity).despawn();
        }
    }
}

fn resolve_hits(
    mut commands: Commands,
    bullets: Query<(Entity, &Pos), With<Bullet>>,
    mut targets: Query<(&mut Target, &Pos)>,
) {
    for (bullet_entity, bullet_pos) in &bullets {
        for (mut target, target_pos) in &mut targets {
            let reach = radius_fp(TARGET_R + BULLET_R);
            if dist2(*bullet_pos, *target_pos) <= reach * reach {
                target.hits += 1;
                target.flash = HIT_FLASH_TICKS;
                commands.entity(bullet_entity).despawn();
                break;
            }
        }
    }
}

fn tick_targets(mut targets: Query<&mut Target>) {
    for mut target in &mut targets {
        if target.flash > 0 {
            target.flash -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deepest grass a crouching soldier still stands out of. A match arm needs
    /// a constant and `STANCE_HEIGHT[STANCE_CROUCH as usize] - 1` isn't one.
    const CROUCH_TOPS_OUT: i32 = 51;

    /// Walking straight at a boulder stops you; walking at it on an angle
    /// slides you around it (the whole point of pushing out along the normal).
    #[test]
    fn cover_deflects_angled_approach() {
        let cover = [(0, 0, 30)]; // rock at the origin, radius 30
        let reach = ((30 + PLAYER_R) * FP) as i64;
        let dist = |p: Pos| isqrt((p.x as i64).pow(2) + (p.y as i64).pow(2));

        // Head-on from the left: pinned at the surface, no sideways drift.
        let mut head_on = Pos { x: -reach as i32 + 5 * FP, y: 0 };
        push_out_of_cover(&mut head_on, &cover);
        assert_eq!(head_on.y, 0, "head-on approach must not slide");
        assert!((dist(head_on) - reach).abs() <= 1, "must end on the surface");

        // Same rock, approached up-and-right from the left side: the into-the-
        // rock part is cancelled, the along-the-surface part survives.
        let mut angled = Pos { x: -reach as i32 + 5 * FP, y: FP };
        push_out_of_cover(&mut angled, &cover);
        assert!(angled.y > FP, "angled approach must deflect along the surface");
        assert!((dist(angled) - reach).abs() <= 1, "must end on the surface");

        // Clear of the rock: untouched.
        let mut clear = Pos { x: 200 * FP, y: 0 };
        push_out_of_cover(&mut clear, &cover);
        assert_eq!((clear.x, clear.y), (200 * FP, 0));
    }

    /// Stance changes go one level at a time, root the pawn for the whole
    /// transition, and cost speed for as long as you stay down.
    #[test]
    fn stance_steps_one_level_at_a_time() {
        let mut stance = Stance::default();
        assert_eq!(stance.speed(), PLAYER_SPEED);

        // Asking for prone from standing passes through crouch, paying the
        // dead time for each leg.
        stance.advance(STANCE_PRONE);
        assert_eq!((stance.level, stance.change), (STANCE_CROUCH, STANCE_DOWN_TICKS));
        for _ in 0..STANCE_DOWN_TICKS {
            stance.advance(STANCE_PRONE);
        }
        assert_eq!(stance.change, 0, "transition must end on its own tick count");
        stance.advance(STANCE_PRONE);
        assert_eq!((stance.level, stance.change), (STANCE_PRONE, STANCE_DOWN_TICKS));

        // Holding the request steady once you're there is a no-op.
        for _ in 0..STANCE_DOWN_TICKS + 5 {
            stance.advance(STANCE_PRONE);
        }
        assert_eq!((stance.level, stance.change), (STANCE_PRONE, 0));

        // Getting up is the slower half.
        stance.advance(STANCE_STAND);
        assert_eq!((stance.level, stance.change), (STANCE_CROUCH, STANCE_UP_TICKS));

        // A garbage level from a peer must not index out of the table.
        let mut input = PlayerInput::default();
        input.set_stance(9);
        assert_eq!(input.stance(), STANCE_PRONE);
        assert!(STANCE_SPEED[STANCE_PRONE as usize] < STANCE_SPEED[STANCE_CROUCH as usize]);
        assert!(STANCE_SPEED[STANCE_CROUCH as usize] < STANCE_SPEED[STANCE_STAND as usize]);
    }

    /// The stance bits have to survive a round trip alongside the other
    /// buttons — they share one byte with fire and sights.
    #[test]
    fn stance_bits_do_not_collide() {
        let mut input = PlayerInput { buttons: BTN_FIRE | BTN_ADS, ..default() };
        input.set_stance(STANCE_CROUCH);
        assert!(input.fire() && input.ads());
        assert_eq!(input.stance(), STANCE_CROUCH);
        input.set_stance(STANCE_STAND);
        assert!(input.fire() && input.ads());
        assert_eq!(input.stance(), STANCE_STAND);
    }

    /// Thickets have to actually land: the per-bush rejections (round the
    /// cluster off, stay off the boulders, stay inside the walls) can quietly
    /// eat most of a cluster if the constants drift.
    #[test]
    fn bush_field_is_sane() {
        let bushes = bush_layout();
        println!("{} bushes", bushes.len());
        assert!(
            bushes.len() >= BUSH_CLUSTERS * 3,
            "thickets came out too thin: {} bushes",
            bushes.len()
        );
        let rocks = rock_layout();
        for &(x, y, bush) in &bushes {
            assert!(x.abs() + bush.r <= ARENA_HALF_W && y.abs() + bush.r <= ARENA_HALF_H);
            for &(rx, ry, rock) in &rocks {
                assert!(!within(x, y, rx, ry, rock.r + bush.r), "bush inside a rock");
            }
        }
    }

    /// The grass has to be worth having: thin ground you can be caught on,
    /// deep patches that hide a standing soldier, and most of the arena in
    /// between. A curve tweak that quietly flattens the field into uniform
    /// shin-deep grass would still *look* fine in a screenshot.
    #[test]
    fn grass_field_has_thin_and_deep_ground() {
        let (mut thin, mut mid, mut deep, mut total) = (0, 0, 0, 0);
        let (mut min, mut max) = (GRASS_MAX_H, 0);
        let mut y = -ARENA_HALF_H;
        while y <= ARENA_HALF_H {
            let mut x = -ARENA_HALF_W;
            while x <= ARENA_HALF_W {
                let h = grass_height(x, y);
                assert!((0..=GRASS_MAX_H).contains(&h), "grass out of range at {x},{y}: {h}");
                min = min.min(h);
                max = max.max(h);
                // Ankle-deep / knee-to-waist / over a crouching soldier.
                match h {
                    0..=9 => thin += 1,
                    10..=CROUCH_TOPS_OUT => mid += 1,
                    _ => deep += 1,
                }
                total += 1;
                x += 4;
            }
            y += 4;
        }
        println!("grass {min}..{max}: {thin} thin / {mid} mid / {deep} deep of {total}");
        assert!(min <= 6, "nowhere in the arena is the grass thin: min {min}");
        assert!(max >= 56, "no deep patches anywhere: max {max}");
        // Deep cover should be a place you go to, not the default state.
        assert!(deep * 100 / total >= 3, "too little deep grass");
        assert!(deep * 100 / total <= 30, "too much deep grass");
        assert!(thin * 100 / total >= 5, "nowhere is open enough to be exposed");
    }

    /// "Smooth transition between areas" is the whole spec of the field: a
    /// player walking a straight line must never step over a wall of grass.
    #[test]
    fn grass_transitions_are_smooth() {
        let mut worst = 0;
        let mut y = -ARENA_HALF_H;
        while y <= ARENA_HALF_H {
            let mut x = -ARENA_HALF_W;
            while x < ARENA_HALF_W {
                let step = (grass_height(x + 2, y) - grass_height(x, y)).abs();
                let up = (grass_height(x, y + 2) - grass_height(x, y)).abs();
                worst = worst.max(step).max(up);
                x += 2;
            }
            y += 2;
        }
        // 2 world units is a sixth of a pawn's radius. At the steepest point in
        // the arena the grass deepens by about 2 units per unit walked — knee to
        // waist over a pawn's width, which is the edge of a thicket rather than
        // a seam. Anything much past that and the noise is showing its lattice.
        println!("worst grass step over 2 units: {worst}");
        assert!(worst <= 4, "grass steps {worst} units over 2 — visible seam");
    }

    /// Going flat is what the grass is for.
    #[test]
    fn grass_hides_a_prone_pawn_first() {
        let mut sampled = 0;
        let mut prone_hidden = 0;
        let mut stand_hidden = 0;
        let mut y = -ARENA_HALF_H;
        while y <= ARENA_HALF_H {
            let mut x = -ARENA_HALF_W;
            while x <= ARENA_HALF_W {
                let (flat, up) = (grass_cover(x, y, STANCE_PRONE), grass_cover(x, y, STANCE_STAND));
                assert!(flat >= up, "prone must never be more exposed than standing");
                assert!((0..=FP).contains(&up));
                prone_hidden += (flat == FP) as i32;
                stand_hidden += (up == FP) as i32;
                sampled += 1;
                x += 8;
            }
            y += 8;
        }
        println!("fully hidden: prone {prone_hidden}, standing {stand_hidden} of {sampled}");
        assert!(prone_hidden * 2 > sampled, "crawling should hide you over most of the map");
        assert!(stand_hidden * 20 < sampled, "standing should almost never be free cover");
    }

    #[test]
    fn rock_field_is_sane() {
        let rocks = rock_layout();
        println!("{} rocks", rocks.len());
        for (x, y, rock) in &rocks {
            println!("  ({x:>4},{y:>4}) r={}", rock.r);
        }
        assert_eq!(rocks.len(), ROCK_COUNT, "layout ran out of room");
        // Every gap walkable, nothing on a spawn or in the practice lane.
        for (i, &(x, y, r)) in rocks.iter().enumerate() {
            assert!(x.abs() + r.r <= ARENA_HALF_W, "rock {i} out of bounds");
            assert!(y.abs() + r.r <= ARENA_HALF_H, "rock {i} out of bounds");
            for &(sx, sy) in &SPAWN_POINTS {
                assert!(!within(x, y, sx, sy, r.r + PLAYER_R), "rock {i} on a spawn");
            }
            for (j, &(ox, oy, o)) in rocks.iter().enumerate().skip(i + 1) {
                assert!(!within(x, y, ox, oy, r.r + o.r + 2 * PLAYER_R), "rocks {i}/{j} pinch");
            }
        }
    }
}
