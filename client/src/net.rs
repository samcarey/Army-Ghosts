//! Session bring-up: launch-config parsing (URL params on web, env vars on
//! native), matchbox signaling, and GGRS session construction.
//!
//! Modes:
//! - `room` set → p2p: connect to the signaling server, wait for `players`
//!   peers, start a GGRS p2p session over the matchbox WebRTC channel.
//! - no `room` → local synctest: all handles local, GGRS re-simulates every
//!   frame (`check_distance`) — the determinism canary AND the offline mode.

use bevy::prelude::*;
use bevy_ggrs::ggrs::{DesyncDetection, PlayerType, SessionBuilder};
use bevy_ggrs::{Rollback, Session};
use bevy_matchbox::matchbox_socket::{RtcIceServerConfig, WebRtcChannel, WebRtcSocketBuilder};
use bevy_matchbox::prelude::*;

use army_ghosts_sim::{spawn_world, MAX_PLAYERS, TICK_HZ};

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
}

const DEFAULT_SIGNALING: &str = "ws://127.0.0.1:3536";

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
    let players = std::env::var("AG_PLAYERS")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(2)
        .clamp(1, MAX_PLAYERS);
    let signaling =
        std::env::var("AG_SIGNALING").unwrap_or_else(|_| DEFAULT_SIGNALING.to_string());
    let ice = parse_ice(std::env::var("AG_ICE").ok());
    LaunchConfig { room, players, signaling, ice }
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
    let players = get(&net, "players")
        .and_then(|p| p.parse().ok())
        .unwrap_or(2)
        .clamp(1, MAX_PLAYERS);
    let signaling = get(&net, "signaling").unwrap_or_else(|| DEFAULT_SIGNALING.to_string());
    let ice = parse_ice(get(&net, "ice"));
    LaunchConfig { room, players, signaling, ice }
}

/// Handoff between `wait_for_players` (which tears the warmup world down) and
/// `finalize_p2p_session` (which builds the real session one frame later).
/// The one-frame gap matters: bevy_ggrs's driver sees "no session" for a tick
/// and resets its frame counter / time accumulator / snapshot bookkeeping.
#[derive(Resource)]
pub struct PendingSession {
    pub players: Vec<PlayerType<PeerId>>,
    pub channel: Option<WebRtcChannel>,
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
    spawn_world(commands, players);
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
            // `next_N` waits until N peers are in the room, then pairs exactly
            // those N — the signaling server closes the room to further joins.
            let url = format!("{}/{}?next={}", launch.signaling, room, launch.players);
            info!("connecting to matchbox room {url} (ice: {:?})", launch.ice);
            // Single unreliable (UDP-like) channel — exactly what GGRS wants;
            // matchbox's `ggrs` feature impls NonBlockingSocket on it.
            let mut builder = WebRtcSocketBuilder::new(url);
            if let Some(urls) = &launch.ice {
                builder = builder.ice_server(RtcIceServerConfig {
                    urls: urls.clone(),
                    ..default()
                });
            }
            commands.insert_resource(MatchboxSocket::from(builder.add_unreliable_channel()));
            // Warmup: run around and shoot while waiting for the room to fill.
            start_local_session(&mut commands, 1);
        }
        None => {
            info!("no room — starting local synctest session ({} players)", launch.players);
            start_local_session(&mut commands, launch.players);
            next_state.set(AppState::InGame);
        }
    }
}

/// Poll the signaling connection until everyone's here, then tear down the
/// warmup world and stash the channel + roster for `finalize_p2p_session`
/// (which runs next frame — see [`PendingSession`]).
pub fn wait_for_players(
    mut commands: Commands,
    socket: Option<ResMut<MatchboxSocket>>,
    pending: Option<Res<PendingSession>>,
    launch: Res<LaunchConfig>,
    rollback_entities: Query<Entity, With<Rollback>>,
) {
    if pending.is_some() {
        return; // teardown already done, finalize runs next frame
    }
    let Some(mut socket) = socket else {
        return; // local mode: no socket
    };
    if socket.get_channel(0).is_err() {
        return; // channel already taken (session started)
    }
    socket.update_peers();
    let players = socket.players();
    if players.len() < launch.players {
        return;
    }
    info!("all {} players connected — restarting into p2p session", launch.players);

    // Tear down the warmup world; the Session's absence next frame makes
    // bevy_ggrs reset its frame counter and snapshot state.
    for entity in &rollback_entities {
        commands.entity(entity).despawn();
    }
    commands.remove_resource::<Session<SessionConfig>>();
    let channel = socket.take_channel(0).expect("take ggrs channel");
    commands.insert_resource(PendingSession { players, channel: Some(channel) });
}

/// One frame after teardown: build the real p2p session and the fresh world.
/// Handle order comes from matchbox's sorted `players()` list, so every peer
/// agrees on who is which handle. Ordered BEFORE `wait_for_players` in the
/// schedule so the bevy_ggrs no-session reset tick happens in between.
pub fn finalize_p2p_session(
    mut commands: Commands,
    pending: Option<ResMut<PendingSession>>,
    launch: Res<LaunchConfig>,
    mut next_state: ResMut<NextState<AppState>>,
) {
    let Some(mut pending) = pending else { return };
    let Some(channel) = pending.channel.take() else { return };

    let mut builder = SessionBuilder::<SessionConfig>::new()
        .with_num_players(launch.players)
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
    spawn_world(&mut commands, launch.players);
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
