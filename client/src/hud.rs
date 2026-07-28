//! HUD overlays (bevy_ui). The round clock and series score, the player list
//! (upper right: auto-numbered players grouped by side, "(you)" on the local
//! one, kills and deaths, per-peer ping in a p2p session, lobby status while the
//! room is open), the local player's health bar, the winner banner between
//! rounds, and the host's START button.

use bevy::prelude::*;
use bevy_ggrs::{LocalPlayers, Session};

use army_ghosts_sim::{
    Bot, Deaths, Health, Kills, Phase, Player, Round, Team, Winner, FP, TEAM_COUNT, TICK_HZ,
};

use crate::net::Lobby;
use crate::render::TEAM_NAMES;
use crate::{AppState, LaunchConfig, SessionConfig};

#[derive(Component)]
pub struct PlayerListText;

/// The health bar's track (the dark trough) and the fill inside it. Top-center:
/// the corners are taken (MENU upper-left, roster upper-right) and the bottom
/// belongs to the thumbs.
#[derive(Component)]
pub struct HealthBar;

#[derive(Component)]
pub struct HealthFill;

const HEALTH_BAR_W: f32 = 168.0;
const HEALTH_BAR_H: f32 = 12.0;
/// Distance from the top edge. The roster hangs off the bottom of this, so the
/// two never share a line.
const HEALTH_BAR_TOP: f32 = ROUND_LINE_TOP + ROUND_LINE_H + 4.0;
/// The fill runs from green through amber to red as the bar empties. Read off
/// the fraction rather than off thresholds, so the colour is a continuous
/// reading of how much trouble you're in.
const HEALTH_FULL: Color = Color::srgb(0.44, 0.76, 0.30);
const HEALTH_HALF: Color = Color::srgb(0.90, 0.72, 0.20);
const HEALTH_LOW: Color = Color::srgb(0.88, 0.24, 0.18);

/// The line above the health bar: `GREEN 2 - 1 TAN    1:47`.
#[derive(Component)]
pub struct RoundText;

/// The between-rounds banner, centre screen.
#[derive(Component)]
pub struct RoundBanner;

#[derive(Component)]
pub struct RoundBannerText;

#[derive(Component)]
pub struct StartButton;

/// Lobby status line, centered at the bottom of the screen (below the START
/// button's row): share hint, waiting-for-host, connecting, starting.
#[derive(Component)]
pub struct StatusText;

#[derive(Component)]
pub struct CopyLinkButton;

#[derive(Component)]
pub struct CopyLinkLabel;

/// Reverts the copy button's "COPIED!" flash back to "COPY LINK".
#[derive(Resource)]
pub struct CopiedFlash(pub Timer);

/// The tappable START band: bottom-center of the screen, in 0-1 window
/// fractions (window coords, y-down). The visual button sits inside it; the
/// hit zone is deliberately larger for thumbs. (Taps that land on the ADS
/// button — which lives in the same corner of the screen — are excluded in
/// [`read_start_input`].)
const START_BAND_X: (f32, f32) = (0.25, 0.75);
const START_BAND_Y: f32 = 0.62;

/// Where the round line sits: above the health bar, which is why the bar itself
/// starts lower than it used to.
const ROUND_LINE_TOP: f32 = 8.0;
const ROUND_LINE_H: f32 = 18.0;

pub fn setup_hud(mut commands: Commands) {
    // Top-center round line, above the health bar.
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            top: Val::Px(ROUND_LINE_TOP),
            left: Val::Px(0.0),
            right: Val::Px(0.0),
            justify_content: JustifyContent::Center,
            ..default()
        })
        .with_children(|row| {
            row.spawn((
                RoundText,
                Text::new(""),
                TextFont { font_size: 15.0, ..default() },
                TextColor(Color::srgba(0.92, 0.96, 0.85, 0.92)),
                TextLayout::new_with_justify(Justify::Center),
            ));
        });

    // The between-rounds banner. Its own full-screen node so it centres on the
    // window rather than on whatever the camera is looking at — the camera may
    // be following a teammate by the time this appears.
    commands
        .spawn((
            RoundBanner,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(0.0),
                right: Val::Px(0.0),
                top: Val::Px(0.0),
                bottom: Val::Px(0.0),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            Visibility::Hidden,
        ))
        .with_children(|screen| {
            screen
                .spawn((
                    Node {
                        padding: UiRect::axes(Val::Px(26.0), Val::Px(14.0)),
                        border_radius: BorderRadius::all(Val::Px(12.0)),
                        ..default()
                    },
                    BackgroundColor(Color::srgba(0.05, 0.07, 0.04, 0.82)),
                ))
                .with_children(|pill| {
                    pill.spawn((
                        RoundBannerText,
                        Text::new(""),
                        TextFont { font_size: 26.0, ..default() },
                        TextColor(Color::srgb(0.95, 1.0, 0.90)),
                        TextLayout::new_with_justify(Justify::Center),
                    ));
                });
        });

    // Top-right row: [COPY LINK] beside the roster text (top-aligned; the
    // roster grows downward as players join). Sits *below* the health bar's
    // bottom edge — on a narrow screen the bar's right end and the roster's
    // left end would otherwise meet in the middle.
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            top: Val::Px(HEALTH_BAR_TOP + HEALTH_BAR_H + 8.0),
            right: Val::Px(10.0),
            column_gap: Val::Px(10.0),
            align_items: AlignItems::FlexStart,
            ..default()
        })
        .with_children(|row| {
            row.spawn((
                CopyLinkButton,
                Button,
                Node {
                    padding: UiRect::axes(Val::Px(10.0), Val::Px(4.0)),
                    border_radius: BorderRadius::all(Val::Px(6.0)),
                    ..default()
                },
                BackgroundColor(Color::srgba(0.16, 0.22, 0.10, 0.85)),
                Visibility::Hidden,
            ))
            .with_children(|pill| {
                pill.spawn((
                    CopyLinkLabel,
                    Text::new("COPY LINK"),
                    TextFont { font_size: 12.0, ..default() },
                    TextColor(Color::srgb(0.85, 0.92, 0.75)),
                ));
            });
            row.spawn((
                PlayerListText,
                Text::new(""),
                TextFont { font_size: 14.0, ..default() },
                TextColor(Color::srgba(0.92, 0.96, 0.85, 0.9)),
                TextLayout::new_with_justify(Justify::Right),
            ));
        });

    // Top-center health bar (hidden until there's a local pawn to report on).
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            top: Val::Px(HEALTH_BAR_TOP),
            left: Val::Px(0.0),
            right: Val::Px(0.0),
            justify_content: JustifyContent::Center,
            ..default()
        })
        .with_children(|row| {
            row.spawn((
                HealthBar,
                Node {
                    width: Val::Px(HEALTH_BAR_W),
                    height: Val::Px(HEALTH_BAR_H),
                    border_radius: BorderRadius::all(Val::Px(HEALTH_BAR_H / 2.0)),
                    padding: UiRect::all(Val::Px(2.0)),
                    ..default()
                },
                BackgroundColor(Color::srgba(0.05, 0.07, 0.04, 0.75)),
                Visibility::Hidden,
            ))
            .with_children(|track| {
                track.spawn((
                    HealthFill,
                    Node {
                        width: Val::Percent(100.0),
                        height: Val::Percent(100.0),
                        border_radius: BorderRadius::all(Val::Px(HEALTH_BAR_H / 2.0)),
                        ..default()
                    },
                    BackgroundColor(HEALTH_FULL),
                ));
            });
        });

    // Bottom-center status line, stacked above the ADS button.
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            left: Val::Px(0.0),
            right: Val::Px(0.0),
            bottom: Val::Px(112.0),
            justify_content: JustifyContent::Center,
            ..default()
        })
        .with_children(|row| {
            row.spawn((
                StatusText,
                Text::new(""),
                TextFont { font_size: 14.0, ..default() },
                TextColor(Color::srgba(0.85, 0.92, 0.75, 0.85)),
                TextLayout::new_with_justify(Justify::Center),
            ));
        });

    // Host-only START button (full-width row centers the pill).
    commands
        .spawn((
            StartButton,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(0.0),
                right: Val::Px(0.0),
                bottom: Val::Px(150.0),
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

/// The local pawn's health. Hidden whenever there isn't one to report on (the
/// lobby, or while you're dead — the respawn countdown in [`StatusText`] is the
/// message then, and an empty bar sitting there would just be noise).
pub fn update_health_bar(
    local: Option<Res<LocalPlayers>>,
    players: Query<(&Player, &Health)>,
    mut tracks: Query<&mut Visibility, With<HealthBar>>,
    mut fills: Query<(&mut Node, &mut BackgroundColor), With<HealthFill>>,
) {
    let health = local
        .as_deref()
        .and_then(|l| l.0.first().copied())
        .and_then(|handle| players.iter().find(|(p, _)| p.handle == handle))
        .map(|(_, health)| *health)
        .filter(|health| health.alive());

    for mut visibility in &mut tracks {
        let wanted = if health.is_some() { Visibility::Inherited } else { Visibility::Hidden };
        if *visibility != wanted {
            *visibility = wanted;
        }
    }
    let Some(health) = health else { return };
    let fraction = health.fraction() as f32 / FP as f32;
    // Green -> amber over the top half, amber -> red over the bottom.
    let color = if fraction > 0.5 {
        HEALTH_HALF.mix(&HEALTH_FULL, (fraction - 0.5) * 2.0)
    } else {
        HEALTH_LOW.mix(&HEALTH_HALF, fraction * 2.0)
    };
    for (mut node, mut background) in &mut fills {
        node.width = Val::Percent(fraction * 100.0);
        background.0 = color;
    }
}

/// `1:47` — the round clock, minutes and seconds.
fn clock(ticks: u32) -> String {
    // Rounded UP, so the last second reads 0:01 for the whole of it rather than
    // sitting on 0:00 while the round is still being fought.
    let seconds = (ticks as usize).div_ceil(TICK_HZ);
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// `GREEN 2 - 1 TAN    1:47`, or the count-in while the banner is up.
///
/// One line for the score and the clock together because they answer the same
/// question — how much of this is left and does it matter — and because the top
/// of a phone screen has room for one line, not two.
pub fn update_round_text(
    state: Res<State<AppState>>,
    round: Option<Res<Round>>,
    mut texts: Query<&mut Text, With<RoundText>>,
) {
    let Ok(mut text) = texts.single_mut() else { return };
    let line = match (state.get(), round.as_deref()) {
        (AppState::InGame, Some(round)) => {
            let tail = match round.phase {
                Phase::Live => clock(round.remaining()),
                Phase::Over(_) => format!("NEXT IN {}", clock(round.intermission_left())),
            };
            format!(
                "{} {} - {} {}    {}",
                TEAM_NAMES[0], round.wins[0], round.wins[1], TEAM_NAMES[1], tail
            )
        }
        _ => String::new(),
    };
    if text.0 != line {
        text.0 = line;
    }
}

/// The winner, centre screen, for as long as the intermission lasts.
pub fn update_round_banner(
    state: Res<State<AppState>>,
    round: Option<Res<Round>>,
    mut banners: Query<&mut Visibility, With<RoundBanner>>,
    mut texts: Query<&mut Text, With<RoundBannerText>>,
) {
    let winner = round
        .as_deref()
        .filter(|_| matches!(state.get(), AppState::InGame))
        .and_then(|round| round.winner().map(|winner| (round.number, winner)));
    for mut visibility in &mut banners {
        let wanted = if winner.is_some() { Visibility::Inherited } else { Visibility::Hidden };
        if *visibility != wanted {
            *visibility = wanted;
        }
    }
    let Some((number, winner)) = winner else { return };
    let line = match winner {
        Winner::Team(side) => format!(
            "{} WINS ROUND {number}",
            TEAM_NAMES[(side as usize).min(TEAM_COUNT - 1)]
        ),
        Winner::Draw => format!("ROUND {number} DRAWN"),
    };
    for mut text in &mut texts {
        if text.0 != line {
            text.0 = line.clone();
        }
    }
}

/// The bottom-center lobby status. The host with company gets no line — the
/// START button right above it IS the message.
pub fn update_status_text(
    state: Res<State<AppState>>,
    launch: Res<LaunchConfig>,
    lobby: Res<Lobby>,
    local: Option<Res<LocalPlayers>>,
    players: Query<(&Player, &Health)>,
    mut texts: Query<&mut Text, With<StatusText>>,
) {
    let Ok(mut text) = texts.single_mut() else { return };
    // Being out outranks any lobby message: it is the only line that answers a
    // question you are asking right now. There is no countdown any more — the
    // answer is "the next round", and the clock at the top of the screen already
    // says how long that is.
    let down = local
        .as_deref()
        .and_then(|l| l.0.first().copied())
        .and_then(|handle| players.iter().find(|(p, _)| p.handle == handle))
        .map(|(_, health)| health.down)
        .filter(|down| *down > 0);
    if down.is_some() {
        let line = "ELIMINATED - SPECTATING".to_string();
        if text.0 != line {
            text.0 = line;
        }
        return;
    }
    // ASCII dots — the embedded default font has no "…" glyph.
    let status = match state.get() {
        AppState::Connecting if launch.room.is_some() => {
            if lobby.roster.is_some() {
                "starting..."
            } else if lobby.ids.is_empty() {
                "connecting..."
            } else if lobby.ids.len() == 1 {
                "share the link to invite others"
            } else if lobby.is_host {
                ""
            } else {
                "waiting for host to start..."
            }
        }
        _ => "",
    };
    if text.0 != status {
        text.0 = status.into();
    }
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

/// Show the COPY LINK pill beside the roster whenever we're in a room's
/// lobby (that's when you're recruiting).
pub fn update_copy_button(
    state: Res<State<AppState>>,
    launch: Res<LaunchConfig>,
    mut buttons: Query<&mut Visibility, With<CopyLinkButton>>,
) {
    let show = matches!(state.get(), AppState::Connecting) && launch.room.is_some();
    for mut visibility in &mut buttons {
        *visibility = if show { Visibility::Inherited } else { Visibility::Hidden };
    }
}

/// Put the join URL on the clipboard and flash "COPIED!" on the button.
pub fn copy_link_pressed(
    mut commands: Commands,
    interactions: Query<&Interaction, (Changed<Interaction>, With<CopyLinkButton>)>,
    launch: Res<LaunchConfig>,
    mut labels: Query<&mut Text, With<CopyLinkLabel>>,
) {
    for interaction in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        let Some(room) = &launch.room else { continue };
        let url = copy_share_url(room);
        info!("share url: {url}");
        for mut label in &mut labels {
            label.0 = "COPIED!".into();
        }
        commands.insert_resource(CopiedFlash(Timer::from_seconds(1.5, TimerMode::Once)));
    }
}

pub fn tick_copied_flash(
    mut commands: Commands,
    flash: Option<ResMut<CopiedFlash>>,
    time: Res<Time>,
    mut labels: Query<&mut Text, With<CopyLinkLabel>>,
) {
    let Some(mut flash) = flash else { return };
    if flash.0.tick(time.delta()).is_finished() {
        for mut label in &mut labels {
            label.0 = "COPY LINK".into();
        }
        commands.remove_resource::<CopiedFlash>();
    }
}

/// Web: write the full join URL to the clipboard (async fire-and-forget —
/// the promise runs on its own; localhost and https are secure contexts, so
/// this works everywhere we serve from). Native: nothing to share a browser
/// URL from — log it. Both return the URL for the log line.
#[cfg(target_arch = "wasm32")]
fn copy_share_url(room: &str) -> String {
    let mut url = format!("?room={room}");
    if let Some(window) = web_sys::window() {
        let location = window.location();
        if let (Ok(origin), Ok(path)) = (location.origin(), location.pathname()) {
            url = format!("{origin}{path}?room={room}");
        }
        let _ = window.navigator().clipboard().write_text(&url);
    }
    url
}

#[cfg(not(target_arch = "wasm32"))]
fn copy_share_url(room: &str) -> String {
    format!("?room={room}")
}

/// Start-trigger input: Enter anywhere, or a tap/click in the bottom-center
/// band under the button. `run_lobby` only honors it on the host with ≥2
/// players present, so stray taps are harmless.
pub fn read_start_input(
    keys: Res<ButtonInput<KeyCode>>,
    mouse: Res<ButtonInput<MouseButton>>,
    touches: Res<Touches>,
    windows: Query<&Window>,
    ui_buttons: Query<&Interaction, With<Button>>,
    mut lobby: ResMut<Lobby>,
) {
    if keys.just_pressed(KeyCode::Enter) {
        lobby.start_requested = true;
        return;
    }
    // The sights and stance buttons sit inside the band; working the controls
    // must not start the match. (`ui_focus_system` has already stamped this
    // frame's Interaction.)
    if ui_buttons.iter().any(|i| *i == Interaction::Pressed) {
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

/// `"5k 3d"` — the board is a scoreboard, so everyone carries a count from the
/// first tick rather than sprouting one the moment they die.
///
/// Abbreviated rather than spelled out because this line already carries a name
/// and, in a room, a ping, and the roster hangs off the right edge of a phone
/// screen. It is deliberately NARROWER than the "3 deaths" it replaced.
fn score_label(kills: u32, deaths: u32) -> String {
    format!("{kills}k {deaths}d")
}

/// A bevy system takes one parameter per thing it reads; this one reads the
/// session, the lobby, who is local and the whole scoreboard.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub fn update_player_list(
    state: Res<State<AppState>>,
    lobby: Res<Lobby>,
    session: Option<Res<Session<SessionConfig>>>,
    local_players: Option<Res<LocalPlayers>>,
    scores: Query<(&Player, &Team, &Health, &Kills, &Deaths, Option<&Bot>)>,
    mut texts: Query<&mut Text, With<PlayerListText>>,
) {
    let Ok(mut text) = texts.single_mut() else { return };

    let mut lines: Vec<String> = Vec::new();
    match state.get() {
        // Lobby: the sorted room roster IS the future handle order, so the
        // numbering here matches the in-game one. Status lives in the
        // bottom-center [`StatusText`], not here.
        AppState::Connecting => {
            for (i, id) in lobby.ids.iter().enumerate() {
                let you = if Some(*id) == lobby.my_id { " (you)" } else { "" };
                lines.push(format!("Player {}{}", i + 1, you));
            }
        }
        AppState::InGame => {
            let num_players = match session.as_deref() {
                Some(Session::P2P(s)) => s.num_players(),
                Some(Session::SyncTest(s)) => s.num_players(),
                _ => 0,
            };
            let local = local_players.map(|l| l.0.clone()).unwrap_or_default();
            // Walk the PAWNS, not the session's handles: bots are pawns without
            // a seat, so anything counting seats would leave them off the board
            // while they were busy killing people. Sorted BY SIDE first, because
            // the question the roster answers in a team game is "who is left on
            // mine" and that is unreadable if the two sides are interleaved.
            let mut board: Vec<(u8, usize, bool, u32, u32, bool)> = scores
                .iter()
                .map(|(player, team, health, kills, deaths, bot)| {
                    (team.0, player.handle, health.alive(), kills.0, deaths.0, bot.is_some())
                })
                .collect();
            board.sort_unstable();
            let mut shown_side: Option<u8> = None;
            for (side, handle, alive, kills, deaths, is_bot) in board {
                if shown_side != Some(side) {
                    // A blank line between the sides, and the side's name over
                    // each block — the roster is the only place the game says
                    // out loud which colour you ended up.
                    if shown_side.is_some() {
                        lines.push(String::new());
                    }
                    lines.push(format!("- {} -", TEAM_NAMES[(side as usize).min(TEAM_COUNT - 1)]));
                    shown_side = Some(side);
                }
                let who = if is_bot {
                    // Numbered within the bots rather than continuing the player
                    // numbering: bots take the handles above the humans, so the
                    // first one is `num_players`.
                    format!("Bot {}", handle.saturating_sub(num_players) + 1)
                } else {
                    let you = if local.contains(&handle) { " (you)" } else { "" };
                    format!("Player {}{}", handle + 1, you)
                };
                // GGRS-measured roundtrip time to each remote peer. Errors
                // (local handles, or a peer still synchronizing) → no ping. Bots
                // are local by construction and never have one.
                let ping = match session.as_deref() {
                    Some(Session::P2P(s)) if !is_bot => s
                        .network_stats(handle)
                        .map(|stats| format!(" {}ms", stats.ping))
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                // Both counts come out of the sim, so every peer's board agrees
                // without anyone sending a score message. The dagger is who is
                // out of this round: with no respawns, that is the single most
                // useful thing on the board.
                let out = if alive { "" } else { " +" };
                lines.push(format!("{}{}  {}{}", who, out, score_label(kills, deaths), ping));
            }
        }
    }
    let joined = lines.join("\n");
    if text.0 != joined {
        text.0 = joined;
    }
}
