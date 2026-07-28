//! Stance control: two round buttons on the right edge (get up / get down)
//! that walk the local player between standing, crouching and prone, plus the
//! label that says which one you are in.
//!
//! Like [`crate::ads`], the buttons are local UI state. What reaches the sim is
//! the *level* you are asking for, packed into the stance bits of
//! `PlayerInput` and re-sent every tick — never a "go down one" edge. Rollback
//! replays a tick any number of times, so an edge would apply as many times as
//! the frame is re-simulated; an absolute level lands on the same stance no
//! matter how often it is re-applied. The sim owns the rest: it steps one level
//! at a time and roots the pawn for the transition (`Stance::advance`).

use bevy::prelude::*;
use bevy_ggrs::LocalPlayers;

use army_ghosts_sim::{Player, Stance, STANCE_CROUCH, STANCE_PRONE, STANCE_STAND};

/// Button diameter / icon size, logical px, and where the column sits: right
/// edge, clear above the fire button's reach.
const BUTTON_SIZE: f32 = 62.0;
const ICON_SIZE: f32 = 34.0;
const RIGHT_OFFSET: f32 = 22.0;
const BOTTOM_OFFSET: f32 = 172.0;

const IDLE_BG: Color = Color::srgba(0.10, 0.13, 0.07, 0.55);
const IDLE_ICON: Color = Color::srgba(0.85, 0.92, 0.75, 0.65);
/// The end of the road (already standing / already prone): still there, plainly
/// not doing anything.
const SPENT_BG: Color = Color::srgba(0.10, 0.13, 0.07, 0.28);
const SPENT_ICON: Color = Color::srgba(0.85, 0.92, 0.75, 0.20);

/// The stance the local player is asking for. Absolute, not a delta — see the
/// module note.
#[derive(Resource, Default)]
pub struct StanceControl {
    pub wanted: u8,
}

/// One of the two stance buttons. `down` is the get-lower one.
#[derive(Component)]
pub struct StanceButton {
    down: bool,
}

/// The chevron inside a button (tinted with whether it can still be used).
#[derive(Component)]
pub struct StanceIcon {
    down: bool,
}

/// The "STANDING" / "CROUCHED" / "PRONE" readout under the buttons.
#[derive(Component)]
pub struct StanceLabel;

pub fn setup_stance(mut commands: Commands, assets: Res<AssetServer>) {
    let chevron: Handle<Image> = assets.load("chevron.png");
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            right: Val::Px(RIGHT_OFFSET),
            bottom: Val::Px(BOTTOM_OFFSET),
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            row_gap: Val::Px(8.0),
            ..default()
        })
        .with_children(|column| {
            // Up on top, down below — the pair reads as the ladder it is.
            for down in [false, true] {
                column
                    .spawn((
                        StanceButton { down },
                        Button,
                        Node {
                            width: Val::Px(BUTTON_SIZE),
                            height: Val::Px(BUTTON_SIZE),
                            border_radius: BorderRadius::all(Val::Percent(50.0)),
                            justify_content: JustifyContent::Center,
                            align_items: AlignItems::Center,
                            ..default()
                        },
                        BackgroundColor(IDLE_BG),
                    ))
                    .with_children(|button| {
                        button.spawn((
                            StanceIcon { down },
                            ImageNode {
                                image: chevron.clone(),
                                color: IDLE_ICON,
                                // One chevron asset, flipped for the way down.
                                flip_y: down,
                                ..default()
                            },
                            Node {
                                width: Val::Px(ICON_SIZE),
                                height: Val::Px(ICON_SIZE),
                                ..default()
                            },
                        ));
                    });
            }
            column.spawn((
                StanceLabel,
                Text::new(""),
                TextFont { font_size: 11.0, ..default() },
                TextColor(Color::srgba(0.85, 0.92, 0.75, 0.75)),
            ));
        });
}

/// Tap a chevron (or C / V on a keyboard) to ask for one level up or down.
/// Taps queue: two on the way down from standing asks for prone, and the sim
/// walks through the crouch on its own.
pub fn read_stance_input(
    keys: Res<ButtonInput<KeyCode>>,
    buttons: Query<(&Interaction, &StanceButton), Changed<Interaction>>,
    mut control: ResMut<StanceControl>,
) {
    let mut step: i32 = 0;
    for (interaction, button) in &buttons {
        if *interaction == Interaction::Pressed {
            step += if button.down { 1 } else { -1 };
        }
    }
    if keys.just_pressed(KeyCode::KeyC) {
        step += 1;
    }
    if keys.just_pressed(KeyCode::KeyV) {
        step -= 1;
    }
    if step != 0 {
        control.wanted =
            (control.wanted as i32 + step).clamp(STANCE_STAND as i32, STANCE_PRONE as i32) as u8;
    }
}

/// Dim whichever button has nowhere left to go, and name the stance underneath.
/// The label follows the pawn's *actual* stance, not the request, so it tells
/// you where you are rather than where you asked to be.
pub fn update_stance_buttons(
    control: Res<StanceControl>,
    local_players: Option<Res<LocalPlayers>>,
    players: Query<(&Player, &Stance)>,
    mut last: Local<Option<(u8, u8)>>,
    mut buttons: Query<(&mut BackgroundColor, &StanceButton)>,
    mut icons: Query<(&mut ImageNode, &StanceIcon)>,
    mut labels: Query<&mut Text, With<StanceLabel>>,
) {
    let actual = local_players
        .as_deref()
        .and_then(|local| local.0.first().copied())
        .and_then(|handle| players.iter().find(|(p, _)| p.handle == handle))
        .map(|(_, stance)| stance.level)
        .unwrap_or(control.wanted);
    if *last == Some((control.wanted, actual)) {
        return;
    }
    *last = Some((control.wanted, actual));

    let spent = |down: bool| {
        if down {
            control.wanted >= STANCE_PRONE
        } else {
            // `==`, not `<=`: standing is level 0, so there is nothing below it
            // and clippy rightly calls the comparison absurd.
            control.wanted == STANCE_STAND
        }
    };
    for (mut background, button) in &mut buttons {
        background.0 = if spent(button.down) { SPENT_BG } else { IDLE_BG };
    }
    for (mut icon, marker) in &mut icons {
        icon.color = if spent(marker.down) { SPENT_ICON } else { IDLE_ICON };
    }
    let name = match actual {
        STANCE_CROUCH => "CROUCHED",
        STANCE_PRONE => "PRONE",
        _ => "STANDING",
    };
    for mut label in &mut labels {
        if label.0 != name {
            label.0 = name.into();
        }
    }
}
