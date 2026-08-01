//! Mobile touch controls: two virtual joysticks, each anchored where its thumb
//! lands. The left one walks; the right one turns the barrel and, dragged out to
//! the rim, pulls the trigger. Produces a [`TouchControls`] resource that
//! `input.rs` merges with keyboard input each tick; render-side systems draw the
//! overlay parented to the camera (screen-fixed), except the trigger bar, which
//! is bevy_ui because it carries a word.
//!
//! **Firing is the far end of the aim stick's travel, not a button.** A thumb
//! that has to leave the stick to shoot cannot turn while shooting, which is the
//! one thing the accuracy model most wants a player able to do badly and pay
//! for: `Aim::swing` charges for traverse, and a control scheme where traverse
//! and trigger are the same thumb is what makes that charge something the player
//! feels rather than reads about. It also frees the whole right half of the
//! screen from pixel-precise thumb work in a firefight.

use bevy::input::touch::Touches;
use bevy::prelude::*;
use bevy::window::PrimaryWindow;

/// Move-stick reach in logical pixels: full deflection at this drag distance.
const JOY_RADIUS: f32 = 60.0;
/// Aim-stick reach — and the trigger threshold. Drag this far from where the
/// thumb landed and the rifle fires; anything short of it only turns.
const AIM_RADIUS: f32 = 60.0;
/// Slack around the aim stick's centre. Under this the stick is not steering at
/// all, so `Facing` falls back to the walk direction (see `PlayerInput::aim`) —
/// which is also what stops a resting thumb's tremor from whipping the barbell
/// of a barrel around and running up `Aim::swing` for nothing.
const AIM_DEAD: f32 = 8.0;

/// The trigger bar: as wide as the stick it sits over, a thumb's clearance above
/// it, and thin enough not to hide the field.
const BAR_W: f32 = AIM_RADIUS * 2.0;
const BAR_H: f32 = 16.0;
const BAR_GAP: f32 = 16.0;

const BAR_TRACK: Color = Color::srgba(0.06, 0.08, 0.04, 0.62);
const BAR_FILL: Color = Color::srgba(0.62, 0.55, 0.20, 0.75);
const BAR_FILL_LIT: Color = Color::srgba(0.98, 0.42, 0.24, 0.95);
const BAR_TEXT: Color = Color::srgba(0.85, 0.92, 0.75, 0.55);
const BAR_TEXT_LIT: Color = Color::srgba(1.0, 0.96, 0.88, 0.98);

#[derive(Resource, Default)]
pub struct TouchControls {
    /// Movement vector, each axis in -1.0..=1.0 (0 when no touch).
    pub move_vec: Vec2,
    /// Where the aim stick is pointing, as a unit vector with y up, or ZERO
    /// when it isn't being asked (inside [`AIM_DEAD`], or no thumb on it) —
    /// which is the value the sim reads as "aim where I'm walking".
    pub aim_vec: Vec2,
    /// Trigger pull, 0..1: how far out of the dead zone toward the firing
    /// threshold the aim thumb has dragged. The bar draws this.
    pub trigger: f32,
    /// Trigger held (via touch) — the aim stick is out at or past the rim.
    pub firing: bool,
    /// Each stick's anchor + current thumb position (window coords, y-down) for
    /// drawing the overlay; None when that stick isn't being touched.
    pub joystick: Option<(Vec2, Vec2)>,
    pub aim_stick: Option<(Vec2, Vec2)>,
    /// True once any touch has ever been seen — i.e. **this player has an aim
    /// stick**, and the move stick must therefore never be allowed to steer.
    ///
    /// `input.rs` is the only reader and that is the whole reason this exists:
    /// the sim falls back to steering by walk when no aim is asked for, which is
    /// right for a keyboard and for a bot and wrong for a thumb. Knowing which
    /// kind of player this is has to happen on the client, because it is a fact
    /// about the hardware and not about the world.
    pub touch_seen: bool,
}

/// Read raw touches into [`TouchControls`]. Touch positions are window
/// coordinates with y down, origin top-left.
///
/// Taps that land on a bevy_ui `Button` are skipped outright: bevy_ui already
/// turns those into `Interaction::Pressed`, and the stick zones below are
/// deliberately generous enough to swallow the sights and stance buttons
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

    // What the barrel was last told, kept for the dead zone below.
    let held = controls.aim_vec;
    controls.move_vec = Vec2::ZERO;
    controls.aim_vec = Vec2::ZERO;
    controls.trigger = 0.0;
    controls.firing = false;
    controls.joystick = None;
    controls.aim_stick = None;

    for touch in touches.iter() {
        // Before the button check, deliberately: working the menu is still proof
        // that there is a thumb on the glass, and that is all this records.
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
            // Aim stick: the DIRECTION is all the sim reads, so it is honoured
            // at any deflection past the dead zone; the distance is spent on
            // the trigger instead.
            let drag = pos - start;
            let dist = drag.length();
            controls.aim_stick = Some((start, pos));
            controls.aim_vec = if dist > AIM_DEAD {
                let unit = drag / dist;
                Vec2::new(unit.x, -unit.y)
            } else {
                // Inside the dead zone the barrel HOLDS where it was, and only a
                // lifted thumb hands it back to the walk direction. Falling back
                // the moment the thumb crosses the centre would whip the aim
                // round to the movement heading every time a player let off the
                // trigger by pulling in — and `Aim::swing` would charge them for
                // the whip, which is an accuracy tax for stopping shooting.
                held
            };
            controls.trigger = ((dist - AIM_DEAD) / (AIM_RADIUS - AIM_DEAD)).clamp(0.0, 1.0);
            controls.firing = dist >= AIM_RADIUS;
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

/// Which piece of stick chrome a sprite is. One component rather than four
/// marker types: `update_overlay` writes all of them, and separate markers would
/// need a pile of mutually-exclusive `Without` filters to prove the queries are
/// disjoint (the same reason `hud::BannerLine` is an enum).
#[derive(Component, Clone, Copy, PartialEq, Eq)]
pub enum StickPart {
    MoveBase,
    MoveKnob,
    AimBase,
    AimKnob,
}

/// The trigger bar over the aim stick, its right-to-left fill, and its word.
#[derive(Component)]
pub struct TriggerBar;
#[derive(Component)]
pub struct TriggerFill;
#[derive(Component)]
pub struct TriggerLabel;

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
    let pale = Color::srgba(1.0, 1.0, 1.0, 0.35);
    let knob = Color::srgba(1.0, 1.0, 1.0, 0.45);
    commands.spawn((overlay(ring.clone(), JOY_RADIUS * 2.0, pale), StickPart::MoveBase));
    commands.spawn((overlay(disc.clone(), 56.0, knob), StickPart::MoveKnob));
    commands.spawn((overlay(ring, AIM_RADIUS * 2.0, pale), StickPart::AimBase));
    commands.spawn((overlay(disc, 56.0, knob), StickPart::AimKnob));

    // The trigger bar. bevy_ui rather than a sprite because it is labelled, and
    // a word is the whole reason a player knows what the loading bar is for.
    commands
        .spawn((
            TriggerBar,
            Node {
                position_type: PositionType::Absolute,
                width: Val::Px(BAR_W),
                height: Val::Px(BAR_H),
                border_radius: BorderRadius::all(Val::Px(BAR_H / 2.0)),
                ..default()
            },
            BackgroundColor(BAR_TRACK),
            Visibility::Hidden,
        ))
        .with_children(|bar| {
            // Pinned to the RIGHT edge and grown leftwards, so the fill runs
            // from the thumb's own side of the stick toward the far one.
            bar.spawn((
                TriggerFill,
                Node {
                    position_type: PositionType::Absolute,
                    right: Val::Px(0.0),
                    top: Val::Px(0.0),
                    bottom: Val::Px(0.0),
                    width: Val::Percent(0.0),
                    border_radius: BorderRadius::all(Val::Px(BAR_H / 2.0)),
                    ..default()
                },
                BackgroundColor(BAR_FILL),
            ));
            // Spawned after the fill, so it reads over it however full it is.
            bar.spawn(Node {
                position_type: PositionType::Absolute,
                left: Val::Px(0.0),
                right: Val::Px(0.0),
                top: Val::Px(0.0),
                bottom: Val::Px(0.0),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            })
            .with_children(|centre| {
                centre.spawn((
                    TriggerLabel,
                    Text::new("TRIGGER"),
                    TextFont { font_size: 10.0, ..default() },
                    TextColor(BAR_TEXT),
                ));
            });
        });
}

/// Convert a window position (y-down, origin top-left) to a camera-child
/// translation (screen-centered, y-up).
fn window_to_overlay(pos: Vec2, size: Vec2) -> Vec3 {
    Vec3::new(pos.x - size.x / 2.0, size.y / 2.0 - pos.y, 100.0)
}

/// Position/show/hide the stick chrome each frame.
pub fn update_overlay(
    controls: Res<TouchControls>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut parts: Query<(&StickPart, &mut Transform, &mut Visibility, &mut Sprite)>,
) {
    let Ok(window) = windows.single() else { return };
    let size = Vec2::new(window.width(), window.height());

    for (part, mut transform, mut visibility, mut sprite) in &mut parts {
        let (stick, reach) = match part {
            StickPart::MoveBase | StickPart::MoveKnob => (controls.joystick, JOY_RADIUS),
            StickPart::AimBase | StickPart::AimKnob => (controls.aim_stick, AIM_RADIUS),
        };
        let Some((anchor, pos)) = stick else {
            *visibility = Visibility::Hidden;
            continue;
        };
        *visibility = Visibility::Visible;
        transform.translation = match part {
            StickPart::MoveBase | StickPart::AimBase => window_to_overlay(anchor, size),
            // The knob follows the thumb, held inside the ring, and sits a hair
            // in front of the base it rides in.
            _ => {
                let clamped = anchor + (pos - anchor).clamp_length_max(reach);
                window_to_overlay(clamped, size) + Vec3::Z
            }
        };
        // The aim knob is the trigger, so it is the one that lights up.
        if *part == StickPart::AimKnob {
            sprite.color = if controls.firing {
                Color::srgba(1.0, 0.72, 0.58, 0.85)
            } else {
                Color::srgba(1.0, 1.0, 1.0, 0.45)
            };
        }
    }
}

/// The bar's own box (moved to follow the thumb) and its fill (grown to follow
/// the pull). Two queries over `Node`, so each needs the other's `Without`.
type Track = (&'static mut Node, &'static mut Visibility, &'static TriggerBar);
type Fill = (&'static mut Node, &'static mut BackgroundColor, &'static TriggerFill);

/// Park the trigger bar over the aim stick and load it as the thumb drags out.
///
/// The bar rides the stick rather than sitting in a fixed corner because the
/// stick itself has no fixed home — it is anchored wherever the thumb landed —
/// and a progress bar that is not beside the thing making progress is a readout
/// the player has to go looking for mid-firefight.
pub fn update_trigger_bar(
    controls: Res<TouchControls>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut bars: Query<Track, Without<TriggerFill>>,
    mut fills: Query<Fill, Without<TriggerBar>>,
    mut labels: Query<&mut TextColor, With<TriggerLabel>>,
) {
    let Ok(window) = windows.single() else { return };
    let (w, h) = (window.width(), window.height());

    let Ok((mut node, mut visibility, _)) = bars.single_mut() else { return };
    let Some((anchor, _)) = controls.aim_stick else {
        *visibility = Visibility::Hidden;
        return;
    };
    *visibility = Visibility::Visible;
    // Centred on the stick, clear of its rim, and kept on screen: a thumb that
    // lands near the top or the right edge must not push its own readout off.
    node.left = Val::Px((anchor.x - BAR_W / 2.0).clamp(4.0, (w - BAR_W - 4.0).max(4.0)));
    node.top = Val::Px((anchor.y - AIM_RADIUS - BAR_GAP - BAR_H).clamp(4.0, (h - BAR_H).max(4.0)));

    for (mut fill, mut colour, _) in &mut fills {
        fill.width = Val::Percent(controls.trigger * 100.0);
        colour.0 = if controls.firing { BAR_FILL_LIT } else { BAR_FILL };
    }
    for mut colour in &mut labels {
        colour.0 = if controls.firing { BAR_TEXT_LIT } else { BAR_TEXT };
    }
}
