//! The upper-left menu: a MENU pill that drops down a panel of actions.
//! NEW ROOM generates a fresh room code and jumps into it (web navigates to
//! `?room=CODE`, which reloads into that room as its first player / host);
//! below it, one `−  LABEL n  +` row per [`Dial`] — how many bots, and how hard
//! they push.
//!
//! Both dials are the same widget with a different [`Dial`] on it, because they
//! do the same thing in every way that matters: each is UI state that reaches
//! the sim ONLY as bits of this player's input, never by writing to the world.
//! That is not a style preference — a resource the menu mutates re-reads
//! differently on a re-simulated tick, which is a desync. See `PlayerInput`.
//!
//! Buttons use bevy_ui's `Interaction` (ui_focus_system handles both mouse
//! and touch out of the box).

use bevy::prelude::*;

use army_ghosts_sim::{BotProfile, AGGRO_LEVELS, FP, MAX_PLAYERS};

#[derive(Component)]
pub struct MenuToggle;

#[derive(Component)]
pub struct MenuPanel;

#[derive(Component)]
pub struct NewRoomButton;

/// A match setting the player can step up and down from the panel.
#[derive(Component, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dial {
    /// How many bots are in the match, `0..=MAX_PLAYERS - players`.
    Bots,
    /// How hard they push, in [`AGGRO_LEVELS`] positions.
    Aggro,
}

impl Dial {
    const ALL: [Dial; 2] = [Dial::Bots, Dial::Aggro];

    fn label(&self, bots: &BotCount, aggro: &Aggression) -> String {
        match self {
            Dial::Bots => format!("BOTS {}", bots.0),
            Dial::Aggro => format!("AGGRO {}%", percent(aggro.0)),
        }
    }
}

/// One of a dial's two steppers: `-1` or `+1`.
#[derive(Component)]
pub struct DialStep {
    pub dial: Dial,
    pub step: i32,
}

/// The readout between a dial's two steppers.
#[derive(Component)]
pub struct DialLabel(pub Dial);

/// How aggressive this client wants the bots, as a level in `1..=AGGRO_LEVELS`.
///
/// UI state, exactly like [`BotCount`] and for exactly the same reason: it
/// reaches the sim only as bits 0-3 of this player's input
/// (`PlayerInput::set_aggression`), never by anything here writing to the world.
/// A resource the menu mutates re-reads differently on a re-simulated tick.
///
/// Aggression rather than any of the other four dials because it is the one the
/// self-play harness says is worth turning: 0.9 loses every decisive pair
/// against the default and 0.1 is about +432 elo, and charging is also what
/// used to pile bots on top of each other. The other dials are reachable from
/// `tools/selfplay.sh` when there is a reason to move them.
#[derive(Resource, Debug, Clone, Copy)]
pub struct Aggression(pub u8);

impl Default for Aggression {
    /// The shipping profile's 0.5, in dial positions.
    fn default() -> Self {
        Self(level_of(BotProfile::default().aggression))
    }
}

/// The dial position that best matches a raw `0..=FP` aggression.
fn level_of(aggression: i32) -> u8 {
    let steps = AGGRO_LEVELS as i32 - 1;
    let nearest = (aggression.clamp(0, FP) * steps + FP / 2) / FP;
    (nearest + 1) as u8
}

/// The dial position for a percentage — how `?aggro=20` seeds the menu.
pub fn level_of_percent(percent: u32) -> u8 {
    let steps = AGGRO_LEVELS as u32 - 1;
    let nearest = (percent.min(100) * steps + 50) / 100;
    (nearest + 1) as u8
}

/// What a dial position means, as a percentage, for the readout.
fn percent(level: u8) -> u32 {
    (level.clamp(1, AGGRO_LEVELS) as u32 - 1) * 100 / (AGGRO_LEVELS as u32 - 1)
}

/// How many bots this client is asking for.
///
/// **UI state, not sim state.** It reaches the sim only as bits 4-7 of this
/// player's input, which is what makes it rollback-safe and what makes every
/// peer agree — see `PlayerInput::set_bots`. Nothing here may write to the
/// world directly; a resource the menu mutates would re-read differently on a
/// re-simulated tick.
///
/// Only the first player's copy is honoured, so in a room this is the host's
/// dial and everyone else's is inert.
#[derive(Resource, Default, Debug, Clone, Copy)]
pub struct BotCount(pub usize);

/// Room codes: 5 chars from an unambiguous alphabet (no 0/O, 1/I/L) so they
/// survive being read aloud across a room.
const CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";

pub fn generate_room_code() -> String {
    let mut seed = entropy_seed() | 1;
    (0..5)
        .map(|_| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            CODE_ALPHABET[((seed >> 33) as usize) % CODE_ALPHABET.len()] as char
        })
        .collect()
}

/// UI-only entropy — nowhere near the deterministic sim.
#[cfg(not(target_arch = "wasm32"))]
fn entropy_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
        .unwrap_or(0)
        ^ (std::process::id() as u64) << 32
}

#[cfg(target_arch = "wasm32")]
fn entropy_seed() -> u64 {
    (js_sys::Math::random() * (1u64 << 53) as f64) as u64
}

/// Jump into a room. Web: rewrite the query string, which reloads the page
/// straight into the lobby. Native has no page to reload — log the code so a
/// dev can relaunch with it.
#[cfg(target_arch = "wasm32")]
fn open_room(code: &str) {
    if let Some(window) = web_sys::window() {
        let _ = window.location().set_search(&format!("room={code}"));
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn open_room(code: &str) {
    info!("new room code {code} — relaunch with AG_ROOM={code} (native can't navigate)");
}

pub fn setup_menu(mut commands: Commands) {
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            top: Val::Px(8.0),
            left: Val::Px(10.0),
            flex_direction: FlexDirection::Column,
            row_gap: Val::Px(6.0),
            align_items: AlignItems::FlexStart,
            ..default()
        })
        .with_children(|col| {
            col.spawn((
                MenuToggle,
                Button,
                Node {
                    padding: UiRect::axes(Val::Px(14.0), Val::Px(7.0)),
                    border_radius: BorderRadius::all(Val::Px(8.0)),
                    ..default()
                },
                BackgroundColor(Color::srgba(0.16, 0.22, 0.10, 0.85)),
            ))
            .with_children(|pill| {
                pill.spawn((
                    Text::new("MENU"),
                    TextFont { font_size: 14.0, ..default() },
                    TextColor(Color::srgb(0.85, 0.92, 0.75)),
                ));
            });

            col.spawn((
                MenuPanel,
                Node {
                    flex_direction: FlexDirection::Column,
                    row_gap: Val::Px(4.0),
                    ..default()
                },
                Visibility::Hidden,
            ))
            .with_children(|panel| {
                panel
                    .spawn((
                        NewRoomButton,
                        Button,
                        Node {
                            padding: UiRect::axes(Val::Px(14.0), Val::Px(7.0)),
                            border_radius: BorderRadius::all(Val::Px(8.0)),
                            ..default()
                        },
                        BackgroundColor(Color::srgba(0.30, 0.50, 0.18, 0.9)),
                    ))
                    .with_children(|pill| {
                        pill.spawn((
                            Text::new("NEW ROOM"),
                            TextFont { font_size: 14.0, ..default() },
                            TextColor(Color::srgb(0.93, 1.0, 0.88)),
                        ));
                    });

                // One `−  LABEL n  +` row per dial. The steppers are the
                // chrome colour rather than the primary green: NEW ROOM leaves
                // the match, these only adjust it.
                for dial in Dial::ALL {
                    dial_row(panel, dial);
                }
            });
        });
}

/// A whole `−  LABEL n  +` row. Built from the [`Dial`] rather than written out
/// twice, so a second setting is one line in [`Dial::ALL`] and cannot drift out
/// of alignment with the first.
fn dial_row(panel: &mut ChildSpawnerCommands, dial: Dial) {
    panel
        .spawn(Node {
            flex_direction: FlexDirection::Row,
            column_gap: Val::Px(4.0),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|row| {
            step_button(row, DialStep { dial, step: -1 }, "-");
            row.spawn((
                DialLabel(dial),
                // Filled in by `update_dial_labels` on the first frame.
                Text::new(""),
                TextFont { font_size: 14.0, ..default() },
                TextColor(Color::srgb(0.85, 0.92, 0.75)),
            ));
            step_button(row, DialStep { dial, step: 1 }, "+");
        });
}

/// One stepper pill. Square-ish padding so `-` and `+` read as a matched pair
/// rather than as two differently-sized words.
fn step_button(parent: &mut ChildSpawnerCommands, marker: DialStep, glyph: &str) {
    parent
        .spawn((
            marker,
            Button,
            Node {
                padding: UiRect::axes(Val::Px(11.0), Val::Px(7.0)),
                border_radius: BorderRadius::all(Val::Px(8.0)),
                ..default()
            },
            BackgroundColor(Color::srgba(0.16, 0.22, 0.10, 0.85)),
        ))
        .with_children(|pill| {
            pill.spawn((
                Text::new(glyph),
                TextFont { font_size: 14.0, ..default() },
                TextColor(Color::srgb(0.85, 0.92, 0.75)),
            ));
        });
}

/// Bevy systems take one parameter per thing they touch; this one legitimately
/// touches a lot, and splitting it would put the "a press that missed every
/// menu button dismisses the panel" rule in a different place from the buttons
/// it is about.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub fn menu_interactions(
    mouse: Res<ButtonInput<MouseButton>>,
    touches: Res<Touches>,
    launch: Res<crate::LaunchConfig>,
    mut bots: ResMut<BotCount>,
    mut aggro: ResMut<Aggression>,
    toggles: Query<&Interaction, (Changed<Interaction>, With<MenuToggle>)>,
    new_rooms: Query<&Interaction, (Changed<Interaction>, With<NewRoomButton>)>,
    steps: Query<(&Interaction, &DialStep), Changed<Interaction>>,
    // Every panel button belongs here, or pressing it dismisses the panel.
    menu_buttons: Query<&Interaction, Or<(With<MenuToggle>, With<NewRoomButton>, With<DialStep>)>>,
    mut panels: Query<&mut Visibility, With<MenuPanel>>,
) {
    for interaction in &toggles {
        if *interaction == Interaction::Pressed {
            for mut visibility in &mut panels {
                *visibility = if *visibility == Visibility::Hidden {
                    Visibility::Inherited
                } else {
                    Visibility::Hidden
                };
            }
        }
    }
    for interaction in &new_rooms {
        if *interaction == Interaction::Pressed {
            let code = generate_room_code();
            info!("menu: new room {code}");
            open_room(&code);
        }
    }
    for (interaction, DialStep { dial, step }) in &steps {
        if *interaction != Interaction::Pressed {
            continue;
        }
        match dial {
            // Bots fill the seats the humans aren't using — the sim clamps this
            // too, but clamping here is what stops the readout showing a number
            // the match is never going to reach.
            Dial::Bots => {
                let room = MAX_PLAYERS.saturating_sub(launch.players) as i32;
                let next = (bots.0 as i32 + step).clamp(0, room) as usize;
                if next != bots.0 {
                    bots.0 = next;
                    info!("menu: {} bots", bots.0);
                }
            }
            // Clamped to 1, not 0: zero is the wire's "not asking" sentinel,
            // and position 1 is already aggression 0.0.
            Dial::Aggro => {
                let next = (aggro.0 as i32 + step).clamp(1, AGGRO_LEVELS as i32) as u8;
                if next != aggro.0 {
                    aggro.0 = next;
                    info!("menu: bot aggression {}%", percent(aggro.0));
                }
            }
        }
    }
    // Any press that didn't land on a menu button dismisses the panel
    // (ui_focus_system has already stamped this frame's Interaction states
    // by the time Update systems run).
    let pressed_somewhere = mouse.just_pressed(MouseButton::Left) || touches.any_just_pressed();
    if pressed_somewhere && menu_buttons.iter().all(|i| *i != Interaction::Pressed) {
        for mut visibility in &mut panels {
            *visibility = Visibility::Hidden;
        }
    }
}

/// Keep every readout in step with its dial.
///
/// The guard is the string comparison, not `Res::is_changed` — a resource guard
/// would be cheaper and would also have to be right about whether the labels
/// exist yet on the frame the resources were last touched, which is how a
/// readout ends up permanently blank. Comparing what is on screen to what
/// should be is the same answer with none of that reasoning.
pub fn update_dial_labels(
    bots: Res<BotCount>,
    aggro: Res<Aggression>,
    mut labels: Query<(&DialLabel, &mut Text)>,
) {
    for (DialLabel(dial), mut text) in &mut labels {
        let wanted = dial.label(&bots, &aggro);
        if text.0 != wanted {
            text.0 = wanted;
        }
    }
}
