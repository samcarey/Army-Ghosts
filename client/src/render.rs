//! Rendering: the float world. Sim entities carry integer `Pos`; these systems
//! attach sprites to them and mirror `Pos` into `Transform` each frame. This
//! is the only place fixed-point becomes f32 — never feed anything from here
//! back into the sim.

use bevy::prelude::*;
use bevy::sprite::Anchor;
use bevy_ggrs::LocalPlayers;

use army_ghosts_sim::{
    Bullet, Bush, Facing, Health, Player, Pos, Rock, Scenario, Stance, Team, ARENA_HALF_H,
    ARENA_HALF_W, STANCE_COUNT, STANCE_STAND, TEAM_COUNT,
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
/// Where the pawn's `Pos` sits in the frame, as a fraction from the top, per
/// stance. Upright figures are anchored where their boots MEET THE GROUND, so
/// they stand on their `Pos` and rise above it; a prone figure is anchored
/// mid-body, because that is what it pivots around when it turns.
///
/// These are the silhouette's bottom, measured off `soldier.png` — NOT the
/// generator's `SOLDIER_GROUND_PY` (58.5), which is where the model's z = 0
/// plane lands. The two differ, and it matters: a boot is a capsule with real
/// extent, and in a 3/4 projection the part of it nearest the camera projects
/// BELOW the origin it is standing on. Anchored on the origin, every pawn's
/// boots hung below the line the grass at its feet is rooted on, which reads
/// exactly as reported: "note foot is below grass".
///
/// Measured bottoms per stance block, over all 16 facings:
///
/// ```text
///   standing   idle 61..64   walk 59..66
///   crouching  idle 61..66   walk 60..67
/// ```
///
/// Anchored on the IDLE MAXIMUM, so a pawn standing still never dips below its
/// own ground line whichever way it faces. The facings that bottom out higher
/// then float up to 3 frame px — invisible, since nothing draws a contact
/// shadow. A walking pawn's trailing boot still swings ~2 px below, and that one
/// is left alone deliberately: anchoring on the walk maximum would hover the
/// figure at idle, and per-frame anchors would bob the whole body in place of
/// swinging the feet. Re-measure with a bbox pass over the sheet if the model or
/// `SOLDIER_TILT` changes.
const STANCE_ANCHOR: [f32; STANCE_COUNT] = [
    64.0 / 72.0,
    66.0 / 72.0,
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
/// sin(40 deg) x 38 px/unit x (SOLDIER_SIZE / frame), plus the `STANCE_ANCHOR`
/// correction (4.3 px standing, 5.9 crouching) — moving the anchor to the boots
/// raised the whole figure against `Pos`, and the muzzle rode up with it. A
/// crawling soldier's rifle is all but on the ground, hence the near-zero third
/// entry, and prone's anchor didn't move.
const STANCE_MUZZLE_LIFT: [f32; STANCE_COUNT] = [26.3, 19.9, 3.0];

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

/// Marks a sprite that stands ON the ground, so its draw order comes from where
/// it stands rather than from a fixed layer: `grass::y_sort` of its ground line,
/// plus a bias.
///
/// Everything in the field shares one z band — pawns, boulders, practice
/// dummies and every band of grass — which is what makes the grass behave: the
/// clumps between you and the camera are drawn after you and swallow your legs,
/// the ones behind you are drawn before you and don't. It also means you can now
/// walk *behind* a boulder as well as in front of one.
///
/// `reach` is how far SOUTH of `Pos` the thing's nearest edge sits, and it is
/// not decoration: a pawn's ground line is its feet (0), but a boulder's is the
/// southern rim of its own footprint. Sorting a boulder by its centre instead
/// let every clump standing in its southern half draw over it — grass growing
/// out of a rock, which is what it looks like.
#[derive(Component)]
pub struct Grounded {
    reach: f32,
    bias: f32,
}

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

/// Team tints, multiplied over the grayscale sheet: the classic army-men GREEN
/// and TAN, which is the whole reason this game looks the way it does.
///
/// Colour is now what tells friend from foe, which makes it a gameplay reading
/// rather than decoration and changes what it has to satisfy. Two constraints,
/// and they pull against each other:
/// * **Both must sit well ABOVE the ground tile in value.** The grass is a dark
///   olive (62,74,42) and a soldier tinted down into that range vanishes into
///   it. That was true when these were per-player uniforms and it is worse now —
///   the green side is the one at risk, so its green is a pale sage rather than
///   anything field-coloured.
/// * **They must differ in HUE AND VALUE, not hue alone.** Under the fog these
///   are drawn at reduced alpha over a green field, and two colours separated
///   only by hue converge as they fade. Tan is the brighter of the two by a
///   clear margin, so a half-faded figure is still identifiable.
const TEAM_COLORS: [Color; TEAM_COUNT] = [
    Color::srgb(0.60, 0.78, 0.52), // green — pale sage, deliberately not field green
    Color::srgb(0.88, 0.79, 0.58), // tan
];

/// Human-facing names for the two sides. Used by the menu's team dial, the round
/// banner and the roster, so all three agree.
pub const TEAM_NAMES: [&str; TEAM_COUNT] = ["GREEN", "TAN"];

/// How far a pawn's tint is nudged per slot within its side, so four figures in
/// the same colours are still four figures. Small on purpose: this must never
/// grow far enough to make one side's darkest read as the other side's
/// lightest.
const TEAM_SHADE_STEP: f32 = 0.05;

/// The tint a pawn wears: its side's colour, shaded a little by its handle.
pub fn team_color(team: Team, handle: usize) -> Color {
    let base = TEAM_COLORS[team.index()];
    // Handles alternate between the sides, so `handle / TEAM_COUNT` is the
    // pawn's place within its own — 0, 1, 2, 3 rather than 0, 2, 4, 6.
    let step = (handle / TEAM_COUNT) as f32 * TEAM_SHADE_STEP;
    let shade = 1.0 - step;
    let rgb = base.to_srgba();
    Color::srgb(rgb.red * shade, rgb.green * shade, rgb.blue * shade)
}

/// The colour a pawn flashes toward when a round lands on it, and how far. Not
/// all the way to red: the tint has to read as "that one just got hit" at a
/// glance across the field without repainting the figure into a different
/// player.
const HURT_COLOR: Color = Color::srgb(1.0, 0.32, 0.24);
const HURT_MIX: f32 = 0.65;

const Z_GROUND: f32 = -10.0;
/// Bullets and their trails ride ABOVE the y-sorted band: a round in flight is
/// off the ground, and a tracer that disappeared behind a tuft would read as a
/// bug rather than as cover.
const Z_TRAIL: f32 = 1.9;
const Z_BULLET: f32 = 2.0;
/// Cover draws *below* the fog mesh at z=5.0 (`vision.rs`) — on purpose: each
/// shadow starts inside its own caster and rolls over its back, so the fog is
/// what shades every rock and bush from the player's side. Canopies go over
/// the boulders and the pawns — you hide *under* a bush.
const Z_BUSH: f32 = 2.5;

/// World units per pixel in `Scenario::GrassStrip`. The pawns stand
/// `2 * STRIP_STANDOFF` (96 units) apart, so at a third of a unit per pixel the
/// pair spans about 290 px of a phone-width window: both of them, the wall
/// between them, and enough ground either side to see where the grass stops.
const STRIP_ZOOM: f32 = 0.33;

pub fn setup_scene(
    mut commands: Commands,
    assets: Res<AssetServer>,
    mut layouts: ResMut<Assets<TextureAtlasLayout>>,
    scenario: Res<Scenario>,
) {
    match *scenario {
        // The game: unzoomed, one world unit per pixel, following the pawn.
        Scenario::Arena => {
            commands.spawn(Camera2d);
        }
        // The rig is a scene to be looked AT, not played: frame both pawns and
        // the wall between them from a fixed camera (`camera_follow` leaves it
        // alone), close enough to see what the grass is doing to the far one.
        Scenario::GrassStrip { .. } => {
            commands.spawn((
                Camera2d,
                Projection::Orthographic(OrthographicProjection {
                    scale: STRIP_ZOOM,
                    ..OrthographicProjection::default_2d()
                }),
            ));
        }
    }
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
    new_players: Query<(Entity, &Player, &Team), Added<Player>>,
    new_bullets: Query<(Entity, &Bullet), Added<Bullet>>,
    stances: Query<(&Player, &Stance)>,
    new_rocks: Query<(Entity, &Rock), Added<Rock>>,
    new_bushes: Query<(Entity, &Bush), Added<Bush>>,
) {
    for (entity, player, team) in &new_players {
        // Grayscale soldier sheet x team tint = one-color plastic figure. The
        // facing comes from picking a sheet row; walk/run frames from
        // `animate_players`. `update_health_visuals` rewrites the colour every
        // frame — this is only what it looks like before the first one.
        let sprite = Sprite {
            image: soldier.image.clone(),
            texture_atlas: Some(TextureAtlas {
                layout: soldier.layout.clone(),
                index: IDLE_FRAME,
            }),
            color: team_color(*team, player.handle),
            custom_size: Some(Vec2::splat(SOLDIER_SIZE)),
            ..default()
        };
        commands.entity(entity).insert((
            sprite,
            // Feet on the pawn's Pos, body rising above it (`animate_players`
            // re-anchors when the stance changes).
            Anchor(stance_anchor(STANCE_STAND)),
            WalkAnim::default(),
            Grounded { reach: 0.0, bias: 0.0 },
            Transform::default(),
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
            // Sorted by the southern rim of its own footprint, so the grass
            // inside that footprint draws BEFORE it — a boulder displaces the
            // sward, it doesn't grow out of it. Only clumps standing in front of
            // the rim cover it, which is what grass in front of a rock does.
            // The hair of bias puts it just under the pawns: a boulder and a
            // soldier on the same line are close enough that the tie should go
            // to the soldier.
            Grounded { reach: rock.r as f32, bias: -0.002 },
            Transform::from_rotation(Quat::from_rotation_z(angle)),
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

/// Show what the sim's [`Health`] is doing to a pawn: flash it toward
/// [`HURT_COLOR`] for the few ticks after it takes a round, and hide it
/// entirely while it's down.
///
/// Must run BEFORE `vision::fade_hidden`, which owns the same sprites' alpha —
/// this writes rgb only and leaves the alpha where concealment put it. Hiding
/// goes through `Visibility` for the same reason: an alpha of zero here would
/// be overwritten a system later.
pub fn update_health_visuals(
    mut players: Query<(&Player, &Team, &Health, &mut Sprite, &mut Visibility), With<Player>>,
) {
    for (player, team, health, mut sprite, mut visibility) in &mut players {
        let wanted = if health.alive() { Visibility::Inherited } else { Visibility::Hidden };
        if *visibility != wanted {
            *visibility = wanted;
        }
        let base = team_color(*team, player.handle);
        let tint = if health.hurt > 0 {
            base.mix(&HURT_COLOR, HURT_MIX)
        } else {
            base
        };
        // Alpha belongs to `fade_hidden`; only the color is ours.
        let alpha = sprite.color.alpha();
        sprite.color = tint.with_alpha(alpha);
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

/// Mirror integer sim positions into render transforms.
pub fn sync_transforms(
    mut movers: Query<(&Pos, Option<&Grounded>, &mut Transform), Without<Bullet>>,
    mut bullets: Query<(&Pos, &MuzzleLift, &mut Transform), With<Bullet>>,
) {
    for (pos, grounded, mut transform) in &mut movers {
        let (x, y) = pos.to_f32();
        transform.translation.x = x;
        transform.translation.y = y;
        // Anything standing in the field re-sorts as it walks: draw order is
        // where your feet are, so the grass in front of you covers you and the
        // grass behind you doesn't (see `grass::y_sort`).
        if let Some(grounded) = grounded {
            transform.translation.z = crate::grass::y_sort(y - grounded.reach) + grounded.bias;
        }
    }
    // Rounds fly at the weapon height they were fired from, not ankle height.
    for (pos, lift, mut transform) in &mut bullets {
        let (x, y) = pos.to_f32();
        transform.translation.x = x;
        transform.translation.y = y + lift.0;
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
#[allow(clippy::too_many_arguments)]
pub fn camera_follow(
    local_players: Option<Res<LocalPlayers>>,
    spectating: Res<crate::spectate::Spectating>,
    players: Query<(&Player, &Pos)>,
    mut cameras: Query<&mut Transform, (With<Camera2d>, Without<Player>)>,
    mut focus: ResMut<CameraFocus>,
    ads: Res<Ads>,
    time: Res<Time>,
    scenario: Res<Scenario>,
) {
    // The rig's camera is fixed on the wall, showing both pawns at once — see
    // `setup_scene`.
    if matches!(*scenario, Scenario::GrassStrip { .. }) {
        return;
    }
    let Some(local) = local_players else { return };
    let Some(first_local) = local.0.first() else { return };
    // Whoever you are watching if you are out, otherwise your own pawn. The
    // camera eases to them rather than cutting, which also does the work of
    // making the change of subject legible — you see the ground go past.
    let watched = spectating.watching.unwrap_or(*first_local);
    let Some((_, pos)) = players.iter().find(|(p, _)| p.handle == watched) else {
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

// NOTE: **do not clamp this camera to the arena.** It was, briefly, on the
// reasoning that a pawn standing on a muster post puts a third of the screen
// outside the world and that looks like a bug. Clamping looks far worse and was
// reported immediately: on any view wider than the arena the bound pins the axis
// outright, so the camera stops following you at all, and on a view slightly
// narrower it follows you *partly*, which reads as the camera being broken
// rather than as a deliberate edge. A camera locked to the player is the thing
// players actually notice; black beyond the wall is not.

