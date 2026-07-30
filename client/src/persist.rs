//! Coming back: who you are between page loads, and what you were in the middle
//! of.
//!
//! Two pieces, and they are independent of each other.
//!
//! # Identity
//!
//! A browser tab that reloads is, to matchbox, a stranger: the signaling server
//! hands out a fresh `PeerId` per connection, so nothing about the socket
//! survives a refresh. [`Identity`] is the thing that does — an opaque token
//! written once to `localStorage` and re-read on every load. It is what lets the
//! other peers recognise a returning player as the one who was at handle 3
//! rather than as an eighth arrival, and it is the only reason a rejoin can put
//! you back in your own pawn.
//!
//! It is deliberately NOT a name, a login or anything a player picks. It says
//! "same browser as before" and nothing else. Clearing site data is quitting.
//!
//! # The stored match
//!
//! [`autosave`] writes the sim's blob (`army_ghosts_sim::save`) to storage a few
//! times a second. On the next load the client reads it back and the world it
//! describes is what you resume into.
//!
//! Storage is the ONLY path that works offline, and offline it is the whole
//! feature — refresh a solo match and the bots, the health, the round clock and
//! the series score are all where you left them. **In a room it is a fallback
//! and a first paint, nothing more**: whatever is in storage is stale by however
//! long the page took to load, and the people still playing have the real world.
//! See `net.rs` for how it is superseded.
//!
//! ## Why an exclusive system
//!
//! `save::capture` takes `&mut World` so the sim can keep its component list to
//! itself, and this runs in `Update` — never in `GgrsSchedule`. A save is a
//! read of whatever the last simulated tick left behind; taking one inside the
//! rollback schedule would make it rollback state, which is a thing that must
//! never happen to something whose whole job is to outlive the process.

use bevy::prelude::*;
use army_ghosts_sim::save::{self, Dials, Save};

use crate::menu::{Aggression, BotCount};
use crate::LaunchConfig;

/// How stale a stored match may be and still be worth resuming into, in
/// seconds. Long enough to survive a crash, a phone call or a browser
/// reloading a suspended tab; short enough that opening the game tomorrow is a
/// new game rather than a resurrection of one you had forgotten about.
pub const RESUME_WINDOW_SECS: f64 = 20.0 * 60.0;

/// Frames between writes. Three times a second: cheap enough not to think
/// about, and it bounds how much of a match a hard crash can cost to a third of
/// a second of walking.
const SAVE_EVERY: u32 = 20;

/// Storage keys. The match is stored per ROOM, so resuming an offline game and
/// resuming a particular room's game don't overwrite each other — and so
/// arriving at a room you have never played does not restore somebody else's
/// match into it.
const IDENTITY_KEY: &str = "army-ghosts.player";
fn match_key(room: Option<&str>) -> String {
    match room {
        Some(room) => format!("army-ghosts.match.{room}"),
        None => "army-ghosts.match.offline".to_string(),
    }
}

/// This browser's stable id, and the room it belongs to.
#[derive(Resource, Debug, Clone)]
pub struct Identity {
    pub player: String,
    key: String,
}

impl Identity {
    /// Read the token, or mint one. Called once at startup, before the app is
    /// built, so the value is available to the lobby from its first frame.
    pub fn load(launch: &LaunchConfig) -> Identity {
        let player = read(IDENTITY_KEY)
            .filter(|token| is_token(token))
            .unwrap_or_else(|| {
                let minted = mint();
                write(IDENTITY_KEY, &minted);
                minted
            });
        Identity { player, key: match_key(launch.room.as_deref()) }
    }
}

/// Tokens travel in the lobby's start message, which is a comma-and-at-sign
/// separated line, so they are restricted to hex and checked on the way in
/// from storage as well as off the wire. A player id that could contain a comma
/// could rewrite the roster.
pub fn is_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 32
        && token.bytes().all(|b| b.is_ascii_hexdigit())
}

/// How much of a match a client is willing to carry from one page load to the
/// next, with the age it was written at.
pub struct Stored {
    pub save: Save,
    pub age_secs: f64,
}

/// The stored match for this room, if there is a fresh one.
///
/// Anything unparseable, from another build, or older than
/// [`RESUME_WINDOW_SECS`] comes back as `None` — the caller then builds a fresh
/// world, which is what happens today and is never wrong, only disappointing.
pub fn stored_match(launch: &LaunchConfig) -> Option<Stored> {
    let raw = read(&match_key(launch.room.as_deref()))?;
    let (stamp, blob) = raw.split_once('\n')?;
    let age_secs = (now_ms() - stamp.parse::<f64>().ok()?) / 1000.0;
    // A negative age means the clock moved backwards between writes (a device
    // correcting its time, most often). Treat it as fresh rather than as
    // twenty minutes stale: the blob is still the last thing this browser saw.
    if age_secs > RESUME_WINDOW_SECS {
        return None;
    }
    Some(Stored { save: Save::decode(blob)?, age_secs: age_secs.max(0.0) })
}

/// Write the world down, a few times a second, for as long as there is one.
///
/// The dials go in with it and that is not a detail: restore an arena of five
/// bots into a client whose bot dial reads zero and `reconcile_bots` will
/// correctly delete all five over the next five ticks. The dial is the source
/// of truth for the count; the world is only its consequence.
pub fn autosave(world: &mut World) {
    let mut countdown = world.get_resource_or_insert_with(SaveClock::default).clone();
    countdown.frames += 1;
    if countdown.frames < SAVE_EVERY {
        world.insert_resource(countdown);
        return;
    }
    countdown.frames = 0;
    world.insert_resource(countdown);

    let dials = Dials {
        bots: world.get_resource::<BotCount>().map_or(0, |b| b.0 as u8),
        aggro: world.get_resource::<Aggression>().map_or(0, |a| a.0),
    };
    let Some(key) = world.get_resource::<Identity>().map(|i| i.key.clone()) else { return };
    let save = save::capture(world, dials);
    // An empty world is not a match to come back to; it is the half-frame
    // between tearing a session down and building the next one, and writing it
    // would replace a perfectly good stored match with nothing.
    if save.pawns.is_empty() {
        return;
    }
    write(&key, &format!("{}\n{}", now_ms(), save.encode()));
}

#[derive(Resource, Default, Clone)]
struct SaveClock {
    frames: u32,
}

// ── The two platforms ───────────────────────────────────────────────────────
//
// Web is the real target and `localStorage` is the real store. The native side
// exists so the whole path — mint an id, write a match, restart the process,
// read it back — can be exercised by a shell script instead of a browser, which
// is what `tools/rejoin-test.sh` does. `AG_STATE_DIR` points it somewhere
// disposable; `AG_PLAYER_ID` overrides the token outright, which is how a test
// makes a second process claim to be the first one coming back.

#[cfg(target_arch = "wasm32")]
mod backend {
    pub fn storage() -> Option<web_sys::Storage> {
        web_sys::window()?.local_storage().ok().flatten()
    }

    pub fn read(key: &str) -> Option<String> {
        storage()?.get_item(key).ok().flatten()
    }

    pub fn write(key: &str, value: &str) {
        // A quota error (private browsing, a full disk) is not worth a crash:
        // the game plays perfectly well without being able to remember itself.
        if let Some(storage) = storage() {
            let _ = storage.set_item(key, value);
        }
    }

    #[allow(dead_code)]
    pub fn remove(key: &str) {
        if let Some(storage) = storage() {
            let _ = storage.remove_item(key);
        }
    }

    pub fn now_ms() -> f64 {
        js_sys::Date::now()
    }

    /// 64 bits of `Math.random`, hex. Not a UUID — nothing reads it as one, and
    /// pulling in a v4 generator would drag another getrandom backend into the
    /// wasm graph for a value that only has to differ from other people's.
    pub fn mint() -> String {
        let hi = (js_sys::Math::random() * u32::MAX as f64) as u32;
        let lo = (js_sys::Math::random() * u32::MAX as f64) as u32;
        format!("{hi:08x}{lo:08x}")
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod backend {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn dir() -> PathBuf {
        std::env::var("AG_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                PathBuf::from(home).join(".army-ghosts")
            })
    }

    fn path(key: &str) -> PathBuf {
        dir().join(key.replace(['/', '\\'], "_"))
    }

    pub fn read(key: &str) -> Option<String> {
        if key == super::IDENTITY_KEY {
            if let Ok(forced) = std::env::var("AG_PLAYER_ID") {
                if !forced.is_empty() {
                    return Some(forced);
                }
            }
        }
        std::fs::read_to_string(path(key)).ok()
    }

    pub fn write(key: &str, value: &str) {
        let _ = std::fs::create_dir_all(dir());
        let _ = std::fs::write(path(key), value);
    }

    #[allow(dead_code)]
    pub fn remove(key: &str) {
        let _ = std::fs::remove_file(path(key));
    }

    pub fn now_ms() -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or_default()
    }

    pub fn mint() -> String {
        // Native identity only has to be unique among processes on this
        // machine, and native is the dev loop rather than the product — the
        // clock and the pid are plenty, and neither needs an RNG crate.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64 + d.as_secs() * 1_000_000_000)
            .unwrap_or_default();
        format!("{:012x}{:04x}", nanos & 0xFFFF_FFFF_FFFF, std::process::id() & 0xFFFF)
    }
}

use backend::{mint, now_ms, read, write};

#[cfg(test)]
mod tests {
    use super::*;

    /// The token goes into the lobby's start line, which is separated by commas
    /// and at-signs. One that could contain either could rewrite the roster it
    /// travels in, so the filter is a security boundary and not a tidiness one.
    #[test]
    fn only_plain_tokens_are_accepted() {
        assert!(is_token("deadbeef00112233"));
        assert!(!is_token(""), "an empty token would match a missing one");
        assert!(!is_token("dead,beef"), "a comma would split the roster");
        assert!(!is_token("dead@beef"), "an at-sign would rewrite a seat");
        assert!(!is_token("../../etc/passwd"), "it is also used as a file name");
        assert!(!is_token(&"a".repeat(33)), "unbounded ids make unbounded packets");
    }

    /// Every room remembers its own match, and offline remembers a different one
    /// again — arriving in a room you have never played must not restore the
    /// solo game you were in this morning.
    #[test]
    fn each_room_remembers_its_own_match() {
        assert_ne!(match_key(Some("abcde")), match_key(None));
        assert_ne!(match_key(Some("abcde")), match_key(Some("fghij")));
    }
}
