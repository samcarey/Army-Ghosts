//! Army Ghosts client: rendering, input, and session bring-up around the
//! deterministic `army-ghosts-sim` core. Runs native (dev loop) and as WASM in
//! the browser (the real target — mobile Safari/Chrome).

mod ads;
mod hud;
mod input;
mod menu;
mod net;
mod render;
mod touch;

pub use net::LaunchConfig;

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
    .init_resource::<ads::Ads>()
    .init_resource::<render::CameraFocus>()
    .init_resource::<net::Lobby>()
    .init_state::<AppState>()
    .add_systems(
        Startup,
        (
            render::setup_scene,
            net::begin_session_setup,
            touch::setup_overlay,
            hud::setup_hud,
            menu::setup_menu,
            ads::setup_ads,
        )
            .chain(),
    )
    .add_systems(
        Update,
        (
            // finalize BEFORE the lobby: the two run a frame apart so
            // bevy_ggrs gets its no-session reset tick between warmup and p2p.
            (hud::read_start_input, net::finalize_p2p_session, net::run_lobby)
                .chain()
                .run_if(in_state(AppState::Connecting)),
            net::log_ggrs_events.run_if(in_state(AppState::InGame)),
            (
                hud::update_player_list,
                hud::update_status_text,
                hud::update_start_button,
                hud::update_copy_button,
            ),
            (hud::copy_link_pressed, hud::tick_copied_flash).chain(),
            menu::menu_interactions,
            (touch::read_touches, touch::update_overlay).chain(),
            // advance_ads owns the aim transition; the aim line and the camera
            // shift both read it, so they follow it in the same frame.
            (
                ads::toggle_ads,
                ads::advance_ads,
                ads::update_ads_button,
                render::attach_sprites,
                render::orient_players,
                render::animate_players,
                render::bullet_trails,
                render::sync_transforms,
                ads::update_aim_line,
                render::camera_follow,
            )
                .chain(),
        ),
    );

    // Loader handshake: flip <body data-game-ready> once frames are rendering
    // so the HTML loading screen knows when to fade out (web only).
    #[cfg(target_arch = "wasm32")]
    app.add_systems(Update, render::signal_game_ready);

    app.run();
}
