//! Grass: the sward everything stands in, and the thing it hides.
//!
//! The depth of the grass anywhere is [`grass_height`] — a pure function in the
//! sim, so every peer agrees without a single entity being spawned or rolled
//! back. Everything here is the render half of that one number, in three layers:
//!
//!   * **The field.** One static mesh over the arena, textured with `grass.png`
//!     tiled in world space and tinted per vertex from the local depth: pale and
//!     patchy over thin ground (the dirt tile shows through the alpha), lush and
//!     opaque in the deep patches. Vertex interpolation across the grid is what
//!     makes area-to-area transitions smooth for free.
//!   * **Tufts.** Clumps scattered on a jittered grid, each scaled to the depth
//!     where it stands, and all of them baked into a second static mesh rather
//!     than spawned as ~1300 sprites nothing ever moves. They draw *under*
//!     everything, so they never hide anyone; they're what makes deep grass read
//!     as deep before you walk into it.
//!   * **Curtains.** The mechanic. A band of the same blades drawn *over*
//!     whatever is standing in the grass, tall enough to swallow exactly as much
//!     of it as the grass is deep, plus a shadow gradient above that so the part
//!     still showing is at least down in the gloom.
//!
//! How much of a pawn a curtain swallows is [`army_ghosts_sim::grass_cover`]:
//! grass hides everything shorter than itself, so it's the ratio of the local
//! depth to the pawn's `STANCE_HEIGHT`. That makes going flat worth far more
//! than any hand-tuned stance bonus — a prone soldier is 15 units tall and
//! disappears in grass a standing one barely notices.
//!
//! Render-only, all of it. The sim never asks who can see what (it can't —
//! every peer simulates every pawn), exactly like `vision.rs`.

use bevy::asset::RenderAssetUsages;
use bevy::image::{ImageAddressMode, ImageLoaderSettings, ImageSampler, ImageSamplerDescriptor};
use bevy::mesh::{Indices, MeshVertexBufferLayoutRef, PrimitiveTopology};
use bevy::prelude::*;
use bevy::render::render_resource::{
    AsBindGroup, RenderPipelineDescriptor, SpecializedMeshPipelineError,
};
use bevy::shader::ShaderRef;
use bevy::sprite::Anchor;
use bevy::sprite_render::{AlphaMode2d, Material2d, Material2dKey, Material2dPlugin};
use bevy_ggrs::LocalPlayers;

use army_ghosts_sim::{
    grass_cover, grass_height, Bush, Player, Pos, Rock, Stance, ARENA_HALF_H, ARENA_HALF_W, FP,
    GRASS_MAX_H, STANCE_COUNT, STANCE_HEIGHT,
};

/// The projection foreshortening: one world unit of *height* draws this far up
/// the screen. It's `sin(40 deg)` — the same tilt the soldier, bush and grass
/// sheets are all modelled at (`tools/gen_assets.py`), which is why a blade, a
/// canopy and a rifle all lean together.
pub const GRASS_VERT: f32 = 0.6428;
/// Frame layout shared by `tufts.png` and `skirt.png` (see `gen_assets.py`):
/// the ground line sits this far up from the bottom edge, and a blade of model
/// height 1.0 rises this fraction of the frame.
const GRASS_BASE_FRAC: f32 = 0.15;
const GRASS_RISE_FRAC: f32 = 0.82;

/// World px the detail texture repeats over. Small enough that blades are blade
/// sized; the shader crosses two octaves of it so the tiling doesn't read.
const GRASS_TEX_PX: f32 = 128.0;
/// Field mesh resolution, world units. The depth field's steepest gradient runs
/// about 2 units per unit walked, so 20 px between samples is well inside what
/// vertex interpolation can carry without banding.
const GRASS_GRID: i32 = 20;

/// Tuft grid spacing, and the depth below which a patch isn't worth a clump.
const TUFT_STEP: i32 = 20;
const TUFT_MIN_H: i32 = 8;
/// Deep grass gets a second, offset pass of clumps — density carries depth as
/// much as height does.
const TUFT_DENSE_H: i32 = 30;
const TUFT_VARIANTS: usize = 8;

const SKIRT_VARIANTS: usize = 3;
const SKIRT_W: u32 = 128;
const SKIRT_H: u32 = 64;

/// Between the dirt tile (-10) and everything that stands on it. Tufts sit just
/// above the field mesh and below all cover and pawns.
const Z_FIELD: f32 = -9.0;
const Z_TUFT: f32 = -8.5;
/// Curtains ride on their owner, just in front of it: shadow first, blades over
/// the top. Small enough offsets that they stay inside the owner's z band.
const Z_CURTAIN_SHADE: f32 = 0.02;
const Z_CURTAIN: f32 = 0.04;

/// Grass shade. Not black — a shadow among green blades is still green, and
/// pure black over a soldier reads as a hole in the sprite. Weak, too: it is
/// the gloom *between* blades, and at full strength it turns every boulder
/// standing in deep grass into a dark hole in the ground.
const SHADE_COLOR: Color = Color::srgb(0.10, 0.14, 0.07);
const SHADE_ALPHA: f32 = 0.34;
/// How far above the blade tips the shadow reaches, as a multiple of the
/// curtain height. Grass doesn't stop shading you the moment it stops touching
/// you.
const SHADE_REACH: f32 = 1.4;
/// Your own pawn's curtain is drawn this much weaker (and its shadow weaker
/// still). Everyone else sees you at full strength — this is only so that lying
/// in deep grass doesn't mean staring at a patch of lawn wondering where you
/// are.
const LOCAL_CURTAIN_ALPHA: f32 = 0.34;
const LOCAL_SHADE_ALPHA: f32 = 0.2;

/// Where a pawn meets the grass, per stance: `base` is its ground line relative
/// to `Pos` (negative = the sprite hangs below), `span` how tall it draws above
/// that line, `height` how tall it physically is, `width` how wide a curtain it
/// needs. The three px numbers are measured off `soldier.png` bounding boxes
/// (frame px x `SOLDIER_SIZE / SOLDIER_FRAME`); prone hangs below `Pos` because
/// that sprite is anchored mid-body, and is the widest because a soldier lying
/// side-on is as long as a standing one is tall.
const STANCE_CURTAIN: [Curtain; STANCE_COUNT] = [
    Curtain { base: -5.1, span: 43.8, height: STANCE_HEIGHT[0] as f32, width: 46.0 },
    Curtain { base: -6.6, span: 38.2, height: STANCE_HEIGHT[1] as f32, width: 46.0 },
    Curtain { base: -17.2, span: 38.3, height: STANCE_HEIGHT[2] as f32, width: 62.0 },
];

/// One thing standing in the grass, as far as the grass is concerned.
#[derive(Component, Copy, Clone)]
pub struct Curtain {
    base: f32,
    span: f32,
    height: f32,
    width: f32,
}

/// Marks the shadow half of a curtain (the blades are the other child).
#[derive(Component)]
pub struct CurtainShade;

/// The sheets the curtains are drawn from (the two static meshes bake theirs in
/// at startup and need nothing kept).
#[derive(Resource)]
pub struct GrassAssets {
    skirt: Handle<Image>,
    skirt_layout: Handle<TextureAtlasLayout>,
    shade: Handle<Image>,
}

/// Textured, vertex-colored, unlit — see `assets/grass.wgsl` for why this isn't
/// `ColorMaterial` (the same vertex-color specialization trap as `FogMaterial`).
#[derive(Asset, TypePath, AsBindGroup, Clone)]
pub struct GrassMaterial {
    /// `x` is how much of a second, finer octave of the texture to cross in —
    /// only the tiled ground wants it (see the shader).
    #[uniform(0)]
    pub params: Vec4,
    #[texture(1)]
    #[sampler(2)]
    pub texture: Handle<Image>,
}

impl Material2d for GrassMaterial {
    fn vertex_shader() -> ShaderRef {
        "grass.wgsl".into()
    }

    fn fragment_shader() -> ShaderRef {
        "grass.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode2d {
        AlphaMode2d::Blend
    }

    fn specialize(
        descriptor: &mut RenderPipelineDescriptor,
        layout: &MeshVertexBufferLayoutRef,
        _key: Material2dKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        descriptor.vertex.buffers = vec![layout.0.get_layout(&[
            Mesh::ATTRIBUTE_POSITION.at_shader_location(0),
            Mesh::ATTRIBUTE_UV_0.at_shader_location(2),
            Mesh::ATTRIBUTE_COLOR.at_shader_location(4),
        ])?];
        Ok(())
    }
}

pub struct GrassPlugin;

impl Plugin for GrassPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(Material2dPlugin::<GrassMaterial>::default());
    }
}

/// A sprite this many world px tall draws grass `h` world units deep, given the
/// sheet layout above.
fn sprite_height(h: f32) -> f32 {
    h * GRASS_VERT / GRASS_RISE_FRAC
}

/// Grass sprites pivot on their ground line, not their middle.
fn grass_anchor() -> Anchor {
    Anchor(Vec2::new(0.0, -0.5 + GRASS_BASE_FRAC))
}

/// Where a pawn stands, in the whole world units the grass field is sampled at.
fn units(pos: &Pos) -> (i32, i32) {
    (pos.x / FP, pos.y / FP)
}

/// The look of grass `h` units deep: a near-white tint over the sheets' own
/// greens (so hue and value drift from area to area without repainting
/// anything), and how completely it covers the dirt underneath. Thin ground is
/// dry, pale and see-through; deep grass is greener, darker and solid.
fn grass_look(h: f32) -> (Color, f32) {
    let f = (h / GRASS_MAX_H as f32).clamp(0.0, 1.0);
    let lerp = |dry: f32, lush: f32| dry + (lush - dry) * f;
    (
        // Kept well DOWN in value on purpose: `PLAYER_COLORS` were picked to sit
        // above the old olive ground tile, and a bright green sward puts a
        // camouflaged soldier back into the background it was tuned against.
        Color::srgb(lerp(0.82, 0.56), lerp(0.78, 0.74), lerp(0.62, 0.50)),
        // Linear, not eased: thin ground has to actually show dirt, or the whole
        // arena reads as one lawn and the depth field may as well not exist.
        0.12 + 0.88 * f,
    )
}

/// How tall to draw a curtain, in world px, given how much of its owner the
/// grass swallows (`fraction`) and how tall that owner draws (`span`).
///
/// The `max` is why grass in front of a crawling soldier is still as tall as the
/// grass beside them: once the fraction saturates, the blades keep growing with
/// the depth even though there's nothing left to hide.
fn cover_px(depth: f32, fraction: f32, span: f32) -> f32 {
    (depth * GRASS_VERT).max(fraction.clamp(0.0, 1.0) * span)
}

/// Cheap deterministic hash for scattering tufts. Same family as the sim's
/// layout hash, so every peer grows the same field — the grass isn't sim state,
/// but a screenshot from two machines should still match.
fn scatter(x: i32, y: i32, salt: u32) -> u32 {
    let mut h = (x as u32).wrapping_mul(0x27D4_EB2D) ^ (y as u32).wrapping_mul(0x1656_67B1) ^ salt;
    h ^= h >> 15;
    h = h.wrapping_mul(0x2545_F491);
    h ^ (h >> 13)
}

pub fn setup_grass(
    mut commands: Commands,
    assets: Res<AssetServer>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut layouts: ResMut<Assets<TextureAtlasLayout>>,
    mut materials: ResMut<Assets<GrassMaterial>>,
) {
    // World-space UVs run well past 1, so the detail texture has to repeat —
    // the default sampler clamps, which would smear one row of pixels across
    // the whole arena.
    let texture = assets.load_with_settings("grass.png", |s: &mut ImageLoaderSettings| {
        s.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
            address_mode_u: ImageAddressMode::Repeat,
            address_mode_v: ImageAddressMode::Repeat,
            ..default()
        });
    });
    commands.spawn((
        Mesh2d(meshes.add(field_mesh())),
        MeshMaterial2d(materials.add(GrassMaterial {
            params: Vec4::new(0.4, 0.0, 0.0, 0.0),
            texture,
        })),
        Transform::from_xyz(0.0, 0.0, Z_FIELD),
    ));
    commands.spawn((
        Mesh2d(meshes.add(tuft_mesh())),
        MeshMaterial2d(materials.add(GrassMaterial {
            params: Vec4::ZERO, // atlas UVs — crossing octaves would sample the neighbours
            texture: assets.load("tufts.png"),
        })),
        Transform::from_xyz(0.0, 0.0, Z_TUFT),
    ));

    let grass = GrassAssets {
        skirt: assets.load("skirt.png"),
        skirt_layout: layouts.add(TextureAtlasLayout::from_grid(
            UVec2::new(SKIRT_W, SKIRT_H),
            SKIRT_VARIANTS as u32,
            1,
            None,
            None,
        )),
        shade: assets.load("shade.png"),
    };
    commands.insert_resource(grass);
}

/// The arena-wide sward mesh: a grid of quads, colored per vertex from the
/// depth field.
fn field_mesh() -> Mesh {
    let (mut positions, mut uvs, mut colors, mut indices) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let cols = (ARENA_HALF_W * 2 / GRASS_GRID) as u32;
    let rows = (ARENA_HALF_H * 2 / GRASS_GRID) as u32;
    for row in 0..=rows {
        for col in 0..=cols {
            let x = -ARENA_HALF_W + col as i32 * GRASS_GRID;
            let y = -ARENA_HALF_H + row as i32 * GRASS_GRID;
            let (tint, alpha) = grass_look(grass_height(x, y) as f32);
            let tint = tint.to_linear();
            positions.push([x as f32, y as f32, 0.0]);
            uvs.push([x as f32 / GRASS_TEX_PX, -y as f32 / GRASS_TEX_PX]);
            colors.push([tint.red, tint.green, tint.blue, alpha]);
        }
    }
    for row in 0..rows {
        for col in 0..cols {
            let a = row * (cols + 1) + col;
            let (b, c, d) = (a + 1, a + cols + 2, a + cols + 1);
            indices.extend([a, b, c, a, c, d]);
        }
    }
    let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default());
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uvs);
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colors);
    mesh.insert_indices(Indices::U32(indices));
    mesh
}

/// Every clump in the arena, as ONE mesh: a quad per tuft, UVs into
/// `tufts.png`, tint in the vertex color.
///
/// Sprites would be the obvious way and are the wrong one here. Nothing about a
/// tuft ever changes, so ~1300 of them would be ~1300 entities extracted, sorted
/// and batched every single frame on a phone, to draw exactly the same thing as
/// a mesh built once. Emitting them north-to-south also fixes their overlap
/// order for free: the near clump is drawn last, so it covers the far one.
fn tuft_mesh() -> Mesh {
    let mut clumps: Vec<(i32, i32, u32)> = Vec::new();
    let mut consider = |x: i32, y: i32, salt: u32| {
        let noise = scatter(x, y, salt);
        // Jitter off the grid, or the clumps line up in rows the eye finds
        // immediately.
        let jx = x + (noise % TUFT_STEP as u32) as i32 - TUFT_STEP / 2;
        let jy = y + (noise / 64 % TUFT_STEP as u32) as i32 - TUFT_STEP / 2;
        if jx.abs() > ARENA_HALF_W || jy.abs() > ARENA_HALF_H {
            return;
        }
        let h = grass_height(jx, jy);
        // The second pass only fills in where the grass is deep: density has to
        // carry depth as much as height does, or a deep patch is just a taller
        // version of the same lawn.
        if h < TUFT_MIN_H || (salt != 0 && h < TUFT_DENSE_H) {
            return;
        }
        clumps.push((jx, jy, noise));
    };
    let mut y = -ARENA_HALF_H;
    while y <= ARENA_HALF_H {
        let mut x = -ARENA_HALF_W;
        while x <= ARENA_HALF_W {
            consider(x, y, 0);
            consider(x + TUFT_STEP / 2, y + TUFT_STEP / 2, 0x51DE);
            x += TUFT_STEP;
        }
        y += TUFT_STEP;
    }
    clumps.sort_by_key(|&(x, y, _)| (std::cmp::Reverse(y), x));

    let (mut positions, mut uvs, mut colors, mut indices) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (x, y, noise) in clumps {
        let h = grass_height(x, y) as f32;
        // Clumps aren't all the same size even where the grass is level.
        let height = sprite_height(h) * (0.78 + (noise / 4096 % 45) as f32 * 0.01);
        let (half_w, x, y) = (height * 0.55, x as f32, y as f32);
        // The frame's ground line is `GRASS_BASE_FRAC` up from its bottom edge,
        // so the quad hangs that much below the clump's own position.
        let (bottom, top) = (y - height * GRASS_BASE_FRAC, y + height * (1.0 - GRASS_BASE_FRAC));
        let variant = (noise / 128) as usize % TUFT_VARIANTS;
        let (u0, u1) = {
            let (a, b) = (variant as f32 / TUFT_VARIANTS as f32,
                          (variant + 1) as f32 / TUFT_VARIANTS as f32);
            if noise / 32 % 2 == 0 { (b, a) } else { (a, b) } // mirror = swapped u
        };
        let (tint, _) = grass_look(h);
        let tint = tint.to_linear();
        let base = positions.len() as u32;
        for (px, py, u, v) in [
            (x - half_w, top, u0, 0.0),
            (x + half_w, top, u1, 0.0),
            (x + half_w, bottom, u1, 1.0),
            (x - half_w, bottom, u0, 1.0),
        ] {
            positions.push([px, py, 0.0]);
            uvs.push([u, v]);
            colors.push([tint.red, tint.green, tint.blue, 1.0]);
        }
        indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default());
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uvs);
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colors);
    mesh.insert_indices(Indices::U32(indices));
    mesh
}

/// Give everything that can stand in the grass a curtain. Runs on `Added<..>`
/// like `attach_sprites`, so rollback-respawned cover gets one too.
pub fn attach_curtains(
    mut commands: Commands,
    grass: Option<Res<GrassAssets>>,
    new_players: Query<Entity, Added<Player>>,
    new_rocks: Query<(Entity, &Rock), Added<Rock>>,
    new_bushes: Query<(Entity, &Bush), Added<Bush>>,
) {
    let Some(grass) = grass else { return };
    for entity in &new_players {
        // Sized every frame by `update_curtains` — a pawn's profile changes
        // with its stance and the ground it's walking over.
        spawn_curtain(&mut commands, &grass, entity, STANCE_CURTAIN[0], 0);
    }
    for (entity, rock) in &new_rocks {
        // A boulder is drawn centred on its `Pos` at `2r * FRAME / FILL`, so its
        // lower edge is about `r` below it, and it stands roughly as tall as it
        // is wide across the bottom.
        let r = rock.r as f32;
        let curtain = Curtain { base: -r * 0.85, span: r * 1.7, height: r * 1.5, width: r * 2.2 };
        spawn_curtain(&mut commands, &grass, entity, curtain, rock.seed as usize);
    }
    for (entity, bush) in &new_bushes {
        // The bush frame is centred on the CANOPY, so its ground line is well
        // below `Pos` (`BUSH_CANOPY_Z * sin(TILT)` of the frame). A bush is
        // taller than it is wide, and grass only ever laps at its stems.
        let r = bush.r as f32;
        let curtain = Curtain { base: -r * 0.72, span: r * 2.4, height: r * 1.8, width: r * 1.7 };
        spawn_curtain(&mut commands, &grass, entity, curtain, bush.seed as usize);
    }
}

fn spawn_curtain(
    commands: &mut Commands,
    grass: &GrassAssets,
    owner: Entity,
    curtain: Curtain,
    variant: usize,
) {
    commands.spawn((
        CurtainShade,
        curtain,
        Sprite {
            image: grass.shade.clone(),
            color: SHADE_COLOR.with_alpha(0.0),
            custom_size: Some(Vec2::ZERO),
            ..default()
        },
        Anchor(Vec2::new(0.0, -0.5)),
        Transform::from_xyz(0.0, 0.0, Z_CURTAIN_SHADE),
        ChildOf(owner),
    ));
    commands.spawn((
        curtain,
        Sprite {
            image: grass.skirt.clone(),
            texture_atlas: Some(TextureAtlas {
                layout: grass.skirt_layout.clone(),
                index: variant % SKIRT_VARIANTS,
            }),
            custom_size: Some(Vec2::ZERO),
            flip_x: variant / SKIRT_VARIANTS % 2 == 1,
            ..default()
        },
        grass_anchor(),
        Transform::from_xyz(0.0, 0.0, Z_CURTAIN),
        ChildOf(owner),
    ));
}

/// Size and tint every curtain from the grass its owner is standing in.
///
/// Cover doesn't move, but pawns do — and cheaply enough (a handful of entities)
/// that there's no point splitting the static case out. Pawns also override the
/// stored profile from their stance: dropping prone changes how tall you are,
/// which is the entire point of the mechanic.
pub fn update_curtains(
    local_players: Option<Res<LocalPlayers>>,
    owners: Query<(&Pos, Option<&Stance>, Option<&Player>)>,
    mut curtains: Query<(
        &ChildOf,
        &Curtain,
        Has<CurtainShade>,
        &mut Sprite,
        &mut Transform,
    )>,
) {
    let local = local_players
        .as_deref()
        .and_then(|local| local.0.first().copied());

    for (parent, stored, is_shade, mut sprite, mut transform) in &mut curtains {
        let Ok((pos, stance, player)) = owners.get(parent.parent()) else { continue };
        // A pawn's profile comes from its stance, not from what it was spawned
        // with: dropping prone changes how tall you are, which is the entire
        // point. Cover keeps the profile it was measured with.
        let profile = stance
            .map(|s| STANCE_CURTAIN[(s.level as usize).min(STANCE_COUNT - 1)])
            .unwrap_or(*stored);
        let (x, y) = units(pos);
        let depth = grass_height(x, y) as f32;
        // Pawns take their coverage straight from the sim's rule rather than
        // recomputing it here, so what the grass hides can't drift from what the
        // sim says it hides. Cover isn't a pawn and has no stance, so it falls
        // back to the same ratio against its own measured height.
        let fraction = match stance {
            Some(s) => grass_cover(x, y, s.level) as f32 / FP as f32,
            None => depth / profile.height,
        };
        let cover = cover_px(depth, fraction, profile.span);
        // Yours is drawn weaker so you can still find yourself in deep grass;
        // everyone else's peer draws you at full strength.
        let mine = matches!((player, local), (Some(p), Some(handle)) if p.handle == handle);
        if is_shade {
            let strength = (depth / GRASS_MAX_H as f32).clamp(0.0, 1.0);
            let alpha = if mine { LOCAL_SHADE_ALPHA } else { 1.0 };
            sprite.custom_size = Some(Vec2::new(profile.width, cover * SHADE_REACH));
            sprite.color = SHADE_COLOR.with_alpha(SHADE_ALPHA * strength * alpha);
        } else {
            let (tint, _) = grass_look(depth);
            let alpha = if mine { LOCAL_CURTAIN_ALPHA } else { 1.0 };
            sprite.custom_size = Some(Vec2::new(profile.width, cover));
            sprite.color = tint.with_alpha(alpha);
        }
        transform.translation.y = profile.base;
    }
}
