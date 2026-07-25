//! Line of sight: everything the local player can't see is covered up.
//!
//! Purely render-side — the sim has no notion of who can see what (it can't:
//! every peer simulates every pawn). Every piece of cover casts a shadow away
//! from the local player's eye; all of them go into one triangle mesh, rebuilt
//! every frame, drawn above the world but below the touch overlay and the HUD.
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

use army_ghosts_sim::{Bush, Player, Pos, Rock};

/// Unlit ground: dark enough to read as "no information", light enough to tell
/// apart from the out-of-arena clear color. Authored in sRGB like every other
/// color in the client; the shader wants linear, so these are converted once
/// per rebuild (`LinearRgba::rgb(0.14, ..)` would be a *much* paler grey).
const FOG_COLOR: Color = Color::srgb(0.14, 0.145, 0.15);
const FOG_UMBRA: f32 = 1.0;
/// Per-bush haze. Low enough that a lone bush only softens what's behind it,
/// high enough that a few deep is effectively cover (1 - 0.66^4 ≈ 0.81).
const BUSH_FOG_COLOR: Color = Color::srgb(0.13, 0.15, 0.12);
const BUSH_FOG_UMBRA: f32 = 0.34;
/// How far a shadow is extended past its caster, world units. Anything past
/// the arena diagonal is off-screen at any sane zoom.
const FOG_FAR: f32 = 1600.0;
/// Sight lines sampled across the angle a piece of cover subtends. More rays =
/// rounder silhouette, since the shadow's shape is traced by where these enter
/// and leave the circle.
const CAST_RAYS: usize = 16;
/// Steps across the front-to-back terminator on each ray. Enough that the ramp
/// reads as a curve rather than a flat wedge.
const TERMINATOR_STEPS: usize = 4;
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
const BLUR_NEAR: f32 = 10.0;
const BLUR_PER_UNIT: f32 = 0.20;
/// Bush shadows blur wider than boulder shadows. A rock edge should still read
/// as an edge; foliage shouldn't read as anything but a smudge.
const ROCK_BLUR_SCALE: f32 = 1.0;
const BUSH_BLUR_SCALE: f32 = 1.8;
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
        buffers.push_caster(eye, pos, bush.r, haze, BUSH_FOG_UMBRA, BUSH_BLUR_SCALE);
    }
    for (rock, pos) in &rocks {
        buffers.push_caster(eye, pos, rock.r, grey, FOG_UMBRA, ROCK_BLUR_SCALE);
    }

    *visibility = if buffers.indices.is_empty() {
        Visibility::Hidden
    } else {
        Visibility::Visible
    };
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, buffers.positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, buffers.colors);
    mesh.insert_indices(Indices::U32(buffers.indices));
}


/// Vertex buffers under construction for the fog mesh.
#[derive(Default)]
struct ShadowMesh {
    positions: Vec<[f32; 3]>,
    colors: Vec<[f32; 4]>,
    indices: Vec<u32>,
}

impl ShadowMesh {
    /// Cast one circle of cover's shadow, as a sweep of rays across the angle
    /// it subtends from the eye.
    ///
    /// Each ray carries three spans: nothing before it reaches the cover, a
    /// front-to-back *terminator* gradient between where it enters and leaves
    /// the cover, and full `umbra` from the far surface outward. So the shadow
    /// starts inside the caster and rolls over its back like a sphere lit from
    /// the player's side — and because every rock and every bush does this
    /// individually, a thicket darkens bush by bush instead of going flat at
    /// the front of the cluster.
    ///
    /// Rays that graze the silhouette enter and leave at the same point, so the
    /// gradient pinches to nothing at the rim and the shadow is full strength
    /// there — which is what makes the caster read as round.
    fn push_caster(
        &mut self,
        eye: Vec2,
        pos: &Pos,
        radius: i32,
        tint: LinearRgba,
        umbra: f32,
        blur: f32,
    ) {
        let (x, y) = pos.to_f32();
        let center = Vec2::new(x, y);
        let r = radius as f32;
        let to_center = center - eye;
        let dist = to_center.length();
        if dist <= r + 0.5 {
            return; // standing in it: cover hides you, it doesn't blind you
        }
        let base = to_center / dist;
        let half_angle = (r / dist).clamp(-1.0, 1.0).asin();
        // Distance to the silhouette, where the cover stops occluding.
        let grazing = (dist * dist - r * r).max(0.0).sqrt();

        // Where each ray enters and leaves the cover.
        let mut rays = Vec::with_capacity(CAST_RAYS + 1);
        for i in 0..=CAST_RAYS {
            let phi = -half_angle + 2.0 * half_angle * i as f32 / CAST_RAYS as f32;
            let (sin, cos) = phi.sin_cos();
            let dir = Vec2::new(base.x * cos - base.y * sin, base.x * sin + base.y * cos);
            let along = dist * cos;
            let half_chord = (r * r - (dist * sin) * (dist * sin)).max(0.0).sqrt();
            let enter = along - half_chord;
            rays.push(Ray {
                origin: eye,
                dir,
                enter,
                exit: (along + half_chord).max(enter + RIM_FEATHER),
            });
        }

        for i in 0..CAST_RAYS {
            let (a, b) = (rays[i], rays[i + 1]);
            // The terminator, in a few steps so the ramp curves instead of
            // reading as a flat wedge.
            for step in 0..TERMINATOR_STEPS {
                let s0 = step as f32 / TERMINATOR_STEPS as f32;
                let s1 = (step + 1) as f32 / TERMINATOR_STEPS as f32;
                let (a0, a1) = (umbra * smoothstep(s0), umbra * smoothstep(s1));
                self.quad(
                    tint,
                    [
                        (a.at(s0), a0),
                        (a.at(s1), a1),
                        (b.at(s1), a1),
                        (b.at(s0), a0),
                    ],
                );
            }
            // The shadow proper, from the far surface out past the arena.
            self.quad(
                tint,
                [
                    (a.at(1.0), umbra),
                    (eye + a.dir * (a.exit + FOG_FAR), umbra),
                    (eye + b.dir * (b.exit + FOG_FAR), umbra),
                    (b.at(1.0), umbra),
                ],
            );
        }

        // Penumbra down both flanks, opening up with distance past the cover.
        //
        // It has to *taper into* the silhouette, not begin there at full width
        // and full alpha. The grazing ray is still ramping through its own
        // terminator at that point, so a skirt that starts opaque hangs a
        // full-strength wing off the side of the cover, ahead of where its own
        // shadow has gone dark — which reads as a little peninsula of shadow on
        // each flank, and as a row of scallops along a cluster of bushes.
        let width_at = |past: f32| (BLUR_NEAR + BLUR_PER_UNIT * past) * blur;
        for (ray, outward) in [
            (rays[0], Vec2::new(rays[0].dir.y, -rays[0].dir.x)),
            (rays[CAST_RAYS], {
                let d = rays[CAST_RAYS].dir;
                Vec2::new(-d.y, d.x)
            }),
        ] {
            // On a grazing ray `enter` sits exactly on the silhouette, so the
            // skirt pinches to a point there and reaches full width only where
            // the terminator has finished.
            let rim = eye + ray.dir * ray.enter;
            let shoulder = eye + ray.dir * ray.exit;
            let out = eye + ray.dir * (ray.exit + FOG_FAR);
            let w_shoulder = outward * width_at(ray.exit - grazing);
            let w_out = outward * width_at(ray.exit - grazing + FOG_FAR);
            self.tri(tint, [(rim, 0.0), (shoulder, umbra), (shoulder + w_shoulder, 0.0)]);
            self.quad(
                tint,
                [
                    (shoulder, umbra),
                    (shoulder + w_shoulder, 0.0),
                    (out + w_out, 0.0),
                    (out, umbra),
                ],
            );
        }
    }

    fn tri(&mut self, tint: LinearRgba, corners: [(Vec2, f32); 3]) {
        let base = self.positions.len() as u32;
        for (p, alpha) in corners {
            self.vertex(p, tint, alpha);
        }
        self.indices.extend([base, base + 1, base + 2]);
    }

    fn quad(&mut self, tint: LinearRgba, corners: [(Vec2, f32); 4]) {
        let base = self.positions.len() as u32;
        for (p, alpha) in corners {
            self.vertex(p, tint, alpha);
        }
        self.indices
            .extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }

    fn vertex(&mut self, p: Vec2, tint: LinearRgba, alpha: f32) {
        self.positions.push([p.x, p.y, 0.0]);
        // Straight through: the shader returns this as-is and the pipeline
        // blends src_alpha / 1-src_alpha.
        self.colors.push([tint.red, tint.green, tint.blue, alpha]);
    }
}

/// One sight line through a piece of cover: where it enters and leaves.
#[derive(Copy, Clone)]
struct Ray {
    origin: Vec2,
    dir: Vec2,
    enter: f32,
    exit: f32,
}

impl Ray {
    /// The point a fraction `s` of the way through the cover.
    fn at(&self, s: f32) -> Vec2 {
        self.origin + self.dir * (self.enter + (self.exit - self.enter) * s)
    }
}

fn smoothstep(s: f32) -> f32 {
    s * s * (3.0 - 2.0 * s)
}
