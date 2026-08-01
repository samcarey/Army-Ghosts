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
pub mod round;
pub mod save;
pub use bot::{bot_think, Bot, BotProfile, BotRoster, MEMORY_TICKS};
pub use round::{Phase, Round, Winner, INTERMISSION_TICKS, ROUND_SECONDS, ROUND_TICKS};
pub use save::{Dials, Save};

/// Fixed-point scale: subunits per world unit (pixel).
pub const FP: i32 = 256;
/// Simulation tick rate (GGRS rollback schedule fps).
pub const TICK_HZ: usize = 60;
/// Sessions are built for up to this many players (`?players=N` picks the
/// actual room size, default 2).
pub const MAX_PLAYERS: usize = 8;

/// Ghost War is two sides, and everything downstream assumes exactly two: the
/// spawn lines, the round's win condition, [`Team::other`] and the harness's
/// pair swap. It is a named constant so those places read as "the other side"
/// rather than as a bare `1`, not because a third team would be a config change.
pub const TEAM_COUNT: usize = 2;
/// How many posts a side musters at — and therefore the largest it can be.
pub const TEAM_SIZE: usize = MAX_PLAYERS / TEAM_COUNT;

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

/// What a pawn keeps of its pace walking SQUARE ON to where it is pointing, and
/// walking STRAIGHT BACKWARDS. Straight ahead is the full `FP`, and everything
/// between is linear in the cosine ([`heading_scale`]) — so a forward diagonal
/// keeps ~93% and a backward one ~62%.
///
/// **This became a question the moment aiming split off movement.** While
/// `Facing` was the move vector there was no such thing as walking sideways: you
/// pointed wherever you walked, by construction, and every step was a forward
/// step. A second stick makes strafing and backpedalling expressible, and a game
/// about creeping through grass should not let a player retreat from a firefight
/// as fast as they advanced into it while keeping their sights on it the whole
/// way.
///
/// It stacks multiplicatively with the stance and with [`ADS_SPEED`], which is
/// the natural reading: all three are fractions of a pace, and a soldier
/// backpedalling in a crouch with the sights up is doing three slow things at
/// once.
pub const STRAFE_SPEED: i32 = FP * 3 / 4;
pub const BACKPEDAL_SPEED: i32 = FP * 9 / 16;
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
/// Ticks a pawn flashes after taking a round (render feedback).
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

// ── How accurate the gun is ─────────────────────────────────────────────────
//
// A round no longer leaves the barrel along `Facing`. It leaves along a
// direction drawn from a cone, and how wide that cone is says everything about
// what the shooter was doing when they pulled the trigger.
//
// **This is the mechanic run-and-gun loses to.** Before it, a sprinting pawn
// and a pawn who had been lying still in the grass for a minute shot exactly
// the same round, so every incentive the rest of the game builds — stance,
// grass, patience, the whole no-respawn premise — was undercut by the one
// system that decides whether any of it pays. Now the cone is the price list.
//
// Three inputs, kept separate because they answer different questions and tune
// independently:
//
// * **[`Aim::sway`]** — how steady the hold is, a consequence of the pawn's
//   POSTURE: stance, speed, and whether the sights are up. It has a settled
//   value for any given posture and it EASES toward it, fast in the wrong
//   direction and slowly in the right one (see [`SWAY_SETTLE_TICKS`]).
// * **[`Aim::bloom`]** — recoil from rounds already fired, an accumulator that
//   gains on every shot and bleeds off every tick. This is both of the "when
//   did you last shoot" questions at once: how long ago falls out of the decay,
//   and how long you have been holding the trigger falls out of the gain
//   outrunning it.
// * **[`SPREAD_MAX`]** — what the sum of those two is worth in angle.
//
// Everything here is integer and every step is idempotent given the tick's
// inputs, so a replayed tick lands on the identical cone. The draw itself comes
// from an LCG in [`Aim`] that is rolled back with the rest of the pawn, for the
// same reason [`bot::Bot`]'s is: a shot that deviated differently on a
// re-simulated frame is a desync, not a glitch.

/// Widest the cone ever gets, as the **tangent of its half-angle** in `FP` —
/// so the half-width at range `d` is `SPREAD_MAX * spread / FP * d / FP`.
///
/// A tangent rather than an angle because the whole point is where the round
/// lands, and `offset = tan * distance` is one multiply where an angle would be
/// a trig table. 0.22 is about 12°: at 100 units the cone is 22 wide either
/// side of a pawn whose own radius is [`PLAYER_R`] (12), so a fully unsteady
/// shooter misses roughly half its rounds at that range and nearly all of them
/// at 260. Point blank it still connects, which is the intended shape — spray
/// is a close-quarters answer, not a substitute for aiming.
pub const SPREAD_MAX: i32 = FP * 32 / 100;

/// The sway a pawn settles to in each stance, standing still with the sights
/// down. `0..=FP`, indexed like [`STANCE_SPEED`].
///
/// Standing is deliberately mediocre rather than bad: half the cone is still
/// 11 units at 100, so a standing shooter who has stopped moving can fight, and
/// wants the sights up. Prone is nearly free, which is what pays for how blind
/// and how slow it is.
pub const STANCE_SWAY: [i32; STANCE_COUNT] = [FP * 52 / 100, FP * 30 / 100, FP * 12 / 100];

/// What the sights are worth: a MULTIPLIER on the settled sway, not a
/// subtraction, so bringing them up is worth most in the stance that is already
/// steadiest. Feet planted and the weapon braced.
///
/// It multiplies rather than subtracts for a second reason, and that reason is
/// what makes it survive [`BTN_ADS`] no longer rooting the pawn: the multiplier
/// is applied to the WHOLE settled hold, movement term included, so a shooter
/// walking with the sights up is steadier than one walking without them and
/// still unsteadier than one standing still with them. Applied as a subtraction
/// — or, as it was first written, as an early return that skipped the movement
/// term entirely — the moment a shooter could walk while aiming they would walk
/// as accurately as they stood.
pub const ADS_SWAY: i32 = FP * 45 / 100;

/// What the sights cost in SPEED: the fraction of the stance's pace a shooter
/// keeps while aiming down them.
///
/// It used to be zero — the sights stopped you dead — and that is a harsher rule
/// than the accuracy model needs. The sway model already prices moving while
/// aiming, twice over (the movement term above, and [`Aim::stir`] giving you
/// away in the grass), so rooting the pawn on top of it was charging for the
/// same decision three times and taking the choice away rather than pricing it.
/// Walking pace with the weapon up is a real thing a soldier does, and now the
/// player can: they just do it at a pace anyone watching has time to react to.
///
/// It stacks with the stance, so a crawl with the sights up is very slow indeed
/// — which is exactly the sniper's hold, and it should cost what it costs.
pub const ADS_SPEED: i32 = FP * 45 / 100;

/// What travelling at full standing speed ADDS to the settled sway, on top of
/// the stance's own. Scaled by the square of the fraction of [`PLAYER_SPEED`]
/// actually being covered, so a half-deflection walk costs a quarter of a run
/// and the difference between creeping and charging is most of the dial.
///
/// Large enough that standing plus a full sprint saturates the cone: 0.52 +
/// 0.60 is past `FP`. That is the intended reading — a running shooter is not
/// inaccurate, they are not aiming.
pub const MOVE_SWAY: i32 = FP * 60 / 100;

/// Ticks to bleed the sway all the way from saturated down to a settled hold.
/// 96 is 1.6 s, which is the "second or two" a shooter needs after coming to a
/// stop before the sights mean anything.
///
/// **The asymmetry with [`SWAY_RISE_TICKS`] is the whole mechanic**, not a
/// polish detail. If steadiness came back as fast as it went, a run-and-gunner
/// could stop for two ticks and shoot as well as someone who had been holding
/// the corner for a minute, and every constant above would be decoration.
/// Unsteadiness is instant; steadiness is earned.
pub const SWAY_SETTLE_TICKS: i32 = 96;
/// And back up. Ten ticks — a sixth of a second — so the moment you move, you
/// have lost it.
pub const SWAY_RISE_TICKS: i32 = 10;

/// What one round adds to [`Aim::bloom`], before the stance's share of it.
pub const RECOIL_PER_SHOT: i32 = FP * 20 / 100;
/// How much of a round's kick the shooter eats in each stance. Braced against
/// the ground, most of it goes into the ground.
pub const STANCE_RECOIL: [i32; STANCE_COUNT] = [FP, FP * 72 / 100, FP * 52 / 100];
// Two more inputs joined the model after the first three shipped, and both are
// about MOVEMENT — one about the target's, one about the shooter's barrel:
//
// * **[`Aim::stir`]** — how much this pawn has been moving lately, and it does
//   not feed the cone at all: it feeds CONCEALMENT. Grass hides a still body
//   and betrays a moving one, so a sprinting pawn forfeits most of what the
//   grass was doing for it ([`MOTION_REVEAL`]) and keeps forfeiting it for the
//   second it takes the sward to settle ([`STIR_DECAY`]). This is the half of
//   "careful play should win" that the cone alone could not buy: the first
//   model taxed a runner's OWN shooting, but a bot (or a person) who charges
//   and only fires after arriving never paid it. Being SEEN on the way in is
//   the tax that cannot be opted out of.
// * **[`Aim::swing`]** — how fast the aim direction has been traversing. A
//   barrel being swung is not a barrel on target: turning faster than
//   [`SWING_FREE`] charges the cone open and holding steady hones it back in
//   over [`SWING_DECAY`]. What this prices is TRACKING, and the price is
//   naturally steepest up close — a target crossing at walking speed forces
//   about 3°/tick of traverse at 40 units and less than 1° at 150, because
//   angular rate is speed over range. So shooting a mover is harder than
//   shooting a camper, most of all at knife range, with nothing in the code
//   that mentions the target at all.

/// How much of its grass concealment a pawn moving at full standing speed
/// forfeits, `0..=FP`. Scaled by the SQUARE of [`Aim::stir`], so creeping is
/// nearly free and the cost lives at the top of the speed range — and since
/// stance caps speed, the stances price themselves: a sprint forfeits 75%, a
/// crouched trot ~23%, a prone crawl ~7%. Crawling is stealth; running is a
/// flag.
pub const MOTION_REVEAL: i32 = FP * 75 / 100;
/// What [`Aim::stir`] loses per tick once the pawn stops: `FP / 64` is exactly
/// 4, so a full sprint takes 64 ticks — a second — to fade back to still. (A
/// power-of-two divisor on purpose; see the `FP is 256` gotcha.) The rise has
/// no constant because it is instant — grass starts moving the moment you do,
/// and only the settling takes time. That second of lingering visibility is the
/// mechanic: stopping does not un-ring the bell, so moving is a decision about
/// the next second, not the next tick.
pub const STIR_DECAY: i32 = FP / 64;

/// Aim traverse under this many FP-sin-units per tick charges nothing —
/// honing. 5 subunits is about 1.1°/tick (67°/s): fine corrections and slow
/// tracking of a distant target stay inside it, whipping to a new target or
/// tracking someone strafing past your muzzle does not.
pub const SWING_FREE: i32 = 5;
/// What each subunit of excess traverse adds to [`Aim::swing`]. At 4, a 30°
/// flick saturates the cone outright and tracking a walking pawn at 40 units
/// (~3°/tick) opens it in about a sixth of a second — while the same target at
/// 150 units tracks for free. Which is the point: angular rate is speed over
/// range, so this is what makes a close mover hard to hit without one line of
/// code that asks how far away anything is.
pub const SWING_GAIN: i32 = 4;
/// What [`Aim::swing`] loses per tick while the barrel holds still: a plain
/// integer of subunits (not a fraction of `FP`, which truncates — see the
/// gotcha), so a fully swung aim hones back in over ~43 ticks, seven tenths of
/// a second.
pub const SWING_DECAY: i32 = 6;

/// How hard the draw leans toward the middle of the cone, as a fraction of
/// `FP`. `FP` is exactly uniform; below it the middle is denser. Concretely the
/// inner half of all rounds falls inside `SPREAD_CORE / 2` of the half-width —
/// at 7/8 that is the inner 44% rather than the inner 50%.
///
/// The distribution is therefore two flat steps with a hard edge at the rim,
/// and the density ratio between them is `(2 - c) / c` — at 15/16 that is
/// **17 : 15**, a slight lean toward the middle that leaves the outer half of
/// the cone thoroughly live.
///
/// # Leaning inward is a SUBSIDY TO THE CARELESS, and it has to be paid for
///
/// This is the thing to understand before touching either constant. The lean
/// raises the odds a round lands by a factor that grows with how bad the cone
/// already is: a tight cone gets nothing, because it was hitting anyway, while
/// a saturated one gets the full `1/c`. So a middle-heavy draw quietly refunds
/// accuracy in proportion to how little the shooter has earned — which is
/// precisely backwards for a game whose whole accuracy model exists to make
/// carelessness expensive.
///
/// Measured, against the shipping profile, with the full run-and-gun kit
/// (`aggression=0.95, caution=0.05, discipline=0.0`) as the candidate:
///
/// | draw                          | run-and-gun scores |
/// |-------------------------------|--------------------|
/// | flat, `SPREAD_MAX` 0.22       |            +14 elo |
/// | core 7/8, `SPREAD_MAX` 0.25   |           +108 elo |
/// | core 7/8, `SPREAD_MAX` 0.36   |            +16 elo |
/// | core 15/16, `SPREAD_MAX` 0.26 |           +137 elo |
/// | core 15/16, `SPREAD_MAX` 0.32 |            +35 elo |
///
/// So the shape was kept gentle (15/16 rather than 7/8) AND [`SPREAD_MAX`] was
/// widened from 0.22 to 0.32 to pay for what remains. The two moves together
/// leave a settled shooter feeling much as it did — the lean puts back in the
/// middle what the width took off — while a saturated cone is genuinely wider
/// than it was. **Change one of these and you must re-measure the other.**
///
/// One caveat on those numbers, because it bit during the tuning: the last two
/// rows were first measured at **-61** and only became what they are after two
/// bot stalls were fixed in the same session. Both stalls flattered careful
/// play, because a side that freezes cannot lose a round it would otherwise
/// have lost. Any bot-behaviour change invalidates a run-and-gun measurement;
/// re-run it, and check `a_match_never_stops_moving_around_an_idle_player`
/// first to be sure the number is about play rather than about paralysis. Both properties are deliberate: a bell
/// would fade the rim out and quietly refund most of what a wide cone is
/// supposed to cost, while a flat draw reads as a broken gun rather than an
/// unsteady soldier.
///
/// Piecewise-linear rather than a curve for one reason that is worth the
/// constraint: it **inverts in closed form**, so [`bot::shot_quality`] — which
/// is the odds a round lands, and which the bots' trigger discipline and their
/// whole close-the-distance judgement are built on — stays exact integer
/// arithmetic instead of an erf approximation. A shape the bots cannot compute
/// the hit probability of is a shape that silently miscalibrates them.
pub const SPREAD_CORE: i32 = FP * 15 / 16;

/// Map a uniform quantile in `±FP` onto `±FP`, leaning toward the middle.
///
/// The inner half of the range is compressed into [`SPREAD_CORE`] of the
/// output and the outer half is stretched over the rest, so `|taper(±FP)| ==
/// FP` exactly — the rim of the cone is exactly where it was, which is what
/// "sharp cutoff at the current edges" means.
pub fn taper(u: i64) -> i64 {
    let fp = FP as i64;
    let c = SPREAD_CORE as i64;
    let t = u.abs().min(fp);
    let mag = if t <= fp / 2 {
        c * t / fp
    } else {
        c / 2 + (2 * fp - c) * (t - fp / 2) / fp
    };
    if u < 0 {
        -mag
    } else {
        mag
    }
}

/// [`taper`] inverted: what share of draws land within `y` of the middle, where
/// `y` is a fraction of the cone's half-width in `FP`. Exact, by construction.
pub fn taper_share(y: i64) -> i64 {
    let fp = FP as i64;
    let c = SPREAD_CORE as i64;
    let y = y.clamp(0, fp);
    if y <= c / 2 {
        y * fp / c
    } else {
        fp / 2 + (y - c / 2) * fp / (2 * fp - c)
    }
}

/// What bleeds off the bloom every tick: a saturated one clears in 128 ticks,
/// which is 2.1 s of holding fire.
///
/// **Check the arithmetic in whole subunits before changing any of these.**
/// `FP` is 256, so a decay expressed as a fraction of it is a very small integer
/// and rounds hard: this was first written `FP / 150`, which truncates to **1**
/// rather than the intended 1.7, and that one lost subunit a tick was enough to
/// make every stance saturate. A prone shooter meant to hold a steady bipod was
/// spraying at `SPREAD_MAX` after eight rounds, and the arena test that fires
/// down the clear lane could not kill anybody from any posture at all.
/// [`recoil_settles_at_a_different_rate_in_each_stance`] pins the three
/// relationships so the next truncation is a failing test rather than a mystery.
///
/// What it buys, per [`FIRE_COOLDOWN`] (12 ticks, so 24 subunits recovered
/// between rounds) against each stance's share of a 51-subunit kick:
///
/// | stance   | kick | net a round | saturates after |
/// |----------|------|-------------|-----------------|
/// | standing |   51 |         +27 |  9 rounds (2 s) |
/// | crouched |   37 |         +13 | 19 rounds (4 s) |
/// | prone    |   27 |          +3 | 85 rounds (17s) |
///
/// Which is the burst-discipline rule falling out of the arithmetic rather than
/// being written down separately: the first round after a pause is the accurate
/// one in every stance, and how many you get after it is what the stance buys.
/// Prone is nearly a sustained-fire position, and that is deliberate — it is
/// paid for in the concealment model, where going flat in deep grass costs most
/// of what you can see.
pub const RECOIL_DECAY: i32 = FP / 128;

/// The only thing that crosses the network: one player's input for one tick.
/// Kept tiny (ggrs serializes it with serde every tick). Joystick axes are
/// quantized to i8 (-127..=127); `buttons` and `dials` are bitfield bytes.
///
/// `dials` is a second byte because `buttons` ran out — fire, sights, two bits
/// of stance and four of bot count fill it exactly. Everything in either byte
/// is an **absolute value re-sent every tick**, never an edge, for the reason
/// spelled out on [`BTN_BOTS_SHIFT`]: rollback replays a tick as often as it
/// likes, and only an idempotent input replays to the same world.
///
/// # Every multi-bit field reads zero as "not asking"
///
/// Stance, bot count, aggression and the team request are all encoded as
/// `1 + value`, so `PlayerInput::default()` asks for **nothing at all** rather
/// than asking to stand up, for no bots, for side zero. That is not tidiness:
/// `default()` is exactly what GGRS hands the sim for a **disconnected**
/// player (`sync_layer::synchronized_inputs` substitutes `T::Input::default()`
/// from the disconnect frame on), so the zero encoding is the one an absent
/// player transmits whether they mean to or not.
///
/// Before this, refreshing the browser stood your pawn up out of the grass it
/// was hiding in — and if you happened to hold handle 0, deleted every bot in
/// the match on the way out, because bots-not-asked-for and bots-none were the
/// same four bits. See `persist.rs` and [`save`] for what a player is expected
/// to do while gone: nothing, in the position they left, still shootable.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct PlayerInput {
    pub move_x: i8,
    pub move_y: i8,
    /// Where the barrel is pointed, as a direction only — its LENGTH is never
    /// read, so a peer may send whatever the stick gave it.
    ///
    /// Zero means "not aiming separately", and [`PlayerInput::aim`] then falls
    /// back to the move vector. That fallback is what keeps every caller that
    /// steers by walking — the bots, the keyboard, every test in the repo —
    /// working exactly as it did when the two were the same number.
    pub aim_x: i8,
    pub aim_y: i8,
    pub buttons: u8,
    /// Bits 0-3: bot aggression ([`DIAL_AGGRO_MASK`]), which only the first
    /// player's copy is read for. Bits 4-5: which side THIS player is asking to
    /// fight on ([`DIAL_TEAM_MASK`]) — everyone's own copy counts, because it is
    /// about them. Bits 6-7 spare.
    pub dials: u8,
}

pub const BTN_FIRE: u8 = 1 << 0;
/// Aiming down sights: the weapon comes up, the hold steadies ([`ADS_SWAY`]) and
/// the pace drops to [`ADS_SPEED`]. It used to plant the feet outright.
pub const BTN_ADS: u8 = 1 << 1;
/// Bits 2-3 carry the stance the player *wants*, as `1 + level` — an absolute
/// level, not a "go down" edge. Edge-triggered inputs would need the sim to
/// remember last tick's buttons, which is exactly the kind of hidden state
/// rollback punishes; a level re-sent every tick re-applies identically no
/// matter how often the frame is replayed.
///
/// **Zero is "not asking", not "stand"**, and the two bits hold `0..=3` so the
/// three levels still fit exactly. A pawn nobody is asking anything of keeps the
/// stance it had, which is what a disconnected player has to do: see the type
/// doc on [`PlayerInput`].
pub const BTN_STANCE_SHIFT: u8 = 2;
pub const BTN_STANCE_MASK: u8 = 0b11 << BTN_STANCE_SHIFT;
/// Bits 4-7 carry how many bots the *first* player wants in the match, as
/// `1 + count` for `0..=8` (so `0` is "not asking" and the largest code is 9,
/// inside the four bits).
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

/// Bits 0-3 of [`PlayerInput::dials`]: how aggressive the first player wants
/// the bots, as a LEVEL rather than the raw `0..=FP` value — four bits is all
/// there is, and a tenth is finer than anyone can judge by eye anyway.
///
/// **Zero means "don't touch it"**, which is what makes this safe to add: a
/// caller that never sets the dial (the self-play harness, whose whole job is
/// to give the two sides *different* aggression from a [`BotRoster`]) keeps the
/// profile it asked for. Levels `1..=AGGRO_LEVELS` map onto `0.0..=1.0`, so the
/// menu's lowest setting is genuinely zero rather than "nearly zero".
///
/// Applied by [`apply_bot_dials`] every tick, to every bot, from an absolute
/// value — the same idempotence argument as the bot count, and the reason the
/// dial can be turned mid-match instead of only at spawn.
pub const DIAL_AGGRO_MASK: u8 = 0b1111;
/// How many positions the aggression dial has. 11, so the steps are tenths.
pub const AGGRO_LEVELS: u8 = 11;

/// Bits 4-5 of [`PlayerInput::dials`]: the side this player would rather be on.
/// `0` is "no preference — put me where the sides need me", `1` and `2` name a
/// team.
///
/// Unlike the two bot dials this is read from EVERY player's own input rather
/// than only the first's, because it is a statement about the sender. It is
/// still an absolute value re-sent every tick for the usual reason, and it is
/// still only a *request*: [`round::balance`] grants it when the sides have room
/// and quietly overrules it when they don't, so no amount of tapping can stack
/// one end of the map. Requests are honoured at the top of a round, never in the
/// middle of one — swapping sides mid-firefight would teleport your allegiance
/// without teleporting you.
pub const DIAL_TEAM_SHIFT: u8 = 4;
pub const DIAL_TEAM_MASK: u8 = 0b11 << DIAL_TEAM_SHIFT;

impl PlayerInput {
    /// Which way this pawn is pointing its weapon: the aim stick if it is being
    /// asked for, else wherever the pawn is walking.
    ///
    /// The fallback is the whole reason aiming could be split off movement
    /// without touching anything else. `Facing` used to BE the move vector, so
    /// every producer of an input in this repo — the bots, the keyboard, the
    /// harness, the tests — aims by walking; leaving `aim_*` at zero keeps all
    /// of them pointing exactly where they used to, and only the twin-stick
    /// touch controls fill it in.
    pub fn aim(&self) -> (i32, i32) {
        match (self.aim_x as i32, self.aim_y as i32) {
            (0, 0) => (self.move_x as i32, self.move_y as i32),
            aim => aim,
        }
    }
    pub fn fire(&self) -> bool {
        self.buttons & BTN_FIRE != 0
    }
    pub fn ads(&self) -> bool {
        self.buttons & BTN_ADS != 0
    }
    /// The stance this player is asking for, or `None` for "leave them as they
    /// are" — see [`BTN_STANCE_SHIFT`]. Clamped, so a peer sending the top code
    /// can't index anything out of range on our side.
    pub fn stance(&self) -> Option<u8> {
        match (self.buttons & BTN_STANCE_MASK) >> BTN_STANCE_SHIFT {
            0 => None,
            code => Some((code - 1).min(STANCE_PRONE)),
        }
    }
    pub fn set_stance(&mut self, level: u8) {
        self.buttons &= !BTN_STANCE_MASK;
        self.buttons |= (level.min(STANCE_PRONE) + 1) << BTN_STANCE_SHIFT;
    }
    /// How many bots this player is asking for, or `None` for "however many
    /// there are" — the same sentinel as the stance, and the reason a
    /// disconnected handle 0 doesn't empty the arena. Clamped so a peer sending
    /// the top code can't ask for more pawns than there are spawn points.
    pub fn bots(&self) -> Option<u8> {
        match (self.buttons & BTN_BOTS_MASK) >> BTN_BOTS_SHIFT {
            0 => None,
            code => Some((code - 1).min(MAX_PLAYERS as u8)),
        }
    }
    pub fn set_bots(&mut self, count: u8) {
        self.buttons &= !BTN_BOTS_MASK;
        self.buttons |= (count.min(MAX_PLAYERS as u8) + 1) << BTN_BOTS_SHIFT;
    }
    /// The aggression this player is dialling, `0..=FP`, or `None` for "leave
    /// the bots' own profiles alone" — see [`DIAL_AGGRO_MASK`].
    pub fn aggression(&self) -> Option<i32> {
        let level = (self.dials & DIAL_AGGRO_MASK).min(AGGRO_LEVELS);
        (level > 0).then(|| (level as i32 - 1) * FP / (AGGRO_LEVELS as i32 - 1))
    }
    /// `level` is `1..=AGGRO_LEVELS`; 0 clears the dial.
    pub fn set_aggression(&mut self, level: u8) {
        self.dials &= !DIAL_AGGRO_MASK;
        self.dials |= level.min(AGGRO_LEVELS);
    }
    /// Which side this player is asking for, or `None` for "wherever you need
    /// me". A peer sending the unused fourth code reads as no preference rather
    /// than as a team that doesn't exist.
    pub fn team_request(&self) -> Option<u8> {
        match (self.dials & DIAL_TEAM_MASK) >> DIAL_TEAM_SHIFT {
            code if code >= 1 && (code as usize) <= TEAM_COUNT => Some(code - 1),
            _ => None,
        }
    }
    pub fn set_team_request(&mut self, team: Option<u8>) {
        self.dials &= !DIAL_TEAM_MASK;
        let code = match team {
            Some(side) if (side as usize) < TEAM_COUNT => side + 1,
            _ => 0,
        };
        self.dials |= code << DIAL_TEAM_SHIFT;
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
/// `spawn_post`) wants a small unique id and there is no reason for bots to
/// need a second kind. What a bot does NOT have is a seat in the session: see
/// [`Intent`] for how it is driven instead.
#[derive(Component, Copy, Clone, Default, Debug, Hash)]
pub struct Player {
    pub handle: usize,
}

/// Which side a pawn is fighting for, `0..TEAM_COUNT`.
///
/// Sim state rather than a lobby fact, and rollback-registered like everything
/// else, because it decides who a bot shoots at and who wins the round — a peer
/// that disagreed about one pawn's colours would disagree about the result.
/// Reassigned only between rounds ([`round::balance`]), so a firefight can't
/// change hands halfway through.
#[derive(Component, Copy, Clone, Default, Debug, Hash, PartialEq, Eq)]
pub struct Team(pub u8);

impl Team {
    /// Clamped, so a value that somehow escaped the balancer can still index the
    /// per-side arrays the round keeps.
    pub fn index(self) -> usize {
        (self.0 as usize).min(TEAM_COUNT - 1)
    }
    pub fn other(self) -> Team {
        Team((TEAM_COUNT - 1 - self.index()) as u8)
    }
}

/// Which side a handle starts on: alternating, so the sides fill evenly however
/// many pawns turn up and in whatever order.
///
/// It is a pure function of the handle on purpose. Team membership is then
/// knowable without looking at the world — which is what lets the self-play
/// harness put one profile on each side by parity, and what makes
/// [`reconcile_bots`] able to place a bot without consulting anything that could
/// differ between peers. A player who asks to switch overrides it from the next
/// round on; nothing else does.
pub fn default_side(handle: usize) -> u8 {
    (handle % TEAM_COUNT) as u8
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

/// Where the barrel points: the last non-zero [`PlayerInput::aim`], raw joystick
/// range (-127..=127 per axis). Bullets fire along this. Defaults to "up".
///
/// It used to be the last non-zero MOVE direction, which is still what it works
/// out to for anything steering by walking — a bot, a keyboard, the harness. A
/// player with two thumbs on the glass can point it somewhere else.
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

/// How well this pawn is holding its weapon — the state behind the cone a round
/// is drawn from. See the constant block above [`SPREAD_MAX`] for the model;
/// this is just where it lives.
///
/// Rollback-registered and checksummed like [`Stance`], and for the same
/// reason: two peers disagreeing about how steady a shooter is disagree about
/// where its rounds went, and waiting for that to show up in [`Pos`] is waiting
/// for someone to die of it.
#[derive(Component, Copy, Clone, Debug, Hash)]
pub struct Aim {
    /// `0..=FP` — unsteadiness from posture. Eased toward [`Aim::settled`].
    pub sway: i32,
    /// `0..=FP` — recoil owed from rounds already fired.
    pub bloom: i32,
    /// `0..=FP` — how much this pawn has been MOVING lately: jumps straight to
    /// the tick's speed fraction and fades by [`STIR_DECAY`]. Feeds the
    /// concealment model, not the cone — see [`MOTION_REVEAL`].
    pub stir: i32,
    /// `0..=FP` — how fast the aim has been TRAVERSING lately: charged by
    /// [`Aim::turn`] and honed back down by [`SWING_DECAY`]. The third term of
    /// the cone.
    pub swing: i32,
    /// LCG state for the next round's deviation. Advanced ONLY when a round is
    /// actually fired, so two pawns holding fire never drift apart.
    seed: u32,
}

impl Default for Aim {
    fn default() -> Self {
        Self::seeded(0, 0)
    }
}

/// Salt for aim RNG seeding, so a round's deviation is uncorrelated with the
/// bots' aim jitter and with the terrain fields.
const AIM_SEED: u32 = 0x5CA7_7E12;

impl Aim {
    /// A fresh, perfectly steady hold, with dice of its own.
    ///
    /// Hashed from the handle for the reason [`bot::Bot::seeded`] hashes: two
    /// pawns firing on the same tick from consecutive seeds would pull visibly
    /// correlated deviations, and a whole squad's rounds veering the same way is
    /// exactly the tell that gives a fake random away. `salt` comes from
    /// [`bot::BotRoster`] for bots, so the self-play harness gets a genuinely
    /// different match out of a different salt.
    ///
    /// That last part fixes something the harness used to say about itself: a
    /// bot with `accuracy = 1.0` never touched its dice, so two such profiles
    /// left exactly one distinct pair to play. The gun draws on every shot
    /// whatever the profile is, so the salt is never inert now.
    pub fn seeded(handle: usize, salt: u32) -> Self {
        let mut seed = (handle as u32)
            .wrapping_mul(0x9E37_79B9)
            .wrapping_add(salt.wrapping_mul(0x85EB_CA6B))
            ^ AIM_SEED;
        seed ^= seed >> 15;
        seed = seed.wrapping_mul(0xC2B2_AE35);
        seed ^= seed >> 13;
        Self {
            // As steady as standing still is — see `rest`, which is the same
            // statement at the top of every round after this one.
            sway: STANCE_SWAY[STANCE_STAND as usize],
            bloom: 0,
            stir: 0,
            swing: 0,
            seed,
        }
    }

    /// Rebuild from stored numbers — [`save`] carries all three across a
    /// rejoin, so a player who reloads mid-burst comes back owing the same
    /// recoil they left owing.
    ///
    /// **The seed is taken exactly as given, and forcing it odd here was a bug.**
    /// The obvious defensive `seed | 1` — copied from [`bot::Bot::seeded`], where
    /// it is harmless — flips the low bit of every even seed, so half of all
    /// restored pawns came back with a gun that was not the gun that was saved.
    /// `a_restored_world_is_the_world_that_was_saved` caught it as a one-off in a
    /// single field, which is exactly what that test is for.
    ///
    /// There is nothing to defend against in any case: this LCG has an odd
    /// additive constant and a multiplier congruent to 1 mod 4, so it is
    /// full-period over every 32-bit value and no seed is a fixed point, zero
    /// included. Only `sway` and `bloom` need clamping, and they are clamped
    /// because this is fed from `localStorage` and from the network.
    pub fn from_parts(sway: i32, bloom: i32, stir: i32, swing: i32, seed: u32) -> Self {
        Self {
            sway: sway.clamp(0, FP),
            bloom: bloom.clamp(0, FP),
            stir: stir.clamp(0, FP),
            swing: swing.clamp(0, FP),
            seed,
        }
    }

    pub fn seed(&self) -> u32 {
        self.seed
    }

    /// The sway this posture settles to, `0..=FP`. `speed` is the distance the
    /// pawn will actually cover this tick, in subunits — see [`step`].
    pub fn settled(stance: &Stance, speed: i32, ads: bool) -> i32 {
        let level = (stance.level as usize).min(STANCE_COUNT - 1);
        let mut sway = STANCE_SWAY[level];
        // Square of the fraction of a full run being covered.
        let frac = (speed.clamp(0, PLAYER_SPEED) as i64 * FP as i64 / PLAYER_SPEED as i64) as i32;
        sway += (frac as i64 * frac as i64 / FP as i64 * MOVE_SWAY as i64 / FP as i64) as i32;
        sway = sway.min(FP);
        // The sights steady the whole hold, movement included — see [`ADS_SWAY`]
        // for why the multiply has to come AFTER the movement term rather than
        // instead of it. This used to return early here, which was harmless only
        // while the sights rooted the pawn and `speed` was therefore always 0.
        if ads {
            sway = sway * ADS_SWAY / FP;
        }
        sway
    }

    /// One tick of settling toward `target`. Rises in [`SWAY_RISE_TICKS`] and
    /// falls in [`SWAY_SETTLE_TICKS`] — the asymmetry is the mechanic.
    ///
    /// Linear rather than exponential on purpose: an integer exponential decay
    /// stalls short of its target on the rounding, and a linear ramp makes the
    /// settle time an exact, statable number of ticks instead of an asymptote.
    pub fn ease(&mut self, target: i32) {
        let target = target.clamp(0, FP);
        if target > self.sway {
            self.sway = (self.sway + (FP / SWAY_RISE_TICKS).max(1)).min(target);
        } else {
            self.sway = (self.sway - (FP / SWAY_SETTLE_TICKS).max(1)).max(target);
        }
    }

    /// What a round start hands everyone: owing no recoil, and holding a weapon
    /// exactly as steadily as someone standing still on a muster line holds one.
    ///
    /// **Not zero**, which is what it was first and which handed every pawn on
    /// the field one free unmissable shot at the top of every round. `sway` is
    /// eased toward the posture's settled value, so starting it below that value
    /// is starting everybody steadier than the posture they are actually in —
    /// and since standing is the posture a round starts in, the free shot went
    /// to whoever pulled the trigger first. Caught by
    /// `rounds_kill_and_the_dead_stay_down`, which was trying to prove a
    /// standing shooter CANNOT reach across the arena and found it landing a
    /// round for 52 damage before it had settled into anything.
    ///
    /// The dice are deliberately left where they are; see the call site in
    /// [`round`].
    pub fn rest(&mut self) {
        self.sway = STANCE_SWAY[STANCE_STAND as usize];
        self.bloom = 0;
        self.stir = 0;
        self.swing = 0;
    }

    /// One tick of recoil bleeding away, grass settling, and the barrel honing
    /// back in.
    pub fn cool(&mut self) {
        self.bloom = (self.bloom - RECOIL_DECAY).max(0);
        self.stir = (self.stir - STIR_DECAY).max(0);
        self.swing = (self.swing - SWING_DECAY).max(0);
    }

    /// A round left this pawn's barrel: light it up.
    ///
    /// **This is what makes holding fire worth anything.** Firing in this game
    /// costs no ammunition and no reload, so before this a withheld shot was
    /// pure forfeited damage and no trigger discipline could ever beat spray on
    /// expectation — measured, painfully: `discipline=0.0` beat the default in
    /// 18 decisive pairs of 18, and widening the cone only made it worse,
    /// because a sprayer still lands the odd round while a discliner lands
    /// nothing at all. The missing cost was CONCEALMENT: a muzzle flash in a
    /// grass field is a flare, so one aimed shot buys a second of being seen
    /// ([`STIR_DECAY`]) and a held trigger is a held flare. Bots need no code to
    /// exploit it — the flash rides the same [`Aim::stir`] the concealment
    /// model already reads, so gunfire pulls every enemy eye toward the shooter
    /// through the ordinary sighting path.
    ///
    /// Full [`FP`] regardless of stance, deliberately: the flash is above the
    /// grass even when the shooter is under it, so prone sustained fire — which
    /// recoil makes nearly free — pays for itself here instead.
    pub fn flash(&mut self) {
        self.stir = FP;
    }

    /// Movement this tick: `speed` in subunits, the same number the pawn's
    /// [`Pos`] actually changed by ([`step_speed`]). Stir has no ease upward —
    /// see [`STIR_DECAY`].
    pub fn disturb(&mut self, speed: i32) {
        let frac =
            (speed.clamp(0, PLAYER_SPEED) as i64 * FP as i64 / PLAYER_SPEED as i64) as i32;
        self.stir = self.stir.max(frac);
    }

    /// The aim direction changed: charge the traverse. `from` and `to` are the
    /// old and new [`Facing`] vectors, in whatever length the joystick gave
    /// them — only the angle between them matters.
    ///
    /// The angle proxy is `|cross| / (|a||b|)`, the sine — exact enough below
    /// 45°/tick, and anything past 90° (`dot < 0`) is treated as the whole
    /// quarter turn, since one tick is 17 ms and nobody hones through a flip.
    /// Turns inside [`SWING_FREE`] charge nothing: slow tracking and fine
    /// corrections ARE honing, and a dead zone is also what keeps joystick
    /// wobble from taxing a player who is merely holding a direction.
    pub fn turn(&mut self, from: (i32, i32), to: (i32, i32)) {
        let (ax, ay, bx, by) = (from.0 as i64, from.1 as i64, to.0 as i64, to.1 as i64);
        let norm = isqrt((ax * ax + ay * ay) * (bx * bx + by * by));
        if norm == 0 {
            return;
        }
        let dot = ax * bx + ay * by;
        let sin = if dot < 0 {
            FP
        } else {
            ((ax * by - ay * bx).abs() * FP as i64 / norm) as i32
        };
        let excess = sin - SWING_FREE;
        if excess > 0 {
            self.swing = (self.swing + excess.saturating_mul(SWING_GAIN)).min(FP);
        }
    }

    /// Charge a round's kick, as much of it as this stance fails to absorb.
    pub fn kick(&mut self, stance: &Stance) {
        let level = (stance.level as usize).min(STANCE_COUNT - 1);
        let owed = (RECOIL_PER_SHOT as i64 * STANCE_RECOIL[level] as i64 / FP as i64) as i32;
        self.bloom = (self.bloom + owed).min(FP);
    }

    /// How wide the cone is right now, `0..=FP`. The three terms simply add:
    /// posture, recoil and traverse are independent costs and a shooter paying
    /// several pays several.
    pub fn spread(&self) -> i32 {
        (self.sway + self.bloom + self.swing).clamp(0, FP)
    }

    /// The half-width of the cone at `range` world units, in world units. What
    /// a bot asks when deciding whether a round is worth firing, and what the
    /// client draws.
    pub fn cone_half_width(spread: i32, range_units: i32) -> i32 {
        let tan = SPREAD_MAX as i64 * spread.clamp(0, FP) as i64 / FP as i64;
        (tan * range_units.max(0) as i64 / FP as i64) as i32
    }

    /// Draw this round's deviation: the tangent of the angle it leaves the
    /// barrel at, within `±SPREAD_MAX * spread`.
    ///
    /// **Slightly denser in the middle, and dead flat to a hard edge** — see
    /// [`taper`]. Not a bell: a Gaussian (or its cheap cousin, the average of
    /// two uniforms) tapers the density to nothing at the rim, which makes a
    /// wide cone far less punishing than it reads on screen, and the cone is a
    /// price that should be paid close to in full. Not flat either, which is
    /// what this was first: a shot with no tendency at all toward where it was
    /// pointed reads as the gun being broken rather than the shooter being
    /// unsteady.
    fn deviate(&mut self) -> i32 {
        let span = SPREAD_MAX as i64 * self.spread() as i64 / FP as i64;
        if span <= 0 {
            return 0;
        }
        self.seed = self
            .seed
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        // A uniform quantile in ±FP, then tapered. Drawing the quantile rather
        // than the offset keeps the taper independent of how wide the cone
        // happens to be.
        let fp = FP as i64;
        let u = ((self.seed >> 8) as i64 % (2 * fp + 1)) - fp;
        (span * taper(u) / fp) as i32
    }
}

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

/// A pawn's condition. `hp` runs to zero, `down` counts how long it has been
/// out, and `hurt` is render feedback.
///
/// **There is no respawn timer, because there is no respawning.** A round is
/// fought with the people who start it: once `down` is non-zero the pawn is out
/// until the next round begins, which is the whole reason a Ghost War round has
/// any tension in it. `down` counts UP rather than down for that reason — there
/// is nothing to count toward, and the client uses it to know how long you have
/// been spectating.
///
/// While down the pawn can't move, fire, change stance or be hit, and the client
/// hides it. That's deliberately one flag rather than despawning the entity — a
/// rollback that un-kills someone then only has to restore a component, instead
/// of resurrecting an entity whose identity the renderer has already forgotten.
#[derive(Component, Copy, Clone, Debug, Hash)]
pub struct Health {
    pub hp: i32,
    /// Ticks this pawn has been out of the round; 0 means alive.
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

/// Where each side musters, world units: a line of [`TEAM_SIZE`] posts down
/// each end of the arena, so a round opens with the two teams the length of the
/// field apart and has to be walked into.
///
/// **The two lines are exact mirrors in x**, and that is load-bearing rather
/// than tidy: the rock and bush fields are NOT mirror-symmetric, so which end a
/// side draws is worth something. The self-play harness cancels it by playing
/// every trial from both ends — which only works if the ends differ in the
/// terrain and in nothing else.
pub const TEAM_SPAWNS: [[(i32, i32); TEAM_SIZE]; TEAM_COUNT] = [
    [(-330, -195), (-330, -65), (-330, 65), (-330, 195)],
    [(330, -195), (330, -65), (330, 65), (330, 195)],
];

/// The post a pawn falls in at. Both indices are clamped: this is called from
/// inside the rollback schedule and a bad index must not panic there.
pub fn spawn_post(team: u8, slot: usize) -> (i32, i32) {
    TEAM_SPAWNS[(team as usize).min(TEAM_COUNT - 1)][slot.min(TEAM_SIZE - 1)]
}

/// Every post on the field, for the layout code that has to keep them clear.
pub fn spawn_points() -> impl Iterator<Item = (i32, i32)> {
    TEAM_SPAWNS.into_iter().flatten()
}

/// Which way a side faces when the round starts: down the field at the other
/// one. Full deflection, in the joystick units [`Facing`] is kept in.
pub fn spawn_facing(team: u8) -> Facing {
    Facing { x: if team == 0 { 127 } else { -127 }, y: 0 }
}

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
/// Elbow room around the spawn posts, so a side can never be walled into its
/// own muster line.
const ROCK_SPAWN_CLEAR: i32 = 40;
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

        // Cover in the middle of the map is the whole point of the map, so
        // nothing is excluded from it any more — the lane this used to keep
        // clear ran from a spawn to a practice dummy, and both ends of it are
        // gone. What is still kept clear is the muster lines themselves.
        if spawn_points().any(|(sx, sy)| within(x, y, sx, sy, r + PLAYER_R + ROCK_SPAWN_CLEAR)) {
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
/// Keep thickets apart and off the muster lines.
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
        if spawn_points().any(|(sx, sy)| within(cx, cy, sx, sy, BUSH_SPREAD + BUSH_SPAWN_CLEAR)) {
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
/// [`Scenario::Gunfire`] is the second rig and it is the arena, not a
/// substitute for it: the real terrain, one pawn to walk around with and one
/// standing in the middle of it firing a round a second. It exists because the
/// client's sound arcs (`client/src/sound.rs`) can only be judged by walking
/// around a source that keeps making noise, and gunfire in a real match is
/// exactly what you cannot arrange to happen on cue.
///
/// Both are dev scenarios and OFFLINE ONLY — `net.rs` ignores them whenever a
/// room is set, because two peers building different worlds is a desync by
/// construction. Nothing in a real match ever sees anything but `Arena`.
#[derive(Resource, Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum Scenario {
    #[default]
    Arena,
    /// A wall of grass `depth` units deep. The east pawn holds `east_stance`
    /// (it has no player: only the first local handle takes input, so its
    /// stance has to be told to it — see `client/src/input.rs`).
    GrassStrip { depth: i32, east_stance: u8 },
    /// The arena, with one pawn firing on a metronome. Same terrain as the
    /// game; only who is standing where, and what the pawn nobody is driving
    /// does with its trigger, are different.
    Gunfire,
}

/// Half the width of the wall in [`Scenario::GrassStrip`], world units — one
/// fog hex across, since a flat-top hex is `2 * HEX_R` corner to corner and the
/// client's `HEX_R` is 16. `strip_table.rs` asserts the two still agree.
pub const STRIP_HALF_W: i32 = 16;
/// How far each pawn stands from the middle of the wall, world units: two hex
/// columns of `1.5 * HEX_R`, which leaves exactly one clear hex between each
/// pawn and the grass.
pub const STRIP_STANDOFF: i32 = 48;

/// How far the player starts from the noise in [`Scenario::Gunfire`], world
/// units — far enough that the first arc you see is a wide, faint one, so
/// walking in is what shows you the mechanic.
pub const DEMO_STANDOFF: i32 = 300;
/// How often the demo pawn pulls its trigger, in ticks. A round a second: often
/// enough to walk a few paces between arcs and watch the next one answer
/// differently, slow enough that they do not pile into one continuous glow.
pub const DEMO_FIRE_TICKS: u32 = TICK_HZ as u32;

impl Scenario {
    /// How deep the grass is at a point in this world, world units.
    pub fn depth(&self, x: i32, y: i32) -> i32 {
        match *self {
            Scenario::Arena | Scenario::Gunfire => grass_height(x, y),
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
            Scenario::Arena | Scenario::Gunfire => rock_layout(),
            Scenario::GrassStrip { .. } => Vec::new(),
        }
    }

    /// The stance every handle *except* the first local one asks for each tick.
    /// In the game that's just "stand"; in the rig it's how the target pawn is
    /// posed, since nothing else can pose it.
    pub fn idle_stance(&self) -> u8 {
        match *self {
            Scenario::Arena | Scenario::Gunfire => STANCE_STAND,
            Scenario::GrassStrip { east_stance, .. } => east_stance.min(STANCE_PRONE),
        }
    }

    /// Whether the handles nobody is driving are holding the trigger this
    /// frame. In the game they never are; in the gunfire demo that metronome is
    /// the entire scene.
    ///
    /// It rides in on the input stream rather than being a system in here for
    /// the reason everything else does: the sim is driven by inputs, and a pawn
    /// that fired because of a rule of its own would be a second source of
    /// truth for the trigger. `client/src/input.rs` sends it, the same place
    /// [`Scenario::idle_stance`] is sent from.
    pub fn idle_fire(&self, frame: u32) -> bool {
        match *self {
            Scenario::Gunfire => frame.is_multiple_of(DEMO_FIRE_TICKS),
            _ => false,
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

    /// The same, for a target that has been MOVING — grass hides a still body
    /// and betrays a moving one, so the concealment is scaled down by
    /// [`MOTION_REVEAL`] times the square of the target's [`Aim::stir`].
    ///
    /// The square matters: it is what makes the cost live at the top of the
    /// speed range, so a crawl is nearly free and a sprint forfeits most of the
    /// grass — and since stance caps speed, the stances price themselves
    /// without appearing here.
    ///
    /// It scales the PRODUCT rather than either term because motion is not
    /// geometry: the grass is exactly as deep and the line exactly as blocked,
    /// but the sward over a moving body is itself moving, and movement is the
    /// one thing an eye picks out of a field at any range. (Which is also why
    /// this is deliberately range-independent — you spot the grass rustling
    /// across the whole map, you just can't tell what is under it. What you CAN
    /// tell is where to watch, and that is [`Sighting::exposure`] rising.)
    pub fn conceal_moving(&self, stir: i32) -> i32 {
        let fp = FP as i64;
        let stir = stir.clamp(0, FP) as i64;
        let reveal = MOTION_REVEAL as i64 * stir * stir / (fp * fp);
        (self.conceal() as i64 * (fp - reveal) / fp) as i32
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
    target_stir: i32,
) -> i32 {
    grass_block(
        eye,
        stance_height(eye_stance),
        target,
        stance_height(target_stance),
        |x, y| scenario.depth(x, y),
    )
    .conceal_moving(target_stir)
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
pub(crate) fn segment_hits_circle(a: Pos, b: Pos, c: Pos, r: i32) -> bool {
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
    target_stir: i32,
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
    let grass = grass_conceal(scenario, eye, eye_stance, target, target_stance, target_stir);
    (seen as i64 * (FP - grass) as i64 / FP as i64) as i32
}

/// Everything every pawn has, human or bot. Kept in one place so the two kinds
/// can't drift apart — a bot that was missing a component the sim's systems
/// filter on would simply stop being simulated, silently.
fn spawn_pawn(commands: &mut Commands, handle: usize, team: Team, slot: usize) -> Entity {
    let (x, y) = spawn_post(team.0, slot);
    commands
        .spawn((
            Player { handle },
            team,
            Intent::default(),
            Pos::from_units(x, y),
            spawn_facing(team.0),
            Cooldown::default(),
            // Salt 0: a human pawn's dice are its handle's. Bots get theirs
            // re-seeded with the roster's salt in `reconcile_bots`, which is
            // the only place that knows one.
            Aim::seeded(handle, 0),
            Stance::default(),
            Health::default(),
            Deaths::default(),
            Kills::default(),
        ))
        .add_rollback()
        .id()
}

/// Spawn the initial world: one pawn per player, split between the two muster
/// lines, plus the procedural rock and bush fields. Both clients run this
/// identically before the first tick.
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
    // **A fresh world starts a fresh round, and forgetting this desynced every
    // p2p match ever played.**
    //
    // [`Round`] is a rollback-registered, CHECKSUMMED resource, and it is not
    // part of the world — it is a resource that outlives one. So the clock the
    // warmup session was running kept running straight into the match, and two
    // peers cannot have warmed up for the same length of time: the host waits
    // for everyone else to arrive. Measured in two browser tabs, at GGRS frame
    // 0, before a single tick of the match had been simulated: one peer's
    // `Round.ticks` read 467 and the other's 149 — exactly the five seconds one
    // spent waiting for the other. Every match therefore began desynced, and
    // reported it at frame 10, the first checksum comparison.
    //
    // It belongs here rather than in the client because the clock is a fact
    // about the world, and this is the one place a world is made. A restore
    // (`save::restore`) does not call this and installs the round it was given,
    // which is the whole point of a resume.
    commands.insert_resource(Round::default());
    if matches!(scenario, Scenario::Gunfire) {
        spawn_gunfire_demo(commands);
    } else {
        for handle in 0..num_players.min(MAX_PLAYERS) {
            // Alternating, so two players are 1v1 rather than 2v0 — see
            // [`default_side`]. The slot follows from it, so the first pair take
            // the southernmost post at each end and the sides fill northward
            // together.
            spawn_pawn(commands, handle, Team(default_side(handle)), handle / TEAM_COUNT);
        }
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
        // The rig poses its own pawns, so it overwrites everything the muster
        // line would have given them. It gets away with that because the round
        // clock does not run here at all (`run_round` returns on any scenario
        // but the arena) — nothing will come along later and re-post them.
        let pawn = spawn_pawn(commands, handle, Team(default_side(handle)), 0);
        commands.entity(pawn).insert((
            Pos::from_units(x, 0),
            Facing { x: toward, y: 0 },
            Stance { level, change: 0 },
        ));
    }
}

/// The gunfire demo ([`Scenario::Gunfire`]): a pawn to walk around with, and one
/// standing in the middle of the arena pulling its trigger once a second.
///
/// Two things about the second pawn. It stands in the CENTRE, so there is a
/// full field of terrain in every direction to walk it from — the muster lines
/// would have put it against a wall with half the approaches off the map. And
/// it points NORTH rather than at you, because a demo that shoots the person
/// looking at it is a demo that ends: nothing drives this pawn but the idle
/// branch of `client/src/input.rs`, and a blank aim leaves [`Facing`] exactly
/// where it was, so the bearing it spawns on is the one it keeps.
///
/// The round clock does not run here (`run_round` returns on any scenario but
/// the arena), which is what stops the pair being re-posted to the muster lines
/// two minutes in.
fn spawn_gunfire_demo(commands: &mut Commands) {
    for (handle, x, y, toward) in [
        (0, -DEMO_STANDOFF, 0, (127, 0)),
        (1, 0, 0, (0, 127)),
    ] {
        let pawn = spawn_pawn(commands, handle, Team(default_side(handle)), 0);
        commands
            .entity(pawn)
            .insert((Pos::from_units(x, y), Facing { x: toward.0, y: toward.1 }));
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
            // The clock, the phase and the series score. A RESOURCE rather than
            // a component because there is exactly one of it and every system
            // that cares wants it without a query — and rollback-registered for
            // the same reason `Health` is: two peers disagreeing about whether
            // the round is still live disagree about everything after it.
            .init_resource::<Round>()
            .rollback_resource_with_copy::<Round>()
            .checksum_resource_with_hash::<Round>()
            .rollback_component_with_copy::<Pos>()
            .rollback_component_with_copy::<Player>()
            .rollback_component_with_copy::<Team>()
            .rollback_component_with_copy::<Intent>()
            .rollback_component_with_copy::<Bot>()
            .rollback_component_with_copy::<Facing>()
            .rollback_component_with_copy::<Cooldown>()
            .rollback_component_with_copy::<Aim>()
            .rollback_component_with_copy::<Stance>()
            .rollback_component_with_copy::<Health>()
            .rollback_component_with_copy::<Deaths>()
            .rollback_component_with_copy::<Kills>()
            .rollback_component_with_copy::<Bullet>()
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
            // And the aim, for the same argument one step further on: a
            // disagreement about how steady a shooter is is a disagreement
            // about where its rounds went, and that only reaches `Pos` when
            // somebody dies of it.
            .checksum_component_with_hash::<Aim>()
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
                    // Between the two: the bot set is settled by now, and
                    // `bot_think` should decide on this tick's dial rather than
                    // last tick's.
                    apply_bot_dials::<C>,
                    read_human_intent::<C>,
                    bot_think,
                    // Between rounds nobody walks and nobody shoots: the result
                    // is already on the banner and a last-second charge into an
                    // enemy spawn would decide nothing while looking as though
                    // it might. Rounds already in the air are deliberately NOT
                    // frozen (below) — a shot that was fired in time still
                    // arrives.
                    (
                        move_players,
                        // After movement and before anything reads a position:
                        // rounds leave the barrel next, and a shot fired from
                        // inside someone else can't hit them.
                        separate_players,
                        // Between the two: this tick's posture is settled, and
                        // the round about to leave the barrel is charged for
                        // it rather than for last tick's.
                        settle_aim,
                        fire_bullets,
                    )
                        .chain()
                        .run_if(round_is_live),
                    move_bullets,
                    resolve_hits,
                    tick_health,
                    // Last, so it judges the tick as it finally stands, and so
                    // the pawns it re-posts at a round start are read by next
                    // tick's intent systems rather than by this tick's.
                    round::run_round::<C>,
                )
                    .chain(),
            );
    }
}

// ── Fixed-tick systems (run inside the GGRS rollback schedule) ──────────────

/// Run condition for everything that only happens while a round is being
/// fought. Reads a rollback-registered resource, so a re-simulated tick asks the
/// question against the same answer.
fn round_is_live(round: Res<Round>) -> bool {
    round.live()
}

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
/// A match-wide setting, read from the LOWEST HANDLE THAT IS ACTUALLY ASKING.
///
/// The bot count and the bot aggression are one setting for the whole match, so
/// exactly one player's copy can be honoured or two menus fight over it. It used
/// to be handle 0's, flatly — which stopped working the moment a seat could be
/// empty. A player who walks away sends a blank input forever (GGRS substitutes
/// `default()` for a disconnected player, and `net.rs` does the same for a
/// vacated seat), so "handle 0's copy" became "nobody's copy" and the dials
/// froze for everyone else until that one person came back.
///
/// Scanning for the first handle that is asking fixes that without giving
/// anything up: it is still one player's copy, still chosen by a rule every peer
/// computes identically from the input stream alone, and still impossible to
/// fight over — the lower handle simply wins. The client half is
/// `MatchRoom::dial_holder`, which is what stops two clients sending at once.
fn dialled<C: Config<Input = PlayerInput>, T>(
    inputs: &PlayerInputs<C>,
    ask: impl Fn(&PlayerInput) -> Option<T>,
) -> Option<T> {
    (0..MAX_PLAYERS)
        .filter_map(|handle| inputs.get(handle))
        .find_map(|&(input, _status)| ask(&input))
}

fn reconcile_bots<C: Config<Input = PlayerInput>>(
    mut commands: Commands,
    inputs: Res<PlayerInputs<C>>,
    roster: Res<BotRoster>,
    pawns: Query<(Entity, &Player, &Team, Option<&Bot>)>,
) {
    let Some(asked) = dialled(&inputs, |input| input.bots()) else { return };

    let mut taken = [false; MAX_PLAYERS];
    let mut on_side = [0usize; TEAM_COUNT];
    let (mut humans, mut bots) = (0usize, Vec::new());
    for (entity, player, team, is_bot) in &pawns {
        if player.handle < MAX_PLAYERS {
            taken[player.handle] = true;
        }
        on_side[team.index()] += 1;
        match is_bot {
            Some(_) => bots.push((player.handle, entity)),
            None => humans += 1,
        }
    }

    // Bots fill the seats the humans aren't using; asking for more than that is
    // asking for a spawn point that doesn't exist.
    let wanted = (asked as usize).min(MAX_PLAYERS.saturating_sub(humans));
    if bots.len() == wanted {
        return;
    }

    if bots.len() < wanted {
        let Some(handle) = (0..MAX_PLAYERS).find(|&h| !taken[h]) else { return };
        // The handle's own side by default, and the other one when that side is
        // already at strength — so a bot arriving mid-round joins whichever end
        // is short rather than making a 5v3. Both peers compute this from the
        // same pawn set, so both put it in the same place.
        let mut side = default_side(handle) as usize;
        if on_side[side] >= TEAM_SIZE {
            side = TEAM_COUNT - 1 - side;
        }
        let pawn = spawn_pawn(&mut commands, handle, Team(side as u8), on_side[side]);
        commands.entity(pawn).insert((
            Bot::seeded(handle, roster.profile(handle), roster.salt),
            Aim::seeded(handle, roster.salt),
        ));
    } else {
        bots.sort_unstable();
        if let Some(&(_, entity)) = bots.last() {
            commands.entity(entity).despawn();
        }
    }
}

/// Push the first player's match dials onto every bot, every tick.
///
/// **Every tick, not at spawn**, and that is the point: the dial can be turned
/// mid-match and the bots already standing there change their minds, instead of
/// the setting only reaching whoever is spawned next. It is safe to do that
/// only because the input carries an ABSOLUTE level — writing the same value
/// onto the same bot twice is writing it once, so a replayed tick lands on the
/// identical world however many times GGRS runs it. A "+1 aggression" edge, or
/// a resource the menu mutated, would not survive the first rollback.
///
/// A zero dial means the sender isn't asking, and the bots keep whatever
/// [`BotRoster`] gave them. That is what lets the self-play harness put two
/// different profiles in one arena while still driving handle 0's input.
fn apply_bot_dials<C: Config<Input = PlayerInput>>(
    inputs: Res<PlayerInputs<C>>,
    mut bots: Query<&mut Bot>,
) {
    let Some(aggression) = dialled(&inputs, |input| input.aggression()) else { return };
    for mut bot in &mut bots {
        // Guarded so an unchanged dial doesn't mark every bot dirty each tick.
        if bot.profile.aggression != aggression {
            bot.profile.aggression = aggression;
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
/// How far this pawn actually travels this tick, per axis in subunits.
///
/// **Extracted so the aim model and the movement share one answer.** How fast a
/// shooter is going is most of what decides its cone ([`MOVE_SWAY`]), and
/// `settle_aim` runs in the same tick as `move_players` off the same [`Intent`]
/// — two copies of this arithmetic that drifted apart would charge a pawn for a
/// sprint it never took, or let it shoot as though it were standing still while
/// crossing the field. There is nothing to keep them honest but this being the
/// only copy.
///
/// A stance change roots you for as long as it takes — that one still comes out
/// `(0, 0)`, and it rides in the input stream so every peer agrees. The SIGHTS
/// no longer do: they scale the stance's pace by [`ADS_SPEED`] instead of
/// cancelling it, so a shooter can walk their weapon onto a target rather than
/// having to choose between moving and aiming.
///
/// The joystick vector is scaled to at most that speed, preserving direction:
/// `v = m * SPEED / max(len, 127)`. Dividing by the
/// *longer* of `len`/127 keeps sub-max deflections proportional while capping
/// diagonals at full speed.
pub fn step(input: &PlayerInput, stance: &Stance) -> (i32, i32) {
    let (mx, my) = (input.move_x as i32, input.move_y as i32);
    if (mx == 0 && my == 0) || stance.change > 0 {
        return (0, 0);
    }
    let mut speed = stance.speed();
    if input.ads() {
        speed = speed * ADS_SPEED / FP;
    }
    speed = speed * heading_scale(input) / FP;
    let len = isqrt((mx * mx + my * my) as i64).max(127) as i32;
    (mx * speed / len, my * speed / len)
}

/// How much of its pace a pawn keeps, given the angle between where it is
/// WALKING and where it is POINTING: `FP` straight ahead, [`STRAFE_SPEED`]
/// square on, [`BACKPEDAL_SPEED`] straight back, linear in the cosine between.
///
/// **It is measured against the input's own aim, not against the [`Facing`]
/// component**, and that is worth a sentence. `step` and `step_speed` are
/// deliberately a pure function of `(input, stance)` so that the movement and
/// the aim model cannot disagree about how fast a pawn is going (see `step`);
/// reaching for a component here would have put a third caller — `settle_aim` —
/// in the position of needing a `Facing` it does not query, and would have made
/// the answer depend on system order. `PlayerInput::aim` is the same direction
/// `move_players` is about to write into `Facing` anyway.
///
/// **Anything that steers by walking is charged nothing**, exactly: for a bot,
/// the keyboard or a test, `aim()` falls back to the move vector, so the cosine
/// is the vector with itself and comes out at precisely `FP` with no rounding
/// slack. Only a player with a second stick can ever be walking one way and
/// pointing another, and only they pay.
pub fn heading_scale(input: &PlayerInput) -> i32 {
    let (mx, my) = (input.move_x as i64, input.move_y as i64);
    let (ax, ay) = input.aim();
    let (ax, ay) = (ax as i64, ay as i64);
    let (walk, point) = (isqrt(mx * mx + my * my), isqrt(ax * ax + ay * ay));
    if walk == 0 || point == 0 {
        return FP;
    }
    let fp = FP as i64;
    let cos = ((mx * ax + my * ay) * fp / (walk * point)).clamp(-fp, fp);
    let strafe = STRAFE_SPEED as i64;
    // Two straight segments meeting at square-on, so it inverts and reasons in
    // closed form and there is no curve to argue about.
    let scale = if cos >= 0 {
        strafe + (fp - strafe) * cos / fp
    } else {
        strafe + (strafe - BACKPEDAL_SPEED as i64) * cos / fp
    };
    scale as i32
}

/// The magnitude of that step — what [`Aim::settled`] is charged for.
pub fn step_speed(input: &PlayerInput, stance: &Stance) -> i32 {
    let (sx, sy) = step(input, stance);
    isqrt((sx as i64 * sx as i64) + (sy as i64 * sy as i64)) as i32
}

/// Everything moving a pawn touches: where it is, where it faces (which charges
/// [`Aim::turn`] — the one place old and new facing are both in hand), and the
/// stance gating its speed.
type Mover = (
    &'static Intent,
    &'static mut Pos,
    &'static mut Facing,
    &'static mut Stance,
    &'static mut Aim,
    &'static Health,
);

fn move_players(
    mut players: Query<Mover, With<Player>>,
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

    for (intent, mut pos, mut facing, mut stance, mut aim, health) in &mut players {
        // The dead hold still: no drifting, no turning, no getting up out of
        // prone. Nothing puts them back until the round does.
        if !health.alive() {
            continue;
        }
        let input = intent.0;
        // Stance first: the requested level rides in the input bits, so every
        // peer starts (and finishes) the same transition on the same tick.
        // Nobody asking — a disconnected player, whose input GGRS blanks — holds
        // the stance they had rather than climbing to their feet unbidden.
        let wanted = input.stance().unwrap_or(stance.level);
        stance.advance(wanted);
        // Turning is its own question now that the aim stick is its own stick:
        // a pawn can traverse the barrel standing perfectly still, and one
        // walking with the aim stick untouched still points where it is going
        // (`PlayerInput::aim` falls back to the move vector for exactly that).
        let (ax, ay) = input.aim();
        if ax != 0 || ay != 0 {
            // The turn is charged HERE, the one place old and new facing are
            // both in hand — `settle_aim` never has to reconstruct what the
            // barrel did.
            aim.turn((facing.x, facing.y), (ax, ay));
            facing.x = ax;
            facing.y = ay;
        }
        let (sx, sy) = step(&input, &stance);
        if sx == 0 && sy == 0 {
            continue;
        }
        pos.x += sx;
        pos.y += sy;
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
/// place (with the sights bit, in those days), so two of them that closed to
/// nothing were pinned there permanently —
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

/// One tick of the aim model: bleed the recoil, and ease the sway toward
/// whatever this posture settles to.
///
/// Runs between the movement and the firing, inside the round gate, so a round
/// leaving the barrel this tick is charged for what the shooter is doing this
/// tick rather than last one. Between rounds it does not run at all and the
/// sway simply holds — there is nothing to shoot at and `run_round` wipes it
/// clean on the way into the next one.
///
/// The dead are left alone rather than reset: a pawn that is out is not holding
/// a weapon, and zeroing it here would hand whoever restores the round a
/// steadiness they hadn't earned.
fn settle_aim(mut pawns: Query<(&Intent, &Stance, &Health, &mut Aim)>) {
    for (intent, stance, health, mut aim) in &mut pawns {
        aim.cool();
        if !health.alive() {
            continue;
        }
        let speed = step_speed(&intent.0, stance);
        aim.disturb(speed);
        aim.ease(Aim::settled(stance, speed, intent.0.ads()));
    }
}

/// Everything firing a round needs to know about the pawn firing it.
type Shooter = (
    &'static Player,
    &'static Intent,
    &'static Pos,
    &'static Facing,
    &'static Stance,
    &'static mut Cooldown,
    &'static mut Aim,
    &'static Health,
);

fn fire_bullets(mut commands: Commands, mut players: Query<Shooter>) {
    for (player, intent, pos, facing, stance, mut cooldown, mut aim, health) in &mut players {
        if cooldown.0 > 0 {
            cooldown.0 -= 1;
        }
        let input = intent.0;
        if !input.fire() || cooldown.0 > 0 || !health.alive() {
            continue;
        }
        cooldown.0 = FIRE_COOLDOWN;
        // Where the round actually goes: the facing, turned by a draw from the
        // cone. Rotating by a TANGENT rather than an angle is what keeps this in
        // integers — adding `t` times the perpendicular to a vector turns it by
        // `atan(t)`, which is exactly the quantity `SPREAD_MAX` is expressed in,
        // and costs two multiplies instead of a trig table.
        //
        // Scaled up by `FP` first so the deviation survives the division:
        // `Facing` is raw joystick range (±127), and a few percent of that
        // rounds to nothing.
        let dev = aim.deviate() as i64;
        let (fx, fy) = (facing.x as i64 * FP as i64, facing.y as i64 * FP as i64);
        let (ax, ay) = (fx - fy * dev / FP as i64, fy + fx * dev / FP as i64);
        let len = isqrt(ax * ax + ay * ay).max(1);
        let vx = (ax * BULLET_SPEED as i64 / len) as i32;
        let vy = (ay * BULLET_SPEED as i64 / len) as i32;
        aim.kick(stance);
        aim.flash();
        // Spawn just outside the player's own radius so the bullet never
        // overlaps its shooter. Down the round's own line, not the facing —
        // a round that left the barrel sideways would otherwise start beside
        // the shooter rather than in front of it.
        let offset = (PLAYER_R + BULLET_R + 2) as i64 * FP as i64;
        let start = Pos {
            x: pos.x + (ax * offset / len) as i32,
            y: pos.y + (ay * offset / len) as i32,
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
        for (rock, pos) in &rocks {
            if let Some(entry) = sweep.entry(*pos, rock.r + BULLET_R) {
                hits.push((entry, pos.x, pos.y, 0, Impact::Rock));
            }
        }
        hits.sort_unstable_by_key(|&(entry, x, y, tie, _)| (entry, x, y, tie));
        let Some(&(.., impact)) = hits.first() else { continue };

        match impact {
            Impact::Rock => {}
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
                        // Out for the rest of the round: 1 is "just now", and
                        // `tick_health` counts it up from there.
                        health.down = 1;
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

/// Age the two render-feedback counters. Nothing here brings anyone back:
/// [`round::run_round`] is the only thing that ever clears `down`, because the
/// only way back into a Ghost War match is the next round.
fn tick_health(mut players: Query<&mut Health>) {
    for mut health in &mut players {
        if health.hurt > 0 {
            health.hurt -= 1;
        }
        if health.down > 0 {
            // Saturating rather than wrapping: eighteen minutes down is not a
            // state anyone should reach, and coming back to life on the tick
            // that a u16 rolls over is not the way to find out.
            health.down = health.down.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The aggression dial spans the whole range, and its ends are the ends —
    /// the lowest position must be genuinely 0, not "nearly 0", or the menu
    /// can't ask for the setting the self-play harness says is best.
    #[test]
    fn the_aggression_dial_covers_zero_to_one() {
        let mut input = PlayerInput::default();
        input.set_aggression(1);
        assert_eq!(input.aggression(), Some(0));
        input.set_aggression(AGGRO_LEVELS);
        assert_eq!(input.aggression(), Some(FP));
        input.set_aggression(AGGRO_LEVELS / 2 + 1);
        assert_eq!(input.aggression(), Some(FP / 2));
        // Monotone, so a step of the dial is always a step of the value.
        let mut last = -1;
        for level in 1..=AGGRO_LEVELS {
            input.set_aggression(level);
            let value = input.aggression().expect("a set dial reads back");
            assert!(value > last, "dial position {level} didn't raise the value");
            last = value;
        }
    }

    /// Zero means "not asking", which is the whole reason this could be added
    /// to the input byte without disturbing the harness: it drives handle 0's
    /// input to set the bot count, and must not thereby flatten the two sides'
    /// aggression to one value and silently invalidate every measurement.
    #[test]
    fn an_unset_aggression_dial_leaves_profiles_alone() {
        assert_eq!(PlayerInput::default().aggression(), None);
        // And it doesn't collide with the bits next door.
        let mut input = PlayerInput::default();
        input.set_bots(5);
        input.set_stance(STANCE_PRONE);
        input.buttons |= BTN_FIRE;
        assert_eq!(input.aggression(), None, "another field leaked into the dial");
        input.set_aggression(4);
        assert_eq!(input.bots(), Some(5), "the dial disturbed the bot count");
        assert_eq!(input.stance(), Some(STANCE_PRONE), "the dial disturbed the stance");
        assert!(input.fire(), "the dial disturbed the trigger");
    }

    /// A peer sending garbage in the spare bits can't index anything out of
    /// range or ask for an aggression outside `0..=FP`.
    #[test]
    fn a_hostile_dial_byte_stays_in_range() {
        for raw in 0..=u8::MAX {
            let input = PlayerInput { dials: raw, ..default() };
            if let Some(value) = input.aggression() {
                assert!((0..=FP).contains(&value), "dials {raw} gave aggression {value}");
            }
        }
    }

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

    /// Holding the trigger opens the cone, and how fast is what the stance buys.
    #[test]
    fn recoil_settles_at_a_different_rate_in_each_stance() {
        // Hold the trigger and count how many rounds it takes to saturate.
        let rounds_to_spray = |level: u8| {
            let stance = Stance { level, change: 0 };
            let mut aim = Aim::seeded(0, 0);
            aim.rest();
            for shot in 1..=400 {
                aim.kick(&stance);
                if aim.bloom >= FP {
                    return Some(shot);
                }
                for _ in 0..FIRE_COOLDOWN {
                    aim.cool();
                }
            }
            None
        };
        let counts: Vec<Option<i32>> =
            (0..STANCE_COUNT as u8).map(rounds_to_spray).collect();
        println!("rounds of held fire before the cone is wide open: {counts:?}");

        // **The decay must actually be worth something.** `FP` is 256, so a
        // per-tick decay written as a fraction of it truncates hard — `FP / 150`
        // rounds to 1 instead of 1.7, and that alone made every stance saturate
        // at the same rate. This is the assertion that would have caught it.
        const { assert!(RECOIL_DECAY >= 2, "RECOIL_DECAY truncated; see its doc comment") };
        let (stand, crouch, prone) = (counts[0], counts[1], counts[2]);
        assert!(
            stand.is_some_and(|n| n <= 15),
            "standing fire must run away quickly, took {stand:?} rounds"
        );
        assert!(
            crouch.unwrap_or(i32::MAX) > stand.unwrap() * 2,
            "crouching must buy at least twice the burst standing does: {crouch:?} vs {stand:?}"
        );
        assert!(
            prone.unwrap_or(i32::MAX) > crouch.unwrap_or(0) * 2,
            "prone must buy at least twice the burst crouching does: {prone:?} vs {crouch:?}"
        );
        // And every stance must eventually pay: a posture that can hold the
        // trigger forever with no cost is a posture with no reason to stop.
        assert!(prone.is_some(), "prone fire never blooms at all");
    }

    /// **Running through grass gives you away, and stopping does not instantly
    /// un-give you away.** Same eye, same target, same tile of deep grass — the
    /// only thing that changes is what the target has been doing.
    #[test]
    fn running_in_grass_gives_you_away() {
        // Deep enough to bury a standing pawn completely if it holds still.
        let strip = Scenario::GrassStrip { depth: 70, east_stance: STANCE_STAND };
        let (eye, target) = (Pos::from_units(-STRIP_STANDOFF, 0), Pos::from_units(0, 0));
        let seen_at = |stir: i32| {
            FP - grass_conceal(&strip, eye, STANCE_STAND, target, STANCE_STAND, stir)
        };

        let still = seen_at(0);
        let crawling = seen_at(FP * STANCE_SPEED[STANCE_PRONE as usize] / PLAYER_SPEED);
        let sprinting = seen_at(FP);
        println!(
            "seen through deep grass: still {still}, crawling {crawling}, sprinting {sprinting} (of {FP})"
        );

        assert!(
            sprinting >= still + FP / 2,
            "a sprint through deep grass must forfeit most of the concealment: \
             still {still} vs sprinting {sprinting}"
        );
        // The ladder: stance caps speed, so the stances price themselves.
        assert!(
            crawling < still + FP / 12,
            "a prone crawl should stay nearly as hidden as holding still: \
             {still} -> {crawling}"
        );
        // And it decays rather than snapping back: a tick after stopping, a
        // sprinter is still almost as visible as mid-sprint.
        let mut aim = Aim::seeded(0, 0);
        aim.disturb(PLAYER_SPEED);
        aim.cool();
        assert!(
            aim.stir > FP * 9 / 10,
            "one tick after a sprint the grass has barely settled, stir {}",
            aim.stir
        );
        // …and after `FP / STIR_DECAY` ticks (a second and change) it is
        // genuinely gone.
        for _ in 0..2 * TICK_HZ {
            aim.cool();
        }
        assert_eq!(aim.stir, 0, "holding still settles the grass");
    }

    /// **A swung barrel is not on target, and honing back in takes time.** The
    /// traverse charge is also what makes a CLOSE mover hard to hit: angular
    /// rate is speed over range, so the same crossing walk that saturates the
    /// cone at 40 units tracks for free at 150.
    #[test]
    fn swinging_the_aim_costs_accuracy_until_it_is_honed_back_in() {
        // A flick: 30 degrees in one tick. Facing vectors on the i8 joystick
        // scale, like the real ones.
        let mut aim = Aim::seeded(0, 0);
        aim.rest();
        let settled = aim.spread();
        aim.turn((127, 0), (110, 63)); // ~30 deg
        assert_eq!(aim.spread(), FP, "a 30-degree flick opens the cone wide");
        // Honing: decays while the barrel holds still, back to the posture's
        // settled spread inside a second.
        for _ in 0..TICK_HZ {
            aim.cool();
        }
        assert_eq!(aim.spread(), settled, "a second of holding hones it back in");

        // Tracking: the angular rate of one walking target, seen from two
        // ranges — `sin per tick ≈ speed / range`, so the SAME walk is a fast
        // traverse up close and a crawl of the barrel at distance. Measured at
        // the PEAK, because that is the gameplay: the cone is open exactly
        // while the runner crosses your muzzle, which is exactly when you want
        // to shoot them, and it closes again once they are past and receding.
        let track = |range: i32| {
            let mut aim = Aim::seeded(0, 0);
            aim.rest();
            let step = PLAYER_SPEED / FP; // units per tick of a full run
            let mut peak = 0;
            for tick in 0..60 {
                // The facing to a target crossing at `step` per tick.
                let (x0, x1) = (step * tick, step * (tick + 1));
                aim.turn((127, x0 * 127 / range), (127, x1 * 127 / range));
                aim.cool();
                peak = peak.max(aim.swing);
            }
            peak
        };
        let (close, far) = (track(40), track(150));
        println!("peak swing tracking a crossing runner: at 40 units {close}, at 150 {far}");
        assert!(
            close >= FP / 2,
            "tracking a runner crossing at 40 units must cost real accuracy, got {close}"
        );
        assert_eq!(far, 0, "the same runner at 150 units tracks inside the dead zone");
    }

    /// **The shape of the cone: denser in the middle, live to a hard edge.**
    /// Histogram the actual draw and check both halves of that claim.
    #[test]
    fn the_cone_leans_toward_the_middle_and_stops_dead_at_its_edge() {
        const BINS: usize = 8;
        let mut aim = Aim::seeded(11, 0);
        aim.sway = FP; // widest cone
        let span = SPREAD_MAX;
        let mut bins = [0usize; BINS];
        let (mut n, mut worst) = (0usize, 0i32);
        for _ in 0..200_000 {
            let d = aim.deviate();
            worst = worst.max(d.abs());
            let bin = (d.abs() as i64 * BINS as i64 / (span as i64 + 1)) as usize;
            bins[bin.min(BINS - 1)] += 1;
            n += 1;
        }
        println!("|deviation| histogram over {n} rounds, {BINS} bins out to the rim:");
        for (i, count) in bins.iter().enumerate() {
            println!("  bin {i}: {:5.2}%", *count as f64 * 100.0 / n as f64);
        }

        // Sharp cutoff: nothing at all past the rim.
        assert!(worst <= span, "a round left the cone: {worst} > {span}");
        // Live to the edge: the outermost bin is not a rounding artefact. A
        // bell-shaped draw would empty this out, which is the failure mode the
        // flat-topped shape exists to avoid.
        assert!(
            bins[BINS - 1] * 40 > n,
            "the rim of the cone is nearly empty ({} of {n}) — the draw has gone \
             bell-shaped and a wide cone no longer costs what it says it does",
            bins[BINS - 1]
        );
        // Denser in the middle: the inner half outnumbers the outer half, by
        // about the 5:3 `SPREAD_CORE` implies, and not by more.
        let inner: usize = bins[..BINS / 2].iter().sum();
        let outer: usize = bins[BINS / 2..].iter().sum();
        println!("inner half {inner}, outer half {outer}, ratio {:.2}", inner as f64 / outer as f64);
        // Denser in the middle, by about the 17:15 `SPREAD_CORE` implies — and
        // deliberately not by much more. The lean is a SUBSIDY to whoever has
        // the widest cone (see `SPREAD_CORE`), so every point of it has to be
        // paid for in `SPREAD_MAX`.
        assert!(inner * 100 > outer * 108, "the middle is not denser: {inner} vs {outer}");
        assert!(inner * 2 < outer * 3, "the middle is TOO dense: {inner} vs {outer}");
    }

    /// **`shot_quality` must be the exact inverse of the draw**, because the
    /// bots' trigger discipline and their whole judgement about closing the
    /// distance are built on it. A mismatch fails silently in a match; this
    /// fails loudly here.
    #[test]
    fn shot_quality_matches_the_rounds_it_predicts() {
        for &range in &[30, 60, 100, 180, 260] {
            for &spread in &[FP / 4, FP / 2, FP * 3 / 4, FP] {
                let predicted = crate::bot::shot_quality(spread, range);
                // Fire a lot of rounds and see how many would pass within a
                // pawn's radius at that range.
                let mut aim = Aim::seeded(range as usize, spread as u32);
                aim.sway = spread;
                let (mut hits, mut n) = (0usize, 0usize);
                for _ in 0..40_000 {
                    // Lateral offset at `range`, in SUBUNITS: the deviation is a
                    // tangent in FP, so `dev * range` is already subunits. Whole
                    // world units would truncate away most of the answer near
                    // the target's own size — and the hit test the sim actually
                    // runs (`resolve_hits`) is in subunits too.
                    let off = aim.deviate() as i64 * range as i64;
                    if off.abs() <= PLAYER_R as i64 * FP as i64 {
                        hits += 1;
                    }
                    n += 1;
                }
                let actual = (hits as i64 * FP as i64 / n as i64) as i32;
                println!(
                    "range {range:3} spread {spread:3}: predicted {predicted:3}, actual {actual:3}"
                );
                assert!(
                    (predicted - actual).abs() <= FP / 20,
                    "shot_quality says {predicted} but {actual} of {n} rounds landed \
                     (range {range}, spread {spread}) — the predictor and the draw \
                     have come apart"
                );
            }
        }
    }

    /// A pawn that has just spawned, or just been picked back up by a round
    /// start, holds its weapon as steadily as standing still — no better. Zero
    /// here handed everyone on the field one free unmissable shot a round.
    #[test]
    fn nobody_starts_a_round_steadier_than_standing() {
        let standing = Aim::settled(&Stance::default(), 0, false);
        assert_eq!(Aim::seeded(3, 0).sway, standing);
        let mut aim = Aim::seeded(3, 0);
        aim.sway = 0;
        aim.bloom = FP;
        aim.rest();
        assert_eq!(aim.sway, standing);
        assert_eq!(aim.bloom, 0);
    }

    /// **A shooter walking with the sights up is steadier than one walking
    /// without them and shakier than one standing still with them.**
    ///
    /// Both halves matter and they used to be unrepresentable: [`Aim::settled`]
    /// returned early on `ads`, skipping the movement term entirely, which was
    /// exactly right while [`BTN_ADS`] rooted the pawn and speed was therefore
    /// always zero. The moment the sights merely SLOWED you, that early return
    /// became a free perfect shot on the move — aim while walking and be as
    /// steady as a planted shooter. This is the assertion that would have caught
    /// it, and it is written as an ordering rather than as numbers so it goes on
    /// meaning the same thing when the constants are tuned.
    #[test]
    fn walking_with_the_sights_up_is_steadier_than_walking_without_them() {
        let stand = Stance::default();
        let walk = PLAYER_SPEED * ADS_SPEED / FP;

        let planted = Aim::settled(&stand, 0, true);
        let aimed_walk = Aim::settled(&stand, walk, true);
        let hip_walk = Aim::settled(&stand, walk, false);
        println!("planted {planted}, walking aimed {aimed_walk}, walking hip {hip_walk} of {FP}");

        assert!(
            aimed_walk > planted,
            "walking with the sights up ({aimed_walk}) cost nothing over standing still \
             with them ({planted}) — the movement term is being skipped again"
        );
        assert!(
            aimed_walk < hip_walk,
            "the sights ({aimed_walk}) bought nothing over hip fire ({hip_walk}) at the same pace"
        );
        // And the sights are still worth more than the movement costs, or there
        // would be no reason to raise them while closing.
        assert!(
            aimed_walk < Aim::settled(&stand, 0, false),
            "walking with the sights up ({aimed_walk}) is worse than just standing there \
             ({}) — nobody would ever advance aiming",
            Aim::settled(&stand, 0, false)
        );
    }

    /// **Walking off your own facing costs speed, and steering by walking costs
    /// nothing** — the second half being what keeps every bot, keyboard and test
    /// in the repo moving at exactly the pace it always did.
    #[test]
    fn walking_sideways_and_backwards_costs_speed() {
        let ahead =
            |ax: i8, ay: i8| PlayerInput { move_y: 127, aim_x: ax, aim_y: ay, ..default() };
        // Pointing exactly where you walk: full pace, and EXACTLY full pace —
        // a rounding slack here would quietly slow every bot in the game.
        assert_eq!(heading_scale(&ahead(0, 127)), FP);
        assert_eq!(heading_scale(&ahead(0, 40)), FP, "a shorter aim vector is the same bearing");
        assert_eq!(
            heading_scale(&PlayerInput { move_x: -90, move_y: 40, ..default() }),
            FP,
            "a producer with no aim stick at all steers by walking and must pay nothing"
        );

        // Square on and straight back are the two anchors the constants name.
        assert_eq!(heading_scale(&ahead(127, 0)), STRAFE_SPEED);
        assert_eq!(heading_scale(&ahead(-127, 0)), STRAFE_SPEED);
        assert_eq!(heading_scale(&ahead(0, -127)), BACKPEDAL_SPEED);

        // And the diagonals land between their neighbours, on both sides.
        let (fore, back) = (heading_scale(&ahead(90, 90)), heading_scale(&ahead(90, -90)));
        println!(
            "ahead {FP}, fore-diagonal {fore}, square {STRAFE_SPEED}, \
             back-diagonal {back}, back {BACKPEDAL_SPEED}"
        );
        assert!(STRAFE_SPEED < fore && fore < FP, "the forward diagonal is not between");
        assert!(BACKPEDAL_SPEED < back && back < STRAFE_SPEED, "the back diagonal is not between");
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
        assert_eq!(input.stance(), Some(STANCE_PRONE));
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
        assert_eq!(input.stance(), Some(STANCE_CROUCH));
        input.set_stance(STANCE_STAND);
        assert!(input.fire() && input.ads());
        assert_eq!(input.stance(), Some(STANCE_STAND));
    }

    /// **A blank input asks for nothing**, which is the one property the whole
    /// disconnected-player story rests on: GGRS substitutes `default()` for a
    /// player who has dropped, so every multi-bit field has to read zero as
    /// "not asking" rather than as its own zero value. Standing up and losing
    /// every bot are what the previous encoding did on a browser refresh.
    #[test]
    fn a_blank_input_asks_for_nothing() {
        let blank = PlayerInput::default();
        assert_eq!(blank.stance(), None, "an absent player would stand up");
        assert_eq!(blank.bots(), None, "an absent handle 0 would empty the arena");
        assert_eq!(blank.aggression(), None);
        assert_eq!(blank.team_request(), None);
        assert!(!blank.fire() && !blank.ads());
        // A blank aim is not "point at the origin", it is "wherever I'm
        // walking" — and a blank input isn't walking either, so an absent
        // player's barrel stays exactly where they left it pointing.
        assert_eq!(blank.aim(), (0, 0), "an absent player would slew their barrel");

        // …and every value a player can actually ask for still survives the
        // round trip, including the zeroes that used to be indistinguishable
        // from silence.
        for level in 0..=STANCE_PRONE {
            let mut input = PlayerInput::default();
            input.set_stance(level);
            assert_eq!(input.stance(), Some(level));
        }
        for count in 0..=MAX_PLAYERS as u8 {
            let mut input = PlayerInput::default();
            input.set_bots(count);
            assert_eq!(input.bots(), Some(count), "asking for {count} bots did not survive");
        }
    }

    /// The aim stick is a second axis, and everything that predates it steers
    /// the barrel by walking.
    ///
    /// That fallback is the entire compatibility story for splitting aim off
    /// movement: the bots, the keyboard, the self-play harness and every test in
    /// this repo write `move_*` and leave `aim_*` at zero, and all of them have
    /// to keep pointing exactly where they used to. Break it and nothing fails
    /// loudly — bots simply stop facing the way they walk.
    #[test]
    fn a_pawn_with_no_aim_stick_points_where_it_walks() {
        let walking = PlayerInput { move_x: -40, move_y: 90, ..default() };
        assert_eq!(walking.aim(), (-40, 90), "a bot stopped facing the way it walks");

        // With the stick in hand the two come apart, and the walk no longer has
        // any say in it — including when the walk is the harder-pressed of the
        // two, which is what a player backing out of a firefight is doing.
        let twin = PlayerInput { move_x: -127, move_y: 0, aim_x: 0, aim_y: 60, ..default() };
        assert_eq!(twin.aim(), (0, 60), "the walk overrode the aim stick");

        // Standing still and turning is a thing only the second stick can do.
        let turning = PlayerInput { aim_x: 127, aim_y: 0, ..default() };
        assert_eq!(turning.aim(), (127, 0));
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
            for (sx, sy) in spawn_points() {
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

    /// Where the sight-line tests sample the arena. The muster lines are no use
    /// for it — they hug the east and west walls, so a 300-unit line east from
    /// one of them leaves the map — so these are spread across the middle
    /// instead, all far enough from the east wall that the longest line still
    /// lands inside it. The documented lane, (-150, 0) looking east, is among
    /// them.
    const SIGHT_PROBES: [(i32, i32); 8] = [
        (-330, -195),
        (-330, 65),
        (-150, 0),
        (-150, -150),
        (-60, 150),
        (0, -150),
        (60, 195),
        (-300, 100),
    ];

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
                    for &(sx, sy) in SIGHT_PROBES.iter() {
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

        let clear = visible_fraction(&bare, eye, STANCE_STAND, target, STANCE_STAND, 0, &[]);
        assert_eq!(clear, FP, "nothing in the way, yet not fully visible");

        let between = [Occluder { pos: Pos::from_units(0, 0), r: 30 }];
        let blocked = visible_fraction(&bare, eye, STANCE_STAND, target, STANCE_STAND, 0, &between);
        assert_eq!(blocked, 0, "a 30u boulder dead between them did not block");

        // The same boulder, with the viewer inside it.
        let inside_it = visible_fraction(
            &bare,
            Pos::from_units(0, 0),
            STANCE_STAND,
            target,
            STANCE_STAND,
            0,
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
                    0,
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
    ///
    /// It samples both muster lines and the middle of the map, so it covers the
    /// two cases that mean different things: **across** the field, which is the
    /// one the mechanic is about, and **along** a muster line, where two pawns
    /// 130 units apart on the same side are as close as any two prone pawns ever
    /// start. The pairs it prints are the ones that got away with being seen.
    #[test]
    fn prone_pawns_are_hidden_across_the_whole_arena() {
        let arena = Scenario::Arena;
        let points: Vec<(i32, i32)> = spawn_points().chain(SIGHT_PROBES).collect();
        let (mut buried, mut total) = (0, 0);
        for &(sx, sy) in &points {
            for &(tx, ty) in &points {
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
                    0,
                    &[],
                );
                if prone * 100 / FP < 5 {
                    buried += 1;
                } else {
                    println!(
                        "  seen: ({sx},{sy}) -> ({tx},{ty}) at {:.3}",
                        prone as f32 / FP as f32
                    );
                }
            }
        }
        println!("prone-to-prone: {buried}/{total} pairs buried");
        // Not every last pair: the field has genuinely bare tiles in it (9% of
        // them, by `grass_field_has_a_mix_of_depths`), and a short line that
        // happens to run down one is meant to be a place where lying flat does
        // not save you. What must not happen is that becoming common — at which
        // point going prone stops being a way to break contact at all.
        assert!(
            buried * 100 / total >= 97,
            "prone-to-prone should be hidden almost everywhere, only {buried}/{total} were"
        );
    }
}
