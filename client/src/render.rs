//! Rendering: the float world. Sim entities carry integer `Pos`; these systems
//! attach sprites to them and mirror `Pos` into `Transform` each frame. This
//! is the only place fixed-point becomes f32 — never feed anything from here
//! back into the sim.

use bevy::prelude::*;
use bevy_ggrs::LocalPlayers;

use army_ghosts_sim::{
    Bullet, Facing, Player, Pos, Target, ARENA_HALF_H, ARENA_HALF_W, PLAYER_R, TARGET_R,
};

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
const Z_BULLET: f32 = 2.0;

pub fn setup_scene(mut commands: Commands, assets: Res<AssetServer>) {
    commands.spawn(Camera2d);
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
    new_players: Query<(Entity, &Player), Added<Player>>,
    new_bullets: Query<Entity, Added<Bullet>>,
    new_targets: Query<Entity, Added<Target>>,
) {
    for (entity, player) in &new_players {
        let color = PLAYER_COLORS[player.handle % PLAYER_COLORS.len()];
        commands.entity(entity).insert((
            Sprite::from_color(color, Vec2::splat((PLAYER_R * 2) as f32)),
            Transform::from_xyz(0.0, 0.0, Z_PLAYER),
        ));
        // Gun barrel child: sticks out the facing side (entity rotates via
        // `orient_players`), so you can see where you'll shoot.
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                Sprite::from_color(color.darker(0.12), Vec2::new(5.0, 12.0)),
                Transform::from_xyz(0.0, PLAYER_R as f32 + 4.0, 0.1),
            ));
        });
    }
    for entity in &new_bullets {
        commands.entity(entity).insert((
            Sprite::from_color(Color::srgb(1.0, 0.9, 0.4), Vec2::splat(4.0)),
            Transform::from_xyz(0.0, 0.0, Z_BULLET),
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

/// Keep the local player centered-ish: the camera eases toward them. (Uses
/// floats freely — camera position is render-only state.)
pub fn camera_follow(
    local_players: Option<Res<LocalPlayers>>,
    players: Query<(&Player, &Pos)>,
    mut cameras: Query<&mut Transform, (With<Camera2d>, Without<Player>)>,
    time: Res<Time>,
) {
    let Some(local) = local_players else { return };
    let Some(first_local) = local.0.first() else { return };
    let Some((_, pos)) = players.iter().find(|(p, _)| p.handle == *first_local) else {
        return;
    };
    let Ok(mut camera) = cameras.single_mut() else { return };
    let (x, y) = pos.to_f32();
    let target = Vec3::new(x, y, camera.translation.z);
    let t = (time.delta_secs() * 5.0).min(1.0);
    camera.translation = camera.translation.lerp(target, t);
}
