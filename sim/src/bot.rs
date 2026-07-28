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
    isqrt, visible_fraction, Health, Intent, Occluder, Player, PlayerInput, Pos, Scenario,
    Stance, BTN_FIRE, FP, STANCE_CROUCH, STANCE_PRONE, STANCE_STAND,
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
}

/// Above this `skill`, a bot leads a moving target. Quake III's threshold for
/// the same technique, on the same 0..1 scale.
const LEAD_SKILL: i32 = FP * 2 / 5;

impl Default for BotProfile {
    /// A competent but beatable opponent: a quarter-second to react, aim good
    /// enough to punish standing in the open, and no strong lean either way
    /// between pushing and holding.
    fn default() -> Self {
        Self {
            skill: FP * 3 / 5,
            accuracy: FP * 7 / 10,
            reaction: 15,
            aggression: FP / 2,
            caution: FP / 2,
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
        // Hashed rather than `BOT_SEED + handle`: consecutive LCG seeds produce
        // visibly correlated first draws, and "all the bots twitch together" is
        // exactly the tell that would give it away.
        let mut seed = (handle as u32).wrapping_mul(0x9E37_79B9) ^ BOT_SEED;
        seed ^= seed >> 15;
        Self { seed: seed | 1, profile, memory: Memory::default() }
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
    pawns: Query<(&Player, &Pos, &Stance, &Health)>,
    rocks: Query<(&crate::Rock, &Pos)>,
    bushes: Query<(&crate::Bush, &Pos)>,
    mut bots: Query<(&Player, &Pos, &Stance, &Health, &mut Bot, &mut Intent)>,
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
    let mut others: Vec<(usize, Pos, u8, bool)> = pawns
        .iter()
        .map(|(player, pos, stance, health)| {
            (player.handle, *pos, stance.level, health.alive())
        })
        .collect();
    others.sort_unstable_by_key(|&(handle, ..)| handle);

    for (player, pos, stance, health, mut bot, mut intent) in &mut bots {
        // The dead do nothing. Still push an empty sighting so the memory keeps
        // advancing in step with the tick count — otherwise "12 ticks ago"
        // would mean something different after every respawn.
        if !health.alive() {
            bot.memory.push(Sighting::default());
            *intent = Intent::default();
            continue;
        }

        let seen = look(&scenario, &occluders, player.handle, *pos, stance.level, &others);
        bot.memory.push(seen);
        *intent = decide(&mut bot, *pos, stance.level, health);
    }
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
    pos: Pos,
    stance: u8,
    others: &[(usize, Pos, u8, bool)],
) -> Sighting {
    let mut best = Sighting::default();
    let mut best_key = (0i32, i64::MAX, usize::MAX);
    for &(handle, their_pos, their_stance, alive) in others {
        if handle == me || !alive {
            continue;
        }
        let exposure = visible_fraction(scenario, pos, stance, their_pos, their_stance, occluders);
        if exposure == 0 {
            continue;
        }
        let d2 = dist2(pos, their_pos);
        // More exposed wins; nearer breaks that tie; handle breaks that one, so
        // two identically-placed pawns can't flip the answer between peers.
        let key = (exposure, -d2, usize::MAX - handle);
        if key > best_key {
            best_key = key;
            best = Sighting {
                who: handle as u8,
                pos: their_pos,
                stance: their_stance,
                exposure,
            };
        }
    }
    best
}

/// Score the courses of action and carry out the winner.
fn decide(
    bot: &mut Bot,
    pos: Pos,
    stance: u8,
    health: &Health,
) -> Intent {
    let profile = bot.profile;
    let hp = health.fraction();
    let hurt = not(hp);

    let contact = bot.memory.attended(profile.reaction);
    let stale = bot.memory.last_contact();

    // Facts the scoring reads. All `0..=FP`.
    let (visible, near, target) = match contact {
        Some(s) => (
            s.exposure,
            nearness(units(dist(pos, s.pos)), ENGAGE_RANGE),
            Some(s),
        ),
        None => (0, 0, None),
    };
    let has_stale = if stale.is_some() { FP } else { 0 };
    let cowardice = sharp(hurt) * profile.caution / FP;

    // Every behavior scores on exactly three considerations — see `score`.
    let mut best = (0, Act::Settle);
    for (act, s) in [
        // Shoot what you can see, the more of it the better, from a range the
        // round will still hurt at.
        (Act::Fight, score(FP, sharp(visible), near, not(cowardice))),
        // Closing is for when you can see them but not well enough to trade,
        // and you're in shape to take the ground.
        (Act::Push, score(profile.aggression, visible, not(near), hp)),
        // Breaking is the mirror: hurt, and someone has eyes on you.
        (Act::Break, score(profile.caution, cowardice, sharp(visible), FP)),
        // Nothing in sight but somewhere to go.
        (Act::Hunt, score(profile.aggression, not(visible), has_stale, hp)),
        // The default. Deliberately weak, so it wins only when nothing else
        // scores at all — a bot that settles while being shot at is a bug.
        (Act::Settle, score(FP / 8, not(visible), FP, FP)),
    ] {
        // Strictly greater, and the list order is fixed, so ties resolve to the
        // earlier entry identically on every peer.
        if s > best.0 {
            best = (s, act);
        }
    }

    match best.1 {
        Act::Fight => engage(bot, pos, target, stance, true),
        Act::Push => {
            let mut intent = engage(bot, pos, target, stance, false);
            if let Some(t) = target {
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
            if let Some(t) = stale {
                let (dx, dy) = toward(pos, t.pos);
                intent.0.move_x = dx;
                intent.0.move_y = dy;
            }
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
    }
}

/// Face a target and (optionally) fire, with Quake III's aim model: the shot
/// goes at where the bot *thinks* they are, which is where they were one
/// reaction time ago, plus a jitter scaled by how inaccurate it is.
fn engage(bot: &mut Bot, pos: Pos, target: Option<Sighting>, stance: u8, hold: bool) -> Intent {
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
    let miss = not(bot.profile.accuracy);
    if miss > 0 {
        let span = (units(dist(pos, aim)).max(1) * miss / FP / 3) * FP;
        aim.x += bot.jitter(span);
        aim.y += bot.jitter(span);
    }

    let (dx, dy) = toward(pos, aim);
    intent.0.move_x = dx;
    intent.0.move_y = dy;
    if hold {
        // Rooted: the stick still turns the pawn, it just doesn't carry it
        // anywhere. Same bit a player's sights button sets.
        intent.0.buttons |= crate::BTN_ADS;
    }
    if units(dist(pos, t.pos)) <= ENGAGE_RANGE {
        intent.0.buttons |= BTN_FIRE;
    }
    intent
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
