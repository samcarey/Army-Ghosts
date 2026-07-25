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
//! paints the average flat across the tile. Adjacent tiles don't share vertices,
//! so the boundaries are hard on purpose: what's hidden reads as a *place* —
//! this hex, not that one — rather than as a smear whose edge you can't locate.
//! A pawn straddling two tiles is shaded by both, which is the tell that the fog
//! belongs to the ground and not to them.
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
//! if the soft version is wanted back.

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
    grass_height, Bush, Player, Pos, Rock, Stance, ARENA_HALF_H, ARENA_HALF_W, PLAYER_R,
    STANCE_HEIGHT,
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
/// Steps along a sight line when testing it against the grass. The depth field's
/// finest octave is 38 units across, so even a sight line the width of the arena
/// samples it faster than it changes.
const GRASS_SAMPLES: usize = 24;
/// Where the grass test starts, as a fraction of the way to the target. The
/// first stretch in front of your face is skipped on the grounds that you are
/// looking *through* the blades you're lying in, not at them — and because the
/// `1/t` in `grass_conceal` has to be kept away from zero. Raise it to make
/// crawling less blinding, lower it to make grass at your nose count.
const GRASS_NEAR_T: f32 = 0.06;
/// Extinction per world unit of blocked sight line. Tuned so grass matters at
/// fighting range without erasing the game: at 60 units apart in ordinary grass
/// two standing pawns barely dim (~0.03), across 300 units they're half gone,
/// and a prone pawn at that range is ~0.8 hidden. Turning this up hides
/// everything from everyone; turning it down makes grass decoration.
const GRASS_EXTINCTION: f32 = 0.010;
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
/// arena is about 750 tiles.
const HEX_R: f32 = 16.0;
/// Points sampled inside each tile before its shadow is averaged: the centre
/// plus one toward every other corner. A tile straddling the edge of a shadow
/// has to come out half dark rather than picking a side — but this is the inner
/// loop of the whole system (probes x tiles x casts, every frame), so it buys
/// that with four points rather than seven.
const HEX_PROBES: usize = 4;

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
/// `centres` is parallel to the tiles in the mesh, so `update_fog` can walk the
/// two together without recomputing any geometry.
#[derive(Resource)]
pub struct FogMesh {
    mesh: Handle<Mesh>,
    centres: Vec<Vec2>,
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
    for centre in &centres {
        // A fan: the centre plus its six corners. The tile gets ONE color, so
        // its vertices are never shared with a neighbour — that hard edge
        // between tiles is the whole point of the quantization.
        let base = positions.len() as u32;
        positions.push([centre.x, centre.y, 0.0]);
        colors.push([0.0; 4]);
        for i in 0..6 {
            let p = hex_corner(*centre, i);
            positions.push([p.x, p.y, 0.0]);
            colors.push([0.0; 4]);
            indices.extend([base, base + 1 + i as u32, base + 1 + ((i as u32 + 1) % 6)]);
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
    commands.insert_resource(FogMesh { mesh, centres });
}

/// Re-shade every tile from the local player's point of view.
///
/// The rays are unchanged — [`Cast`] cones from the shoulder cameras, plus the
/// grass ray test — and are still evaluated at continuous points. What's
/// quantized is only the *display*: each tile integrates the shadow over its own
/// area (`HEX_PROBES` points) and paints the average across the whole hex.
pub fn update_fog(
    fog: Res<FogMesh>,
    mut meshes: ResMut<Assets<Mesh>>,
    local_players: Option<Res<LocalPlayers>>,
    players: Query<(&Player, &Pos, &Stance)>,
    rocks: Query<(&Rock, &Pos)>,
    bushes: Query<(&Bush, &Pos)>,
    mut fog_view: Query<&mut Visibility, With<Fog>>,
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
    let Some(mesh) = meshes.get_mut(&fog.mesh) else { return };
    *visibility = Visibility::Visible;
    // Cover doesn't move and the grass field is fixed, so the whole picture is a
    // function of where the viewer stands and how tall they are — and the answer
    // is quantized to 32-unit tiles anyway, so it doesn't need recomputing for
    // every pixel of walking. This is the cheap half of what quantizing bought:
    // standing still costs nothing, and walking recomputes ~20 times a second
    // instead of 60. The threshold has to stay well under a tile or shadows lag
    // visibly behind the pawn.
    if last.is_some_and(|(p, level)| level == eye_level && p.distance_squared(eye) < HEX_R * HEX_R / 9.0)
    {
        return;
    }
    *last = Some((eye, eye_level));

    let casts = casts_from(eye, &rocks, &bushes);
    let grey = FOG_COLOR.to_linear();
    let mut colors: Vec<[f32; 4]> = Vec::with_capacity(fog.centres.len() * 7);
    for centre in &fog.centres {
        // Integrate over the tile rather than sampling its middle: a hex that
        // straddles the edge of a shadow should come out half dark, not pick a
        // side. The centre plus the six corner midpoints is enough for that.
        // Grass once per tile, at the centre: the depth field is smooth over a
        // 32-unit hex, so probing it four times costs four ray marches to say
        // the same thing. Cover is the opposite — a shadow edge is hard, and
        // catching it half way across a tile is the entire point of probing.
        //
        // What the grass term answers is "could I see someone STANDING here",
        // which is the question the player asks of a tile — and it keeps open
        // ground bright, instead of drowning it in the grass that hides the dirt
        // but not a soldier.
        let grass = grass_conceal(eye, eye_h, *centre, STANCE_HEIGHT[0] as f32);
        let mut sum = 0.0;
        for i in 0..HEX_PROBES {
            let p = match i {
                0 => *centre,
                _ => centre.lerp(hex_corner(*centre, (i - 1) * 2), 0.62),
            };
            sum += 1.0 - (1.0 - coverage_at(&casts, p)) * (1.0 - grass);
        }
        // Tiles carry more of the shadow than the old swept mesh did: the whole
        // point of quantizing is that the fog is the thing you read the map
        // from, and at the soft version's `TERRAIN_SHADOW_SCALE` the tiles were
        // too faint to tell apart over a busy grass texture. Still short of
        // opaque, so a hidden hex is "no information", not a hole.
        let alpha = sum / HEX_PROBES as f32 * TILE_SHADOW_SCALE;
        for _ in 0..7 {
            colors.push([grey.red, grey.green, grey.blue, alpha]);
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
/// elevation. Walk the sight line, and at each step ask how high up the target a
/// sight line grazing the blade tips there would land. A blade of depth `g` at
/// fraction `t` along the path, seen from an eye at height `E`, hides everything
/// on the target below
///
///     E + (g - E) / t
///
/// (similar triangles: the line through the tip carries on to the target plane).
/// The share of the body under that line is how much *this* step hides.
///
/// Those shares then go through Beer-Lambert rather than a max, and that choice
/// is the whole character of the mechanic. Grass is not a wall: it is blades with
/// gaps, so a metre of it dims and fifty metres of it hides. Taking the worst
/// step instead made every prone pawn on the map invisible from everywhere and
/// every prone *viewer* blind, because somewhere on any long line there is always
/// one blade taller than the sight line. Extinction over the blocked LENGTH gives
/// the thing its distance falloff: with `GRASS_EXTINCTION`, about 150 units of
/// fully-blocking grass hides ~80% of a pawn.
///
/// Two more things fall out of the geometry rather than being written into it:
///   * **Grass at the target's own feet (t = 1) hides it up to `g`** — which is
///     [`army_ghosts_sim::grass_cover`], the standing-in-it rule, as the limiting
///     case of this one.
///   * **Lying down costs you sight as well as buying it.** A prone eye is 15
///     units up, so anything deeper than that near you blocks; you keep close
///     range vision and lose the far half of the field. Crawling to the edge of a
///     patch to see out of it is the intended move.
///
/// The eye here is the pawn itself, not the pulled-back shoulder cameras the
/// [`Cast`]s use: peeking *around* a field of grass isn't a thing.
fn grass_conceal(eye: Vec2, eye_h: f32, target: Vec2, target_h: f32) -> f32 {
    let dist = eye.distance(target);
    if target_h <= 0.0 || dist < 1.0 {
        return 0.0;
    }
    let mut blocked_len = 0.0;
    let step = dist * (1.0 - GRASS_NEAR_T) / GRASS_SAMPLES as f32;
    for i in 0..GRASS_SAMPLES {
        let t = GRASS_NEAR_T
            + (1.0 - GRASS_NEAR_T) * (i + 1) as f32 / GRASS_SAMPLES as f32;
        let p = eye.lerp(target, t);
        let depth = grass_height(p.x.round() as i32, p.y.round() as i32) as f32;
        let reaches = eye_h + (depth - eye_h) / t;
        blocked_len += (reaches / target_h).clamp(0.0, 1.0) * step;
    }
    1.0 - (-GRASS_EXTINCTION * blocked_len).exp()
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
pub fn fade_hidden(
    local_players: Option<Res<LocalPlayers>>,
    rocks: Query<(&Rock, &Pos)>,
    bushes: Query<(&Bush, &Pos)>,
    mut players: Query<(&Player, &Pos, &Stance, &mut Sprite), With<Player>>,
) {
    let Some(local) = local_players else { return };
    let Some(&handle) = local.0.first() else { return };
    let eye = players
        .iter()
        .find(|(p, _, _, _)| p.handle == handle)
        .map(|(_, pos, stance, _)| {
            let (x, y) = pos.to_f32();
            (Vec2::new(x, y), eye_height(stance))
        });
    let Some((viewer, viewer_h)) = eye else { return };

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
    for (player, pos, stance, mut sprite) in &mut players {
        if player.handle == handle {
            sprite.color.set_alpha(1.0); // never hide yourself
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
        let grass = grass_conceal(viewer, viewer_h, body, eye_height(stance));
        sprite
            .color
            .set_alpha(((1.0 - hidden) * (1.0 - grass)).clamp(0.0, 1.0));
    }
}

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

        let standing = grass_conceal(west, h(0), east, h(0));
        let crouching = grass_conceal(west, h(0), east, h(1));
        let prone = grass_conceal(west, h(0), east, h(2));
        assert!((0.0..=1.0).contains(&standing));
        assert!(
            prone > crouching && crouching > standing,
            "lower must always be harder to see: {standing} / {crouching} / {prone}"
        );

        // Distance has to matter, or grass stops being terrain and becomes a
        // property of standing in it.
        assert!(
            grass_conceal(west, h(0), near, h(0)) < standing,
            "a pawn 60 units away must be plainer than one 300 away"
        );

        // Lying down buys concealment and costs sight: the same target is harder
        // to make out from down in the blades.
        assert!(
            grass_conceal(west, h(2), east, h(0)) > standing,
            "a prone viewer must see less, not more"
        );

        // Bare ground hides nobody. (0, -250) sits in the arena's thinnest
        // corner; if the depth field is ever reseeded this may need moving.
        let bare = Vec2::new(-40.0, -250.0);
        assert!(
            grass_conceal(bare, h(0), bare + Vec2::new(60.0, 0.0), h(0)) < 0.2,
            "thin ground must not conceal"
        );
    }
}
