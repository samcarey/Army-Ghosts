//! Aim down sights: a right-edge toggle button (crosshair ring) that steadies
//! the weapon and slows the player to `ADS_SPEED` of their pace, pushes the
//! camera forward along their facing so they can see what they're shooting at,
//! and draws the line the shot would take.
//!
//! It used to stop the player dead. It doesn't any more — the sway model
//! already prices moving while aiming, and `Aim::stir` already gives a mover
//! away in the grass, so rooting the pawn on top of those was charging for one
//! decision three times and removing the choice rather than pricing it. You can
//! now walk your weapon onto a target; you just do it at a pace anyone watching
//! has time to react to.
//!
//! The state is purely local UI state ([`Ads`]); it reaches the deterministic
//! sim only as the `BTN_ADS` bit that `input.rs` puts in the outgoing
//! `PlayerInput`, so every peer applies the slow from the same input
//! stream. The camera shift and the aim line are render-only.

use bevy::prelude::*;
use bevy_ggrs::LocalPlayers;

use army_ghosts_sim::{
    Aim, Facing, Player, Pos, Rock, Stance, ARENA_HALF_H, ARENA_HALF_W, BULLET_R, FP, PLAYER_R,
    SPREAD_MAX,
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

/// Button diameter / icon size, logical px, and where it sits: the right edge,
/// clear above the aim stick's usual thumb arc. It is on the side the trigger is
/// on because raising the sights is a shooting decision — the left thumb is
/// walking and should never have to leave the stick to take one.
const BUTTON_SIZE: f32 = 76.0;
const ICON_SIZE: f32 = 54.0;
/// High enough to clear the arc a planted aim thumb sweeps: a touch that lands
/// on a button is skipped outright (`touch::on_ui_button`), so a crosshair
/// sitting where thumbs come down would cost the player their aim stick for the
/// length of a drag as well as flipping their sights.
const RIGHT_OFFSET: f32 = 20.0;
const BOTTOM_OFFSET: f32 = 190.0;

/// The aim line: thin, white, and drawn above everything standing in the field
/// but below bullets and their trails (see the `Z_*` ladder in `render.rs`).
/// It has to clear the top of the y-sorted band (`grass::Z_SORT_HI`) — where it
/// used to sit, grass south of the shooter drew over their own sight line.
const AIM_LINE_WIDTH: f32 = 0.6;
const AIM_LINE_ALPHA: f32 = 0.22;
/// The cone edges, relative to the centre line's. Fainter, because there are two
/// of them and they are further from where the player is looking — at equal
/// alpha the pair reads as the aim and the real one as a decoration between them.
const AIM_EDGE_ALPHA: f32 = 0.6;
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

/// The shot-path lines: the centre of the aim, and the two edges of the cone a
/// round is actually drawn from.
///
/// **The cone is the only thing that tells a player what the accuracy model is
/// doing to them.** Everything the sim charges for — running, standing up,
/// holding the trigger down — happens to a number they cannot see, and a
/// mechanic that silently decides whether your rounds land is a mechanic that
/// reads as the game cheating. Two lines opening and closing around the aim say
/// it without a word of UI: come to a stop and they draw together, break into a
/// run and they fly apart.
///
/// It shows only with the sights up, which is a deliberate limit rather than an
/// omission. Hip fire is the state the cone is widest in and the state the
/// player is least able to act on it, and three lines swinging around the pawn
/// at all times would be permanent clutter on a phone screen. Wanting to know
/// what your spread is is exactly the moment you should be aiming.
///
/// `0` is the centre; `-1` and `1` are the edges.
#[derive(Component)]
pub struct AimLine(pub i8);

/// Bottom-center round button with the crosshair icon, plus the (initially
/// hidden) aim line sprite.
pub fn setup_ads(mut commands: Commands, assets: Res<AssetServer>) {
    for edge in [0i8, -1, 1] {
        commands.spawn((
            AimLine(edge),
            Sprite::from_color(Color::srgba(1.0, 1.0, 1.0, 0.0), Vec2::new(1.0, AIM_LINE_WIDTH)),
            Transform::from_xyz(0.0, 0.0, Z_AIM_LINE),
            Visibility::Hidden,
        ));
    }

    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            right: Val::Px(RIGHT_OFFSET),
            bottom: Val::Px(BOTTOM_OFFSET),
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
/// bullet would: at the first boulder it would hit, else at the arena
/// edge (bullets outrange the arena, so their tick TTL never decides this).
pub fn update_aim_line(
    ads: Res<Ads>,
    local_players: Option<Res<LocalPlayers>>,
    players: Query<(&Player, &Pos, &Facing, &Stance, &Aim)>,
    rocks: Query<(&Rock, &Pos)>,
    mut lines: Query<(&AimLine, &mut Transform, &mut Sprite, &mut Visibility)>,
) {
    let amount = ads.amount();
    let aim = local_players.as_deref().and_then(|local| {
        let handle = *local.0.first()?;
        let (_, pos, facing, stance, aim) = players.iter().find(|(p, ..)| p.handle == handle)?;
        let dir = Vec2::new(facing.x as f32, facing.y as f32).try_normalize()?;
        let (x, y) = pos.to_f32();
        // Same muzzle offset `fire_bullets` spawns the bullet at, so the line
        // starts exactly where the tracer will.
        Some((
            Vec2::new(x, y) + dir * (PLAYER_R + BULLET_R + 2) as f32,
            dir,
            crate::render::muzzle_lift(stance.level),
            // The half-angle of the cone, straight off the sim's own number so
            // the picture cannot drift from the arithmetic that fires the round.
            (SPREAD_MAX as f32 * aim.spread() as f32 / (FP * FP) as f32).atan(),
        ))
    });
    let Some((start, dir, lift, half_angle)) = aim.filter(|_| amount > 0.001) else {
        for (_, _, mut sprite, mut visibility) in &mut lines {
            sprite.color = Color::srgba(1.0, 1.0, 1.0, 0.0);
            *visibility = Visibility::Hidden;
        }
        return;
    };

    for (line, mut transform, mut sprite, mut visibility) in &mut lines {
        let angle = dir.y.atan2(dir.x) + line.0 as f32 * half_angle;
        let along = Vec2::from_angle(angle);
        // Each line is stopped by whatever IS in its own way — an edge that
        // clears a boulder the centre runs into is the useful half of the
        // picture, since that is where the round can still get through.
        let range = rocks
            .iter()
            .map(|(rock, pos)| (pos, rock.r))
            .filter_map(|(pos, radius)| {
                let (x, y) = pos.to_f32();
                ray_circle_range(start, along, Vec2::new(x, y), (radius + BULLET_R) as f32)
            })
            .fold(arena_range(start, along), f32::min);

        let alpha = AIM_LINE_ALPHA * amount * if line.0 == 0 { 1.0 } else { AIM_EDGE_ALPHA };
        *visibility = Visibility::Visible;
        sprite.custom_size = Some(Vec2::new(range.max(0.0), AIM_LINE_WIDTH));
        sprite.color = Color::srgba(1.0, 1.0, 1.0, alpha);
        // Lifted to weapon height like the tracers, so the shot line leaves the
        // rifle rather than the soldier's boots (see `render::muzzle_lift`).
        let mid = start + along * range / 2.0;
        transform.translation = Vec3::new(mid.x, mid.y + lift, Z_AIM_LINE);
        transform.rotation = Quat::from_rotation_z(angle);
    }
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
