//! Line of sight: everything the local player can't see is covered up.
//!
//! Purely render-side — the sim has no notion of who can see what (it can't:
//! every peer simulates every pawn). Every piece of cover casts a shadow away
//! from the local player; all of them go into one triangle mesh, rebuilt every
//! frame, drawn above the world but below the touch overlay and the HUD.
//!
//! Sight lines start `VIEW_PULLBACK` *behind* the pawn rather than at it — a
//! third-person camera over the shoulder — so you can peek around cover you're
//! hugging instead of having it black out half the screen. See `push_caster`.
//!
//! Two kinds of cover, which hide differently:
//!   * **Boulders** cast opaque grey. Anything standing in it — a player, a
//!     bullet, another rock — is genuinely gone, not dimmed.
//!   * **Bushes** cast a translucent haze. One bush between you and something
//!     is a smudge; a whole thicket is nearly as good as a wall. That stacking
//!     is free: the shadows are separate triangles in one alpha-blended mesh,
//!     so each overlap multiplies what gets through — the same stacking the
//!     translucent bush sprites themselves do on the ground. Bush shadows are
//!     emitted first so the opaque boulder shadows paint over them.
//!
//! The shadows are *soft*, because a hard-edged wedge reads as a polygon
//! someone drew on the level rather than as fog. Two things do that:
//!   * Each shadow starts *inside* its caster: `push_caster` sweeps rays across
//!     the angle the cover subtends and ramps alpha from nothing where a ray
//!     enters the circle to full where it leaves, so cover is lit on the
//!     player's side and rolls into darkness over its back, like a sphere lit
//!     from one side. Cover therefore draws UNDER the fog (`render.rs` `Z_*`) —
//!     the fog is what shades it. It also means a thicket darkens bush by bush
//!     from the inside, instead of going flat at the front of the cluster.
//!     Grazing rays enter and leave at nearly the same point, so `RIM_FEATHER`
//!     gives them a minimum ramp — without it every rock has a hard rim exactly
//!     where the eye looks.
//!   * Both flanks get a penumbra skirt fading to nothing, narrow at the
//!     silhouette and widening with distance *past the caster* — what an area
//!     light does. Measuring that from the eye instead makes the skirt balloon
//!     around the near side and halo over the lit flanks of its own rock.
//!
//! Tint and falloff both ride in vertex colors, which is why this uses its own
//! [`FogMaterial`] rather than `ColorMaterial`; see `assets/fog.wgsl`.

use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, MeshVertexBufferLayoutRef, PrimitiveTopology};
use bevy::prelude::*;
use bevy::render::render_resource::{
    AsBindGroup, RenderPipelineDescriptor, SpecializedMeshPipelineError,
};
use bevy::shader::ShaderRef;
use bevy::sprite_render::{AlphaMode2d, Material2d, Material2dKey, Material2dPlugin};
use bevy_ggrs::LocalPlayers;

use army_ghosts_sim::{Bush, Player, Pos, Rock, PLAYER_R};

/// Unlit ground: dark enough to read as "no information", light enough to tell
/// apart from the out-of-arena clear color. Authored in sRGB like every other
/// color in the client; the shader wants linear, so these are converted once
/// per rebuild (`LinearRgba::rgb(0.14, ..)` would be a *much* paler grey).
const FOG_COLOR: Color = Color::srgb(0.14, 0.145, 0.15);
const FOG_UMBRA: f32 = 1.0;
/// Per-bush haze. A lone bush nearly hides what's behind it and two are all but
/// solid (1 - 0.085^2 ≈ 0.99), so a thicket is a wall.
const BUSH_FOG_COLOR: Color = Color::srgb(0.13, 0.15, 0.12);
const BUSH_FOG_UMBRA: f32 = 0.915;
/// How far a shadow is extended past its caster, world units. Anything past
/// the arena diagonal is off-screen at any sane zoom.
const FOG_FAR: f32 = 1600.0;
/// Sight lines sampled across the angle a piece of cover subtends. More rays =
/// rounder silhouette, since the shadow's shape is traced by where these enter
/// and leave the circle.
const CAST_RAYS: usize = 16;
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
/// Steps across the front-to-back terminator on each ray. Enough that the ramp
/// reads as a curve rather than a flat wedge.
const TERMINATOR_STEPS: usize = 4;
/// Steps along the body of the shadow. The inward feather varies down its
/// length (the cone narrows, the blur widens), so it needs more than the two
/// ends to interpolate between.
const BODY_STEPS: usize = 3;
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
/// What share of a shadow gets painted on the terrain. The rest of its
/// strength goes into hiding whoever stands in it (`fade_hidden`), so cover is
/// total against players while the ground behind it only dims — you keep a
/// sense of terrain you can't actually see into.
const TERRAIN_SHADOW_SCALE: f32 = 0.5;
/// Above the world (players, bushes, bullets, aim line), below the
/// camera-parented touch overlay at z=100. The HUD is bevy_ui, own pass.
const Z_FOG: f32 = 5.0;

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
            Mesh::ATTRIBUTE_UV_0.at_shader_location(2),
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

/// Handle to the shadow mesh, rebuilt in place each frame.
#[derive(Resource)]
pub struct FogMesh(Handle<Mesh>);

/// Marks the fog entity.
#[derive(Component)]
pub struct Fog;

pub fn setup_fog(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<FogMaterial>>,
) {
    // RenderAssetUsages::default() keeps the mesh in the main world too, which
    // is what lets `update_fog` mutate it every frame.
    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::default(),
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, Vec::<[f32; 3]>::new());
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, Vec::<[f32; 2]>::new());
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, Vec::<[f32; 4]>::new());
    let mesh = meshes.add(mesh);
    commands.spawn((
        Fog,
        Mesh2d(mesh.clone()),
        MeshMaterial2d(materials.add(FogMaterial::default())),
        Transform::from_xyz(0.0, 0.0, Z_FOG),
        Visibility::Hidden,
        // The Aabb is computed once when Mesh2d is added; our vertices move
        // every frame, so culling against that stale box would blink the fog
        // out whenever the camera left the original bounds.
        bevy::camera::visibility::NoFrustumCulling,
    ));
    commands.insert_resource(FogMesh(mesh));
}

/// Rebuild the shadow mesh from the local player's point of view.
pub fn update_fog(
    fog: Res<FogMesh>,
    mut meshes: ResMut<Assets<Mesh>>,
    local_players: Option<Res<LocalPlayers>>,
    players: Query<(&Player, &Pos)>,
    rocks: Query<(&Rock, &Pos)>,
    bushes: Query<(&Bush, &Pos)>,
    mut fog_view: Query<&mut Visibility, With<Fog>>,
) {
    let Ok(mut visibility) = fog_view.single_mut() else { return };
    let eye = local_players.as_deref().and_then(|local| {
        let handle = *local.0.first()?;
        let (_, pos) = players.iter().find(|(p, _)| p.handle == handle)?;
        let (x, y) = pos.to_f32();
        Some(Vec2::new(x, y))
    });
    // No pawn to see from (lobby warmup): show the whole field.
    let Some(eye) = eye else {
        *visibility = Visibility::Hidden;
        return;
    };
    let Some(mesh) = meshes.get_mut(&fog.0) else { return };

    let mut buffers = ShadowMesh::default();
    // Haze first, opaque grey over the top of it.
    let (haze, grey) = (BUSH_FOG_COLOR.to_linear(), FOG_COLOR.to_linear());
    for (bush, pos) in &bushes {
        let (x, y) = pos.to_f32();
        if let Some(cast) = Cast::new(eye, Vec2::new(x, y), bush.r as f32, BUSH_FOG_UMBRA, BUSH_BLUR_SCALE) {
            buffers.push_caster(&cast, haze);
        }
    }
    for (rock, pos) in &rocks {
        let (x, y) = pos.to_f32();
        if let Some(cast) = Cast::new(eye, Vec2::new(x, y), rock.r as f32, FOG_UMBRA, ROCK_BLUR_SCALE) {
            buffers.push_caster(&cast, grey);
        }
    }

    *visibility = if buffers.indices.is_empty() {
        Visibility::Hidden
    } else {
        Visibility::Visible
    };
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, buffers.positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, buffers.uvs);
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, buffers.colors);
    mesh.insert_indices(Indices::U32(buffers.indices));
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
            t,
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

/// Vertex buffers under construction for the fog mesh.
#[derive(Default)]
struct ShadowMesh {
    positions: Vec<[f32; 3]>,
    uvs: Vec<[f32; 2]>,
    colors: Vec<[f32; 4]>,
    indices: Vec<u32>,
}

impl ShadowMesh {
    /// Lay one caster's shadow into the mesh as a grid of quads: across the
    /// sight lines, and along each of them through the terminator and then the
    /// body. Vertex alpha carries the terminator only — the sideways falloff
    /// travels in the UVs and is evaluated per pixel in `fog.wgsl`.
    fn push_caster(&mut self, cast: &Cast, tint: LinearRgba) {
        let rays: Vec<Ray> = (0..=CAST_RAYS)
            .map(|i| cast.ray(-1.0 + 2.0 * i as f32 / CAST_RAYS as f32))
            .collect();

        // Every sight line is sampled at the same milestones so neighbouring
        // lines pair up into quads.
        let sample = |ray: &Ray, j: usize| {
            if j <= TERMINATOR_STEPS {
                let s = j as f32 / TERMINATOR_STEPS as f32;
                (ray.at(s), smoothstep(s))
            } else {
                let f = (j - TERMINATOR_STEPS) as f32 / BODY_STEPS as f32;
                (ray.point(ray.exit + (ray.end - ray.exit) * f), 1.0)
            }
        };

        for i in 0..CAST_RAYS {
            let (a, b) = (&rays[i], &rays[i + 1]);
            for j in 0..TERMINATOR_STEPS + BODY_STEPS {
                let corner = |ray: &Ray, j: usize| {
                    let (p, depth) = sample(ray, j);
                    let (_, edge) = cast.spread(p);
                    // Terrain only takes a share of the shadow, so you keep a
                    // sense of ground you can't see; the full strength is spent
                    // on hiding whoever is standing in it (`fade_hidden`).
                    (p, ray.t, edge, cast.umbra * depth * TERRAIN_SHADOW_SCALE)
                };
                let (p0, t0, e0, a0) = corner(a, j);
                let (p1, t1, e1, a1) = corner(a, j + 1);
                let (p2, t2, e2, a2) = corner(b, j + 1);
                let (p3, t3, e3, a3) = corner(b, j);
                let base = self.positions.len() as u32;
                self.vertex(p0, t0, e0, tint, a0);
                self.vertex(p1, t1, e1, tint, a1);
                self.vertex(p2, t2, e2, tint, a2);
                self.vertex(p3, t3, e3, tint, a3);
                self.indices
                    .extend([base, base + 1, base + 2, base, base + 2, base + 3]);
            }
        }
    }

    fn vertex(&mut self, p: Vec2, t: f32, edge: f32, tint: LinearRgba, alpha: f32) {
        self.positions.push([p.x, p.y, 0.0]);
        self.uvs.push([t, edge]);
        self.colors.push([tint.red, tint.green, tint.blue, alpha]);
    }
}

/// One sight line through a piece of cover.
#[derive(Copy, Clone)]
struct Ray {
    /// Lateral fraction across the cover: -1 and +1 are the umbra boundaries.
    t: f32,
    origin: Vec2,
    dir: Vec2,
    enter: f32,
    exit: f32,
    /// Where this line leaves the umbra: the shared apex, or off past the arena.
    end: f32,
}

impl Ray {
    /// The point a fraction `s` of the way through the cover.
    fn at(&self, s: f32) -> Vec2 {
        self.point(self.enter + (self.exit - self.enter) * s)
    }

    fn point(&self, t: f32) -> Vec2 {
        self.origin + self.dir * t
    }
}

fn smoothstep(s: f32) -> f32 {
    s * s * (3.0 - 2.0 * s)
}

/// Fade other players by how well the cover hides them from the local pawn.
///
/// This is the *full* shadow, not the halved version painted on the ground: a
/// pawn in complete cover goes completely invisible, which is what makes hiding
/// mean anything, while the terrain behind them stays merely dim. Sampled at
/// several points across the body rather than just the centre, so someone
/// edging out from behind a rock fades in gradually instead of popping.
pub fn fade_hidden(
    local_players: Option<Res<LocalPlayers>>,
    rocks: Query<(&Rock, &Pos)>,
    bushes: Query<(&Bush, &Pos)>,
    mut players: Query<(&Player, &Pos, &mut Sprite), With<Player>>,
) {
    let Some(local) = local_players else { return };
    let Some(&handle) = local.0.first() else { return };
    let viewer = players.iter().find(|(p, _, _)| p.handle == handle).map(|(_, pos, _)| {
        let (x, y) = pos.to_f32();
        Vec2::new(x, y)
    });
    let Some(viewer) = viewer else { return };

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
    for (player, pos, mut sprite) in &mut players {
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
        sprite.color.set_alpha((1.0 - hidden).clamp(0.0, 1.0));
    }
}
