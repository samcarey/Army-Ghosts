//! Aim down sights: a bottom-center toggle button (crosshair ring) that roots
//! the player in place, pushes the camera forward along their facing so they
//! can see what they're shooting at, and draws the line the shot would take.
//!
//! The state is purely local UI state ([`Ads`]); it reaches the deterministic
//! sim only as the `BTN_ADS` bit that `input.rs` puts in the outgoing
//! `PlayerInput`, so every peer applies the movement lock from the same input
//! stream. The camera shift and the aim line are render-only.

use bevy::prelude::*;
use bevy_ggrs::LocalPlayers;

use army_ghosts_sim::{
    Facing, Player, Pos, Rock, Stance, Target, ARENA_HALF_H, ARENA_HALF_W, BULLET_R, PLAYER_R,
    TARGET_R,
};

/// How far the camera slides toward what you're aiming at, world units (=
/// pixels at base zoom). Half of a ~400 px mobile screen: deliberately a fixed
/// distance, so the shift is the same on every platform and window size.
const ADS_SHIFT: f32 = 200.0;
/// Transition time in and out, seconds (eased, not linear).
const ADS_EASE_SECS: f32 = 0.5;
/// Time constant for the shift *direction* following a turn, seconds. The
/// facing itself snaps between stick directions; without this the camera would
/// snap with it.
const AIM_TURN_TAU: f32 = 0.12;

/// Button diameter / icon size / distance above the bottom edge, logical px.
const BUTTON_SIZE: f32 = 76.0;
const ICON_SIZE: f32 = 54.0;
const BOTTOM_OFFSET: f32 = 22.0;

/// The aim line: thin, white, and drawn above everything standing in the field
/// but below bullets and their trails (see the `Z_*` ladder in `render.rs`).
/// It has to clear the top of the y-sorted band (`grass::Z_SORT_HI`) — where it
/// used to sit, grass south of the shooter drew over their own sight line.
const AIM_LINE_WIDTH: f32 = 0.6;
const AIM_LINE_ALPHA: f32 = 0.22;
const Z_AIM_LINE: f32 = 1.85;

/// Local aim-down-sights state. Not rollback state — it only ever feeds a
/// button bit into the input stream.
#[derive(Resource, Default)]
pub struct Ads {
    pub active: bool,
    /// Linear 0..1 transition progress; [`Ads::amount`] eases it.
    progress: f32,
    /// Smoothed aim direction (unit vector) the camera shifts along.
    dir: Vec2,
}

impl Ads {
    /// Eased 0..1 transition (smoothstep): drives both the camera shift and
    /// the aim line's opacity.
    pub fn amount(&self) -> f32 {
        let p = self.progress;
        p * p * (3.0 - 2.0 * p)
    }

    /// How far the camera is currently pushed off the player, world units.
    pub fn camera_offset(&self) -> Vec2 {
        self.dir * ADS_SHIFT * self.amount()
    }
}

/// The round ADS button (bevy_ui `Button`, so `Interaction` covers touch too).
#[derive(Component)]
pub struct AdsButton;

/// The crosshair image inside the button (tinted with the ADS state).
#[derive(Component)]
pub struct AdsIcon;

/// The shot-path line (one long-lived sprite, stretched/rotated each frame).
#[derive(Component)]
pub struct AimLine;

/// Bottom-center round button with the crosshair icon, plus the (initially
/// hidden) aim line sprite.
pub fn setup_ads(mut commands: Commands, assets: Res<AssetServer>) {
    commands.spawn((
        AimLine,
        Sprite::from_color(Color::srgba(1.0, 1.0, 1.0, 0.0), Vec2::new(1.0, AIM_LINE_WIDTH)),
        Transform::from_xyz(0.0, 0.0, Z_AIM_LINE),
        Visibility::Hidden,
    ));

    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            left: Val::Px(0.0),
            right: Val::Px(0.0),
            bottom: Val::Px(BOTTOM_OFFSET),
            justify_content: JustifyContent::Center,
            ..default()
        })
        .with_children(|row| {
            row.spawn((
                AdsButton,
                Button,
                Node {
                    width: Val::Px(BUTTON_SIZE),
                    height: Val::Px(BUTTON_SIZE),
                    border_radius: BorderRadius::all(Val::Percent(50.0)),
                    justify_content: JustifyContent::Center,
                    align_items: AlignItems::Center,
                    ..default()
                },
                BackgroundColor(Color::srgba(0.10, 0.13, 0.07, 0.55)),
            ))
            .with_children(|button| {
                button.spawn((
                    AdsIcon,
                    ImageNode {
                        image: assets.load("crosshair.png"),
                        color: Color::srgba(0.85, 0.92, 0.75, 0.65),
                        ..default()
                    },
                    Node {
                        width: Val::Px(ICON_SIZE),
                        height: Val::Px(ICON_SIZE),
                        ..default()
                    },
                ));
            });
        });
}

/// Toggle ADS: tap/click the crosshair button, or Shift on a keyboard (the
/// desktop dev loop and headless tests need a key).
pub fn toggle_ads(
    keys: Res<ButtonInput<KeyCode>>,
    buttons: Query<&Interaction, (Changed<Interaction>, With<AdsButton>)>,
    mut ads: ResMut<Ads>,
) {
    let tapped = buttons.iter().any(|i| *i == Interaction::Pressed);
    if tapped || keys.just_pressed(KeyCode::ShiftLeft) || keys.just_pressed(KeyCode::ShiftRight) {
        ads.active = !ads.active;
    }
}

/// Advance the aim transition: `progress` walks to its target over
/// `ADS_EASE_SECS`, and the shift direction chases the local player's facing.
pub fn advance_ads(
    time: Res<Time>,
    mut ads: ResMut<Ads>,
    local_players: Option<Res<LocalPlayers>>,
    players: Query<(&Player, &Facing)>,
) {
    let dt = time.delta_secs();
    let step = if ADS_EASE_SECS > 0.0 { dt / ADS_EASE_SECS } else { 1.0 };
    ads.progress = if ads.active {
        (ads.progress + step).min(1.0)
    } else {
        (ads.progress - step).max(0.0)
    };

    let Some(facing) = local_facing(local_players.as_deref(), &players) else { return };
    if ads.progress <= 0.0 {
        // Not aiming: keep the direction current so raising sights pushes the
        // camera the right way from the very first frame.
        ads.dir = facing;
        return;
    }
    let k = 1.0 - (-dt / AIM_TURN_TAU).exp();
    ads.dir = ads.dir.lerp(facing, k).normalize_or(facing);
}

/// The local pawn's facing as a unit vector (None if there isn't one yet).
fn local_facing(
    local_players: Option<&LocalPlayers>,
    players: &Query<(&Player, &Facing)>,
) -> Option<Vec2> {
    let handle = *local_players?.0.first()?;
    let (_, facing) = players.iter().find(|(p, _)| p.handle == handle)?;
    Vec2::new(facing.x as f32, facing.y as f32).try_normalize()
}

/// Stretch the white line from the muzzle along the aim, stopping where the
/// bullet would: at the first target or boulder it would hit, else at the arena
/// edge (bullets outrange the arena, so their tick TTL never decides this).
pub fn update_aim_line(
    ads: Res<Ads>,
    local_players: Option<Res<LocalPlayers>>,
    players: Query<(&Player, &Pos, &Facing, &Stance)>,
    targets: Query<&Pos, With<Target>>,
    rocks: Query<(&Rock, &Pos)>,
    mut lines: Query<(&mut Transform, &mut Sprite, &mut Visibility), With<AimLine>>,
) {
    let Ok((mut transform, mut sprite, mut visibility)) = lines.single_mut() else { return };
    let amount = ads.amount();
    let aim = local_players
        .as_deref()
        .and_then(|local| {
            let handle = *local.0.first()?;
            let (_, pos, facing, stance) = players.iter().find(|(p, ..)| p.handle == handle)?;
            let dir = Vec2::new(facing.x as f32, facing.y as f32).try_normalize()?;
            let (x, y) = pos.to_f32();
            // Same muzzle offset `fire_bullets` spawns the bullet at, so the
            // line starts exactly where the tracer will.
            Some((
                Vec2::new(x, y) + dir * (PLAYER_R + BULLET_R + 2) as f32,
                dir,
                crate::render::muzzle_lift(stance.level),
            ))
        });
    let Some((start, dir, lift)) = aim.filter(|_| amount > 0.001) else {
        *visibility = Visibility::Hidden;
        return;
    };

    let blockers = targets
        .iter()
        .map(|pos| (pos, TARGET_R))
        .chain(rocks.iter().map(|(rock, pos)| (pos, rock.r)));
    let range = blockers
        .filter_map(|(pos, radius)| {
            let (x, y) = pos.to_f32();
            ray_circle_range(start, dir, Vec2::new(x, y), (radius + BULLET_R) as f32)
        })
        .fold(arena_range(start, dir), f32::min);

    *visibility = Visibility::Visible;
    sprite.custom_size = Some(Vec2::new(range.max(0.0), AIM_LINE_WIDTH));
    sprite.color = Color::srgba(1.0, 1.0, 1.0, AIM_LINE_ALPHA * amount);
    // Lifted to weapon height like the tracers, so the shot line leaves the
    // rifle rather than the soldier's boots (see `render::muzzle_lift`).
    let mid = start + dir * range / 2.0;
    transform.translation = Vec3::new(mid.x, mid.y + lift, Z_AIM_LINE);
    transform.rotation = Quat::from_rotation_z(dir.y.atan2(dir.x));
}

/// Distance from `start` along `dir` to the arena wall (where bullets despawn).
fn arena_range(start: Vec2, dir: Vec2) -> f32 {
    let axis = |p: f32, d: f32, half: f32| {
        if d > 0.0 {
            (half - p) / d
        } else if d < 0.0 {
            (-half - p) / d
        } else {
            f32::MAX
        }
    };
    axis(start.x, dir.x, ARENA_HALF_W as f32)
        .min(axis(start.y, dir.y, ARENA_HALF_H as f32))
        .max(0.0)
}

/// Distance to the near side of a circle along the ray, if it's ahead of us.
fn ray_circle_range(start: Vec2, dir: Vec2, center: Vec2, radius: f32) -> Option<f32> {
    let to_center = center - start;
    let along = to_center.dot(dir);
    let perp2 = to_center.length_squared() - along * along;
    let half_chord2 = radius * radius - perp2;
    if half_chord2 < 0.0 {
        return None; // ray passes wide
    }
    let hit = along - half_chord2.sqrt();
    (hit >= 0.0).then_some(hit)
}

/// Light the button up while aiming.
pub fn update_ads_button(
    ads: Res<Ads>,
    mut was_active: Local<Option<bool>>,
    mut buttons: Query<&mut BackgroundColor, With<AdsButton>>,
    mut icons: Query<&mut ImageNode, With<AdsIcon>>,
) {
    if *was_active == Some(ads.active) {
        return;
    }
    *was_active = Some(ads.active);
    for mut background in &mut buttons {
        background.0 = if ads.active {
            Color::srgba(0.30, 0.50, 0.18, 0.85)
        } else {
            Color::srgba(0.10, 0.13, 0.07, 0.55)
        };
    }
    for mut icon in &mut icons {
        icon.color = if ads.active {
            Color::srgba(1.0, 0.95, 0.70, 0.95)
        } else {
            Color::srgba(0.85, 0.92, 0.75, 0.65)
        };
    }
}
