//! Rounds: the shape a Ghost War match takes.
//!
//! Two sides muster at opposite ends of the field and fight for two minutes.
//! **Nobody respawns.** A round ends the moment one side has nobody left
//! standing, or when the clock runs out — and then the side with more people
//! still on their feet takes it. Level pegging is a draw. Six seconds later the
//! next one starts, everybody up, everybody back on their line.
//!
//! That is the whole design, and every rule in it exists to make being alive
//! worth something. The old arena respawned you a second and a half after you
//! died, which made a death cost nothing but a walk; here it costs you the
//! round, and it costs your side a gun.
//!
//! # Why the state is a rollback resource
//!
//! There is exactly one clock and one scoreboard, so a resource is the honest
//! shape for it — but it must be a **rollback-registered** one. GGRS re-runs
//! ticks from restored snapshots, and a clock that carried on regardless would
//! read differently on the re-run, which is a desync. Registering it means the
//! phase, the tick count and the series score all rewind with the world; the
//! `checksum_resource_with_hash` next to the registration is what turns a
//! disagreement about the round into an immediate desync report rather than two
//! peers quietly running different matches.
//!
//! # Why team assignment happens here and not in the lobby
//!
//! [`balance`] runs at the top of every round, out of the same input stream
//! everything else travels on. A lobby message would have to be agreed
//! separately, would arrive at a tick nobody could name, and would give two
//! peers different worlds if one of them missed it. Reading an absolute request
//! out of each player's input instead means the assignment is a pure function of
//! state every peer already has.

use std::cmp::Ordering;

use bevy::prelude::*;
use bevy_ggrs::ggrs::Config;
use bevy_ggrs::PlayerInputs;

use crate::{
    spawn_facing, spawn_post, Aim, Bot, Bullet, Cooldown, Health, Player, PlayerInput, Pos,
    Scenario, Stance, Team, MAX_PLAYERS, TEAM_COUNT, TEAM_SIZE, TICK_HZ,
};

/// How long a round runs before the clock decides it.
pub const ROUND_SECONDS: usize = 120;
pub const ROUND_TICKS: u32 = (ROUND_SECONDS * TICK_HZ) as u32;
/// The gap between rounds: long enough to read the banner and see who is on
/// which side next, short enough that nobody puts the phone down.
pub const INTERMISSION_TICKS: u32 = (6 * TICK_HZ) as u32;

/// Who took a round.
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub enum Winner {
    Team(u8),
    /// Both sides wiped out on the same tick, or the clock ran out with the same
    /// number standing on each side.
    Draw,
}

#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub enum Phase {
    /// Both sides in the field and the clock running.
    Live,
    /// Decided. The banner is up and the next round is counting in.
    Over(Winner),
}

/// The clock, the phase and the series score.
#[derive(Resource, Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub struct Round {
    /// 1-based, and it only ever goes up — this is a series, not a best-of.
    pub number: u32,
    pub phase: Phase,
    /// Ticks spent in the CURRENT phase, so it counts the round up while live
    /// and the intermission up while over.
    pub ticks: u32,
    /// Rounds taken by each side. Draws are in neither.
    pub wins: [u32; TEAM_COUNT],
}

impl Default for Round {
    fn default() -> Self {
        Self { number: 1, phase: Phase::Live, ticks: 0, wins: [0; TEAM_COUNT] }
    }
}

impl Round {
    pub fn live(&self) -> bool {
        matches!(self.phase, Phase::Live)
    }

    /// Ticks left on the round clock; 0 once it has run out or the round is
    /// already decided.
    pub fn remaining(&self) -> u32 {
        match self.phase {
            Phase::Live => ROUND_TICKS.saturating_sub(self.ticks),
            Phase::Over(_) => 0,
        }
    }

    /// Ticks until the next round starts, while one is counting in.
    pub fn intermission_left(&self) -> u32 {
        match self.phase {
            Phase::Live => 0,
            Phase::Over(_) => INTERMISSION_TICKS.saturating_sub(self.ticks),
        }
    }

    /// How many rounds have been settled — the one on the banner counts, since
    /// its result is already in [`Round::wins`]. What the self-play harness
    /// stops on.
    pub fn decided(&self) -> u32 {
        self.number - 1 + u32::from(!self.live())
    }

    /// Rounds that ended level. Not stored: it is whatever is left over once
    /// both sides' wins are taken off the settled total, and a third counter
    /// would be a third thing that could disagree with the other two.
    pub fn draws(&self) -> u32 {
        self.decided().saturating_sub(self.wins[0] + self.wins[1])
    }

    pub fn winner(&self) -> Option<Winner> {
        match self.phase {
            Phase::Over(winner) => Some(winner),
            Phase::Live => None,
        }
    }
}

/// Deterministic team assignment: honour each request where the sides have room
/// for it, and leave everyone else where they already were.
///
/// `entries` is `(handle, current side, requested side)`, **in handle order**.
/// The return is parallel to it: `(side, slot)` per pawn, where the slot is the
/// post on that side's muster line.
///
/// The algorithm is a single pass with a cap, and the simplicity is the point —
/// two peers have to reach the same answer, and anything that iterated to a
/// balance could reach it by a different route. Each pawn asks for a side; it
/// gets it if that side is not already at strength, and the other one otherwise.
/// So the sides can never differ by more than one, an early handle outranks a
/// late one when a side is oversubscribed, and a pawn that asks for nothing
/// simply stays put.
///
/// Note the cap is computed from how many pawns there actually are, not from
/// [`TEAM_SIZE`]: three pawns balance 2-1, not 3-0, which is what makes a small
/// game playable rather than a walkover.
pub fn balance(entries: &[(usize, u8, Option<u8>)]) -> Vec<(u8, usize)> {
    let cap = entries.len().div_ceil(TEAM_COUNT).clamp(1, TEAM_SIZE);
    let mut filled = [0usize; TEAM_COUNT];
    let mut out = Vec::with_capacity(entries.len());
    for &(_, current, request) in entries {
        let asked = (request.unwrap_or(current) as usize).min(TEAM_COUNT - 1);
        let side = if filled[asked] < cap { asked } else { TEAM_COUNT - 1 - asked };
        out.push((side as u8, filled[side]));
        filled[side] += 1;
    }
    out
}

/// Advance the clock, decide the round, and start the next one.
///
/// Runs LAST in the tick, so it judges the world as the tick finally left it —
/// including the round that landed on the last frame — and so the pawns it
/// re-posts at a round start are read by the next tick's intent systems rather
/// than by this tick's, which have already run.
pub fn run_round<C: Config<Input = PlayerInput>>(
    mut commands: Commands,
    scenario: Res<Scenario>,
    inputs: Res<PlayerInputs<C>>,
    mut round: ResMut<Round>,
    mut pawns: Query<PawnData>,
    bullets: Query<Entity, With<Bullet>>,
) {
    // The measuring rig is one fixed scene, not a match: no clock, and above all
    // nothing that would re-post its two carefully placed pawns.
    if !matches!(*scenario, Scenario::Arena) {
        return;
    }

    match round.phase {
        Phase::Live => {
            round.ticks += 1;
            let mut alive = [0u32; TEAM_COUNT];
            let mut present = [0u32; TEAM_COUNT];
            for (_, team, _, _, _, _, _, health, _) in &pawns {
                present[team.index()] += 1;
                if health.alive() {
                    alive[team.index()] += 1;
                }
            }
            // A side with nobody in it has not been wiped out — it has not
            // turned up. Without this, a warmup with no bots would end a round
            // on its first tick and every tick after that, forever.
            let contested = present.iter().all(|&n| n > 0);
            let decided = if contested && alive.contains(&0) {
                Some(match (alive[0], alive[1]) {
                    // Both sides down on the same tick: two rounds crossing in
                    // the air is rare and it is a draw, not a win for whoever
                    // the query happened to reach first.
                    (0, 0) => Winner::Draw,
                    (0, _) => Winner::Team(1),
                    _ => Winner::Team(0),
                })
            } else if round.ticks >= ROUND_TICKS {
                Some(match alive[0].cmp(&alive[1]) {
                    Ordering::Greater => Winner::Team(0),
                    Ordering::Less => Winner::Team(1),
                    Ordering::Equal => Winner::Draw,
                })
            } else {
                None
            };
            if let Some(winner) = decided {
                if let Winner::Team(side) = winner {
                    round.wins[(side as usize).min(TEAM_COUNT - 1)] += 1;
                }
                round.phase = Phase::Over(winner);
                round.ticks = 0;
            }
        }
        Phase::Over(_) => {
            round.ticks += 1;
            if round.ticks >= INTERMISSION_TICKS {
                round.number += 1;
                round.phase = Phase::Live;
                round.ticks = 0;
                start_round(&mut commands, &inputs, pawns.reborrow(), &bullets);
            }
        }
    }
}

/// Everything a round start rewrites on a pawn. Named once because both the
/// system and the helper it hands the query to have to spell it out, and a query
/// passed by `&mut` between them would be invariant over its lifetimes and
/// refuse to compile — hence `reborrow` at the call site.
type PawnData = (
    &'static Player,
    &'static mut Team,
    &'static mut Pos,
    &'static mut crate::Facing,
    &'static mut Stance,
    &'static mut Cooldown,
    &'static mut Aim,
    &'static mut Health,
    Option<&'static Bot>,
);

/// Everybody up, everybody re-sided, everybody back on their line.
fn start_round<C: Config<Input = PlayerInput>>(
    commands: &mut Commands,
    inputs: &PlayerInputs<C>,
    mut pawns: Query<PawnData>,
    bullets: &Query<Entity, With<Bullet>>,
) {
    // Sorted by handle, because `balance` resolves an oversubscribed side in
    // favour of the earlier handle and query order is not a determinism
    // guarantee.
    let mut entries: Vec<(usize, u8, Option<u8>)> = pawns
        .iter()
        .map(|(player, team, _, _, _, _, _, _, is_bot)| {
            // A bot never asks for a side; it goes where it is put. `get` rather
            // than an index because a bot's handle is deliberately outside the
            // session's range.
            let request = match is_bot {
                Some(_) => None,
                None => inputs
                    .get(player.handle)
                    .and_then(|&(input, _status)| input.team_request()),
            };
            (player.handle, team.0, request)
        })
        .collect();
    entries.sort_unstable_by_key(|&(handle, ..)| handle);

    // Handles are always below `MAX_PLAYERS`, so an array indexed by one is a
    // lookup table rather than a map — and a map would be an iteration-order
    // hazard in a sim that can't have one.
    let mut posting = [(0u8, 0usize); MAX_PLAYERS];
    for (&(handle, ..), &post) in entries.iter().zip(balance(&entries).iter()) {
        if handle < MAX_PLAYERS {
            posting[handle] = post;
        }
    }

    for (player, mut team, mut pos, mut facing, mut stance, mut cooldown, mut aim, mut health) in
        pawns
            .iter_mut()
            .map(|(p, t, pos, f, s, c, a, h, _)| (p, t, pos, f, s, c, a, h))
    {
        let (side, slot) = posting[player.handle.min(MAX_PLAYERS - 1)];
        let (x, y) = spawn_post(side, slot);
        *team = Team(side);
        *pos = Pos::from_units(x, y);
        *facing = spawn_facing(side);
        *stance = Stance::default();
        *cooldown = Cooldown::default();
        // Steady again, and owing no recoil — but the dice carry on where they
        // left off rather than being re-seeded. Re-seeding would need the
        // roster's salt, which this doesn't have, and would hand every round of
        // a match the identical sequence of deviations besides.
        aim.rest();
        // Everything, including `down` — this is the only place anyone comes
        // back, and they come back whole.
        *health = Health::default();
    }

    // Rounds still in the air belong to the round that fired them.
    for entity in bullets {
        commands.entity(entity).despawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(handle, current, request)` for a full field that nobody has asked to
    /// move from.
    fn settled(n: usize) -> Vec<(usize, u8, Option<u8>)> {
        (0..n)
            .map(|handle| (handle, crate::default_side(handle), None))
            .collect()
    }

    /// The sides come out even however many pawns there are, and a pawn nobody
    /// has moved stays where it was.
    #[test]
    fn balance_keeps_the_sides_even() {
        for n in 1..=MAX_PLAYERS {
            let entries = settled(n);
            let posts = balance(&entries);
            let mut count = [0usize; TEAM_COUNT];
            for &(side, _) in &posts {
                count[side as usize] += 1;
            }
            assert!(
                count[0].abs_diff(count[1]) <= 1,
                "{n} pawns split {count:?}, which is not a fair fight"
            );
            for (&(handle, current, _), &(side, _)) in entries.iter().zip(posts.iter()) {
                assert_eq!(side, current, "handle {handle} was moved without asking");
            }
        }
    }

    /// Two pawns on a side must not be sent to the same post, or a round opens
    /// with someone standing inside someone else.
    #[test]
    fn balance_hands_out_distinct_posts() {
        let posts = balance(&settled(MAX_PLAYERS));
        let mut seen = posts.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), posts.len(), "two pawns drew the same post: {posts:?}");
        for &(_, slot) in &posts {
            assert!(slot < TEAM_SIZE, "post {slot} is off the end of the muster line");
        }
    }

    /// A request is granted when the side it names has room.
    #[test]
    fn a_request_moves_you() {
        let mut entries = settled(4);
        // Handle 1 starts on side 1 and asks for side 0.
        entries[1].2 = Some(0);
        let posts = balance(&entries);
        assert_eq!(posts[1].0, 0, "the request was ignored");
        let mut count = [0usize; TEAM_COUNT];
        for &(side, _) in &posts {
            count[side as usize] += 1;
        }
        assert_eq!(count, [2, 2], "granting a request unbalanced the sides: {count:?}");
    }

    /// …and overruled when it doesn't. Everyone piling onto one side is exactly
    /// what a team dial invites, and the answer has to be "the first four of
    /// you", not "all of you".
    #[test]
    fn a_side_cannot_be_stacked() {
        let entries: Vec<_> = (0..MAX_PLAYERS)
            .map(|handle| (handle, crate::default_side(handle), Some(0)))
            .collect();
        let posts = balance(&entries);
        let mut count = [0usize; TEAM_COUNT];
        for &(side, _) in &posts {
            count[side as usize] += 1;
        }
        assert_eq!(count, [TEAM_SIZE, TEAM_SIZE]);
        // The earlier handles got what they asked for; the later ones didn't.
        for (handle, &(side, _)) in posts.iter().enumerate() {
            let expected = if handle < TEAM_SIZE { 0 } else { 1 };
            assert_eq!(side, expected, "handle {handle} landed on the wrong side");
        }
    }

    /// The two ends are exact mirrors. The harness's whole variance reduction is
    /// playing every trial from both of them, which only cancels the terrain if
    /// the posts themselves are identical.
    #[test]
    fn the_muster_lines_mirror_each_other() {
        for slot in 0..TEAM_SIZE {
            let (wx, wy) = spawn_post(0, slot);
            let (ex, ey) = spawn_post(1, slot);
            assert_eq!(wx, -ex, "post {slot} is not mirrored in x");
            assert_eq!(wy, ey, "post {slot} sits at a different height on each side");
        }
        assert_eq!(spawn_facing(0).x, -spawn_facing(1).x);
    }
}
