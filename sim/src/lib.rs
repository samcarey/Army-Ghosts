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
/// Sessions are built for up to this many players; milestone 1 plays with 2.
pub const MAX_PLAYERS: usize = 4;

/// Arena half-extents in world units (pixels).
pub const ARENA_HALF_W: i32 = 400;
pub const ARENA_HALF_H: i32 = 300;

/// Player movement speed, subunits per tick (2 px/tick = 120 px/s).
pub const PLAYER_SPEED: i32 = 2 * FP;
/// Bullet speed, subunits per tick (8 px/tick = 480 px/s).
pub const BULLET_SPEED: i32 = 8 * FP;
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

impl PlayerInput {
    pub fn fire(&self) -> bool {
        self.buttons & BTN_FIRE != 0
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

// ── World setup ─────────────────────────────────────────────────────────────

/// Fixed spawn points (world units), one per handle. Deterministic — every
/// peer spawns the identical world before the session starts ticking.
pub const SPAWN_POINTS: [(i32, i32); MAX_PLAYERS] = [(-150, 0), (150, 0), (0, -150), (0, 150)];

/// Practice dummies sit on the spawn axis: walk straight out from spawn and
/// they're dead ahead (also makes hit registration trivially testable).
pub const TARGET_POINTS: [(i32, i32); 2] = [(-300, 0), (300, 0)];

/// Spawn the initial world: one pawn per player plus the practice targets.
/// Both clients run this identically before the first tick.
pub fn spawn_world(commands: &mut Commands, num_players: usize) {
    for handle in 0..num_players {
        let (x, y) = SPAWN_POINTS[handle];
        commands
            .spawn((
                Player { handle },
                Pos::from_units(x, y),
                Facing::default(),
                Cooldown::default(),
            ))
            .add_rollback();
    }
    for (x, y) in TARGET_POINTS {
        commands
            .spawn((Target::default(), Pos::from_units(x, y)))
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
            .rollback_component_with_copy::<Bullet>()
            .rollback_component_with_copy::<Target>()
            // Desync detection: checksum the position state every frame so a
            // nondeterminism bug surfaces as a GGRS desync event immediately,
            // not as subtly diverged worlds.
            .checksum_component_with_hash::<Pos>()
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
    mut players: Query<(&Player, &mut Pos, &mut Facing)>,
) {
    for (player, mut pos, mut facing) in &mut players {
        let (input, _status) = inputs[player.handle];
        let (mx, my) = (input.move_x as i32, input.move_y as i32);
        if mx == 0 && my == 0 {
            continue;
        }
        // Scale the joystick vector to at most PLAYER_SPEED, preserving
        // direction: v = m * SPEED / max(len, 127). Dividing by the *longer*
        // of len/127 keeps sub-max joystick deflections proportional while
        // capping diagonals at full speed.
        let len = isqrt((mx * mx + my * my) as i64).max(127) as i32;
        pos.x += mx * PLAYER_SPEED / len;
        pos.y += my * PLAYER_SPEED / len;
        pos.x = pos.x.clamp(-(ARENA_HALF_W - PLAYER_R) * FP, (ARENA_HALF_W - PLAYER_R) * FP);
        pos.y = pos.y.clamp(-(ARENA_HALF_H - PLAYER_R) * FP, (ARENA_HALF_H - PLAYER_R) * FP);
        facing.x = mx;
        facing.y = my;
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

fn move_bullets(mut commands: Commands, mut bullets: Query<(Entity, &mut Bullet, &mut Pos)>) {
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
