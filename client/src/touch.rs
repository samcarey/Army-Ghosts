//! Mobile touch controls: left-half virtual joystick (anchored where the
//! thumb lands), right-side fire button. Produces a [`TouchControls`] resource
//! that `input.rs` merges with keyboard input each tick; render-side systems
//! draw the joystick/button overlay parented to the camera (screen-fixed).

use bevy::input::touch::Touches;
use bevy::prelude::*;
use bevy::window::PrimaryWindow;

/// Joystick reach in logical pixels: full deflection at this drag distance.
const JOY_RADIUS: f32 = 60.0;
/// Fire button center offset from the bottom-right corner (logical px).
const FIRE_OFFSET: Vec2 = Vec2::new(90.0, 90.0);
const FIRE_RADIUS: f32 = 48.0;

#[derive(Resource, Default)]
pub struct TouchControls {
    /// Movement vector, each axis in -1.0..=1.0 (0 when no touch).
    pub move_vec: Vec2,
    /// Fire button currently held (via touch).
    pub firing: bool,
    /// Joystick anchor + current thumb position (window coords, y-down) for
    /// drawing the overlay; None when not touching.
    pub joystick: Option<(Vec2, Vec2)>,
    /// True once any touch has ever been seen — switches the UI overlay on
    /// (desktop never shows the touch chrome).
    pub touch_seen: bool,
}

/// Read raw touches into [`TouchControls`]. Touch positions are window
/// coordinates with y down, origin top-left.
///
/// Taps that land on a bevy_ui `Button` are skipped outright: bevy_ui already
/// turns those into `Interaction::Pressed`, and the joystick/fire zones below
/// are deliberately generous enough to swallow the sights and stance buttons
/// otherwise. Asking the UI where its buttons actually are beats keeping a
/// second copy of their geometry here in step with the layout.
pub fn read_touches(
    touches: Res<Touches>,
    windows: Query<&Window, With<PrimaryWindow>>,
    ui_buttons: Query<(&ComputedNode, &UiGlobalTransform, &InheritedVisibility), With<Button>>,
    mut controls: ResMut<TouchControls>,
) {
    let Ok(window) = windows.single() else { return };
    let size = Vec2::new(window.width(), window.height());
    // `ComputedNode` works in physical pixels; touches arrive in logical ones.
    let scale = window.scale_factor();

    controls.move_vec = Vec2::ZERO;
    controls.firing = false;
    controls.joystick = None;

    for touch in touches.iter() {
        controls.touch_seen = true;
        let start = touch.start_position();
        let pos = touch.position();
        if on_ui_button(&ui_buttons, start * scale) {
            continue;
        }
        if start.x < size.x * 0.45 {
            // Movement stick: anchor at the touch-down point, direction =
            // drag vector (y flipped to world-up), clamped to JOY_RADIUS.
            let drag = (pos - start) / JOY_RADIUS;
            let drag = if drag.length() > 1.0 { drag.normalize() } else { drag };
            controls.move_vec = Vec2::new(drag.x, -drag.y);
            controls.joystick = Some((start, pos));
        } else {
            // Right side: fire button. Any right-side touch near the button
            // (generously, anywhere in the right-bottom quadrant) fires — a
            // firefight is no time for pixel-precise thumbs.
            let button_center = Vec2::new(size.x - FIRE_OFFSET.x, size.y - FIRE_OFFSET.y);
            if pos.distance(button_center) < FIRE_RADIUS * 2.5 {
                controls.firing = true;
            }
        }
    }
}

/// Is this physical-pixel point inside a visible bevy_ui button?
fn on_ui_button(
    buttons: &Query<(&ComputedNode, &UiGlobalTransform, &InheritedVisibility), With<Button>>,
    physical: Vec2,
) -> bool {
    buttons
        .iter()
        .any(|(node, transform, visible)| visible.get() && node.contains_point(*transform, physical))
}

// ── Overlay rendering ───────────────────────────────────────────────────────

#[derive(Component)]
pub struct JoystickBase;
#[derive(Component)]
pub struct JoystickKnob;
#[derive(Component)]
pub struct FireButton;

/// Overlay sprites are children of the camera, so translations here are
/// screen-space offsets from the screen center (world y up). Runs after
/// `setup_scene` (needs the camera).
pub fn setup_overlay(
    mut commands: Commands,
    assets: Res<AssetServer>,
    cameras: Query<Entity, With<Camera2d>>,
) {
    let Ok(camera) = cameras.single() else { return };
    let ring: Handle<Image> = assets.load("ring.png");
    let disc: Handle<Image> = assets.load("disc.png");

    let overlay = |image: Handle<Image>, size: f32, color: Color| {
        (
            Sprite { image, color, custom_size: Some(Vec2::splat(size)), ..default() },
            Transform::from_xyz(0.0, 0.0, 100.0),
            Visibility::Hidden,
            ChildOf(camera),
        )
    };
    commands.spawn((overlay(ring.clone(), JOY_RADIUS * 2.0, Color::srgba(1.0, 1.0, 1.0, 0.35)), JoystickBase));
    commands.spawn((overlay(disc.clone(), 56.0, Color::srgba(1.0, 1.0, 1.0, 0.45)), JoystickKnob));
    commands.spawn((overlay(disc, FIRE_RADIUS * 2.0, Color::srgba(0.9, 0.4, 0.3, 0.45)), FireButton));
}

/// Convert a window position (y-down, origin top-left) to a camera-child
/// translation (screen-centered, y-up).
fn window_to_overlay(pos: Vec2, size: Vec2) -> Vec3 {
    Vec3::new(pos.x - size.x / 2.0, size.y / 2.0 - pos.y, 100.0)
}

/// Position/show/hide the overlay each frame.
pub fn update_overlay(
    controls: Res<TouchControls>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut base: Query<(&mut Transform, &mut Visibility), (With<JoystickBase>, Without<JoystickKnob>, Without<FireButton>)>,
    mut knob: Query<(&mut Transform, &mut Visibility), (With<JoystickKnob>, Without<JoystickBase>, Without<FireButton>)>,
    mut fire: Query<(&mut Transform, &mut Visibility, &mut Sprite), (With<FireButton>, Without<JoystickBase>, Without<JoystickKnob>)>,
) {
    let Ok(window) = windows.single() else { return };
    let size = Vec2::new(window.width(), window.height());

    if let Ok((mut transform, mut visibility)) = base.single_mut() {
        if let Some((anchor, _)) = controls.joystick {
            transform.translation = window_to_overlay(anchor, size);
            *visibility = Visibility::Visible;
        } else {
            *visibility = Visibility::Hidden;
        }
    }
    if let Ok((mut transform, mut visibility)) = knob.single_mut() {
        if let Some((anchor, pos)) = controls.joystick {
            let clamped = anchor + (pos - anchor).clamp_length_max(JOY_RADIUS);
            transform.translation = window_to_overlay(clamped, size) + Vec3::Z;
            *visibility = Visibility::Visible;
        } else {
            *visibility = Visibility::Hidden;
        }
    }
    if let Ok((mut transform, mut visibility, mut sprite)) = fire.single_mut() {
        let center = Vec2::new(size.x - FIRE_OFFSET.x, size.y - FIRE_OFFSET.y);
        transform.translation = window_to_overlay(center, size);
        *visibility = if controls.touch_seen { Visibility::Visible } else { Visibility::Hidden };
        sprite.color = if controls.firing {
            Color::srgba(0.95, 0.35, 0.25, 0.85)
        } else {
            Color::srgba(0.9, 0.4, 0.3, 0.45)
        };
    }
}
