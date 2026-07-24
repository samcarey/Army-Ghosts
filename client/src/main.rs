//! Army Ghosts client: rendering, input, and session bring-up around the
//! deterministic `army-ghosts-sim` core. Runs native (dev loop) and as WASM in
//! the browser (the real target — mobile Safari/Chrome).

mod input;
mod net;
mod render;
mod touch;

use bevy::prelude::*;
use bevy_ggrs::{GgrsPlugin, ReadInputs};
use bevy_matchbox::matchbox_socket::PeerId;

use army_ghosts_sim::{PlayerInput, SimPlugin};

/// The one ggrs session config used everywhere. The `PeerId` address type is
/// only exercised by real p2p sessions; synctest sessions ignore it, so local
/// mode shares the same config (and therefore the same `SimPlugin`
/// instantiation).
pub type SessionConfig = bevy_ggrs::GgrsConfig<PlayerInput, PeerId>;

#[derive(States, Clone, Eq, PartialEq, Hash, Debug, Default)]
pub enum AppState {
    /// Waiting for peers on the matchbox signaling server (skipped in local
    /// synctest mode).
    #[default]
    Connecting,
    InGame,
}

fn main() {
    let launch = net::launch_config();
    info!("launch config: {launch:?}");

    let mut app = App::new();
    app.add_plugins(
        DefaultPlugins
            .set(WindowPlugin {
                primary_window: Some(Window {
                    title: "Army Ghosts".into(),
                    ..default()
                }),
                ..default()
            })
            // matchbox at debug is invaluable when p2p bring-up misbehaves —
            // it traces the whole signaling + ICE handshake.
            .set(bevy::log::LogPlugin {
                filter: "info,wgpu=error,naga=warn".into(),
                ..default()
            }),
    )
    .add_plugins(GgrsPlugin::<SessionConfig>::default())
    .add_plugins(SimPlugin::<SessionConfig>::default())
    .add_systems(ReadInputs, input::read_local_inputs)
    .insert_resource(launch)
    .init_resource::<touch::TouchControls>()
    .init_state::<AppState>()
    .add_systems(
        Startup,
        (render::setup_scene, net::begin_session_setup, touch::setup_overlay).chain(),
    )
    .add_systems(
        Update,
        (
            net::wait_for_players.run_if(in_state(AppState::Connecting)),
            net::log_ggrs_events.run_if(in_state(AppState::InGame)),
            (touch::read_touches, touch::update_overlay).chain(),
            (
                render::attach_sprites,
                render::orient_players,
                render::sync_transforms,
                render::camera_follow,
            )
                .chain(),
        ),
    );

    app.run();
}
