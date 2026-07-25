//! Rendering: the float world. Sim entities carry integer `Pos`; these systems
//! attach sprites to them and mirror `Pos` into `Transform` each frame. This
//! is the only place fixed-point becomes f32 — never feed anything from here
//! back into the sim.

use bevy::prelude::*;
use bevy::sprite::Anchor;
use bevy_ggrs::LocalPlayers;

use army_ghosts_sim::{
    Bullet, Bush, Facing, Player, Pos, Rock, Target, ARENA_HALF_H, ARENA_HALF_W, TARGET_R,
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

/// The soldier sprite sheet (tools/gen_assets.py): a grid of 64px frames.
/// Columns are animation (0 idle, 1-6 walk, 7-12 run); rows are the 16 facings,
/// clockwise from away-from-camera.
///
/// The soldier is drawn 3/4 — modelled in 3D and projected 40 degrees off
/// top-down — so it must NEVER be rotated: head stays up and feet stay down,
/// and the facing comes from picking a row. That's why there's no
/// `orient_players` any more.
#[derive(Resource)]
pub struct SoldierSheet {
    image: Handle<Image>,
    layout: Handle<TextureAtlasLayout>,
}

const SOLDIER_FRAME_PX: u32 = 64;
const SOLDIER_COLS: u32 = 13;
const SOLDIER_DIRS: u32 = 16;
/// Where the figure's ground point sits in the frame, as a fraction from the
/// top (`SOLDIER_GROUND_PY / SOLDIER_FRAME_PX` in the generator). The sprite is
/// anchored there so the soldier's feet stand on its `Pos` and the body rises
/// above it, instead of the collision circle cutting it in half.
const SOLDIER_GROUND: f32 = 52.0 / 64.0;
const IDLE_FRAME: usize = 0;
const WALK_START: usize = 1;
const RUN_START: usize = 7;
const CYCLE_FRAMES: usize = 6;
/// Rendered size (world px). The figure fills most of the frame's height, so
/// this draws a soldier about 42 px tall standing on the 24 px collision
/// circle — roughly the proportions these games use.
const SOLDIER_SIZE: f32 = 50.0;
/// How far above a pawn's `Pos` its weapon is drawn, world px. The 3/4 sprite
/// stands with its feet on the Pos, so the rifle sits about this far up the
/// screen; shots have to be lifted to match or tracers appear to leave the
/// soldier's boots. Derived from the rifle's height in the sheet: z 1.16 units
/// x sin(40 deg) x 38 px/unit x (SOLDIER_SIZE / frame) ~= 22.
pub const MUZZLE_LIFT: f32 = 22.0;

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

/// The boulder sheet (tools/gen_assets.py): one row of 96px grayscale blob
/// variants whose outlines average `ROCK_FILL_PX` of the frame, so a rock of
/// sim radius `r` draws at `2r * FRAME / FILL`.
#[derive(Resource)]
pub struct RockSheet {
    image: Handle<Image>,
    layout: Handle<TextureAtlasLayout>,
}

/// The bush sheet, laid out exactly like the boulder sheet.
#[derive(Resource)]
pub struct BushSheet {
    image: Handle<Image>,
    layout: Handle<TextureAtlasLayout>,
}

/// Both cover sheets share these: 96px frames whose blobs average 40px radius,
/// so a piece of cover with sim radius `r` draws at `2r * FRAME / (2 * FILL)`.
const COVER_FRAME_PX: u32 = 96;
const COVER_VARIANTS: u32 = 4;
const COVER_FILL_PX: f32 = 40.0;

/// Canopy opacity. Deliberately partial: one bush is a smudge you can still
/// make out a soldier through, and overlapping bushes stack toward solid — the
/// same stacking the shadow layer does in `vision.rs`.
const BUSH_ALPHA: f32 = 0.62;

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

/// Per-handle tints, multiplied over the grayscale sheet. Desaturated — these
/// are uniforms, not team jerseys — but kept well ABOVE the ground tile in
/// value. Muted is not the same as invisible: the grass is a dark olive
/// (62,74,42) and a soldier tinted down into that range vanishes into it, so
/// the separation here is value, not saturation.
const PLAYER_COLORS: [Color; 8] = [
    Color::srgb(0.66, 0.74, 0.50), // olive drab
    Color::srgb(0.82, 0.74, 0.54), // khaki
    Color::srgb(0.62, 0.70, 0.76), // field grey
    Color::srgb(0.78, 0.58, 0.48), // brick
    Color::srgb(0.84, 0.78, 0.62), // sand
    Color::srgb(0.62, 0.58, 0.70), // slate
    Color::srgb(0.52, 0.72, 0.68), // teal drab
    Color::srgb(0.84, 0.66, 0.44), // ochre
];

const Z_GROUND: f32 = -10.0;
const Z_TARGET: f32 = 0.0;
/// Boulders sit under the pawns, so you can walk in front of one.
const Z_ROCK: f32 = 0.5;
const Z_PLAYER: f32 = 1.0;
const Z_TRAIL: f32 = 1.9;
const Z_BULLET: f32 = 2.0;
/// Cover draws *below* the fog mesh at z=5.0 (`vision.rs`) — on purpose: each
/// shadow starts inside its own caster and rolls over its back, so the fog is
/// what shades every rock and bush from the player's side. Canopies go over
/// the boulders and the pawns — you hide *under* a bush.
const Z_BUSH: f32 = 2.5;

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
            SOLDIER_COLS,
            SOLDIER_DIRS,
            None,
            None,
        )),
    });
    commands.insert_resource(TracerImage(assets.load("tracer.png")));
    let cover_grid = || {
        TextureAtlasLayout::from_grid(UVec2::splat(COVER_FRAME_PX), COVER_VARIANTS, 1, None, None)
    };
    commands.insert_resource(RockSheet {
        image: assets.load("rocks.png"),
        layout: layouts.add(cover_grid()),
    });
    commands.insert_resource(BushSheet {
        image: assets.load("bushes.png"),
        layout: layouts.add(cover_grid()),
    });
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
    rock_sheet: Res<RockSheet>,
    bush_sheet: Res<BushSheet>,
    new_players: Query<(Entity, &Player), Added<Player>>,
    new_bullets: Query<(Entity, &Bullet), Added<Bullet>>,
    new_targets: Query<Entity, Added<Target>>,
    new_rocks: Query<(Entity, &Rock), Added<Rock>>,
    new_bushes: Query<(Entity, &Bush), Added<Bush>>,
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
            // Feet on the pawn's Pos, body rising above it.
            Anchor(Vec2::new(0.0, 0.5 - SOLDIER_GROUND)),
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
    for (entity, rock) in &new_rocks {
        let shade = 0.38 + (rock.seed / 1024 % 64) as f32 * 0.0022;
        commands.entity(entity).insert(cover_sprite(
            &rock_sheet.image,
            &rock_sheet.layout,
            rock.r,
            rock.seed,
            Color::srgb(shade, shade * 0.98, shade * 0.92),
            Z_ROCK,
        ));
    }
    for (entity, bush) in &new_bushes {
        // Brighter and greener than the ground tile on purpose: at 60%-ish
        // opacity over grass, anything subtler just reads as a dark patch of
        // dirt, and cover you can't see is cover you can't use.
        let shade = 0.52 + (bush.seed / 1024 % 64) as f32 * 0.0032;
        commands.entity(entity).insert(cover_sprite(
            &bush_sheet.image,
            &bush_sheet.layout,
            bush.r,
            bush.seed,
            Color::srgba(shade * 0.75, shade * 1.45, shade * 0.55, BUSH_ALPHA),
            Z_BUSH,
        ));
    }
}

/// One piece of cover's look. Variant, spin and shade all come off its own
/// seed, so a dozen rocks out of four textures still read as a dozen different
/// boulders — and every peer draws the same field (cosmetic, but it keeps
/// screenshots comparable across machines).
fn cover_sprite(
    image: &Handle<Image>,
    layout: &Handle<TextureAtlasLayout>,
    radius: i32,
    seed: u32,
    color: Color,
    z: f32,
) -> (Sprite, Transform) {
    let angle = (seed / COVER_VARIANTS % 360) as f32 * std::f32::consts::PI / 180.0;
    (
        Sprite {
            image: image.clone(),
            texture_atlas: Some(TextureAtlas {
                layout: layout.clone(),
                index: (seed % COVER_VARIANTS) as usize,
            }),
            color,
            custom_size: Some(Vec2::splat(
                (radius * 2) as f32 * COVER_FRAME_PX as f32 / (COVER_FILL_PX * 2.0),
            )),
            ..default()
        },
        Transform::from_xyz(0.0, 0.0, z).with_rotation(Quat::from_rotation_z(angle)),
    )
}

/// Advance each soldier's walk/run cycle from their *rendered* speed (Pos
/// delta per frame — works for remote players too, and rollback corrections
/// just read as a brief stutter). Stationary → idle frame; sub-max analog
/// deflection → walk cycle; near-full speed → run cycle.
pub fn animate_players(
    time: Res<Time>,
    mut players: Query<(&Pos, &Facing, &mut WalkAnim, &mut Sprite), With<Player>>,
) {
    let dt = time.delta_secs();
    if dt <= 0.0 {
        return;
    }
    for (pos, facing, mut anim, mut sprite) in &mut players {
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
        let column = if speed < IDLE_BELOW {
            anim.phase = 0.0;
            IDLE_FRAME
        } else {
            anim.phase = (anim.phase + speed * dt / CYCLE_LEN_PX).fract();
            let start = if speed > RUN_ABOVE { RUN_START } else { WALK_START };
            start + ((anim.phase * CYCLE_FRAMES as f32) as usize).min(CYCLE_FRAMES - 1)
        };
        // Row = facing, measured clockwise from "away from the camera", which
        // is how the generator lays the sheet out.
        let bearing = (facing.x as f32).atan2(facing.y as f32);
        let step = std::f32::consts::TAU / SOLDIER_DIRS as f32;
        let row = (bearing / step).round().rem_euclid(SOLDIER_DIRS as f32) as usize;
        let index = row * SOLDIER_COLS as usize + column;
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
        let p = Vec2::new(x, y + MUZZLE_LIFT); // match the lifted tracer
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
    mut movers: Query<(&Pos, &mut Transform), Without<Bullet>>,
    mut bullets: Query<(&Pos, &mut Transform), With<Bullet>>,
    mut targets: Query<(&Target, &mut Sprite)>,
) {
    for (pos, mut transform) in &mut movers {
        let (x, y) = pos.to_f32();
        transform.translation.x = x;
        transform.translation.y = y;
    }
    // Rounds fly at weapon height, not ankle height.
    for (pos, mut transform) in &mut bullets {
        let (x, y) = pos.to_f32();
        transform.translation.x = x;
        transform.translation.y = y + MUZZLE_LIFT;
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
