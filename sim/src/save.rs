//! The match, written down: capture the world to a blob, put it back.
//!
//! This exists so a player who closes the tab can come back to the match they
//! left. Two very different paths want the identical bytes:
//!
//! * **From storage.** The client writes the blob to `localStorage` every few
//!   ticks. Refresh, and an offline match — bots, health, round clock, series
//!   score — is exactly where it was. This is the only path that works when
//!   there is nobody else to ask.
//! * **From the other peers.** In a room the stored copy is stale by however
//!   long the reload took, so it is worth nothing: what counts is the world the
//!   people still playing are in. A returning peer is handed a blob captured
//!   live and every peer in the match restarts its session from that same blob
//!   — see `client/src/net.rs`.
//!
//! # Why a restart, and not a "join"
//!
//! GGRS fixes its player list when the session is built and has no join-in-
//! progress. A returning browser tab is a NEW matchbox peer with a new
//! `PeerId`, so it cannot be the peer the old session is addressing — there is
//! no seat to slide back into. The only honest move is for **everybody** to
//! rebuild a session from an agreed world at frame 0, which is precisely what
//! the warmup-to-p2p swap already does; a rejoin is that same manoeuvre with a
//! world handed to it instead of a fresh one.
//!
//! That makes this blob a **determinism boundary**. Every peer restores the
//! same bytes and must land on byte-identical worlds, or the very first tick of
//! the resumed session is a desync. Hence:
//!
//! * Integers only, ASCII only, no floats and no maps — the crate's rule, and
//!   here it is also the wire format.
//! * Pawns are written **in handle order**, so capturing the same world twice
//!   gives the same string. Query iteration order is archetype order and is not
//!   a determinism guarantee anywhere else in this sim either.
//! * The version tag is checked on the way in. A blob left in `localStorage` by
//!   an older build is rejected outright rather than half-read into a world
//!   that is subtly wrong — bump [`FORMAT`] whenever a field moves.
//!
//! # What is deliberately NOT in it
//!
//! * **The rock and bush fields.** Both are pure functions of fixed seeds, so
//!   restoring them means calling the same layout code the world was built
//!   with. Writing 200 boulders into every save to no purpose.
//! * **Rounds in the air.** They are dropped, exactly as a round boundary drops
//!   them ([`crate::round`]). A resume is a discontinuity however carefully it
//!   is done, and a restored bullet would also arrive with none of the
//!   render-side state (`MuzzleLift`) the client attaches when it is fired, so
//!   it would draw out of someone's boots for the second it was alive.
//! * **A bot's mind.** `Bot` carries an RNG seed and 24 ticks of sightings, and
//!   both are rebuilt from the handle by [`Bot::seeded`] instead of being
//!   shipped. That is a real cost — the bots forget what they had seen, which
//!   is about a quarter of a second of reaction — and it is worth it: seed and
//!   memory are reconstructed identically on every peer from a number they all
//!   already have, so there is one less thing in the blob that could disagree.

use bevy::prelude::*;
use bevy_ggrs::AddRollbackCommandExtension;

use crate::{
    bush_layout, rock_layout, Aim, Bot, BotRoster, Cooldown, Deaths, Facing, Health, Intent, Kills,
    Phase, Player, Pos, Round, Stance, Team, Winner, MAX_PLAYERS, STANCE_PRONE, TEAM_COUNT,
};

/// Blob format version. Bump on any field change: [`Save::decode`] refuses
/// anything else, which is what stops a stale `localStorage` entry from a
/// previous build being read as a world.
pub const FORMAT: u32 = 2;

const TAG: &str = "army-ghosts-save";

/// The menu settings the world was running under.
///
/// Not sim state — they live in the client's menu and reach the sim through the
/// input stream — but a save without them is a broken save. Restore an arena of
/// five bots into a client whose bot dial reads zero and `reconcile_bots` will
/// correctly, immediately, delete all five: the dial is the source of truth for
/// the count and the world is only its consequence.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Dials {
    /// Bots asked for, `0..=MAX_PLAYERS`.
    pub bots: u8,
    /// Aggression dial POSITION (`0` = not asking), not the `0..=FP` value.
    pub aggro: u8,
}

/// One pawn, flattened. Every field is a component the sim rolls back; nothing
/// derived and nothing render-side.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PawnSave {
    pub handle: usize,
    /// Whether the sim drives this one. A human pawn whose player is absent is
    /// still a human pawn — it just has nobody sending it inputs.
    pub bot: bool,
    pub team: u8,
    pub x: i32,
    pub y: i32,
    pub facing_x: i32,
    pub facing_y: i32,
    pub cooldown: u16,
    /// How steady the hold was, how much recoil was owed, and the gun's own
    /// dice — see [`crate::Aim`]. Carried rather than rebuilt because a player
    /// who reloads the page mid-burst must not come back with a clean first
    /// shot: the whole accuracy model is about what you were doing a second
    /// ago, and a rejoin is exactly a second ago.
    pub sway: i32,
    pub bloom: i32,
    /// Recent movement and recent aim traverse — carried for the same reason as
    /// the other two: what you were doing a second ago is exactly what a rejoin
    /// resumes into, and a runner must not come back as still as a statue.
    pub stir: i32,
    pub swing: i32,
    pub aim_seed: u32,
    pub stance_level: u8,
    pub stance_change: u16,
    pub hp: i32,
    pub down: u16,
    pub hurt: u16,
    pub deaths: u32,
    pub kills: u32,
}

/// A whole match, at one instant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Save {
    pub dials: Dials,
    pub round: Round,
    /// In handle order — see the module doc on why that is load-bearing.
    pub pawns: Vec<PawnSave>,
}

impl Save {
    /// How many pawns the sim drives itself. What the bot dial has to be set to
    /// for [`crate::reconcile_bots`] to leave the restored world alone.
    pub fn bot_count(&self) -> usize {
        self.pawns.iter().filter(|p| p.bot).count()
    }

    /// Human pawns, i.e. how many session seats the restored world expects.
    pub fn human_count(&self) -> usize {
        self.pawns.len() - self.bot_count()
    }

    /// Is this handle's pawn still standing? What "put me back if I am alive"
    /// asks, and the client asks it before deciding whether the pawn it is
    /// about to restore is one you can play.
    pub fn alive(&self, handle: usize) -> bool {
        self.pawns.iter().any(|p| p.handle == handle && p.down == 0)
    }
}

/// `Phase` as a small integer. `Over` carries a winner, so the codes run
/// `Live`, `Over(Draw)`, then one per side.
fn phase_code(phase: Phase) -> u32 {
    match phase {
        Phase::Live => 0,
        Phase::Over(Winner::Draw) => 1,
        Phase::Over(Winner::Team(side)) => 2 + side.min(TEAM_COUNT as u8 - 1) as u32,
    }
}

fn phase_from_code(code: u32) -> Option<Phase> {
    match code {
        0 => Some(Phase::Live),
        1 => Some(Phase::Over(Winner::Draw)),
        n if (n as usize) < 2 + TEAM_COUNT => Some(Phase::Over(Winner::Team((n - 2) as u8))),
        _ => None,
    }
}

// ── Writing ─────────────────────────────────────────────────────────────────

impl Save {
    /// The blob. One line for the header, one for the round, one per pawn — a
    /// few hundred bytes for a full arena, which is what makes it cheap enough
    /// to write to storage several times a second and to hand a rejoining peer
    /// in a single reliable packet.
    pub fn encode(&self) -> String {
        use std::fmt::Write;
        let mut out = String::with_capacity(64 + 80 * self.pawns.len());
        let _ = writeln!(out, "{TAG} {FORMAT} {} {}", self.dials.bots, self.dials.aggro);
        let _ = write!(
            out,
            "round {} {} {}",
            self.round.number,
            phase_code(self.round.phase),
            self.round.ticks
        );
        for wins in self.round.wins {
            let _ = write!(out, " {wins}");
        }
        out.push('\n');
        for p in &self.pawns {
            let _ = writeln!(
                out,
                "pawn {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
                p.handle,
                u8::from(p.bot),
                p.team,
                p.x,
                p.y,
                p.facing_x,
                p.facing_y,
                p.cooldown,
                p.sway,
                p.bloom,
                p.stir,
                p.swing,
                p.aim_seed,
                p.stance_level,
                p.stance_change,
                p.hp,
                p.down,
                p.hurt,
                p.deaths,
                p.kills,
            );
        }
        out
    }
}

/// Pull the live world into a blob.
///
/// Takes `&mut World` rather than a `Query` on purpose: the component list is
/// the sim's business, and threading ten `&'static` types through a public type
/// alias so the client can name them would export exactly the coupling this is
/// meant to avoid. The client calls it from an ordinary exclusive system.
///
/// **Not a rollback system.** It runs in `Update`, off whatever the last
/// simulated tick left behind, and it never writes to the world. A save taken
/// from a predicted frame that later rolls back is a save of a world that
/// almost happened, which for this purpose is indistinguishable from one that
/// did — nobody is going to notice the eight subunits.
pub fn capture(world: &mut World, dials: Dials) -> Save {
    let round = world.get_resource::<Round>().copied().unwrap_or_default();
    let mut query = world.query::<(
        &Player,
        &Team,
        &Pos,
        &Facing,
        &Cooldown,
        &Aim,
        &Stance,
        &Health,
        &Deaths,
        &Kills,
        Option<&Bot>,
    )>();
    let mut pawns: Vec<PawnSave> = query
        .iter(world)
        .map(
            |(player, team, pos, facing, cooldown, aim, stance, health, deaths, kills, bot)| {
                PawnSave {
                    handle: player.handle,
                    bot: bot.is_some(),
                    team: team.0,
                    x: pos.x,
                    y: pos.y,
                    facing_x: facing.x,
                    facing_y: facing.y,
                    cooldown: cooldown.0,
                    sway: aim.sway,
                    bloom: aim.bloom,
                    stir: aim.stir,
                    swing: aim.swing,
                    aim_seed: aim.seed(),
                    stance_level: stance.level,
                    stance_change: stance.change,
                    hp: health.hp,
                    down: health.down,
                    hurt: health.hurt,
                    deaths: deaths.0,
                    kills: kills.0,
                }
            },
        )
        .collect();
    pawns.sort_unstable_by_key(|p| p.handle);
    Save { dials, round, pawns }
}

// ── Reading ─────────────────────────────────────────────────────────────────

/// Whitespace-separated integer reader. The blob is written a line at a time
/// for legibility in a log, but read as one flat token stream — newlines carry
/// no meaning, so a packet that arrived with its line endings mangled still
/// parses.
struct Tokens<'a>(std::str::SplitWhitespace<'a>);

impl Tokens<'_> {
    fn word(&mut self) -> Option<&str> {
        self.0.next()
    }
    fn num<T: std::str::FromStr>(&mut self) -> Option<T> {
        self.0.next()?.parse().ok()
    }
    /// A keyword that must be there, e.g. the `pawn` at the head of a row.
    fn keyword(&mut self, expected: &str) -> Option<()> {
        (self.0.next()? == expected).then_some(())
    }
}

impl Save {
    /// Parse a blob, or `None` if it is anything other than a save this build
    /// wrote.
    ///
    /// Every failure returns `None` rather than a partial world: this is fed
    /// from `localStorage` (which any user can edit) and from the network
    /// (where any peer can), so it is a parser of hostile input and there is no
    /// such thing as a half-restored match. Values are clamped on the way in
    /// for the same reason — a handle past [`MAX_PLAYERS`] or a stance of 9
    /// must not index anything.
    pub fn decode(text: &str) -> Option<Save> {
        let mut t = Tokens(text.split_whitespace());
        t.keyword(TAG)?;
        if t.num::<u32>()? != FORMAT {
            return None;
        }
        let dials = Dials {
            bots: t.num::<u8>()?.min(MAX_PLAYERS as u8),
            aggro: t.num()?,
        };

        t.keyword("round")?;
        let number = t.num()?;
        let phase = phase_from_code(t.num()?)?;
        let ticks = t.num()?;
        let mut wins = [0u32; TEAM_COUNT];
        for slot in wins.iter_mut() {
            *slot = t.num()?;
        }

        let mut pawns = Vec::new();
        while let Some(word) = t.word() {
            if word != "pawn" {
                return None;
            }
            pawns.push(PawnSave {
                handle: t.num::<usize>()?.min(MAX_PLAYERS - 1),
                bot: t.num::<u8>()? != 0,
                team: t.num::<u8>()?.min(TEAM_COUNT as u8 - 1),
                x: t.num()?,
                y: t.num()?,
                facing_x: t.num()?,
                facing_y: t.num()?,
                cooldown: t.num()?,
                sway: t.num()?,
                bloom: t.num()?,
                stir: t.num()?,
                swing: t.num()?,
                aim_seed: t.num()?,
                stance_level: t.num::<u8>()?.min(STANCE_PRONE),
                stance_change: t.num()?,
                hp: t.num()?,
                down: t.num()?,
                hurt: t.num()?,
                deaths: t.num()?,
                kills: t.num()?,
            });
        }
        // Two pawns on one handle would give `Bullet::owner`, the scoreboard and
        // the input stream two pawns to mean, and the duplicate is exactly what
        // a hand-edited blob produces.
        let mut handles: Vec<usize> = pawns.iter().map(|p| p.handle).collect();
        handles.sort_unstable();
        handles.dedup();
        if handles.len() != pawns.len() {
            return None;
        }
        pawns.sort_unstable_by_key(|p| p.handle);

        Some(Save { dials, round: Round { number, phase, ticks, wins }, pawns })
    }
}

/// Build the world the blob describes: every pawn as it stood, the round clock
/// where it was, and the cover field rebuilt from its seeds.
///
/// The caller is responsible for the world being EMPTY first — this spawns, it
/// does not reconcile — and for the session being rebuilt around it. In the
/// client both are `net.rs`'s job, which tears the old world down one frame
/// earlier for reasons that have nothing to do with saving (bevy_ggrs needs a
/// session-less tick to reset its frame counter).
///
/// Only the arena is restorable. The measuring rig is a fixed scene reached by
/// a URL and there is nothing in it worth carrying across a refresh.
pub fn restore(commands: &mut Commands, save: &Save, roster: &BotRoster) {
    commands.insert_resource(save.round);
    for p in &save.pawns {
        let mut pawn = commands.spawn((
            Player { handle: p.handle },
            Team(p.team),
            // Blank, and refilled before anything reads it: `read_human_intent`
            // and `bot_think` both run at the head of the tick. A pawn whose
            // player is not in the session keeps this blank input, which now
            // means "asking for nothing" — so it stands exactly where the blob
            // put it instead of climbing to its feet. See `PlayerInput`.
            Intent::default(),
            Pos { x: p.x, y: p.y },
            Facing { x: p.facing_x, y: p.facing_y },
            Cooldown(p.cooldown),
            // Clamped and de-zeroed on the way in by `from_parts`: this is a
            // parser of hostile input, and a zero seed is an LCG fixed point.
            Aim::from_parts(p.sway, p.bloom, p.stir, p.swing, p.aim_seed),
            Stance { level: p.stance_level, change: p.stance_change },
            Health { hp: p.hp, down: p.down, hurt: p.hurt },
            Deaths(p.deaths),
            Kills(p.kills),
        ));
        pawn.add_rollback();
        if p.bot {
            // Rebuilt from the handle rather than carried in the blob, so every
            // peer's bots come back identical without a byte on the wire — at
            // the price of the sightings they had. Module doc has the argument.
            pawn.insert(Bot::seeded(p.handle, roster.profile(p.handle), roster.salt));
        }
    }
    for (x, y, rock) in rock_layout() {
        commands.spawn((rock, Pos::from_units(x, y))).add_rollback();
    }
    for (x, y, bush) in bush_layout() {
        commands.spawn((bush, Pos::from_units(x, y))).add_rollback();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Save {
        Save {
            dials: Dials { bots: 5, aggro: 7 },
            round: Round {
                number: 4,
                phase: Phase::Over(Winner::Team(1)),
                ticks: 91,
                wins: [2, 1],
            },
            pawns: vec![
                PawnSave {
                    handle: 0,
                    bot: false,
                    team: 0,
                    x: -1234,
                    y: 5678,
                    facing_x: -127,
                    facing_y: 42,
                    cooldown: 3,
                    sway: 118,
                    bloom: 40,
                    stir: 200,
                    swing: 31,
                    aim_seed: 0xDEAD_BEEF,
                    stance_level: STANCE_PRONE,
                    stance_change: 11,
                    hp: 37,
                    down: 0,
                    hurt: 4,
                    deaths: 2,
                    kills: 6,
                },
                PawnSave {
                    handle: 3,
                    bot: true,
                    team: 1,
                    x: 900,
                    y: -900,
                    facing_x: 127,
                    facing_y: 0,
                    cooldown: 0,
                    sway: 0,
                    bloom: 0,
                    stir: 0,
                    swing: 0,
                    aim_seed: 12_345,
                    stance_level: 1,
                    stance_change: 0,
                    hp: 0,
                    down: 240,
                    hurt: 0,
                    deaths: 1,
                    kills: 0,
                },
            ],
        }
    }

    /// The blob is the format two peers agree a world in. If it doesn't survive
    /// a round trip exactly, they are agreeing on different worlds.
    #[test]
    fn a_save_survives_the_round_trip() {
        let save = sample();
        let text = save.encode();
        println!("{text}");
        assert_eq!(Save::decode(&text).as_ref(), Some(&save));
        // …and re-encoding is byte-identical, which is what makes the blob
        // itself comparable — the rejoin handshake leans on that.
        assert_eq!(Save::decode(&text).unwrap().encode(), text);
    }

    /// Every phase has to survive, including the two that only exist for a few
    /// seconds between rounds. A draw read back as a win would hand somebody a
    /// round they didn't take.
    #[test]
    fn every_round_phase_survives() {
        for phase in [
            Phase::Live,
            Phase::Over(Winner::Draw),
            Phase::Over(Winner::Team(0)),
            Phase::Over(Winner::Team(1)),
        ] {
            let mut save = sample();
            save.round.phase = phase;
            let back = Save::decode(&save.encode()).expect("decode");
            assert_eq!(back.round.phase, phase);
        }
    }

    /// This parser is fed from `localStorage` and from the network, so both a
    /// bored user and a hostile peer can write its input. Nothing it is handed
    /// may produce a half-built world or an out-of-range index.
    #[test]
    fn nothing_malformed_becomes_a_world() {
        let good = sample().encode();
        for bad in [
            "".to_string(),
            "nonsense".to_string(),
            // Another build's format.
            good.replace(&format!("{TAG} {FORMAT}"), &format!("{TAG} {}", FORMAT + 1)),
            // Truncated mid-pawn: the commonest real corruption, a storage
            // write that didn't finish.
            good[..good.len() - 12].to_string(),
            // A phase code that names a team that doesn't exist.
            good.replace("round 4 3", "round 4 99"),
            // Two pawns claiming one handle.
            format!("{good}{}", good.lines().last().unwrap()),
        ] {
            assert!(Save::decode(&bad).is_none(), "accepted a bad save: {bad:?}");
        }

        // Values past the end of their tables are clamped, not rejected — a
        // peer sending handle 900 is a bug, but it must not index anything.
        let wild = sample()
            .encode()
            .replace("pawn 3 1 1", "pawn 900 1 9")
            .replace(" 1 0 0 240", " 9 0 0 240");
        let back = Save::decode(&wild).expect("clamped, not rejected");
        assert!(back.pawns.iter().all(|p| p.handle < MAX_PLAYERS));
        assert!(back.pawns.iter().all(|p| (p.team as usize) < TEAM_COUNT));
        assert!(back.pawns.iter().all(|p| p.stance_level <= STANCE_PRONE));
    }

    /// Pawns come out in handle order however they went in, because two peers
    /// comparing blobs have to be comparing the same string.
    #[test]
    fn pawns_are_written_in_handle_order() {
        let mut save = sample();
        save.pawns.reverse();
        let back = Save::decode(&save.encode()).expect("decode");
        let handles: Vec<usize> = back.pawns.iter().map(|p| p.handle).collect();
        assert_eq!(handles, vec![0, 3]);
    }

    /// The counts the client steers by: how many seats the session needs, what
    /// the bot dial has to say, and whether the player coming back is still in
    /// the fight.
    #[test]
    fn a_save_reports_who_is_in_it() {
        let save = sample();
        assert_eq!(save.bot_count(), 1);
        assert_eq!(save.human_count(), 1);
        assert!(save.alive(0), "handle 0 is on its feet");
        assert!(!save.alive(3), "handle 3 is out of the round");
        assert!(!save.alive(7), "nobody is at handle 7 at all");
    }
}
