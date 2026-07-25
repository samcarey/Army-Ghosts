//! Rendering: the float world. Sim entities carry integer `Pos`; these systems
//! attach sprites to them and mirror `Pos` into `Transform` each frame. This
//! is the only place fixed-point becomes f32 — never feed anything from here
//! back into the sim.

use bevy::prelude::*;
use bevy_ggrs::LocalPlayers;

use army_ghosts_sim::{
    Bullet, Facing, Player, Pos, Target, ARENA_HALF_H, ARENA_HALF_W, TARGET_R,
};

use crate::ads::Ads;

/// Tell the HTML loader the game is actually drawing frames (it holds its
/// "Starting..." screen until `<body data-game-ready="1">` so it never cuts
/// to a blank canvas while pipelines compile / assets decode). A few frames
/// in is a good-enough proxy for "first real frame presented".
#[cfg(target_arch = "wasm32")]
pub fn signal_game_ready(mut frames: Local<u32>) {
    if *frames > 10 {
        return;
    }
    *frames += 1;
    if *frames == 10 {
        if let Some(body) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.body())
        {
            let _ = body.set_attribute("data-game-ready", "1");
        }
    }
}

/// The soldier sprite sheet (tools/gen_assets.py): 64px frames in one row,
/// drawn facing UP so `orient_players`' rotation convention works unchanged.
/// Frame 0 idle, 1-6 walk cycle, 7-12 run cycle.
#[derive(Resource)]
pub struct SoldierSheet {
    image: Handle<Image>,
    layout: Handle<TextureAtlasLayout>,
}

const SOLDIER_FRAME_PX: u32 = 64;
const SOLDIER_FRAMES: u32 = 13;
const IDLE_FRAME: usize = 0;
const WALK_START: usize = 1;
const RUN_START: usize = 7;
const CYCLE_FRAMES: usize = 6;
/// Rendered size (world px). 64 tex px map to 44 world px, which puts the
/// drawn shoulders at ~22 px against the 24 px collision diameter and the
/// muzzle right at the bullet spawn offset.
const SOLDIER_SIZE: f32 = 44.0;

/// Animation thresholds/cadence, world px/s. Full stick = 120 px/s (run);
/// partial thumbstick deflection walks. One stride cycle per 36 px walked
/// keeps footfalls glued to the ground speed.
const IDLE_BELOW: f32 = 6.0;
const RUN_ABOVE: f32 = 78.0;
const CYCLE_LEN_PX: f32 = 36.0;

/// Render-only walk-cycle state (deliberately NOT rollback-registered:
/// cosmetic, so rollbacks never touch it and determinism is untouched).
#[derive(Component, Default)]
pub struct WalkAnim {
    phase: f32,
    last_pos: Option<Vec2>,
}

/// The tracer streak texture (drawn pointing +x; rotated to flight angle).
#[derive(Resource)]
pub struct TracerImage(Handle<Image>);

/// Bullet look: an elongated tracer rotated to its velocity angle.
const BULLET_COLOR: Color = Color::srgb(1.0, 0.9, 0.4);
const BULLET_LEN: f32 = 14.0;
const BULLET_WIDTH: f32 = 2.0;

/// Trail: each frame a bullet leaves a fading segment behind it. Render-only
/// entities/state, never rollback-registered.
const TRAIL_TTL: f32 = 0.15;
const TRAIL_WIDTH: f32 = 1.25;
const TRAIL_ALPHA: f32 = 0.45;
/// A bullet moves 16 px/frame at 60 fps; anything much longer than a few
/// frames' travel is a rollback teleport — skip it rather than streak it.
const TRAIL_MAX_SEG: f32 = 48.0;

/// Render-only per-bullet trail bookkeeping.
#[derive(Component, Default)]
pub struct TrailState {
    last: Option<Vec2>,
}

/// One fading trail segment (independent entity; outlives its bullet).
#[derive(Component)]
pub struct TrailSegment {
    ttl: f32,
}

/// Per-handle player colors (army-men greens first — you are green).
const PLAYER_COLORS: [Color; 8] = [
    Color::srgb(0.35, 0.65, 0.25), // green
    Color::srgb(0.75, 0.65, 0.30), // tan
    Color::srgb(0.55, 0.60, 0.75), // blue-gray
    Color::srgb(0.70, 0.40, 0.30), // rust
    Color::srgb(0.80, 0.75, 0.60), // sand
    Color::srgb(0.45, 0.30, 0.55), // purple
    Color::srgb(0.30, 0.60, 0.60), // teal
    Color::srgb(0.85, 0.55, 0.20), // orange
];

const Z_GROUND: f32 = -10.0;
const Z_TARGET: f32 = 0.0;
const Z_PLAYER: f32 = 1.0;
const Z_TRAIL: f32 = 1.9;
const Z_BULLET: f32 = 2.0;

pub fn setup_scene(
    mut commands: Commands,
    assets: Res<AssetServer>,
    mut layouts: ResMut<Assets<TextureAtlasLayout>>,
) {
    commands.spawn(Camera2d);
    commands.insert_resource(SoldierSheet {
        image: assets.load("soldier.png"),
        layout: layouts.add(TextureAtlasLayout::from_grid(
            UVec2::splat(SOLDIER_FRAME_PX),
            SOLDIER_FRAMES,
            1,
            None,
            None,
        )),
    });
    commands.insert_resource(TracerImage(assets.load("tracer.png")));
    commands.insert_resource(ClearColor(Color::srgb(0.08, 0.10, 0.06)));
    // Tiled grass/dirt ground across the arena (texture from tools/gen_assets.py).
    commands.spawn((
        Sprite {
            image: assets.load("ground.png"),
            custom_size: Some(Vec2::new((ARENA_HALF_W * 2) as f32, (ARENA_HALF_H * 2) as f32)),
            image_mode: SpriteImageMode::Tiled {
                tile_x: true,
                tile_y: true,
                stretch_value: 1.0,
            },
            ..default()
        },
        Transform::from_xyz(0.0, 0.0, Z_GROUND),
    ));
}

/// Give every newly spawned sim entity its look. Runs on `Added<..>` so
/// rollback-respawned entities (bullets especially) get sprites too.
pub fn attach_sprites(
    mut commands: Commands,
    soldier: Res<SoldierSheet>,
    tracer: Res<TracerImage>,
    new_players: Query<(Entity, &Player), Added<Player>>,
    new_bullets: Query<(Entity, &Bullet), Added<Bullet>>,
    new_targets: Query<Entity, Added<Target>>,
) {
    for (entity, player) in &new_players {
        // Grayscale soldier sheet x per-player tint = one-color plastic
        // figure. Rifle direction comes from `orient_players` rotation;
        // walk/run frames from `animate_players`.
        let sprite = Sprite {
            image: soldier.image.clone(),
            texture_atlas: Some(TextureAtlas {
                layout: soldier.layout.clone(),
                index: IDLE_FRAME,
            }),
            color: PLAYER_COLORS[player.handle % PLAYER_COLORS.len()],
            custom_size: Some(Vec2::splat(SOLDIER_SIZE)),
            ..default()
        };
        commands.entity(entity).insert((
            sprite,
            WalkAnim::default(),
            Transform::from_xyz(0.0, 0.0, Z_PLAYER),
        ));
    }
    for (entity, bullet) in &new_bullets {
        // Velocity is constant for a bullet's whole life, so the flight-angle
        // rotation is set once here; `sync_transforms` only writes translation.
        let angle = (bullet.vy as f32).atan2(bullet.vx as f32);
        commands.entity(entity).insert((
            Sprite {
                image: tracer.0.clone(),
                color: BULLET_COLOR,
                custom_size: Some(Vec2::new(BULLET_LEN, BULLET_WIDTH)),
                ..default()
            },
            TrailState::default(),
            Transform::from_xyz(0.0, 0.0, Z_BULLET)
                .with_rotation(Quat::from_rotation_z(angle)),
        ));
    }
    for entity in &new_targets {
        commands.entity(entity).insert((
            Sprite::from_color(Color::srgb(0.55, 0.55, 0.55), Vec2::splat((TARGET_R * 2) as f32)),
            Transform::from_xyz(0.0, 0.0, Z_TARGET),
        ));
    }
}

/// Rotate player pawns toward their sim `Facing` (render-only — the sim's
/// notion of facing stays the raw integer vector).
pub fn orient_players(mut players: Query<(&Facing, &mut Transform), With<Player>>) {
    for (facing, mut transform) in &mut players {
        if facing.x == 0 && facing.y == 0 {
            continue;
        }
        let angle = (facing.y as f32).atan2(facing.x as f32);
        transform.rotation = Quat::from_rotation_z(angle - std::f32::consts::FRAC_PI_2);
    }
}

/// Advance each soldier's walk/run cycle from their *rendered* speed (Pos
/// delta per frame — works for remote players too, and rollback corrections
/// just read as a brief stutter). Stationary → idle frame; sub-max analog
/// deflection → walk cycle; near-full speed → run cycle.
pub fn animate_players(
    time: Res<Time>,
    mut players: Query<(&Pos, &mut WalkAnim, &mut Sprite), With<Player>>,
) {
    let dt = time.delta_secs();
    if dt <= 0.0 {
        return;
    }
    for (pos, mut anim, mut sprite) in &mut players {
        let (x, y) = pos.to_f32();
        let p = Vec2::new(x, y);
        let speed = match anim.last_pos {
            // Cap: a rollback correction or respawn can jump Pos; don't let
            // one frame's teleport read as supersonic legs.
            Some(last) => (p - last).length().min(6.0) / dt,
            None => 0.0,
        };
        anim.last_pos = Some(p);
        let Some(atlas) = sprite.texture_atlas.as_mut() else { continue };
        let index = if speed < IDLE_BELOW {
            anim.phase = 0.0;
            IDLE_FRAME
        } else {
            anim.phase = (anim.phase + speed * dt / CYCLE_LEN_PX).fract();
            let start = if speed > RUN_ABOVE { RUN_START } else { WALK_START };
            start + ((anim.phase * CYCLE_FRAMES as f32) as usize).min(CYCLE_FRAMES - 1)
        };
        if atlas.index != index {
            atlas.index = index;
        }
    }
}

/// Leave a fading streak behind each bullet: one segment per frame from its
/// last rendered position, and age out old segments. Segments are independent
/// entities so they linger (and fade) after the bullet despawns.
pub fn bullet_trails(
    mut commands: Commands,
    time: Res<Time>,
    mut bullets: Query<(&Pos, &mut TrailState), With<Bullet>>,
    mut segments: Query<(Entity, &mut TrailSegment, &mut Sprite)>,
) {
    let dt = time.delta_secs();
    for (pos, mut state) in &mut bullets {
        let (x, y) = pos.to_f32();
        let p = Vec2::new(x, y);
        if let Some(last) = state.last {
            let delta = p - last;
            let len = delta.length();
            if len > 0.5 && len <= TRAIL_MAX_SEG {
                let mid = (p + last) / 2.0;
                commands.spawn((
                    // Slight overlength so consecutive segments overlap into
                    // one continuous streak.
                    Sprite::from_color(
                        BULLET_COLOR.with_alpha(TRAIL_ALPHA),
                        Vec2::new(len + 1.5, TRAIL_WIDTH),
                    ),
                    Transform::from_xyz(mid.x, mid.y, Z_TRAIL)
                        .with_rotation(Quat::from_rotation_z(delta.y.atan2(delta.x))),
                    TrailSegment { ttl: TRAIL_TTL },
                ));
            }
        }
        state.last = Some(p);
    }
    for (entity, mut segment, mut sprite) in &mut segments {
        segment.ttl -= dt;
        if segment.ttl <= 0.0 {
            commands.entity(entity).despawn();
        } else {
            sprite
                .color
                .set_alpha(TRAIL_ALPHA * segment.ttl / TRAIL_TTL);
        }
    }
}

/// Mirror integer sim positions into render transforms, and flash targets that
/// were just hit.
pub fn sync_transforms(
    mut movers: Query<(&Pos, &mut Transform)>,
    mut targets: Query<(&Target, &mut Sprite)>,
) {
    for (pos, mut transform) in &mut movers {
        let (x, y) = pos.to_f32();
        transform.translation.x = x;
        transform.translation.y = y;
    }
    for (target, mut sprite) in &mut targets {
        sprite.color = if target.flash > 0 {
            Color::srgb(0.95, 0.25, 0.2)
        } else {
            Color::srgb(0.55, 0.55, 0.55)
        };
    }
}

/// Where the camera is easing to, before the aim shift. Tracked separately
/// from the camera transform so the follow lerp and the ADS ease don't fight
/// over the same value (the shift rides on top of the focus point).
#[derive(Resource, Default)]
pub struct CameraFocus(Vec2);

/// Keep the local player centered-ish: the camera eases toward them, offset by
/// however far sights are raised. (Uses floats freely — camera position is
/// render-only state.)
pub fn camera_follow(
    local_players: Option<Res<LocalPlayers>>,
    players: Query<(&Player, &Pos)>,
    mut cameras: Query<&mut Transform, (With<Camera2d>, Without<Player>)>,
    mut focus: ResMut<CameraFocus>,
    ads: Res<Ads>,
    time: Res<Time>,
) {
    let Some(local) = local_players else { return };
    let Some(first_local) = local.0.first() else { return };
    let Some((_, pos)) = players.iter().find(|(p, _)| p.handle == *first_local) else {
        return;
    };
    let Ok(mut camera) = cameras.single_mut() else { return };
    let (x, y) = pos.to_f32();
    let t = (time.delta_secs() * 5.0).min(1.0);
    focus.0 = focus.0.lerp(Vec2::new(x, y), t);
    let aim = focus.0 + ads.camera_offset();
    camera.translation.x = aim.x;
    camera.translation.y = aim.y;
}
