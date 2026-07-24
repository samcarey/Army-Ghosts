//! Rendering: the float world. Sim entities carry integer `Pos`; these systems
//! attach sprites to them and mirror `Pos` into `Transform` each frame. This
//! is the only place fixed-point becomes f32 — never feed anything from here
//! back into the sim.

use bevy::prelude::*;
use bevy_ggrs::LocalPlayers;

use army_ghosts_sim::{Bullet, Player, Pos, Target, ARENA_HALF_H, ARENA_HALF_W, PLAYER_R, TARGET_R};

/// Per-handle player colors (army-men greens first — you are green).
const PLAYER_COLORS: [Color; 4] = [
    Color::srgb(0.35, 0.65, 0.25), // green
    Color::srgb(0.75, 0.65, 0.30), // tan
    Color::srgb(0.55, 0.60, 0.75), // blue-gray
    Color::srgb(0.70, 0.40, 0.30), // rust
];

const Z_GROUND: f32 = -10.0;
const Z_TARGET: f32 = 0.0;
const Z_PLAYER: f32 = 1.0;
const Z_BULLET: f32 = 2.0;

pub fn setup_scene(mut commands: Commands) {
    commands.spawn(Camera2d);
    commands.insert_resource(ClearColor(Color::srgb(0.08, 0.10, 0.06)));
    // Placeholder ground: a flat dirt-green arena quad. Milestone 1 replaces
    // this with a tiled texture.
    commands.spawn((
        Sprite::from_color(
            Color::srgb(0.24, 0.30, 0.16),
            Vec2::new((ARENA_HALF_W * 2) as f32, (ARENA_HALF_H * 2) as f32),
        ),
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
