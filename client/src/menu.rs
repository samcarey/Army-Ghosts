//! The upper-left menu: a MENU pill that drops down a panel of actions.
//! NEW ROOM generates a fresh room code and jumps into it (web navigates to
//! `?room=CODE`, which reloads into that room as its first player / host), and
//! a `−  BOTS n  +` row adds and removes bots.
//!
//! Buttons use bevy_ui's `Interaction` (ui_focus_system handles both mouse
//! and touch out of the box).

use bevy::prelude::*;

use army_ghosts_sim::MAX_PLAYERS;

#[derive(Component)]
pub struct MenuToggle;

#[derive(Component)]
pub struct MenuPanel;

#[derive(Component)]
pub struct NewRoomButton;

/// `-1` or `+1`, whichever this button is.
#[derive(Component)]
pub struct BotStepButton(pub i32);

/// The `BOTS n` readout between the two step buttons.
#[derive(Component)]
pub struct BotCountLabel;

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

                // `−  BOTS n  +` on one row. The steppers are the chrome
                // colour rather than the primary green: NEW ROOM leaves the
                // match, these only adjust it.
                panel
                    .spawn(Node {
                        flex_direction: FlexDirection::Row,
                        column_gap: Val::Px(4.0),
                        align_items: AlignItems::Center,
                        ..default()
                    })
                    .with_children(|row| {
                        step_button(row, BotStepButton(-1), "-");
                        row.spawn((
                            BotCountLabel,
                            Text::new("BOTS 0"),
                            TextFont { font_size: 14.0, ..default() },
                            TextColor(Color::srgb(0.85, 0.92, 0.75)),
                        ));
                        step_button(row, BotStepButton(1), "+");
                    });
            });
        });
}

/// One stepper pill. Square-ish padding so `-` and `+` read as a matched pair
/// rather than as two differently-sized words.
fn step_button(parent: &mut ChildSpawnerCommands, marker: BotStepButton, glyph: &str) {
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

pub fn menu_interactions(
    mouse: Res<ButtonInput<MouseButton>>,
    touches: Res<Touches>,
    launch: Res<crate::LaunchConfig>,
    mut bots: ResMut<BotCount>,
    toggles: Query<&Interaction, (Changed<Interaction>, With<MenuToggle>)>,
    new_rooms: Query<&Interaction, (Changed<Interaction>, With<NewRoomButton>)>,
    steps: Query<(&Interaction, &BotStepButton), Changed<Interaction>>,
    // Every panel button belongs here, or pressing it dismisses the panel.
    menu_buttons: Query<
        &Interaction,
        Or<(With<MenuToggle>, With<NewRoomButton>, With<BotStepButton>)>,
    >,
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
    // Bots fill the seats the humans aren't using — the sim clamps this too,
    // but clamping here is what stops the readout showing a number the match
    // is never going to reach.
    let room = MAX_PLAYERS.saturating_sub(launch.players);
    for (interaction, step) in &steps {
        if *interaction == Interaction::Pressed {
            let next = (bots.0 as i32 + step.0).clamp(0, room as i32) as usize;
            if next != bots.0 {
                bots.0 = next;
                info!("menu: {} bots", bots.0);
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

/// Keep the readout in step with the count. Change-detected, so it writes the
/// text only when the number actually moves.
pub fn update_bot_label(
    bots: Res<BotCount>,
    mut labels: Query<&mut Text, With<BotCountLabel>>,
) {
    if !bots.is_changed() {
        return;
    }
    for mut text in &mut labels {
        **text = format!("BOTS {}", bots.0);
    }
}
