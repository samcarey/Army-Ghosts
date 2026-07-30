//! Persistence end to end: a match written down, put back, and played on.
//!
//! Two separate promises are tested here, and they fail in completely different
//! ways:
//!
//! * **A restored world is the world that was saved.** Cheap to believe, easy
//!   to get wrong in one field out of fifteen, and the failure is a player
//!   coming back to a match that has quietly forgotten their health or their
//!   side's rounds.
//! * **A restored world is a legal starting point for a LOCKSTEP session.**
//!   This is the one that matters and the one unit tests cannot reach. A resume
//!   has every peer rebuild its GGRS session from one agreed blob at frame 0;
//!   if two peers restoring identical bytes diverge by a subunit, the first
//!   tick of the resumed match is a desync and the match is over. So the blob
//!   is decoded twice into two independent apps which are then played in
//!   parallel off identical inputs and compared every tick.
//!
//! Like `combat.rs` these run in a real synctest session at
//! `with_check_distance(2)` — every frame re-simulated and checksummed — so
//! restored state that doesn't roll back properly (a `Round` resource read from
//! a blob, say) fails here rather than in somebody's match.

use std::time::Duration;

use bevy::platform::collections::HashMap;
use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ggrs::ggrs::{PlayerType, SessionBuilder};
use bevy_ggrs::{GgrsPlugin, LocalInputs, ReadInputs, Session};

use army_ghosts_sim::save::{self, Dials};
use army_ghosts_sim::*;

type TestConfig = bevy_ggrs::GgrsConfig<PlayerInput, String>;

/// Handles 4 and 5 are the two posts either side of the arena's ONE clear lane
/// (see `combat.rs`) — the only pair with no boulder between them, and so the
/// only pair one can shoot the other down.
const SHOOTER: usize = 4;
const VICTIM: usize = 5;
const LANE_PLAYERS: usize = 6;

/// What every handle is sending this tick, plus a tick counter so a test can
/// generate a long deterministic input stream without writing it down.
///
/// Note `inputs` holds whole `PlayerInput`s rather than a "who is driving"
/// pointer: the point of most of these tests is a handle sending
/// `PlayerInput::default()` — the blank input GGRS substitutes for a player who
/// has dropped — while the others carry on, and that needs every seat
/// independently settable.
#[derive(Resource)]
struct Script {
    players: usize,
    inputs: [PlayerInput; MAX_PLAYERS],
    /// Handles whose inputs are generated from [`Script::tick`] instead of read
    /// from `inputs` — the deterministic churn the lockstep test needs.
    churn: bool,
    tick: u32,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            players: 1,
            inputs: [PlayerInput::default(); MAX_PLAYERS],
            churn: false,
            tick: 0,
        }
    }
}

/// A busy, reproducible input for one handle on one tick: walking a circle that
/// drifts, firing on a prime cycle and changing stance on another, so movement,
/// the trigger, the stance machine and the round clock are all exercised — and
/// so the two peers in the lockstep test have plenty of opportunity to disagree.
fn churn_input(tick: u32, handle: usize) -> PlayerInput {
    let phase = tick.wrapping_mul(7).wrapping_add(handle as u32 * 29);
    let mut input = PlayerInput {
        move_x: (((phase % 255) as i32) - 127) as i8,
        move_y: (((phase.wrapping_mul(3) % 255) as i32) - 127) as i8,
        ..default()
    };
    if phase % 11 < 4 {
        input.buttons |= BTN_FIRE;
    }
    input.set_stance(((phase / 13) % 3) as u8);
    input
}

fn read_inputs(mut commands: Commands, mut script: ResMut<Script>) {
    let tick = script.tick;
    script.tick += 1;
    let mut inputs = HashMap::new();
    for handle in 0..script.players {
        let input = if script.churn {
            let mut input = churn_input(tick, handle);
            // The bot dial rides on handle 0 and only handle 0. Re-sent every
            // tick as an absolute count, exactly as the client does it.
            if handle == 0 {
                input.set_bots(script.inputs[0].bots().unwrap_or(0));
            }
            input
        } else {
            script.inputs[handle.min(MAX_PLAYERS - 1)]
        };
        inputs.insert(handle, input);
    }
    commands.insert_resource(LocalInputs::<TestConfig>(inputs));
}

/// An app with a session and the sim, and nothing in the world yet — `build`
/// gets to decide whether it is a fresh arena or a restored one.
///
/// The world it hands back has had **zero ticks run on it**, which these tests
/// need and `combat.rs`'s equivalent does not: comparing a restored world
/// against the blob it came from only means anything if nothing has moved in
/// between, and a restored arena of bots moves on its very first tick. Startup
/// and Update share a frame in bevy, so the trick is to let that frame pass
/// with no TIME in it — the ggrs driver accumulates against the tick length and
/// a zero delta never reaches it.
fn app_with(players: usize, build: impl Fn(&mut Commands) + Send + Sync + 'static) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::ZERO))
        .add_plugins(GgrsPlugin::<TestConfig>::default())
        .add_plugins(SimPlugin::<TestConfig>::default())
        .insert_resource(Script { players, ..default() })
        .add_systems(ReadInputs, read_inputs)
        .add_systems(Startup, move |mut commands: Commands| build(&mut commands));

    let mut builder = SessionBuilder::<TestConfig>::new()
        .with_num_players(players)
        .with_check_distance(2);
    for handle in 0..players {
        builder = builder.add_player(PlayerType::Local, handle).expect("add player");
    }
    app.insert_resource(Session::SyncTest(
        builder.start_synctest_session().expect("start synctest"),
    ));
    app.update(); // Startup: the world exists, and not a tick has been run in it
    // A hair over the tick length from here on, because the driver's
    // accumulator test is a strict `>`. One update is now one tick.
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / TICK_HZ as f64 + 1e-6,
    )));
    app
}

/// A fresh arena: `humans` human pawns, and `bots` bots reconciled in over the
/// following ticks the way the real game brings them in.
fn arena(humans: usize, bots: usize) -> App {
    let players = humans.max(1);
    let mut app = app_with(players, move |commands| {
        spawn_world(commands, humans, Scenario::Arena)
    });
    ask_for_bots(&mut app, bots);
    run(&mut app, bots + 1); // one tick per bot, plus one to settle
    app
}

/// The same world the blob describes, in a session of `players` seats.
fn resumed(save: &Save, players: usize) -> App {
    let bots = save.dials.bots as usize;
    let restoring = save.clone();
    let mut app = app_with(players, move |commands| {
        save::restore(commands, &restoring, &BotRoster::default())
    });
    ask_for_bots(&mut app, bots);
    app
}

/// Set the bot dial the way the client does — on handle 0's input, absolutely,
/// every tick. Restoring a world without also restoring the dial means
/// `reconcile_bots` deletes every bot that was just brought back, which is the
/// single easiest way to get this feature wrong.
fn ask_for_bots(app: &mut App, bots: usize) {
    app.world_mut().resource_mut::<Script>().inputs[0].set_bots(bots as u8);
}

/// Set what a handle sends from now on, verbatim.
///
/// Deliberately dumb: driving handle 0 replaces the bot dial along with
/// everything else, because half these tests are about a handle that has
/// stopped asking for anything and a helper that quietly kept the dial alive
/// would hide the very thing being measured. Tests that mean to keep the bots
/// say `set_bots` themselves.
fn drive(app: &mut App, handle: usize, input: PlayerInput) {
    app.world_mut().resource_mut::<Script>().inputs[handle.min(MAX_PLAYERS - 1)] = input;
}

fn run(app: &mut App, updates: usize) {
    for _ in 0..updates {
        app.update();
    }
}

fn capture(app: &mut App, dials: Dials) -> Save {
    save::capture(app.world_mut(), dials)
}

fn pawn(app: &mut App, handle: usize) -> (Pos, Health, Stance, Team) {
    app.world_mut()
        .query::<(&Player, &Pos, &Health, &Stance, &Team)>()
        .iter(app.world())
        .find(|(player, ..)| player.handle == handle)
        .map(|(_, pos, health, stance, team)| (*pos, *health, *stance, *team))
        .expect("pawn should exist")
}

fn bot_count(app: &mut App) -> usize {
    app.world_mut().query::<&Bot>().iter(app.world()).count()
}

/// Point `handle` at `target` and hold the trigger.
fn shoot_at(app: &mut App, handle: usize, target: Pos) {
    let (from, ..) = pawn(app, handle);
    let (dx, dy) = ((target.x - from.x) as i64, (target.y - from.y) as i64);
    let len = isqrt(dx * dx + dy * dy).max(1);
    drive(
        app,
        handle,
        PlayerInput {
            move_x: (dx * 127 / len) as i8,
            move_y: (dy * 127 / len) as i8,
            buttons: BTN_FIRE,
            ..default()
        },
    );
}

// ── The tests ───────────────────────────────────────────────────────────────

/// Play a match, write it down, put it back: the world that comes out is the
/// world that went in, field for field.
///
/// Compared as BLOBS rather than field by field, deliberately — a new component
/// added to the save then has to survive this test the day it is added, instead
/// of the day somebody remembers to assert on it.
#[test]
fn a_restored_world_is_the_world_that_was_saved() {
    let mut live = arena(2, 4);
    live.world_mut().resource_mut::<Script>().churn = true;
    run(&mut live, 400);

    let dials = Dials { bots: 4, aggro: 6 };
    let before = capture(&mut live, dials);
    // Somebody has to have been hit and somebody has to have moved, or this
    // proves only that two fresh arenas look alike.
    assert!(
        before.pawns.iter().any(|p| p.hp < MAX_HEALTH) || before.round.number > 1,
        "400 ticks of six pawns fighting produced no damage and no rounds; \
         the test is no longer exercising anything"
    );

    let text = before.encode();
    let decoded = Save::decode(&text).expect("our own blob must parse");
    let mut back = resumed(&decoded, 2);
    let after = capture(&mut back, dials);

    assert_eq!(after, before, "the restored world differs from the saved one");
    assert_eq!(after.encode(), text, "…and so does its blob");
}

/// **The one that matters.** Two peers decode the same blob, rebuild a session
/// each and play on from identical inputs; they must stay byte-identical.
///
/// This is what a rejoin does — every peer in the match restarts from one
/// agreed world at frame 0 — so a divergence here is a desync on the first tick
/// of a resumed match, which is the whole feature failing at the moment it is
/// used. Compared EVERY tick rather than at the end, because two worlds that
/// diverge and then re-converge (a bot's RNG landing back in step, a round
/// resetting everyone to their posts) would pass an end-state check while being
/// exactly the bug this is looking for.
#[test]
fn two_peers_restoring_one_blob_stay_in_lockstep() {
    let mut live = arena(2, 5);
    live.world_mut().resource_mut::<Script>().churn = true;
    run(&mut live, 300);

    let dials = Dials { bots: 5, aggro: 0 };
    let text = capture(&mut live, dials).encode();
    // Decoded twice, independently: a peer receives bytes, not a struct, and
    // `decode` is the only thing standing between the two of them agreeing.
    let one = Save::decode(&text).expect("decode");
    let two = Save::decode(&text).expect("decode");

    let (mut peer_a, mut peer_b) = (resumed(&one, 2), resumed(&two, 2));
    for app in [&mut peer_a, &mut peer_b] {
        app.world_mut().resource_mut::<Script>().churn = true;
    }

    for tick in 0..600 {
        peer_a.update();
        peer_b.update();
        let (a, b) = (capture(&mut peer_a, dials), capture(&mut peer_b, dials));
        assert_eq!(
            a,
            b,
            "two peers restoring one blob diverged at tick {tick}:\n{}\nvs\n{}",
            a.encode(),
            b.encode()
        );
    }

    let end = capture(&mut peer_a, dials);
    println!(
        "600 ticks past the resume: round {}, {} pawns, {} still standing",
        end.round.number,
        end.pawns.len(),
        end.pawns.iter().filter(|p| p.down == 0).count()
    );
}

/// The literal request: refresh, and if it is the same round and you are alive,
/// you are back where you were — not back on your post, and not at full health.
#[test]
fn coming_back_puts_you_where_you_left() {
    let mut live = arena(2, 3);
    live.world_mut().resource_mut::<Script>().churn = true;
    run(&mut live, 240);
    // Somewhere that is emphatically not a muster post, and lying down in it.
    drive(&mut live, 0, PlayerInput::default());
    live.world_mut().resource_mut::<Script>().churn = false;
    let mut prone = PlayerInput { move_x: 100, move_y: -60, ..default() };
    prone.set_stance(STANCE_PRONE);
    prone.set_bots(3);
    drive(&mut live, 0, prone);
    run(&mut live, 120);

    let (pos, health, stance, team) = pawn(&mut live, 0);
    let round_before = *live.world().resource::<Round>();
    assert_eq!(stance.level, STANCE_PRONE, "the setup never got the pawn down");
    assert!(health.alive(), "this test is about coming back alive");

    let text = capture(&mut live, Dials { bots: 3, aggro: 0 }).encode();
    let save = Save::decode(&text).expect("decode");
    assert!(save.alive(0), "the blob should say handle 0 is still up");

    let mut back = resumed(&save, 2);
    let (pos_back, health_back, stance_back, team_back) = pawn(&mut back, 0);
    assert_eq!(pos_back, pos, "came back somewhere else");
    assert_eq!(health_back.hp, health.hp, "came back at a different health");
    assert_eq!(stance_back.level, stance.level, "came back standing up");
    assert_eq!(team_back, team, "came back on the other side");
    assert_eq!(*back.world().resource::<Round>(), round_before, "came back to a different round");

    // And it STAYS there: nothing in the round machinery re-posts a pawn just
    // because the world is new.
    run(&mut back, 30);
    let (pos_later, ..) = pawn(&mut back, 0);
    assert_eq!(pos_later, pos, "something re-posted the pawn after the resume");
}

/// The other half of a refresh: while you are gone your pawn stays exactly
/// where you left it, in the stance you left it in — and can still be shot.
///
/// GGRS hands the sim `PlayerInput::default()` for a player who has dropped, so
/// this drives a handle with a blank input and watches what the sim makes of
/// it. Before the "zero means not asking" encoding, the blank stance bits read
/// as a request to STAND, so refreshing your browser stood your pawn up out of
/// the grass it was hiding in — the pawn was still there and still vulnerable,
/// which was the requirement, but it helpfully made itself a better target.
#[test]
fn a_vanished_player_holds_their_ground_and_can_still_be_killed() {
    let mut app = arena(LANE_PLAYERS, 0);

    // Get the victim down into the grass first, so there is a stance worth
    // holding on to.
    let mut go_prone = PlayerInput::default();
    go_prone.set_stance(STANCE_PRONE);
    drive(&mut app, VICTIM, go_prone);
    run(&mut app, 2 * STANCE_DOWN_TICKS as usize + 4);
    let (posted, health_before, stance_before, _) = pawn(&mut app, VICTIM);
    assert_eq!(stance_before.level, STANCE_PRONE, "the setup never got the victim down");

    // …and now they are gone. Nothing but blank inputs from here on.
    drive(&mut app, VICTIM, PlayerInput::default());
    run(&mut app, 180);

    let (pos, health, stance, _) = pawn(&mut app, VICTIM);
    assert_eq!(pos, posted, "an absent pawn wandered off");
    assert_eq!(stance.level, STANCE_PRONE, "an absent pawn stood up by itself");
    assert_eq!(health.hp, health_before.hp, "an absent pawn took damage from nothing");

    // Still a target. The shooter walks the one clear lane and holds the
    // trigger; three centred rounds is a kill, so 20 seconds is generous.
    for _ in 0..1200 {
        shoot_at(&mut app, SHOOTER, pos);
        app.update();
        if !pawn(&mut app, VICTIM).1.alive() {
            break;
        }
    }
    let (resting, health, ..) = pawn(&mut app, VICTIM);
    assert!(!health.alive(), "an absent player was invulnerable — hp {}", health.hp);
    assert_eq!(resting, pos, "the body moved");
}

/// A pawn nobody is driving must not empty the arena on its way out.
///
/// The bot count rides on handle 0's input and only handle 0's, so a blank
/// input there used to read as "no bots, please" — and whoever happened to hold
/// handle 0 refreshing their browser deleted every bot in the match, one per
/// tick, while their own pawn stood there. Any change to who owns the bot dial
/// has to keep this true.
#[test]
fn the_bots_survive_whoever_holds_the_dial_dropping_out() {
    let mut app = arena(2, 4);
    assert_eq!(bot_count(&mut app), 4, "the arena never filled");

    drive(&mut app, 0, PlayerInput::default());
    run(&mut app, 300);

    assert_eq!(bot_count(&mut app), 4, "the bots left with the player who wasn't asking");
    // …and the pawn belonging to the player who stopped asking is still there
    // to be found at all (`pawn` panics if it isn't).
    let _ = pawn(&mut app, 0);
}

/// The round clock comes back running.
///
/// [`Round`] is the only rollback-registered RESOURCE in the sim, and a resume
/// restores it from a blob rather than building it — so this checks the thing
/// that is easy to get wrong about that: a match resumed 30 ticks short of the
/// end of an intermission counts those 30 ticks out and starts the next round,
/// on the posts, at full health, with the series score it was carrying.
///
/// The world is built by hand rather than played into this state because
/// reaching a real intermission means either two minutes of clock or reaching
/// into `Health` from outside the schedule — and an external write is exactly
/// what a synctest at `check_distance(2)` undoes when it re-simulates from its
/// snapshot, so it would test nothing at all.
#[test]
fn a_resumed_intermission_counts_itself_out() {
    let nearly_done = Save {
        dials: Dials { bots: 0, aggro: 0 },
        round: Round {
            number: 3,
            phase: Phase::Over(Winner::Team(0)),
            ticks: INTERMISSION_TICKS - 30,
            wins: [2, 0],
        },
        pawns: (0..4)
            .map(|handle| save::PawnSave {
                handle,
                bot: false,
                team: default_side(handle),
                // Scattered where they fell, none of them on a post, and two of
                // them out of the round.
                x: (handle as i32 * 37 - 60) * FP,
                y: (handle as i32 * 23 - 40) * FP,
                facing_x: 0,
                facing_y: 127,
                cooldown: 7,
                stance_level: STANCE_PRONE,
                stance_change: 0,
                hp: if handle % 2 == 0 { 12 } else { 0 },
                down: if handle % 2 == 0 { 0 } else { 300 },
                hurt: 0,
                deaths: 1,
                kills: 1,
            })
            .collect(),
    };
    // Through the blob, not around it: this is the state a peer would be handed.
    let save = Save::decode(&nearly_done.encode()).expect("decode");
    let mut app = resumed(&save, 4);
    assert_eq!(app.world().resource::<Round>().number, 3);

    run(&mut app, 29);
    assert_eq!(app.world().resource::<Round>().number, 3, "the next round started early");

    run(&mut app, 2);
    let round = *app.world().resource::<Round>();
    assert_eq!(round.number, 4, "the resumed intermission never ran out");
    assert!(round.live(), "the next round did not start");
    assert_eq!(round.wins, [2, 0], "the series score did not survive the resume");

    for handle in 0..4 {
        let (pos, health, stance, team) = pawn(&mut app, handle);
        let (px, py) = spawn_post(team.0, handle / TEAM_COUNT);
        assert_eq!(pos, Pos::from_units(px, py), "handle {handle} is off its post");
        assert!(health.alive(), "handle {handle} did not come back for the new round");
        assert_eq!(health.hp, MAX_HEALTH, "handle {handle} came back hurt");
        assert_eq!(stance.level, STANCE_STAND, "handle {handle} came back prone");
    }
}

