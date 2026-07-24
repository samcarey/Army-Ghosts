//! The upper-left menu: a MENU pill that drops down a panel of actions.
//! Currently one action — NEW ROOM: generate a fresh room code and jump into
//! it (web navigates to `?room=CODE`, which reloads into that room as its
//! first player / host).
//!
//! Buttons use bevy_ui's `Interaction` (ui_focus_system handles both mouse
//! and touch out of the box).

use bevy::prelude::*;

#[derive(Component)]
pub struct MenuToggle;

#[derive(Component)]
pub struct MenuPanel;

#[derive(Component)]
pub struct NewRoomButton;

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
            });
        });
}

pub fn menu_interactions(
    mouse: Res<ButtonInput<MouseButton>>,
    touches: Res<Touches>,
    toggles: Query<&Interaction, (Changed<Interaction>, With<MenuToggle>)>,
    new_rooms: Query<&Interaction, (Changed<Interaction>, With<NewRoomButton>)>,
    menu_buttons: Query<&Interaction, Or<(With<MenuToggle>, With<NewRoomButton>)>>,
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
