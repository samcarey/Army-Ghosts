//! HUD overlays (bevy_ui). The player list (upper right: auto-numbered
//! players, "(you)" on the local one, per-peer ping in a p2p session, lobby
//! status while the room is open) and the host's START button.

use bevy::prelude::*;
use bevy_ggrs::{LocalPlayers, Session};

use crate::net::Lobby;
use crate::{AppState, SessionConfig};

#[derive(Component)]
pub struct PlayerListText;

#[derive(Component)]
pub struct StartButton;

/// The tappable START band: bottom-center of the screen, in 0-1 window
/// fractions (window coords, y-down). The visual button sits inside it; the
/// hit zone is deliberately larger for thumbs.
const START_BAND_X: (f32, f32) = (0.25, 0.75);
const START_BAND_Y: f32 = 0.72;

pub fn setup_hud(mut commands: Commands) {
    commands.spawn((
        PlayerListText,
        Text::new(""),
        TextFont { font_size: 14.0, ..default() },
        TextColor(Color::srgba(0.92, 0.96, 0.85, 0.9)),
        TextLayout::new_with_justify(Justify::Right),
        Node {
            position_type: PositionType::Absolute,
            top: Val::Px(8.0),
            right: Val::Px(10.0),
            ..default()
        },
    ));

    // Host-only START button (full-width row centers the pill).
    commands
        .spawn((
            StartButton,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(0.0),
                right: Val::Px(0.0),
                bottom: Val::Percent(12.0),
                justify_content: JustifyContent::Center,
                ..default()
            },
            Visibility::Hidden,
        ))
        .with_children(|row| {
            row.spawn((
                Node {
                    padding: UiRect::axes(Val::Px(28.0), Val::Px(12.0)),
                    border_radius: BorderRadius::all(Val::Px(10.0)),
                    ..default()
                },
                BackgroundColor(Color::srgba(0.30, 0.50, 0.18, 0.9)),
            ))
            .with_children(|pill| {
                pill.spawn((
                    Text::new("TAP TO START"),
                    TextFont { font_size: 20.0, ..default() },
                    TextColor(Color::srgb(0.93, 1.0, 0.88)),
                ));
            });
        });
}

/// Show the START button only to the host, once there's someone to play with
/// and the start hasn't been triggered yet.
pub fn update_start_button(
    state: Res<State<AppState>>,
    lobby: Res<Lobby>,
    mut buttons: Query<&mut Visibility, With<StartButton>>,
) {
    let show = matches!(state.get(), AppState::Connecting)
        && lobby.is_host
        && lobby.ids.len() >= 2
        && lobby.roster.is_none();
    for mut visibility in &mut buttons {
        *visibility = if show { Visibility::Visible } else { Visibility::Hidden };
    }
}

/// Start-trigger input: Enter anywhere, or a tap/click in the bottom-center
/// band under the button. `run_lobby` only honors it on the host with ≥2
/// players present, so stray taps are harmless.
pub fn read_start_input(
    keys: Res<ButtonInput<KeyCode>>,
    mouse: Res<ButtonInput<MouseButton>>,
    touches: Res<Touches>,
    windows: Query<&Window>,
    mut lobby: ResMut<Lobby>,
) {
    if keys.just_pressed(KeyCode::Enter) {
        lobby.start_requested = true;
        return;
    }
    let Ok(window) = windows.single() else { return };
    let (w, h) = (window.width(), window.height());
    let in_band = |p: Vec2| {
        p.x > w * START_BAND_X.0 && p.x < w * START_BAND_X.1 && p.y > h * START_BAND_Y
    };
    if mouse.just_pressed(MouseButton::Left) {
        if let Some(pos) = window.cursor_position() {
            if in_band(pos) {
                lobby.start_requested = true;
            }
        }
    }
    for touch in touches.iter_just_pressed() {
        if in_band(touch.position()) {
            lobby.start_requested = true;
        }
    }
}

pub fn update_player_list(
    state: Res<State<AppState>>,
    lobby: Res<Lobby>,
    session: Option<Res<Session<SessionConfig>>>,
    local_players: Option<Res<LocalPlayers>>,
    mut texts: Query<&mut Text, With<PlayerListText>>,
) {
    let Ok(mut text) = texts.single_mut() else { return };

    let mut lines: Vec<String> = Vec::new();
    match state.get() {
        // Lobby: the sorted room roster IS the future handle order, so the
        // numbering here matches the in-game one.
        AppState::Connecting => {
            if lobby.ids.is_empty() {
                // ASCII dots — the embedded default font has no "…" glyph.
                lines.push("connecting...".into());
            } else {
                for (i, id) in lobby.ids.iter().enumerate() {
                    let you = if Some(*id) == lobby.my_id { " (you)" } else { "" };
                    lines.push(format!("Player {}{}", i + 1, you));
                }
                if lobby.roster.is_some() {
                    lines.push("starting...".into());
                } else if lobby.ids.len() == 1 {
                    lines.push("share the link to invite...".into());
                } else if lobby.is_host {
                    lines.push("tap START when ready".into());
                } else {
                    lines.push("waiting for host to start...".into());
                }
            }
        }
        AppState::InGame => {
            let num_players = match session.as_deref() {
                Some(Session::P2P(s)) => s.num_players(),
                Some(Session::SyncTest(s)) => s.num_players(),
                _ => 0,
            };
            let local = local_players.map(|l| l.0.clone()).unwrap_or_default();
            for handle in 0..num_players {
                let you = if local.contains(&handle) { " (you)" } else { "" };
                // GGRS-measured roundtrip time to each remote peer. Errors
                // (local handles, or a peer still synchronizing) → no ping.
                let ping = match session.as_deref() {
                    Some(Session::P2P(s)) => s
                        .network_stats(handle)
                        .map(|stats| format!(" {}ms", stats.ping))
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                lines.push(format!("Player {}{}{}", handle + 1, you, ping));
            }
        }
    }
    let joined = lines.join("\n");
    if text.0 != joined {
        text.0 = joined;
    }
}
