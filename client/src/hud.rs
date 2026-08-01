//! HUD overlays (bevy_ui). The round clock and series score, the troop count
//! (upper right), the local player's health bar, the between-rounds banner and
//! the full scoreboard it carries, the lobby roster and status, and the host's
//! START button.
//!
//! # Where the scoreboard lives, and why it moved
//!
//! The full board — every pawn, both sides, kills, deaths, ping — used to sit in
//! the upper right for the whole match. It is the wrong thing to have there
//! while a round is live: eight lines of text over a quarter of a phone screen,
//! answering a question ("what is everyone's score") that nobody asks with a
//! round in the air, and covering the field you are trying to read. What you
//! actually want mid-round is one number per side — **how many of us are left,
//! how many of them** — which is the whole state of a no-respawn round in nine
//! characters.
//!
//! So the board moved to the banner, and appears exactly when there is nothing
//! else to look at or something has changed:
//! * **Between rounds**, under the result and a count-in to the next one. The
//!   round is over, the field is frozen, and this is when a scoreboard is worth
//!   reading.
//! * **When a pawn joins or leaves** ([`BoardFlash`]), for a few seconds. Bots
//!   arrive and go on a dial, so "who am I actually playing with" changes
//!   mid-match and needs saying.

use bevy::prelude::*;
use bevy_ggrs::{LocalPlayers, Session};

use army_ghosts_sim::{
    Bot, Deaths, Health, Kills, Phase, Player, Round, Team, Winner, FP, MAX_PLAYERS, TEAM_COUNT,
    TICK_HZ,
};

use crate::net::Lobby;
use crate::render::TEAM_NAMES;
use crate::{AppState, LaunchConfig, SessionConfig};

/// The upper-right readout. It carries the joining roster in the lobby (there is
/// no game to count yet) and the per-side troop count once a match is running.
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

/// The line above the health bar: `ALPHA 2 - 1 BRAVO    1:47`.
#[derive(Component)]
pub struct RoundText;

/// The between-rounds banner, centre screen. A full-screen node; the pill it
/// centres is [`BannerPill`].
#[derive(Component)]
pub struct RoundBanner;

/// The banner's dark pill — the part that actually covers ground, and therefore
/// the part an edge nameplate has to dodge (`nameplate::hud_boxes`). Marked
/// separately from [`RoundBanner`], whose box is the whole window and would push
/// every plate clean off the screen.
#[derive(Component)]
pub struct BannerPill;

/// The banner's two text lines, above the board, as one enum component rather
/// than two marker types.
///
/// One system writes both, and two separate `&mut Text` queries would each need
/// a `Without` filter against the other before bevy would accept them as
/// disjoint. An enum states the same thing once.
#[derive(Component, Clone, Copy, PartialEq, Eq)]
pub enum BannerLine {
    /// Who won the round, or what just changed about the roster.
    Headline,
    /// `NEXT ROUND IN 0:04`. Empty outside an intermission.
    Countdown,
}

/// One line of the board: a fixed-width gutter for the skull, then the line.
///
/// The board is a POOL of these rather than one multi-line `Text`, and the skull
/// is what forced that — an icon has to be an `ImageNode`, and a `Text` node
/// cannot hold one. Pooled rather than respawned per board because the pool is
/// twelve nodes that exist for the process's life, against churning UI entities
/// every time a bot lands.
#[derive(Component)]
pub struct BoardRow {
    /// Which line of the board this row IS, fixed when it was spawned.
    ///
    /// Load-bearing, and it cost a screenshot to find out: a row's place on
    /// screen is its place among the container's children, but `Query` iteration
    /// order is archetype order and has nothing to do with spawn order. Handing
    /// out board lines in iteration order therefore scattered them — the `ALPHA`
    /// heading came out UNDER the `BRAVO` block, with the right names under the
    /// wrong side. Every row asks for its own line instead.
    index: usize,
    skull: Entity,
    label: Entity,
}

/// The red skull marking a pawn that is out of the round. It is the FIRST thing
/// in its row, so it hangs to the left of where the names start.
#[derive(Component)]
pub struct BoardSkull;

/// The board's rows, sized for the worst case: a heading and a blank line per
/// side, plus a line per pawn.
const BOARD_ROWS: usize = MAX_PLAYERS + TEAM_COUNT * 2;

/// The gutter a name is indented by, and the icon that sits in it. The icon is
/// `SKULL_PX` square with the rest as its right margin, so the two together are
/// the column every name starts after.
///
/// **The icon's box is there whether or not a skull is showing**, because it is
/// hidden with `Visibility` and a hidden node KEEPS its layout — which is the
/// whole reason the living and the dead line up. `Display::None` would take the
/// box away and step every living pawn's name 22 px left. (`nameplate.rs` uses
/// `Display::None` on its arrow for the opposite reason and says so.)
const SKULL_COL: f32 = 22.0;
const SKULL_PX: f32 = 17.0;
/// Blood red, and bright: it has to carry at 17 px against a near-black pill,
/// and it is the only thing on this HUD that is not some shade of army green.
const SKULL_RED: Color = Color::srgb(0.88, 0.16, 0.13);

/// How long the board stays up after the roster changes, seconds.
const ROSTER_FLASH: f32 = 3.0;

/// Column width for a name on the banner's board. Padding only lines columns up
/// if the glyphs are all one width, and here they are: bevy's embedded
/// `default_font` is FiraMono. (The same font is why the roster's ping and score
/// columns have always aligned.)
const NAME_COL: usize = 22;

/// Why the board is on screen when no round is being counted in: who was on the
/// roster last frame, what changed, and how long that is still worth saying.
///
/// The name is stored beside the handle rather than re-derived, because a pawn
/// that has just LEFT cannot be asked what it was called — and "Bot 3 left" is
/// the whole message.
#[derive(Resource, Default)]
pub struct BoardFlash {
    /// `(handle, name)`, sorted. Handles decide whether anything changed; the
    /// names are only for the headline.
    seen: Vec<(usize, String)>,
    /// Seconds of board left to show.
    left: f32,
    headline: String,
}

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

/// The tappable START band: a generous thumb-sized zone around the pill, given
/// as a fraction of the window's width and a range of logical pixels UP FROM ITS
/// BOTTOM EDGE — the same units the pill itself is laid out in, so the two
/// cannot drift apart on a screen of a different shape.
///
/// It used to be "everything below 0.62 of the height", which was fine while the
/// right thumb's home was a fire button off to one side of it. It stopped being
/// fine when that thumb got a floating stick instead: a stick is anchored
/// wherever it lands, its natural home is the bottom centre-right, and the host
/// walking around during warmup would have started the match by planting it.
/// (Taps that land on a bevy_ui button are excluded separately in
/// [`read_start_input`], which is what keeps the stance column below here inert.)
const START_BAND_X: (f32, f32) = (0.2, 0.8);
const START_BAND_Y: (f32, f32) = (196.0, 306.0);

/// Where the round line sits: above the health bar, which is why the bar itself
/// starts lower than it used to.
const ROUND_LINE_TOP: f32 = 8.0;
const ROUND_LINE_H: f32 = 18.0;

pub fn setup_hud(mut commands: Commands, assets: Res<AssetServer>) {
    let skull_icon: Handle<Image> = assets.load("skull.png");
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
                    BannerPill,
                    Node {
                        padding: UiRect::axes(Val::Px(26.0), Val::Px(14.0)),
                        border_radius: BorderRadius::all(Val::Px(12.0)),
                        flex_direction: FlexDirection::Column,
                        align_items: AlignItems::Center,
                        row_gap: Val::Px(9.0),
                        ..default()
                    },
                    BackgroundColor(Color::srgba(0.05, 0.07, 0.04, 0.82)),
                ))
                .with_children(|pill| {
                    pill.spawn((
                        BannerLine::Headline,
                        Text::new(""),
                        TextFont { font_size: 26.0, ..default() },
                        TextColor(Color::srgb(0.95, 1.0, 0.90)),
                        TextLayout::new_with_justify(Justify::Center),
                    ));
                    pill.spawn((
                        BannerLine::Countdown,
                        Text::new(""),
                        TextFont { font_size: 15.0, ..default() },
                        TextColor(Color::srgba(0.85, 0.92, 0.75, 0.88)),
                        TextLayout::new_with_justify(Justify::Center),
                    ));
                    pill.spawn(Node {
                        flex_direction: FlexDirection::Column,
                        ..default()
                    })
                    .with_children(|board| {
                        for index in 0..BOARD_ROWS {
                            let mut skull = Entity::PLACEHOLDER;
                            let mut label = Entity::PLACEHOLDER;
                            board
                                .spawn(Node {
                                    align_items: AlignItems::Center,
                                    display: Display::None,
                                    ..default()
                                })
                                .with_children(|row| {
                                    // `Visibility::Hidden` rather than
                                    // `Display::None`, and that is the whole
                                    // trick: a hidden node KEEPS its box, so the
                                    // gutter is there for the living too and
                                    // every name starts on the same column.
                                    skull = row
                                        .spawn((
                                            BoardSkull,
                                            ImageNode {
                                                image: skull_icon.clone(),
                                                color: SKULL_RED,
                                                ..default()
                                            },
                                            Node {
                                                width: Val::Px(SKULL_PX),
                                                height: Val::Px(SKULL_PX),
                                                margin: UiRect::right(Val::Px(
                                                    SKULL_COL - SKULL_PX,
                                                )),
                                                ..default()
                                            },
                                            Visibility::Hidden,
                                        ))
                                        .id();
                                    label = row
                                        .spawn((
                                            Text::new(""),
                                            TextFont { font_size: 13.0, ..default() },
                                            TextColor(Color::srgba(0.92, 0.96, 0.85, 0.92)),
                                        ))
                                        .id();
                                })
                                .insert(BoardRow { index, skull, label });
                        }
                    });
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

    // Bottom-center status line, stacked above the stance column (which is
    // taller than the sights button that used to be under here — see
    // `stance.rs`, and move these two if that column ever changes height).
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            left: Val::Px(0.0),
            right: Val::Px(0.0),
            bottom: Val::Px(186.0),
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
                bottom: Val::Px(224.0),
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

/// `ALPHA 2 - 1 BRAVO    1:47`, or just the series score while a round is being
/// counted in.
///
/// One line for the score and the clock together because they answer the same
/// question — how much of this is left and does it matter — and because the top
/// of a phone screen has room for one line, not two.
///
/// The count-in used to hang off the end of this line and now lives on the
/// banner instead, beside the board it belongs with. Two countdowns on one
/// screen is one too many, and of the two places this is the wrong one: the top
/// line is about the SERIES, and between rounds the series has not changed.
pub fn update_round_text(
    state: Res<State<AppState>>,
    round: Option<Res<Round>>,
    mut texts: Query<&mut Text, With<RoundText>>,
) {
    let Ok(mut text) = texts.single_mut() else { return };
    let line = match (state.get(), round.as_deref()) {
        (AppState::InGame, Some(round)) => {
            let score = format!(
                "{} {} - {} {}",
                TEAM_NAMES[0], round.wins[0], round.wins[1], TEAM_NAMES[1]
            );
            match round.phase {
                Phase::Live => format!("{score}    {}", clock(round.remaining())),
                Phase::Over(_) => score,
            }
        }
        _ => String::new(),
    };
    if text.0 != line {
        text.0 = line;
    }
}

/// Watch the roster, and arm the board when it changes.
///
/// Owns [`BoardFlash`] entirely, so [`update_round_banner`] is a pure reader of
/// it. Two things about the arming rule are deliberate:
/// * **A change to or from an EMPTY roster does not count.** Bringing a match up
///   and tearing one down are not somebody joining, and the warmup-to-p2p swap
///   (`net::run_lobby` despawns every rollback entity and rebuilds a frame
///   later) walks through empty on its way. Announcing "everyone left" in the
///   middle of that would be reporting the machinery.
/// * **Names are refreshed even when nothing changed**, because the seat count a
///   bot is numbered from does move under it (again, at that swap) without the
///   set of handles moving at all.
///
/// A rollback can in principle add and remove a bot inside its own window; this
/// runs in `Update` and only ever sees where the sim came to rest, so the worst
/// case is a board that flashes for a change that was un-made. Render-only
/// state, so that is cosmetic rather than a desync.
pub fn watch_roster(
    state: Res<State<AppState>>,
    time: Res<Time>,
    session: Option<Res<Session<SessionConfig>>>,
    pawns: Query<(&Player, Option<&Bot>)>,
    mut flash: ResMut<BoardFlash>,
) {
    flash.left = (flash.left - time.delta_secs()).max(0.0);
    if !matches!(state.get(), AppState::InGame) {
        flash.seen.clear();
        return;
    }
    let seats = seat_count(session.as_deref());
    let mut now: Vec<(usize, String)> = pawns
        .iter()
        .map(|(player, bot)| {
            (player.handle, pawn_name(player.handle, bot.is_some(), seats))
        })
        .collect();
    now.sort_unstable();

    let same = now.len() == flash.seen.len()
        && now.iter().zip(&flash.seen).all(|((a, _), (b, _))| a == b);
    if !same && !now.is_empty() && !flash.seen.is_empty() {
        flash.headline = roster_headline(&flash.seen, &now);
        flash.left = ROSTER_FLASH;
    }
    flash.seen = now;
}

/// `BOT 3 JOINED` / `BOT 3 LEFT`, or a plain statement that something moved when
/// more than one pawn did at once.
///
/// Naming the pawn is most of the value: bots arrive and leave on a dial, so
/// "the roster changed" would leave you reading the board to find out what — and
/// the point of the headline is to save you that.
fn roster_headline(before: &[(usize, String)], after: &[(usize, String)]) -> String {
    let missing = |from: &[(usize, String)], other: &[(usize, String)]| -> Vec<String> {
        from.iter()
            .filter(|(handle, _)| !other.iter().any(|(o, _)| o == handle))
            .map(|(_, name)| name.to_uppercase())
            .collect()
    };
    let joined = missing(after, before);
    let gone = missing(before, after);
    match (joined.as_slice(), gone.as_slice()) {
        ([one], []) => format!("{one} JOINED"),
        ([], [one]) => format!("{one} LEFT"),
        _ => "ROSTER CHANGED".to_string(),
    }
}

/// The banner: the result and a count-in between rounds, the board underneath,
/// and the board on its own for a few seconds whenever the roster changes.
#[allow(clippy::too_many_arguments)]
pub fn update_round_banner(
    state: Res<State<AppState>>,
    round: Option<Res<Round>>,
    flash: Res<BoardFlash>,
    session: Option<Res<Session<SessionConfig>>>,
    local_players: Option<Res<LocalPlayers>>,
    scores: Query<ScoreRow>,
    mut banners: Query<&mut Visibility, (With<RoundBanner>, Without<BoardSkull>)>,
    mut lines: Query<(&BannerLine, &mut Text, &mut Node)>,
    mut rows: Query<(&BoardRow, &mut Node), Without<BannerLine>>,
    mut skulls: Query<&mut Visibility, (With<BoardSkull>, Without<RoundBanner>)>,
    mut labels: Query<&mut Text, Without<BannerLine>>,
) {
    let in_game = matches!(state.get(), AppState::InGame);
    let counting_in = round
        .as_deref()
        .filter(|_| in_game)
        .and_then(|round| {
            round.winner().map(|w| (round.number, w, round.intermission_left()))
        });
    let show = counting_in.is_some() || (in_game && flash.left > 0.0);
    for mut visibility in &mut banners {
        let wanted = if show { Visibility::Inherited } else { Visibility::Hidden };
        if *visibility != wanted {
            *visibility = wanted;
        }
    }
    if !show {
        return;
    }

    // An intermission outranks a roster flash: it has more to say (the result,
    // the count-in) and it already carries the same board.
    let (headline, countdown) = match counting_in {
        Some((number, winner, left)) => {
            let result = match winner {
                Winner::Team(side) => format!(
                    "{} WINS ROUND {number}",
                    TEAM_NAMES[(side as usize).min(TEAM_COUNT - 1)]
                ),
                Winner::Draw => format!("ROUND {number} DRAWN"),
            };
            (result, format!("NEXT ROUND IN {}", clock(left)))
        }
        None => (flash.headline.clone(), String::new()),
    };
    for (line, mut text, mut node) in &mut lines {
        let wanted = match line {
            BannerLine::Headline => &headline,
            BannerLine::Countdown => &countdown,
        };
        if text.0 != *wanted {
            text.0 = wanted.clone();
        }
        // An empty line has to leave NO BOX, not a zero-height one: `row_gap`
        // spaces children whatever their size, so an empty `Text` still costs
        // the gap either side of it and the pill grows a hole.
        let display = if wanted.is_empty() { Display::None } else { Display::Flex };
        if node.display != display {
            node.display = display;
        }
    }

    let local = local_players.map(|l| l.0.clone()).unwrap_or_default();
    let board = board_rows(session.as_deref(), &local, &scores);
    // Each row asks for ITS OWN line by index. Enumerating the query instead
    // would hand line 0 to whichever row the archetype happens to hold first —
    // see `BoardRow::index`.
    for (row, mut node) in &mut rows {
        let entry = board.get(row.index);
        // Rows past the end of the board leave no box at all, so the pill shrinks
        // to the roster rather than reserving room for eight pawns that aren't
        // there.
        let display = if entry.is_some() { Display::Flex } else { Display::None };
        if node.display != display {
            node.display = display;
        }
        let Some(entry) = entry else { continue };
        if let Ok(mut text) = labels.get_mut(row.label) {
            if text.0 != entry.text {
                text.0 = entry.text.clone();
            }
        }
        if let Ok(mut visibility) = skulls.get_mut(row.skull) {
            let wanted = if entry.dead { Visibility::Inherited } else { Visibility::Hidden };
            if *visibility != wanted {
                *visibility = wanted;
            }
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

/// Start-trigger input: Enter anywhere, or a tap/click in the band around the
/// button ([`START_BAND_X`]). `run_lobby` only honors it on the host with ≥2
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
        let up = h - p.y;
        p.x > w * START_BAND_X.0
            && p.x < w * START_BAND_X.1
            && up > START_BAND_Y.0
            && up < START_BAND_Y.1
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

/// What a pawn is called, everywhere a pawn is called anything: the roster here,
/// the spectate button (`spectate.rs`) and the nameplate over its head
/// (`nameplate.rs`).
///
/// ONE function on purpose. Three spellings of the same pawn is worse than any
/// one of them, and it had already happened — the spectate button used to print a
/// bot's raw handle, so the button said "BOT 5" about the pawn the roster called
/// "Bot 4". That was survivable while the two were on opposite corners of the
/// screen and stops being so now that the name is also floating over the soldier.
///
/// `num_players` is how many SEATS the session has. Bots take the handles above
/// the humans, so numbering them from there makes "Bot 1" the first bot rather
/// than the first pawn that happens to be one.
pub fn pawn_name(handle: usize, is_bot: bool, num_players: usize) -> String {
    if is_bot {
        format!("Bot {}", handle.saturating_sub(num_players) + 1)
    } else {
        format!("Player {}", handle + 1)
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

/// Every field the board reads off a pawn, as one type because it is passed to
/// [`board_lines`] rather than only used inline.
type ScoreRow = (
    &'static Player,
    &'static Team,
    &'static Health,
    &'static Kills,
    &'static Deaths,
    Option<&'static Bot>,
);

/// How many SEATS the session has, which is what bot numbering counts from. Zero
/// before there is a session at all (the lobby, the frame between warmup and
/// p2p) — [`pawn_name`] copes.
fn seat_count(session: Option<&Session<SessionConfig>>) -> usize {
    match session {
        Some(Session::P2P(s)) => s.num_players(),
        Some(Session::SyncTest(s)) => s.num_players(),
        _ => 0,
    }
}

/// One line of the board: what it says, and whether the pawn it names is out of
/// the round. `dead` is what lights the skull; headings and blanks are never it.
struct BoardEntry {
    text: String,
    dead: bool,
}

impl BoardEntry {
    fn heading(text: String) -> Self {
        Self { text, dead: false }
    }
}

/// The full scoreboard, one line per pawn under a heading per side.
///
/// Walks the PAWNS, not the session's handles: bots are pawns without a seat, so
/// anything counting seats would leave them off the board while they were busy
/// killing people. Sorted BY SIDE first — the question a board answers in a team
/// game is "how did MY side do", which is unreadable if the two are interleaved.
fn board_rows(
    session: Option<&Session<SessionConfig>>,
    local: &[usize],
    scores: &Query<ScoreRow>,
) -> Vec<BoardEntry> {
    let seats = seat_count(session);
    let mut rows: Vec<(u8, usize, bool, u32, u32, bool)> = scores
        .iter()
        .map(|(player, team, health, kills, deaths, bot)| {
            (team.0, player.handle, health.alive(), kills.0, deaths.0, bot.is_some())
        })
        .collect();
    rows.sort_unstable();

    let mut lines: Vec<BoardEntry> = Vec::new();
    let mut shown_side: Option<u8> = None;
    for (side, handle, alive, kills, deaths, is_bot) in rows {
        if shown_side != Some(side) {
            if shown_side.is_some() {
                lines.push(BoardEntry::heading(String::new()));
            }
            let name = TEAM_NAMES[(side as usize).min(TEAM_COUNT - 1)];
            lines.push(BoardEntry::heading(format!("- {name} -")));
            shown_side = Some(side);
        }
        let mut who = pawn_name(handle, is_bot, seats);
        if !is_bot && local.contains(&handle) {
            who.push_str(" (you)");
        }
        // GGRS-measured roundtrip time to each remote peer. Errors (local
        // handles, or a peer still synchronizing) → no ping. Bots are local by
        // construction and never have one.
        let ping = match session {
            Some(Session::P2P(s)) if !is_bot => s
                .network_stats(handle)
                .map(|stats| format!("  {}ms", stats.ping))
                .unwrap_or_default(),
            _ => String::new(),
        };
        // Both counts come out of the sim, so every peer's board agrees without
        // anyone sending a score message. Who is OUT rides in the skull rather
        // than in this string — it used to be a " +" suffix on the name, which
        // read as a plus sign and got asked about.
        lines.push(BoardEntry {
            text: format!("{who:<NAME_COL$}{}{ping}", score_label(kills, deaths)),
            dead: !alive,
        });
    }
    lines
}

/// `> ALPHA 3` over `  BRAVO 2` — the upper-right troop count, and the whole
/// state of a no-respawn round.
///
/// Two things about the shape. The marker is a PREFIX of fixed width on both
/// lines rather than a suffix on one, because the block is right-justified
/// against the screen edge and a marker only one line carries would step the
/// other one sideways. And it exists at all because **the sides no longer differ
/// in colour** (`render::ARMY_GREEN`) — with a tan army opposite you, which
/// count was yours went without saying; now nothing else on the screen says it.
///
/// [`TEAM_NAMES`] being two five-letter words is doing quiet work here: the two
/// lines come out the same width with no padding at all.
fn troop_lines(alive: [usize; TEAM_COUNT], mine: Option<usize>) -> Vec<String> {
    (0..TEAM_COUNT)
        .map(|side| {
            let you = if mine == Some(side) { "> " } else { "  " };
            format!("{you}{} {}", TEAM_NAMES[side], alive[side])
        })
        .collect()
}

/// The upper right: who is in the room while one is open, and how many are left
/// standing on each side once a match is running.
pub fn update_player_list(
    state: Res<State<AppState>>,
    lobby: Res<Lobby>,
    local_players: Option<Res<LocalPlayers>>,
    pawns: Query<(&Player, &Team, &Health)>,
    mut texts: Query<&mut Text, With<PlayerListText>>,
) {
    let Ok(mut text) = texts.single_mut() else { return };

    let lines: Vec<String> = match state.get() {
        // Lobby: the sorted room roster IS the future handle order, so the
        // numbering here matches the in-game one. Status lives in the
        // bottom-center [`StatusText`], not here. This one stays a list —
        // nobody is alive or dead yet and there are no sides to count.
        AppState::Connecting => lobby
            .ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let you = if Some(*id) == lobby.my_id { " (you)" } else { "" };
                format!("Player {}{}", i + 1, you)
            })
            .collect(),
        AppState::InGame => {
            let local = local_players.map(|l| l.0.clone()).unwrap_or_default();
            let mut alive = [0usize; TEAM_COUNT];
            let mut mine = None;
            for (player, team, health) in &pawns {
                let side = (team.0 as usize).min(TEAM_COUNT - 1);
                if health.alive() {
                    alive[side] += 1;
                }
                // Which side is yours is read off your PAWN, alive or dead, so
                // the marker stays put while you spectate.
                if local.contains(&player.handle) {
                    mine = Some(side);
                }
            }
            troop_lines(alive, mine)
        }
    };
    let joined = lines.join("\n");
    if text.0 != joined {
        text.0 = joined;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The numbering three places now share. Bots start at 1 from the first BOT,
    /// not from the first pawn — a one-seat game with seven bots has a "Bot 1",
    /// and the pawn on handle 7 is "Bot 7" rather than "Bot 8".
    #[test]
    fn bots_are_numbered_from_the_first_bot_not_the_first_pawn() {
        assert_eq!(pawn_name(0, false, 1), "Player 1");
        assert_eq!(pawn_name(1, true, 1), "Bot 1");
        assert_eq!(pawn_name(7, true, 1), "Bot 7");
        // Two humans in a room: the bots start again from one above them.
        assert_eq!(pawn_name(2, true, 2), "Bot 1");
    }

    /// No session yet (the lobby, the warmup) means no seat count, and a bot's
    /// name still has to come out as a name rather than as a panic.
    #[test]
    fn a_bot_without_a_session_to_count_seats_is_still_named() {
        assert_eq!(pawn_name(0, true, 0), "Bot 1");
    }

    fn roster(handles: &[usize]) -> Vec<(usize, String)> {
        handles.iter().map(|h| (*h, format!("Bot {h}"))).collect()
    }

    /// The headline names the pawn, which is the point of having one — bots
    /// arrive and leave on a dial and "something changed" would send you to the
    /// board to find out what.
    #[test]
    fn the_headline_names_the_pawn_that_joined_or_left() {
        assert_eq!(roster_headline(&roster(&[0, 1]), &roster(&[0, 1, 2])), "BOT 2 JOINED");
        assert_eq!(roster_headline(&roster(&[0, 1, 2]), &roster(&[0, 1])), "BOT 2 LEFT");
    }

    /// A pawn that has LEFT can't be asked its name, so the headline has to come
    /// out of what was stored before it went. This is the case that would come
    /// out blank if the names were re-derived from the world.
    #[test]
    fn a_departed_pawn_is_still_named_from_what_was_stored() {
        let before = vec![(0, "Player 1".to_string()), (3, "Bot 3".to_string())];
        assert_eq!(roster_headline(&before, &before[..1]), "BOT 3 LEFT");
    }

    /// More than one pawn moving at once is real (a frame can carry several
    /// ticks) and there is no sentence that names them all in a headline.
    #[test]
    fn several_at_once_are_reported_without_naming_any() {
        assert_eq!(roster_headline(&roster(&[0]), &roster(&[0, 1, 2])), "ROSTER CHANGED");
        // Swapped, not added to: neither "joined" nor "left" alone is true.
        assert_eq!(roster_headline(&roster(&[0, 1]), &roster(&[0, 2])), "ROSTER CHANGED");
    }

    /// The two lines must come out the same width, or the block steps sideways
    /// against the right edge of the screen as the marker moves between them.
    #[test]
    fn the_troop_count_marks_your_side_without_moving_the_other_line() {
        let lines = troop_lines([3, 2], Some(0));
        assert_eq!(lines, vec!["> ALPHA 3", "  BRAVO 2"]);
        let lines = troop_lines([3, 2], Some(1));
        assert_eq!(lines, vec!["  ALPHA 3", "> BRAVO 2"]);
        for count in [[0, 0], [4, 4]] {
            let lines = troop_lines(count, None);
            assert_eq!(lines[0].len(), lines[1].len(), "{lines:?} would step");
        }
    }
}
