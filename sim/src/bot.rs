//! Bots: pawns the sim drives itself.
//!
//! # Why this shape
//!
//! A bot here has a harder constraint than a bot in most games: it runs inside
//! a **rollback** simulation. GGRS will re-run any tick as often as it likes,
//! from a restored snapshot, and every peer must reach the same decision from
//! the same state. That rules out most of the usual architectures on its own:
//!
//! * A **behavior tree** carries a running-node cursor between ticks.
//! * **HTN** (what Killzone 2's bots use) carries a whole task stack plus the
//!   world-state assumptions it was planned under, and replanning mid-rollback
//!   is a desync waiting to happen.
//! * An **FSM** carries a current state and timers, and timers are exactly
//!   where wall-clock creeps into a sim that must not read one.
//!
//! **Utility scoring carries almost nothing**: score every option against the
//! world as it is, take the best. Re-evaluating a rolled-back tick returns the
//! same answer by construction, which is the same property the rest of this
//! crate is built on. So that is what this is — Dave Mark's IAUS shape, in
//! integers.
//!
//! What state there *is* rolls back with the pawn ([`Bot`] is registered like
//! any other tick-evolving component), and it is deliberately tiny: an RNG
//! word, and a ring buffer of what the bot has seen.
//!
//! # What was copied, and from where
//!
//! Three mechanisms are lifted more or less directly from shipped shooters,
//! because in each case the obvious implementation is the one that breaks under
//! rollback and theirs happens not to:
//!
//! * **Reaction time is a ring buffer, not a timer** — Counter-Strike: Source
//!   (`cs_bot_vision.cpp`, `UpdateReactionQueue`). CS keeps 20 entries of "the
//!   biggest threat right now" and has the bot attend to the one *N steps
//!   back*. Fixed size, integer indices, measured in update intervals rather
//!   than seconds: it snapshots for free, and the bot genuinely acts on stale
//!   information instead of being artificially slowed. See [`Memory`].
//! * **Skill and accuracy are different knobs** — Quake III (`ai_dmq3.c`,
//!   `BotAimAtEnemy`). `aim_skill` GATES TECHNIQUES (only above 0.4 does the
//!   bot lead a moving target); `aim_accuracy` only sets how much noise is
//!   added to the aim point. A weak bot is one that doesn't know to lead — not
//!   a good bot with shaky hands. See [`BotProfile`].
//! * **Visibility is sampled across the body** — also CS, which tests
//!   gut/head/feet/left/right and returns which are visible. Here that is
//!   [`crate::visible_fraction`]'s five points, so cover degrades in fifths.
//!
//! # The one rule to keep
//!
//! Everything in here must be a pure function of rolled-back state. No
//! `Time`, no `HashMap` iteration, no floats, and nothing that depends on
//! query iteration order — where order could matter (which enemy to pick when
//! two score identically) it is broken by handle, which every peer agrees on.

use bevy::prelude::*;

use crate::{
    isqrt, spawn_post, visible_fraction, Aim, Health, Intent, Occluder, Player,
    PlayerInput, Pos, Scenario, Stance, Team, BTN_FIRE, BULLET_R, FP, MAX_PLAYERS,
    PLAYER_R, STANCE_CROUCH, STANCE_PRONE, STANCE_STAND, TEAM_SIZE,
};

/// How many ticks of sightings a bot keeps. Caps [`BotProfile::reaction`]: at
/// 60 Hz this is 400 ms, which is past the slowest reaction any shipped
/// shooter bothers with (Counter-Strike's easiest profiles sit at 300 ms).
pub const MEMORY_TICKS: usize = 24;

/// No enemy. `u8` rather than `Option<usize>` so [`Sighting`] stays `Copy` and
/// small — this array is snapshotted every tick.
const NOBODY: u8 = u8::MAX;

/// Beyond this the bot won't open fire, world units. Rounds carry much further
/// (`BULLET_TTL` is 90 ticks at `BULLET_SPEED`), but damage falls off with
/// range and giving away your position for a graze is a bad trade.
const ENGAGE_RANGE: i32 = 260;

/// How far apart the two memory samples used to estimate a target's velocity
/// are, in ticks. Long enough that a walking pawn has moved more than rounding,
/// short enough to still be this engagement.
const LEAD_SPAN: usize = 8;

/// Below this share of [`MAX_HEALTH`] a bot starts preferring to break contact.
const HURT_FRACTION: i32 = FP * 2 / 5;

/// How close [`Act::Push`] will close, world units. Past this it stops walking
/// and just fights.
///
/// `Push`'s score already fades as the range drops (`not(near)`), but "fades"
/// is not "stops", and a bot that keeps walking at someone it is already
/// shooting ends up standing on them. `separate_players` makes that survivable
/// — pawns can no longer interpenetrate — but two soldiers shoving each other
/// around a field is still not what closing to contact should look like. Three
/// body widths: inside the range where damage barely falls off, outside arm's
/// reach.
const PUSH_STANDOFF: i32 = 72;

/// How much clearance a teammate needs either side of the shot line before the
/// bot will pull the trigger, world units on top of the round's own hitbox.
///
/// Friendly fire is on, and it costs a round now that nobody respawns: a
/// teammate you shoot is a gun your side fights the rest of the round without.
/// So this is not politeness, it is arithmetic. Generous — half a body again —
/// because the bot is aiming at where it *thinks* the enemy is and the round has
/// jitter on it, so a line that only just clears a friend is a line that
/// sometimes doesn't.
const FRIENDLY_CLEARANCE: i32 = 12;

/// The middle of one side's muster line. The posts are symmetric about `y = 0`,
/// so it is the average of the two ends whatever [`TEAM_SIZE`] is.
fn muster_middle(line: u8) -> Pos {
    let (x, top) = spawn_post(line, TEAM_SIZE - 1);
    let (_, bottom) = spawn_post(line, 0);
    Pos::from_units(x, (top + bottom) / 2)
}

/// How long a bot will push against something it is not getting past before it
/// tries going round, in ticks. A third of a second — long enough that brushing
/// a rock in passing is not "stuck", short enough that nobody watches it happen.
///
/// **Bots have no pathfinding**, deliberately: they steer straight at whatever
/// they want and let `push_out_of_cover` deflect them, which works because that
/// cancels only the INTO-the-rock part of a step, so an angled approach slides
/// around. A DEAD-ON approach cancels entirely, and then the bot walks into the
/// same boulder for the rest of the round. Measured: a bot marching at an enemy
/// it had lost track of ground against a rock 23 units short of it for **68
/// seconds**, never arriving and — because `Act::Hunt` does not shoot — never
/// firing either. That is the fifth distinct cause
/// `a_match_never_stops_moving_around_an_idle_player` has caught, and the first
/// that is about geometry rather than about deciding.
const STUCK_TICKS: u16 = 20;

/// How far a bot must get from where it stood [`STUCK_TICKS`] ticks ago to
/// count as making progress, world units.
///
/// **A tick-to-tick comparison does not work**, and the reason is worth having
/// written down: a bot leaning on a boulder is shoved back out to the rock's
/// surface every tick by `push_out_of_cover`, and lands on a slightly different
/// SUBUNIT each time. Exact "did my position change" is therefore true every
/// tick while the bot goes nowhere at all, and the counter never climbs — which
/// is exactly what the first version of this did, silently. A crouched bot
/// genuinely walking covers about 22 units in this window; a jammed one covers
/// a fraction of one.
const STUCK_REACH: i32 = 6;

/// How long a bot will hold a spot it has seen nothing from before trying
/// somewhere else, in ticks. Four seconds.
///
/// This is the whole of the search behaviour and it is deliberately a long time:
/// holding still in cover and watching an approach IS the careful game, and a bot
/// that moved on the moment it lost contact would never be lying in wait for
/// anybody. What it must not be is permanent.
const SEARCH_PATIENCE: u16 = 4 * crate::TICK_HZ as u16;

/// Where a bot with nobody in sight is walking, and which end of its patrol it
/// is on.
///
/// # Which point, and why it is the middle of the enemy's line
///
/// Something like this became necessary the moment the sides started at opposite
/// ends of the field. Under the old scattered spawns every bot could see someone
/// within a second, so "no contact" was a state that barely existed and
/// [`Act::Hunt`] only ever needed a last known position to walk to. Now a round
/// opens with 660 units of grass between the two sides and NOBODY has a last
/// known position — so without an objective the whole field would crouch down
/// where it stood and wait out the clock, and every round would be a draw.
///
/// **Which point, exactly, turned out to matter far more than it sounds like it
/// should**, so all three candidates were run through the self-play harness
/// rather than argued about. Twenty pairs each, identical profiles both sides,
/// counting how many of the 360 rounds ended level — a drawn round being the
/// signature of two sides that never found each other:
///
/// | objective                       | drawn | mean round |
/// |---------------------------------|-------|------------|
/// | middle of the enemy line (this) |  **4**|      72 s  |
/// | the post opposite its own       |    56 |      80 s  |
/// | the middle of its OWN lane      |   132 |      88 s  |
///
/// The two rejected ones are the intuitive ones, which is why they are worth
/// recording. Sending each bot to the post facing it fans the line out nicely
/// and then walks the two sides PAST each other down parallel corridors with
/// grass in between, after which both sit down on the ground they have just
/// swapped. Sending each to the middle of its own lane is the same failure
/// earlier: they stop at the halfway line, four abreast, and a side that has
/// arrived has nothing left to advance toward.
///
/// Converging on one point looks like it should bunch them into a column — and
/// it does, at the end — but the 540 units they cover getting there is where the
/// fighting happens, and arriving late and bunched beats arriving early and
/// apart. The concern is real enough to have its own mitigation:
/// [`blocked_by_a_friend`] is what stops a column shooting through itself.
///
/// **A bot that arrived and found nothing used to sit down for the rest of the
/// round**, and that is the "then nothing happens" failure in its purest form.
/// [`ARRIVED`] fades `Act::Hunt` out over the last stretch of the walk so a bot
/// does not grind into the far wall — which is right, and which leaves
/// `Act::Settle` the only thing scoring. A bot that settles on empty ground then
/// never gets up again, because nothing it can see will ever change.
///
/// Caught by `a_match_never_stops_moving_around_an_idle_player` once the accuracy
/// model made rounds survivable enough to reach that state. The recorded case is
/// worth keeping: two bots stopped a hundred-odd units short of the enemy's line,
/// sat down, and spent the remaining **36 seconds** of the round there — while
/// the last live enemy lay prone and hidden in the middle of the map they had
/// just walked across. Neither side was hiding from the other; both had simply
/// run out of anywhere to be.
///
/// So the objective is a PATROL — their end, then yours, then theirs — and the
/// leg changes when the bot has **watched one spot for [`SEARCH_PATIENCE`] and
/// nothing came**. Two things about that trigger are load-bearing:
///
/// * **It is patience, not arrival.** Tying the flip to a distance means two
///   thresholds that have to agree about what "arrived" means, and they don't:
///   the bot stops advancing wherever `Act::Hunt` falls under `Act::Settle`,
///   which moves with its health, so any fixed radius leaves a band where it is
///   too close to walk and too far to have got there. Measured with the flip at
///   `ARRIVED`: the bot stopped 170 units short of a line it never reached, and
///   sat exactly as long as before.
/// * **Contact resets it**, so a bot in a firefight is never on a schedule, and
///   one lying in ambush stays there as long as it has anything to watch.
///
/// Sweeping is also what finds someone prone in deep grass: concealment falls off
/// with range, so a searcher that keeps moving eventually gets close enough to
/// see, and one that sits still never will.
fn patrol(bot: &mut Bot, team: Team) -> Pos {
    if bot.patience >= SEARCH_PATIENCE {
        bot.patience = 0;
        bot.homing = !bot.homing;
    } else {
        bot.patience += 1;
    }
    muster_middle(if bot.homing { team.0 } else { team.other().0 })
}

/// How close counts as arriving at the objective, world units.
///
/// `Act::Hunt`'s third consideration is how much ground there is left to make,
/// and it fades to nothing over the last `ARRIVED * 6` units of the walk. The
/// SCALE of that ramp is load-bearing in a way that is easy to get wrong, and
/// both ends of the range have been measured:
///
/// * `ARENA_HALF_W * 2` (800), on the reasoning that distance should be measured
///   against the biggest distance in the arena, leaves the consideration under
///   `Act::Settle`'s flat weight while the bot is still 165 units short. Against
///   an objective in the middle of the field that meant both sides sitting down
///   330 units apart — outside [`ENGAGE_RANGE`] — and a full arena of bots firing
///   NOT ONE ROUND in a minute.
/// * `ARRIVED` alone (80) is the other extreme: the bot walks all the way in.
///   With the objective on the enemy line that is a charge into their spawn, and
///   it tripled the drawn-round rate (12 of 360 against 4).
///
/// `ARRIVED * 6` stops a bot about 120 units short of the enemy line, which is
/// roughly where the two advances meet. The constant itself is a few body widths
/// because that is the unit "have I arrived" is naturally measured in; the
/// multiplier is what the harness picked.
///
/// **Stopping short is correct and used to be permanent**, which is what
/// [`SEARCH_PATIENCE`] is for — see [`patrol`].
const ARRIVED: i32 = 80;

/// How much a bot wants to walk to its objective when it has seen nobody.
///
/// Fixed rather than a [`BotProfile`] dial, because it is not a personality: a
/// side that will not leave its own half is not playing a cautious game, it is
/// not playing. Comfortably above [`Act::Settle`]'s `FP / 8` so the walk always
/// wins while there is ground to make, and comfortably below `FP` so a hurt bot
/// still prefers `Act::Break` — the `hp` consideration on both is what arbitrates.
const ADVANCE: i32 = FP / 2;

/// What a bot knows about the biggest threat at one past tick.
///
/// The enemy's *state at the time*, not a pointer to it — that is the whole
/// point of the delay. A bot with a 250 ms reaction aims at where you were a
/// quarter of a second ago, which is what makes it possible to break away from
/// one by moving.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Sighting {
    /// Handle of the enemy, or [`NOBODY`].
    who: u8,
    pos: Pos,
    stance: u8,
    /// How much of them was showing, `0..=FP`.
    exposure: i32,
}

impl Default for Sighting {
    fn default() -> Self {
        Self { who: NOBODY, pos: Pos { x: 0, y: 0 }, stance: STANCE_STAND, exposure: 0 }
    }
}

/// The reaction-time delay line: one [`Sighting`] per tick, round-robin.
///
/// Counter-Strike's `UpdateReactionQueue` in the shape this sim wants. Writing
/// is O(1) and the whole thing is `Copy`, so rollback costs a memcpy.
#[derive(Copy, Clone, Debug)]
pub struct Memory {
    slots: [Sighting; MEMORY_TICKS],
    /// Where the NEXT write goes.
    head: u8,
    /// How many slots have ever been written, saturating at `MEMORY_TICKS` —
    /// so a bot that has only just spawned can't attend to a slot it never
    /// filled and "remember" the origin.
    filled: u8,
}

impl Default for Memory {
    fn default() -> Self {
        Self { slots: [Sighting::default(); MEMORY_TICKS], head: 0, filled: 0 }
    }
}

impl Memory {
    fn push(&mut self, seen: Sighting) {
        self.slots[self.head as usize] = seen;
        self.head = ((self.head as usize + 1) % MEMORY_TICKS) as u8;
        self.filled = (self.filled + 1).min(MEMORY_TICKS as u8);
    }

    /// The sighting from `back` ticks ago, if the bot has been alive that long.
    fn recall(&self, back: usize) -> Option<Sighting> {
        let back = back.min(MEMORY_TICKS - 1);
        if back >= self.filled as usize {
            return None;
        }
        // `head` points at the next write, so the newest entry is head - 1.
        let i = (self.head as usize + MEMORY_TICKS - 1 - back) % MEMORY_TICKS;
        Some(self.slots[i])
    }

    /// The threat the bot is *conscious* of: what it saw one reaction time ago.
    fn attended(&self, reaction: u8) -> Option<Sighting> {
        self.recall(reaction as usize).filter(|s| s.who != NOBODY)
    }

    /// The most recent sighting of anyone at all, however stale — "last known
    /// position", which is what a bot hunts toward once contact is lost.
    fn last_contact(&self) -> Option<Sighting> {
        (0..self.filled as usize).find_map(|back| self.recall(back).filter(|s| s.who != NOBODY))
    }
}

/// How good a bot is, and at what. Every field is a dial the self-play harness
/// can vary, which is the point of having them in one struct.
///
/// The split between `skill` and `accuracy` is Quake III's and is worth keeping
/// straight: `skill` decides which *techniques* the bot has, `accuracy` decides
/// how much noise is on the aim. Turning `accuracy` down makes a bot that
/// misses; turning `skill` down makes a bot that shoots where you *were*.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BotProfile {
    /// `0..=FP`. Gates techniques — above [`LEAD_SKILL`] the bot leads a moving
    /// target instead of aiming at it.
    pub skill: i32,
    /// `0..=FP`. 0 sprays, `FP` is exact. Scales the aim jitter.
    pub accuracy: i32,
    /// Ticks between seeing something and being able to act on it. Clamped to
    /// [`MEMORY_TICKS`] - 1.
    pub reaction: u8,
    /// `0..=FP`. Weights closing the distance against holding ground.
    pub aggression: i32,
    /// `0..=FP`. Weights using concealment and breaking contact when hurt.
    pub caution: i32,
    /// `0..=FP`. How good a shot this bot insists on before it will take one —
    /// the share of the cone the target has to fill (see [`shot_quality`]).
    ///
    /// **This is the dial the accuracy model added, and it is the one that
    /// decides whether a bot plays carefully or sprays.** 0 pulls the trigger
    /// whenever an enemy is in range, which under a cone that opens with
    /// movement means emptying a magazine into the grass while running. `FP`
    /// waits for a round that essentially cannot miss, which costs opportunities
    /// and — past a point — costs the round, because a bot that never shoots
    /// never wins one.
    ///
    /// It is a gate on the trigger rather than a sixth consideration, so the
    /// "exactly three considerations" rule below is untouched.
    pub discipline: i32,
}

/// Above this `skill`, a bot leads a moving target. Quake III's threshold for
/// the same technique, on the same 0..1 scale.
const LEAD_SKILL: i32 = FP * 2 / 5;

impl Default for BotProfile {
    /// A competent but beatable opponent: a quarter-second to react, aim good
    /// enough to punish standing in the open, and no strong lean either way
    /// between pushing and holding.
    ///
    /// **These are unchanged by the move to rounds, and one of them nearly
    /// wasn't.** With the sides mustering a field apart, `aggression=0.2` came
    /// out ahead of this `0.5` in every decisive pair, which reads as a clear
    /// instruction to lower it — and lowering it was wrong. The dial was
    /// weighting `Act::Hunt` as well as `Act::Push` at the time, so a low setting
    /// was not "fight more patiently", it was "don't cross the field"; the
    /// winning bot was winning by making its opponent do all the walking. Adopted
    /// as the default it played itself to a standstill — 330 of 450 rounds drawn.
    /// The fix was to give the advance its own weight ([`ADVANCE`]), after which
    /// 0.2 against 0.5 is **150 pairs, none of them decisive**: no difference at
    /// all.
    ///
    /// What survives that is the ends of the range, which still say what they
    /// said: `0.9` loses (about -251 elo). So the shape of the finding is "don't
    /// charge", not "hang back", and the middle is where it already was.
    fn default() -> Self {
        Self {
            skill: FP * 3 / 5,
            accuracy: FP * 7 / 10,
            reaction: 15,
            aggression: FP / 2,
            caution: FP / 2,
            discipline: FP * 55 / 100,
        }
    }
}

/// How good a shot this is: the odds a round lands within the target's own
/// width, `0..=FP`. Capped at `FP` once the cone is the narrower of the two,
/// which is what "this cannot really miss" means.
///
/// **This is the exact inverse of the draw, not an approximation of it**, and
/// keeping it exact is why [`crate::taper`] is piecewise-linear rather than a
/// curve. The deviation is drawn as a tapered quantile (denser in the middle,
/// hard edge at the rim), so the share of draws inside the target is
/// [`crate::taper_share`] of the target's half-width over the cone's — which is
/// a closed form. Under the flat draw this was simply that ratio; the taper
/// lifts it, most at long range where the cone is widest, which is exactly the
/// case the middle-heavy shape exists to soften.
///
/// If the draw's shape ever changes, THIS MUST CHANGE WITH IT. It is what the
/// `discipline` trigger gate and `worth_a_round`'s close-the-distance judgement
/// are both built on, so a mismatch does not fail loudly — it quietly teaches
/// every bot the wrong thing about its own gun.
/// `shot_quality_matches_the_rounds_it_predicts` is the test that pins them
/// together.
///
/// It is an estimate of the GUN, deliberately, and takes no account of the bot's
/// own aim jitter or of how much of the target it can see. Those are the other
/// two thirds of the Quake III split and they are scored elsewhere; folding them
/// in here would make one number mean three things and the dial that reads it
/// impossible to tune.
pub fn shot_quality(spread: i32, range_units: i32) -> i32 {
    // **In SUBUNITS throughout.** Going through `Aim::cone_half_width` (whole
    // world units, for the renderer and the friendly-lane test) truncates, and
    // when the cone is within a unit or two of the target's own size that
    // truncation IS the answer — measured at 240 predicted against 254 actual
    // at 60 units, an error a twentieth-wide tolerance still caught.
    let tan = crate::SPREAD_MAX as i64 * spread.clamp(0, FP) as i64 / FP as i64;
    let half = tan * range_units.max(0) as i64;
    let target = PLAYER_R as i64 * FP as i64;
    if half <= target {
        return FP;
    }
    crate::taper_share(target * FP as i64 / half).clamp(0, FP as i64) as i32
}

/// What a round fired right now is worth: how likely it is to land, times
/// whether this is a range worth spending it at.
///
/// **This replaced the bare `nearness` that [`Act::Fight`] and [`Act::Push`]
/// used to weigh, and the swap is what stops the accuracy model deadlocking the
/// match.** Range alone said "you are close enough to shoot" — which, once a
/// round came out of a cone, stopped being the same question as "you can hit
/// them from here". Two bots that had closed to a standoff both scored `Fight`
/// at the top, both rooted, both fired, and with the cone wide open at that
/// range neither ever connected. Measured before this: the whole arena stood
/// still for **38 seconds** in `a_match_never_stops_moving_around_an_idle_player`
/// and the match got through 4 rounds in 90 seconds instead of 7.
///
/// Scoring the *shot* instead closes the loop: a bot that cannot hit from where
/// it stands scores `not(worth)` on `Push` and walks in until it can. Steadiness
/// and distance are both ways of being unable to hit, and the fix is the same
/// one for both — get closer, or hold still.
///
/// Two curves multiplied into one consideration, which is the same shape
/// `cowardice` already has, so the three-considerations-per-behavior rule below
/// still holds.
fn worth_a_round(spread: i32, dist_units: i32) -> i32 {
    shot_quality(spread, dist_units) * nearness(dist_units, ENGAGE_RANGE) / FP
}

/// Which profile each handle's bot is built with, and what salts their dice.
///
/// **Configuration, not tick state.** `reconcile_bots` reads it, but
/// only at the instant a bot spawns, and it must be CONSTANT for the life of a
/// match — exactly like [`Scenario`]. That constancy is the whole licence for
/// reading a resource inside the rollback schedule: a re-simulated tick spawns
/// the same bot it spawned the first time. Mutate it mid-match and you have
/// rebuilt the bug `BotCount` exists to avoid (the menu writes a resource, a
/// rollback re-reads it, the peers disagree) — which is why the number of bots
/// travels in the input stream and this does not.
///
/// In a room every peer must therefore build the same roster before the first
/// tick. Today that is free: the game never varies it, and `Default` gives
/// every bot [`BotProfile::default`]. It exists so the self-play harness can
/// put two different profiles in one arena.
#[derive(Resource, Clone, Copy, Debug)]
pub struct BotRoster {
    profiles: [BotProfile; MAX_PLAYERS],
    /// Mixed into every bot's RNG seed. Without it a deterministic sim plays
    /// the same match every time, so the harness would learn nothing from
    /// running a second one; changing the salt is what makes two matches with
    /// the same profiles genuinely different games.
    pub salt: u32,
}

impl Default for BotRoster {
    fn default() -> Self {
        Self { profiles: [BotProfile::default(); MAX_PLAYERS], salt: 0 }
    }
}

impl BotRoster {
    /// The profile for a handle. Out-of-range handles get the default rather
    /// than panicking — `reconcile_bots` runs mid-rollback.
    pub fn profile(&self, handle: usize) -> BotProfile {
        self.profiles.get(handle).copied().unwrap_or_default()
    }

    pub fn set(&mut self, handle: usize, profile: BotProfile) {
        if let Some(slot) = self.profiles.get_mut(handle) {
            *slot = profile;
        }
    }
}

/// A pawn the sim drives itself.
///
/// Rollback-registered like every other tick-evolving component: `seed` and
/// `memory` both advance every tick, and a bot whose RNG didn't roll back
/// would take a different shot on a re-simulated frame — which is a desync.
#[derive(Component, Copy, Clone, Debug)]
pub struct Bot {
    /// LCG state, advanced by [`Bot::rand`]. Seeded from the handle so two bots
    /// in the same match never draw the same jitter in lockstep.
    seed: u32,
    pub profile: BotProfile,
    memory: Memory,
    /// Which leg of the patrol this bot is on — see [`patrol`]. `false` is the
    /// outbound one, which is where a round starts.
    homing: bool,
    /// Ticks spent watching an empty field, reset by any contact. Both of these
    /// are tick state and roll back with the rest of the pawn.
    patience: u16,
    /// The aim wander, subunits — where in its miss envelope this bot's barrel
    /// currently points. A bounded random walk; see the note in [`engage`].
    drift_x: i32,
    drift_y: i32,
    /// Where this bot stood when the progress check last ran, how long ago that
    /// was, and what it concluded — see [`STUCK_TICKS`].
    last: Pos,
    stuck: u16,
    blocked: bool,
    /// Which way it steps aside when it is stuck. Fixed at construction from
    /// the handle so it is stable (a bot that picked a new side each tick would
    /// jitter in place) and so two bots wedged against each other peel off in
    /// opposite directions rather than shadowing one another.
    veer: i8,
}

impl Default for Bot {
    fn default() -> Self {
        Self::new(0, BotProfile::default())
    }
}

/// Salt for bot RNG seeding, so bot jitter is uncorrelated with the rock, bush
/// and grass fields.
const BOT_SEED: u32 = 0xB07_5EED;

impl Bot {
    pub fn new(handle: usize, profile: BotProfile) -> Self {
        Self::seeded(handle, profile, 0)
    }

    /// The same, with the roster's salt mixed in — see [`BotRoster::salt`].
    pub fn seeded(handle: usize, profile: BotProfile, salt: u32) -> Self {
        // Hashed rather than `BOT_SEED + handle`: consecutive LCG seeds produce
        // visibly correlated first draws, and "all the bots twitch together" is
        // exactly the tell that would give it away. The salt goes through the
        // same mixing step for the same reason — consecutive salts must not
        // give two matches correlated opening shots.
        let mut seed = (handle as u32)
            .wrapping_mul(0x9E37_79B9)
            .wrapping_add(salt.wrapping_mul(0x85EB_CA6B))
            ^ BOT_SEED;
        seed ^= seed >> 15;
        seed = seed.wrapping_mul(0xC2B2_AE35);
        seed ^= seed >> 13;
        Self {
            seed: seed | 1,
            profile,
            memory: Memory::default(),
            homing: false,
            patience: 0,
            drift_x: 0,
            drift_y: 0,
            last: Pos { x: 0, y: 0 },
            stuck: 0,
            blocked: false,
            veer: if handle.is_multiple_of(2) { 1 } else { -1 },
        }
    }

    /// The same LCG the layout code uses, advanced once per call. Deterministic
    /// because `seed` is rolled-back state.
    fn rand(&mut self) -> u32 {
        self.seed = self.seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.seed >> 8
    }

    /// A signed jitter in `-span..=span`.
    fn jitter(&mut self, span: i32) -> i32 {
        if span <= 0 {
            return 0;
        }
        (self.rand() % (span as u32 * 2 + 1)) as i32 - span
    }
}

// ── Considerations ──────────────────────────────────────────────────────────
//
// Each maps some fact about the world onto `0..=FP` — "how much does this
// favour the behavior being scored". Kept as named functions rather than
// inlined arithmetic because the whole tuning surface is which curve goes on
// which axis, and that is only legible if the curves have names.

/// Emphasise the top of the range: `x²`. Something that matters only when it's
/// nearly true (a target you can almost see is not a target).
fn sharp(x: i32) -> i32 {
    let x = x.clamp(0, FP);
    x * x / FP
}

/// The complement.
fn not(x: i32) -> i32 {
    FP - x.clamp(0, FP)
}

/// `1` at zero distance, falling to `0` at `span`, linear.
fn nearness(dist_units: i32, span: i32) -> i32 {
    if span <= 0 {
        return 0;
    }
    (FP - dist_units.clamp(0, span) * FP / span).clamp(0, FP)
}

/// Combine considerations into a score.
///
/// Multiplying values in `0..=FP` drags a score down as considerations are
/// added, which unfairly penalises the behaviors that think hardest — the
/// standard fix is the geometric mean, which is awkward in integer math.
/// Sidestepped rather than solved: **every behavior scores on exactly three
/// considerations**, so the bias is identical across them and cancels. If a
/// fourth is ever wanted, all of them need one.
fn score(weight: i32, a: i32, b: i32, c: i32) -> i32 {
    let fp = FP as i64;
    let (a, b, c) = (a.clamp(0, FP) as i64, b.clamp(0, FP) as i64, c.clamp(0, FP) as i64);
    ((weight as i64 * a / fp) * b / fp * c / fp) as i32
}

// ── The brain ───────────────────────────────────────────────────────────────

/// One candidate course of action. Scored, then the winner produces the
/// [`Intent`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Act {
    /// Stand and shoot at what you can see.
    Fight,
    /// Close the distance, shooting on the way.
    Push,
    /// Get away from the last contact and get low.
    Break,
    /// Move toward where they were last seen.
    Hunt,
    /// Stay put, get low, and let the grass do the work.
    Settle,
}

/// Decide what every bot is doing this tick.
///
/// Runs at the head of `GgrsSchedule`, so it reads the world exactly as the
/// previous tick left it — settled, respawns included — and writes only
/// [`Intent`], which the movement and firing systems consume immediately after.
///
/// The two queries are disjoint by component, not by filter: `pawns` reads
/// `Pos`/`Stance`/`Health`/`Player` and `bots` writes only `Bot` and `Intent`.
/// Both read `Pos`, which is fine, and neither writes what the other reads.
pub fn bot_think(
    scenario: Res<Scenario>,
    pawns: Query<(&Player, &Team, &Pos, &Stance, &Health, &Aim)>,
    rocks: Query<(&crate::Rock, &Pos)>,
    bushes: Query<(&crate::Bush, &Pos)>,
    mut bots: Query<Thinker>,
) {
    // Built once and shared: a pairwise sweep asks this many times over.
    // Order-independent — every use is an `any()` — so no sort is needed.
    let occluders: Vec<Occluder> = rocks
        .iter()
        .map(|(rock, pos)| Occluder { pos: *pos, r: rock.r })
        .chain(bushes.iter().map(|(bush, pos)| Occluder { pos: *pos, r: bush.r }))
        .collect();

    // Sorted by handle: target selection breaks ties on it, and query order is
    // not a determinism guarantee.
    let mut others: Vec<Contact> = pawns
        .iter()
        .map(|(player, team, pos, stance, health, aim)| Contact {
            handle: player.handle,
            team: *team,
            pos: *pos,
            stance: stance.level,
            stir: aim.stir,
            alive: health.alive(),
        })
        .collect();
    others.sort_unstable_by_key(|c| c.handle);

    for (player, team, pos, stance, health, aim, mut bot, mut intent) in &mut bots {
        // The dead do nothing. Still push an empty sighting so the memory keeps
        // advancing in step with the tick count — otherwise "12 ticks ago"
        // would mean something different after every round.
        if !health.alive() {
            bot.memory.push(Sighting::default());
            *intent = Intent::default();
            continue;
        }

        let seen = look(
            &scenario,
            &occluders,
            player.handle,
            *team,
            *pos,
            stance.level,
            &others,
        );
        bot.memory.push(seen);
        // Only the friends worth not shooting: living, and not this bot.
        let friends: Vec<Pos> = others
            .iter()
            .filter(|c| c.alive && c.team == *team && c.handle != player.handle)
            .map(|c| c.pos)
            .collect();
        // Last tick's cone — `settle_aim` runs later in this one. That is the
        // right answer as well as the only available one: a shooter decides on
        // how steady it FEELS, which is how steady it was a moment ago.
        //
        // "Hidden" is the bot's own honest estimate of itself: buried where it
        // stands, and not lit up by its own recent movement or muzzle flash.
        // Both are things it genuinely knows — its grass and its own `stir` —
        // so no peer can disagree.
        // **Stuck detection reads LAST tick's intent**, which is still sitting
        // in the component at this point — `decide` overwrites it below. So the
        // question is exactly "did I ask to walk, and did I get anywhere?",
        // with no extra bookkeeping.
        //
        // It asks `step_speed`, NOT whether the move vector is non-zero, and
        // the difference is the whole thing: **the move vector doubles as the
        // AIM direction**, so a bot standing still and aiming (`engage` sets
        // both the vector and `BTN_ADS`, which roots the pawn) has a non-zero
        // vector and by definition never moves. Counting that as stuck made
        // every aiming bot "stuck" within a third of a second and then rotated
        // its aim a quarter turn — two bots at point-blank range firing at
        // right angles to each other, forever. `step_speed` is the one place
        // that knows rooted from walking.
        let tried_to_walk = crate::step_speed(&intent.0, stance) > 0;
        if !tried_to_walk {
            bot.stuck = 0;
            bot.blocked = false;
            bot.last = *pos;
        } else {
            bot.stuck = bot.stuck.saturating_add(1);
            if bot.stuck >= STUCK_TICKS {
                let no_progress = units(dist(*pos, bot.last)) < STUCK_REACH;
                // **Still stuck after stepping aside means that side was the
                // wrong one — so try the other.** A fixed side is a coin flip
                // against the geometry and loses half the time: measured, a bot
                // marching due north into a boulder veered due east into the
                // arena wall and stayed there. Alternating guarantees escape
                // within two windows, and the flip is deterministic state so
                // every peer picks the same way out.
                if no_progress && bot.blocked {
                    bot.veer = -bot.veer;
                }
                bot.blocked = no_progress;
                bot.stuck = 0;
                bot.last = *pos;
            }
        }

        let gun = Gun {
            spread: aim.spread(),
            poised: Aim::settled(&Stance { level: stance.level, change: 0 }, 0, true),
            hidden: is_concealed(&scenario, *pos, stance.level) && aim.stir < FP / 4,
        };
        *intent = decide(&mut bot, *pos, *team, stance.level, health, gun, &friends);
    }
}

/// A pawn the brain drives, and everything it decides from.
type Thinker = (
    &'static Player,
    &'static Team,
    &'static Pos,
    &'static Stance,
    &'static Health,
    &'static Aim,
    &'static mut Bot,
    &'static mut Intent,
);

/// One pawn as the brain sees it. A struct rather than a tuple because it grew a
/// `team` and "which of these five fields was the bool again" is exactly the
/// sort of thing that silently makes a bot shoot its own side.
#[derive(Copy, Clone, Debug)]
struct Contact {
    handle: usize,
    team: Team,
    pos: Pos,
    stance: u8,
    /// The pawn's [`Aim::stir`] — how much it has been moving. Movement costs
    /// grass concealment, so a runner enters the memory buffer from much
    /// further away than a creeper: bots hunt movement without a line of code
    /// that says so.
    stir: i32,
    alive: bool,
}

/// The most dangerous enemy this bot can see right now, from its own eyes.
///
/// "Most dangerous" is deliberately crude — most exposed, nearest on a tie.
/// Counter-Strike weighs threat by weapon and facing; there is one weapon here,
/// so what's left is who is most shootable.
fn look(
    scenario: &Scenario,
    occluders: &[Occluder],
    me: usize,
    my_team: Team,
    pos: Pos,
    stance: u8,
    others: &[Contact],
) -> Sighting {
    let mut best = Sighting::default();
    let mut best_key = (0i32, i64::MAX, usize::MAX);
    for other in others {
        // Teammates are not threats and are never shot at. This is the only
        // place friend and foe are told apart, deliberately: everything
        // downstream reads the memory buffer, so a teammate that never gets into
        // it can never be aimed at, led, hunted or broken away from.
        if other.handle == me || !other.alive || other.team == my_team {
            continue;
        }
        let exposure = visible_fraction(
            scenario,
            pos,
            stance,
            other.pos,
            other.stance,
            other.stir,
            occluders,
        );
        if exposure == 0 {
            continue;
        }
        let d2 = dist2(pos, other.pos);
        // More exposed wins; nearer breaks that tie; handle breaks that one, so
        // two identically-placed pawns can't flip the answer between peers.
        let key = (exposure, -d2, usize::MAX - other.handle);
        if key > best_key {
            best_key = key;
            best = Sighting {
                who: other.handle as u8,
                pos: other.pos,
                stance: other.stance,
                exposure,
            };
        }
    }
    best
}

/// Score the courses of action and carry out the winner.
/// The state of this bot's gun and its cover, bundled: the three facts the
/// trigger decision needs travel together or not at all.
#[derive(Copy, Clone)]
struct Gun {
    /// The live cone, [`crate::Aim::spread`] — last tick's, which is the right
    /// answer as well as the only available one.
    spread: i32,
    /// The cone a settled, braced, honed hold would have from this stance —
    /// **the shot the bot could take if it planted its feet**, which is what
    /// the SCORING reads (see the note in [`decide`]); only the trigger reads
    /// the live one.
    poised: i32,
    /// Whether this bot still has concealment to lose — see the gate in
    /// [`engage`].
    hidden: bool,
}

fn decide(
    bot: &mut Bot,
    pos: Pos,
    team: Team,
    stance: u8,
    health: &Health,
    gun: Gun,
    friends: &[Pos],
) -> Intent {
    let profile = bot.profile;
    let hp = health.fraction();
    let hurt = not(hp);

    let contact = bot.memory.attended(profile.reaction);
    let stale = bot.memory.last_contact();

    // Facts the scoring reads. All `0..=FP`. Note `worth` is scored on the
    // POISED cone, not the live one, and that is load-bearing: scoring the live
    // spread reads as sound and is a feedback loop — moving widens the cone, a
    // wide cone makes `Act::Push` score higher, and `Push` moves, so once a bot
    // started closing it could never stop, because the reason it could not
    // shoot was that it was walking. What that looks like from above is the
    // whole side breaking into a charge and arriving in a heap — which is how
    // it was found: `bots_do_not_shoot_their_own_side` went from 3 rounds in 60
    // lined up on a teammate to 5 in 30, not because the clearance test got
    // worse but because the bots were standing on top of each other. The
    // settled hold makes `worth` a fact about RANGE and STANCE — the two things
    // closing and getting down actually change — and leaves the live cone the
    // one job it is right for: the trigger, in `engage`.
    let (visible, worth, target) = match contact {
        Some(s) => (
            s.exposure,
            worth_a_round(gun.poised, units(dist(pos, s.pos))),
            Some(s),
        ),
        None => (0, 0, None),
    };
    // Where to go when there is nothing to shoot at: the last place anyone was
    // seen if there is one, otherwise the enemy line. How much that is worth is
    // how far away it still is — a bot that has arrived has nothing left to
    // advance toward and should be looking around instead of walking into a wall.
    let (march, ground_to_make) = match stale {
        // Somebody to go and look at, and the patrol's clock stops: a bot with
        // anything at all to watch is not searching.
        Some(s) => {
            bot.patience = 0;
            (s.pos, FP)
        }
        None => {
            let aim = patrol(bot, team);
            (aim, not(nearness(units(dist(pos, aim)), ARRIVED * 6)))
        }
    };

    // The third consideration on the walk: what walking would cost. It is `hp`
    // only while the bot can ACTUALLY SEE somebody — advancing on a fight in
    // progress while weak is the risk this exists to price. With nobody in
    // sight there is no fight to walk into, and charging `hp` anyway is a stall
    // that has now been found TWICE, at both ends of the same mistake:
    //
    //   * keyed on nothing at all, a lone survivor on 18 hp scored `Hunt` at
    //     `ADVANCE × 0.18 ≈ 23` against `Settle`'s flat 32 and held still for a
    //     measured 84 seconds with nobody in its memory;
    //   * keyed on a STALE contact, two hurt bots 45 units apart — each with a
    //     seconds-old memory of the other and neither able to see anything now,
    //     both prone in grass — both settled and held still for 116 seconds.
    //     A memory is not an enemy you are looking at.
    //
    // Between them these are two of the five distinct causes
    // `a_match_never_stops_moving_around_an_idle_player` has caught. The rule
    // that survives both: a bot avoids a fight it can see, and searches
    // whatever its health.
    let risk = if contact.is_some() { hp } else { FP };
    let cowardice = sharp(hurt) * profile.caution / FP;

    // Every behavior scores on exactly three considerations — see `score`.
    let mut best = (0, Act::Settle);
    for (act, s) in [
        // Shoot what you can see, the more of it the better, from a range the
        // round will still hurt at.
        (Act::Fight, score(FP, sharp(visible), worth, not(cowardice))),
        // Closing is for when you can see them but not well enough to trade,
        // and you're in shape to take the ground.
        (Act::Push, score(profile.aggression, visible, not(worth), hp)),
        // Breaking is the mirror: hurt, and someone has eyes on you.
        (Act::Break, score(profile.caution, cowardice, sharp(visible), FP)),
        // Nothing in sight, but there is always somewhere to go — the enemy
        // line, if nobody has been seen yet.
        //
        // **Weighted by `ADVANCE`, NOT by `aggression`**, and that separation is
        // the whole reason the dial means anything now. It used to be
        // `aggression`, which quietly made one number do two jobs: how hard to
        // push someone you can SEE, and whether to cross the field at all. Under
        // the old respawning arena those never came apart, because the spawns
        // were 300 units from each other and there was nothing to cross.
        //
        // With the sides a field apart it broke in the most misleading way
        // possible: 0.2 aggression beats the 0.5 default in EVERY decisive pair
        // — because it lets the other side do the walking and shoots them as
        // they arrive — and then 0.2 against ITSELF is two teams sitting on
        // their own halves. Measured, after adopting 0.2 as the default and
        // before this split: **330 of 450 rounds drawn**, mean round 101 s of a
        // 120 s clock. A dial whose best setting makes the game stop is a dial
        // wired to the wrong thing.
        //
        // So closing on a visible enemy is aggression, and walking to an
        // objective you have seen nobody at is not — it is just playing.
        (Act::Hunt, score(ADVANCE, not(visible), ground_to_make, risk)),
        // The default. Deliberately weak, so it wins only when nothing else
        // scores at all — a bot that settles while being shot at is a bug.
        //
        // **`FP / 8` is the pace of the whole game and it was measured.** In a
        // field this deep `visible` is a fraction almost everywhere, so
        // `sharp(visible)` on `Fight` leaves this competitive with real
        // behaviours far more often than the "only when nothing else scores"
        // above makes it sound — which is exactly why halving it is such a
        // violent change. Twenty pairs of identical profiles, mean round length
        // net of the intermission:
        //
        //   FP / 8   — 66 s of fighting, 4 of 360 rounds drawn
        //   FP / 16  — 5.5 s, 2 of 360 drawn
        //
        // Both are decisive; only one is a round. At `FP / 16` eight bots wipe
        // each other out before a human could cross the field, because nobody
        // ever holds still and the whole match is one charge. Deep grass plus a
        // long approach is what makes creeping worth anything, and this weight
        // is what buys it. Do not treat it as a spare knob.
        (Act::Settle, score(FP / 8, not(visible), FP, FP)),
    ] {
        // Strictly greater, and the list order is fixed, so ties resolve to the
        // earlier entry identically on every peer.
        if s > best.0 {
            best = (s, act);
        }
    }

    let mut intent = match best.1 {
        Act::Fight => engage(bot, pos, target, stance, gun, friends, true),
        Act::Push => {
            // Already close enough: hold and shoot rather than walk into them.
            // `engage(hold)` roots the pawn, so this is `Act::Fight` in all but
            // name once the standoff is reached — which is the point.
            let inside = target
                .map(|t| units(dist(pos, t.pos)) <= PUSH_STANDOFF)
                .unwrap_or(true);
            let mut intent = engage(bot, pos, target, stance, gun, friends, inside);
            if let (false, Some(t)) = (inside, target) {
                let (dx, dy) = toward(pos, t.pos);
                intent.0.move_x = dx;
                intent.0.move_y = dy;
            }
            intent
        }
        Act::Break => {
            let mut intent = Intent::default();
            if let Some(t) = stale.or(target) {
                let (dx, dy) = toward(t.pos, pos); // away
                intent.0.move_x = dx;
                intent.0.move_y = dy;
            }
            // Down, but not flat while still moving: prone movement is a crawl
            // and being caught crawling in the open is worse than standing.
            intent.0.set_stance(STANCE_CROUCH);
            intent
        }
        Act::Hunt => {
            let mut intent = Intent::default();
            let (dx, dy) = toward(pos, march);
            intent.0.move_x = dx;
            intent.0.move_y = dy;
            intent
        }
        Act::Settle => {
            let mut intent = Intent::default();
            // Get low where the grass is worth using. Prone is close to blind
            // in a deep tile — measured, not guessed: a prone pawn sees a
            // standing one at 0.12 at 40 units and 0.004 at 80 — so it is only
            // worth it when actually hurt. Otherwise crouch, which costs far
            // less sight.
            let want = if hurt > HURT_FRACTION { STANCE_PRONE } else { STANCE_CROUCH };
            intent.0.set_stance(want);
            intent
        }
    };

    // **Going round whatever it has been walking into.** Applied last, over
    // whatever the winning behaviour decided, because being stuck is a fact
    // about the WORLD rather than about the choice: `Act::Hunt` marching at a
    // last known position and `Act::Push` closing on a live one wedge against a
    // boulder identically, and both want the same answer.
    //
    // A quarter turn rather than a reversal, and to a fixed side ([`Bot::veer`]):
    // `push_out_of_cover` cancels only the into-the-rock part of a step, so a
    // step ALONG the obstacle survives in full and the bot slides round instead
    // of bouncing off and coming straight back. Reversing would do exactly that
    // bouncing, and picking a fresh side each tick would jitter on the spot.
    // Never while aiming, for the reason spelled out at the stuck counter: the
    // move vector is also the aim, and a rotated aim is a bot shooting at
    // nothing. `bot.stuck` cannot climb while rooted anyway; this is the belt
    // to that braces.
    if bot.blocked && !intent.0.ads() {
        let (mx, my) = (intent.0.move_x as i32, intent.0.move_y as i32);
        if mx != 0 || my != 0 {
            let side = bot.veer as i32;
            intent.0.move_x = (-my * side).clamp(-127, 127) as i8;
            intent.0.move_y = (mx * side).clamp(-127, 127) as i8;
        }
    }
    intent
}

/// Face a target and (optionally) fire, with Quake III's aim model: the shot
/// goes at where the bot *thinks* they are, which is where they were one
/// reaction time ago, plus a jitter scaled by how inaccurate it is.
fn engage(
    bot: &mut Bot,
    pos: Pos,
    target: Option<Sighting>,
    stance: u8,
    gun: Gun,
    friends: &[Pos],
    hold: bool,
) -> Intent {
    let mut intent = Intent(PlayerInput::default());
    intent.0.set_stance(stance.min(STANCE_PRONE));
    let Some(t) = target else { return intent };

    let mut aim = t.pos;

    // Leading is a TECHNIQUE, gated by skill — not a bonus to accuracy. Below
    // the threshold the bot simply doesn't know to do it and shoots where they
    // were. Velocity comes from the memory buffer, which is already there for
    // the reaction delay.
    if bot.profile.skill >= LEAD_SKILL {
        let older = bot.memory.recall(bot.profile.reaction as usize + LEAD_SPAN);
        if let Some(o) = older.filter(|o| o.who == t.who) {
            let flight = dist(pos, t.pos) / crate::BULLET_SPEED as i64;
            let vx = (t.pos.x - o.pos.x) as i64 / LEAD_SPAN as i64;
            let vy = (t.pos.y - o.pos.y) as i64 / LEAD_SPAN as i64;
            // Only as much of the lead as the bot's skill is worth, so the
            // technique fades in rather than switching on.
            let share = bot.profile.skill.clamp(0, FP) as i64;
            aim.x += (vx * flight * share / FP as i64) as i32;
            aim.y += (vy * flight * share / FP as i64) as i32;
        }
    }

    // Q3's `bestorigin[i] += 20 * crandom() * (1 - aim_accuracy)`, in subunits
    // and scaled by range so the jitter is an ANGLE rather than a fixed offset
    // — otherwise perfect accuracy at 300 units would be easier than at 30.
    //
    // **A bounded random WALK, not a fresh draw every tick — because the
    // barrel is real now.** The offset used to be redrawn from scratch sixty
    // times a second, which was harmless while the aim point was bookkeeping;
    // the moment [`crate::Aim::turn`] started charging for traverse it became a
    // barrel VIBRATING several degrees per tick, and the sim — correctly —
    // priced it: every bot's cone sat wide open, nobody could finish a kill,
    // and with salt 0 all three rounds of `rounds_are_fought_to_a_finish` ran
    // the clock out. Walking the offset in small steps keeps each tick's turn
    // inside [`crate::SWING_FREE`], so a shaky bot pays with MISSES (the offset
    // is still nonzero most of the time) rather than with a saturated cone —
    // and the misses now drift across the target instead of teleporting, which
    // is also what a hand actually does. A real flick — switching targets —
    // still swings the whole facing and still costs; that is the mechanic, not
    // a bug in it.
    let miss = not(bot.profile.accuracy);
    if miss > 0 {
        let span = (units(dist(pos, aim)).max(1) * miss / FP / 3) * FP;
        let step = (span / 6).max(1);
        bot.drift_x = (bot.drift_x + bot.jitter(step)).clamp(-span, span);
        bot.drift_y = (bot.drift_y + bot.jitter(step)).clamp(-span, span);
        aim.x += bot.drift_x;
        aim.y += bot.drift_y;
    }

    let (dx, dy) = toward(pos, aim);
    intent.0.move_x = dx;
    intent.0.move_y = dy;
    if hold {
        // Rooted: the stick still turns the pawn, it just doesn't carry it
        // anywhere. Same bit a player's sights button sets.
        intent.0.buttons |= crate::BTN_ADS;
    }
    // Face them regardless. Three separate things have to be true before the
    // trigger goes, and they fail for different reasons:
    //
    //  * in range at all, or the round is wasted on the damage floor;
    //  * steady enough to be worth a round — `discipline`, and this is the
    //    clause that makes a bot stop and settle instead of firing on the move;
    //  * nobody of its own in the lane.
    //
    // Note the bot goes on HOLDING (the sights bit above is set whatever this
    // decides), so a bot that finds itself too unsteady stands still and gets
    // steady rather than doing nothing. That is the whole behaviour change: it
    // waits out its own sway and then fires, which from outside reads as taking
    // aim.
    let range = units(dist(pos, t.pos));
    // **The discipline gate is AMBUSH PATIENCE, and nothing else.** It took
    // three designs to land here, and the two failures say why this is the
    // shape:
    //
    //   1. A gate on absolute quality only ever passed point-blank (proximity
    //      saturates quality whatever the cone does) and held fire FOREVER at
    //      long range. `discipline=0.0` beat it in 23 of 25 decisive pairs.
    //   2. A gate on quality relative to the best-from-here honed shot fixed
    //      both ends and still lost by +247 elo, and the loss is a theorem,
    //      not a tuning miss: firing costs no ammunition, so once you are in a
    //      fight every withheld positive-expectation shot is a pure loss.
    //      There is NO trigger policy for a visible bot that beats "fire
    //      whenever anything might land", and pretending otherwise just
    //      donates rounds to whoever isn't.
    //
    // What firing DOES cost is concealment ([`Aim::flash`]), and that cost
    // only exists while you have concealment to lose. So the gate runs
    // exactly while the bot is still hidden — buried in its own grass and not
    // lit up by its own recent movement or muzzle — and asks whether this
    // shot is honed enough to be worth breaking cover for, with `discipline`
    // setting the bar as a share of the best shot available from here
    // (`poised`). One round later the flash has spent the concealment, the
    // gate is moot by its own logic, and the bot fires at will like everyone
    // else — which is why the first shot out of an ambush is the aimed one
    // and the burst that follows is not.
    let live = shot_quality(gun.spread, range) as i64;
    let best = shot_quality(gun.poised, range) as i64;
    let worth_breaking_cover = live * FP as i64 >= bot.profile.discipline as i64 * best;
    if range <= ENGAGE_RANGE
        && (worth_breaking_cover || !gun.hidden)
        && !blocked_by_a_friend(pos, aim, gun.spread, friends)
    {
        intent.0.buttons |= BTN_FIRE;
    }
    intent
}

/// Is one of your own standing in the way?
///
/// Tested against the JITTERED aim point rather than the target, because that is
/// the line the round will actually take, and only over the stretch the round
/// actually covers: from where it leaves the barrel to where it is going.
///
/// **The two ends of that stretch are both load-bearing, and getting them from a
/// clamped point-segment distance does not work.** `segment_hits_circle` folds
/// everything before the start of the segment onto the start point, so a
/// teammate standing BEHIND the shooter reads as being on the line at range
/// zero. Since `separate_players` holds pawns exactly `2 * PLAYER_R` (24) apart
/// and the block radius is 26, that made *any* two adjacent teammates jam each
/// other's triggers permanently, whichever way either of them was facing.
/// Measured: three bots stood in a huddle 32 units from a live enemy, all three
/// rooted and aiming, not one of them firing, for the rest of the round. It is
/// the same shape as the two-bots-on-one-subunit deadlock `separate_players`
/// exists for, and it reads the same way from outside — "then nothing happens".
///
/// So the test is an explicit projection along the shot with both ends open:
/// * nearer than the MUZZLE and you are not in front of the barrel at all — the
///   round is born `PLAYER_R + BULLET_R + 2` units out and is already past you;
/// * beyond the aim point and you are behind the target, which is not danger.
fn blocked_by_a_friend(pos: Pos, aim: Pos, spread: i32, friends: &[Pos]) -> bool {
    let muzzle = radius_fp(PLAYER_R + BULLET_R + 2);
    let (dx, dy) = ((aim.x - pos.x) as i64, (aim.y - pos.y) as i64);
    let len = isqrt(dx * dx + dy * dy);
    if len == 0 {
        return false;
    }
    friends.iter().any(|&friend| {
        let (fx, fy) = ((friend.x - pos.x) as i64, (friend.y - pos.y) as i64);
        // How far down the shot they stand, and how far off it.
        let along = (fx * dx + fy * dy) / len;
        let across = (fx * dy - fy * dx).abs() / len;
        // **The lane is as wide as the CONE, not as the aim line**, and it has
        // to be measured where the teammate actually stands rather than at the
        // target: a cone opens with range, so a friend ten units ahead is in far
        // less danger than one at the far end of the same shot.
        //
        // Without this the bots started shooting their own side the day rounds
        // stopped flying straight — `bots_do_not_shoot_their_own_side` went from
        // clean to 5 of 30 rounds lined up on a teammate, because every one of
        // those decisions was made against a line the round no longer took. It
        // is the same arithmetic the fixed [`FRIENDLY_CLEARANCE`] was always
        // doing by hand, now that there is a real number to do it with.
        // Capped: an uncapped flare at a saturated cone reached 57 units at
        // the far end of a long shot, so a teammate nowhere near the line
        // blocked it — measured as four bots firing NINETEEN rounds in a
        // minute, a squad strangling itself. The cap concedes that a wildly
        // sprayed round MIGHT hit a friend well off the line; that risk is
        // priced by the round being wild everywhere, not by never firing.
        let flare = Aim::cone_half_width(spread, units(along)).min(FRIENDLY_CLEARANCE);
        let reach = radius_fp(PLAYER_R + BULLET_R + FRIENDLY_CLEARANCE + flare);
        along > muzzle && along < len && across <= reach
    })
}

/// `r` world units as subunits — the same conversion `radius_fp` does in the
/// sim, repeated here so this file does not need it exported.
fn radius_fp(units: i32) -> i64 {
    units as i64 * FP as i64
}

// ── Small geometry helpers ──────────────────────────────────────────────────

fn dist2(a: Pos, b: Pos) -> i64 {
    let (dx, dy) = ((b.x - a.x) as i64, (b.y - a.y) as i64);
    dx * dx + dy * dy
}

fn dist(a: Pos, b: Pos) -> i64 {
    isqrt(dist2(a, b))
}

/// Subunits to whole world units.
fn units(fp: i64) -> i32 {
    (fp / FP as i64) as i32
}

/// A joystick vector from `from` toward `to`, scaled to full deflection so the
/// pawn moves at its stance's full speed — the sim divides by the longer of
/// `len/127`, so anything shorter is a walk.
fn toward(from: Pos, to: Pos) -> (i8, i8) {
    let (dx, dy) = ((to.x - from.x) as i64, (to.y - from.y) as i64);
    let len = isqrt(dx * dx + dy * dy);
    if len == 0 {
        return (0, 0);
    }
    ((dx * 127 / len) as i8, (dy * 127 / len) as i8)
}

/// Whether a bot in this stance is buried where it stands — the limiting case
/// of the concealment model (grass at your own feet). The brain reads the full
/// [`visible_fraction`]; this is for the tests and the harness.
pub fn is_concealed(scenario: &Scenario, pos: Pos, stance: u8) -> bool {
    scenario.cover(pos.x / FP, pos.y / FP, stance) >= FP
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(who: u8, x: i32) -> Sighting {
        Sighting { who, pos: Pos::from_units(x, 0), stance: STANCE_STAND, exposure: FP }
    }

    /// The delay line returns what was pushed N ticks ago, which is the whole
    /// mechanism: `attended` is how stale the bot's picture of the world is,
    /// and an off-by-one here is a bot that reacts a tick early or late for
    /// every engagement it ever has.
    #[test]
    fn memory_recalls_the_tick_it_was_asked_for() {
        let mut m = Memory::default();
        for tick in 0..MEMORY_TICKS {
            m.push(seen(1, tick as i32));
        }
        // 0 ticks back is the newest.
        assert_eq!(m.recall(0).unwrap().pos.x, Pos::from_units(MEMORY_TICKS as i32 - 1, 0).x);
        for back in 0..MEMORY_TICKS {
            let want = MEMORY_TICKS as i32 - 1 - back as i32;
            assert_eq!(
                m.recall(back).unwrap().pos.x,
                Pos::from_units(want, 0).x,
                "recall({back}) should be the sighting from {back} ticks ago"
            );
        }
    }

    /// A bot that has only just spawned must not "remember" ticks it was never
    /// alive for — otherwise its first engagement is fought against a sighting
    /// at the origin that never happened.
    #[test]
    fn memory_refuses_to_recall_before_it_existed() {
        let mut m = Memory::default();
        assert!(m.recall(0).is_none(), "an empty memory recalled something");
        m.push(seen(1, 5));
        assert!(m.recall(0).is_some());
        assert!(m.recall(1).is_none(), "recalled a tick that never happened");
        assert!(m.attended(10).is_none(), "attended a tick that never happened");
    }

    /// Sightings of nobody are still pushed, so the buffer stays in step with
    /// the tick count — but they must not read as contact.
    #[test]
    fn empty_sightings_advance_time_without_becoming_contact() {
        let mut m = Memory::default();
        m.push(seen(3, 40));
        for _ in 0..5 {
            m.push(Sighting::default());
        }
        assert!(m.attended(0).is_none(), "an empty sighting read as contact");
        assert_eq!(m.attended(5).unwrap().who, 3, "the real sighting moved off its tick");
        // Last contact ignores the gap and finds the real one.
        assert_eq!(m.last_contact().unwrap().who, 3);
    }

    /// Two bots with different handles draw different jitter. Consecutive LCG
    /// seeds would leave them visibly correlated, which is what the hash in
    /// `Bot::new` is for.
    #[test]
    fn bots_with_different_handles_do_not_move_in_lockstep() {
        let (mut a, mut b) = (Bot::new(4, BotProfile::default()), Bot::new(5, BotProfile::default()));
        let draws: Vec<(i32, i32)> = (0..16).map(|_| (a.jitter(100), b.jitter(100))).collect();
        assert!(
            draws.iter().filter(|(x, y)| x == y).count() < 4,
            "two bots drew the same jitter too often: {draws:?}"
        );
    }

    /// **Which friends block a shot, and — the part that bit — which do not.**
    ///
    /// A clamped point-to-segment distance answers this wrongly at both ends,
    /// and the near end is the one that matters: it folds everything behind the
    /// shooter onto the muzzle, so a teammate standing at your shoulder reads as
    /// standing in your line. `separate_players` holds pawns 24 units apart and
    /// the block radius is 26, so that made every adjacent pair of teammates jam
    /// each other permanently — three bots huddled near an enemy, all aiming,
    /// none firing, for the rest of the round.
    #[test]
    fn only_a_friend_actually_in_the_lane_blocks_the_shot() {
        let me = Pos::from_units(0, 0);
        let target = Pos::from_units(200, 0);
        let at = |x: i32, y: i32| vec![Pos::from_units(x, y)];

        assert!(
            blocked_by_a_friend(me, target, 0, &at(100, 0)),
            "a teammate squarely in the lane must block"
        );
        assert!(
            blocked_by_a_friend(me, target, 0, &at(100, PLAYER_R + BULLET_R)),
            "a teammate a body's width off the lane is still in danger"
        );
        assert!(
            !blocked_by_a_friend(me, target, 0, &at(100, 60)),
            "a teammate well clear of the lane must not block"
        );
        // The regression, in both directions along the line.
        assert!(
            !blocked_by_a_friend(me, target, 0, &at(-24, 0)),
            "a teammate BEHIND the shooter blocked the shot — this is the deadlock"
        );
        assert!(
            !blocked_by_a_friend(me, target, 0, &at(-24, 8)),
            "a teammate behind and to one side blocked the shot"
        );
        assert!(
            !blocked_by_a_friend(me, target, 0, &at(260, 0)),
            "a teammate beyond the target is not in the round's way"
        );
        // Right against the muzzle: the round is born 16 units out, so a friend
        // inside that is already behind it.
        assert!(
            !blocked_by_a_friend(me, target, 0, &at(10, 0)),
            "a teammate inside the muzzle offset blocked a round born past them"
        );
        assert!(!blocked_by_a_friend(me, target, 0, &[]), "nobody about, nothing to block");
    }

    /// Two pawns at the minimum separation `separate_players` allows, facing
    /// opposite ways. Neither is in the other's lane, and this is the exact
    /// configuration the old test got wrong.
    #[test]
    fn touching_teammates_do_not_jam_each_other() {
        let a = Pos::from_units(0, 0);
        let b = Pos::from_units(2 * PLAYER_R, 0);
        assert!(!blocked_by_a_friend(a, Pos::from_units(-200, 0), 0, &[b]));
        assert!(!blocked_by_a_friend(b, Pos::from_units(200, 0), 0, &[a]));
    }

    /// **An unsteady shooter needs a wider lane**, because the round is not
    /// going where the aim line points — it is going somewhere in a cone that
    /// opens with range. A teammate 40 units off a 200-unit shot is clear of a
    /// steady gun and squarely inside a saturated one.
    ///
    /// This is the half of `bots_do_not_shoot_their_own_side` that unit-tests:
    /// when rounds started coming out of a cone and this did not, five rounds in
    /// thirty were lined up on somebody's own side.
    #[test]
    fn a_wide_cone_needs_a_wider_lane() {
        let me = Pos::from_units(0, 0);
        let target = Pos::from_units(200, 0);
        // 34 units off the line: outside the steady lane (26), inside the
        // saturated one (26 + the flare, capped at FRIENDLY_CLEARANCE = 38).
        let beside = vec![Pos::from_units(100, 34)];
        assert!(
            !blocked_by_a_friend(me, target, 0, &beside),
            "a steady gun's round goes where it is pointed; 34 units is clear"
        );
        assert!(
            blocked_by_a_friend(me, target, FP, &beside),
            "a saturated cone at 100 units down the line must widen the lane \
             over a teammate 34 units off it"
        );
        // And the widening is by RANGE (up to its cap), not a flat bonus: the
        // same teammate the same distance off the line, but close enough that
        // the cone has not opened yet, is still not in danger.
        let close = vec![Pos::from_units(25, 34)];
        assert!(
            !blocked_by_a_friend(me, Pos::from_units(400, 0), FP, &close),
            "the cone is under 6 units wide at 25; a teammate 34 off is safe"
        );
    }

    /// The same handle always starts from the same seed — a bot is deterministic
    /// from its identity, not from when it happened to be created.
    #[test]
    fn a_bot_is_reproducible_from_its_handle() {
        let mut a = Bot::new(6, BotProfile::default());
        let mut b = Bot::new(6, BotProfile::default());
        for _ in 0..32 {
            assert_eq!(a.rand(), b.rand());
        }
    }
}
