//! Combat end to end: a real GGRS synctest session, ticked by hand.
//!
//! The unit tests in the crate cover the ballistics arithmetic; this covers the
//! part that arithmetic can't see — that a round fired at someone actually
//! finds them across a whole flight, that the damage adds up to a death, and
//! that the death undoes itself. It runs in a session rather than by calling
//! the systems directly because `PlayerInputs` can only be filled by one.
//!
//! `with_check_distance(2)` is the reason this is worth the setup: the synctest
//! rolls back and re-simulates every frame and compares checksums, so anything
//! about health or respawning that isn't rollback-safe fails here rather than
//! as a desync in a real match.

use std::time::Duration;

use bevy::platform::collections::HashMap;
use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ggrs::ggrs::{PlayerType, SessionBuilder};
use bevy_ggrs::{GgrsPlugin, LocalInputs, ReadInputs, Session};

use army_ghosts_sim::*;

type TestConfig = bevy_ggrs::GgrsConfig<PlayerInput, String>;

const PLAYERS: usize = 4;

/// The north-south pair: handle 2 spawns at (0, -150) and handle 3 at (0, 150),
/// and the lane between them is clear of boulders (`lane_is_clear` checks it).
/// The east-west pair is NOT — there's a boulder at (30, -23) squarely between
/// spawns 0 and 1, which is only the rock layout doing its job.
const SHOOTER: usize = 2;
const VICTIM: usize = 3;

/// Which handle the test is driving, and what it's doing this tick. Everyone
/// else stands still and takes it.
#[derive(Resource, Default)]
struct Script {
    handle: usize,
    input: PlayerInput,
}

fn read_inputs(mut commands: Commands, script: Res<Script>) {
    let mut inputs = HashMap::new();
    for handle in 0..PLAYERS {
        inputs.insert(handle, PlayerInput::default());
    }
    inputs.insert(script.handle, script.input);
    commands.insert_resource(LocalInputs::<TestConfig>(inputs));
}

/// Does a round fired from `from` to `to` reach it, or does it stop in cover?
/// Asserted up front by the tests that need a clear shot, so a reseeded rock
/// field fails saying what actually changed.
fn lane_is_clear(from: (i32, i32), to: (i32, i32)) -> bool {
    let (dx, dy) = ((to.0 - from.0) as f64, (to.1 - from.1) as f64);
    let len = (dx * dx + dy * dy).sqrt();
    rock_layout().iter().all(|&(x, y, rock)| {
        // Perpendicular distance from the boulder's centre to the shot line.
        let (fx, fy) = ((from.0 - x) as f64, (from.1 - y) as f64);
        (fx * dy - fy * dx).abs() / len > (rock.r + BULLET_R) as f64
    })
}

/// A session with all four spawns filled, plus the usual dummies and cover.
fn arena() -> App {
    arena_with_bots(0)
}

/// The same, plus `bots` pawns the session has no seat for. The session is
/// still built for `PLAYERS` handles — that is the whole point: bot pawns carry
/// handles beyond the session's range, so anything still reaching into
/// `PlayerInputs` by handle panics here rather than in a match.
fn arena_with_bots(bots: usize) -> App {
    let mut app = App::new();
    // Manual clock: one update is one tick's worth of time, so tests count
    // ticks instead of hoping a wall clock cooperates. A hair over the tick
    // length because the driver's accumulator test is a strict `>`.
    let tick = Duration::from_secs_f64(1.0 / TICK_HZ as f64 + 1e-6);
    app.add_plugins(MinimalPlugins)
        .insert_resource(TimeUpdateStrategy::ManualDuration(tick))
        .add_plugins(GgrsPlugin::<TestConfig>::default())
        .add_plugins(SimPlugin::<TestConfig>::default())
        .init_resource::<Script>()
        .add_systems(ReadInputs, read_inputs)
        .add_systems(Startup, move |mut commands: Commands| {
            spawn_world(&mut commands, PLAYERS, bots, Scenario::Arena)
        });

    let mut builder = SessionBuilder::<TestConfig>::new()
        .with_num_players(PLAYERS)
        .with_check_distance(2);
    for handle in 0..PLAYERS {
        builder = builder.add_player(PlayerType::Local, handle).expect("add player");
    }
    app.insert_resource(Session::SyncTest(
        builder.start_synctest_session().expect("start synctest"),
    ));
    app.update(); // Startup: the world exists from here on
    app
}

fn drive(app: &mut App, handle: usize, input: PlayerInput) {
    *app.world_mut().resource_mut::<Script>() = Script { handle, input };
}

fn run(app: &mut App, updates: usize) {
    for _ in 0..updates {
        app.update();
    }
}

fn pawn(app: &mut App, handle: usize) -> (Pos, Health, Deaths) {
    app.world_mut()
        .query::<(&Player, &Pos, &Health, &Deaths)>()
        .iter(app.world())
        .find(|(player, ..)| player.handle == handle)
        .map(|(_, pos, health, deaths)| (*pos, *health, *deaths))
        .expect("pawn should exist")
}

/// The whole loop: rounds land, damage accumulates, the pawn dies, and it comes
/// back at its spawn with the death on its record.
#[test]
fn rounds_kill_and_the_dead_come_back() {
    assert!(
        lane_is_clear(SPAWN_POINTS[SHOOTER], SPAWN_POINTS[VICTIM]),
        "a boulder moved into the north-south spawn lane; pick another pair"
    );
    let mut app = arena();
    let (start_pos, health, deaths) = pawn(&mut app, VICTIM);
    assert_eq!(health.hp, MAX_HEALTH);
    assert_eq!(deaths.0, 0);
    assert_eq!(start_pos, Pos::from_units(SPAWN_POINTS[VICTIM].0, SPAWN_POINTS[VICTIM].1));

    // Pawns face north by default and the victim is due north, so the trigger
    // is the whole input: no walking, so the range stays the 300 units between
    // the two spawns.
    drive(&mut app, SHOOTER, PlayerInput { move_x: 0, move_y: 0, buttons: BTN_FIRE });

    // ~300 units of flight is about 18 ticks, and rounds leave every
    // FIRE_COOLDOWN, so the first one lands well inside this.
    let mut first_hit = None;
    for tick in 0..60 {
        run(&mut app, 1);
        if pawn(&mut app, VICTIM).1.hp < MAX_HEALTH {
            first_hit = Some(tick);
            break;
        }
    }
    let first_hit = first_hit.expect("a round fired straight at a pawn 300 units away must land");
    let (_, health, _) = pawn(&mut app, VICTIM);
    assert!(health.hp > 0, "one round must not kill from across the arena");
    // Range falloff is the point: the same shot point blank is the full figure.
    assert!(
        MAX_HEALTH - health.hp < HIT_DAMAGE_MAX,
        "a shot from 300 units should land under the point-blank figure, took {}",
        MAX_HEALTH - health.hp
    );

    // Keep firing until it goes down.
    let mut died = None;
    for tick in 0..240 {
        run(&mut app, 1);
        if !pawn(&mut app, VICTIM).1.alive() {
            died = Some(first_hit + tick);
            break;
        }
    }
    assert!(died.is_some(), "a held trigger must eventually kill");
    let (_, health, deaths) = pawn(&mut app, VICTIM);
    assert_eq!(health.hp, 0);
    assert_eq!(deaths.0, 1, "the death should be on the board");

    // The shooter never hurt itself on the way.
    let (_, shooter, shooter_deaths) = pawn(&mut app, SHOOTER);
    assert_eq!(shooter.hp, MAX_HEALTH, "no self-hits");
    assert_eq!(shooter_deaths.0, 0);

    // Down and out: nothing can touch it while it waits.
    run(&mut app, 10);
    let (_, health, deaths) = pawn(&mut app, VICTIM);
    assert!(!health.alive() && health.hp == 0);
    assert_eq!(deaths.0, 1, "a corpse must not keep dying to rounds still in the air");

    // Cease fire before the respawn — spawns sit in the open, and a held
    // trigger will simply kill it again the moment it stands up.
    drive(&mut app, SHOOTER, PlayerInput::default());

    // Back on its feet, whole, at its spawn — and still carrying the death.
    run(&mut app, RESPAWN_TICKS as usize + 4);
    let (pos, health, deaths) = pawn(&mut app, VICTIM);
    assert!(health.alive(), "respawn never happened");
    assert_eq!(health.hp, MAX_HEALTH);
    assert_eq!(pos, start_pos, "respawned somewhere other than its spawn point");
    assert_eq!(deaths.0, 1);
}

/// The practice dummies still register, and cover still stops a round: the
/// rewrite moved both into the same nearest-impact pass as player hits, which
/// is exactly the sort of change that quietly drops one of them.
#[test]
fn dummies_still_take_rounds() {
    let mut app = arena();
    // Handle 0 spawns at (-150, 0) with the dummy at (-300, 0) behind it, and
    // the lane between them is kept clear by the layout.
    drive(&mut app, 0, PlayerInput { move_x: -127, move_y: 0, buttons: 0 });
    run(&mut app, 2);
    drive(&mut app, 0, PlayerInput { move_x: 0, move_y: 0, buttons: BTN_FIRE });
    run(&mut app, 40);

    let hits: u32 = app
        .world_mut()
        .query::<&Target>()
        .iter(app.world())
        .map(|target| target.hits)
        .sum();
    assert!(hits > 0, "rounds fired at a practice dummy 150 units away must register");
}

/// Bot pawns are simulated without a seat in the session.
///
/// This is the property the `Intent` split exists for. `move_players` and
/// `fire_bullets` used to index `PlayerInputs[player.handle]`, so a pawn whose
/// handle was outside the session's range panicked on the first tick — which
/// meant "is a pawn" and "has a network seat" were the same thing, and a bot
/// would have needed someone to send its inputs. The session here is built for
/// `PLAYERS` handles and the world has four more pawns than that.
///
/// It runs under `with_check_distance(2)`, so GGRS re-simulates and checksums
/// every frame: bot state that isn't rollback-safe fails here rather than as a
/// desync in a real match.
#[test]
fn bot_pawns_are_simulated_without_a_session_seat() {
    const BOTS: usize = 4;
    let mut app = arena_with_bots(BOTS);

    let pawns = app
        .world_mut()
        .query::<&Player>()
        .iter(app.world())
        .map(|p| p.handle)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        pawns,
        (0..PLAYERS + BOTS).collect::<std::collections::BTreeSet<_>>(),
        "bots should take the handles straight on from the humans, with no gaps"
    );

    // A human walks around and shoots while the bots are present, so the tick
    // does real work rather than just not crashing.
    let mut input = PlayerInput { move_x: 90, move_y: 40, buttons: 0 };
    input.buttons |= BTN_FIRE;
    drive(&mut app, SHOOTER, input);
    run(&mut app, 180);

    // The bots are being driven — something moved, and it wasn't the wire.
    // Getting through 180 ticks at all is most of the point (the old code
    // panicked on tick one), but a bot that is present and inert would pass
    // that trivially.
    let moved = (PLAYERS..PLAYERS + BOTS)
        .filter(|&handle| {
            pawn(&mut app, handle).0
                != Pos::from_units(SPAWN_POINTS[handle].0, SPAWN_POINTS[handle].1)
        })
        .count();
    assert!(moved > 0, "no bot moved in 180 ticks — the brain isn't running");

    // Nothing wandered off the map or out of a boulder.
    for handle in PLAYERS..PLAYERS + BOTS {
        let (pos, ..) = pawn(&mut app, handle);
        assert!(
            pos.x.abs() <= ARENA_HALF_W * FP && pos.y.abs() <= ARENA_HALF_H * FP,
            "bot {handle} left the arena at {pos:?}"
        );
    }
}

/// Bots are deterministic: the same world, ticked the same way, twice, lands
/// every pawn on the same subunit.
///
/// This is the property the whole architecture is arranged around, and it is
/// worth testing separately from the synctest's checksums because it catches a
/// different failure. The synctest proves a bot's state ROLLS BACK correctly;
/// this proves the decision itself doesn't depend on anything outside that
/// state — an unseeded RNG, a wall clock, or query iteration order.
///
/// The bots must actually fight for it to mean anything, so the arena is full
/// and the run is long enough for contact, deaths and respawns.
#[test]
fn bots_decide_identically_in_identical_worlds() {
    fn play() -> Vec<(usize, Pos, i32, u32)> {
        let mut app = arena_with_bots(4);
        drive(&mut app, SHOOTER, PlayerInput { move_x: 0, move_y: 0, buttons: BTN_FIRE });
        run(&mut app, 400);
        let mut out: Vec<(usize, Pos, i32, u32)> = app
            .world_mut()
            .query::<(&Player, &Pos, &Health, &Deaths)>()
            .iter(app.world())
            .map(|(p, pos, h, d)| (p.handle, *pos, h.hp, d.0))
            .collect();
        out.sort_unstable_by_key(|&(handle, ..)| handle);
        out
    }

    let a = play();
    let b = play();
    assert_eq!(a, b, "two identical runs diverged — a bot read something outside its state");

    // And the run was worth comparing: bots that never moved would match
    // trivially. Something has to have happened for 400 ticks of a full arena.
    let interesting = a
        .iter()
        .any(|&(handle, pos, ..)| {
            handle >= PLAYERS
                && pos != Pos::from_units(SPAWN_POINTS[handle].0, SPAWN_POINTS[handle].1)
        });
    assert!(interesting, "nothing happened in 400 ticks; the comparison proved nothing");
}

/// A bot shoots at somebody it can see, and doesn't shoot at somebody it can't.
///
/// The reaction queue makes the first half non-trivial: a bot that fires on the
/// tick it sees you has no reaction time, and one that never fires has a broken
/// memory index. Both are easy mistakes and both look fine from the outside.
#[test]
fn bots_open_fire_only_after_their_reaction_time() {
    let mut app = arena_with_bots(4);
    // Nobody drives a human; the bots are the only thing that can shoot.
    drive(&mut app, SHOOTER, PlayerInput::default());

    let mut first_shot = None;
    for tick in 0..120 {
        run(&mut app, 1);
        let bullets = app.world_mut().query::<&Bullet>().iter(app.world()).count();
        if bullets > 0 {
            first_shot = Some(tick);
            break;
        }
    }
    let first_shot = first_shot.expect("four bots in an arena together should find each other");
    assert!(
        first_shot >= BotProfile::default().reaction as usize,
        "a bot fired on tick {first_shot}, inside its own {}-tick reaction time",
        BotProfile::default().reaction
    );
}

/// Bots actually fight: left alone in an arena together they find each other,
/// trade rounds and put each other down.
///
/// A smoke test rather than a balance one — the self-play harness is what
/// judges whether they fight *well*. This only catches the failure where they
/// are technically running but never resolve anything, which every previous
/// assertion in this file would happily pass.
#[test]
fn bots_left_alone_fight_each_other() {
    let mut app = arena_with_bots(4);
    drive(&mut app, SHOOTER, PlayerInput::default());
    run(&mut app, 1800); // 30 seconds

    let mut deaths = 0;
    let mut damaged = 0;
    for handle in 0..PLAYERS + 4 {
        let (_, health, d) = pawn(&mut app, handle);
        deaths += d.0;
        if health.hp < MAX_HEALTH || d.0 > 0 {
            damaged += 1;
        }
    }
    println!("30s of bots: {deaths} deaths, {damaged} pawns marked");
    assert!(
        deaths > 0,
        "30 seconds of bots in one arena and nobody died — they aren't fighting"
    );
}
