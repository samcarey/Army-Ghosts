//! HUD overlays (bevy_ui). Currently just the player list, upper right:
//! auto-numbered players, "(you)" on the local one, and a waiting line while
//! the matchbox room fills.

use bevy::prelude::*;
use bevy_ggrs::{LocalPlayers, Session};
use bevy_matchbox::prelude::*;

use crate::{AppState, LaunchConfig, SessionConfig};

#[derive(Component)]
pub struct PlayerListText;

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
}

pub fn update_player_list(
    state: Res<State<AppState>>,
    launch: Res<LaunchConfig>,
    socket: Option<Res<MatchboxSocket>>,
    session: Option<Res<Session<SessionConfig>>>,
    local_players: Option<Res<LocalPlayers>>,
    mut texts: Query<&mut Text, With<PlayerListText>>,
) {
    let Ok(mut text) = texts.single_mut() else { return };

    let mut lines: Vec<String> = Vec::new();
    match state.get() {
        // Warmup: you're always provisionally Player 1; connected peers fill
        // in behind you (final numbering comes from the sorted peer-id list
        // when the real session starts).
        AppState::Connecting => {
            lines.push("Player 1 (you)".into());
            let connected = socket
                .as_ref()
                .map(|s| s.connected_peers().count())
                .unwrap_or(0);
            for i in 0..connected {
                lines.push(format!("Player {}", i + 2));
            }
            // ASCII dots — the embedded default font has no "…" glyph.
            lines.push(format!("waiting {}/{}...", connected + 1, launch.players));
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
                lines.push(format!("Player {}{}", handle + 1, you));
            }
        }
    }
    let joined = lines.join("\n");
    if text.0 != joined {
        text.0 = joined;
    }
}
