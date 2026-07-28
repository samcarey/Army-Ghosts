//! One match, played as fast as the CPU will run it.
//!
//! The scene is the real arena with the real bots — this drives the same
//! `SimPlugin` schedule a browser does, through a real GGRS session, because
//! anything less would be measuring a different game from the one that ships.
//! What it strips is everything outside the sim: no window, no renderer, no
//! sockets, and a manual clock so a tick costs exactly what the arithmetic
//! costs.
//!
//! # What a match is now, and why the score changed
//!
//! A match is `rounds` Ghost War rounds: eight bots, four a side, mustered at
//! opposite ends, **nobody respawning**, two minutes or until one side is wiped
//! out. The score is **rounds won minus rounds lost**.
//!
//! That replaces kills-minus-deaths, and the reason is not taste. Under respawn,
//! a death cost you a walk back and the trade was the whole game, so the
//! differential *was* the objective. Under rounds it isn't even close: a bot that
//! trades one-for-one every time breaks even on kills and loses every round it
//! is outnumbered at the end of, and a bot that kills three and dies has won
//! nothing if its side still loses 4-1. Scoring the trade would measure a game
//! nobody is playing.
//!
//! Kills and deaths are still read out, as diagnostics — "won every round while
//! killing nobody" and "won every round 4-0" are both possible and are not the
//! same bot.
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
    default_side, spawn_world, BotProfile, BotRoster, Deaths, Kills, Player, PlayerInput, Round,
    Scenario, SimPlugin, INTERMISSION_TICKS, MAX_PLAYERS, ROUND_TICKS, TEAM_COUNT, TICK_HZ,
};

/// The address type is irrelevant to a synctest session; `String` matches what
/// the sim's own tests use.
type HarnessConfig = bevy_ggrs::GgrsConfig<PlayerInput, String>;

/// What a match produced.
#[derive(Copy, Clone, Debug, Default)]
pub struct Outcome {
    /// Rounds taken by each side.
    pub wins: [u32; TEAM_COUNT],
    /// Rounds that ended level — both sides wiped out together, or the clock
    /// running out with the same number standing.
    pub draws: u32,
    /// Ticks the match took, for the cost report.
    pub ticks: usize,
    pub kills: [u32; MAX_PLAYERS],
    pub deaths: [u32; MAX_PLAYERS],
}

impl Outcome {
    /// The candidate's rounds won minus its rounds lost. Draws are in neither,
    /// which is what makes the trial's sign meaningful: a match that ends level
    /// is a tie, and the sequential test drops it rather than modelling it.
    pub fn net(&self, candidate_team: u8) -> i32 {
        let mine = self.wins[(candidate_team as usize).min(TEAM_COUNT - 1)] as i32;
        let theirs = self.wins[TEAM_COUNT - 1 - (candidate_team as usize).min(TEAM_COUNT - 1)] as i32;
        mine - theirs
    }

    pub fn rounds(&self) -> u32 {
        self.wins.iter().sum::<u32>() + self.draws
    }

    pub fn total_kills(&self) -> u32 {
        self.kills.iter().sum()
    }

    pub fn total_deaths(&self) -> u32 {
        self.deaths.iter().sum()
    }
}

/// How many bots the harness asks for. Every post on both lines, filled.
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

/// Play `rounds` rounds and report what happened.
///
/// `candidate_team` is the side the candidate profile fights for; `salt` varies
/// the bots' dice so two matches with the same sides are still different games
/// (see [`BotRoster::salt`]).
pub fn play(
    candidate: BotProfile,
    baseline: BotProfile,
    candidate_team: u8,
    salt: u32,
    rounds: u32,
) -> Outcome {
    let mut app = arena(candidate, baseline, candidate_team, salt);
    // A round that nobody wins runs the full clock, so the worst case is every
    // round going the distance. Overshooting the budget rather than running
    // forever: a bot that hides for two minutes is a legitimate strategy and the
    // harness has to be able to sit through it.
    let budget = rounds as usize * (ROUND_TICKS + INTERMISSION_TICKS) as usize + 4 * MAX_PLAYERS;
    let mut ticks = 0;
    while ticks < budget && app.world().resource::<Round>().decided() < rounds {
        app.update();
        ticks += 1;
    }
    let mut outcome = read_outcome(&mut app);
    outcome.ticks = ticks;
    // The budget allows every round to run the full clock, so exhausting it is
    // not "the bots were slow" — it is the round cycle failing to advance, which
    // would otherwise show up as a match scored on however many rounds it
    // happened to reach.
    debug_assert_eq!(
        outcome.rounds(),
        rounds,
        "the tick budget ran out after {} of {rounds} rounds",
        outcome.rounds()
    );
    outcome
}

/// The match, built and warmed up but not yet played, so a caller that wants to
/// watch it tick by tick (a diagnostic, a regression test) can drive it itself
/// instead of only seeing the final score.
pub fn arena(candidate: BotProfile, baseline: BotProfile, candidate_team: u8, salt: u32) -> App {
    let mut roster = BotRoster::default();
    roster.salt = salt;
    // Sides are a pure function of the handle (`default_side`), so the roster
    // can be built without looking at a world that doesn't exist yet. That is
    // the whole reason the sim states it that way rather than assigning teams
    // out of the order bots happen to spawn in — `teams_follow_the_handles`
    // below is the check that it still holds.
    for handle in 0..MAX_PLAYERS {
        let mine = default_side(handle) == candidate_team;
        roster.set(handle, if mine { candidate } else { baseline });
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
    // many.
    for _ in 0..MAX_PLAYERS + 2 {
        app.update();
    }
    // Then put the clock back to the top. Those ticks were a round in progress
    // as far as the sim was concerned, fought by whichever bots had turned up
    // yet — handle 0 spent eight of them alone on the field. Measuring from a
    // full arena costs one line and removes the whole question.
    app.world_mut().insert_resource(Round::default());
    app
}

/// The scoreboard as it stands.
pub fn read_outcome(app: &mut App) -> Outcome {
    let round = *app.world().resource::<Round>();
    let mut outcome = Outcome {
        wins: round.wins,
        draws: round.draws(),
        ..Outcome::default()
    };
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
        "a death went uncredited — the diagnostics are only readable while every one of them is"
    );
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use army_ghosts_sim::Team;

    /// The harness assigns profiles by handle parity and reads the result by
    /// team, so those two had better be the same thing. If `reconcile_bots` ever
    /// stops putting a handle on its own side, every measurement this crate has
    /// ever printed becomes a mixture of both profiles and none of them says so.
    #[test]
    fn teams_follow_the_handles() {
        let mut app = arena(BotProfile::default(), BotProfile::default(), 0, 1);
        let mut query = app.world_mut().query::<(&Player, &Team)>();
        let mut seen = 0;
        for (player, team) in query.iter(app.world()) {
            assert_eq!(
                team.0,
                default_side(player.handle),
                "handle {} landed on side {}, not the side its profile was written for",
                player.handle,
                team.0
            );
            seen += 1;
        }
        assert_eq!(seen, MAX_PLAYERS, "the arena did not fill up");
    }

    /// Rounds actually resolve, and they resolve by somebody being wiped out
    /// rather than by the clock — if they only ever ran out of time this would
    /// be measuring how long two sides can avoid each other.
    #[test]
    fn rounds_are_fought_to_a_finish() {
        let outcome = play(BotProfile::default(), BotProfile::default(), 0, 1, 3);
        assert_eq!(outcome.rounds(), 3, "the match stopped short: {outcome:?}");
        assert!(outcome.total_kills() > 0, "three rounds and nobody died");
        let full_clock = 3 * ROUND_TICKS as usize;
        assert!(
            outcome.ticks < full_clock,
            "every round ran the clock out ({} ticks of a possible {full_clock}) — \
             the sides are not finding each other",
            outcome.ticks
        );
    }

    /// The score is zero sum, which is what lets the sign of one side's
    /// differential decide the trial on its own.
    #[test]
    fn the_two_sides_differentials_cancel() {
        let outcome = play(BotProfile::default(), BotProfile::default(), 0, 2, 2);
        assert_eq!(outcome.net(0), -outcome.net(1));
        assert_eq!(outcome.total_kills(), outcome.total_deaths(), "a death went uncredited");
    }

    /// The same match twice is the same match: the harness inherits the sim's
    /// determinism, so a measurement is reproducible and a difference between
    /// two runs is a real difference between the profiles.
    #[test]
    fn a_match_replays_identically() {
        let a = play(BotProfile::default(), BotProfile::default(), 0, 7, 2);
        let b = play(BotProfile::default(), BotProfile::default(), 0, 7, 2);
        assert_eq!((a.wins, a.draws, a.kills, a.deaths), (b.wins, b.draws, b.kills, b.deaths));
    }

    /// …and the salt actually changes it. Without this the harness would run
    /// hundreds of matches, every one of them the same match, and report a
    /// confident result from a sample of one.
    ///
    /// It compares the KILL tallies rather than the round results, because two
    /// different matches can perfectly well be won by the same side — that is a
    /// coincidence, not evidence the salt did nothing.
    #[test]
    fn the_salt_makes_a_different_match() {
        let a = play(BotProfile::default(), BotProfile::default(), 0, 7, 2);
        let b = play(BotProfile::default(), BotProfile::default(), 0, 8, 2);
        assert_ne!(
            (a.kills, a.deaths),
            (b.kills, b.deaths),
            "two salts played out identically — BotRoster::salt isn't reaching the bots"
        );
    }
}
