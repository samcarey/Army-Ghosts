//! Line of sight: everything the local player can't see is covered up.
//!
//! Purely render-side — the sim has no notion of who can see what (it can't:
//! every peer simulates every pawn).
//!
//! Two halves, and keeping them apart is what makes this tractable:
//!
//! **The rays are continuous.** Every piece of cover resolves into a [`Cast`] —
//! a shadow cone swept from the viewer — and grass, which is a depth field
//! rather than a set of casters, gets an elevation ray test instead
//! ([`grass_conceal`]). Both answer the same question about any point on the
//! map: how much of someone standing THERE could you see from HERE.
//!
//! **The display is quantized.** The arena is a grid of flat-top hexes, and each
//! one integrates that answer over its own area ([`HEX_PROBES`] points) and
//! paints it across the tile: flat over the middle ([`TILE_PLATEAU`]), then a rim
//! that blends to a value shared with the neighbours meeting at each corner, so
//! the grid reads as tiles without hard-edged polygons. What's hidden is a
//! *place* — this hex, not that one — rather than a smear whose edge you can't
//! locate. Tiles also ease toward their target over [`TILE_EASE`] rather than
//! snapping, so cover sliding through your sight lines dissolves the fog instead
//! of flickering it. A pawn straddling two tiles is shaded by both, which is the
//! tell that the fog belongs to the ground and not to them.
//!
//! Sight lines start `VIEW_PULLBACK` *behind* the pawn rather than at it — a
//! third-person camera over the shoulder, at two [`SHOULDER_OFFSET`] positions —
//! so you can peek around cover you're hugging instead of having it black out
//! half the screen. Ground is only dark where BOTH shoulders are blocked.
//!
//! Three kinds of concealment, which hide differently:
//!   * **Boulders** cast opaque grey. Anything standing in it is genuinely gone.
//!   * **Bushes** cast a translucent haze. One bush is a smudge; a thicket is
//!     nearly a wall, because each overlap multiplies what gets through.
//!   * **Grass** hides by height along the line rather than by blocking it: see
//!     [`grass_conceal`] for why that ends up as extinction over a length.
//!
//! Tiles carry the strength in vertex colors, which is why this uses its own
//! [`FogMaterial`] rather than `ColorMaterial`; see `assets/fog.wgsl`.
//!
//! This replaced a swept soft-shadow mesh — per-caster geometry with penumbra
//! skirts and a per-pixel feather in the shader. It was prettier and much harder
//! to read: you could never tell where a shadow's edge actually was, and three
//! different concealment systems each had their own falloff. `git log` this file
//! if the per-caster version is wanted back.

use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, MeshVertexBufferLayoutRef, PrimitiveTopology};
use bevy::prelude::*;
use bevy::render::render_resource::{
    AsBindGroup, RenderPipelineDescriptor, SpecializedMeshPipelineError,
};
use bevy::shader::ShaderRef;
use bevy::sprite_render::{AlphaMode2d, Material2d, Material2dKey, Material2dPlugin};
use bevy_ggrs::LocalPlayers;

use army_ghosts_sim::{
    Aim, Block as SimBlock, Bush, Player, Pos, Rock, Scenario, Stance, Team, ARENA_HALF_H,
    ARENA_HALF_W, FP, PLAYER_R, STANCE_HEIGHT,
};

/// Unlit ground: dark enough to read as "no information", light enough to tell
/// apart from the out-of-arena clear color. Authored in sRGB like every other
/// color in the client; the shader wants linear, so these are converted once
/// per rebuild (`LinearRgba::rgb(0.14, ..)` would be a *much* paler grey).
const FOG_COLOR: Color = Color::srgb(0.14, 0.145, 0.15);
const FOG_UMBRA: f32 = 1.0;
/// Per-bush haze. A lone bush nearly hides what's behind it and two are all but
/// solid (1 - 0.085^2 ≈ 0.99), so a thicket is a wall. (Bushes used to tint
/// their shadow greener than a boulder's; a tile carries one color, so that
/// distinction now lives only in the strength.)
const BUSH_FOG_UMBRA: f32 = 0.915;
/// How far a shadow is extended past its caster, world units. Anything past
/// the arena diagonal is off-screen at any sane zoom.
const FOG_FAR: f32 = 1600.0;
/// How far behind the pawn sight lines start, world units — the third-person
/// camera setback. Bigger = more you can see around cover you're touching.
/// Roughly half a mobile screen, matching `ads.rs`'s ADS_SHIFT so the two
/// framing distances feel like the same game.
///
/// It works less by shrinking shadows than by making them *parallel*: cover
/// blocks roughly its own width instead of a wedge that flares with distance.
/// Which is why the exact value matters less than you'd think — 60 and 200 fog
/// about the same share of the arena — but pressed up against a boulder the
/// setback is everything, and there a bigger number wins.
const VIEW_PULLBACK: f32 = 50.0;
/// Half the gap between the two virtual shoulder cameras, world units. Ground
/// is only dark where BOTH shoulders are blocked, so this trims every shadow by
/// the fraction `offset / radius` — keep it well under the smallest cover
/// radius (bushes bottom out at 13) or small cover stops casting at all.
const SHOULDER_OFFSET: f32 = 30.0;
/// Shortest terminator a ray may have, world units. Rays grazing the silhouette
/// enter and leave the cover at nearly the same point, so without a floor they
/// snap from lit to full shadow over no distance at all — a hard rim right at
/// the edge of every rock, which is exactly where the eye looks.
const RIM_FEATHER: f32 = 14.0;
/// Penumbra width, world units: a floor, plus a share of how far *past its
/// caster* the point is. Measuring from the caster rather than from the eye is
/// what a real penumbra does — and it keeps the skirt from ballooning around
/// the near side, where it would halo out over the lit flanks of the very rock
/// casting the shadow. The floor matters more than it looks: most shadow edges
/// on screen are close to their caster, so it sets the softness you actually
/// see. Don't chase fuzziness with the rate instead — past ~0.3 the distant
/// penumbras grow wider than a rock and the whole field merges into mush.
const BLUR_NEAR: f32 = 2.0;
const BLUR_PER_UNIT: f32 = 0.04;
/// Floor on how much of a shadow's half width is gradient rather than solid,
/// so even cover whose feather works out narrower than this keeps a soft edge
/// instead of a cut one.
const EDGE_MIN_FRACTION: f32 = 0.08;
/// Bush shadows blur wider than boulder shadows. A rock edge should still read
/// as an edge; foliage shouldn't read as anything but a smudge.
const ROCK_BLUR_SCALE: f32 = 1.0;
const BUSH_BLUR_SCALE: f32 = 1.8;
// The grass model's constants — how many steps a sight line is sampled at,
// where sampling starts, and the extinction per blocked unit — moved to the sim
// with the model itself. `GRASS_EXTINCTION` in particular no longer exists as a
// number anywhere: it was folded into the integer `EXP_NEG` table, which *is*
// the constant now. Tune it there.

/// How dark a fully-hidden tile gets. The rest of the hiding is spent on
/// whoever is standing there (`fade_hidden`), so cover stays total against
/// players while the ground merely goes unreadable — you keep a sense of terrain
/// you can't see into.
const TILE_SHADOW_SCALE: f32 = 0.8;
/// Above the world (players, bushes, bullets, aim line), below the
/// camera-parented touch overlay at z=100. The HUD is bevy_ui, own pass.
const Z_FOG: f32 = 5.0;
/// Hex circumradius, world units — corner to corner is `2 * HEX_R`, flat to flat
/// `sqrt(3) * HEX_R`. Sized against the pawn (24 units across) rather than
/// against the cover: much bigger and you can't tell which side of a boulder a
/// tile is on; much smaller and quantizing stops simplifying anything. At 16 the
/// arena is about 780 tiles.
///
/// It comes FROM THE SIM because the grass depth is quantized to the same grid
/// (`grass_height` is constant across a tile): the fog and the grass must draw
/// the same honeycomb or the tile you can see is not the tile you're hiding in.
/// The layout here — odd-q offset, columns of `1.5 R`, odd ones dropped half a
/// row — has to match `hex_cell` there too.
const HEX_R: f32 = army_ghosts_sim::HEX_R as f32;
/// Points sampled inside each tile before its shadow is averaged: the centre
/// plus one toward every other corner. A tile straddling the edge of a shadow
/// has to come out half dark rather than picking a side — but this is the inner
/// loop of the whole system (probes x tiles x casts, every frame), so it buys
/// that with four points rather than seven.
const HEX_PROBES: usize = 4;
/// How far out from a tile's centre its flat plateau reaches, as a fraction of
/// the way to the corners; the rest is rim that blends into the neighbours. All
/// plateau is a honeycomb of hard polygons, all rim is a smear with no tiles left
/// in it.
const TILE_PLATEAU: f32 = 0.55;
/// Time constant for a tile easing toward its target darkness, seconds. Long
/// enough to dissolve rather than flicker as cover slides in and out of the
/// sight lines, short enough that the fog isn't lagging behind you when it
/// matters.
const TILE_EASE: f32 = 0.14;

/// Flat vertex-color material for the fog mesh — no uniforms, no textures.
///
/// `ColorMaterial` would do if it honored vertex colors here, but it gates them
/// behind a `VERTEX_COLORS` shader def chosen when the entity is *first*
/// specialized, and bevy only re-specializes on `Changed<Mesh2d>` /
/// `Changed<MeshMaterial2d>` — never on the mesh asset gaining an attribute.
/// A fog mesh that starts empty and grows its colors therefore keeps a pipeline
/// compiled without the def and silently paints every shadow flat: correct
/// geometry, hard edges, vertex alpha ignored. Forcing the vertex layout in
/// [`Material2d::specialize`] below makes that impossible.
#[derive(Asset, TypePath, AsBindGroup, Clone, Default)]
pub struct FogMaterial {}

impl Material2d for FogMaterial {
    fn vertex_shader() -> ShaderRef {
        "fog.wgsl".into()
    }

    fn fragment_shader() -> ShaderRef {
        "fog.wgsl".into()
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
            Mesh::ATTRIBUTE_COLOR.at_shader_location(4),
        ])?];
        Ok(())
    }
}

pub struct VisionPlugin;

impl Plugin for VisionPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(Material2dPlugin::<FogMaterial>::default());
    }
}

/// The hex fog: static geometry (one fan per tile), colors rewritten each frame.
///
/// `centres`, `corners`, `target` and `shown` are all parallel to the tiles in
/// the mesh, so `update_fog` walks them together without touching geometry.
#[derive(Resource)]
pub struct FogMesh {
    mesh: Handle<Mesh>,
    centres: Vec<Vec2>,
    /// For each tile, the shared-corner index of each of its six corners. A
    /// corner belongs to up to three hexes; giving all of them the same value
    /// there is what makes the rims blend instead of butting up against each
    /// other.
    corners: Vec<[u32; 6]>,
    /// How many tiles meet at each shared corner (three inside the field, fewer
    /// at the walls).
    corner_share: Vec<f32>,
    /// Where each tile's shadow is heading, and where it has actually got to.
    target: Vec<f32>,
    shown: Vec<f32>,
}

/// Marks the fog entity.
#[derive(Component)]
pub struct Fog;

/// Corner `i` of a flat-top hex of circumradius [`HEX_R`].
fn hex_corner(centre: Vec2, i: usize) -> Vec2 {
    let a = std::f32::consts::PI / 3.0 * i as f32;
    centre + Vec2::new(HEX_R * a.cos(), HEX_R * a.sin())
}

/// Every tile centre covering the arena, in odd-q offset layout. Columns step by
/// `1.5 * R` and odd ones drop half a row, which is what interlocks the grid.
fn hex_centres() -> Vec<Vec2> {
    let (dx, dy) = (HEX_R * 1.5, HEX_R * 3.0f32.sqrt());
    let cols = ((ARENA_HALF_W as f32 * 2.0) / dx).ceil() as i32 + 2;
    let rows = ((ARENA_HALF_H as f32 * 2.0) / dy).ceil() as i32 + 2;
    let mut centres = Vec::with_capacity((cols * rows) as usize);
    for col in -1..cols {
        for row in -1..rows {
            let c = Vec2::new(
                -ARENA_HALF_W as f32 + col as f32 * dx,
                -ARENA_HALF_H as f32 + row as f32 * dy + if col % 2 == 0 { 0.0 } else { dy * 0.5 },
            );
            // Stop at the walls. Past them there is no ground to shade, and a
            // honeycomb hanging over the black outside the arena is the sort of
            // thing that reads as a rendering bug.
            if c.x.abs() < ARENA_HALF_W as f32 + HEX_R && c.y.abs() < ARENA_HALF_H as f32 {
                centres.push(c);
            }
        }
    }
    centres
}

pub fn setup_fog(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<FogMaterial>>,
) {
    let centres = hex_centres();
    let (mut positions, mut colors, mut indices) = (Vec::new(), Vec::new(), Vec::new());
    // Shared corners, found by position: hexes that meet at a point compute the
    // same coordinates for it, to well within this rounding.
    let mut corner_ids: std::collections::HashMap<(i32, i32), u32> = Default::default();
    let mut corner_share: Vec<f32> = Vec::new();
    let mut corners: Vec<[u32; 6]> = Vec::with_capacity(centres.len());

    for centre in &centres {
        // Thirteen vertices per tile: the centre and an inner ring carrying the
        // tile's own value, then the outer corners carrying the value shared
        // with the neighbours. So each tile is a flat plateau with a feathered
        // rim, and along any shared edge both tiles interpolate between the same
        // two corner values — no seam, and no visible hard boundary.
        let base = positions.len() as u32;
        let mut ids = [0u32; 6];
        positions.push([centre.x, centre.y, 0.0]);
        for i in 0..6 {
            let p = centre.lerp(hex_corner(*centre, i), TILE_PLATEAU);
            positions.push([p.x, p.y, 0.0]);
        }
        for i in 0..6 {
            let p = hex_corner(*centre, i);
            positions.push([p.x, p.y, 0.0]);
            let key = ((p.x * 2.0).round() as i32, (p.y * 2.0).round() as i32);
            let id = *corner_ids.entry(key).or_insert_with(|| {
                corner_share.push(0.0);
                corner_share.len() as u32 - 1
            });
            corner_share[id as usize] += 1.0;
            ids[i] = id;
        }
        corners.push(ids);
        colors.extend(std::iter::repeat_n([0.0f32; 4], 13));
        for i in 0..6u32 {
            let j = (i + 1) % 6;
            let (in_i, in_j) = (base + 1 + i, base + 1 + j);
            let (out_i, out_j) = (base + 7 + i, base + 7 + j);
            indices.extend([base, in_i, in_j]);                 // plateau
            indices.extend([in_i, out_i, out_j, in_i, out_j, in_j]); // rim
        }
    }

    // RenderAssetUsages::default() keeps the mesh in the main world too, which
    // is what lets `update_fog` mutate it every frame.
    let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default());
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colors);
    mesh.insert_indices(Indices::U32(indices));
    let mesh = meshes.add(mesh);
    commands.spawn((
        Fog,
        Mesh2d(mesh.clone()),
        MeshMaterial2d(materials.add(FogMaterial::default())),
        Transform::from_xyz(0.0, 0.0, Z_FOG),
        Visibility::Hidden,
        // The Aabb is computed once when Mesh2d is added. The geometry is static
        // now, so this is belt and braces — but the box is only as good as the
        // first frame's, and the cost of getting it wrong is the fog blinking out.
        bevy::camera::visibility::NoFrustumCulling,
    ));
    let tiles = centres.len();
    commands.insert_resource(FogMesh {
        mesh,
        centres,
        corners,
        corner_share,
        target: vec![0.0; tiles],
        shown: vec![0.0; tiles],
    });
}

/// Re-shade every tile from the local player's point of view.
///
/// The rays are unchanged — [`Cast`] cones from the shoulder cameras, plus the
/// grass ray test — and are still evaluated at continuous points. What's
/// quantized is only the *display*: each tile integrates the shadow over its own
/// area ([`HEX_PROBES`] points) and paints one value across its plateau.
///
/// Two things soften that. Between tiles, the rim blends to a value shared with
/// the neighbours, so the grid reads as tiles without hard-edged polygons. Over
/// time, each tile eases toward its target ([`TILE_EASE`]) rather than snapping,
/// so walking dissolves the fog instead of flickering it — which also hides the
/// fact that the targets themselves only recompute every third of a tile walked.
pub fn update_fog(
    mut fog: ResMut<FogMesh>,
    mut meshes: ResMut<Assets<Mesh>>,
    time: Res<Time>,
    local_players: Option<Res<LocalPlayers>>,
    players: Query<(&Player, &Pos, &Stance)>,
    rocks: Query<(&Rock, &Pos)>,
    bushes: Query<(&Bush, &Pos)>,
    mut fog_view: Query<&mut Visibility, With<Fog>>,
    scenario: Res<Scenario>,
    mut last: Local<Option<(Vec2, u8)>>,
) {
    let Ok(mut visibility) = fog_view.single_mut() else { return };
    let eye = local_players.as_deref().and_then(|local| {
        let handle = *local.0.first()?;
        let (_, pos, stance) = players.iter().find(|(p, _, _)| p.handle == handle)?;
        let (x, y) = pos.to_f32();
        Some((Vec2::new(x, y), eye_height(stance), stance.level))
    });
    // No pawn to see from (lobby warmup): show the whole field.
    let Some((eye, eye_h, eye_level)) = eye else {
        *visibility = Visibility::Hidden;
        return;
    };
    *visibility = Visibility::Visible;

    // Cover doesn't move and the grass field is fixed, so the targets are a
    // function of where the viewer stands and how tall they are — and the answer
    // is quantized to 32-unit tiles anyway, so it doesn't need recomputing for
    // every pixel of walking. Standing still costs nothing; walking recomputes
    // ~20 times a second instead of 60, and the easing below covers the steps.
    let moved = !last
        .is_some_and(|(p, level)| level == eye_level && p.distance_squared(eye) < HEX_R * HEX_R / 9.0);
    if moved {
        *last = Some((eye, eye_level));
        let casts = casts_from(eye, &rocks, &bushes);
        // Split the borrow: the targets are written while the centres are read.
        let FogMesh { centres, target, .. } = &mut *fog;
        for (tile, centre) in centres.iter().enumerate() {
            // Grass once per tile, at the centre: the depth field is smooth over
            // a 32-unit hex, so probing it four times costs four ray marches to
            // say the same thing. Cover is the opposite — a shadow edge is hard,
            // and catching it half way across a tile is the point of probing.
            //
            // What the grass term answers is "could I see someone STANDING
            // here", which is the question the player asks of a tile — and it
            // keeps open ground bright, instead of drowning it in the grass that
            // hides the dirt but not a soldier.
            let grass = grass_conceal(&scenario, eye, eye_h, *centre, STANCE_HEIGHT[0] as f32, 0);
            let mut sum = 0.0;
            for i in 0..HEX_PROBES {
                let p = match i {
                    0 => *centre,
                    _ => centre.lerp(hex_corner(*centre, (i - 1) * 2), 0.62),
                };
                sum += 1.0 - (1.0 - coverage_at(&casts, p)) * (1.0 - grass);
            }
            // Tiles carry more of the shadow than the old swept mesh did: the
            // whole point of quantizing is that the fog is what you read the map
            // from. Still short of opaque, so a hidden hex is "no information",
            // not a hole.
            target[tile] = sum / HEX_PROBES as f32 * TILE_SHADOW_SCALE;
        }
    }

    let Some(mesh) = meshes.get_mut(&fog.mesh) else { return };
    // Frame-rate independent ease, so the dissolve looks the same at 30 fps as
    // at 120.
    let k = 1.0 - (-time.delta_secs() / TILE_EASE).exp();
    let FogMesh { corners, corner_share, target, shown, .. } = &mut *fog;
    let mut corner_sum = vec![0.0f32; corner_share.len()];
    for (tile, shown) in shown.iter_mut().enumerate() {
        *shown += (target[tile] - *shown) * k;
        for corner in corners[tile] {
            corner_sum[corner as usize] += *shown;
        }
    }

    let grey = FOG_COLOR.to_linear();
    let mut colors: Vec<[f32; 4]> = Vec::with_capacity(shown.len() * 13);
    for (tile, shown) in shown.iter().enumerate() {
        let vertex = |alpha: f32| [grey.red, grey.green, grey.blue, alpha];
        colors.extend(std::iter::repeat_n(vertex(*shown), 7)); // centre + plateau
        for corner in corners[tile] {
            colors.push(vertex(corner_sum[corner as usize] / corner_share[corner as usize]));
        }
    }
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colors);
}

/// Every piece of cover's shadow cone, resolved against one viewpoint.
fn casts_from(
    eye: Vec2,
    rocks: &Query<(&Rock, &Pos)>,
    bushes: &Query<(&Bush, &Pos)>,
) -> Vec<Cast> {
    let mut casts = Vec::new();
    for (bush, pos) in bushes {
        let (x, y) = pos.to_f32();
        casts.extend(Cast::new(eye, Vec2::new(x, y), bush.r as f32, BUSH_FOG_UMBRA, BUSH_BLUR_SCALE));
    }
    for (rock, pos) in rocks {
        let (x, y) = pos.to_f32();
        casts.extend(Cast::new(eye, Vec2::new(x, y), rock.r as f32, FOG_UMBRA, ROCK_BLUR_SCALE));
    }
    casts
}

/// One piece of cover's shadow, resolved against the viewer.
///
/// Sight lines are parameterised by `t`, the shared lateral fraction: line `t`
/// leaves the camera pair at offset `t * SHOULDER_OFFSET` and crosses the cover
/// at offset `t * r`, so t = ±1 are exactly the two umbra boundaries (right
/// shoulder grazing the left edge and vice versa). The umbra's half width at
/// axial distance x is therefore
///     w(x) = offset + (r - offset) * x / dist
/// which *widens* when the cover is broader than the camera pair and *narrows*
/// to a point when it isn't. That second case is why this is a cone and not a
/// wedge: once the shoulders are set wider than the cover, no ground past a
/// certain distance is hidden from both of them, and the shadow has to close.
struct Cast {
    center: Vec2,
    r: f32,
    eye: Vec2,
    base: Vec2,
    across: Vec2,
    dist: f32,
    umbra: f32,
    blur: f32,
    /// Where the two boundaries meet, when the cover is narrower than the
    /// camera pair. `None` means the shadow diverges and runs off the arena.
    apex: Option<Vec2>,
}

impl Cast {
    /// `None` when the viewer is inside the cover — the test is on the pawn,
    /// never the cameras, so standing in a bush hides you without blinding you.
    fn new(player: Vec2, center: Vec2, r: f32, umbra: f32, blur: f32) -> Option<Cast> {
        let to_center = center - player;
        let range = to_center.length();
        if range <= r + 0.5 {
            return None;
        }
        let base = to_center / range;
        // Straight back along this caster's bearing, so the cameras, the pawn
        // and the cover stay colinear and the distance is just the sum.
        let eye = player - base * VIEW_PULLBACK;
        let dist = range + VIEW_PULLBACK;
        let h = SHOULDER_OFFSET;
        Some(Cast {
            center,
            r,
            eye,
            base,
            across: Vec2::new(-base.y, base.x),
            dist,
            umbra,
            blur,
            apex: (h > r).then(|| eye + base * (h * dist / (h - r))),
        })
    }

    /// The sight line at lateral fraction `t`, and where it meets the cover.
    fn ray(&self, t: f32) -> Ray {
        let h = SHOULDER_OFFSET;
        let origin = self.eye + self.across * (t * h);
        let dir = (self.base * self.dist + self.across * (t * (self.r - h))).normalize_or(self.base);
        // General ray/circle: the origin is off-axis, so the chord isn't just
        // `dist * sin` any more.
        let oc = self.center - origin;
        let proj = oc.dot(dir);
        let half_chord = (self.r * self.r - (oc.length_squared() - proj * proj))
            .max(0.0)
            .sqrt();
        let enter = proj - half_chord;
        let exit = (proj + half_chord).max(enter + RIM_FEATHER);
        Ray {
            origin,
            dir,
            enter,
            exit,
            // Every line meets the others at the apex, so that's one shared
            // point; without one the shadow just runs off the arena.
            end: match self.apex {
                Some(a) => (a - origin).length().max(exit + 1.0),
                None => exit + FOG_FAR,
            },
        }
    }

    /// Umbra half width at the axial distance of `p`, and the fraction of it
    /// that should be gradient rather than solid.
    fn spread(&self, p: Vec2) -> (f32, f32) {
        let x = (p - self.eye).dot(self.base);
        let half_width = SHOULDER_OFFSET + (self.r - SHOULDER_OFFSET) * x / self.dist;
        // The gradient is a *fraction* of the half width, not an absolute
        // distance: measured absolutely, a feather that keeps growing while the
        // cone keeps narrowing eats the shadow from both sides and nothing ever
        // reaches full strength. Capped at 1, the centre line always does.
        let feather = (BLUR_NEAR + BLUR_PER_UNIT * (x - self.dist).max(0.0)) * self.blur;
        let edge = if half_width > 0.0 {
            (feather / half_width).clamp(EDGE_MIN_FRACTION, 1.0)
        } else {
            1.0
        };
        (half_width, edge)
    }

    /// How strongly this cover shadows an arbitrary world point, 0..=umbra.
    /// The same formula the mesh and `fog.wgsl` between them evaluate, so what
    /// hides an enemy always matches what you can see on the ground.
    fn coverage(&self, p: Vec2) -> f32 {
        let (half_width, edge) = self.spread(p);
        if half_width <= 0.0 {
            return 0.0;
        }
        let t = (p - self.eye).dot(self.across) / half_width;
        if t.abs() >= 1.0 {
            return 0.0;
        }
        let ray = self.ray(t);
        let along = (p - ray.origin).dot(ray.dir);
        if along <= ray.enter || along > ray.end {
            return 0.0;
        }
        let depth = smoothstep(((along - ray.enter) / (ray.exit - ray.enter)).clamp(0.0, 1.0));
        let lateral = smoothstep(((1.0 - t.abs()) / edge).clamp(0.0, 1.0));
        self.umbra * depth * lateral
    }
}

/// Total shadow at a point, compositing every caster the way the mesh does.
fn coverage_at(casts: &[Cast], p: Vec2) -> f32 {
    let mut through = 1.0;
    for cast in casts {
        through *= 1.0 - cast.coverage(p);
    }
    1.0 - through
}

/// One sight line through a piece of cover.
#[derive(Copy, Clone)]
struct Ray {
    origin: Vec2,
    dir: Vec2,
    enter: f32,
    exit: f32,
    /// Where this line leaves the umbra: the shared apex, or off past the arena.
    end: f32,
}

fn smoothstep(s: f32) -> f32 {
    s * s * (3.0 - 2.0 * s)
}

/// How much of a pawn `target_h` units tall the GRASS between it and the eye
/// hides, 0..=1.
///
/// Grass can't cast a [`Cast`] — it isn't a set of discrete casters but a
/// continuous depth field — so it gets the honest thing instead: a ray test in
/// elevation.
///
/// **The model itself lives in the sim** ([`army_ghosts_sim::grass_block`], which
/// is where the derivation, the two terms and the history are written down).
/// It moved there when bots arrived: a bot decides from what it can see and
/// every peer has to reach the same decision, so the answer has to be integer
/// and it has to be somewhere both can ask. What is left here is unit
/// conversion — the renderer thinks in `Vec2` and f32 shares, the sim in [`Pos`]
/// and FP — so that there is exactly ONE implementation and what hides a bot is
/// what hides a player, by construction rather than by agreement.
///
/// The eye here is the pawn itself, not the pulled-back shoulder cameras the
/// [`Cast`]s use: peeking *around* a field of grass isn't a thing. (That is also
/// why the [`Cast`] machinery did NOT move — it is a camera model, and it
/// answers a different question from the one a pawn asks about itself.)
/// `stir` is the target's [`army_ghosts_sim::Aim::stir`] — a moving body
/// forfeits concealment (`MOTION_REVEAL`), and the fade has to show the same
/// number the bots decide on. A hypothetical target (the fog tiles, the strip
/// table) passes 0: a tile is asking about ground, and ground holds still.
fn grass_conceal(
    scenario: &Scenario,
    eye: Vec2,
    eye_h: f32,
    target: Vec2,
    target_h: f32,
    stir: i32,
) -> f32 {
    grass_conceal_in(eye, eye_h, target, target_h, stir, |x, y| scenario.depth(x, y))
}

/// [`grass_conceal`] over an arbitrary depth field, rather than whichever one
/// the scenario supplies. `vision/strip_table.rs` is what needs it: the table
/// sweeps depths the arena doesn't contain.
fn grass_conceal_in(
    eye: Vec2,
    eye_h: f32,
    target: Vec2,
    target_h: f32,
    stir: i32,
    depth_at: impl Fn(i32, i32) -> i32,
) -> f32 {
    grass_block(eye, eye_h, target, target_h, depth_at).0.conceal_moving(stir) as f32 / FP as f32
}

/// World units to the sim's fixed point. Rounded rather than truncated so a
/// sight line's ends land where the renderer drew them.
fn to_pos(v: Vec2) -> Pos {
    Pos { x: (v.x * FP as f32).round() as i32, y: (v.y * FP as f32).round() as i32 }
}

/// The sim's [`army_ghosts_sim::Block`] in the units this side of the fence
/// reads in: shares of a body and world units, rather than FP and subunits.
/// A view, not a second implementation — every number in it is the sim's.
#[derive(Copy, Clone, Debug, Default)]
struct Block(SimBlock);

impl Block {
    /// The largest share of the target any one step's grass stands over, 0..=1.
    /// Only the strip table reads the two terms apart; the game only ever wants
    /// them multiplied.
    #[cfg(test)]
    fn covered(&self) -> f32 {
        self.0.covered as f32 / FP as f32
    }
    /// How much of the line is blocked, world units.
    #[cfg(test)]
    fn length(&self) -> f32 {
        self.0.length as f32 / FP as f32
    }
    /// Still-target concealment. The game itself always goes through
    /// [`grass_conceal_in`], which takes the target's stir; the strip table
    /// tabulates the still case.
    #[cfg(test)]
    fn conceal(&self) -> f32 {
        self.0.conceal() as f32 / FP as f32
    }
}

fn grass_block(
    eye: Vec2,
    eye_h: f32,
    target: Vec2,
    target_h: f32,
    depth_at: impl Fn(i32, i32) -> i32,
) -> Block {
    Block(army_ghosts_sim::grass_block(
        to_pos(eye),
        eye_h as i32,
        to_pos(target),
        target_h as i32,
        depth_at,
    ))
}

/// A pawn's eye height, world units — where it looks from, and (near enough)
/// where it can be seen to.
fn eye_height(stance: &Stance) -> f32 {
    STANCE_HEIGHT[(stance.level as usize).min(STANCE_HEIGHT.len() - 1)] as f32
}

/// Fade other players by how well cover and grass hide them from the local pawn.
///
/// Cover is the *full* shadow, not the halved version painted on the ground: a
/// pawn in complete cover goes completely invisible, which is what makes hiding
/// mean anything, while the terrain behind them stays merely dim. Sampled at
/// several points across the body rather than just the centre, so someone
/// edging out from behind a rock fades in gradually instead of popping.
///
/// The grass term multiplies with that — two independent ways of not being seen,
/// so a bush in front of a pawn already lying in deep grass compounds.
///
/// **Your own side is exempt.** Teammates are drawn at full opacity wherever
/// they are, which is a deliberate departure from "you see what your pawn can
/// see": in a game without respawns, not knowing whether the four people you are
/// relying on are alive and where makes the whole thing unplayable, and every
/// squad shooter solves it the same way. What it costs is that the fog is no
/// longer a pure statement about sight lines — a teammate visible through a
/// boulder is you reading a radio, not your eyes.
/// A pawn as the fade reads it — and its `Aim`, because the enemy's own recent
/// movement is part of how visible they are.
type Faded = (
    &'static Player,
    &'static Team,
    &'static Pos,
    &'static Stance,
    &'static Aim,
    &'static mut Sprite,
);

pub fn fade_hidden(
    local_players: Option<Res<LocalPlayers>>,
    rocks: Query<(&Rock, &Pos)>,
    bushes: Query<(&Bush, &Pos)>,
    mut players: Query<Faded, With<Player>>,
    scenario: Res<Scenario>,
) {
    let Some(local) = local_players else { return };
    let Some(&handle) = local.0.first() else { return };
    let me = players
        .iter()
        .find(|(p, ..)| p.handle == handle)
        .map(|(_, team, pos, stance, _, _)| {
            let (x, y) = pos.to_f32();
            (*team, Vec2::new(x, y), eye_height(stance))
        });
    let Some((my_team, viewer, viewer_h)) = me else { return };

    let mut casts: Vec<Cast> = Vec::new();
    for (bush, pos) in &bushes {
        let (x, y) = pos.to_f32();
        casts.extend(Cast::new(viewer, Vec2::new(x, y), bush.r as f32, BUSH_FOG_UMBRA, BUSH_BLUR_SCALE));
    }
    for (rock, pos) in &rocks {
        let (x, y) = pos.to_f32();
        casts.extend(Cast::new(viewer, Vec2::new(x, y), rock.r as f32, FOG_UMBRA, ROCK_BLUR_SCALE));
    }

    let reach = PLAYER_R as f32 * 0.7;
    for (player, team, pos, stance, aim, mut sprite) in &mut players {
        // Never hide yourself, and never hide your own side — see the note above.
        if player.handle == handle || *team == my_team {
            sprite.color.set_alpha(1.0);
            continue;
        }
        let (x, y) = pos.to_f32();
        let body = Vec2::new(x, y);
        let hidden: f32 = [
            Vec2::ZERO,
            Vec2::new(reach, 0.0),
            Vec2::new(-reach, 0.0),
            Vec2::new(0.0, reach),
            Vec2::new(0.0, -reach),
        ]
        .iter()
        .map(|offset| coverage_at(&casts, body + *offset))
        .sum::<f32>()
            / 5.0;
        // The enemy's own stir: a runner is faded back IN, because the grass
        // over them is moving and yours are the eyes it is moving in front of.
        let grass = grass_conceal(&scenario, viewer, viewer_h, body, eye_height(stance), aim.stir);
        sprite
            .color
            .set_alpha(((1.0 - hidden) * (1.0 - grass)).clamp(0.0, 1.0));
    }
}

/// The concealment table generator (`tools/grass-table.sh`).
#[cfg(test)]
mod strip_table;

#[cfg(test)]
mod tests {
    use super::*;

    /// The properties the grass sight model is *for*. All of these are things a
    /// plausible-looking tweak to `GRASS_EXTINCTION` or the ray math has broken
    /// at least once, and none of them depend on the exact numbers.
    #[test]
    fn grass_hides_by_height_distance_and_stance() {
        let h = |level: usize| STANCE_HEIGHT[level] as f32;
        let (west, east) = (Vec2::new(-150.0, 0.0), Vec2::new(150.0, 0.0));
        let near = Vec2::new(-90.0, 0.0); // same bearing, a fifth of the way

        let standing = grass_conceal(&Scenario::Arena, west, h(0), east, h(0), 0);
        let crouching = grass_conceal(&Scenario::Arena, west, h(0), east, h(1), 0);
        let prone = grass_conceal(&Scenario::Arena, west, h(0), east, h(2), 0);
        assert!((0.0..=1.0).contains(&standing));
        assert!(
            prone > crouching && crouching > standing,
            "lower must always be harder to see: {standing} / {crouching} / {prone}"
        );

        // Distance has to matter, or grass stops being terrain and becomes a
        // property of standing in it.
        assert!(
            grass_conceal(&Scenario::Arena, west, h(0), near, h(0), 0) < standing,
            "a pawn 60 units away must be plainer than one 300 away"
        );

        // Lying down buys concealment and costs sight: the same target is harder
        // to make out from down in the blades.
        assert!(
            grass_conceal(&Scenario::Arena, west, h(2), east, h(0), 0) > standing,
            "a prone viewer must see less, not more"
        );

        // Bare ground hides nobody. There is none left in the arena — the field
        // is a thick even sward now — so this asks the rig for it instead of
        // hunting for the thinnest corner of the map and re-hunting whenever
        // `GRASS_SEED` moves.
        let bare = Scenario::GrassStrip { depth: 0, east_stance: 0 };
        assert_eq!(
            grass_conceal(&bare, west, h(0), east, h(0), 0),
            0.0,
            "bare ground must not conceal"
        );

        // Standing has to remain a fight. The arena's own grass is waist-deep
        // everywhere, and if that were enough to hide an upright soldier there
        // would be no reason to ever go prone — nor any way to find anybody.
        //
        // Asked at FIGHTING range, not across the whole arena: a sight line the
        // width of the map crosses ~12 tiles and `covered` takes the deepest, so
        // fading out at 300 units is the model working, not failing.
        let close = grass_conceal(&Scenario::Arena, west, h(0), west + Vec2::new(80.0, 0.0), h(0), 0);
        assert!(
            close < 0.7,
            "the field must not hide a standing pawn 80 units away: {close}"
        );
    }
}
