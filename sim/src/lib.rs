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

pub mod bot;
pub use bot::{bot_think, Bot, BotProfile, BotRoster, MEMORY_TICKS};

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

/// Hit points a pawn spawns with.
pub const MAX_HEALTH: i32 = 100;
/// Damage of a perfect round: dead center, point blank. Three of those kill,
/// which at [`FIRE_COOLDOWN`] is 0.6s of holding the trigger on someone —
/// short enough to punish standing in the open, long enough that the first
/// round is a warning rather than a verdict.
pub const HIT_DAMAGE_MAX: i32 = 42;
/// What's left of that at the very edge of the hitbox, as a fraction of `FP`.
/// A graze is worth having (it still costs the target a third of a good hit)
/// without being worth aiming for.
pub const DAMAGE_EDGE_FRAC: i32 = FP * 30 / 100;
/// Range falloff, world units: full damage out to `NEAR`, then linearly down
/// to `FAR_FRAC` at `FAR` and flat beyond. `FAR` is a little under the arena's
/// long diagonal, so a cross-map round always lands in the floor.
pub const DAMAGE_NEAR: i32 = 120;
pub const DAMAGE_FAR: i32 = 520;
pub const DAMAGE_FAR_FRAC: i32 = FP * 45 / 100;
/// Ticks between dying and standing back up at your spawn (1.5s). You are
/// frozen, unhittable and hidden for the whole count.
pub const RESPAWN_TICKS: u16 = 90;
/// Ticks a pawn flashes after taking a round (render feedback, like
/// [`HIT_FLASH_TICKS`] on the dummies).
pub const HURT_FLASH_TICKS: u16 = 9;

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
/// Bits 4-7 carry how many bots the *first* player wants in the match, `0..=8`.
///
/// It rides in the input stream for exactly the reason the stance level does,
/// and it is worth spelling out because "read it from a resource the menu
/// writes" is the obvious implementation and is wrong: a rollback re-runs a
/// tick from a restored snapshot, and a resource the UI has changed in the
/// meantime makes that re-run differ from the original. An **absolute count**
/// carried in the inputs is idempotent — replaying the tick reconciles to the
/// same number however many times it happens — and it arrives on every peer
/// through the one channel they already agree on.
///
/// Only the first player's copy is honoured ([`reconcile_bots`]); everyone
/// else's is ignored, so two people pressing the button can't fight over it.
pub const BTN_BOTS_SHIFT: u8 = 4;
pub const BTN_BOTS_MASK: u8 = 0b1111 << BTN_BOTS_SHIFT;

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
    /// How many bots this player is asking for, clamped so a peer sending 15
    /// can't ask for more pawns than there are spawn points.
    pub fn bots(&self) -> u8 {
        ((self.buttons & BTN_BOTS_MASK) >> BTN_BOTS_SHIFT).min(MAX_PLAYERS as u8)
    }
    pub fn set_bots(&mut self, count: u8) {
        self.buttons &= !BTN_BOTS_MASK;
        self.buttons |= count.min(MAX_PLAYERS as u8) << BTN_BOTS_SHIFT;
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

/// A pawn. `handle` is its identity everywhere in the sim — who fired a round,
/// whose deaths these are, which spawn point it comes back at.
///
/// For a HUMAN pawn it is also the GGRS player handle, and `0..num_players` are
/// exactly the human pawns. A BOT pawn carries one too, allocated above that
/// range, because everything downstream (`Bullet::owner`, `Deaths`, the roster,
/// `SPAWN_POINTS`) wants a small unique id and there is no reason for bots to
/// need a second kind. What a bot does NOT have is a seat in the session: see
/// [`Intent`] for how it is driven instead.
#[derive(Component, Copy, Clone, Default, Debug, Hash)]
pub struct Player {
    pub handle: usize,
}

/// What a pawn is trying to do this tick, in exactly the form a human's
/// controller produces.
///
/// This is the seam that lets bots exist at all. `move_players` and
/// `fire_bullets` used to index `PlayerInputs[player.handle]` directly, which
/// tied "is a pawn" to "has a seat in the GGRS session" — a bot would have
/// needed a network handle, and someone would have had to send its inputs.
/// Now the two systems read an `Intent` and don't care where it came from:
/// [`read_human_intent`] copies it off the wire for human pawns, and the bot
/// brain computes it from the rolled-back world for bot pawns. Since every peer
/// simulates every pawn from identical state, every peer computes identical bot
/// intents — zero bandwidth, and no peer is authoritative over a bot.
///
/// Rollback-registered even though it is rewritten at the head of every tick
/// and so cannot strictly go stale: a bot that ever wants hysteresis (holding a
/// heading, committing to a rush) would evolve it, and finding out then would
/// mean finding out as a desync.
#[derive(Component, Copy, Clone, Default, Debug)]
pub struct Intent(pub PlayerInput);

// `Bot` — the component, its profile and its brain — lives in `bot.rs`.

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

/// A pawn's condition. `hp` runs to zero, `down` is the respawn countdown that
/// starts when it gets there, and `hurt` is render feedback.
///
/// While `down > 0` the pawn is out of the game entirely: it can't move, fire,
/// change stance or be hit, and the client hides it. That's deliberately one
/// flag rather than despawning the entity — a rollback that un-kills someone
/// then only has to restore a component, instead of resurrecting an entity
/// whose identity the renderer has already forgotten.
#[derive(Component, Copy, Clone, Debug, Hash)]
pub struct Health {
    pub hp: i32,
    /// Ticks until respawn; 0 means alive.
    pub down: u16,
    /// Ticks left of the hit flash.
    pub hurt: u16,
}

impl Default for Health {
    fn default() -> Self {
        Self { hp: MAX_HEALTH, down: 0, hurt: 0 }
    }
}

impl Health {
    pub fn alive(&self) -> bool {
        self.down == 0
    }
    /// 0..=FP — what the health bar draws.
    pub fn fraction(&self) -> i32 {
        (self.hp.max(0) * FP / MAX_HEALTH).min(FP)
    }
}

/// How many times this pawn has been killed. Sim state (it's decided by hits,
/// which are decided by the input stream), so every peer's scoreboard agrees
/// without anyone sending a score message.
#[derive(Component, Copy, Clone, Default, Debug, Hash)]
pub struct Deaths(pub u32);

/// How many pawns this one has put down. The other half of the scoreboard, and
/// the reason the self-play harness can tell a good bot from a hidden one:
/// score a match on deaths alone and the winner is whoever turtled hardest.
///
/// Every death is credited, including a bot shooting its own side — the
/// harness's teams are bookkeeping the sim knows nothing about, and a kill it
/// declined to count would quietly reward friendly fire.
#[derive(Component, Copy, Clone, Default, Debug, Hash)]
pub struct Kills(pub u32);

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

/// The band the whole field lives in, world units. Every tile is THICK — nowhere
/// is bare, nowhere is over a standing soldier's head ([`STANCE_HEIGHT`] is
/// 64/52/15) — so the variety between tiles is about what a CROUCHING pawn can
/// do: the deep end buries one completely, the shallow end leaves them showing.
///
/// This replaced a 0..72 continuous field with real clearings in it. The old one
/// looked more dramatic and played worse: cover you could only find in patches
/// meant most ground was worthless, and thin ground left a prone pawn exposed
/// with no way to read where. Keeping the whole band deep, and quantizing it to
/// the tiles the fog already draws, is what makes the depth legible — a tile is
/// a thing you can look at and judge.
pub const GRASS_MIN_H: i32 = 0;
pub const GRASS_MAX_H: i32 = 62;
/// The shallowest a GRASSY tile can be, world units. Nothing lands between 0 and
/// this: a tile either has grass or it doesn't, which is what makes bare ground
/// read as a place rather than as the field thinning out.
pub const GRASS_BARE_BELOW: i32 = 14;
/// How much of the noise's bottom end comes out bare, as a fraction of `FP`.
/// Tuned against the histogram in `grass_field_has_a_mix_of_depths` for about a
/// tenth of the map.
///
/// This is a separate decision from the depth mapping, and it has to be: fold
/// bare ground into the bottom of one 0..`GRASS_MAX_H` range and the whole
/// range shifts down with it, which took the tiles deep enough to bury a
/// crouching pawn from 8% of the map to 3%. Bare ground and how deep grass gets
/// are different questions.
///
/// Applied BEFORE the per-tile jitter, so bare tiles come out in coherent
/// patches the way the noise does; jittering first speckles single bare tiles
/// through otherwise grassy ground.
const GRASS_BARE_SPREAD: i32 = FP / 14;
/// How far a single tile may sit off what the noise says, world units — the
/// per-tile break-up that keeps neighbouring tiles distinguishable even where
/// the underlying noise is flat. A sixth of the band: enough that adjacent tiles
/// visibly differ (which is the whole point of quantizing), not so much that the
/// honeycomb stops following the terrain underneath it.
const GRASS_TILE_JITTER: u32 = 6;
/// Octave lattice sizes, world units. The coarsest is deliberately large next
/// to the 800x600 arena — the point of it is a handful of *regions* per map, not
/// a lawn of dots — with two finer octaves for break-up. Their weights fall off
/// steeply (100/34/12) so the fine detail stays a texture: the depth under your
/// feet must never jump as you walk, which `grass_transitions_are_smooth`
/// asserts.
const GRASS_CELL: i32 = 300;
const GRASS_CELL_MID: i32 = 105;
const GRASS_CELL_FINE: i32 = 38;
const GRASS_SEED: u32 = 0x883D_58B3;

/// Hex tile circumradius, world units — corner to corner is `2 * HEX_R`, flat to
/// flat `sqrt(3) * HEX_R`. The client's fog paints these tiles and the grass is
/// uniform within one, so the two must be the same grid: the size lives here and
/// `vision.rs` reads it.
pub const HEX_R: i32 = 16;
/// Column pitch, world units (`1.5 * HEX_R`), and row pitch in FP subunits
/// (`sqrt(3) * HEX_R`, which is irrational and so has to live in fixed point).
/// Odd columns drop half a row, which is what interlocks the grid.
const HEX_COL: i32 = HEX_R * 3 / 2;
const HEX_ROW_FP: i32 = 7094;

/// Centre of the hex at odd-q offset coordinates `(col, row)`, in FP.
fn hex_centre_fp(col: i32, row: i32) -> (i32, i32) {
    (
        (-ARENA_HALF_W + col * HEX_COL) * FP,
        -ARENA_HALF_H * FP
            + row * HEX_ROW_FP
            + if col.rem_euclid(2) == 1 { HEX_ROW_FP / 2 } else { 0 },
    )
}

/// Which hex tile a world point falls in, as `(col, row)`.
///
/// By nearest centre, which IS hex containment: the Voronoi cells of a hex
/// lattice are its hexes. Nine candidates is enough — the true cell is always
/// within one column and one row of the rounded guess — and it avoids the
/// cube-rounding dance that the usual pixel-to-hex conversion needs, which is
/// awkward to do exactly in integers.
pub fn hex_cell(x: i32, y: i32) -> (i32, i32) {
    let (px, py) = (x as i64 * FP as i64, y as i64 * FP as i64);
    let col0 = (x + ARENA_HALF_W).div_euclid(HEX_COL);
    let row0 = ((y + ARENA_HALF_H) as i64 * FP as i64).div_euclid(HEX_ROW_FP as i64) as i32;
    let mut best = (i64::MAX, col0, row0);
    for col in col0 - 1..=col0 + 1 {
        for row in row0 - 1..=row0 + 1 {
            let (cx, cy) = hex_centre_fp(col, row);
            let (dx, dy) = (px - cx as i64, py - cy as i64);
            let d2 = dx * dx + dy * dy;
            // Ties broken by the lowest (col, row) so the seam between two
            // equidistant tiles always falls the same way.
            if d2 < best.0 {
                best = (d2, col, row);
            }
        }
    }
    (best.1, best.2)
}

/// Centre of a hex tile in whole world units — where the grass field is sampled
/// for the whole tile.
fn hex_centre(col: i32, row: i32) -> (i32, i32) {
    let (x, y) = hex_centre_fp(col, row);
    ((x + FP / 2).div_euclid(FP), (y + FP / 2).div_euclid(FP))
}

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

/// How deep the grass is at a world point, world units
/// ([`GRASS_MIN_H`]..=[`GRASS_MAX_H`]).
///
/// **Constant across a hex tile, different from tile to tile.** The noise is
/// sampled once at the tile's centre and that answer covers the whole tile, so
/// the map is a honeycomb of even swards rather than either a continuous slope
/// or a scatter of clumps — you can look at a tile and know what it costs you to
/// cross it, which is the point of quantizing anything.
///
/// Variety comes at three scales, and all three are wanted:
///   * **Area to area** — the coarse octave (300 units) drifts whole regions
///     shallower and deeper.
///   * **Tile to tile** — the mid and fine octaves (105 / 38 units) differ
///     across a 24-unit tile pitch, so neighbours in the same region still
///     differ.
///   * **Tile by itself** — a small per-tile hash jitter, so the honeycomb never
///     flattens out into an obvious gradient where the noise happens to be
///     level.
///
/// The band is deliberately clear of both ends: nothing is thin enough to leave
/// a prone pawn exposed, nothing deep enough to swallow a standing one, so the
/// tile-to-tile variety is about what a CROUCHING pawn can do — which is where
/// the interesting decisions are.
pub fn grass_height(x: i32, y: i32) -> i32 {
    let (col, row) = hex_cell(x, y);
    let (cx, cy) = hex_centre(col, row);
    let n = (value_noise(cx, cy, GRASS_CELL, GRASS_SEED) * 100
        + value_noise(cx, cy, GRASS_CELL_MID, GRASS_SEED ^ 0x9E37_79B9) * 45
        + value_noise(cx, cy, GRASS_CELL_FINE, GRASS_SEED ^ 0x85EB_CA6B) * 30)
        / 175;
    // Summed octaves pile up around their middle, so without a stretch the whole
    // map lands within a few units of the mean and the tiles stop differing.
    // This opens the middle half of the range out to the full band. It is the
    // same shape as the contrast curve the old field used, and it is fine HERE
    // because the band it stretches into is deep at both ends — what made that
    // one bad was the bias curve after it, which pushed the result toward bare
    // ground.
    let spread = ((n - FP / 4) * 2).clamp(0, FP);
    // Then bias the SHALLOW HALF down and leave the deep half alone, so short
    // ground is common without the deep end being squashed with it. Biasing the
    // whole range (averaging it with its own square, which is what the old field
    // did) costs the map nearly every tile that buries a crouching pawn — it
    // took the deep tiles from 8% to 1% — and those are the tiles worth crossing
    // the map for. The two halves meet at the midpoint, so there's no kink.
    let spread = if spread < FP / 2 { spread * spread * 2 / FP } else { spread };
    if spread < GRASS_BARE_SPREAD {
        return 0;
    }
    let depth = GRASS_BARE_BELOW + (GRASS_MAX_H - GRASS_BARE_BELOW) * spread / FP;
    let jitter = (grass_hash(col, row, GRASS_SEED ^ 0xC2B2_AE35) % (2 * GRASS_TILE_JITTER + 1))
        as i32
        - GRASS_TILE_JITTER as i32;
    (depth + jitter).clamp(GRASS_BARE_BELOW, GRASS_MAX_H)
}

/// What fraction of a pawn standing at `(x, y)` in this stance the grass
/// swallows, 0..=FP. Falls straight out of the two heights: the grass hides
/// everything below its own tips, so going flat is worth far more than any
/// stance bonus would be — 15 units of prone soldier disappears in grass that
/// barely reaches a standing one's knees.
pub fn grass_cover(x: i32, y: i32, stance: u8) -> i32 {
    Scenario::Arena.cover(x, y, stance)
}

/// Which world to build.
///
/// [`Scenario::Arena`] is the game. [`Scenario::GrassStrip`] is the concealment
/// measuring rig made playable: a wall of grass one fog hex wide with a pawn
/// either side of it and NOTHING else on the map — no boulders, no bushes, no
/// practice dummies, and no grass anywhere but the wall. It exists so that the
/// numbers in `client/src/vision/strip_table.rs` and the pictures in
/// `tools/grass-shots.sh` are the same scene rather than two descriptions of
/// one, and so the wall can be set to depths the procedural field never reaches.
///
/// It is a dev scenario and OFFLINE ONLY — `net.rs` ignores it whenever a room
/// is set, because two peers building different worlds is a desync by
/// construction. Nothing in a real match ever sees anything but `Arena`.
#[derive(Resource, Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum Scenario {
    #[default]
    Arena,
    /// A wall of grass `depth` units deep. The east pawn holds `east_stance`
    /// (it has no player: only the first local handle takes input, so its
    /// stance has to be told to it — see `client/src/input.rs`).
    GrassStrip { depth: i32, east_stance: u8 },
}

/// Half the width of the wall in [`Scenario::GrassStrip`], world units — one
/// fog hex across, since a flat-top hex is `2 * HEX_R` corner to corner and the
/// client's `HEX_R` is 16. `strip_table.rs` asserts the two still agree.
pub const STRIP_HALF_W: i32 = 16;
/// How far each pawn stands from the middle of the wall, world units: two hex
/// columns of `1.5 * HEX_R`, which leaves exactly one clear hex between each
/// pawn and the grass.
pub const STRIP_STANDOFF: i32 = 48;

impl Scenario {
    /// How deep the grass is at a point in this world, world units.
    pub fn depth(&self, x: i32, y: i32) -> i32 {
        match *self {
            Scenario::Arena => grass_height(x, y),
            Scenario::GrassStrip { depth, .. } => {
                if x.abs() <= STRIP_HALF_W {
                    depth
                } else {
                    0
                }
            }
        }
    }

    /// [`grass_cover`] in this world: the fraction of a pawn in `stance` that
    /// the grass here stands taller than, 0..=[`FP`].
    pub fn cover(&self, x: i32, y: i32, stance: u8) -> i32 {
        let body = STANCE_HEIGHT[(stance as usize).min(STANCE_COUNT - 1)];
        (self.depth(x, y) * FP / body).min(FP)
    }

    /// The boulders in this world. The rig has none, and the renderer needs to
    /// know that as much as `spawn_world` does — grass must not grow out of a
    /// rock that exists, nor be kept off one that doesn't.
    pub fn rocks(&self) -> Vec<(i32, i32, Rock)> {
        match *self {
            Scenario::Arena => rock_layout(),
            Scenario::GrassStrip { .. } => Vec::new(),
        }
    }

    /// The stance every handle *except* the first local one asks for each tick.
    /// In the game that's just "stand"; in the rig it's how the target pawn is
    /// posed, since nothing else can pose it.
    pub fn idle_stance(&self) -> u8 {
        match *self {
            Scenario::Arena => STANCE_STAND,
            Scenario::GrassStrip { east_stance, .. } => east_stance.min(STANCE_PRONE),
        }
    }
}

// ── Sight lines ─────────────────────────────────────────────────────────────
//
// How much of one pawn another can see. This lived in the client in f32 for as
// long as it was only ever a rendering question — the sim can't have a view,
// because every peer simulates every pawn. Bots changed that: a bot decides
// from what it can see, and every peer has to reach the same decision, so the
// answer has to be integer and it has to live here.
//
// What moved is the GRASS half, which is the model proper and is now shared:
// `client/src/vision.rs` calls this rather than keeping a second copy, so what
// hides a bot and what hides a player are the same number by construction.
// What did NOT move is the client's `Cast` machinery. That is a *camera* model
// — sight lines swept from two points behind either shoulder so a player can
// peek around cover they're hugging — and it answers a different question from
// the one a pawn asks about itself. The cover test below is pawn-centred.

/// How many steps a sight line is sampled at.
const GRASS_SAMPLES: i64 = 24;

/// Sight lines are sampled at `t = (T_BASE + T_RISE * (i + 1)) / T_DEN` for `i`
/// in `0..GRASS_SAMPLES` — the client's old `0.06 + 0.94 * (i + 1) / 24`
/// written as an exact rational, which is what lets every division here be
/// exact instead of nearly so. The last step lands on `t = 1` exactly.
///
/// Why it doesn't start at zero: `reaches` below divides by `t`, so a step at
/// the viewer's own feet would have every blade towering over the whole target.
/// Starting at 0.06 bounds that without pulling the near end so far in that it
/// misses the grass a pawn is actually lying in.
const T_DEN: i64 = 1200;
const T_BASE: i64 = 72;
const T_RISE: i64 = 47;

/// `e^(-GRASS_EXTINCTION * n) * FP` for a blocked length of `n` whole world
/// units, the Beer-Lambert term the f32 model called `.exp()`. A table because
/// this is the only transcendental in the model and the argument is bounded:
/// past ~52 units of blocked grass the answer is indistinguishable from opaque.
///
/// `GRASS_EXTINCTION` (0.12) does not appear anywhere else — the table *is* the
/// constant. It is anchored on the case the mechanic exists for: two pawns
/// lying either side of a body's width (~33 units) of shin-deep grass cannot
/// see each other at all (`EXP_NEG[33] = 5`, i.e. alpha 0.02).
const EXP_NEG: [i32; 65] = [
    256, 227, 201, 179, 158, 140, 125, 111, //
    98, 87, 77, 68, 61, 54, 48, 42, //
    38, 33, 30, 26, 23, 21, 18, 16, //
    14, 13, 11, 10, 9, 8, 7, 6, //
    6, 5, 4, 4, 3, 3, 3, 2, //
    2, 2, 2, 1, 1, 1, 1, 1, //
    1, 1, 1, 1, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, //
    0,
];

/// `1 - e^(-GRASS_EXTINCTION * length)` in `0..=FP`, for a blocked `length` in
/// subunits: how solid the grass on a line is. Linear between table entries;
/// the curve is smooth enough that the worst interpolation error is under a
/// fifth of a percent.
fn extinction(length_fp: i64) -> i32 {
    let fp = FP as i64;
    let n = length_fp.div_euclid(fp);
    if n >= EXP_NEG.len() as i64 - 1 {
        return FP;
    }
    let (a, b) = (EXP_NEG[n as usize] as i64, EXP_NEG[n as usize + 1] as i64);
    let decay = a + (b - a) * length_fp.rem_euclid(fp) / fp;
    (fp - decay) as i32
}

/// What the grass on one sight line does, split into the two questions that
/// make the model work. Kept as a pair rather than folded into one number
/// because they are the two things worth looking at when tuning, and the strip
/// table tabulates both.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Block {
    /// The largest share of the target that any single step's grass stands
    /// over, `0..=FP` — the geometric ceiling on how much can be hidden. If the
    /// tallest blade between you only reaches his knees, his head is in clear
    /// air and no amount of distance hides it.
    pub covered: i32,
    /// How much of the line is blocked, subunits, weighted by that share. How
    /// *solid* the cover is, and the only place distance enters.
    pub length: i64,
}

impl Block {
    /// The two terms multiplied: the ceiling, times how opaque the grass under
    /// it is.
    pub fn conceal(&self) -> i32 {
        (self.covered as i64 * extinction(self.length) as i64 / FP as i64) as i32
    }
}

/// Subunits to world units, rounded — which sample point a step lands on.
fn round_units(fp: i64) -> i32 {
    (fp + FP as i64 / 2).div_euclid(FP as i64) as i32
}

/// A pawn's height in this stance, world units: where it looks from, and
/// (near enough) how far up it can be seen.
pub fn stance_height(stance: u8) -> i32 {
    STANCE_HEIGHT[(stance as usize).min(STANCE_COUNT - 1)]
}

/// What the grass on one sight line does, over an arbitrary depth field rather
/// than a scenario's. The strip table needs the closure: it sweeps depths the
/// arena doesn't contain.
///
/// Grass isn't a set of casters, it's a depth field, so it gets an elevation
/// test rather than a shadow. Walk the line, and at each step ask how high up
/// the target a sight line grazing the blade tips there would land. A blade of
/// depth `g` at fraction `t`, seen from an eye at height `E`, hides everything
/// on the target below `E + (g - E) / t` (similar triangles: the line through
/// the tip carries on to the target plane). The share of the body under that
/// line is how much *this* step hides.
///
/// Those shares combine as TWO separate questions, and keeping them apart is
/// what makes the model behave — see [`Block`]. Multiplying them says: the part
/// of him below the grass line is hidden as completely as the depth of grass in
/// the way allows, and the part above it is always visible. Which is the answer
/// you'd give looking at the situation.
///
/// **Beer-Lambert over the blocked length ALONE was the previous model, and its
/// failure is worth remembering**: with no geometric ceiling, enough distance
/// hid anybody behind anything, so ankle-deep grass eventually erased a
/// standing man — while 30 units of shin-deep grass, which you genuinely cannot
/// see a prone man through, dimmed him by a quarter. Both directions were wrong
/// at once. The `covered` term is the fix.
///
/// Two things fall out of the geometry rather than being written into it:
///   * **Grass at the target's own feet (`t = 1`) hides it up to `g`** — which
///     is [`grass_cover`], the standing-in-it rule, as the limiting case of this
///     one. With `covered` taking the worst step, that rule is the model's
///     ceiling exactly, not merely its cousin.
///   * **Lying down costs you sight as well as buying it.** A prone eye is 15
///     units up, so grass deeper than that covers every target completely; what
///     is left is the length term, so a prone pawn sees a body's width into the
///     field and no further. Going flat is for breaking contact, not fighting.
///
/// Note what a TILED field does to this: a long line crosses many tiles and
/// `covered` takes the DEEPEST, so range costs visibility in steps as the line
/// picks up deeper tiles — which is why the arena numbers keep falling with
/// distance even though `length` saturates within ~50 units.
pub fn grass_block(
    eye: Pos,
    eye_h: i32,
    target: Pos,
    target_h: i32,
    depth_at: impl Fn(i32, i32) -> i32,
) -> Block {
    let (dx, dy) = ((target.x - eye.x) as i64, (target.y - eye.y) as i64);
    let dist = isqrt(dx * dx + dy * dy);
    if target_h <= 0 || dist < FP as i64 {
        return Block::default();
    }
    let fp = FP as i64;
    // One step's arc. Only `1 - t0` of the line is sampled.
    let step = dist * (T_DEN - T_BASE) / T_DEN / GRASS_SAMPLES;
    let mut block = Block::default();
    for i in 0..GRASS_SAMPLES {
        let t = T_BASE + T_RISE * (i + 1);
        let px = eye.x as i64 + dx * t / T_DEN;
        let py = eye.y as i64 + dy * t / T_DEN;
        let depth = depth_at(round_units(px), round_units(py)) as i64;
        // Similar triangles, written over a common denominator so it stays one
        // exact division.
        let reaches = eye_h as i64 * t + (depth - eye_h as i64) * T_DEN;
        let share = (reaches * fp / (t * target_h as i64)).clamp(0, fp);
        block.covered = block.covered.max(share as i32);
        block.length += share * step / fp;
    }
    block
}

/// How much of a pawn at `target` the grass hides from an eye at `eye`,
/// `0..=FP`. The eye is the pawn itself, not a camera behind it — peeking
/// *around* a field of grass isn't a thing.
pub fn grass_conceal(
    scenario: &Scenario,
    eye: Pos,
    eye_stance: u8,
    target: Pos,
    target_stance: u8,
) -> i32 {
    grass_block(
        eye,
        stance_height(eye_stance),
        target,
        stance_height(target_stance),
        |x, y| scenario.depth(x, y),
    )
    .conceal()
}

/// A circle that blocks sight: a boulder or a bush. Built once per tick and
/// shared across every query, because a pairwise sweep over 8 pawns asks 56
/// times and rebuilding the field each time would dominate.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Occluder {
    pub pos: Pos,
    pub r: i32,
}

/// Whether the segment `a`..`b` passes within `r` world units of `c`.
///
/// The clamped-projection form, deliberately: [`Sweep::entry`] solves the same
/// geometry but is sized for one tick of bullet travel, and its `half_b * half_b`
/// overflows `i64` on a segment stretched across the arena. This one never
/// squares anything larger than a coordinate.
fn segment_hits_circle(a: Pos, b: Pos, c: Pos, r: i32) -> bool {
    let (dx, dy) = ((b.x - a.x) as i64, (b.y - a.y) as i64);
    let (fx, fy) = ((c.x - a.x) as i64, (c.y - a.y) as i64);
    let rr = radius_fp(r);
    let dd = dx * dx + dy * dy;
    if dd == 0 {
        return fx * fx + fy * fy <= rr * rr;
    }
    let t = (fx * dx + fy * dy).clamp(0, dd);
    let (px, py) = (fx - dx * t / dd, fy - dy * t / dd);
    px * px + py * py <= rr * rr
}

/// Whether `p` is inside `o`.
fn inside(o: &Occluder, p: Pos) -> bool {
    let (dx, dy) = ((p.x - o.pos.x) as i64, (p.y - o.pos.y) as i64);
    let rr = radius_fp(o.r);
    dx * dx + dy * dy <= rr * rr
}

/// How far across the body the four outer sample points sit, subunits. The same
/// spread the renderer fades on, so someone edging out of cover comes into view
/// gradually instead of popping.
const BODY_REACH: i32 = PLAYER_R * 7 / 10 * FP;

/// How much of the pawn at `target` the pawn at `eye` can see, `0..=FP`.
///
/// Cover and grass answer separately and multiply, exactly as the renderer's
/// fade does. Cover is sampled at five points across the body — centre plus
/// four edges, which is Counter-Strike's gut/head/feet/left/right test in the
/// shape this game's geometry takes — so it degrades in fifths rather than
/// snapping between hidden and seen.
///
/// `occluders` is every boulder and bush; build it once a tick. Note what is
/// deliberately absent: cover the EYE is already inside doesn't occlude it.
/// Standing in a bush hides you without blinding you, and that has to be true
/// here for the same reason it's true on screen.
pub fn visible_fraction(
    scenario: &Scenario,
    eye: Pos,
    eye_stance: u8,
    target: Pos,
    target_stance: u8,
    occluders: &[Occluder],
) -> i32 {
    let offsets = [
        (0, 0),
        (BODY_REACH, 0),
        (-BODY_REACH, 0),
        (0, BODY_REACH),
        (0, -BODY_REACH),
    ];
    let mut clear = 0;
    for (ox, oy) in offsets {
        let p = Pos { x: target.x + ox, y: target.y + oy };
        let blocked = occluders
            .iter()
            .any(|o| !inside(o, eye) && segment_hits_circle(eye, p, o.pos, o.r));
        if !blocked {
            clear += 1;
        }
    }
    if clear == 0 {
        return 0;
    }
    let seen = clear * FP / offsets.len() as i32;
    let grass = grass_conceal(scenario, eye, eye_stance, target, target_stance);
    (seen as i64 * (FP - grass) as i64 / FP as i64) as i32
}

/// Everything every pawn has, human or bot. Kept in one place so the two kinds
/// can't drift apart — a bot that was missing a component the sim's systems
/// filter on would simply stop being simulated, silently.
fn spawn_pawn(commands: &mut Commands, handle: usize, x: i32, y: i32) -> Entity {
    commands
        .spawn((
            Player { handle },
            Intent::default(),
            Pos::from_units(x, y),
            Facing::default(),
            Cooldown::default(),
            Stance::default(),
            Health::default(),
            Deaths::default(),
            Kills::default(),
        ))
        .add_rollback()
        .id()
}

/// Spawn the initial world: one pawn per player, the practice targets, and the
/// procedural rock and bush fields. Both clients run this identically before
/// the first tick.
///
/// **No bots.** They are not spawned here on purpose: the number of them is
/// carried in the input stream and applied by [`reconcile_bots`], which is the
/// only way it can be rollback-safe and the only way two peers can be sure to
/// agree. Spawning some here as well would give the count two sources of truth,
/// and the reconciler — correctly — would immediately undo whichever one it
/// disagreed with. They appear over the first few ticks instead, one per tick.
pub fn spawn_world(commands: &mut Commands, num_players: usize, scenario: Scenario) {
    if let Scenario::GrassStrip { east_stance, .. } = scenario {
        spawn_grass_strip(commands, east_stance);
        return;
    }
    for handle in 0..num_players.min(MAX_PLAYERS) {
        let (x, y) = SPAWN_POINTS[handle];
        spawn_pawn(commands, handle, x, y);
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

/// The measuring scene ([`Scenario::GrassStrip`]): two pawns facing each other
/// across the wall, and nothing else at all.
///
/// West is handle 0 — the pawn the camera follows and the keyboard drives. East
/// is the target: it spawns already in `east_stance` so there's no getting-down
/// animation to wait out, and `idle_stance` keeps it there.
fn spawn_grass_strip(commands: &mut Commands, east_stance: u8) {
    for (handle, x, toward, level) in [
        (0, -STRIP_STANDOFF, 127, STANCE_STAND),
        (1, STRIP_STANDOFF, -127, east_stance.min(STANCE_PRONE)),
    ] {
        let pawn = spawn_pawn(commands, handle, x, 0);
        commands
            .entity(pawn)
            .insert((Facing { x: toward, y: 0 }, Stance { level, change: 0 }));
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

fn radius_fp(r_units: i32) -> i64 {
    (r_units * FP) as i64
}

// ── Ballistics ──────────────────────────────────────────────────────────────
//
// A bullet covers 16 world units per tick and a pawn is 24 across, so where the
// round *is* at the end of a tick says very little about what it went through
// during one: sampling the endpoint clips the near edge of anyone it passes
// diagonally, and would let a round skip a target that was never more than a
// few units off its line. Both matter more here than they did against the
// practice dummies, because the damage now depends on *how* centered the hit
// was — so the whole tick's travel is treated as a segment and swept.

/// The segment a bullet swept this tick, plus the constants every test against
/// it needs. `from` is the start (this tick's `Pos` minus one tick of
/// velocity); `d` is the step; `dd` its squared length; `len` its length.
#[derive(Copy, Clone)]
struct Sweep {
    from: Pos,
    dx: i64,
    dy: i64,
    dd: i64,
    len: i64,
}

impl Sweep {
    /// The step a bullet at `pos` just took.
    fn of(bullet: &Bullet, pos: Pos) -> Option<Self> {
        let (dx, dy) = (bullet.vx as i64, bullet.vy as i64);
        let dd = dx * dx + dy * dy;
        if dd == 0 {
            return None;
        }
        let from = Pos { x: pos.x - bullet.vx, y: pos.y - bullet.vy };
        Some(Self { from, dx, dy, dd, len: isqrt(dd) })
    }

    /// Where the sweep first touches the circle at `center` of `radius` world
    /// units, as `t * dd` — an integer in `0..=dd`, comparable between
    /// candidates because every candidate shares this sweep. `None` if it
    /// misses, or the circle is behind the round.
    ///
    /// Standard ray/circle solve, halved (`b/2`) so nothing overflows: the
    /// early bounding reject also keeps `f` inside a step plus a radius, which
    /// is what makes the squares here small.
    fn entry(&self, center: Pos, radius: i32) -> Option<i64> {
        let r = radius_fp(radius);
        let fx = (self.from.x - center.x) as i64;
        let fy = (self.from.y - center.y) as i64;
        let ff = fx * fx + fy * fy;
        // Nothing further than a step plus a radius away can be swept.
        let reach = self.len + r;
        if ff > reach * reach {
            return None;
        }
        let c = ff - r * r;
        if c <= 0 {
            return Some(0); // started inside it
        }
        let half_b = fx * self.dx + fy * self.dy;
        if half_b >= 0 {
            return None; // moving away from it
        }
        let disc = half_b * half_b - self.dd * c;
        if disc < 0 {
            return None;
        }
        let entry = -half_b - isqrt(disc);
        (0..=self.dd).contains(&entry).then_some(entry)
    }

    /// How far the sweep's *line* passes from a point, in subunits. This is the
    /// "how centered was it" measure, and it deliberately isn't the distance at
    /// the moment of contact: a round that enters a pawn's edge at the very end
    /// of a tick is still a dead-center shot, it just hasn't got there yet.
    fn miss(&self, center: Pos) -> i64 {
        let fx = (self.from.x - center.x) as i64;
        let fy = (self.from.y - center.y) as i64;
        let cross = fx * self.dy - fy * self.dx;
        cross.abs() / self.len
    }
}

/// What a round takes off, from how centered it was and how far it flew.
///
/// `miss` is the perpendicular distance from the pawn's center to the shot line
/// and `reach` the radius at which that stops being a hit (both subunits);
/// `travelled` is the distance from the muzzle, also subunits. Two independent
/// scalings of [`HIT_DAMAGE_MAX`], each with a floor, so the worst possible
/// hit still does about a tenth of the best one and a hit is never worth
/// nothing.
pub fn bullet_damage(miss: i64, reach: i64, travelled: i64) -> i32 {
    let fp = FP as i64;
    // Centered: FP dead center, 0 at the rim.
    let centered = if reach <= 0 { fp } else { (reach - miss.clamp(0, reach)) * fp / reach };
    let center_scale = DAMAGE_EDGE_FRAC as i64 + (fp - DAMAGE_EDGE_FRAC as i64) * centered / fp;

    let units = travelled / fp;
    let (near, far, floor) = (DAMAGE_NEAR as i64, DAMAGE_FAR as i64, DAMAGE_FAR_FRAC as i64);
    let range_scale = if units <= near {
        fp
    } else if units >= far {
        floor
    } else {
        fp - (fp - floor) * (units - near) / (far - near)
    };

    (HIT_DAMAGE_MAX as i64 * center_scale / fp * range_scale / fp).max(1) as i32
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
            // The sim owns `Scenario`, so it guarantees one exists rather than
            // leaving every caller to remember — `init_resource` defers to a
            // scenario the client has already chosen and only fills in `Arena`
            // when nobody has. Without this, anything that builds a bare app
            // (the combat tests, the harness) panics the moment a bot asks what
            // world it is in.
            .init_resource::<Scenario>()
            // Same deal: the roster is config the game never varies, so it is
            // filled in rather than demanded, and only the harness overrides it.
            .init_resource::<BotRoster>()
            .rollback_component_with_copy::<Pos>()
            .rollback_component_with_copy::<Player>()
            .rollback_component_with_copy::<Intent>()
            .rollback_component_with_copy::<Bot>()
            .rollback_component_with_copy::<Facing>()
            .rollback_component_with_copy::<Cooldown>()
            .rollback_component_with_copy::<Stance>()
            .rollback_component_with_copy::<Health>()
            .rollback_component_with_copy::<Deaths>()
            .rollback_component_with_copy::<Kills>()
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
            // Health too: a disagreement about who is alive diverges the whole
            // match, and it can happen a long way from anyone's position (two
            // peers resolving the same round against different pawns), so
            // waiting for it to show up in `Pos` would be waiting for a
            // respawn.
            .checksum_component_with_hash::<Health>()
            .add_systems(
                GgrsSchedule,
                (
                    // Intent first, and nothing before it: both intent systems
                    // want the world as the previous tick left it — fully
                    // settled, including respawns — and everything after them
                    // wants this tick's decisions.
                    // First: it changes the pawn set, and everything after
                    // wants a settled one.
                    reconcile_bots::<C>,
                    read_human_intent::<C>,
                    bot_think,
                    move_players,
                    // After movement and before anything reads a position:
                    // rounds leave the barrel next, and a shot fired from
                    // inside someone else can't hit them.
                    separate_players,
                    fire_bullets,
                    move_bullets,
                    resolve_hits,
                    tick_targets,
                    respawn_players,
                )
                    .chain(),
            );
    }
}

// ── Fixed-tick systems (run inside the GGRS rollback schedule) ──────────────

/// Add or remove bot pawns until the match has as many as the first player is
/// asking for.
///
/// **Reconciliation, not a command.** The input carries an absolute count, and
/// this closes the gap by one pawn per tick; replaying a tick therefore reaches
/// the same world however many times GGRS runs it. A "+1 bot" edge would apply
/// once per re-simulation instead, which is the same trap stance avoided by
/// sending a level rather than a "go down".
///
/// Every choice here is made on a sorted key rather than query order: the new
/// bot takes the lowest free handle, and the one removed is the highest. Two
/// peers must pick the same pawn or they diverge immediately.
///
/// One pawn per tick is deliberate — it keeps the work bounded, and at 60 Hz
/// filling an arena still looks instant.
fn reconcile_bots<C: Config<Input = PlayerInput>>(
    mut commands: Commands,
    inputs: Res<PlayerInputs<C>>,
    roster: Res<BotRoster>,
    pawns: Query<(Entity, &Player, Option<&Bot>)>,
) {
    // The first player's copy, and only theirs.
    let Some(&(input, _status)) = inputs.get(0) else { return };

    let mut taken = [false; MAX_PLAYERS];
    let (mut humans, mut bots) = (0usize, Vec::new());
    for (entity, player, is_bot) in &pawns {
        if player.handle < MAX_PLAYERS {
            taken[player.handle] = true;
        }
        match is_bot {
            Some(_) => bots.push((player.handle, entity)),
            None => humans += 1,
        }
    }

    // Bots fill the seats the humans aren't using; asking for more than that is
    // asking for a spawn point that doesn't exist.
    let wanted = (input.bots() as usize).min(MAX_PLAYERS.saturating_sub(humans));
    if bots.len() == wanted {
        return;
    }

    if bots.len() < wanted {
        let Some(handle) = (0..MAX_PLAYERS).find(|&h| !taken[h]) else { return };
        let (x, y) = SPAWN_POINTS[handle];
        let pawn = spawn_pawn(&mut commands, handle, x, y);
        commands
            .entity(pawn)
            .insert(Bot::seeded(handle, roster.profile(handle), roster.salt));
    } else {
        bots.sort_unstable();
        if let Some(&(_, entity)) = bots.last() {
            commands.entity(entity).despawn();
        }
    }
}

/// Copy this tick's inputs off the wire onto the human pawns.
///
/// Guarded with `get` rather than indexed: a pawn whose handle has no seat in
/// the session is a bug, but it should not be a panic in the middle of a
/// rollback. Bot pawns are excluded — their handles are deliberately outside
/// the session's range.
fn read_human_intent<C: Config<Input = PlayerInput>>(
    inputs: Res<PlayerInputs<C>>,
    mut pawns: Query<(&Player, &mut Intent), Without<Bot>>,
) {
    for (player, mut intent) in &mut pawns {
        if let Some(&(input, _status)) = inputs.get(player.handle) {
            intent.0 = input;
        }
    }
}

/// `With<Player>` is load-bearing, not decoration: it is what makes this query
/// provably disjoint from the `Without<Player>` one below, and both touch `Pos`.
fn move_players(
    mut players: Query<(&Intent, &mut Pos, &mut Facing, &mut Stance, &Health), With<Player>>,
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

    for (intent, mut pos, mut facing, mut stance, health) in &mut players {
        // The dead hold still: no drifting, no turning, no getting up out of
        // prone while you wait. `respawn_players` puts them back.
        if !health.alive() {
            continue;
        }
        let input = intent.0;
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

/// Pawns are solid: shove any two that are inside each other apart until they
/// aren't.
///
/// **This exists because two bots could stand on the same subunit forever.**
/// Nothing stopped pawns interpenetrating, and `Act::Fight` roots a bot in
/// place, so two of them that closed to nothing were pinned there permanently —
/// facing each other, firing, and unable to connect, because a round is born
/// `PLAYER_R + BULLET_R + 2` units down the barrel and that is *past* a target
/// standing 1.6 units away. Measured before the fix: one pair spent 3100
/// consecutive ticks like that and neither lost a single point of health. Two
/// soldiers occupying the same square metre was always wrong; that it was also
/// a stable trap is what made it urgent.
///
/// Runs after [`move_players`] and over EVERY living pawn, not just the ones
/// that moved. A rooted pawn has to be separable or the trap above survives the
/// fix — both of those bots were standing perfectly still.
///
/// Determinism: every push is computed against a SNAPSHOT taken before any of
/// them are applied, so the result doesn't depend on which pawn the query
/// happened to visit first, and two pawns push off each other symmetrically.
/// One pass doesn't fully untangle a three-way pile, which is fine — it runs
/// again next tick, and a bounded amount of work per tick is worth more than a
/// settled answer this one.
fn separate_players(
    mut players: Query<(&Player, &mut Pos, &Health), With<Player>>,
    rocks: Query<(&Rock, &Pos), Without<Player>>,
) {
    let reach = (2 * PLAYER_R * FP) as i64;

    // Sorted by handle: this is the snapshot every push is measured against.
    let mut standing: Vec<(usize, Pos)> = players
        .iter()
        .filter(|(_, _, health)| health.alive())
        .map(|(player, pos, _)| (player.handle, *pos))
        .collect();
    standing.sort_unstable_by_key(|&(handle, _)| handle);
    if standing.len() < 2 {
        return;
    }

    // Nothing is touching almost every tick — bots hold a standoff and players
    // spread out — so bail before the expensive part rather than rebuilding and
    // re-sorting the rock field for nobody.
    let touching = standing.iter().enumerate().any(|(i, &(_, a))| {
        standing[i + 1..].iter().any(|&(_, b)| {
            let (dx, dy) = ((a.x - b.x) as i64, (a.y - b.y) as i64);
            dx * dx + dy * dy < reach * reach
        })
    });
    if !touching {
        return;
    }

    let mut cover: Vec<(i32, i32, i32)> = rocks
        .iter()
        .map(|(rock, pos)| (pos.x, pos.y, rock.r))
        .collect();
    cover.sort_unstable();

    for (player, mut pos, health) in &mut players {
        if !health.alive() {
            continue;
        }
        let (mut shift_x, mut shift_y) = (0i64, 0i64);
        for &(other, at) in &standing {
            if other == player.handle {
                continue;
            }
            let (dx, dy) = ((pos.x - at.x) as i64, (pos.y - at.y) as i64);
            let d2 = dx * dx + dy * dy;
            if d2 >= reach * reach {
                continue;
            }
            let d = isqrt(d2);
            if d == 0 {
                // Exactly co-located, which a respawn onto an occupied spawn
                // point can still manage. Any direction will do as long as the
                // two pick opposite ones and every peer agrees — so it comes
                // off the handles, which every peer sorts identically.
                let away = if player.handle < other { -1 } else { 1 };
                shift_x += away * reach / 2;
                continue;
            }
            // Half the overlap each, so a pair separates without either being
            // the one that gives way.
            let overlap = (reach - d) / 2;
            shift_x += dx * overlap / d;
            shift_y += dy * overlap / d;
        }
        if shift_x == 0 && shift_y == 0 {
            continue;
        }
        pos.x = pos.x.saturating_add(shift_x as i32);
        pos.y = pos.y.saturating_add(shift_y as i32);
        // Being shoved off someone must not shove you into a boulder, or out of
        // the arena.
        push_out_of_cover(&mut pos, &cover);
        pos.x = pos.x.clamp(-(ARENA_HALF_W - PLAYER_R) * FP, (ARENA_HALF_W - PLAYER_R) * FP);
        pos.y = pos.y.clamp(-(ARENA_HALF_H - PLAYER_R) * FP, (ARENA_HALF_H - PLAYER_R) * FP);
    }
}

fn fire_bullets(
    mut commands: Commands,
    mut players: Query<(&Player, &Intent, &Pos, &Facing, &mut Cooldown, &Health)>,
) {
    for (player, intent, pos, facing, mut cooldown, health) in &mut players {
        if cooldown.0 > 0 {
            cooldown.0 -= 1;
        }
        let input = intent.0;
        if !input.fire() || cooldown.0 > 0 || !health.alive() {
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
) {
    for (entity, mut bullet, mut pos) in &mut bullets {
        pos.x += bullet.vx;
        pos.y += bullet.vy;
        bullet.ttl = bullet.ttl.saturating_sub(1);
        let out = pos.x.abs() > ARENA_HALF_W * FP || pos.y.abs() > ARENA_HALF_H * FP;
        if bullet.ttl == 0 || out {
            commands.entity(entity).despawn();
        }
    }
}

/// What a round ran into first.
#[derive(Copy, Clone)]
enum Impact {
    /// Handle hit, and how far off center the shot line passed (subunits).
    Player(usize, i64),
    /// The dummy standing here (matched back by position — they never move).
    Target(Pos),
    /// Stopped in cover. Nothing to apply; the round just dies.
    Rock,
}

/// Resolve every round against everything it could have swept through this
/// tick, and apply only the *first* thing along its path — so cover really
/// does stop a bullet that would otherwise have carried on into someone behind
/// it, rather than both happening because they were checked in different
/// systems.
fn resolve_hits(
    mut commands: Commands,
    bullets: Query<(Entity, &Bullet, &Pos)>,
    mut players: Query<(&Player, &Pos, &mut Health, &mut Deaths, &mut Kills)>,
    mut targets: Query<(&mut Target, &Pos)>,
    rocks: Query<(&Rock, &Pos)>,
) {
    for (bullet_entity, bullet, bullet_pos) in &bullets {
        let Some(sweep) = Sweep::of(bullet, *bullet_pos) else { continue };

        // Sorted, not first-found: query iteration order is not a determinism
        // guarantee, and "which of these did the round reach first" has to come
        // out the same on every peer. The key is (distance along the sweep,
        // position, handle) — position alone would tie between two pawns
        // standing on the same subunit, which nothing prevents.
        let mut hits: Vec<(i64, i32, i32, usize, Impact)> = Vec::new();
        for (player, pos, health, ..) in &players {
            if player.handle == bullet.owner || !health.alive() {
                continue;
            }
            if let Some(entry) = sweep.entry(*pos, PLAYER_R + BULLET_R) {
                hits.push((entry, pos.x, pos.y, player.handle + 1, Impact::Player(player.handle, sweep.miss(*pos))));
            }
        }
        for (_, pos) in &targets {
            if let Some(entry) = sweep.entry(*pos, TARGET_R + BULLET_R) {
                hits.push((entry, pos.x, pos.y, 0, Impact::Target(*pos)));
            }
        }
        for (rock, pos) in &rocks {
            if let Some(entry) = sweep.entry(*pos, rock.r + BULLET_R) {
                hits.push((entry, pos.x, pos.y, 0, Impact::Rock));
            }
        }
        hits.sort_unstable_by_key(|&(entry, x, y, tie, _)| (entry, x, y, tie));
        let Some(&(.., impact)) = hits.first() else { continue };

        match impact {
            Impact::Rock => {}
            Impact::Target(at) => {
                for (mut target, pos) in &mut targets {
                    if *pos == at {
                        target.hits += 1;
                        target.flash = HIT_FLASH_TICKS;
                        break;
                    }
                }
            }
            Impact::Player(handle, miss) => {
                // Distance flown: every round travels at the same speed, so the
                // ticks it has burned are its range — no extra state to roll
                // back. Plus the muzzle offset it was born at.
                let flown = (PLAYER_R + BULLET_R + 2) as i64 * FP as i64
                    + (BULLET_TTL - bullet.ttl) as i64 * BULLET_SPEED as i64;
                let damage = bullet_damage(miss, radius_fp(PLAYER_R + BULLET_R), flown);
                let mut killed = false;
                for (player, _, mut health, mut deaths, _) in &mut players {
                    if player.handle != handle {
                        continue;
                    }
                    health.hp -= damage;
                    health.hurt = HURT_FLASH_TICKS;
                    if health.hp <= 0 {
                        health.hp = 0;
                        health.down = RESPAWN_TICKS;
                        deaths.0 += 1;
                        killed = true;
                    }
                    break;
                }
                // A second pass rather than one: the victim's borrow above is
                // exclusive, and the shooter is another row of the same query.
                // Only walked when someone actually went down.
                if killed {
                    for (player, _, _, _, mut kills) in &mut players {
                        if player.handle == bullet.owner {
                            kills.0 += 1;
                            break;
                        }
                    }
                }
            }
        }
        commands.entity(bullet_entity).despawn();
    }
}

/// Count the dead back in. Everything about the pawn resets — position, facing,
/// stance, trigger — so a respawn is a clean start and not a corpse teleporting
/// home still lying down.
fn respawn_players(
    mut players: Query<(&Player, &mut Pos, &mut Facing, &mut Stance, &mut Cooldown, &mut Health)>,
) {
    for (player, mut pos, mut facing, mut stance, mut cooldown, mut health) in &mut players {
        if health.hurt > 0 {
            health.hurt -= 1;
        }
        if health.down == 0 {
            continue;
        }
        health.down -= 1;
        if health.down > 0 {
            continue;
        }
        let (x, y) = SPAWN_POINTS[player.handle % MAX_PLAYERS];
        *pos = Pos::from_units(x, y);
        *facing = Facing::default();
        *stance = Stance::default();
        *cooldown = Cooldown::default();
        health.hp = MAX_HEALTH;
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

    /// The MIX. Every stance has to have ground that suits it and ground that
    /// doesn't, or the terrain isn't saying anything: short tiles where lying
    /// down still leaves you showing, deep ones that bury a crouching pawn, and
    /// the bulk in between.
    ///
    /// The percentages are the design, so they're printed as a histogram — if a
    /// curve tweak moves them, the test says which way rather than just failing.
    #[test]
    fn grass_field_has_a_mix_of_depths() {
        let prone_h = STANCE_HEIGHT[STANCE_PRONE as usize];
        let crouch_h = STANCE_HEIGHT[STANCE_CROUCH as usize];
        let (mut min, mut max, mut sum, mut total) = (GRASS_MAX_H, 0, 0i64, 0i64);
        let (mut bare, mut short, mut deep) = (0i64, 0i64, 0i64);
        let mut bins = [0i64; 6];
        let mut y = -ARENA_HALF_H;
        while y <= ARENA_HALF_H {
            let mut x = -ARENA_HALF_W;
            while x <= ARENA_HALF_W {
                let h = grass_height(x, y);
                assert!(
                    (GRASS_MIN_H..=GRASS_MAX_H).contains(&h),
                    "grass out of band at {x},{y}: {h}"
                );
                min = min.min(h);
                max = max.max(h);
                sum += h as i64;
                // Bare = ground texture, nothing growing; short = a prone pawn
                // is not fully buried; deep = a crouching one is.
                bare += (h == 0) as i64;
                short += (h > 0 && h <= prone_h) as i64;
                deep += (h > crouch_h) as i64;
                bins[((h - GRASS_MIN_H) * 6 / (GRASS_MAX_H - GRASS_MIN_H + 1)) as usize] += 1;
                total += 1;
                x += 4;
            }
            y += 4;
        }
        let pct = |n: i64| n * 100 / total;
        let step = (GRASS_MAX_H - GRASS_MIN_H + 1) / 6;
        println!("grass {min}..{max}, mean {}:", sum / total);
        for (i, n) in bins.iter().enumerate() {
            println!(
                "  {:>2}..{:<2} {:>3}% {}",
                GRASS_MIN_H + i as i32 * step,
                GRASS_MIN_H + (i as i32 + 1) * step - 1,
                pct(*n),
                "#".repeat((pct(*n) * 2) as usize)
            );
        }
        println!(
            "  {}% bare + {}% short = {}% where a prone pawn shows; \
             {}% deep enough to bury a crouching one",
            pct(bare),
            pct(short),
            pct(bare + short),
            pct(deep)
        );

        assert!(
            max < STANCE_HEIGHT[STANCE_STAND as usize],
            "nothing should be over a standing soldier's head: max {max}"
        );
        assert!(
            max - min >= 24,
            "the field is too flat to be worth quantizing: {min}..{max}"
        );
        // Open ground is terrain you read and cross carefully: real, but never
        // the norm. Nowhere to hide at all and the map is a shooting gallery;
        // nothing but cover and there is no reason to move. Bare tiles are
        // asserted separately from short ones because they are what the player
        // actually SEES as open — the ground texture with nothing on it.
        assert!(
            (4..=20).contains(&pct(bare)),
            "bare ground should be patches, not the map: {}%",
            pct(bare)
        );
        // Bare and short are counted together because they answer one question —
        // where does lying down stop working — and `GRASS_BARE_BELOW` decides
        // how the total splits between them. With the floor at 14 (just under a
        // prone pawn's 15), grass that leaves one showing is a sliver and bare
        // ground does nearly all of this job; drop the floor and it shifts the
        // other way. The sum is the design; the split is a look.
        assert!(
            (5..=30).contains(&pct(bare + short)),
            "ground a prone pawn shows on should be a feature, not the map: {}%",
            pct(bare + short)
        );
        assert!(
            (5..=40).contains(&pct(deep)),
            "crouching should be a real choice, not always or never: {}%",
            pct(deep)
        );
    }

    /// The spec the field is quantized for: **one depth per hex tile.** Every
    /// point in a tile has to answer the same as the tile's centre, or the
    /// honeycomb the fog draws and the grass the player reads are different
    /// shapes.
    #[test]
    fn grass_is_uniform_within_a_hex_tile() {
        let mut tiles = std::collections::BTreeMap::new();
        let mut y = -ARENA_HALF_H;
        while y <= ARENA_HALF_H {
            let mut x = -ARENA_HALF_W;
            while x <= ARENA_HALF_W {
                let cell = hex_cell(x, y);
                let h = grass_height(x, y);
                match tiles.get(&cell) {
                    None => {
                        tiles.insert(cell, h);
                    }
                    Some(&first) => assert_eq!(
                        h, first,
                        "grass changes inside tile {cell:?} at {x},{y}: {h} vs {first}"
                    ),
                }
                x += 2;
            }
            y += 2;
        }
        // ...and the tiles differ from each other. Depths are integers in a
        // 37-wide band, so a good field uses most of the values available.
        let depths: std::collections::BTreeSet<_> = tiles.values().collect();
        println!("{} tiles, {} distinct depths", tiles.len(), depths.len());
        assert!(tiles.len() > 600, "the arena should be ~750 tiles: {}", tiles.len());
        assert!(
            depths.len() >= 20,
            "tiles barely differ: only {} distinct depths",
            depths.len()
        );
    }

    /// The depth only ever changes at a tile boundary, and never by a wall's
    /// worth when it does.
    ///
    /// This used to assert the field was CONTINUOUS (no step over 4 units
    /// anywhere). Quantizing to tiles makes steps the intended behaviour, so
    /// what's left to check is that they land on tile edges — a step in the
    /// middle of a tile would mean the quantization is broken — and that
    /// neighbours stay in the same conversation: walking from one tile to the
    /// next is a change of stance's worth of grass at most, never ankle-deep to
    /// over your head.
    #[test]
    fn grass_steps_only_at_tile_edges() {
        let mut worst = 0;
        let mut edges = 0;
        let mut y = -ARENA_HALF_H;
        while y <= ARENA_HALF_H {
            let mut x = -ARENA_HALF_W;
            while x < ARENA_HALF_W {
                for (nx, ny) in [(x + 2, y), (x, y + 2)] {
                    let (here, there) = (grass_height(x, y), grass_height(nx, ny));
                    let step = (there - here).abs();
                    if step > 0 {
                        assert_ne!(
                            hex_cell(x, y),
                            hex_cell(nx, ny),
                            "grass changed by {step} inside one tile at {x},{y}"
                        );
                    }
                    // The edge of a bare patch is a real edge — grass simply
                    // stops — so it isn't held to the bound below. Everything
                    // else is grass meeting grass.
                    if here == 0 || there == 0 {
                        edges += (step > 0) as i32;
                    } else {
                        worst = worst.max(step);
                    }
                }
                x += 2;
            }
            y += 2;
        }
        assert!(edges > 0, "no bare ground meets grass anywhere");
        println!("worst step between neighbouring tiles: {worst}");
        // Around two thirds of the band. Quantizing plus `GRASS_TILE_JITTER`
        // makes some step inevitable and a visible one is the point — the edge
        // of a patch of long grass is a real thing — but a tile may not go from
        // ankle-deep to over a crouching pawn's head in one stride, which reads
        // as a hedge rather than as terrain.
        assert!(
            worst <= 34,
            "neighbouring tiles differ by {worst} units — that reads as a wall, not terrain"
        );
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

    /// A round travelling right along y=0, one tick's worth of step, starting
    /// `units` to the left of the origin and offset `off` units on y.
    fn shot(units: i32, off: i32) -> (Bullet, Pos) {
        let bullet = Bullet { owner: 0, ttl: BULLET_TTL - 1, vx: BULLET_SPEED, vy: 0 };
        (bullet, Pos::from_units(-units + BULLET_SPEED / FP, off))
    }

    /// The sweep is what makes "how centered" mean anything: a round is tested
    /// against the whole tick of travel, not against wherever it happened to
    /// stop, and the miss distance is the shot *line's*, not the contact
    /// point's.
    #[test]
    fn bullets_sweep_their_whole_step() {
        let reach = PLAYER_R + BULLET_R;

        // A round that ends the tick short of the pawn but crosses it during
        // one still hits. (16 units per tick against a 24-unit pawn: sampling
        // the endpoint would let this through.)
        let (bullet, pos) = shot(reach + 4, 0);
        let sweep = Sweep::of(&bullet, pos).unwrap();
        assert!(sweep.entry(Pos::from_units(0, 0), reach).is_some(), "swept round must hit");

        // Dead center is dead center even when contact is at the very end of
        // the step — the miss is measured off the line.
        assert_eq!(sweep.miss(Pos::from_units(0, 0)), 0);

        // Passing the pawn's shoulder: hit, and the miss is the real offset.
        let (bullet, pos) = shot(reach + 4, 10);
        let sweep = Sweep::of(&bullet, pos).unwrap();
        assert!(sweep.entry(Pos::from_units(0, 0), reach).is_some());
        let miss = sweep.miss(Pos::from_units(0, 0));
        assert!((miss - 10 * FP as i64).abs() <= 2, "miss should read ~10 units, got {miss}");

        // Wide, and behind: neither is a hit.
        let (bullet, pos) = shot(reach + 4, reach + 6);
        assert!(Sweep::of(&bullet, pos).unwrap().entry(Pos::from_units(0, 0), reach).is_none());
        let (bullet, pos) = shot(-200, 0); // already 200 units past, moving away
        assert!(Sweep::of(&bullet, pos).unwrap().entry(Pos::from_units(0, 0), reach).is_none());
    }

    /// The damage curve has to stay monotone in both arguments and keep its
    /// floors — an edge hit at range must still be worth firing, and a perfect
    /// round must not one-shot.
    #[test]
    fn damage_falls_off_with_center_and_range() {
        let reach = radius_fp(PLAYER_R + BULLET_R);
        let at = |off_units: i32, range_units: i32| {
            bullet_damage(off_units as i64 * FP as i64, reach, range_units as i64 * FP as i64)
        };

        let best = at(0, 0);
        assert_eq!(best, HIT_DAMAGE_MAX, "point blank dead center is the full figure");
        assert!(best * 2 < MAX_HEALTH, "one round must never be half a life");
        assert!(best * 3 >= MAX_HEALTH, "three perfect rounds should kill");

        // Monotone in centeredness, at a fixed range...
        let mut previous = i32::MAX;
        for off in 0..=(PLAYER_R + BULLET_R) {
            let damage = at(off, 0);
            assert!(damage <= previous, "damage rose as the shot went wider at {off}");
            previous = damage;
        }
        // ...and in range, at a fixed centeredness.
        let mut previous = i32::MAX;
        for range in (0..=800).step_by(20) {
            let damage = at(0, range);
            assert!(damage <= previous, "damage rose with range at {range}");
            previous = damage;
        }

        // Both floors hold, and the worst hit still registers.
        assert_eq!(at(0, 2000), at(0, DAMAGE_FAR), "range falloff must flatten out");
        let worst = at(PLAYER_R + BULLET_R, 2000);
        assert!(worst >= 1, "a hit is never worth nothing");
        assert!(worst * 6 < best, "a long graze should be a fraction of a good hit");
        // Beyond the hitbox the caller never asks, but the clamp must hold.
        assert_eq!(at(500, 0), at(PLAYER_R + BULLET_R, 0));
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

    // ── Sight lines ─────────────────────────────────────────────────────────

    /// The f32 model this was ported from, verbatim, so the port can be checked
    /// against the thing it replaced instead of against its own assertions.
    /// If this ever has to change to keep the test passing, the port drifted.
    pub(super) fn f32_conceal(
        eye: (f32, f32),
        eye_h: f32,
        target: (f32, f32),
        target_h: f32,
        depth: &dyn Fn(i32, i32) -> i32,
    ) -> f32 {
        const SAMPLES: i32 = 24;
        const NEAR_T: f32 = 0.06;
        const EXTINCTION: f32 = 0.12;
        let dist = ((target.0 - eye.0).powi(2) + (target.1 - eye.1).powi(2)).sqrt();
        if target_h <= 0.0 || dist < 1.0 {
            return 0.0;
        }
        let (mut covered, mut length) = (0.0f32, 0.0f32);
        let step = dist * (1.0 - NEAR_T) / SAMPLES as f32;
        for i in 0..SAMPLES {
            let t = NEAR_T + (1.0 - NEAR_T) * (i + 1) as f32 / SAMPLES as f32;
            let px = eye.0 + (target.0 - eye.0) * t;
            let py = eye.1 + (target.1 - eye.1) * t;
            let d = depth(px.round() as i32, py.round() as i32) as f32;
            let reaches = eye_h + (d - eye_h) / t;
            let share = (reaches / target_h).clamp(0.0, 1.0);
            covered = covered.max(share);
            length += share * step;
        }
        covered * (1.0 - (-EXTINCTION * length).exp())
    }

    /// The port is faithful: over every stance pairing, every grass depth and a
    /// spread of ranges, the integer answer tracks the f32 one it replaced.
    ///
    /// This sweeps a UNIFORM field on purpose — it isolates the arithmetic from
    /// the tile quantization, which `integer_and_f32_agree_on_the_tiled_arena`
    /// covers separately and which is where the two genuinely can diverge.
    #[test]
    fn integer_concealment_matches_the_f32_model_it_replaced() {
        let mut worst = 0.0f32;
        let mut worst_case = String::new();
        for depth in [0, 8, GRASS_BARE_BELOW, 15, 24, 33, 44, GRASS_MAX_H, 64] {
            let field = move |_x: i32, _y: i32| depth;
            for eye_stance in 0..STANCE_COUNT as u8 {
                for target_stance in 0..STANCE_COUNT as u8 {
                    for range in [20, 40, 80, 150, 300] {
                        let got = grass_block(
                            Pos::from_units(0, 0),
                            stance_height(eye_stance),
                            Pos::from_units(range, 0),
                            stance_height(target_stance),
                            field,
                        )
                        .conceal() as f32
                            / FP as f32;
                        let want = f32_conceal(
                            (0.0, 0.0),
                            stance_height(eye_stance) as f32,
                            (range as f32, 0.0),
                            stance_height(target_stance) as f32,
                            &field,
                        );
                        let err = (got - want).abs();
                        if err > worst {
                            worst = err;
                            worst_case = format!(
                                "depth {depth}, eye {eye_stance} -> target {target_stance} \
                                 at {range}u: integer {got:.4} vs f32 {want:.4}"
                            );
                        }
                    }
                }
            }
        }
        assert!(worst < 0.02, "port drifted from the f32 model: {worst_case}");
    }

    /// The same comparison over the REAL tiled field, which is the hard case and
    /// the one worth stating a number for.
    ///
    /// Integer sample points can round to the far side of a hex edge from where
    /// the f32 ones landed, and since `covered` takes the WORST step, one
    /// reassigned sample near a boundary moves the answer by a whole tile's
    /// depth. So the tolerance here is much looser than the uniform-field one
    /// above, and deliberately so — this test exists to bound that effect and
    /// notice if it ever stops being an edge case. It prints the worst offender
    /// either way, so a failure explains itself.
    #[test]
    fn integer_and_f32_agree_on_the_tiled_arena() {
        let field = |x: i32, y: i32| Scenario::Arena.depth(x, y);
        let (mut worst, mut worst_case) = (0.0f32, String::new());
        let (mut n, mut sum) = (0, 0.0f32);
        for eye_stance in 0..STANCE_COUNT as u8 {
            for target_stance in 0..STANCE_COUNT as u8 {
                for range in [40, 80, 150, 300] {
                    for &(sx, sy) in SPAWN_POINTS.iter() {
                        let got = grass_block(
                            Pos::from_units(sx, sy),
                            stance_height(eye_stance),
                            Pos::from_units(sx + range, sy),
                            stance_height(target_stance),
                            field,
                        )
                        .conceal() as f32
                            / FP as f32;
                        let want = f32_conceal(
                            (sx as f32, sy as f32),
                            stance_height(eye_stance) as f32,
                            ((sx + range) as f32, sy as f32),
                            stance_height(target_stance) as f32,
                            &field,
                        );
                        let err = (got - want).abs();
                        sum += err;
                        n += 1;
                        if err > worst {
                            worst = err;
                            worst_case = format!(
                                "from ({sx},{sy}) +{range}u, eye {eye_stance} -> \
                                 target {target_stance}: integer {got:.4} vs f32 {want:.4}"
                            );
                        }
                    }
                }
            }
        }
        let mean = sum / n as f32;
        println!("tiled-field port error over {n} lines: mean {mean:.4}, worst {worst:.4}");
        println!("  worst: {worst_case}");

        // The lane CLAUDE.md quotes numbers for, both ways, so the prose can be
        // checked against the code rather than trusted.
        println!("  the documented lane, from (-150,0) east:");
        for (eye_stance, target_stance, range) in
            [(0u8, 0u8, 40), (0, 0, 80), (0, 0, 150), (2, 0, 40), (2, 0, 80)]
        {
            let got = FP
                - grass_block(
                    Pos::from_units(-150, 0),
                    stance_height(eye_stance),
                    Pos::from_units(-150 + range, 0),
                    stance_height(target_stance),
                    field,
                )
                .conceal();
            let want = 1.0
                - f32_conceal(
                    (-150.0, 0.0),
                    stance_height(eye_stance) as f32,
                    ((-150 + range) as f32, 0.0),
                    stance_height(target_stance) as f32,
                    &field,
                );
            println!(
                "    eye {eye_stance} -> target {target_stance} at {range:>3}u: \
                 integer {:.3}, f32 {want:.3}",
                got as f32 / FP as f32
            );
        }
        assert!(mean < 0.02, "port drifts on the tiled field on average: mean {mean:.4}");
        assert!(worst < 0.25, "port drifts badly somewhere on the tiled field: {worst_case}");
    }

    /// The spec the whole concealment model is anchored on: two pawns lying
    /// either side of a body's width of shin-deep grass cannot see each other.
    /// `GRASS_EXTINCTION` was chosen to make this true and the extinction table
    /// carries it, so this is what would catch the table being regenerated
    /// wrong.
    #[test]
    fn prone_pawns_cannot_see_through_shin_deep_grass() {
        let shin = 33;
        let field = |_x: i32, _y: i32| shin;
        let seen = FP
            - grass_block(
                Pos::from_units(0, 0),
                stance_height(STANCE_PRONE),
                Pos::from_units(33, 0),
                stance_height(STANCE_PRONE),
                field,
            )
            .conceal();
        assert!(
            seen * 100 / FP <= 5,
            "prone through {shin}u of shin-deep grass: {seen}/{FP} visible, want <= 5%"
        );
    }

    /// The guards that stop the above being satisfied by hiding everyone always:
    /// bare ground hides nobody, and deeper grass never hides less.
    #[test]
    fn grass_conceals_monotonically_and_bare_ground_conceals_nothing() {
        let bare = |_x: i32, _y: i32| 0;
        for stance in 0..STANCE_COUNT as u8 {
            let c = grass_block(
                Pos::from_units(0, 0),
                stance_height(stance),
                Pos::from_units(60, 0),
                stance_height(stance),
                bare,
            )
            .conceal();
            assert_eq!(c, 0, "bare ground hid a stance-{stance} pawn");
        }
        let mut last = -1;
        for depth in 0..=GRASS_MAX_H {
            let field = move |_x: i32, _y: i32| depth;
            let c = grass_block(
                Pos::from_units(0, 0),
                stance_height(STANCE_STAND),
                Pos::from_units(60, 0),
                stance_height(STANCE_STAND),
                field,
            )
            .conceal();
            assert!(c >= last, "depth {depth} concealed less than {}", depth - 1);
            last = c;
        }
    }

    /// `covered` is a ceiling, not an accumulator: grass that only reaches a
    /// standing pawn's knees leaves his head in clear air however far the line
    /// runs through it. This is the property the predecessor model lacked — it
    /// let enough distance hide anybody behind anything.
    #[test]
    fn short_grass_never_hides_a_standing_pawn_however_far_it_runs() {
        let knee = |_x: i32, _y: i32| 20;
        let far = grass_block(
            Pos::from_units(0, 0),
            stance_height(STANCE_STAND),
            Pos::from_units(600, 0),
            stance_height(STANCE_STAND),
            knee,
        );
        let visible = FP - far.conceal();
        assert!(
            visible * 100 / FP >= 30,
            "knee-deep grass erased a standing pawn at 600u: only {visible}/{FP} visible"
        );
    }

    /// Cover blocks sight, and — the part that is easy to get backwards — cover
    /// you are standing INSIDE does not blind you. A pawn in a bush is hidden
    /// by it, not blinkered by it.
    #[test]
    fn cover_blocks_sight_without_blinding_whoever_is_inside_it() {
        let bare = Scenario::GrassStrip { depth: 0, east_stance: STANCE_STAND };
        let (eye, target) = (Pos::from_units(-60, 0), Pos::from_units(60, 0));

        let clear = visible_fraction(&bare, eye, STANCE_STAND, target, STANCE_STAND, &[]);
        assert_eq!(clear, FP, "nothing in the way, yet not fully visible");

        let between = [Occluder { pos: Pos::from_units(0, 0), r: 30 }];
        let blocked = visible_fraction(&bare, eye, STANCE_STAND, target, STANCE_STAND, &between);
        assert_eq!(blocked, 0, "a 30u boulder dead between them did not block");

        // The same boulder, with the viewer inside it.
        let inside_it = visible_fraction(
            &bare,
            Pos::from_units(0, 0),
            STANCE_STAND,
            target,
            STANCE_STAND,
            &between,
        );
        assert_eq!(inside_it, FP, "cover the eye is inside blinded it");
    }

    /// Cover degrades in fifths rather than snapping: someone edging out from
    /// behind a boulder is partly visible before they are wholly visible.
    #[test]
    fn cover_degrades_across_the_body() {
        let bare = Scenario::GrassStrip { depth: 0, east_stance: STANCE_STAND };
        let eye = Pos::from_units(-60, 0);
        let rock = [Occluder { pos: Pos::from_units(0, 0), r: 12 }];
        // Far enough at the top of the sweep that the body's NEAR edge is clear
        // too, not just its centre — at half this offset the lower sample point
        // is still behind the rock and the answer is 4/5.
        let seen: Vec<i32> = (0..=5)
            .map(|step| {
                visible_fraction(
                    &bare,
                    eye,
                    STANCE_STAND,
                    Pos::from_units(60, step * 12),
                    STANCE_STAND,
                    &rock,
                )
            })
            .collect();
        assert_eq!(seen[0], 0, "dead behind the rock, yet visible");
        assert_eq!(*seen.last().unwrap(), FP, "well clear of the rock, yet hidden");
        assert!(
            seen.windows(2).all(|w| w[1] >= w[0]),
            "edging out of cover did not reveal monotonically: {seen:?}"
        );
        assert!(
            seen.iter().any(|&v| v > 0 && v < FP),
            "cover snapped from hidden to seen with no partial step: {seen:?}"
        );
    }

    /// Prone-to-prone is hidden everywhere in the arena — the field is deep
    /// enough that going flat genuinely breaks contact, which is the mechanic
    /// the depth band was tuned for.
    #[test]
    fn prone_pawns_are_hidden_across_the_whole_arena() {
        let arena = Scenario::Arena;
        let (mut buried, mut total) = (0, 0);
        for &(sx, sy) in SPAWN_POINTS.iter() {
            for &(tx, ty) in SPAWN_POINTS.iter() {
                if (sx, sy) == (tx, ty) {
                    continue;
                }
                total += 1;
                let prone = visible_fraction(
                    &arena,
                    Pos::from_units(sx, sy),
                    STANCE_PRONE,
                    Pos::from_units(tx, ty),
                    STANCE_PRONE,
                    &[],
                );
                if prone * 100 / FP < 5 {
                    buried += 1;
                }
            }
        }
        assert_eq!(
            buried, total,
            "prone-to-prone across the arena should be hidden everywhere, {buried}/{total} were"
        );
    }
}
