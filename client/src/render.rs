//! Rendering: the float world. Sim entities carry integer `Pos`; these systems
//! attach sprites to them and mirror `Pos` into `Transform` each frame. This
//! is the only place fixed-point becomes f32 — never feed anything from here
//! back into the sim.

use bevy::prelude::*;
use bevy::sprite::Anchor;
use bevy_ggrs::LocalPlayers;

use army_ghosts_sim::{
    Bullet, Bush, Facing, Player, Pos, Rock, Stance, Target, ARENA_HALF_H, ARENA_HALF_W,
    STANCE_COUNT, STANCE_STAND, TARGET_R,
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

/// The soldier sprite sheet (tools/gen_assets.py): a grid of 72px frames.
/// Rows are the 16 facings, clockwise from away-from-camera; columns are three
/// stance blocks (standing, crouching, prone) of 13 animation frames each
/// (0 idle, 1-6 walk, 7-12 run).
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

const SOLDIER_FRAME_PX: u32 = 72;
/// Animation columns per stance, and the full sheet width in columns.
const STANCE_COLS: u32 = 13;
const SOLDIER_COLS: u32 = STANCE_COLS * STANCE_COUNT as u32;
const SOLDIER_DIRS: u32 = 16;
/// Where the pawn's `Pos` sits in the frame, as a fraction from the top (the
/// generator's `SOLDIER_GROUND_PY` / `SOLDIER_PRONE_PY` over the frame size),
/// per stance. Upright figures are anchored at the ground between their feet,
/// so they stand on their `Pos` and rise above it; a prone figure is anchored
/// mid-body, because that is what it pivots around when it turns.
const STANCE_ANCHOR: [f32; STANCE_COUNT] = [
    58.5 / 72.0,
    58.5 / 72.0,
    36.0 / 72.0,
];
const IDLE_FRAME: usize = 0;
const WALK_START: usize = 1;
const RUN_START: usize = 7;
const CYCLE_FRAMES: usize = 6;
/// Rendered size (world px). The figure fills most of the frame's height, so
/// this draws a soldier about 42 px tall standing on the 24 px collision
/// circle — roughly the proportions these games use.
const SOLDIER_SIZE: f32 = 56.25;
/// How far above a pawn's `Pos` its weapon is drawn, world px, per stance. The
/// 3/4 sprite stands with its feet on the Pos, so the rifle sits this far up
/// the screen; shots have to be lifted to match or tracers appear to leave the
/// soldier's boots. Derived from the rifle's height in the sheet: z units x
/// sin(40 deg) x 38 px/unit x (SOLDIER_SIZE / frame). A crawling soldier's
/// rifle is all but on the ground, hence the near-zero third entry.
const STANCE_MUZZLE_LIFT: [f32; STANCE_COUNT] = [22.0, 14.0, 3.0];

pub fn muzzle_lift(level: u8) -> f32 {
    STANCE_MUZZLE_LIFT[(level as usize).min(STANCE_COUNT - 1)]
}

/// Render-only: the height a bullet's tracer flies at, fixed when it spawns
/// from the stance its shooter fired in. Rollback re-spawns bullets, which
/// re-runs `attach_sprites` against the rolled-back stance, so this stays
/// correct through corrections.
#[derive(Component)]
pub struct MuzzleLift(f32);

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

/// The bush sheet: same 96px frames, but the bushes are *colour* (fractal
/// branches and leaves, see `tools/gen_assets.py`) and modelled in 3D at the
/// soldiers' viewing angle — so unlike boulders they have a definite up and
/// must never be spun by their seed.
#[derive(Resource)]
pub struct BushSheet {
    image: Handle<Image>,
    layout: Handle<TextureAtlasLayout>,
}

/// Both cover sheets use 96px frames; a piece of cover with sim radius `r`
/// draws at `2r * FRAME / (2 * FILL)`, where `FILL` is the mean radius the
/// generator fills its frame to.
const COVER_FRAME_PX: u32 = 96;
const ROCK_VARIANTS: u32 = 4;
const ROCK_FILL_PX: f32 = 40.0;
const BUSH_VARIANTS: u32 = 6;
const BUSH_FILL_PX: f32 = 30.0;

/// Canopy opacity. Deliberately partial: one bush is a smudge you can still
/// make out a soldier through, and overlapping bushes stack toward solid — the
/// same stacking the shadow layer does in `vision.rs`. Higher than it used to
/// be because the canopy now has real gaps in it: the leaves do some of the
/// see-through that a flat alpha used to have to do alone.
const BUSH_ALPHA: f32 = 0.90;

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
    let cover_grid = |variants| {
        TextureAtlasLayout::from_grid(UVec2::splat(COVER_FRAME_PX), variants, 1, None, None)
    };
    commands.insert_resource(RockSheet {
        image: assets.load("rocks.png"),
        layout: layouts.add(cover_grid(ROCK_VARIANTS)),
    });
    commands.insert_resource(BushSheet {
        image: assets.load("bushes.png"),
        layout: layouts.add(cover_grid(BUSH_VARIANTS)),
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
    stances: Query<(&Player, &Stance)>,
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
            // Feet on the pawn's Pos, body rising above it (`animate_players`
            // re-anchors when the stance changes).
            Anchor(stance_anchor(STANCE_STAND)),
            WalkAnim::default(),
            Transform::from_xyz(0.0, 0.0, Z_PLAYER),
        ));
    }
    for (entity, bullet) in &new_bullets {
        // Velocity is constant for a bullet's whole life, so the flight-angle
        // rotation is set once here; `sync_transforms` only writes translation.
        let angle = (bullet.vy as f32).atan2(bullet.vx as f32);
        // Fired from the shooter's rifle, wherever that was: a crawling
        // soldier's rounds skim the ground, a standing one's fly at chest
        // height. Frozen at spawn — the shooter may stand up mid-flight.
        let lift = stances
            .iter()
            .find(|(p, _)| p.handle == bullet.owner)
            .map(|(_, stance)| muzzle_lift(stance.level))
            .unwrap_or(STANCE_MUZZLE_LIFT[0]);
        commands.entity(entity).insert((
            Sprite {
                image: tracer.0.clone(),
                color: BULLET_COLOR,
                custom_size: Some(Vec2::new(BULLET_LEN, BULLET_WIDTH)),
                ..default()
            },
            MuzzleLift(lift),
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
        // Variant, spin and shade all come off the rock's own seed, so a dozen
        // boulders out of four textures still read as a dozen boulders — and
        // every peer draws the same field (cosmetic, but it keeps screenshots
        // comparable across machines).
        // Lightened when the grass went in: against an olive ground tile 0.38
        // read as stone, but with a sward around it a dark boulder reads as a
        // hole in the ground rather than a rock standing in the field.
        let shade = 0.52 + (rock.seed / 1024 % 64) as f32 * 0.0022;
        let angle = (rock.seed / ROCK_VARIANTS % 360) as f32 * std::f32::consts::PI / 180.0;
        commands.entity(entity).insert((
            Sprite {
                image: rock_sheet.image.clone(),
                texture_atlas: Some(TextureAtlas {
                    layout: rock_sheet.layout.clone(),
                    index: (rock.seed % ROCK_VARIANTS) as usize,
                }),
                color: Color::srgb(shade, shade * 0.98, shade * 0.92),
                custom_size: Some(cover_size(rock.r, ROCK_FILL_PX)),
                ..default()
            },
            Transform::from_xyz(0.0, 0.0, Z_ROCK).with_rotation(Quat::from_rotation_z(angle)),
        ));
    }
    for (entity, bush) in &new_bushes {
        // The bush frames carry their own greens, so the tint is near-white —
        // just enough per-seed value drift that neighbours in a thicket don't
        // look stamped. No rotation (a 3/4-view bush spun on its z tips over);
        // variety comes from six variants plus a mirror.
        let shade = 0.86 + (bush.seed / 1024 % 32) as f32 * 0.0075;
        commands.entity(entity).insert((
            Sprite {
                image: bush_sheet.image.clone(),
                texture_atlas: Some(TextureAtlas {
                    layout: bush_sheet.layout.clone(),
                    index: (bush.seed % BUSH_VARIANTS) as usize,
                }),
                color: Color::srgba(shade * 0.97, shade, shade * 0.93, BUSH_ALPHA),
                custom_size: Some(cover_size(bush.r, BUSH_FILL_PX)),
                flip_x: bush.seed / BUSH_VARIANTS % 2 == 1,
                ..default()
            },
            Transform::from_xyz(0.0, 0.0, Z_BUSH),
        ));
    }
}

/// On-screen size of a piece of cover: scale the frame so the blob the
/// generator filled it to (`fill` px mean radius) lands on the sim radius.
fn cover_size(radius: i32, fill: f32) -> Vec2 {
    Vec2::splat((radius * 2) as f32 * COVER_FRAME_PX as f32 / (fill * 2.0))
}

/// Where a stance's sprite hangs relative to the pawn's `Pos`.
fn stance_anchor(level: u8) -> Vec2 {
    Vec2::new(0.0, 0.5 - STANCE_ANCHOR[(level as usize).min(STANCE_COUNT - 1)])
}

/// Advance each soldier's walk/run cycle from their *rendered* speed (Pos
/// delta per frame — works for remote players too, and rollback corrections
/// just read as a brief stutter). Stationary → idle frame; sub-max analog
/// deflection → walk cycle; near-full speed → run cycle. The stance picks
/// which block of columns all of that indexes into.
pub fn animate_players(
    time: Res<Time>,
    mut players: Query<
        (&Pos, &Facing, &Stance, &mut WalkAnim, &mut Sprite, &mut Anchor),
        With<Player>,
    >,
) {
    let dt = time.delta_secs();
    if dt <= 0.0 {
        return;
    }
    for (pos, facing, stance, mut anim, mut sprite, mut anchor) in &mut players {
        let wanted = stance_anchor(stance.level);
        if anchor.0 != wanted {
            anchor.0 = wanted;
        }
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
            // Only a standing soldier can outrun the walk cycle: crouching and
            // crawling top out below the threshold, and the sheet's run
            // columns for those stances just repeat the walk.
            let running = speed > RUN_ABOVE && stance.level == STANCE_STAND;
            let start = if running { RUN_START } else { WALK_START };
            start + ((anim.phase * CYCLE_FRAMES as f32) as usize).min(CYCLE_FRAMES - 1)
        };
        // Row = facing, measured clockwise from "away from the camera", which
        // is how the generator lays the sheet out; the stance selects which
        // 13-column block of that row to read.
        let bearing = (facing.x as f32).atan2(facing.y as f32);
        let step = std::f32::consts::TAU / SOLDIER_DIRS as f32;
        let row = (bearing / step).round().rem_euclid(SOLDIER_DIRS as f32) as usize;
        let block = (stance.level as usize).min(STANCE_COUNT - 1) * STANCE_COLS as usize;
        let index = row * SOLDIER_COLS as usize + block + column;
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
    mut bullets: Query<(&Pos, &MuzzleLift, &mut TrailState), With<Bullet>>,
    mut segments: Query<(Entity, &mut TrailSegment, &mut Sprite)>,
) {
    let dt = time.delta_secs();
    for (pos, lift, mut state) in &mut bullets {
        let (x, y) = pos.to_f32();
        let p = Vec2::new(x, y + lift.0); // match the lifted tracer
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
    mut bullets: Query<(&Pos, &MuzzleLift, &mut Transform), With<Bullet>>,
    mut targets: Query<(&Target, &mut Sprite)>,
) {
    for (pos, mut transform) in &mut movers {
        let (x, y) = pos.to_f32();
        transform.translation.x = x;
        transform.translation.y = y;
    }
    // Rounds fly at the weapon height they were fired from, not ankle height.
    for (pos, lift, mut transform) in &mut bullets {
        let (x, y) = pos.to_f32();
        transform.translation.x = x;
        transform.translation.y = y + lift.0;
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
