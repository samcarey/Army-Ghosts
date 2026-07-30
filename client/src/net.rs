//! Session bring-up: launch-config parsing (URL params on web, env vars on
//! native), matchbox signaling, the lobby, rejoining, and GGRS session
//! construction.
//!
//! Modes:
//! - `room` set → p2p lobby: the room stays open and peers accumulate (up to
//!   `players`, default all `MAX_PLAYERS` seats). The host — lowest sorted
//!   peer id, identical on every peer's view — starts the match for whoever
//!   is present (START button / Enter), or it auto-starts at the cap.
//! - no `room` → local synctest: all handles local, GGRS re-simulates every
//!   frame (`check_distance`) — the determinism canary AND the offline mode.
//!
//! # Coming back
//!
//! A refreshed browser tab is a NEW matchbox peer with a new `PeerId`, and GGRS
//! fixes its player list when the session is built — there is no join-in-
//! progress and no seat to slide back into. So a rejoin is not a join at all:
//! **every peer in the match rebuilds its session, at frame 0, from one agreed
//! world.** That world is a `save::Save` blob captured live by one peer and
//! broadcast on the reliable channel, and the manoeuvre is the same one the
//! warmup-to-p2p swap already performs — tear the world down, let a tick pass
//! with no session so bevy_ggrs resets its bookkeeping, build the next one.
//!
//! Three consequences run through everything below:
//!
//! * **A handle is a position in the roster and must never move.** Renumbering
//!   the seats when somebody leaves would hand a returning player somebody
//!   else's pawn. So a seat whose player is gone stays in the roster as a
//!   VACANT seat, held [`PlayerType::Local`] by every peer and fed a blank
//!   input by all of them — see [`Seat`]. Their pawn stands where they left it
//!   and can still be shot, which is what it should do.
//! * **A GGRS session consumes its channel.** `take_channel` moves it out of
//!   the socket and the session drops it at the end, so a rebuild needs a fresh
//!   one: the socket is built with a pool of them and the generation number in
//!   the `go:` message names which — see [`SESSION_CHANNELS`].
//! * **Identity has to outlive the socket**, which is `persist::Identity`.

use bevy::prelude::*;
use bevy_ggrs::ggrs::{DesyncDetection, PlayerType, SessionBuilder};
use bevy_ggrs::{Rollback, Session};
use bevy_matchbox::matchbox_socket::{RtcIceServerConfig, WebRtcChannel, WebRtcSocketBuilder};
use bevy_matchbox::prelude::*;

use army_ghosts_sim::save::{self, Dials};
use army_ghosts_sim::{
    spawn_world, BotRoster, Save, Scenario, GRASS_MAX_H, MAX_PLAYERS, STANCE_STAND, TICK_HZ,
};

use crate::menu::{Aggression, BotCount};
use crate::persist::{self, Identity};
use crate::{AppState, SessionConfig};

/// How the game was launched: who to connect to, or synctest when `room` is
/// absent. Parsed once at startup, before the Bevy app is built.
#[derive(Resource, Debug, Clone)]
pub struct LaunchConfig {
    pub room: Option<String>,
    pub players: usize,
    /// Signaling server base URL, e.g. `ws://127.0.0.1:3536`.
    pub signaling: String,
    /// ICE (STUN) server URLs. `None` → matchbox's default Google STUN pair.
    /// `Some(vec![])` (from `ice=none`) → host candidates only — required for
    /// fast local/LAN testing on networks that eat STUN (this Mac's firewall
    /// drops the responses, and browsers then stall ICE gathering for ~40s
    /// per handshake leg, which reads as "p2p is broken").
    pub ice: Option<Vec<String>>,
    /// Which world to build. Always [`Scenario::Arena`] in a real match; the
    /// measuring rig is offline-only (see [`parse_scenario`]).
    pub scenario: Scenario,
    /// How many bot pawns to spawn alongside the humans. The menu is the usual
    /// way in; this is the starting value, so a test can ask for a full arena
    /// without touching the UI (`?bots=5`, `AG_BOTS=5`).
    pub bots: usize,
    /// Where the menu's aggression dial starts, as a percentage `0..=100`
    /// rounded to the nearest position (`?aggro=20`, `AG_AGGRO=20`). Same
    /// arrangement as `bots`: the URL and the menu are one setting, not two.
    pub aggro: Option<u32>,
    /// Whether to pick up where this browser left off (`?resume=0` turns it
    /// off). On by default — the whole point — but a test that wants a
    /// guaranteed fresh arena, and a player who has wedged themselves
    /// somewhere, both need the door.
    pub resume: bool,
}

const DEFAULT_SIGNALING: &str = "ws://127.0.0.1:3536";

/// Room size defaults: with a room, `players` is the seat CAP — default all 8
/// (the match starts with whoever's present when the host taps START, and
/// auto-starts if the room actually fills). Offline defaults to 1 (a phantom
/// second pawn in solo practice reads as "someone else is here"); an explicit
/// `players` still forces a multi-handle local synctest for testing.
fn resolve_players(explicit: Option<usize>, has_room: bool, scenario: Scenario) -> usize {
    if matches!(scenario, Scenario::GrassStrip { .. }) {
        return 2; // the rig is a fixed two-hander; whatever `players` said is noise
    }
    explicit
        .unwrap_or(if has_room { MAX_PLAYERS } else { 1 })
        .clamp(1, MAX_PLAYERS)
}

/// Bots fill the seats the humans aren't using, so the two together can never
/// exceed [`MAX_PLAYERS`] — there are only that many spawn points. The rig is a
/// fixed two-hander and takes none.
fn resolve_bots(explicit: Option<usize>, players: usize, scenario: Scenario) -> usize {
    if matches!(scenario, Scenario::GrassStrip { .. }) {
        return 0;
    }
    explicit.unwrap_or(0).min(MAX_PLAYERS.saturating_sub(players))
}

/// Dev scenario override — `AG_SCENARIO` natively, `?scenario=` on the web:
///
/// * `strip` — the concealment rig at the deepest grass the field can hold
/// * `strip:<depth>` — that wall, `depth` units deep
/// * `strip:<depth>:<level>` — and the east pawn crouching (1) or prone (2)
///
/// Ignored outright when a room is set: peers that disagree about which world
/// they are in desync on the first tick, and nothing about this is worth that
/// risk. Anything unrecognised is the game.
fn parse_scenario(raw: Option<String>, has_room: bool) -> Scenario {
    if has_room {
        return Scenario::Arena;
    }
    let Some(raw) = raw else { return Scenario::Arena };
    let mut parts = raw.split(':');
    if parts.next() != Some("strip") {
        return Scenario::Arena;
    }
    Scenario::GrassStrip {
        depth: parts
            .next()
            .and_then(|d| d.parse().ok())
            .unwrap_or(GRASS_MAX_H)
            .clamp(0, GRASS_MAX_H),
        east_stance: parts.next().and_then(|s| s.parse().ok()).unwrap_or(STANCE_STAND),
    }
}

fn parse_ice(raw: Option<String>) -> Option<Vec<String>> {
    let raw = raw?;
    if raw == "none" {
        return Some(vec![]);
    }
    Some(raw.split(',').map(str::to_string).collect())
}

/// `?resume=0` / `AG_RESUME=0` — anything but an explicit "0" leaves it on.
fn parse_resume(raw: Option<String>) -> bool {
    raw.as_deref() != Some("0")
}

/// Native: env vars (`AG_ROOM`, `AG_PLAYERS`, `AG_SIGNALING`).
#[cfg(not(target_arch = "wasm32"))]
pub fn launch_config() -> LaunchConfig {
    let room = std::env::var("AG_ROOM").ok().filter(|r| !r.is_empty());
    let scenario = parse_scenario(std::env::var("AG_SCENARIO").ok(), room.is_some());
    let players = resolve_players(
        std::env::var("AG_PLAYERS").ok().and_then(|p| p.parse().ok()),
        room.is_some(),
        scenario,
    );
    let bots = resolve_bots(
        std::env::var("AG_BOTS").ok().and_then(|b| b.parse().ok()),
        players,
        scenario,
    );
    let aggro = std::env::var("AG_AGGRO").ok().and_then(|a| a.parse().ok());
    let signaling =
        std::env::var("AG_SIGNALING").unwrap_or_else(|_| DEFAULT_SIGNALING.to_string());
    let ice = parse_ice(std::env::var("AG_ICE").ok());
    let resume = parse_resume(std::env::var("AG_RESUME").ok());
    LaunchConfig { room, players, signaling, ice, scenario, bots, aggro, resume }
}

/// Web: `window.__AG_NET__ = { room, players, signaling }`, set by index.html
/// from the URL query string *before* WASM init.
#[cfg(target_arch = "wasm32")]
pub fn launch_config() -> LaunchConfig {
    fn get(obj: &wasm_bindgen::JsValue, key: &str) -> Option<String> {
        js_sys::Reflect::get(obj, &key.into())
            .ok()
            .and_then(|v| v.as_string())
            .filter(|s| !s.is_empty())
    }
    let window = web_sys::window().expect("no window");
    let net = js_sys::Reflect::get(&window, &"__AG_NET__".into()).unwrap_or_default();
    let room = get(&net, "room");
    let scenario = parse_scenario(get(&net, "scenario"), room.is_some());
    let players = resolve_players(
        get(&net, "players").and_then(|p| p.parse().ok()),
        room.is_some(),
        scenario,
    );
    let bots = resolve_bots(get(&net, "bots").and_then(|b| b.parse().ok()), players, scenario);
    let aggro = get(&net, "aggro").and_then(|a| a.parse().ok());
    let signaling = get(&net, "signaling").unwrap_or_else(|| DEFAULT_SIGNALING.to_string());
    let ice = parse_ice(get(&net, "ice"));
    let resume = parse_resume(get(&net, "resume"));
    LaunchConfig { room, players, signaling, ice, scenario, bots, aggro, resume }
}

// ── Seats, generations and the wire ─────────────────────────────────────────

/// Matchbox channel indices, in builder order: 0 reliable (lobby control), then
/// a pool of unreliable ones for GGRS.
pub const LOBBY_CHANNEL: usize = 0;
pub const GGRS_CHANNEL_BASE: usize = 1;

/// How many sessions one socket can host: the first match plus
/// `SESSION_CHANNELS - 1` rebuilds.
///
/// A GGRS session takes ownership of its channel and drops it when the session
/// is replaced, so each rebuild needs an unused one and there is no way to hand
/// a used channel back to the socket. Every peer derives the index the same way
/// — `GGRS_CHANNEL_BASE + generation`, and the generation is carried in the
/// message that starts the session — so a peer that joined late and has used no
/// channels still lands on the same one as a peer that has been through three
/// rebuilds. Extra data channels cost a few hundred bytes of SDP each at
/// connect time and nothing after that.
pub const SESSION_CHANNELS: usize = 8;

/// One place in the roster. **The index is the GGRS handle**, for the whole
/// life of the match.
///
/// `peer` is `None` for a VACANT seat: a player who has gone and not come back.
/// The seat cannot simply be removed, because removing it would slide every
/// later handle down one and hand somebody else's pawn to whoever was behind
/// them. Instead every peer registers a vacant seat as [`PlayerType::Local`]
/// and sends it a blank input (`input.rs`), so all of them independently
/// produce the identical input for it and the pawn stands exactly where its
/// player left it — still solid, still shootable, asking for nothing. That last
/// part is what `PlayerInput`'s zero-means-not-asking encoding buys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seat {
    pub player: String,
    pub peer: Option<PeerId>,
}

/// "I am here, this is who I am, and this is what is going on in this room" —
/// `hi:<player id>:<generation>:<roster>`, sent to every peer we meet. A peer
/// that is not in a match sends `-` for the last two.
///
/// **The room state is in the introduction and not in a reply to it, and that
/// is load-bearing.** It used to be two messages: everyone said hello, and a
/// peer already in a match answered "busy". Which left a race a returning
/// player loses: it learns our NAME from our hello, and a name is all it needs
/// to build a roster — so if the answer had not arrived yet, a peer that had
/// just refreshed would sort itself lowest, decide it was the host, and start a
/// SECOND match in a room that already had one. Measured, not theorised: two
/// browser tabs, and the reloaded one hosted its own generation 0 while the
/// other was still playing.
///
/// Folding the two together makes that unrepresentable. You cannot know who we
/// are without knowing whether we are busy, because it is one message.
const HI: &str = "hi:";
/// "That seat is mine, let me back in." Sent by a returning player to the one
/// peer that gets to answer it.
const REJOIN: &str = "rejoin:";
/// "Build this session now": generation, roster, and the world to start it
/// from (empty for a cold start).
const GO: &str = "go:";

fn encode_seats(seats: &[Seat]) -> String {
    seats
        .iter()
        .map(|seat| match seat.peer {
            Some(peer) => format!("{}@{peer}", seat.player),
            None => format!("{}@-", seat.player),
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Parse a roster off the wire. Any malformed entry rejects the whole line —
/// a half-read roster is a session built around the wrong handles.
fn decode_seats(text: &str) -> Option<Vec<Seat>> {
    if text.is_empty() || text.split(',').count() > MAX_PLAYERS {
        return None;
    }
    text.split(',')
        .map(|entry| {
            let (player, peer) = entry.split_once('@')?;
            // The same filter storage applies, now against a stranger's packet:
            // a player id containing a comma or an at-sign could rewrite the
            // rest of the roster it travels in.
            if !persist::is_token(player) {
                return None;
            }
            let peer = match peer {
                "-" => None,
                id => Some(PeerId(uuid::Uuid::parse_str(id).ok()?)),
            };
            Some(Seat { player: player.to_string(), peer })
        })
        .collect()
}

/// The match this peer is in: which session generation, who is in which seat,
/// and which seat is ours. Absent in warmup and in offline play.
#[derive(Resource, Clone, Debug)]
pub struct MatchRoom {
    pub generation: u32,
    pub seats: Vec<Seat>,
    /// Our own handle — the index of our seat.
    pub me: usize,
}

impl MatchRoom {
    /// Handles held by nobody: registered `Local` and fed a blank input, so
    /// `input.rs` can tell them from our own seat.
    pub fn vacant(&self, handle: usize) -> bool {
        self.seats.get(handle).map(|s| s.peer.is_none()).unwrap_or(false)
    }

    /// Which seat sends the match-wide dials (how many bots, how aggressive):
    /// the lowest one somebody is actually sitting in.
    ///
    /// The sim honours the lowest handle that is ASKING (`dialled`), so exactly
    /// one client may send or the menus fight. It used to be handle 0 flatly on
    /// both sides, which froze the dials for the whole room the moment the
    /// player holding handle 0 walked away — their seat then sends a blank
    /// input forever, and a blank input asks for nothing.
    pub fn dial_holder(&self) -> Option<usize> {
        self.seats.iter().position(|seat| seat.peer.is_some())
    }
}

/// Handoff between the systems that tear the world down and
/// [`finalize_p2p_session`], which builds the next one a frame later. The
/// one-frame gap matters: bevy_ggrs's driver sees "no session" for a tick and
/// resets its frame counter / time accumulator / snapshot bookkeeping.
#[derive(Resource)]
pub struct PendingSession {
    pub players: Vec<PlayerType<PeerId>>,
    pub channel: Option<WebRtcChannel>,
    /// The world to start from, or `None` for a fresh arena.
    pub save: Option<Save>,
    pub room: MatchRoom,
}

/// A `go:` we have accepted but cannot act on yet, because our own WebRTC mesh
/// does not yet include everyone in it. GGRS must not be handed a peer matchbox
/// has never heard of.
#[derive(Resource)]
pub struct PendingGo {
    generation: u32,
    seats: Vec<Seat>,
    save: Option<Save>,
}

/// Set by [`run_room`] when a returning player has been recognised and WE are
/// the peer that gets to answer; consumed by [`serve_rejoin`], which is
/// exclusive because capturing the world takes `&mut World`.
#[derive(Resource)]
pub struct RejoinRequest {
    seats: Vec<Seat>,
}

/// Live lobby view + start handshake state, updated every frame by
/// [`run_room`] and read by the HUD (player list, START button visibility).
#[derive(Resource, Default)]
pub struct Lobby {
    /// All peers in the room including us, sorted by id — the order the
    /// match will use for player handles. Empty until signaling assigns us
    /// an id.
    pub ids: Vec<PeerId>,
    pub my_id: Option<PeerId>,
    /// Whether we're the host (lowest sorted id) — the peer allowed to start.
    pub is_host: bool,
    /// The agreed player set once start is triggered; held until our WebRTC
    /// mesh actually includes every member.
    pub roster: Option<Vec<Seat>>,
    /// Set by the UI (tap/click/Enter), consumed by [`run_lobby`].
    pub start_requested: bool,
    /// Each peer's stable player id, learned from its [`HELLO`]. A peer we have
    /// not heard from yet cannot be put in a roster: its handle would be a seat
    /// nobody could ever reclaim.
    names: Vec<(PeerId, String)>,
    /// Peers we have received a lobby packet from. Stronger evidence of
    /// reachability than matchbox's own connected list — see [`reachable`].
    heard: Vec<PeerId>,
    /// Peers we have already introduced ourselves to, and the generation we
    /// introduced ourselves AS. When that changes everyone is told again, so a
    /// peer idling in warmup learns that the match it is waiting on has moved
    /// on without it.
    greeted: Vec<PeerId>,
    greeted_at: Option<u32>,
    /// A match is already running in this room, per the peers in it: the
    /// generation and the roster they advertised. Blocks starting a second one.
    pub away: Option<(u32, Vec<Seat>)>,
    /// Whether that roster has a seat with our name on it — i.e. we are a
    /// player coming back rather than a newcomer arriving late.
    pub returning: bool,
    /// Frames until the next `rejoin:` retry.
    retry: u32,
}

impl Lobby {
    fn name_of(&self, peer: PeerId) -> Option<&str> {
        self.names.iter().find(|(id, _)| *id == peer).map(|(_, name)| name.as_str())
    }
}

fn start_local_session(commands: &mut Commands, players: usize) {
    let mut builder = SessionBuilder::<SessionConfig>::new()
        .with_num_players(players)
        .with_check_distance(2);
    for handle in 0..players {
        builder = builder
            .add_player(PlayerType::Local, handle)
            .expect("add local player");
    }
    let session = builder.start_synctest_session().expect("start synctest");
    commands.insert_resource(Session::SyncTest(session));
}

/// Startup: always start playing immediately. With a room, that's a 1-player
/// "warmup" session while matchbox gathers peers (torn down and replaced by
/// the p2p session when everyone's in); without one it's plain local mode.
///
/// Either way, if this browser has a match in storage for this room and it is
/// recent enough, the world it describes is what we start in rather than a
/// fresh arena. Offline that IS the resume — there is nobody to ask. With a
/// room it is a first paint: something recognisable to look at while the mesh
/// forms, and it is replaced wholesale by whatever the peers still playing
/// hand back.
pub fn begin_session_setup(
    mut commands: Commands,
    launch: Res<LaunchConfig>,
    roster: Res<BotRoster>,
    mut bots: ResMut<BotCount>,
    mut aggro: ResMut<Aggression>,
    mut next_state: ResMut<NextState<AppState>>,
) {
    let stored = launch
        .resume
        .then(|| persist::stored_match(&launch))
        .flatten()
        // The measuring rig is a fixed scene reached by a URL; there is nothing
        // in it worth carrying across a refresh, and its two carefully placed
        // pawns are the whole point of it.
        .filter(|_| matches!(launch.scenario, Scenario::Arena));

    match &launch.room {
        Some(room) => {
            // Plain open room (no `?next=` — that closed-at-N matchmaking is
            // gone): everyone joining connects to everyone present, and the
            // lobby decides when to start.
            let url = format!("{}/{}", launch.signaling, room);
            info!("connecting to matchbox room {url} (ice: {:?})", launch.ice);
            let mut builder = WebRtcSocketBuilder::new(url).add_reliable_channel();
            // The GGRS channel pool: one per session this socket may host. See
            // `SESSION_CHANNELS` for why they cannot be reused.
            for _ in 0..SESSION_CHANNELS {
                builder = builder.add_unreliable_channel();
            }
            if let Some(urls) = &launch.ice {
                builder = builder.ice_server(RtcIceServerConfig {
                    urls: urls.clone(),
                    ..default()
                });
            }
            commands.insert_resource(MatchboxSocket::from(builder));
            // Warmup: run around and shoot while waiting for the room to fill.
            // Always the real arena — `parse_scenario` refuses the rig with a
            // room set, but spell it out rather than rely on that here.
            start_local_session(&mut commands, 1);
            seed_world(&mut commands, stored, 1, Scenario::Arena, &roster, &mut bots, &mut aggro);
        }
        None => {
            info!(
                "no room — starting local synctest session ({} players, {} bots, {:?})",
                launch.players, launch.bots, launch.scenario
            );
            start_local_session(&mut commands, launch.players);
            seed_world(
                &mut commands,
                stored,
                launch.players,
                launch.scenario,
                &roster,
                &mut bots,
                &mut aggro,
            );
            next_state.set(AppState::InGame);
        }
    }
}

/// Fill an empty world: the stored match if there is one, otherwise a fresh
/// arena.
///
/// The dials come back with it, and that is not bookkeeping. Restore five bots
/// into a client whose bot dial reads zero and `reconcile_bots` will correctly
/// delete all five over the next five ticks — the dial is where the count
/// actually lives, and the pawns are only its consequence.
fn seed_world(
    commands: &mut Commands,
    stored: Option<persist::Stored>,
    players: usize,
    scenario: Scenario,
    roster: &BotRoster,
    bots: &mut BotCount,
    aggro: &mut Aggression,
) {
    match stored {
        Some(stored) => {
            info!(
                "resuming the stored match: round {}, {} pawns ({} bots), {:.0}s old",
                stored.save.round.number,
                stored.save.pawns.len(),
                stored.save.bot_count(),
                stored.age_secs,
            );
            apply_dials(stored.save.dials, bots, aggro);
            save::restore(commands, &stored.save, roster);
        }
        None => spawn_world(commands, players, scenario),
    }
}

fn apply_dials(dials: Dials, bots: &mut BotCount, aggro: &mut Aggression) {
    bots.0 = (dials.bots as usize).min(MAX_PLAYERS);
    if dials.aggro > 0 {
        aggro.0 = dials.aggro;
    }
}

// ── The room: everything that happens on the reliable channel ───────────────

/// Poll the socket and work the reliable channel, in the lobby and in a match
/// alike.
///
/// Deliberately ONE system across both, because the traffic is the same
/// traffic: every peer introduces itself, every peer answers, and the answer
/// simply differs depending on whether there is a match on. Splitting it left
/// the lobby unable to hear that a match was running — which is how a peer that
/// refreshed could elect ITSELF host (a fresh `PeerId` sorts wherever it likes)
/// and start a second match in a room that already had one.
pub fn run_room(
    mut commands: Commands,
    socket: Option<ResMut<MatchboxSocket>>,
    mut lobby: ResMut<Lobby>,
    identity: Res<Identity>,
    room: Option<Res<MatchRoom>>,
    pending: Option<Res<PendingSession>>,
    going: Option<Res<PendingGo>>,
) {
    let Some(mut socket) = socket else { return };
    // Once every channel is spoken for there is nothing left to build a session
    // out of, so stop pretending we could.
    if socket.get_channel(LOBBY_CHANNEL).is_err() {
        return;
    }
    // The RETURN value, not just the side effect: a peer matchbox reports as
    // gone is the one thing that can clear it from `heard`, since `heard`
    // deliberately outlives matchbox's connected list (see `reachable`).
    let changes = socket.update_peers();
    let Some(my_id) = socket.id() else {
        return; // signaling handshake hasn't assigned us an id yet
    };
    for (peer, state) in changes {
        if state == PeerState::Disconnected {
            lobby.heard.retain(|heard| *heard != peer);
        }
    }

    let mut ids: Vec<PeerId> = socket.connected_peers().collect();
    ids.push(my_id);
    ids.sort();
    lobby.my_id = Some(my_id);
    lobby.is_host = ids.first() == Some(&my_id);
    lobby.ids = ids.clone();
    lobby.names.retain(|(peer, _)| ids.contains(peer));
    lobby.greeted.retain(|peer| ids.contains(peer));
    // Our own name is always known, and putting it in the same table as
    // everyone else's means the roster builder has one place to look.
    if lobby.name_of(my_id).is_none() {
        lobby.names.push((my_id, identity.player.clone()));
    }

    // Introduce ourselves to anyone we haven't, room state and all. Sent per
    // peer rather than broadcast once, because a peer that arrives later never
    // saw the broadcast — and re-sent to everyone whenever the generation
    // changes, so a peer idling in warmup finds out that the match it is
    // waiting on has moved.
    let hi = match room.as_deref() {
        Some(room) => format!(
            "{HI}{}:{}:{}",
            identity.player,
            room.generation,
            encode_seats(&room.seats)
        ),
        None => format!("{HI}{}:-:-", identity.player),
    };
    if lobby.greeted_at != room.as_deref().map(|r| r.generation) {
        lobby.greeted.clear();
        lobby.greeted_at = room.as_deref().map(|r| r.generation);
    }
    let strangers: Vec<PeerId> = ids
        .iter()
        .copied()
        .filter(|peer| *peer != my_id && !lobby.greeted.contains(peer))
        .collect();
    for peer in strangers {
        socket
            .channel_mut(LOBBY_CHANNEL)
            .send(hi.clone().into_bytes().into(), peer);
        lobby.greeted.push(peer);
    }

    let mut rejoining: Option<(PeerId, String)> = None;
    let mut received: Vec<(PeerId, String)> = Vec::new();
    for (peer, packet) in socket.channel_mut(LOBBY_CHANNEL).receive() {
        match std::str::from_utf8(&packet) {
            Ok(text) => received.push((peer, text.to_string())),
            Err(_) => warn!("lobby: ignoring a packet that isn't text"),
        }
    }

    for (peer, text) in received {
        // Whatever they said, saying anything at all proves the link works.
        if !lobby.heard.contains(&peer) {
            lobby.heard.push(peer);
        }
        if let Some(rest) = text.strip_prefix(HI) {
            let Some((name, state)) = parse_hi(rest) else {
                warn!("lobby: ignoring a malformed hi");
                continue;
            };
            lobby.names.retain(|(id, _)| *id != peer);
            lobby.names.push((peer, name.clone()));
            // What they told us about the room. A peer that has just arrived
            // where a match is already running has no other way to find out,
            // and MUST find out before `run_lobby` lets it host one of its own.
            if let Some((generation, seats)) = state {
                lobby.returning = seats.iter().any(|seat| seat.player == identity.player);
                lobby.away = Some((generation, seats));
            }
            // And what we make of them. Somebody whose player id is in our
            // roster on a different peer is somebody who has come back.
            if let Some(room) = room.as_deref() {
                rejoining = rejoining.or_else(|| {
                    seat_of(room, &name).filter(|&h| room.seats[h].peer != Some(peer))?;
                    Some((peer, name.clone()))
                });
            }
            continue;
        }

        if let Some(name) = text.strip_prefix(REJOIN) {
            if let Some(room) = room.as_deref() {
                if seat_of(room, name).is_some_and(|h| room.seats[h].peer != Some(peer)) {
                    rejoining = Some((peer, name.to_string()));
                }
            }
            continue;
        }

        if let Some(rest) = text.strip_prefix(GO) {
            let here = room.as_deref().map(|r| r.generation);
            match parse_go(rest) {
                Some((generation, seats, save)) => {
                    // Strictly newer, always: a `go:` for a generation we are
                    // already in (a duplicate, or a second peer answering the
                    // same rejoin) would restart a perfectly good session and,
                    // worse, restart it from a different world.
                    if here.is_some_and(|current| generation <= current)
                        || going.as_deref().is_some_and(|g| generation <= g.generation)
                    {
                        continue;
                    }
                    info!(
                        "room: generation {generation} starting with {} seats",
                        seats.len()
                    );
                    lobby.roster = Some(seats.clone());
                    commands.insert_resource(PendingGo { generation, seats, save });
                }
                None => warn!("lobby: ignoring a malformed go message"),
            }
            continue;
        }
    }

    // We are in a match and somebody we know has come back on a new peer id.
    // Exactly one peer may answer, or two rosters race and half the room ends
    // up in each: the lowest-numbered seat that is actually here does it.
    if let (Some(room), Some((peer, name))) = (room.as_deref(), rejoining) {
        if pending.is_none() && going.is_none() && answering_peer(room, &lobby) == Some(room.me) {
            let seats = reseat(room, &lobby, &name, peer);
            info!("room: {name} is back — resyncing {} seats", seats.len());
            commands.insert_resource(RejoinRequest { seats });
        }
    }

    // We are the one who came back: ask. Repeatedly, because the answer
    // involves a world capture on somebody else's frame and packets can be
    // dropped before the data channel is properly up.
    if room.is_none() && pending.is_none() && going.is_none() && lobby.returning {
        lobby.retry = lobby.retry.saturating_sub(1);
        if lobby.retry == 0 {
            lobby.retry = REJOIN_RETRY_FRAMES;
            if let Some((_, seats)) = lobby.away.clone() {
                let ask = format!("{REJOIN}{}", identity.player);
                for peer in seats.iter().filter_map(|seat| seat.peer) {
                    if reachable(&lobby, peer) {
                        socket
                            .channel_mut(LOBBY_CHANNEL)
                            .send(ask.clone().into_bytes().into(), peer);
                    }
                }
            }
        }
    }
}

/// Frames between `rejoin:` asks — a third of a second, which is often enough
/// to feel instant and rare enough not to matter if nobody is listening.
const REJOIN_RETRY_FRAMES: u32 = 20;

/// `<player id>:<generation>:<roster>`, where a peer not in a match sends `-`
/// for the last two. Returns the name and, if they are playing, what they are
/// playing.
fn parse_hi(rest: &str) -> Option<(String, Option<(u32, Vec<Seat>)>)> {
    let mut parts = rest.splitn(3, ':');
    let name = parts.next()?;
    // Checked here as well as on the way out of storage: this one arrives from
    // a stranger, and it ends up in a roster made of separators.
    if !persist::is_token(name) {
        return None;
    }
    let (generation, seats) = (parts.next()?, parts.next()?);
    let state = match (generation, seats) {
        ("-", _) | (_, "-") => None,
        (generation, seats) => Some((generation.parse().ok()?, decode_seats(seats)?)),
    };
    Some((name.to_string(), state))
}

fn parse_go(rest: &str) -> Option<(u32, Vec<Seat>, Option<Save>)> {
    let (generation, rest) = rest.split_once(':')?;
    let (seats, blob) = rest.split_once('|')?;
    let save = if blob.trim().is_empty() {
        None
    } else {
        // A world we cannot parse is not a world we can play: better to ignore
        // the whole message and let the sender retry than to build a session
        // around an arena somebody else isn't in.
        Some(Save::decode(blob)?)
    };
    Some((generation.parse().ok()?, decode_seats(seats)?, save))
}

/// Is this peer somewhere we can reach?
///
/// **Not simply "matchbox says connected"**, and the difference is the whole
/// reason a rejoin works at all.
///
/// Measured, in two browser tabs: a peer that reloaded into a room with a match
/// already running sat for 150 seconds with `connected_peers()` reporting
/// NOBODY — while reading lobby messages, the entire handshake, from the very
/// peer it was waiting for. matchbox raises `PeerState::Connected` only once
/// every channel of the socket has opened (`wait_for_ready` awaits them in
/// turn), which is the likeliest reason a mid-match arrival never trips it; the
/// exact mechanism was not chased further, because the flag is the wrong thing
/// to ask either way.
///
/// A packet we have actually received is direct evidence that the link works,
/// and it is the evidence this uses. GGRS may then be handed a channel that is
/// still opening — that is what its synchronisation phase is for, and packets
/// dropped before it opens cost a moment, not the session.
fn reachable(lobby: &Lobby, peer: PeerId) -> bool {
    Some(peer) == lobby.my_id || lobby.ids.contains(&peer) || lobby.heard.contains(&peer)
}

fn seat_of(room: &MatchRoom, player: &str) -> Option<usize> {
    room.seats.iter().position(|seat| seat.player == player)
}

/// Which seat gets to answer a rejoin: the lowest-numbered one whose player is
/// actually in the room. Every peer computes it from its own view, and the
/// views agree because they are all looking at the same mesh — but the answer
/// only has to be right on ONE peer, the one that decides it is itself.
fn answering_peer(room: &MatchRoom, lobby: &Lobby) -> Option<usize> {
    room.seats
        .iter()
        .position(|seat| seat.peer.is_some_and(|peer| reachable(lobby, peer)))
}

/// The roster for the next generation: the returning player back in their own
/// seat, everyone still here left exactly where they were, and everyone else
/// vacated.
///
/// **Handles never move.** A seat is emptied rather than removed, because
/// removing one would slide every later handle down and hand a returning player
/// somebody else's pawn. Vacating instead costs nothing: `input.rs` sends a
/// blank input for the seat on every peer, so the pawn stands where it was left
/// and can still be shot — which is what should happen to somebody who has
/// walked away from a round they are still in.
fn reseat(room: &MatchRoom, lobby: &Lobby, returning: &str, peer: PeerId) -> Vec<Seat> {
    room.seats
        .iter()
        .map(|seat| {
            if seat.player == returning {
                return Seat { player: seat.player.clone(), peer: Some(peer) };
            }
            let here = seat.peer.filter(|p| reachable(lobby, *p));
            Seat { player: seat.player.clone(), peer: here }
        })
        .collect()
}

/// Capture the world and broadcast it as the next generation.
///
/// Exclusive because `save::capture` takes `&mut World` — the sim keeps its
/// component list to itself, so there is no query to ask for. The capture is of
/// this peer's live world, which makes the answering peer briefly authoritative
/// over everyone: that is not a hole in the p2p model so much as the honest
/// shape of a restart, and it lasts exactly one message.
pub fn serve_rejoin(world: &mut World) {
    let Some(request) = world.remove_resource::<RejoinRequest>() else { return };
    let Some(generation) = world.get_resource::<MatchRoom>().map(|r| r.generation + 1) else {
        return;
    };
    if generation as usize >= SESSION_CHANNELS {
        warn!(
            "room: out of session channels after {} rebuilds — {} cannot be let back in \
             without everyone reloading",
            generation - 1,
            request.seats.len()
        );
        return;
    }

    let dials = Dials {
        bots: world.get_resource::<BotCount>().map_or(0, |b| b.0 as u8),
        aggro: world.get_resource::<Aggression>().map_or(0, |a| a.0),
    };
    let save = save::capture(world, dials);
    let message = format!(
        "{GO}{generation}:{}|{}",
        encode_seats(&request.seats),
        save.encode()
    );

    world.resource_scope(|world, mut socket: Mut<MatchboxSocket>| {
        let me = world.get_resource::<Lobby>().and_then(|l| l.my_id);
        for peer in request.seats.iter().filter_map(|seat| seat.peer) {
            if Some(peer) != me {
                socket
                    .channel_mut(LOBBY_CHANNEL)
                    .send(message.clone().into_bytes().into(), peer);
            }
        }
    });

    // …and take our own medicine through the identical path — including the
    // parse. Restoring from the struct we captured while everyone else restores
    // from the bytes we sent would make this peer the one place a codec bug
    // could not show up.
    let save = Save::decode(&save.encode()).expect("our own blob must parse");
    world.insert_resource(PendingGo { generation, seats: request.seats, save: Some(save) });
}

/// Turn an accepted `go:` into a torn-down world and a [`PendingSession`], once
/// our own mesh actually contains everyone in it.
///
/// The wait is the same one the original lobby start does, and for the same
/// reason: GGRS must not be handed a peer matchbox has never heard of. A peer
/// that is merely SLOW to mesh gets there; a peer that is gone was vacated by
/// whoever built the roster, so it is not waited on at all.
pub fn adopt_pending_go(
    mut commands: Commands,
    socket: Option<ResMut<MatchboxSocket>>,
    going: Option<Res<PendingGo>>,
    lobby: Res<Lobby>,
    identity: Res<Identity>,
    rollback_entities: Query<Entity, With<Rollback>>,
    mut waited: Local<u32>,
) {
    let (Some(going), Some(mut socket)) = (going, socket) else { return };
    let Some(my_id) = lobby.my_id else { return };
    let me = going
        .seats
        .iter()
        .position(|seat| seat.player == identity.player);
    let Some(me) = me else {
        // The match started (or resumed) without us. Stay in warmup; a later
        // generation may yet have room. Logged rather than silent, because
        // "nothing happened and nothing was said" is the hardest shape of bug
        // to find in a handshake.
        info!(
            "room: generation {} has no seat for us — staying in warmup",
            going.generation
        );
        commands.remove_resource::<PendingGo>();
        return;
    };
    let missing: Vec<PeerId> = going
        .seats
        .iter()
        .filter_map(|seat| seat.peer)
        .filter(|peer| !reachable(&lobby, *peer))
        .collect();
    if !missing.is_empty() {
        // Rarely, and with the peers named: this is the shape a stalled
        // rejoin takes, and "nothing happened" is the hardest thing to debug.
        if *waited % 600 == 0 {
            info!(
                "room: generation {} is waiting on {} peer(s) to reach us: {missing:?}",
                going.generation,
                missing.len()
            );
        }
        *waited += 1;
        return;
    }
    *waited = 0;

    let channel_index = GGRS_CHANNEL_BASE + going.generation as usize;
    let Ok(channel) = socket.take_channel(channel_index) else {
        error!("room: no channel left for generation {} — cannot start", going.generation);
        commands.remove_resource::<PendingGo>();
        return;
    };
    info!(
        "all {} seats accounted for — starting generation {} on channel {channel_index}",
        going.seats.len(),
        going.generation
    );

    // Tear the current world down; the Session's absence next frame makes
    // bevy_ggrs reset its frame counter and snapshot state.
    for entity in &rollback_entities {
        commands.entity(entity).despawn();
    }
    commands.remove_resource::<Session<SessionConfig>>();

    let players = going
        .seats
        .iter()
        .map(|seat| match seat.peer {
            // Ours, and every vacant seat: see `Seat`. A vacant handle is Local
            // on EVERY peer and fed a blank input by all of them, which is the
            // only way GGRS will let a session run with nobody in a seat.
            Some(peer) if peer != my_id => PlayerType::Remote(peer),
            _ => PlayerType::Local,
        })
        .collect();
    commands.insert_resource(PendingSession {
        players,
        channel: Some(channel),
        save: going.save.clone(),
        room: MatchRoom {
            generation: going.generation,
            seats: going.seats.clone(),
            me,
        },
    });
    commands.remove_resource::<PendingGo>();
}

/// The open-room lobby's one remaining job: deciding, as host, to start.
///
/// Everything else — peers, names, messages — is [`run_room`]'s, because it all
/// happens in a match too.
pub fn run_lobby(
    mut commands: Commands,
    socket: Option<ResMut<MatchboxSocket>>,
    mut lobby: ResMut<Lobby>,
    identity: Res<Identity>,
    launch: Res<LaunchConfig>,
    pending: Option<Res<PendingSession>>,
    going: Option<Res<PendingGo>>,
) {
    let start_requested = std::mem::take(&mut lobby.start_requested);
    let Some(mut socket) = socket else { return };
    if pending.is_some() || going.is_some() || lobby.roster.is_some() {
        return; // already on our way into a session
    }
    // A match is already running in this room. Whether we have a seat in it or
    // not, we do not get to start a second one — and a peer that has just
    // refreshed can easily sort lowest and think it is the host.
    if lobby.away.is_some() {
        return;
    }
    let Some(my_id) = lobby.my_id else { return };
    if !lobby.is_host || lobby.ids.len() < 2 {
        return;
    }
    if !(start_requested || lobby.ids.len() >= launch.players) {
        return;
    }

    // A peer whose player id we have not heard yet cannot be given a seat: the
    // seat would be one nobody could ever reclaim, since reclaiming it means
    // matching that id. They are one packet away, so waiting costs a frame.
    let mut seats: Vec<Seat> = Vec::new();
    for peer in &lobby.ids {
        let Some(name) = lobby.name_of(*peer) else {
            info!("lobby: waiting to hear who {peer} is before starting");
            return;
        };
        seats.push(Seat { player: name.to_string(), peer: Some(*peer) });
    }
    seats.truncate(launch.players); // host is index 0, always included
    if !seats.iter().any(|seat| seat.player == identity.player) {
        return;
    }

    let message = format!("{GO}0:{}|", encode_seats(&seats));
    info!("lobby: hosting — starting match with {} players", seats.len());
    for peer in seats.iter().filter_map(|seat| seat.peer).filter(|p| *p != my_id) {
        socket
            .channel_mut(LOBBY_CHANNEL)
            .send(message.clone().into_bytes().into(), peer);
    }
    lobby.roster = Some(seats.clone());
    commands.insert_resource(PendingGo { generation: 0, seats, save: None });
}

/// One frame after teardown: build the real p2p session and the world to play
/// it in. Handle order comes from the roster, which every peer received
/// verbatim, so every peer agrees on who is which handle.
pub fn finalize_p2p_session(
    mut commands: Commands,
    pending: Option<ResMut<PendingSession>>,
    roster: Res<BotRoster>,
    mut bots: ResMut<BotCount>,
    mut aggro: ResMut<Aggression>,
    mut next_state: ResMut<NextState<AppState>>,
) {
    let Some(mut pending) = pending else { return };
    let Some(channel) = pending.channel.take() else { return };
    let num_players = pending.players.len();

    let mut builder = SessionBuilder::<SessionConfig>::new()
        .with_num_players(num_players)
        .with_input_delay(2)
        // Surface any nondeterminism as an explicit desync event instead of
        // silently diverged worlds (pairs with the sim's Pos checksums).
        .with_desync_detection_mode(DesyncDetection::On { interval: 10 })
        .with_fps(TICK_HZ)
        .expect("valid fps");
    for (handle, player) in pending.players.iter().enumerate() {
        builder = builder.add_player(*player, handle).expect("add player");
    }
    let session = builder.start_p2p_session(channel).expect("start p2p session");
    commands.insert_resource(Session::P2P(session));
    // **Forget every rollback entity this process has ever registered.**
    //
    // bevy_ggrs mixes `RollbackOrdered::order(rollback)` — a global,
    // monotonically increasing registration index — into the per-component
    // checksum, and its own doc says it keeps entries "even if they have since
    // been deleted". So the checksum of a pawn depends not on the pawn but on
    // how many rollback entities this peer had spawned before it, over the
    // whole life of the process.
    //
    // Two peers rebuilding a session from one agreed world therefore desync
    // unless they happen to have spawned the same number of entities getting
    // there. The original warmup-to-p2p swap survives that only by coincidence:
    // every peer's warmup is the same one pawn and the same cover. Restoring a
    // stored match into the warmup breaks the coincidence, and this is what it
    // looks like when it breaks — byte-identical worlds, `DESYNC at frame 50`.
    //
    // Resetting here is safe precisely because it is between sessions: the old
    // world was despawned a frame ago and the new one is spawned below, so
    // every peer numbers the same entities from zero in the same order.
    commands.insert_resource(bevy_ggrs::RollbackOrdered::default());
    // The frame counter reset to 0 with the session gone, but Time<GgrsTime>
    // kept the warmup's elapsed time — bevy_ggrs would then `advance_to` an
    // EARLIER moment and panic ("tried to move time backwards"). Fresh clock.
    commands.insert_resource(Time::new_with(bevy_ggrs::GgrsTime));

    match pending.save.take() {
        // Resuming: the world came off the wire, and the dials come with it or
        // `reconcile_bots` deletes every bot it just brought back.
        Some(save) => {
            info!(
                "resuming generation {} at round {} with {} pawns",
                pending.room.generation,
                save.round.number,
                save.pawns.len()
            );
            apply_dials(save.dials, &mut bots, &mut aggro);
            save::restore(&mut commands, &save, &roster);
        }
        // A cold start. A p2p match is the arena, full stop: every peer must
        // build the same world and only one of them typed the URL.
        //
        // No bots here either: they are never spawned with the world. The count
        // rides in the input stream and `reconcile_bots` applies it, which is
        // what makes it agree across peers without anyone sending a bot message.
        None => spawn_world(&mut commands, num_players, Scenario::Arena),
    }

    commands.insert_resource(pending.room.clone());
    commands.remove_resource::<PendingSession>();
    // Note the stored blob is deliberately NOT cleared here. `autosave` will
    // overwrite it with this world a third of a second from now anyway, and
    // keeping it current is worth something: it is what is left if every peer
    // in the room goes away at once.
    next_state.set(AppState::InGame);
}

/// Log GGRS session events — desyncs especially. A desync means the integer
/// sim broke determinism somewhere; treat it as a bug, always. Also logs each
/// remote peer's ping periodically (same stats the HUD player list shows).
///
/// `Option`, because a match now has moments with no session in it: a rejoin
/// tears the world down and leaves bevy_ggrs a bare tick to reset in, and we
/// are already `InGame` when that happens.
pub fn log_ggrs_events(
    session: Option<ResMut<Session<SessionConfig>>>,
    mut frames: Local<u32>,
) {
    let Some(mut session) = session else { return };
    if let Session::P2P(session) = session.as_mut() {
        for event in session.events() {
            match event {
                bevy_ggrs::ggrs::GgrsEvent::DesyncDetected {
                    frame, local_checksum, remote_checksum, ..
                } => {
                    error!("DESYNC at frame {frame}: local {local_checksum:#x} vs remote {remote_checksum:#x}");
                }
                other => info!("ggrs event: {other:?}"),
            }
        }
        *frames += 1;
        if *frames % 300 == 0 {
            for handle in session.remote_player_handles() {
                if let Ok(stats) = session.network_stats(handle) {
                    info!("net: player {} ping {}ms", handle + 1, stats.ping);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(byte: u8) -> PeerId {
        PeerId(uuid::Uuid::from_bytes([byte; 16]))
    }

    fn seats() -> Vec<Seat> {
        vec![
            Seat { player: "aaaa1111".into(), peer: Some(peer(1)) },
            Seat { player: "bbbb2222".into(), peer: None },
            Seat { player: "cccc3333".into(), peer: Some(peer(3)) },
        ]
    }

    /// The roster is the handle order, so it has to survive the wire exactly —
    /// including which seats are empty.
    #[test]
    fn a_roster_survives_the_wire() {
        let text = encode_seats(&seats());
        assert_eq!(decode_seats(&text), Some(seats()));
    }

    /// A roster arrives from a stranger, so a player id that could contain the
    /// separators would let one peer rewrite everybody else's seat.
    #[test]
    fn a_hostile_roster_is_refused() {
        for bad in [
            "",
            "no-at-sign",
            "aaaa1111@not-a-uuid",
            // A player id smuggling a separator.
            "aa,aa@00000000-0000-0000-0000-000000000001",
            "aa@bb@00000000-0000-0000-0000-000000000001",
            // More seats than there are spawn points.
            &["aaaa1111@-"; MAX_PLAYERS + 1].join(","),
        ] {
            assert!(decode_seats(bad).is_none(), "accepted a bad roster: {bad:?}");
        }
    }

    /// The introduction carries the room state, so a peer cannot learn who we
    /// are without also learning whether a match is already running.
    ///
    /// That is the fix for a race caught in a browser: as two separate
    /// messages, a tab that had just refreshed could have our NAME (enough to
    /// build a roster) without yet having our "busy", elect itself host, and
    /// start a second match in a room that already had one.
    #[test]
    fn an_introduction_says_whether_a_match_is_running() {
        let lobby = parse_hi("aaaa1111:-:-").expect("a peer in the lobby");
        assert_eq!(lobby.0, "aaaa1111");
        assert_eq!(lobby.1, None, "a lobby peer must not look like a match");

        let playing = parse_hi(&format!("aaaa1111:4:{}", encode_seats(&seats())))
            .expect("a peer in a match");
        assert_eq!(playing.1, Some((4, seats())));

        for bad in [
            "",
            "aaaa1111",              // no room state at all
            "aaaa1111:4",            // a generation and no roster
            "aaaa1111:4:garbage",
            "not-a-token:-:-",       // an id that could rewrite a roster
            "aaaa1111:x:aaaa1111@-", // a generation that isn't a number
        ] {
            assert!(parse_hi(bad).is_none(), "accepted a bad hi: {bad:?}");
        }
    }

    /// A `go:` carries a generation, a roster and optionally a world; a cold
    /// start has no world and must not be read as a broken one.
    #[test]
    fn a_go_message_carries_a_generation_and_maybe_a_world() {
        let cold = format!("0:{}|", encode_seats(&seats()));
        let (generation, roster, save) = parse_go(&cold).expect("cold start");
        assert_eq!(generation, 0);
        assert_eq!(roster, seats());
        assert!(save.is_none(), "a cold start has no world in it");

        let world = Save {
            dials: Dials { bots: 2, aggro: 5 },
            round: default(),
            pawns: vec![],
        };
        let warm = format!("3:{}|{}", encode_seats(&seats()), world.encode());
        let (generation, _, save) = parse_go(&warm).expect("resume");
        assert_eq!(generation, 3);
        assert_eq!(save.map(|s| s.dials), Some(Dials { bots: 2, aggro: 5 }));

        for bad in ["", "0", "x:aaaa1111@-|", &format!("0:{}", encode_seats(&seats()))] {
            assert!(parse_go(bad).is_none(), "accepted a bad go: {bad:?}");
        }
    }

    /// Coming back must put you in YOUR seat and leave everyone else's alone —
    /// and must empty, never remove, the seats of people who have gone, because
    /// a handle is a position in this list.
    #[test]
    fn reseating_keeps_every_handle_where_it_was() {
        let room = MatchRoom { generation: 1, seats: seats(), me: 0 };
        let lobby = Lobby {
            my_id: Some(peer(1)),
            // Seat 2's player has gone for good; seat 1's is the one returning.
            ids: vec![peer(1), peer(9)],
            ..default()
        };
        let next = reseat(&room, &lobby, "bbbb2222", peer(9));

        assert_eq!(next.len(), room.seats.len(), "a handle moved");
        let players: Vec<&str> = next.iter().map(|s| s.player.as_str()).collect();
        assert_eq!(players, vec!["aaaa1111", "bbbb2222", "cccc3333"]);
        assert_eq!(next[0].peer, Some(peer(1)), "the peer still here lost its seat");
        assert_eq!(next[1].peer, Some(peer(9)), "the returning player is not back");
        assert_eq!(next[2].peer, None, "the peer that left should be vacated, not removed");
    }

    /// Exactly one peer answers a rejoin, or two rosters race and the room ends
    /// up split between them. It is the lowest seat that is actually present —
    /// so if seat 0 is the one that walked away, seat 1 does it.
    #[test]
    fn only_one_peer_answers_a_rejoin() {
        let mut room = MatchRoom { generation: 1, seats: seats(), me: 0 };
        let lobby = Lobby { my_id: Some(peer(3)), ids: vec![peer(3)], ..default() };
        // Seat 0's peer is not in this peer's view at all, and seat 1 is vacant.
        assert_eq!(answering_peer(&room, &lobby), Some(2));

        room.seats[0].peer = Some(peer(3));
        assert_eq!(answering_peer(&room, &lobby), Some(0), "the earliest present seat answers");

        for seat in &mut room.seats {
            seat.peer = None;
        }
        assert_eq!(answering_peer(&room, &lobby), None, "an empty room answers nothing");
    }

    /// The channel pool is what bounds how many times a room can be rebuilt,
    /// and the arithmetic that picks one has to stay inside the socket.
    #[test]
    fn every_generation_has_a_channel() {
        for generation in 0..SESSION_CHANNELS {
            let index = GGRS_CHANNEL_BASE + generation;
            assert!(index > LOBBY_CHANNEL, "generation {generation} collides with the lobby");
            assert!(
                index < GGRS_CHANNEL_BASE + SESSION_CHANNELS,
                "generation {generation} has no channel"
            );
        }
    }
}
