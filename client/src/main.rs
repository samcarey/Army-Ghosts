//! Army Ghosts client: rendering, input, and session bring-up around the
//! deterministic `army-ghosts-sim` core. Runs native (dev loop) and as WASM in
//! the browser (the real target — mobile Safari/Chrome).

mod ads;
mod grass;
mod hud;
mod input;
mod menu;
mod nameplate;
mod net;
mod render;
mod spectate;
mod stance;
mod touch;
mod vision;

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
    .add_plugins(vision::VisionPlugin)
    .add_plugins(grass::GrassPlugin)
    .add_plugins(GgrsPlugin::<SessionConfig>::default())
    .add_plugins(SimPlugin::<SessionConfig>::default())
    .add_systems(ReadInputs, input::read_local_inputs)
    // Which world this is, as its own resource: the renderer asks it how deep
    // the grass is (`Scenario::depth`) and it decides what `spawn_world`
    // builds, so everything downstream reads one value instead of pattern
    // matching on the launch config.
    .insert_resource(launch.scenario)
    // Seeds the menu's dial from `?bots=` / `AG_BOTS`, so the URL and the UI
    // are the same setting rather than two.
    .insert_resource(menu::BotCount(launch.bots))
    // …and the aggression dial the same way, so `?aggro=` and the menu row are
    // one setting. Unset leaves it on the shipping profile's value.
    .insert_resource(
        launch
            .aggro
            .map(|percent| menu::Aggression(menu::level_of_percent(percent)))
            .unwrap_or_default(),
    )
    .insert_resource(launch)
    // Which side this player is asking for. Nothing seeds it from the URL: it
    // is a mid-match choice, not a launch setting, and `round::balance` may well
    // overrule it anyway.
    .init_resource::<menu::SidePick>()
    .init_resource::<spectate::Spectating>()
    .init_resource::<touch::TouchControls>()
    .init_resource::<ads::Ads>()
    .init_resource::<stance::StanceControl>()
    .init_resource::<render::CameraFocus>()
    .init_resource::<net::Lobby>()
    .init_state::<AppState>()
    .add_systems(
        Startup,
        (
            render::setup_scene,
            grass::setup_grass,
            net::begin_session_setup,
            touch::setup_overlay,
            hud::setup_hud,
            menu::setup_menu,
            ads::setup_ads,
            stance::setup_stance,
            spectate::setup_spectate,
            nameplate::setup_nameplates,
            vision::setup_fog,
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
                hud::update_health_bar,
                hud::update_round_text,
                hud::update_round_banner,
            ),
            (hud::copy_link_pressed, hud::tick_copied_flash).chain(),
            menu::menu_interactions,
            menu::update_dial_labels,
            (touch::read_touches, touch::update_overlay).chain(),
            // advance_ads owns the aim transition; the aim line and the camera
            // shift both read it, so they follow it in the same frame.
            (
                ads::toggle_ads,
                ads::advance_ads,
                ads::update_ads_button,
                stance::read_stance_input,
                stance::update_stance_buttons,
                // Before `camera_follow` below, which reads the target it
                // picks — so the frame you die is the frame the camera starts
                // moving, rather than the one after it.
                spectate::update_spectate,
                spectate::update_spectate_button,
                render::attach_sprites,
                grass::attach_grass_shade,
                render::animate_players,
                // Owns the pawns' rgb; `fade_hidden` below owns their alpha.
                render::update_health_visuals,
                render::bullet_trails,
                render::sync_transforms,
                grass::update_grass_shade,
                ads::update_aim_line,
                vision::update_fog,
                vision::fade_hidden,
                render::camera_follow,
                // AFTER the camera: nameplates are screen positions, and one
                // computed against last frame's camera slides about as you walk.
                nameplate::update_nameplates,
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
