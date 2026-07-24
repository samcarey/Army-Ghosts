//! Session bring-up: launch-config parsing (URL params on web, env vars on
//! native), matchbox signaling, and GGRS session construction.
//!
//! Modes:
//! - `room` set → p2p: connect to the signaling server, wait for `players`
//!   peers, start a GGRS p2p session over the matchbox WebRTC channel.
//! - no `room` → local synctest: all handles local, GGRS re-simulates every
//!   frame (`check_distance`) — the determinism canary AND the offline mode.

use bevy::prelude::*;
use bevy_ggrs::ggrs::{DesyncDetection, SessionBuilder};
use bevy_ggrs::Session;
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
}

const DEFAULT_SIGNALING: &str = "ws://127.0.0.1:3536";

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
    LaunchConfig { room, players, signaling }
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
    LaunchConfig { room, players, signaling }
}

/// Startup: either open the matchbox socket (p2p) or start the synctest
/// session immediately (local).
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
            info!("connecting to matchbox room {url}");
            // Single unreliable (UDP-like) channel — exactly what GGRS wants;
            // matchbox's `ggrs` feature impls NonBlockingSocket on it.
            commands.insert_resource(MatchboxSocket::new_unreliable(url));
        }
        None => {
            info!("no room — starting local synctest session ({} players)", launch.players);
            let mut builder = SessionBuilder::<SessionConfig>::new()
                .with_num_players(launch.players)
                .with_check_distance(2);
            for handle in 0..launch.players {
                builder = builder
                    .add_player(bevy_ggrs::ggrs::PlayerType::Local, handle)
                    .expect("add local player");
            }
            let session = builder.start_synctest_session().expect("start synctest");
            commands.insert_resource(Session::SyncTest(session));
            spawn_world(&mut commands, launch.players);
            next_state.set(AppState::InGame);
        }
    }
}

/// Poll the signaling connection until everyone's here, then hand the WebRTC
/// channel to GGRS. Handle order comes from matchbox's sorted `players()`
/// list, so every peer agrees on who is which handle.
pub fn wait_for_players(
    mut commands: Commands,
    socket: Option<ResMut<MatchboxSocket>>,
    launch: Res<LaunchConfig>,
    mut next_state: ResMut<NextState<AppState>>,
) {
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
    info!("all {} players connected — starting p2p session", launch.players);

    let mut builder = SessionBuilder::<SessionConfig>::new()
        .with_num_players(launch.players)
        .with_input_delay(2)
        // Surface any nondeterminism as an explicit desync event instead of
        // silently diverged worlds (pairs with the sim's Pos checksums).
        .with_desync_detection_mode(DesyncDetection::On { interval: 10 })
        .with_fps(TICK_HZ)
        .expect("valid fps");
    for (handle, player) in players.into_iter().enumerate() {
        builder = builder.add_player(player, handle).expect("add player");
    }
    let channel = socket.take_channel(0).expect("take ggrs channel");
    let session = builder.start_p2p_session(channel).expect("start p2p session");
    commands.insert_resource(Session::P2P(session));
    spawn_world(&mut commands, launch.players);
    next_state.set(AppState::InGame);
}

/// Log GGRS session events — desyncs especially. A desync means the integer
/// sim broke determinism somewhere; treat it as a bug, always.
pub fn log_ggrs_events(mut session: ResMut<Session<SessionConfig>>) {
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
    }
}
