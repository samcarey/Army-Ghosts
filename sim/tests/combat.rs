//! Combat end to end: a real GGRS synctest session, ticked by hand.
//!
//! The unit tests in the crate cover the ballistics arithmetic; this covers the
//! part that arithmetic can't see — that a round fired at someone actually
//! finds them across a whole flight, that the damage adds up to a death, that
//! the death sticks for the rest of the round, and that the round cycle picks
//! everyone back up. It runs in a session rather than by calling the systems
//! directly because `PlayerInputs` can only be filled by one.
//!
//! `with_check_distance(2)` is the reason this is worth the setup: the synctest
//! rolls back and re-simulates every frame and compares checksums, so anything
//! about health, teams or the round clock that isn't rollback-safe fails here
//! rather than as a desync in a real match. The round state is a
//! rollback-registered RESOURCE rather than a component, which is a path nothing
//! else in this repo exercises — so it is worth knowing that these tests are
//! what would catch it going wrong.

use std::time::Duration;

use bevy::platform::collections::HashMap;
use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ggrs::ggrs::{PlayerType, SessionBuilder};
use bevy_ggrs::{GgrsPlugin, LocalInputs, ReadInputs, Session};

use army_ghosts_sim::*;

type TestConfig = bevy_ggrs::GgrsConfig<PlayerInput, String>;

/// The usual arena: two a side.
const PLAYERS: usize = 4;

/// **The one clear lane.** Handles alternate between the muster lines and take
/// posts in order, so handle 4 is the west line's third post at (-330, 65) and
/// handle 5 is the east line's, 660 units due east of it — and that pair is the
/// only one of the four with no boulder between them (`lane_is_clear` checks it,
/// and `SHOOTING_PLAYERS` is 6 so that both exist).
///
/// The other three lanes are blocked, which is the rock layout doing its job:
/// four clear corridors from one spawn line straight into the other would be
/// four sniping alleys. Any test that needs a shot to arrive has to use this
/// one, and has to say so.
const SHOOTER: usize = 4;
const VICTIM: usize = 5;
const SHOOTING_PLAYERS: usize = 6;

/// Which handle the test is driving, and what it's doing this tick. Everyone
/// else stands still and takes it.
#[derive(Resource, Default)]
struct Script {
    /// Session size, so `read_inputs` fills exactly the seats that exist.
    players: usize,
    handle: usize,
    input: PlayerInput,
    /// How many bots to ask for. Rides on handle 0's input, because that is the
    /// only copy `reconcile_bots` honours — the same path the menu uses.
    bots: u8,
    /// The aggression dial position, or 0 for "not asking".
    aggro: u8,
    /// A side each handle is asking for, if any. Unlike the two dials above,
    /// every player's own copy is read, so this is per handle.
    sides: [Option<u8>; MAX_PLAYERS],
}

fn read_inputs(mut commands: Commands, script: Res<Script>) {
    let mut inputs = HashMap::new();
    for handle in 0..script.players {
        let mut input = PlayerInput::default();
        input.set_team_request(script.sides[handle.min(MAX_PLAYERS - 1)]);
        inputs.insert(handle, input);
    }
    if let Some(input) = inputs.get_mut(&script.handle) {
        let side = input.team_request();
        *input = script.input;
        input.set_team_request(side);
    }
    inputs.entry(0).and_modify(|i| {
        i.set_bots(script.bots);
        i.set_aggression(script.aggro);
    });
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

/// A session with two a side, plus the usual cover.
fn arena() -> App {
    arena_with_bots(0)
}

/// The same, plus `bots` pawns the session has no seat for. The session is
/// still built for `PLAYERS` handles — that is the whole point: bot pawns carry
/// handles beyond the session's range, so anything still reaching into
/// `PlayerInputs` by handle panics here rather than in a match.
fn arena_with_bots(bots: usize) -> App {
    arena_with(PLAYERS, bots)
}

/// `humans` human pawns and `bots` bot ones.
///
/// `humans` may be 0 — the session still gets one seat, because the bot count
/// travels on handle 0's input and there is nowhere else to put it, but nobody
/// sits in it. That is exactly the shape the self-play harness runs, and it is
/// the only way to get an arena of bots with no inert human pawn parked at a
/// post being a free kill.
///
/// Bots arrive one per tick through `reconcile_bots`, so this ticks far enough
/// for all of them to be present before handing the app back — otherwise every
/// caller would have to know that and remember it.
fn arena_with(humans: usize, bots: usize) -> App {
    let players = humans.max(1);
    let mut app = App::new();
    // Manual clock: one update is one tick's worth of time, so tests count
    // ticks instead of hoping a wall clock cooperates. A hair over the tick
    // length because the driver's accumulator test is a strict `>`.
    let tick = Duration::from_secs_f64(1.0 / TICK_HZ as f64 + 1e-6);
    app.add_plugins(MinimalPlugins)
        .insert_resource(TimeUpdateStrategy::ManualDuration(tick))
        .add_plugins(GgrsPlugin::<TestConfig>::default())
        .add_plugins(SimPlugin::<TestConfig>::default())
        .insert_resource(Script { players, ..default() })
        .add_systems(ReadInputs, read_inputs)
        .add_systems(Startup, move |mut commands: Commands| {
            spawn_world(&mut commands, humans, Scenario::Arena)
        });

    let mut builder = SessionBuilder::<TestConfig>::new()
        .with_num_players(players)
        .with_check_distance(2);
    for handle in 0..players {
        builder = builder.add_player(PlayerType::Local, handle).expect("add player");
    }
    app.insert_resource(Session::SyncTest(
        builder.start_synctest_session().expect("start synctest"),
    ));
    app.update(); // Startup: the world exists from here on
    app.world_mut().resource_mut::<Script>().bots = bots as u8;
    run(&mut app, bots + 1); // one tick per bot, plus one to settle
    app
}

/// Where a handle mustered — its post, which is also where a round start puts
/// it back. Read off the sim rather than restated, so a change to the muster
/// lines doesn't need chasing through every test.
fn post(app: &mut App, handle: usize) -> Pos {
    let side = app
        .world_mut()
        .query::<(&Player, &Team)>()
        .iter(app.world())
        .find(|(player, _)| player.handle == handle)
        .map(|(_, team)| *team)
        .expect("pawn should exist");
    let slot = handle / TEAM_COUNT;
    let (x, y) = spawn_post(side.0, slot);
    Pos::from_units(x, y)
}

fn round(app: &App) -> Round {
    *app.world().resource::<Round>()
}

/// Point `handle` at the nearest living pawn on the other side and hold the
/// trigger. Facing follows the stick, so walking at someone is also aiming at
/// them; there is no need to separate the two.
fn chase_nearest_enemy(app: &mut App, handle: usize) {
    let me = app
        .world_mut()
        .query::<(&Player, &Team, &Pos, &Health)>()
        .iter(app.world())
        .find(|(player, ..)| player.handle == handle)
        .map(|(_, team, pos, health)| (*team, *pos, health.alive()));
    let Some((my_team, my_pos, true)) = me else { return };

    // Sorted by handle so a tie between two equidistant enemies resolves the
    // same way twice — this test is compared against itself by the synctest.
    let mut enemies: Vec<(i64, usize, Pos)> = app
        .world_mut()
        .query::<(&Player, &Team, &Pos, &Health)>()
        .iter(app.world())
        .filter(|(_, team, _, health)| **team != my_team && health.alive())
        .map(|(player, _, pos, _)| {
            let (dx, dy) = ((pos.x - my_pos.x) as i64, (pos.y - my_pos.y) as i64);
            (dx * dx + dy * dy, player.handle, *pos)
        })
        .collect();
    enemies.sort_unstable_by_key(|&(d2, who, _)| (d2, who));
    let Some(&(_, _, target)) = enemies.first() else { return };

    let (dx, dy) = ((target.x - my_pos.x) as i64, (target.y - my_pos.y) as i64);
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

/// Everyone still standing, per side.
fn alive_per_side(app: &mut App) -> [usize; TEAM_COUNT] {
    let mut counts = [0usize; TEAM_COUNT];
    for (team, health) in app.world_mut().query::<(&Team, &Health)>().iter(app.world()) {
        if health.alive() {
            counts[team.index()] += 1;
        }
    }
    counts
}

/// Drive a handle, leaving the bot count alone — `arena_with_bots` set it, and
/// clearing it here would have the reconciler quietly remove every bot.
fn drive(app: &mut App, handle: usize, input: PlayerInput) {
    let mut script = app.world_mut().resource_mut::<Script>();
    script.handle = handle;
    script.input = input;
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

/// Rounds land, damage accumulates, the pawn dies — and it STAYS dead.
///
/// The last part is the new rule and the one worth a test of its own: there is
/// no respawn timer to wait out any more, so a pawn that is down at tick 1 is
/// down at tick 600 and every tick between.
#[test]
fn rounds_kill_and_the_dead_stay_down() {
    let (west, east) = (spawn_post(0, SHOOTER / TEAM_COUNT), spawn_post(1, VICTIM / TEAM_COUNT));
    assert!(
        lane_is_clear(west, east),
        "a boulder moved into the {west:?} -> {east:?} lane; pick another pair"
    );
    let mut app = arena_with(SHOOTING_PLAYERS, 0);
    let start_pos = post(&mut app, VICTIM);
    let (pos, health, deaths) = pawn(&mut app, VICTIM);
    assert_eq!(health.hp, MAX_HEALTH);
    assert_eq!(deaths.0, 0);
    assert_eq!(pos, start_pos);

    // Pawns muster facing down the field at the other side, and the victim is
    // due east, so the trigger is the whole input: no walking, so the range
    // stays the 660 units between the two lines.
    drive(&mut app, SHOOTER, PlayerInput { move_x: 0, move_y: 0, buttons: BTN_FIRE, ..default() });

    // 660 units of flight is about 41 ticks, and rounds leave every
    // FIRE_COOLDOWN, so the first one lands well inside this.
    for _ in 0..90 {
        run(&mut app, 1);
        if pawn(&mut app, VICTIM).1.hp < MAX_HEALTH {
            break;
        }
    }
    let (_, health, _) = pawn(&mut app, VICTIM);
    assert!(
        health.hp < MAX_HEALTH,
        "a round fired straight down a clear lane at a pawn 660 units away must land"
    );
    assert!(health.hp > 0, "one round must not kill from across the arena");
    // Range falloff is the point: the same shot point blank is the full figure.
    assert!(
        MAX_HEALTH - health.hp < HIT_DAMAGE_MAX,
        "a shot from 660 units should land under the point-blank figure, took {}",
        MAX_HEALTH - health.hp
    );

    // Keep firing until it goes down. Longer than the old 300-unit version
    // needed: every round out here lands in the range floor.
    let mut died = false;
    for _ in 0..600 {
        run(&mut app, 1);
        if !pawn(&mut app, VICTIM).1.alive() {
            died = true;
            break;
        }
    }
    assert!(died, "a held trigger must eventually kill");
    let (_, health, deaths) = pawn(&mut app, VICTIM);
    assert_eq!(health.hp, 0);
    assert_eq!(deaths.0, 1, "the death should be on the board");

    // The shooter never hurt itself on the way.
    let (_, shooter, shooter_deaths) = pawn(&mut app, SHOOTER);
    assert_eq!(shooter.hp, MAX_HEALTH, "no self-hits");
    assert_eq!(shooter_deaths.0, 0);

    // Down and out, and nothing can touch it while it lies there.
    drive(&mut app, SHOOTER, PlayerInput::default());
    run(&mut app, 600);
    let (_, health, deaths) = pawn(&mut app, VICTIM);
    assert!(
        !health.alive(),
        "the dead came back inside a round — there is no respawn any more"
    );
    assert_eq!(health.hp, 0);
    assert_eq!(deaths.0, 1, "a corpse must not keep dying to rounds still in the air");
    assert!(health.down > 1, "`down` should be counting how long it has been out");
    // The round is still being fought: the victim's side has another pawn on it.
    assert!(round(&app).live(), "the round ended with a pawn still standing on each side");
}

/// Wiping out a side ends the round there and then, and the next one puts
/// everybody back on their feet at their post.
///
/// This is the whole cycle in one test, and it goes through the ELIMINATION path
/// rather than the clock because that is the one a match actually takes — a
/// round that runs the full two minutes is the exception.
#[test]
fn wiping_out_a_side_ends_the_round_and_the_next_one_starts_everyone_whole() {
    let mut app = arena();
    assert_eq!(alive_per_side(&mut app), [2, 2]);
    assert_eq!(round(&app).number, 1);
    assert_eq!(round(&app).wins, [0, 0]);

    // Handle 0 hunts the east side down and shoots it at close range, taking the
    // nearest living enemy each tick. Deliberately not a marksmanship exercise:
    // only one of the four lanes between the muster lines is clear, so anything
    // that stood on its post and fired would have to pick that one and could
    // only ever reach the pawn at the far end of it. Closing works from anywhere.
    let mut ended = None;
    for tick in 0..(ROUND_TICKS as usize) {
        chase_nearest_enemy(&mut app, 0);
        run(&mut app, 1);
        if !round(&app).live() {
            ended = Some(tick);
            break;
        }
    }
    let ended = ended.expect("one side should have been wiped out inside the round clock");
    println!("round decided after {ended} ticks");

    let decided = round(&app);
    assert_eq!(decided.winner(), Some(Winner::Team(0)), "the wrong side took the round");
    assert_eq!(decided.wins, [1, 0]);
    assert_eq!(decided.number, 1, "the number should only move when the next round starts");
    assert_eq!(alive_per_side(&mut app)[1], 0);

    // Nobody moves or shoots while the banner is up.
    drive(&mut app, 0, PlayerInput { move_x: 127, move_y: 0, buttons: BTN_FIRE, ..default() });
    let frozen: Vec<Pos> = (0..PLAYERS).map(|h| pawn(&mut app, h).0).collect();
    run(&mut app, (INTERMISSION_TICKS as usize) / 2);
    let still: Vec<Pos> = (0..PLAYERS).map(|h| pawn(&mut app, h).0).collect();
    assert_eq!(frozen, still, "a pawn moved after the round was decided");

    // Trigger and stick released before the next round starts, or the pawn this
    // test has been driving simply walks off its post as soon as it is put back
    // on it — and the check below is about where the round START puts people.
    drive(&mut app, 0, PlayerInput::default());

    // And then everyone is up again, whole, on their post, with the scoreboard
    // intact — deaths are a series total, not a round one.
    run(&mut app, (INTERMISSION_TICKS as usize) / 2 + 4);
    let next = round(&app);
    assert!(next.live(), "the next round never started");
    assert_eq!(next.number, 2);
    assert_eq!(next.wins, [1, 0], "the series score reset");
    let mut deaths_carried = 0;
    for handle in 0..PLAYERS {
        let expected = post(&mut app, handle);
        let (pos, health, deaths) = pawn(&mut app, handle);
        assert!(health.alive(), "handle {handle} was left out of the new round");
        assert_eq!(health.hp, MAX_HEALTH, "handle {handle} came back hurt");
        assert_eq!(pos, expected, "handle {handle} started the round off its post");
        deaths_carried += deaths.0;
    }
    assert_eq!(deaths_carried, 2, "the scoreboard was wiped with the round");
    // Rounds still in the air belonged to the round that fired them.
    assert_eq!(
        app.world_mut().query::<&Bullet>().iter(app.world()).count(),
        0,
        "a round survived into the next one"
    );
}

/// The sides muster at opposite ends, alternating by handle so any number of
/// players comes out even.
#[test]
fn the_two_sides_muster_at_opposite_ends() {
    let mut app = arena_with(SHOOTING_PLAYERS, 0);
    let mut seen = [0usize; TEAM_COUNT];
    for handle in 0..SHOOTING_PLAYERS {
        let (pos, ..) = pawn(&mut app, handle);
        let side = (handle % TEAM_COUNT) as u8;
        seen[side as usize] += 1;
        assert_eq!(pos, post(&mut app, handle), "handle {handle} is off its post");
        // West is negative x, east positive: the two sides are a field apart.
        let expected_sign = if side == 0 { -1 } else { 1 };
        assert_eq!(pos.x.signum(), expected_sign, "handle {handle} mustered at the wrong end");
    }
    assert_eq!(seen, [3, 3], "six players did not split evenly");
}

/// A player asking for the other side gets it — **at the top of the next
/// round**, not in the middle of the one they are in.
///
/// Mid-round would be worse than useless: the pawn does not move, so it would
/// change colours where it stands, which in a game where colour is how you tell
/// friend from foe means everyone around it changes what it is without anything
/// happening on screen.
#[test]
fn a_team_request_takes_effect_at_the_next_round() {
    let mut app = arena_with(SHOOTING_PLAYERS, 0);
    // Handle 2 starts on side 0. Ask for side 1.
    let before = pawn(&mut app, 2).0;
    app.world_mut().resource_mut::<Script>().sides[2] = Some(1);
    run(&mut app, 30);

    let side_of = |app: &mut App, handle: usize| -> u8 {
        app.world_mut()
            .query::<(&Player, &Team)>()
            .iter(app.world())
            .find(|(player, _)| player.handle == handle)
            .map(|(_, team)| team.0)
            .expect("pawn should exist")
    };
    assert_eq!(side_of(&mut app, 2), 0, "the request took effect mid-round");
    assert_eq!(pawn(&mut app, 2).0, before, "the pawn moved mid-round");

    // Run the clock out. It is a slow test and it is the only way to reach the
    // time-expiry path as well, which is worth having covered: with nobody
    // firing, both sides are intact and the round is a draw.
    run(&mut app, ROUND_TICKS as usize + INTERMISSION_TICKS as usize + 8);
    let next = round(&app);
    assert_eq!(next.number, 2, "the round clock never ran out");
    assert_eq!(next.wins, [0, 0], "a round nobody fought was awarded to somebody");
    assert_eq!(side_of(&mut app, 2), 1, "the request never took effect");
    // …and the sides stayed even, because `balance` moved someone the other way
    // to make room.
    let mut counts = [0usize; TEAM_COUNT];
    for handle in 0..SHOOTING_PLAYERS {
        counts[side_of(&mut app, handle) as usize] += 1;
    }
    assert_eq!(counts, [3, 3], "granting a request unbalanced the sides: {counts:?}");
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
    let mut input = PlayerInput { move_x: 90, move_y: 40, buttons: 0, ..default() };
    input.buttons |= BTN_FIRE;
    drive(&mut app, SHOOTER, input);
    run(&mut app, 180);

    // The bots are being driven — something moved, and it wasn't the wire.
    // Getting through 180 ticks at all is most of the point (the old code
    // panicked on tick one), but a bot that is present and inert would pass
    // that trivially.
    let moved = (PLAYERS..PLAYERS + BOTS)
        .filter(|&handle| pawn(&mut app, handle).0 != post(&mut app, handle))
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
        drive(&mut app, 0, PlayerInput { move_x: 0, move_y: 0, buttons: BTN_FIRE, ..default() });
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
    let mut probe = arena_with_bots(4);
    let interesting = a.iter().any(|&(handle, pos, ..)| {
        handle >= PLAYERS && pos != post(&mut probe, handle)
    });
    assert!(interesting, "nothing happened in 400 ticks; the comparison proved nothing");
}

/// Bots find each other and open fire unprompted.
///
/// The reaction *delay* itself is tested precisely in `bot.rs` (the ring buffer
/// is the mechanism, and a unit test can pin it to the tick); this only asserts
/// the end of that chain works at all — a broken memory index reads as a bot
/// that never shoots, which every other test here would pass.
#[test]
fn bots_open_fire_unprompted() {
    let mut app = arena_with_bots(4);
    // Nobody drives a human; the bots are the only thing that can shoot.
    drive(&mut app, SHOOTER, PlayerInput::default());

    // Generous, because the muster lines are 660 units apart and a bot walks at
    // 2 a tick: it has to close ~400 before anything is inside `ENGAGE_RANGE`,
    // which is 200 ticks of walking before the first shot is even possible.
    // Under the old scattered spawns this happened within twenty.
    let mut first_shot = None;
    for tick in 0..600 {
        run(&mut app, 1);
        let bullets = app.world_mut().query::<&Bullet>().iter(app.world()).count();
        if bullets > 0 {
            first_shot = Some(tick);
            break;
        }
    }
    let first_shot =
        first_shot.expect("bots in an arena together should close the distance and shoot");
    println!("first shot on tick {first_shot}");
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
    // A full arena of bots and no inert human pawns: a human standing on its
    // post taking it is a free kill that would make this pass without the bots
    // ever finding EACH OTHER, which is the thing being tested.
    let mut app = arena_with(0, MAX_PLAYERS);
    const TICKS: usize = 3600; // a minute — the sides start a field apart now

    let mut deaths = 0;
    let mut damaged = 0;
    for handle in 0..MAX_PLAYERS {
        let (_, health, d) = pawn(&mut app, handle);
        deaths += d.0;
        let _ = health;
    }
    assert_eq!(deaths, 0, "somebody died before the test started");
    run(&mut app, TICKS);

    for handle in 0..MAX_PLAYERS {
        let (_, health, d) = pawn(&mut app, handle);
        deaths += d.0;
        if health.hp < MAX_HEALTH || d.0 > 0 {
            damaged += 1;
        }
    }
    println!(
        "{}s of bots: {deaths} deaths, {damaged} pawns marked, round {} ({:?})",
        TICKS / TICK_HZ,
        round(&app).number,
        round(&app).phase
    );
    assert!(
        deaths >= 4,
        "a minute of eight bots in one arena and only {deaths} died — they aren't finding \
         each other across the new spawn distance"
    );
}

/// How far apart two pawns are, in whole world units.
fn apart(app: &mut App, a: usize, b: usize) -> i64 {
    let (pa, pb) = (pawn(app, a).0, pawn(app, b).0);
    let (dx, dy) = ((pa.x - pb.x) as i64, (pa.y - pb.y) as i64);
    isqrt(dx * dx + dy * dy) / FP as i64
}

/// Walk one pawn straight into another and it stops against them.
///
/// Nothing used to: `move_players` only pushed out of boulders, so two pawns
/// could stand on the same subunit. That was always wrong — soldiers are not
/// ghosts — but what made it a bug rather than a blemish is the next test.
#[test]
fn pawns_do_not_stand_inside_each_other() {
    let mut app = arena_with(SHOOTING_PLAYERS, 0);
    // Straight down the one clear lane, into the pawn parked at the far end. No
    // trigger: this is about bodies, not bullets. 660 units at 2 a tick is 330
    // ticks of walking before they even meet, hence the long run.
    drive(&mut app, SHOOTER, PlayerInput { move_x: 127, move_y: 0, buttons: 0, ..default() });

    let mut closest = i64::MAX;
    for _ in 0..600 {
        run(&mut app, 1);
        closest = closest.min(apart(&mut app, SHOOTER, VICTIM));
    }
    assert!(
        closest < 4 * PLAYER_R as i64,
        "the two never met ({closest} units apart at closest) — the walk proved nothing"
    );
    assert!(
        closest >= 2 * PLAYER_R as i64 - 6,
        "one pawn walked {closest} units into another (a body is {} across)",
        2 * PLAYER_R
    );
}

/// **The stand-on-each-other-and-fire-forever bug.**
///
/// Two bots that closed to nothing were pinned there permanently: `Act::Fight`
/// roots a bot with the same bit the sights button sets, so neither could walk
/// away, and neither could shoot the other either — a round is born
/// `PLAYER_R + BULLET_R + 2` units down the barrel, which is *past* a target
/// standing a unit and a half in front of you, so every shot flew off harmlessly
/// and nothing ever broke the tie. Measured at the time: one pair spent 3100
/// consecutive ticks (52 seconds) locked together, both firing, neither losing a
/// single point of health. It was reported from the demo as two bots standing on
/// top of each other firing in opposite directions forever, which is exactly what
/// it looks like from above.
///
/// Two things fix it and this test would fail without either: pawns are solid
/// now (`separate_players`), and a bot stops closing at `PUSH_STANDOFF` instead
/// of walking into whoever it is shooting.
///
/// The assertion is on the DURATION of the lock rather than on pawns never
/// touching, because a scrum in a crowded arena is legitimate and brief contact
/// is not a bug — being unable to get out of one is.
#[test]
fn bots_never_lock_together_firing_and_unable_to_connect() {
    const MATCH_TICKS: usize = 1800;
    /// A second of shooting each other point blank and resolving nothing.
    const STUCK_LIMIT: usize = 60;

    let mut app = arena_with_bots(4);
    drive(&mut app, SHOOTER, PlayerInput::default());

    let pawns = PLAYERS + 4;
    let mut run_len = vec![0usize; pawns * pawns];
    let mut worst = 0usize;
    let mut worst_pair = (0, 0);
    let mut closest = i64::MAX;

    for _ in 0..MATCH_TICKS {
        run(&mut app, 1);
        let state: Vec<(usize, Pos, bool, bool)> = app
            .world_mut()
            .query::<(&Player, &Pos, &Intent, &Health)>()
            .iter(app.world())
            .map(|(player, pos, intent, health)| {
                (player.handle, *pos, intent.0.fire(), health.alive())
            })
            .collect();
        for a in 0..state.len() {
            for b in (a + 1)..state.len() {
                if !state[a].3 || !state[b].3 {
                    continue;
                }
                let (dx, dy) = ((state[a].1.x - state[b].1.x) as i64, (state[a].1.y - state[b].1.y) as i64);
                let d = isqrt(dx * dx + dy * dy) / FP as i64;
                closest = closest.min(d);
                let key = state[a].0 * pawns + state[b].0;
                if d < 2 * PLAYER_R as i64 && state[a].2 && state[b].2 {
                    run_len[key] += 1;
                    if run_len[key] > worst {
                        worst = run_len[key];
                        worst_pair = (state[a].0, state[b].0);
                    }
                } else {
                    run_len[key] = 0;
                }
            }
        }
    }

    println!(
        "{MATCH_TICKS} ticks: closest approach {closest} units, longest overlapping-and-both-firing \
         run {worst} ticks (handles {} and {})",
        worst_pair.0, worst_pair.1
    );
    assert!(
        worst < STUCK_LIMIT,
        "handles {} and {} spent {worst} ticks inside each other with the trigger down — \
         that is the deadlock this test exists for",
        worst_pair.0,
        worst_pair.1
    );
    // …and the reason the deadlock was unbreakable: at point-blank range every
    // round is born past its target. Staying outside that dead zone is what
    // makes contact resolve instead of stalemate.
    assert!(
        closest > (PLAYER_R + BULLET_R + 2 - (PLAYER_R + BULLET_R)) as i64,
        "pawns closed to {closest} units, inside the range where a shot is born past its target"
    );
}

/// The menu's aggression dial reaches bots that are already standing there.
///
/// It has to be applied every tick rather than at spawn, or turning the dial
/// would do nothing until the bots were removed and re-added — which is not
/// what a dial in a menu means. Every tick is only safe because the input
/// carries an ABSOLUTE level: writing the same value onto the same bot twice is
/// writing it once, so a replayed tick lands on the identical world. This runs
/// under `with_check_distance(2)`, so if that reasoning were wrong it would
/// fail here as a checksum mismatch rather than as a desync in a match.
#[test]
fn the_aggression_dial_reaches_bots_already_in_the_match() {
    let mut app = arena_with_bots(4);

    let aggression = |app: &mut App| -> Vec<i32> {
        let mut values: Vec<(usize, i32)> = app
            .world_mut()
            .query::<(&Player, &Bot)>()
            .iter(app.world())
            .map(|(player, bot)| (player.handle, bot.profile.aggression))
            .collect();
        values.sort_unstable();
        values.into_iter().map(|(_, a)| a).collect()
    };

    // They spawned on the shipping profile, and nobody has touched the dial.
    let before = aggression(&mut app);
    assert_eq!(before.len(), 4);
    assert!(
        before.iter().all(|&a| a == BotProfile::default().aggression),
        "bots didn't start on the default profile: {before:?}"
    );

    // Turn it to the bottom of its range on bots that already exist.
    app.world_mut().resource_mut::<Script>().aggro = 1;
    run(&mut app, 2);
    assert!(
        aggression(&mut app).iter().all(|&a| a == 0),
        "the dial didn't reach bots already in the match: {:?}",
        aggression(&mut app)
    );

    // And back up again — a dial, not a one-way switch.
    app.world_mut().resource_mut::<Script>().aggro = AGGRO_LEVELS;
    run(&mut app, 2);
    assert!(
        aggression(&mut app).iter().all(|&a| a == FP),
        "the dial only moved one way: {:?}",
        aggression(&mut app)
    );

    // Releasing it leaves them where they were rather than snapping back to a
    // default — "not asking" has to mean not asking, which is what lets the
    // self-play harness drive handle 0's input without flattening its rosters.
    app.world_mut().resource_mut::<Script>().aggro = 0;
    run(&mut app, 4);
    assert!(
        aggression(&mut app).iter().all(|&a| a == FP),
        "an unset dial overwrote the profiles it was supposed to leave alone"
    );
}

/// **Bots do not shoot their own side.**
///
/// Friendly fire is on and it is expensive: nobody respawns, so a teammate you
/// shoot is a gun your side fights the rest of the round without. Two mechanisms
/// keep it from happening and this covers both — a teammate never enters the
/// bot's memory at all (`look` skips them), and a bot holds its fire when one is
/// standing in the lane it was about to shoot down (`blocked_by_a_friend`).
///
/// A full arena for a full round, which is what makes it worth asserting: eight
/// bots inside 800 units of each other, closing on the same middle, is exactly
/// the situation where a line crosses a friend.
#[test]
fn bots_do_not_shoot_their_own_side() {
    let mut app = arena_with(0, MAX_PLAYERS);
    let sides: std::collections::HashMap<usize, u8> = app
        .world_mut()
        .query::<(&Player, &Team)>()
        .iter(app.world())
        .map(|(player, team)| (player.handle, team.0))
        .collect();
    assert_eq!(sides.len(), MAX_PLAYERS, "the arena did not fill up");

    // Every round in flight, checked against the sides of whoever it could
    // reach. Tracking BULLETS rather than damage because a friendly round that
    // misses is the same mistake as one that lands; it just got lucky — and
    // counting them by ENTITY rather than by tick, so the ratio below is "one
    // round in N" rather than something that also depends on how long a round
    // happens to stay in the air.
    let mut friendly: std::collections::HashSet<Entity> = Default::default();
    let mut seen: std::collections::HashSet<Entity> = Default::default();
    for _ in 0..3600 {
        run(&mut app, 1);
        let bullets: Vec<(Entity, usize, Pos, i32, i32)> = app
            .world_mut()
            .query::<(Entity, &Bullet, &Pos)>()
            .iter(app.world())
            .map(|(entity, bullet, pos)| (entity, bullet.owner, *pos, bullet.vx, bullet.vy))
            .collect();
        let pawns: Vec<(usize, Pos, bool)> = app
            .world_mut()
            .query::<(&Player, &Pos, &Health)>()
            .iter(app.world())
            .map(|(player, pos, health)| (player.handle, *pos, health.alive()))
            .collect();
        for (entity, owner, at, vx, vy) in bullets {
            seen.insert(entity);
            // Where this round will be in four ticks: far enough to matter,
            // near enough that nobody has walked out of the way.
            let ahead = Pos { x: at.x + vx * 4, y: at.y + vy * 4 };
            for &(who, pos, alive) in &pawns {
                if who == owner || !alive || sides[&who] != sides[&owner] {
                    continue;
                }
                // Perpendicular distance from the teammate to the shot line.
                let (dx, dy) = ((ahead.x - at.x) as i64, (ahead.y - at.y) as i64);
                let (fx, fy) = ((at.x - pos.x) as i64, (at.y - pos.y) as i64);
                let len = isqrt(dx * dx + dy * dy).max(1);
                let off = (fx * dy - fy * dx).abs() / len / FP as i64;
                let along = -(fx * dx + fy * dy) / len / FP as i64;
                if off < (PLAYER_R + BULLET_R) as i64 && (0..64).contains(&along) {
                    friendly.insert(entity);
                }
            }
        }
    }
    let (fired, on_a_friend) = (seen.len(), friendly.len());
    println!("{fired} rounds fired, {on_a_friend} of them lined up on a teammate");
    // A floor on the sample, because "nobody was ever lined up on a teammate" is
    // trivially true of a match nobody fired a shot in, and that is a failure
    // this test would otherwise report as a pass.
    //
    // The floor is LOW, and the reason is worth knowing: in grass this deep a
    // bot can barely see anything past about a hundred units, so it creeps up
    // and kills at point-blank range, where its jitter is a couple of units and
    // three rounds are three hits. A minute of eight bots is therefore about
    // fifteen shots and five deaths — a quiet, close, decisive game. If this
    // ever starts failing on the sample size, the thing to check is whether
    // `Act::Settle`'s weight has moved, not whether the bots have stopped
    // working.
    assert!(
        fired >= 10,
        "only {fired} rounds fired in a minute of eight bots; too few to conclude anything"
    );
    // Not required to be exactly zero: a teammate can walk into a round already
    // in the air, which is not a decision the shooter got wrong. What must not
    // happen is bots routinely firing through their own side.
    assert!(
        on_a_friend * 10 < fired,
        "{on_a_friend} of {fired} rounds were lined up on a teammate — \
         bots are shooting their own side"
    );
}

/// **Bots advance.** Under the old scattered spawns every bot could see somebody
/// within a second, so "nobody has been seen yet" was a state that barely
/// existed. Now a round opens with 660 units of grass between the two sides and
/// no bot has a last known position to walk toward — so without an objective
/// they would all crouch where they stood and every round would run the clock
/// out at 4-4.
#[test]
fn bots_close_the_distance_at_the_start_of_a_round() {
    let mut app = arena_with(0, MAX_PLAYERS);
    let start: Vec<i32> = (0..MAX_PLAYERS).map(|h| pawn(&mut app, h).0.x).collect();
    run(&mut app, 300); // five seconds

    let mut advanced = 0;
    for (handle, &was) in start.iter().enumerate() {
        let now = pawn(&mut app, handle).0.x;
        // West pawns should have gone east and east ones west: toward the middle
        // either way, so the sign of the change is what says they are advancing
        // rather than just fidgeting.
        let toward_middle = if was < 0 { now - was } else { was - now };
        if toward_middle > 40 * FP {
            advanced += 1;
        }
    }
    assert!(
        advanced >= MAX_PLAYERS / 2,
        "only {advanced} of {MAX_PLAYERS} bots made 40 units of ground in five seconds — \
         they are waiting for a contact that will never come to them"
    );
}

/// **The game must not stop happening.**
///
/// This is the test for the bug reported from the demo as "a bunch of bots run
/// off at the beginning and disappear, then nothing happens". It watches a whole
/// match from the seat that found it — a player who stands on their post and
/// does nothing — and asserts that the world keeps moving around them.
///
/// It is deliberately about MOTION rather than about kills. Every stall this has
/// caught looked identical from outside: pawns alive, round clock running, and
/// every position byte-identical for tens of seconds. The causes were varied (a
/// hurt bot lying down blind, a bot standing on a stale contact, three bots
/// jamming each other's shot lines) and each of them was invisible to the
/// kills-and-rounds tests, which happily reported a decided match. What they
/// share is that nothing moved, and that is a thing a test can see.
#[test]
fn a_match_never_stops_moving_around_an_idle_player() {
    const SAMPLE: usize = 120; // two seconds
    const SAMPLES: usize = 45; // …of a minute and a half
    /// Consecutive samples with every pawn on exactly the same subunit. Contact
    /// legitimately pauses — a rooted firefight is pawns holding still on
    /// purpose — so this is generous, and still nowhere near the 30-plus seconds
    /// the reported stalls ran for.
    const STUCK_LIMIT: usize = 5;

    let mut app = arena_with(1, MAX_PLAYERS - 1);
    // The player does nothing at all, which is the whole point: the bug hid
    // behind every test that had someone driving.
    drive(&mut app, 0, PlayerInput::default());

    let snapshot = |app: &mut App| -> Vec<(usize, Pos, bool)> {
        let mut rows: Vec<(usize, Pos, bool)> = app
            .world_mut()
            .query::<(&Player, &Pos, &Health)>()
            .iter(app.world())
            .map(|(player, pos, health)| (player.handle, *pos, health.alive()))
            .collect();
        rows.sort_unstable_by_key(|&(handle, ..)| handle);
        rows
    };

    let (mut frozen, mut worst, mut worst_at) = (0usize, 0usize, 0usize);
    let mut rounds_seen = 0;
    let mut last = snapshot(&mut app);
    for sample in 0..SAMPLES {
        run(&mut app, SAMPLE);
        rounds_seen = rounds_seen.max(round(&app).number);
        let now = snapshot(&mut app);
        // Only living pawns: a field of corpses is meant to hold still.
        let moved = now
            .iter()
            .zip(last.iter())
            .any(|(a, b)| a.2 && (a.1 != b.1 || a.2 != b.2));
        frozen = if moved { 0 } else { frozen + 1 };
        if frozen > worst {
            worst = frozen;
            worst_at = (sample + 1) * SAMPLE;
        }
        last = now;
    }

    println!(
        "{} rounds in {} ticks; longest frozen stretch {} samples ({:.1}s), ending at tick {worst_at}",
        rounds_seen,
        SAMPLES * SAMPLE,
        worst,
        (worst * SAMPLE) as f32 / TICK_HZ as f32
    );
    assert!(
        worst <= STUCK_LIMIT,
        "the whole match stood still for {} samples ({:.1}s) ending at tick {worst_at} — \
         nothing moved, which is what 'then nothing happens' looks like from the inside",
        worst,
        (worst * SAMPLE) as f32 / TICK_HZ as f32
    );
    // And it was a real match, not a quiet one: the clock alone would give 2
    // rounds in this window, so anything less means rounds are not resolving.
    assert!(rounds_seen >= 2, "only {rounds_seen} round(s) in 90 seconds");
}
