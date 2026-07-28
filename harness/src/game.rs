//! One match, played as fast as the CPU will run it.
//!
//! The scene is the real arena with the real bots — this drives the same
//! `SimPlugin` schedule a browser does, through a real GGRS session, because
//! anything less would be measuring a different game from the one that ships.
//! What it strips is everything outside the sim: no window, no renderer, no
//! sockets, and a manual clock so a tick costs exactly what the arithmetic
//! costs.
//!
//! # No humans at all
//!
//! `spawn_world(.., 0, ..)` spawns no player pawns, so the arena is eight bots
//! and nothing else. The session still has one handle — it has to, because
//! `reconcile_bots` reads the bot count off handle 0's input and that is the
//! only channel it travels on — but that handle owns no pawn. A human pawn
//! standing inert at a spawn point would be a free kill parked on one side of
//! the map, which is exactly the sort of thing that quietly decides a match.

use std::time::Duration;

use bevy::platform::collections::HashMap;
use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ggrs::ggrs::{PlayerType, SessionBuilder};
use bevy_ggrs::{GgrsPlugin, LocalInputs, ReadInputs, Session};

use army_ghosts_sim::{
    spawn_world, BotProfile, BotRoster, Deaths, Kills, Player, PlayerInput, Scenario, SimPlugin,
    MAX_PLAYERS, TICK_HZ,
};

/// The address type is irrelevant to a synctest session; `String` matches what
/// the sim's own tests use.
type HarnessConfig = bevy_ggrs::GgrsConfig<PlayerInput, String>;

/// Which handles belong to the candidate. `false` is the baseline's.
pub type Sides = [bool; MAX_PLAYERS];

/// What a match produced, per handle.
#[derive(Copy, Clone, Debug, Default)]
pub struct Outcome {
    pub kills: [u32; MAX_PLAYERS],
    pub deaths: [u32; MAX_PLAYERS],
}

impl Outcome {
    /// The candidate's kills minus its deaths: who won the trade.
    ///
    /// Deaths alone would be the obvious score and is the wrong one — it makes
    /// the best possible bot the one that lies in the deepest grass it can find
    /// and never fires, and `caution` would win every run. Every death in the
    /// arena is credited to somebody, so the baseline's differential is exactly
    /// this negated and the sign alone decides the match.
    pub fn net(&self, sides: &Sides) -> i32 {
        (0..MAX_PLAYERS)
            .filter(|&handle| sides[handle])
            .map(|handle| self.kills[handle] as i32 - self.deaths[handle] as i32)
            .sum()
    }

    pub fn total_kills(&self) -> u32 {
        self.kills.iter().sum()
    }

    pub fn total_deaths(&self) -> u32 {
        self.deaths.iter().sum()
    }
}

/// How many bots the harness asks for. Every spawn point, filled.
const FILL: u8 = MAX_PLAYERS as u8;

/// The bot count the harness is asking for, sent on handle 0's input every
/// tick — the same absolute-value-not-an-edge channel the menu's dial uses.
#[derive(Resource, Copy, Clone)]
struct Fill(u8);

fn read_inputs(mut commands: Commands, fill: Res<Fill>) {
    let mut input = PlayerInput::default();
    input.set_bots(fill.0);
    let mut inputs = HashMap::new();
    inputs.insert(0usize, input);
    commands.insert_resource(LocalInputs::<HarnessConfig>(inputs));
}

/// Play one match and report what each handle did.
///
/// `sides` says which handles are the candidate's; `salt` varies the bots' dice
/// so two matches with the same sides are still different games (see
/// [`BotRoster::salt`]).
pub fn play(
    candidate: BotProfile,
    baseline: BotProfile,
    sides: &Sides,
    salt: u32,
    ticks: usize,
) -> Outcome {
    let mut app = arena(candidate, baseline, sides, salt);
    for _ in 0..ticks {
        app.update();
    }
    read_outcome(&mut app)
}

/// The match, built and warmed up but not yet played, so a caller that wants to
/// watch it tick by tick (a diagnostic, a regression test) can drive it itself
/// instead of only seeing the final score.
pub fn arena(
    candidate: BotProfile,
    baseline: BotProfile,
    sides: &Sides,
    salt: u32,
) -> App {
    let mut roster = BotRoster::default();
    roster.salt = salt;
    for (handle, &is_candidate) in sides.iter().enumerate() {
        roster.set(handle, if is_candidate { candidate } else { baseline });
    }

    let mut app = App::new();
    // One update is one tick's worth of time. A hair over the tick length
    // because bevy_ggrs's accumulator test is a strict `>`; without this the
    // session would step on some updates and not others.
    let tick = Duration::from_secs_f64(1.0 / TICK_HZ as f64 + 1e-6);
    app.add_plugins(MinimalPlugins)
        .insert_resource(TimeUpdateStrategy::ManualDuration(tick))
        .add_plugins(GgrsPlugin::<HarnessConfig>::default())
        .add_plugins(SimPlugin::<HarnessConfig>::default())
        .insert_resource(roster)
        .insert_resource(Fill(FILL))
        .add_systems(ReadInputs, read_inputs)
        .add_systems(Startup, |mut commands: Commands| {
            spawn_world(&mut commands, 0, Scenario::Arena)
        });

    // One player, no rollback verification. `check_distance(0)` is the whole
    // speed difference from `sim/tests/combat.rs`, which re-simulates every
    // frame on purpose: correctness is that file's job and it has already been
    // done by the time anything gets here.
    let session = SessionBuilder::<HarnessConfig>::new()
        .with_num_players(1)
        .with_check_distance(0)
        .add_player(PlayerType::Local, 0)
        .expect("add player")
        .start_synctest_session()
        .expect("start synctest");
    app.insert_resource(Session::SyncTest(session));

    app.update(); // Startup
    // Bots arrive one per tick, so the arena isn't full until it has had that
    // many. Not counted against the match: a bot that spent the first eight
    // ticks not existing didn't play a shorter game, its opponents just weren't
    // there yet, and the pair swap cancels whatever ordering that leaves.
    for _ in 0..MAX_PLAYERS + 2 {
        app.update();
    }
    app
}

/// The scoreboard as it stands.
pub fn read_outcome(app: &mut App) -> Outcome {
    let mut outcome = Outcome::default();
    let mut query = app.world_mut().query::<(&Player, &Kills, &Deaths)>();
    for (player, kills, deaths) in query.iter(app.world()) {
        if player.handle < MAX_PLAYERS {
            outcome.kills[player.handle] = kills.0;
            outcome.deaths[player.handle] = deaths.0;
        }
    }
    debug_assert_eq!(
        outcome.total_kills(),
        outcome.total_deaths(),
        "a death went uncredited — `net` is only a zero-sum score while every one of them is"
    );
    outcome
}

/// Every way to split eight handles into two fours, in a fixed order.
///
/// This is where most of the variety between matches comes from, and it is
/// variety of a useful kind: the spawn points are fixed and asymmetric (the
/// rock field is not the same in every corner), so which four seats a profile
/// gets is a real difference in the game it plays. 70 of them, and the pair
/// swap plays each one from both sides.
pub fn splits() -> Vec<Sides> {
    let mut out = Vec::new();
    for mask in 0u32..(1 << MAX_PLAYERS) {
        if mask.count_ones() as usize != MAX_PLAYERS / 2 {
            continue;
        }
        let mut sides = [false; MAX_PLAYERS];
        for (handle, side) in sides.iter_mut().enumerate() {
            *side = mask & (1 << handle) != 0;
        }
        out.push(sides);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 8 choose 4, and every one of them balanced.
    #[test]
    fn splits_are_all_the_even_ones() {
        let splits = splits();
        assert_eq!(splits.len(), 70);
        for sides in &splits {
            assert_eq!(sides.iter().filter(|&&s| s).count(), MAX_PLAYERS / 2);
        }
        // Distinct, and the list is a fixed order — two runs must line up pair
        // for pair or the pairing isn't pairing anything.
        let mut seen = splits.clone();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), splits.len());
    }

    /// The score is zero sum, which is what lets the sign of one side's
    /// differential decide the match on its own: flip who is called the
    /// candidate and the number must simply change sign.
    #[test]
    fn the_two_sides_differentials_cancel() {
        let sides = splits()[0];
        let mut flipped = sides;
        for side in flipped.iter_mut() {
            *side = !*side;
        }
        let outcome = play(BotProfile::default(), BotProfile::default(), &sides, 1, 600);
        assert_eq!(outcome.net(&sides), -outcome.net(&flipped));
        assert_eq!(outcome.total_kills(), outcome.total_deaths(), "a death went uncredited");
        assert!(outcome.total_kills() > 0, "600 ticks of eight bots and nobody died");
    }

    /// The same match twice is the same match: the harness inherits the sim's
    /// determinism, so a measurement is reproducible and a difference between
    /// two runs is a real difference between the profiles.
    #[test]
    fn a_match_replays_identically() {
        let sides = splits()[3];
        let a = play(BotProfile::default(), BotProfile::default(), &sides, 7, 400);
        let b = play(BotProfile::default(), BotProfile::default(), &sides, 7, 400);
        assert_eq!(a.kills, b.kills);
        assert_eq!(a.deaths, b.deaths);
    }

    /// …and the salt actually changes it. Without this the harness would run
    /// hundreds of matches, every one of them the same match, and report a
    /// confident result from a sample of one.
    #[test]
    fn the_salt_makes_a_different_match() {
        let sides = splits()[3];
        let a = play(BotProfile::default(), BotProfile::default(), &sides, 7, 400);
        let b = play(BotProfile::default(), BotProfile::default(), &sides, 8, 400);
        assert_ne!(
            (a.kills, a.deaths),
            (b.kills, b.deaths),
            "two salts played out identically — BotRoster::salt isn't reaching the bots"
        );
    }
}

