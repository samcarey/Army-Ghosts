//! Grass: the sward everything stands in, and the thing it hides.
//!
//! The depth of the grass anywhere is [`Scenario::depth`] — a pure function in the
//! sim, so every peer agrees without a single entity being spawned or rolled
//! back. Everything here is the render half of that one number, in three layers:
//!
//!   * **The field.** One static mesh over the arena, textured with `grass.png`
//!     tiled in world space and tinted per vertex from the local depth: pale and
//!     patchy over thin ground (the dirt tile shows through the alpha), lush and
//!     opaque in the deep patches. Vertex interpolation across the grid is what
//!     makes area-to-area transitions smooth for free.
//!   * **Tufts.** Thousands of small clumps scattered on a fine jittered grid,
//!     each as tall as the grass is deep where it stands, baked into static
//!     meshes — one per horizontal band of the arena — and **y-sorted** against
//!     everything else standing on the ground.
//!   * **Shade.** The one thing that rides on a pawn: the gloom down among the
//!     blades, reaching as far up the body as the grass buries it.
//!
//! The y-sorting is the whole mechanic. Nothing hides a soldier except the
//! clumps that happen to stand between him and the camera, so what you get is
//! what the geometry says: walk north through a patch and the blades in front of
//! you drop behind you a clump at a time, uncovering you gradually; go prone and
//! the same field swallows you. Deeper grass hides more because it is taller and
//! thicker, not because anything is scaling a mask.
//!
//! It replaced a "curtain" — a band of blades parented to each pawn and sized
//! from [`army_ghosts_sim::grass_cover`]. It was cheap and it was wrong: the
//! grass moved with you, so you wore it rather than stood in it, and clumps north
//! of you covered your head while your boots stuck out below them. `grass_cover`
//! survives as the rule the shade follows.
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

use army_ghosts_sim::{
    Player, Pos, Scenario, Stance, ARENA_HALF_H, ARENA_HALF_W, FP, GRASS_MAX_H,
    STANCE_COUNT,
};

/// The projection foreshortening: one world unit of *height* draws this far up
/// the screen. It's `sin(40 deg)` — the same tilt the soldier, bush and grass
/// sheets are all modelled at (`tools/gen_assets.py`), which is why a blade, a
/// canopy and a rifle all lean together.
pub const GRASS_VERT: f32 = 0.6428;
/// Frame layout of `tufts.png` (see `gen_assets.py`): the ground line sits this
/// far up from the bottom edge, and a blade of model height 1.0 rises this
/// fraction of the frame.
const GRASS_BASE_FRAC: f32 = 0.15;
const GRASS_RISE_FRAC: f32 = 0.82;
/// Frame aspect, so a clump `h` px tall is drawn `h * TUFT_ASPECT` wide.
const TUFT_ASPECT: f32 = 28.0 / 48.0;

/// World px the detail texture repeats over. Small enough that blades are blade
/// sized; the shader crosses two octaves of it so the tiling doesn't read.
const GRASS_TEX_PX: f32 = 128.0;
/// Field mesh resolution, world units. The depth field's steepest gradient runs
/// about 2 units per unit walked, so 20 px between samples is well inside what
/// vertex interpolation can carry without banding.
const GRASS_GRID: i32 = 20;

/// Candidate grid for scattering clumps, world units. Fine, because the field
/// has to read as a *thatch* — thousands of small tufts you walk into one at a
/// time, not tussocks you step over.
///
/// It has to be finer than the narrowest tuft or short grass falls apart into
/// dots: a clump is drawn `TUFT_ASPECT` as wide as it is tall, so at the field's
/// floor (26 units, ~12 px wide after the projection) a 6-unit grid left visible
/// ground between neighbours however many were accepted. At 4 they overlap at
/// every depth in the game.
const TUFT_STEP: i32 = 4;
/// Depth below which the ground is bare — the sward texture is the grass there.
const TUFT_MIN_H: i32 = 6;
/// `tufts.png` layout: four plants, four frames each, species-major (see
/// `gen_assets.py` `_GRASS_SPECIES`) — meadow, tussock, bent, weed.
const TUFT_SPECIES: usize = 4;
const SPECIES_FRAMES: usize = 4;
const TUFT_FRAMES: usize = TUFT_SPECIES * SPECIES_FRAMES;

/// How much of each plant grows on lush, middling and parched ground, as raw
/// weights read off [`dryness`] and normalised.
///
/// A patch is DOMINATED rather than pure, which is the whole point of drawing
/// the species from a field instead of from the clump's own hash: even weights
/// everywhere would be confetti — four plants evenly mixed at every scale reads
/// as noise, not as ground that differs from the ground next to it. Dominance
/// is what makes an area a place.
///
/// **Three stops rather than two, because two only ever made two kinds of
/// place.** With a straight lush→dry lerp the greenest ground and the merely
/// average ground were both meadow-led — the tussock's weight sat under the
/// meadow's at every dryness, so it was a supporting plant everywhere and led
/// nothing (measured: 11 of 12 patches meadow, 1 bent, 0 tussock). Giving the
/// lush end to the tussock costs one number and buys a third kind of area:
/// dark dense cushions where the ground is best, open meadow in the middle,
/// fine seed-headed bent where it has burnt off.
const SPECIES_MIX: [[f32; 3]; TUFT_SPECIES] = [
    [0.30, 0.55, 0.20], // meadow
    [0.55, 0.22, 0.05], // tussock
    [0.05, 0.13, 0.58], // bent
    [0.10, 0.10, 0.17], // weed
];

/// How dry the ground is, area to area: two octaves of value noise, coarse
/// enough to make regions and fine enough that their edges are ragged.
const DRY_CELL: f32 = 190.0;
const DRY_CELL_FINE: f32 = 58.0;
const DRY_FINE_SHARE: f32 = 0.32;
/// The window the blend is stretched through. Value noise bunches hard around
/// its middle and blending two octaves narrows it further, so a window that
/// looks reasonable sits off where the numbers actually are — the same trap the
/// wind's `GUST_LO`/`GUST_HI` fell into twice. `the_ground_is_mostly_green_with_
/// dry_patches_here_and_there` prints the distribution; tune against that
/// rather than against these numbers looking sensible.
const DRY_LO: f32 = 0.53;
const DRY_HI: f32 = 0.96;
const DRY_SEED: u32 = 0x5EED_1A17;
const DRY_SEED_FINE: u32 = 0x9C4F_20B3;
/// What full dryness does to the tint, as an rgb offset. It is a HUE shift and
/// almost not a value one — red up, blue down, green left alone — so that
/// depth keeps the light-to-dark axis to itself (see [`dryness`]). Straw is not
/// brighter than grass; it is yellower.
///
/// **Deliberately smaller than the plants' own contribution.** The first
/// version leaned twice as hard on red here, on top of a dry plant that was
/// already tan, and the two multiplied into rust: the dry areas read as an
/// autumn wood rather than as a field that had gone over. What says "dry" is
/// mostly the bent plant's silhouette and palette; this only tips the ground it
/// stands on far enough to agree with it.
const DRY_SHIFT: [f32; 3] = [0.14, -0.01, -0.10];
/// How much of the sward's cover full dryness takes away, letting the dry earth
/// underneath show through between the blades. Small: this is grass thinning
/// out, not bare ground, and the tufts standing in it are unaffected — a dry
/// patch you could see the soil through everywhere would be a patch nobody
/// could hide in, which is the one thing the brief said not to do.
const DRY_THIN: f32 = 0.20;
/// What the measuring rig gets. `Scenario::GrassStrip` exists to be
/// photographed and compared against the last set of photographs, so its ground
/// is one fixed shade for the same reason its wind is a dead calm.
const DRY_RIG: f32 = 0.25;

/// The ground itself, under everything (the dirt tile is at -10).
const Z_FIELD: f32 = -9.0;
/// The z band that y-sorting maps the arena into. Everything standing on the
/// ground — pawns, boulders, targets and every band of grass — gets a z in here
/// from its ground line, so what's drawn over what falls out of who is nearer the
/// camera. Bullets (2.0), their trails (1.9) and bush canopies (2.5) sit above
/// the whole band on purpose.
pub const Z_SORT_LO: f32 = 0.1;
pub const Z_SORT_HI: f32 = 1.8;
/// How thick a slice of the arena shares one grass mesh, world units. Grass is
/// baked per band rather than per clump: a mesh has ONE sort key, so this is the
/// resolution of the y-sorting, and the only place a pawn can be slotted into
/// the grass is BETWEEN two bands.
///
/// So a band's worth of grass north of you draws over you regardless, with its
/// blades rooted that far up your body — and at the 12 units this used to be,
/// that is a blade growing out of your knee with your boots showing underneath
/// it. It was invisible when the field was sparse ankle-high scatter and became
/// obvious the moment it was a thatch. At 4 (a sixth of a pawn's width, about a
/// boot) it reads as a blade leaning across you.
///
/// The cost is one mesh and one draw call per band — 150 rather than 50 — which
/// is the trade: this is the number to raise first if the grass ever costs too
/// much on a phone, and the artefact to look for after raising it.
const GRASS_BAND: i32 = 4;

/// The shade rides a hair in front of its pawn — enough to beat the sprite it
/// darkens, small enough to stay inside that pawn's y-sorted slot.
const Z_SHADE_LIFT: f32 = 0.01;

/// Grass shade. Not black — a shadow among green blades is still green, and
/// pure black over a soldier reads as a hole in the sprite. Weak, too: the
/// blades in front of you are what actually hide you, and this is only the
/// gloom down among them.
const SHADE_COLOR: Color = Color::srgb(0.10, 0.14, 0.07);
const SHADE_ALPHA: f32 = 0.30;

/// Where a pawn meets the grass, per stance: `base` is its ground line relative
/// to `Pos` (negative = the sprite hangs below) and `span` how tall it draws
/// above that line. Measured off `soldier.png` bounding boxes (frame px x
/// `SOLDIER_SIZE / SOLDIER_FRAME`); prone hangs below `Pos` because that sprite
/// is anchored mid-body, and is the widest because a soldier lying side-on is as
/// long as a standing one is tall.
/// (`base` follows `render::STANCE_ANCHOR`: when the anchor moved to the boots
/// the upright figures rose 4.3 / 5.9 px against `Pos`, and their ground lines
/// came up with them — an upright pawn's gloom now starts essentially at its
/// feet, which is the point.)
const STANCE_SHADE: [ShadeProfile; STANCE_COUNT] = [
    ShadeProfile { base: -0.8, span: 43.8, width: 40.0 },
    ShadeProfile { base: -0.7, span: 38.2, width: 40.0 },
    ShadeProfile { base: -17.2, span: 38.3, width: 54.0 },
];

/// How a pawn sits in the grass, for the purpose of shading it.
#[derive(Component, Copy, Clone)]
pub struct ShadeProfile {
    base: f32,
    span: f32,
    width: f32,
}

/// The one sheet drawn per-entity (the grass meshes bake theirs in at startup).
#[derive(Resource)]
pub struct GrassAssets {
    shade: Handle<Image>,
}

/// Textured, vertex-colored, unlit — see `assets/grass.wgsl` for why this isn't
/// `ColorMaterial` (the same vertex-color specialization trap as `FogMaterial`).
#[derive(Asset, TypePath, AsBindGroup, Clone)]
pub struct GrassMaterial {
    /// `(octave cross, sward ripple in uv, seconds, spare)`.
    ///
    /// `x` is how much of a second, finer octave of the texture to cross in and
    /// `y` how far that texture slides with the wind — only the tiled ground
    /// wants either, because the tuft sheet's UVs point into an ATLAS and
    /// moving them samples the neighbouring frame. `z` is the clock, written
    /// every frame by [`crate::wind::drive_grass`], and it is the only thing
    /// the CPU tells the wind: everything else about it is a pure function of
    /// world position, so thousands of baked quads lean without a byte of
    /// geometry being re-uploaded.
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
            // How this vertex takes the wind: px at full gust, and the clump's
            // phase. Both meshes must carry it — a layout is per pipeline and
            // both share this material — so the field's is all zeroes.
            Mesh::ATTRIBUTE_UV_1.at_shader_location(3),
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

/// Draw depth for something whose ground line is at world `y`.
///
/// This is the whole trick behind grass that behaves: the arena's y axis maps
/// onto a z band, south (nearer the camera) getting the higher z, so the painter
/// sorts everything standing on the ground by how close it is to the viewer. A
/// clump of grass south of a soldier is drawn after him and swallows his legs; a
/// clump the same height two feet north of him is drawn before him and doesn't
/// touch him. Nothing is attached to anybody: the grass just sits on the map and
/// he walks into it and out of it.
pub fn y_sort(y: f32) -> f32 {
    let f = ((ARENA_HALF_H as f32 - y) / (2.0 * ARENA_HALF_H as f32)).clamp(0.0, 1.0);
    Z_SORT_LO + (Z_SORT_HI - Z_SORT_LO) * f
}

/// Where a pawn stands, in the whole world units the grass field is sampled at.
fn units(pos: &Pos) -> (i32, i32) {
    (pos.x / FP, pos.y / FP)
}

/// Smooth interpolation between two lattice corners, sampled off [`scatter`] so
/// the field costs nothing to store and every peer grows the same one.
fn value_noise(x: f32, y: f32, cell: f32, salt: u32) -> f32 {
    let (gx, gy) = (x / cell, y / cell);
    let (ix, iy) = (gx.floor(), gy.floor());
    let (fx, fy) = (gx - ix, gy - iy);
    let (ux, uy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
    let corner = |cx: f32, cy: f32| (scatter(cx as i32, cy as i32, salt) & 0xFFFF) as f32 / 65535.0;
    let top = corner(ix, iy) + (corner(ix + 1.0, iy) - corner(ix, iy)) * ux;
    let bottom = corner(ix, iy + 1.0) + (corner(ix + 1.0, iy + 1.0) - corner(ix, iy + 1.0)) * ux;
    top + (bottom - top) * uy
}

/// How dry the ground is here: 0 lush green, 1 straw. **Render-only, and
/// deliberately UNCORRELATED with the depth field.**
///
/// Both of those are load-bearing.
///
/// *Render-only*, because dryness changes nothing about what can be seen —
/// concealment, `visible_fraction`, the fog tiles and every bot decision still
/// read `Scenario::depth` and nothing else. Grass that hid you by being brown
/// would have to be sim state: integers, rollback registration, a checksum, all
/// for a colour.
///
/// *Uncorrelated*, because the alternative is a second readout of the same
/// number. Depth already drives the tint's VALUE — dark is deep — and painting
/// dryness on the same axis would just make deep tiles doubly dark and say
/// nothing new. So depth keeps value and dryness takes HUE: a dry deep patch is
/// dark khaki, a lush thin one is pale green, and the two facts stay separately
/// readable. That the field crosses the depth field's hex tiles at its own scale
/// is the other half of it — smooth colour over hard-edged tiles is what stops
/// the honeycomb reading as a honeycomb.
fn dryness(scenario: Scenario, x: i32, y: i32) -> f32 {
    if matches!(scenario, Scenario::GrassStrip { .. }) {
        return DRY_RIG;
    }
    let t = ((parch(x, y) - DRY_LO) / (DRY_HI - DRY_LO)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The raw two-octave blend, before the window. Split out because the window is
/// the part that has to be tuned against measured quantiles rather than guessed
/// (see [`DRY_LO`]), and a number you cannot print is a number you will guess.
fn parch(x: i32, y: i32) -> f32 {
    let (x, y) = (x as f32, y as f32);
    (1.0 - DRY_FINE_SHARE) * value_noise(x, y, DRY_CELL, DRY_SEED)
        + DRY_FINE_SHARE * value_noise(x, y, DRY_CELL_FINE, DRY_SEED_FINE)
}

/// Which of the four plants grows at a spot, from how dry it is there plus the
/// spot's own roll.
fn species(dry: f32, roll: u32) -> usize {
    let mut weights = [0.0_f32; TUFT_SPECIES];
    let mut total = 0.0;
    // Which half of the curve, and how far along it.
    let (stop, t) = if dry < 0.5 { (0, dry * 2.0) } else { (1, (dry - 0.5) * 2.0) };
    for (i, stops) in SPECIES_MIX.iter().enumerate() {
        weights[i] = stops[stop] + (stops[stop + 1] - stops[stop]) * t;
        total += weights[i];
    }
    let mut pick = (roll & 0xFFFF) as f32 / 65535.0 * total;
    for (i, w) in weights.iter().enumerate() {
        pick -= w;
        if pick <= 0.0 {
            return i;
        }
    }
    TUFT_SPECIES - 1
}

/// The look of grass `h` units deep on ground `dry` parched: a near-white tint
/// over the sheets' own greens (so hue and value drift from area to area
/// without repainting anything), and how completely it covers the dirt
/// underneath. Thin ground is pale and see-through; deep grass is greener,
/// darker and solid.
fn grass_look(h: f32, dry: f32) -> (Color, f32) {
    let f = (h / GRASS_MAX_H as f32).clamp(0.0, 1.0);
    let lerp = |thin: f32, deep: f32| thin + (deep - thin) * f;
    // A WIDE spread, pale thin green to dark lush green, because this is what
    // actually makes one tile read as deeper than the next. Seen from almost
    // straight down, a third more blade height barely registers — the
    // silhouettes overlap into the same mass either way — so depth has to carry
    // in value and hue as well, the way it does on any top-down map.
    // Kept out of the bright end regardless: `TEAM_COLORS` were picked to sit
    // above the ground in value, and a vivid sward puts a camouflaged soldier
    // back into the background it was tuned against.
    let base = [lerp(0.90, 0.46), lerp(0.85, 0.68), lerp(0.60, 0.42)];
    let tint: [f32; 3] = std::array::from_fn(|c| (base[c] + DRY_SHIFT[c] * dry).clamp(0.0, 1.0));
    (
        Color::srgb(tint[0], tint[1], tint[2]),
        // Saturating fast, not linear: grass covers the soil long before it gets
        // tall, so anything from ankle deep up is solid sward with no dirt
        // showing through — the ground layer is the thatch the tufts stand in,
        // and if it fades with depth then short grass reads as bare earth with
        // clumps on it. Only genuinely bare ground (`Scenario::GrassStrip` uses
        // depth 0 for its clear lanes) shows soil.
        ((0.12 + 3.0 * f) * (1.0 - DRY_THIN * dry)).min(1.0),
    )
}

/// Chance out of 255 that a candidate spot grows a clump, from the depth there.
///
/// Nearly flat, and that is the point: **depth sets how TALL the grass is, not
/// whether there is any.** Short grass is still a complete carpet, just a short
/// one — a lawn is not a sparse meadow. Every version of this that ramped with
/// depth (`0.05 + 0.50 f^2` when the map was meant to be part bare, then
/// `0.10 + 0.75 f`) produced the same complaint: the shallow end reads as
/// scattered clumps with ground showing between them, because thinning the
/// count and narrowing the sprites compound.
///
/// What little ramp is left only stops the deepest grass from looking sparser
/// than the rest once its taller blades start hiding each other.
fn tuft_density(h: i32) -> u32 {
    let f = (h as f32 / GRASS_MAX_H as f32).clamp(0.0, 1.0);
    (255.0 * (0.80 + 0.20 * f)) as u32
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
    mut materials: ResMut<Assets<GrassMaterial>>,
    scenario: Res<Scenario>,
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
        Mesh2d(meshes.add(field_mesh(*scenario))),
        MeshMaterial2d(materials.add(GrassMaterial {
            params: Vec4::new(0.4, crate::wind::SWARD_RIPPLE_PX / GRASS_TEX_PX, 0.0, 0.0),
            texture,
        })),
        Transform::from_xyz(0.0, 0.0, Z_FIELD),
    ));
    // One mesh per band, all sharing a material: a mesh has a single sort key,
    // so a band is the unit of y-sorting.
    let tufts = materials.add(GrassMaterial {
        // Atlas UVs: neither crossing octaves nor sliding them with the wind is
        // available here — both would sample the neighbouring frame. A clump
        // leans by having its geometry moved instead (`ATTRIBUTE_UV_1` below).
        params: Vec4::ZERO,
        texture: assets.load("tufts.png"),
    });
    let mut clumps = 0;
    for (z, mesh, count) in tuft_bands(*scenario) {
        clumps += count;
        commands.spawn((
            Mesh2d(meshes.add(mesh)),
            MeshMaterial2d(tufts.clone()),
            Transform::from_xyz(0.0, 0.0, z),
        ));
    }
    info!("grass: {clumps} clumps in {} bands", 2 * ARENA_HALF_H / GRASS_BAND);

    commands.insert_resource(GrassAssets { shade: assets.load("shade.png") });
}

/// The arena-wide sward mesh: a grid of quads, colored per vertex from the
/// depth field.
fn field_mesh(scenario: Scenario) -> Mesh {
    let (mut positions, mut uvs, mut colors, mut indices) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    // The sward is a flat sheet with nothing to bend, so it takes no geometric
    // wind at all — it ripples in its texture instead. The attribute is here
    // because the vertex layout is shared with the tufts, not because the
    // ground moves.
    let mut sway: Vec<[f32; 2]> = Vec::new();
    let cols = (ARENA_HALF_W * 2 / GRASS_GRID) as u32;
    let rows = (ARENA_HALF_H * 2 / GRASS_GRID) as u32;
    for row in 0..=rows {
        for col in 0..=cols {
            let x = -ARENA_HALF_W + col as i32 * GRASS_GRID;
            let y = -ARENA_HALF_H + row as i32 * GRASS_GRID;
            let (tint, alpha) =
                grass_look(scenario.depth(x, y) as f32, dryness(scenario, x, y));
            let tint = tint.to_linear();
            positions.push([x as f32, y as f32, 0.0]);
            uvs.push([x as f32 / GRASS_TEX_PX, -y as f32 / GRASS_TEX_PX]);
            sway.push([0.0, 0.0]);
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
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_1, sway);
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colors);
    mesh.insert_indices(Indices::U32(indices));
    mesh
}

/// Every clump in the arena, baked into one mesh per `GRASS_BAND`-thick slice:
/// `(draw z, mesh, clump count)`, southernmost band last.
///
/// Two decisions live here.
///
/// *Meshes, not sprites.* Nothing about a tuft ever changes, so thousands of
/// them as entities would be thousands of extractions, sorts and batches every
/// frame on a phone to draw exactly what a mesh built once draws. The cost is
/// that a mesh has a single sort key — hence bands.
///
/// *Bands, not one mesh.* A single mesh would have to be drawn either wholly
/// before or wholly after each soldier, which is what makes grass look painted
/// onto people. Slicing the arena by y and giving each slice the z of its
/// SOUTHERN edge means the grass between you and the camera is drawn after you
/// and the grass behind you before you — no matter where anyone stands, and with
/// nothing parented to anyone.
fn tuft_bands(scenario: Scenario) -> Vec<(f32, Mesh, usize)> {
    let bands = (2 * ARENA_HALF_H / GRASS_BAND) as usize;
    let mut binned: Vec<Vec<(i32, i32, u32)>> = vec![Vec::new(); bands];
    // Boulders displace the sward. Without this, clumps grow inside the rock's
    // own footprint and — since they stand SOUTH of its ground line — draw over
    // it, which reads as grass growing out of solid stone. `Grounded`'s reach in
    // `render.rs` is the other half of the same fix.
    let rocks = scenario.rocks();
    let mut consider = |x: i32, y: i32| {
        let noise = scatter(x, y, 0x51DE);
        // Jitter off the grid, or the clumps line up in rows the eye finds
        // immediately.
        let jx = x + (noise % TUFT_STEP as u32) as i32 - TUFT_STEP / 2;
        let jy = y + (noise / 64 % TUFT_STEP as u32) as i32 - TUFT_STEP / 2;
        if jx.abs() > ARENA_HALF_W || jy.abs() > ARENA_HALF_H {
            return;
        }
        let h = scenario.depth(jx, jy);
        if h < TUFT_MIN_H || (noise >> 16 & 0xFF) > tuft_density(h) {
            return;
        }
        // Just inside the rim rather than clear of it: a hard ring of bare
        // ground around every boulder would be as obvious as the clumps were.
        if rocks.iter().any(|&(rx, ry, rock)| {
            let (dx, dy) = ((jx - rx) as i64, (jy - ry) as i64);
            dx * dx + dy * dy < ((rock.r - 2).max(0) as i64).pow(2)
        }) {
            return;
        }
        let band = ((ARENA_HALF_H - jy) / GRASS_BAND).clamp(0, bands as i32 - 1) as usize;
        binned[band].push((jx, jy, noise));
    };
    let mut y = -ARENA_HALF_H;
    while y <= ARENA_HALF_H {
        let mut x = -ARENA_HALF_W;
        while x <= ARENA_HALF_W {
            consider(x, y);
            x += TUFT_STEP;
        }
        y += TUFT_STEP;
    }

    let mut out = Vec::with_capacity(bands);
    for (band, mut clumps) in binned.into_iter().enumerate() {
        // Within a band the painter still has to run north to south, so a near
        // clump covers the far one it overlaps.
        clumps.sort_by_key(|&(x, y, _)| (std::cmp::Reverse(y), x));
        let count = clumps.len();
        let (mut positions, mut uvs, mut colors, mut indices) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut sway: Vec<[f32; 2]> = Vec::new();
        for (x, y, noise) in clumps {
            let h = scenario.depth(x, y) as f32;
            // Which PLANT grows here comes from the ground (`dryness`); which of
            // its four frames is this clump's own business. Rolled off a
            // separate salt rather than more bits of `noise`, which is already
            // spoken for four times over — two draws that shared bits would
            // correlate the plant with the clump's height or its mirroring, and
            // that reads as a pattern long before anyone works out what it is.
            let dry = dryness(scenario, x, y);
            let frame = species(dry, scatter(x, y, 0xB0A5)) * SPECIES_FRAMES
                + (noise / 128) as usize % SPECIES_FRAMES;
            // Clumps aren't all the same size even where the grass is level —
            // but only just. This jitter competes directly with the tile-to-tile
            // depth difference the field is quantized to produce, and at the
            // ±22% it used to be it drowned it: neighbouring tiles read as one
            // noisy sward instead of two swards of different depth.
            let height = sprite_height(h) * (0.90 + (noise / 4096 % 21) as f32 * 0.01);
            let (half_w, x, y) = (height * TUFT_ASPECT * 0.5, x as f32, y as f32);
            // The frame's ground line is `GRASS_BASE_FRAC` up from its bottom
            // edge, so the quad hangs that much below the clump's own position.
            let (bottom, top) =
                (y - height * GRASS_BASE_FRAC, y + height * (1.0 - GRASS_BASE_FRAC));
            let (u0, u1) = {
                let (a, b) = (frame as f32 / TUFT_FRAMES as f32,
                              (frame + 1) as f32 / TUFT_FRAMES as f32);
                if noise / 32 % 2 == 0 { (b, a) } else { (a, b) } // mirror = swapped u
            };
            let (tint, _) = grass_look(h, dry);
            let tint = tint.to_linear();
            // How this clump takes the wind, baked in once: how far its TIP
            // travels at a full gust (its root does not move, which is what
            // makes it bend instead of slide), and its own grain — a small
            // offset into the fine term, so a stand of grass dapples instead of
            // moving as one piece at the finest scale. It costs nothing here,
            // because the clump already has a hash.
            let tip = crate::wind::TUFT_SWAY_FRAC * height;
            let grain = ((noise / 1_048_576 % 1024) as f32 / 1024.0 - 0.5)
                * 2.0
                * crate::wind::TUFT_GRAIN;
            let base = positions.len() as u32;
            for (px, py, u, v, lean) in [
                (x - half_w, top, u0, 0.0, tip),
                (x + half_w, top, u1, 0.0, tip),
                (x + half_w, bottom, u1, 1.0, 0.0),
                (x - half_w, bottom, u0, 1.0, 0.0),
            ] {
                positions.push([px, py, 0.0]);
                uvs.push([u, v]);
                sway.push([lean, grain]);
                colors.push([tint.red, tint.green, tint.blue, 1.0]);
            }
            indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
        }
        let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default());
        mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
        mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uvs);
        mesh.insert_attribute(Mesh::ATTRIBUTE_UV_1, sway);
        mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colors);
        mesh.insert_indices(Indices::U32(indices));
        // The band sorts as if it all stood on its MIDDLE line. Its southern
        // edge is the intuitive choice — "anything north of this band is behind
        // every blade in it" — but it puts the whole quantization error on one
        // side: every blade in the band then draws over a pawn standing anywhere
        // in it, including the ones rooted a full band NORTH of that pawn's
        // feet, which is a blade visibly sprouting above the bottom of its boot.
        // Sorting on the middle splits the error either way and halves it: at
        // most half a band of blades sprout above the boot, and at most half a
        // band of blades in front of it get drawn behind it instead. Neither is
        // visible at `GRASS_BAND` 4; the first one was at 4 and glaring at 12.
        let middle = (ARENA_HALF_H - band as i32 * GRASS_BAND) as f32 - GRASS_BAND as f32 / 2.0;
        out.push((y_sort(middle), mesh, count));
    }
    out
}

/// Give every pawn the shadow the grass throws up its front. Runs on
/// `Added<Player>` like `attach_sprites`, so a pawn respawned by a session
/// restart gets one too.
///
/// Only pawns: boulders and bushes standing in the grass are covered by the
/// blades in front of them like everything else, and a boulder that also carried
/// its own gloom just read as a hole in the ground.
pub fn attach_grass_shade(
    mut commands: Commands,
    grass: Option<Res<GrassAssets>>,
    new_players: Query<Entity, Added<Player>>,
) {
    let Some(grass) = grass else { return };
    for entity in &new_players {
        commands.spawn((
            STANCE_SHADE[0], // sized every frame below; the stance changes it
            Sprite {
                image: grass.shade.clone(),
                color: SHADE_COLOR.with_alpha(0.0),
                custom_size: Some(Vec2::ZERO),
                ..default()
            },
            Anchor(Vec2::new(0.0, -0.5)),
            Transform::from_xyz(0.0, 0.0, Z_SHADE_LIFT),
            ChildOf(entity),
        ));
    }
}

/// Size each pawn's shade from the grass it is standing in.
///
/// This is the one thing that rides on a pawn rather than on the map, and it is
/// the only one that should: the blades are the map's, but the gloom down among
/// them belongs to whoever is standing there. It reaches exactly as far up the
/// body as the sim says the grass buries it — `grass_cover`, depth over
/// `STANCE_HEIGHT` — so dropping prone darkens all of you and standing in the
/// same spot darkens your boots.
pub fn update_grass_shade(
    owners: Query<(&Pos, &Stance)>,
    mut shades: Query<(&ChildOf, &mut ShadeProfile, &mut Sprite, &mut Transform)>,
    scenario: Res<Scenario>,
) {
    for (parent, mut profile, mut sprite, mut transform) in &mut shades {
        let Ok((pos, stance)) = owners.get(parent.parent()) else { continue };
        *profile = STANCE_SHADE[(stance.level as usize).min(STANCE_COUNT - 1)];
        let (x, y) = units(pos);
        let buried = scenario.cover(x, y, stance.level) as f32 / FP as f32;
        sprite.custom_size = Some(Vec2::new(profile.width, buried * profile.span));
        sprite.color = SHADE_COLOR.with_alpha(SHADE_ALPHA * buried);
        transform.translation.y = profile.base;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::mesh::VertexAttributeValues;

    fn wind_attribute(mesh: &Mesh) -> Vec<[f32; 2]> {
        match mesh.attribute(Mesh::ATTRIBUTE_UV_1) {
            Some(VertexAttributeValues::Float32x2(values)) => values.clone(),
            _ => panic!("this mesh carries no wind attribute"),
        }
    }

    /// **A clump BENDS.** Its tip takes the wind and its root is pinned, which
    /// is the difference between grass leaning and grass sliding across the
    /// ground — and it is decided here, at bake time, because the tufts are a
    /// static mesh and there is no frame in which this could be checked by
    /// looking. Getting it the other way round (every vertex leaning equally)
    /// would have the whole field skate downwind and back sixty times a second.
    #[test]
    fn a_clump_takes_the_wind_at_its_tip_and_not_at_its_root() {
        let (mut tips, mut grains) = (Vec::new(), Vec::new());
        for (_, mesh, count) in tuft_bands(Scenario::Arena) {
            let sway = wind_attribute(&mesh);
            assert_eq!(sway.len(), count * 4, "a clump is four vertices");
            // Vertex order per clump: top-left, top-right, bottom-right,
            // bottom-left (see the quad wound in `tuft_bands`).
            for quad in sway.chunks(4) {
                assert!(quad[0][0] > 0.0 && quad[1][0] > 0.0, "a clump's tip is pinned: {quad:?}");
                assert!(quad[2][0] == 0.0 && quad[3][0] == 0.0, "a clump's root moves: {quad:?}");
                // One grain for the whole clump. Per-vertex, it would shear as
                // well as bend, which is a clump changing width in the wind.
                assert!(quad.iter().all(|v| v[1] == quad[0][1]), "a clump is out of step with itself");
                tips.push(quad[0][0]);
                grains.push(quad[0][1]);
            }
        }
        let (lo, hi) = (
            tips.iter().cloned().fold(f32::MAX, f32::min),
            tips.iter().cloned().fold(0.0_f32, f32::max),
        );
        println!("{} clumps: tips travel {lo:.1}..{hi:.1} px at a full gust", tips.len());
        // Deep grass leans further than short grass, because the lean is a
        // fraction of the clump's own height rather than a number of pixels.
        assert!(hi > lo * 2.0, "every clump leans the same distance: {lo}..{hi}");
        // The deepest clump on the map draws about 62 px tall, and a tip that
        // travelled much past half of that would read as the clump being
        // dragged rather than bent — `wind::TUFT_SWAY_FRAC`'s `const` block is
        // the other half of this guard, and catches it at build time.
        assert!(hi < 31.0, "a clump leans further than it stands: {hi}");

        // …and neighbours are not in lockstep, which is what makes a patch
        // dapple instead of tilting as one sheet.
        let spread = grains.iter().cloned().fold(f32::MIN, f32::max)
            - grains.iter().cloned().fold(f32::MAX, f32::min);
        assert!(
            spread > crate::wind::TUFT_GRAIN,
            "the whole field moves as one: grain spread {spread}"
        );
    }

    /// Sample the dryness field on the same grid the sward mesh is built on.
    fn arena_dryness() -> Vec<f32> {
        let mut out = Vec::new();
        let mut y = -ARENA_HALF_H;
        while y <= ARENA_HALF_H {
            let mut x = -ARENA_HALF_W;
            while x <= ARENA_HALF_W {
                out.push(dryness(Scenario::Arena, x, y));
                x += GRASS_GRID;
            }
            y += GRASS_GRID;
        }
        out
    }

    /// **Brown here and there, not brown all over.** The whole request this
    /// field answers was for the ground to stop being one picturesque green,
    /// and the failure mode on the other side is a field that is uniformly
    /// mottled — which is just as flat, only browner, and costs the concealment
    /// model the lush ground it is tuned against.
    ///
    /// So this prints the distribution and asserts the SHAPE of it: a green
    /// majority, a real dry minority, and both ends actually reached somewhere.
    /// Value noise bunches around its middle and blending two octaves narrows it
    /// further, so the window that produces this is not one anybody guessed —
    /// print before tuning.
    #[test]
    fn the_ground_is_mostly_green_with_dry_patches_here_and_there() {
        let field = arena_dryness();
        let share = |lo: f32, hi: f32| {
            field.iter().filter(|&&d| d >= lo && d < hi).count() as f32 / field.len() as f32
        };
        let (green, turning, dry) = (share(0.0, 0.25), share(0.25, 0.6), share(0.6, 1.01));
        // The window is tuned against these, not against what looks sensible.
        let mut raw = Vec::new();
        let mut y = -ARENA_HALF_H;
        while y <= ARENA_HALF_H {
            let mut x = -ARENA_HALF_W;
            while x <= ARENA_HALF_W {
                raw.push(parch(x, y));
                x += GRASS_GRID;
            }
            y += GRASS_GRID;
        }
        raw.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let q = |p: f32| raw[(p * (raw.len() - 1) as f32) as usize];
        println!(
            "blend quantiles: p05 {:.3} p25 {:.3} p50 {:.3} p75 {:.3} p95 {:.3} (window {DRY_LO}..{DRY_HI})",
            q(0.05), q(0.25), q(0.50), q(0.75), q(0.95),
        );
        println!(
            "{} samples: {:.0}% green, {:.0}% turning, {:.0}% dry (min {:.2} max {:.2})",
            field.len(),
            green * 100.0,
            turning * 100.0,
            dry * 100.0,
            field.iter().cloned().fold(f32::MAX, f32::min),
            field.iter().cloned().fold(0.0_f32, f32::max),
        );
        assert!(green > 0.45, "the field has gone brown: only {green:.2} of it is green");
        assert!(dry > 0.08, "there is nowhere dry: {dry:.2}");
        assert!(dry < 0.30, "dry ground is the exception, not the rule: {dry:.2}");
        // Both ends have to be REACHED, not merely approached: a field that only
        // ever ran 0.2..0.7 would tint the whole map and never say "this patch,
        // not that one".
        assert!(field.iter().cloned().fold(f32::MAX, f32::min) < 0.02, "nowhere is fully lush");
        assert!(field.iter().cloned().fold(0.0_f32, f32::max) > 0.98, "nowhere is fully dry");
    }

    /// A patch is dominated by one plant and is never pure. Both halves matter:
    /// an even mix everywhere is confetti (four plants at every scale reads as
    /// noise rather than as one place differing from the next), and a pure patch
    /// is a tiled texture with a visible seam where it meets the next one.
    #[test]
    fn a_patch_is_dominated_by_one_plant_but_never_only_one() {
        // Half the coarse dryness cell, which is about what fits on a phone
        // screen at once — the scale at which "this patch differs from that
        // one" is a thing a player can actually see. A whole cell is too coarse
        // to measure leadership with: dry ground is 17% of the map, so at that
        // size every block averages some of it in and the dry plant leads
        // nothing while plainly taking over on screen.
        let block = DRY_CELL as i32 / 2;
        let (mut leaders, mut pure) = (vec![0_usize; TUFT_SPECIES], 0);
        let mut blocks = 0;
        let mut y = -ARENA_HALF_H;
        while y + block <= ARENA_HALF_H {
            let mut x = -ARENA_HALF_W;
            while x + block <= ARENA_HALF_W {
                let mut counts = [0_usize; TUFT_SPECIES];
                for sy in (y..y + block).step_by(TUFT_STEP as usize) {
                    for sx in (x..x + block).step_by(TUFT_STEP as usize) {
                        let dry = dryness(Scenario::Arena, sx, sy);
                        counts[species(dry, scatter(sx, sy, 0xB0A5))] += 1;
                    }
                }
                let total: usize = counts.iter().sum();
                let best = counts.iter().max().unwrap();
                assert!(
                    *best as f32 / total as f32 > 0.30,
                    "no plant leads this patch: {counts:?}"
                );
                if counts.contains(&0) {
                    pure += 1;
                }
                leaders[counts.iter().position(|c| c == best).unwrap()] += 1;
                blocks += 1;
                x += block;
            }
            y += block;
        }
        println!("{blocks} patches, led by {leaders:?} (meadow, tussock, bent, weed)");
        assert_eq!(pure, 0, "{pure} patches grow only some of the plants");
        // The dry plant has to actually take over somewhere, or the sheet's most
        // distinctive frames are scattered thinly and never read as a place.
        assert!(leaders[2] > 0, "the dry plant never leads a patch: {leaders:?}");
        // …and the green ones still hold the field.
        assert!(
            leaders[0] + leaders[1] > blocks / 2,
            "the field is more dry than green: {leaders:?}"
        );
    }

    /// Dryness paints HUE, depth paints VALUE, and they have to stay on their
    /// own axes — otherwise a dry deep patch reads as shallow and the tint stops
    /// being a usable answer to "can I hide there?".
    #[test]
    fn drying_the_ground_changes_its_colour_without_changing_how_deep_it_looks() {
        let luma = |c: Color| {
            let c = c.to_linear();
            0.2126 * c.red + 0.7152 * c.green + 0.0722 * c.blue
        };
        // What depth is worth on the value axis, which is the yardstick the
        // dryness shift has to stay well under. Stated as a RATIO rather than
        // an absolute tolerance because the two are only ever read against each
        // other: the question is never "is this shift small" but "could anyone
        // mistake it for depth".
        let depth_swing = luma(grass_look(0.0, 0.0).0) - luma(grass_look(GRASS_MAX_H as f32, 0.0).0);
        for &h in &[10.0_f32, 30.0, 50.0, GRASS_MAX_H as f32] {
            let (green, _) = grass_look(h, 0.0);
            let (straw, _) = grass_look(h, 1.0);
            let (dl, dr) = (luma(straw) - luma(green), straw.to_srgba().red - green.to_srgba().red);
            println!(
                "depth {h:>5.0}: drying moves luma {dl:+.3} ({:.0}% of what depth moves it), red {dr:+.3}",
                100.0 * dl.abs() / depth_swing,
            );
            assert!(
                dl.abs() < depth_swing / 4.0,
                "drying changed how deep the grass looks: {dl:+.3} against depth's {depth_swing:.3}"
            );
            assert!(dr > 0.10, "drying barely changed the colour: {dr:+.3}");
        }
        // And depth still owns value at either extreme of dryness.
        for dry in [0.0_f32, 1.0] {
            let thin = luma(grass_look(0.0, dry).0);
            let deep = luma(grass_look(GRASS_MAX_H as f32, dry).0);
            assert!(thin - deep > 0.15, "depth stopped reading at dryness {dry}: {thin} {deep}");
        }
    }

    /// The sward is a flat sheet with nothing to bend, so it takes no geometric
    /// wind at all — it ripples in its own texture instead. It carries the
    /// attribute only because the vertex layout is shared with the tufts, and a
    /// non-zero one here would be the GROUND sliding under everybody's feet.
    #[test]
    fn the_ground_itself_never_moves() {
        for value in wind_attribute(&field_mesh(Scenario::Arena)) {
            assert_eq!(value, [0.0, 0.0], "the ground took the wind");
        }
    }
}
