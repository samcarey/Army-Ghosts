//! Session bring-up: launch-config parsing (URL params on web, env vars on
//! native), matchbox signaling, the lobby, and GGRS session construction.
//!
//! Modes:
//! - `room` set → p2p lobby: the room stays open and peers accumulate (up to
//!   `players`, default all `MAX_PLAYERS` seats). The host — lowest sorted
//!   peer id, identical on every peer's view — starts the match for whoever
//!   is present (START button / Enter), or it auto-starts at the cap.
//! - no `room` → local synctest: all handles local, GGRS re-simulates every
//!   frame (`check_distance`) — the determinism canary AND the offline mode.

use bevy::prelude::*;
use bevy_ggrs::ggrs::{DesyncDetection, PlayerType, SessionBuilder};
use bevy_ggrs::{Rollback, Session};
use bevy_matchbox::matchbox_socket::{RtcIceServerConfig, WebRtcChannel, WebRtcSocketBuilder};
use bevy_matchbox::prelude::*;

use army_ghosts_sim::{
    spawn_world, Scenario, GRASS_MAX_H, MAX_PLAYERS, STANCE_STAND, TICK_HZ,
};

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
    let signaling =
        std::env::var("AG_SIGNALING").unwrap_or_else(|_| DEFAULT_SIGNALING.to_string());
    let ice = parse_ice(std::env::var("AG_ICE").ok());
    LaunchConfig { room, players, signaling, ice, scenario, bots }
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
    let signaling = get(&net, "signaling").unwrap_or_else(|| DEFAULT_SIGNALING.to_string());
    let ice = parse_ice(get(&net, "ice"));
    LaunchConfig { room, players, signaling, ice, scenario, bots }
}

/// Handoff between `run_lobby` (which tears the warmup world down) and
/// `finalize_p2p_session` (which builds the real session one frame later).
/// The one-frame gap matters: bevy_ggrs's driver sees "no session" for a tick
/// and resets its frame counter / time accumulator / snapshot bookkeeping.
#[derive(Resource)]
pub struct PendingSession {
    pub players: Vec<PlayerType<PeerId>>,
    pub channel: Option<WebRtcChannel>,
}

/// Matchbox channel indices, in builder order: 0 reliable (lobby control),
/// 1 unreliable (the GGRS transport).
pub const LOBBY_CHANNEL: usize = 0;
pub const GGRS_CHANNEL: usize = 1;
/// The one lobby message: `start:<uuid>,<uuid>,...` — the roster in sorted
/// order, broadcast by the host on the reliable channel.
const START_PREFIX: &[u8] = b"start:";

/// Live lobby view + start handshake state, updated every frame by
/// [`run_lobby`] and read by the HUD (player list, START button visibility).
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
    pub roster: Option<Vec<PeerId>>,
    /// Set by the UI (tap/click/Enter), consumed by `run_lobby`.
    pub start_requested: bool,
}

fn start_local_session(commands: &mut Commands, players: usize, bots: usize, scenario: Scenario) {
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
    spawn_world(commands, players, bots, scenario);
}

/// Startup: always start playing immediately. With a room, that's a 1-player
/// "warmup" session while matchbox gathers peers (torn down and replaced by
/// the p2p session when everyone's in); without one it's plain local mode.
pub fn begin_session_setup(
    mut commands: Commands,
    launch: Res<LaunchConfig>,
    mut next_state: ResMut<NextState<AppState>>,
) {
    match &launch.room {
        Some(room) => {
            // Plain open room (no `?next=` — that closed-at-N matchmaking is
            // gone): everyone joining connects to everyone present, and the
            // lobby decides when to start.
            let url = format!("{}/{}", launch.signaling, room);
            info!("connecting to matchbox room {url} (ice: {:?})", launch.ice);
            let mut builder = WebRtcSocketBuilder::new(url);
            if let Some(urls) = &launch.ice {
                builder = builder.ice_server(RtcIceServerConfig {
                    urls: urls.clone(),
                    ..default()
                });
            }
            // Channel order defines the indices: reliable lobby control
            // first, then the unreliable (UDP-like) channel GGRS wants
            // (matchbox's `ggrs` feature impls NonBlockingSocket on it).
            commands.insert_resource(MatchboxSocket::from(
                builder.add_reliable_channel().add_unreliable_channel(),
            ));
            // Warmup: run around and shoot while waiting for the room to fill.
            // Always the real arena — `parse_scenario` refuses the rig with a
            // room set, but spell it out rather than rely on that here.
            // No bots in warmup: the world is torn down and rebuilt when the
            // match starts, and bots that vanish on START read as a bug.
            start_local_session(&mut commands, 1, 0, Scenario::Arena);
        }
        None => {
            info!(
                "no room — starting local synctest session ({} players, {} bots, {:?})",
                launch.players, launch.bots, launch.scenario
            );
            start_local_session(&mut commands, launch.players, launch.bots, launch.scenario);
            next_state.set(AppState::InGame);
        }
    }
}

/// The open-room lobby. Every frame: refresh the peer view, adopt a start
/// roster if the host broadcast one, decide to start if WE are the host
/// (START button, or the room hit the `players` cap), and once the agreed
/// roster is fully connected, tear down the warmup world and stash the GGRS
/// channel + roster for `finalize_p2p_session` (next frame — see
/// [`PendingSession`]). Peers who join after the start idle in warmup.
pub fn run_lobby(
    mut commands: Commands,
    socket: Option<ResMut<MatchboxSocket>>,
    pending: Option<Res<PendingSession>>,
    mut lobby: ResMut<Lobby>,
    launch: Res<LaunchConfig>,
    rollback_entities: Query<Entity, With<Rollback>>,
) {
    let start_requested = std::mem::take(&mut lobby.start_requested);
    if pending.is_some() {
        return; // teardown already done, finalize runs next frame
    }
    let Some(mut socket) = socket else {
        return; // local mode: no socket
    };
    if socket.get_channel(GGRS_CHANNEL).is_err() {
        return; // channel already taken (session started)
    }
    socket.update_peers();
    let Some(my_id) = socket.id() else {
        return; // signaling handshake hasn't assigned us an id yet
    };

    // The canonical room view: sorted ids, self included — identical on every
    // peer, so everyone agrees who the host (first entry) is.
    let mut ids: Vec<PeerId> = socket.connected_peers().collect();
    ids.push(my_id);
    ids.sort();
    let is_host = ids.first() == Some(&my_id);
    lobby.my_id = Some(my_id);
    lobby.is_host = is_host;
    lobby.ids = ids.clone();

    // Adopt a roster broadcast by the host.
    for (_, packet) in socket.channel_mut(LOBBY_CHANNEL).receive() {
        let Some(rest) = packet.strip_prefix(START_PREFIX) else { continue };
        let parsed: Option<Vec<PeerId>> = std::str::from_utf8(rest).ok().and_then(|s| {
            s.split(',')
                .map(|u| uuid::Uuid::parse_str(u).ok().map(PeerId))
                .collect()
        });
        match parsed {
            Some(roster) => {
                info!("lobby: received start roster ({} players)", roster.len());
                lobby.roster = Some(roster);
            }
            None => warn!("lobby: ignoring malformed start message"),
        }
    }

    // Host: start on request, or automatically when the room hits the cap.
    if lobby.roster.is_none()
        && is_host
        && ids.len() >= 2
        && (start_requested || ids.len() >= launch.players)
    {
        let mut roster = ids;
        roster.truncate(launch.players); // host is index 0, always included
        let msg = format!(
            "start:{}",
            roster.iter().map(ToString::to_string).collect::<Vec<_>>().join(",")
        );
        info!("lobby: hosting — starting match with {} players", roster.len());
        for peer in roster.iter().copied().filter(|p| *p != my_id) {
            socket
                .channel_mut(LOBBY_CHANNEL)
                .send(msg.clone().into_bytes().into(), peer);
        }
        lobby.roster = Some(roster);
    }

    // Launch once our own mesh includes every roster member (the host's does
    // by construction; a non-host may still be mid-handshake with a third
    // peer, and GGRS must not send to a peer matchbox doesn't know yet).
    let Some(roster) = lobby.roster.clone() else { return };
    if !roster.contains(&my_id) {
        return; // match started without us (beyond the cap) — stay in warmup
    }
    if !roster.iter().all(|p| lobby.ids.contains(p)) {
        return;
    }
    info!("all {} players connected — restarting into p2p session", roster.len());

    // Tear down the warmup world; the Session's absence next frame makes
    // bevy_ggrs reset its frame counter and snapshot state.
    for entity in &rollback_entities {
        commands.entity(entity).despawn();
    }
    commands.remove_resource::<Session<SessionConfig>>();
    let channel = socket.take_channel(GGRS_CHANNEL).expect("take ggrs channel");
    let players = roster
        .iter()
        .map(|p| if *p == my_id { PlayerType::Local } else { PlayerType::Remote(*p) })
        .collect();
    commands.insert_resource(PendingSession { players, channel: Some(channel) });
}

/// One frame after teardown: build the real p2p session and the fresh world.
/// Handle order comes from the roster's sorted peer-id order, so every peer
/// agrees on who is which handle. Ordered BEFORE `run_lobby` in the schedule
/// so the bevy_ggrs no-session reset tick happens in between.
pub fn finalize_p2p_session(
    mut commands: Commands,
    pending: Option<ResMut<PendingSession>>,
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
    // The frame counter reset to 0 with the session gone, but Time<GgrsTime>
    // kept the warmup's elapsed time — bevy_ggrs would then `advance_to` an
    // EARLIER moment and panic ("tried to move time backwards"). Fresh clock.
    commands.insert_resource(Time::new_with(bevy_ggrs::GgrsTime));
    commands.remove_resource::<PendingSession>();
    // A p2p match is the arena, full stop: every peer must build the same world
    // and only one of them typed the URL.
    //
    // Bot count is 0 here for exactly that reason and NOT `launch.bots`: two
    // peers joining with different `?bots=` would build different worlds, which
    // is a desync before the first tick. In a room the count has to be *agreed*,
    // so it rides in the host's start roster — until that lands, rooms are
    // humans only and bots are an offline feature.
    spawn_world(&mut commands, num_players, 0, Scenario::Arena);
    next_state.set(AppState::InGame);
}

/// Log GGRS session events — desyncs especially. A desync means the integer
/// sim broke determinism somewhere; treat it as a bug, always. Also logs each
/// remote peer's ping periodically (same stats the HUD player list shows).
pub fn log_ggrs_events(mut session: ResMut<Session<SessionConfig>>, mut frames: Local<u32>) {
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
