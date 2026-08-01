//! Gunfire you can hear but cannot see.
//!
//! Every round somebody else fires throws a wedge of yellow light on the ground
//! around your own soldier, pointing roughly where the shot came from: narrow
//! and strong up close, wide and faint far off. It is the only thing in the game
//! that reports something happening outside your line of sight, which is why it
//! exists at all — the concealment model is good enough that an enemy in deep
//! grass fifty units away is genuinely invisible, and a firefight you cannot
//! locate at all is not tense, it is arbitrary.
//!
//! **Render-only, like `vision.rs` and `nameplate.rs`, and for the same
//! reason.** What one pawn can hear is a fact about who is holding the phone,
//! and the sim has no point of view — every peer simulates every pawn, so an
//! arc computed in there would have to be computed for all of them and would be
//! rolled back and re-drawn as a side effect of somebody else's packet arriving.
//! Nothing here feeds back: the sim is read, never written.
//!
//! # The lie is the mechanic
//!
//! The arc does not point at the shooter. It points somewhere *within* an error
//! it announces, and the error is the whole design:
//!
//! * The wedge is drawn as a flat plateau with both rims fading out, so there is
//!   no bright centre line to read a bearing off. What it says is "in here".
//! * The centre is displaced from the truth by a uniform draw across
//!   [`OFFSET_SHARE`] of the half-angle — uniform because any peakier
//!   distribution makes the middle of the wedge the best guess again, which is
//!   the readout this is built to withhold. The share is under 1, so the shooter
//!   is always genuinely inside the arc: it is imprecise, never a lie.
//! * That displacement then WALKS, sinusoidally, over a few seconds — so a
//!   listener who stands still and collects six shots from one rifle cannot
//!   average the error away, which is exactly what independent per-shot draws
//!   would have let them do. The walk is per SOURCE rather than per shot for the
//!   same reason, and it re-draws itself only after that source has been quiet
//!   for [`QUIET_RESEED`]: within a burst the error drifts, between engagements
//!   it is a fresh guess.
//!
//! So closing the distance genuinely buys information — the arc narrows and its
//! error narrows with it, both proportional — and standing off does not.
//!
//! # Noticing a shot at all
//!
//! There is no event to subscribe to: a shot is a bullet entity and a cooldown,
//! both rollback state, and both get restored — an `Added<Bullet>` would fire
//! again every time the frame it spawned on is re-simulated, which in a p2p
//! match is most frames. So a shot is identified by the FRAME IT WAS FIRED ON,
//! recovered from the shooter's [`Cooldown`] (`frame - (FIRE_COOLDOWN - left)`),
//! and each source's last one is remembered. Rollback rewinds the frame counter
//! and the cooldown together, so the same shot recovers the same number however
//! often it is replayed, and a burst is a run of distinct ones.
//!
//! # See it
//!
//! `?scenario=gunfire` (or `AG_SCENARIO=gunfire`) is the arena with one pawn
//! standing in the middle of it firing a round a second, so the arcs can be
//! walked around instead of waited for.

use std::f32::consts::{PI, TAU};

use bevy::mesh::MeshVertexBufferLayoutRef;
use bevy::platform::collections::HashMap;
use bevy::prelude::*;
use bevy::render::render_resource::{
    AsBindGroup, RenderPipelineDescriptor, SpecializedMeshPipelineError,
};
use bevy::shader::ShaderRef;
use bevy::sprite_render::{AlphaMode2d, Material2d, Material2dKey, Material2dPlugin};
use bevy_ggrs::{LocalPlayers, RollbackFrameCount};

use army_ghosts_sim::{Cooldown, Player, Pos, FIRE_COOLDOWN};

use crate::spectate::Spectating;

/// Nothing further off than this is heard at all, world units. The arena is
/// 800x600, so this covers all of it and a shot from the far corner still says
/// something — just barely, and across most of a half-circle.
const HEAR_RANGE: f32 = 1000.0;
/// Inside this the arc is as tight as it gets: a rifle going off in your face
/// needs no help being located, and the last few units of closing should not
/// keep paying out.
const POINT_BLANK: f32 = 100.0;

/// The ring the wedge is drawn in, world units from the listener. The hole is
/// what the brief asked for and what makes the effect readable — a wedge that
/// touched the soldier would be read as something happening TO him, and it
/// would sit under the one sprite the player is actually watching. The outer
/// rim stays inside half the short side of a portrait phone, so an arc is never
/// clipped by the screen edge on the platform this game is for.
const RING_INNER: f32 = 74.0;
const RING_OUTER: f32 = 126.0;

/// Half-angle of the wedge at [`POINT_BLANK`] and at [`HEAR_RANGE`], radians —
/// so about 20 degrees across when it is next to you and 130 when it is the
/// length of the map away.
///
/// It opens with the SQUARE ROOT of the distance, not with the distance: the
/// arena is 800x600 and almost every shot anybody fires in it lands in the
/// first third of that range, so a straight line spends most of its travel on
/// distances that never happen and leaves every real engagement reading as
/// "about here". Under a root, 300 units — which is a long shot in this game —
/// is already a 70-degree wedge.
const SPREAD_NEAR: f32 = 0.17;
const SPREAD_FAR: f32 = 1.14;

/// Brightness at [`POINT_BLANK`] and at [`HEAR_RANGE`], before the shot starts
/// fading. Falls off as the square of the remaining range, which is much closer
/// to how a bang actually reads than a straight line: a shot at half the map
/// away is faint, not half-bright. Both ends are deliberately well short of
/// opaque — this is a light thrown on the grass, and at full strength it would
/// read as a wall rather than as a glow.
const LOUD_NEAR: f32 = 0.78;
const LOUD_FAR: f32 = 0.12;

/// How long one shot stays on screen, seconds, and how much of that is the
/// snap up to full. The rise is nearly instant and the fall is slow — the same
/// asymmetry `Aim::sway` is built on, for the same reason: a bang arrives all
/// at once and its memory does not.
const PING_LIFE: f32 = 1.4;
const PING_ATTACK: f32 = 0.05;

/// How much of the half-angle the drawn centre may be displaced by. Under 1 on
/// purpose: the shooter is always inside the arc that gets drawn, so the wedge
/// is imprecise rather than dishonest, and there is always a sliver of arc on
/// the far side of the truth to make that visible.
const OFFSET_SHARE: f32 = 0.72;
/// Seconds for the displacement to walk one full circuit of its sine, picked
/// per source. "A few seconds" — long enough that it reads as drift rather than
/// as a wobble, short enough to have moved noticeably by the second shot.
const WALK_PERIOD: (f32, f32) = (3.5, 6.5);
/// How long a source has to have been silent before its error is re-drawn from
/// scratch rather than carrying on walking. Inside this, successive shots are
/// one engagement and share one drifting error.
const QUIET_RESEED: f32 = 2.5;

/// Muzzle yellow — warm and pale rather than saturated, because it is drawn over
/// dark olive sward and a deep yellow at low alpha turns muddy green.
const SOUND_COLOR: Color = Color::srgb(1.0, 0.87, 0.42);
/// Above the fog (5.0). A sound cue that got dimmed by the very fog it exists to
/// see through would be at its faintest exactly where it is needed.
const Z_SOUND: f32 = 6.0;

/// The wedge, drawn per pixel — see `assets/sound.wgsl` for the shape and for
/// why it is a shader rather than a fan of triangles.
#[derive(Asset, TypePath, AsBindGroup, Clone)]
pub struct SoundMaterial {
    #[uniform(0)]
    tint: LinearRgba,
    /// `(half-angle, inner rim as a fraction of the outer, intensity, grain)`.
    #[uniform(1)]
    arc: Vec4,
}

impl Material2d for SoundMaterial {
    fn vertex_shader() -> ShaderRef {
        "sound.wgsl".into()
    }

    fn fragment_shader() -> ShaderRef {
        "sound.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode2d {
        AlphaMode2d::Blend
    }

    fn specialize(
        descriptor: &mut RenderPipelineDescriptor,
        layout: &MeshVertexBufferLayoutRef,
        _key: Material2dKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        // Position alone: the shader wants the raw local point (the quad runs
        // -1..1) and computes everything else from it, so there is nothing for a
        // uv or a vertex color to carry.
        descriptor.vertex.buffers =
            vec![layout.0.get_layout(&[Mesh::ATTRIBUTE_POSITION.at_shader_location(0)])?];
        Ok(())
    }
}

pub struct SoundPlugin;

impl Plugin for SoundPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(Material2dPlugin::<SoundMaterial>::default())
            .init_resource::<Heard>();
    }
}

/// The unit quad every arc is drawn on: 2x2 in local space, so the shader's
/// radius is `length(local)` and a `Transform` scale is the outer radius. One
/// mesh for every ping there will ever be — only the material differs.
#[derive(Resource)]
pub struct PingQuad(Handle<Mesh>);

pub fn setup_sound(mut commands: Commands, mut meshes: ResMut<Assets<Mesh>>) {
    commands.insert_resource(PingQuad(meshes.add(Rectangle::new(2.0, 2.0))));
}

/// Where one source's bearing error currently sits, and how fast it is walking.
///
/// `phase` is an angle on a sine whose amplitude is the whole error budget, so
/// `offset()` is a signed share of it. Seeding by `asin` rather than by picking
/// a phase directly is what makes the FIRST reading of a fresh error uniform —
/// `sin` of a uniform phase is not uniform, it piles up at the extremes, which
/// would put the shooter near the rim of the arc far more often than in it.
#[derive(Copy, Clone, Debug)]
struct Wander {
    phase: f32,
    rate: f32,
    /// Seconds since this source was last heard from.
    quiet: f32,
}

impl Wander {
    /// A fresh error of `start` (a share in -1..=1), walking at one circuit per
    /// `period` seconds. `backward` reflects the phase so the walk can set off
    /// either way from the same starting error.
    fn seeded(start: f32, period: f32, backward: bool) -> Self {
        let phase = start.clamp(-1.0, 1.0).asin();
        Self {
            phase: if backward { PI - phase } else { phase },
            rate: TAU / period,
            quiet: 0.0,
        }
    }

    fn advance(&mut self, dt: f32) {
        // Wrapped rather than left to grow: `sin` of a big f32 loses precision,
        // and this ticks for as long as the tab is open.
        self.phase = (self.phase + self.rate * dt).rem_euclid(TAU);
        self.quiet += dt;
    }

    fn offset(&self) -> f32 {
        self.phase.sin()
    }
}

/// What this client remembers about other people's gunfire. Render-only state:
/// nothing in here is rolled back, checksummed or sent anywhere.
#[derive(Resource)]
pub struct Heard {
    /// Per source: the frame its most recently noticed shot was fired on.
    shots: HashMap<usize, i32>,
    /// Per source: the bearing error every arc of theirs is currently drawn with.
    wander: HashMap<usize, Wander>,
    /// Client-side dice. Deliberately NOT the sim's — this is a fact about one
    /// screen, and two peers seeing different arcs is not a desync, it is two
    /// people standing in different places.
    rng: u32,
}

impl Default for Heard {
    fn default() -> Self {
        Self { shots: HashMap::new(), wander: HashMap::new(), rng: 0x5EED_1234 }
    }
}

impl Heard {
    /// Uniform in 0..1.
    fn unit(&mut self) -> f32 {
        self.rng = self.rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.rng >> 8) as f32 / (1u32 << 24) as f32
    }

    /// Uniform in -1..1.
    fn signed(&mut self) -> f32 {
        self.unit() * 2.0 - 1.0
    }
}

/// One shot, on screen. `source` is where it was fired from rather than a
/// bearing, so the arc keeps pointing at the PLACE as the listener walks — which
/// is what a player expects of a noise that has already happened, and what makes
/// walking two paces and looking again worth doing.
#[derive(Component)]
pub struct Ping {
    source: Vec2,
    from: usize,
    /// How far off the truth this arc's centre may be dragged — a share of the
    /// wedge's own half-angle, so it is fixed at the range the bang happened at
    /// along with everything else distance decides.
    limit: f32,
    loud: f32,
    age: f32,
}

/// Half-angle of the wedge for a shot this far off, radians.
fn spread(dist: f32) -> f32 {
    let t = ((dist - POINT_BLANK) / (HEAR_RANGE - POINT_BLANK)).clamp(0.0, 1.0);
    SPREAD_NEAR + (SPREAD_FAR - SPREAD_NEAR) * t.sqrt()
}

/// How strongly it is drawn, before the fade.
fn loudness(dist: f32) -> f32 {
    let t = ((dist - POINT_BLANK) / (HEAR_RANGE - POINT_BLANK)).clamp(0.0, 1.0);
    let near = (1.0 - t) * (1.0 - t);
    LOUD_FAR + (LOUD_NEAR - LOUD_FAR) * near
}

/// How much of it is left after `age` seconds.
fn envelope(age: f32) -> f32 {
    if age >= PING_LIFE {
        return 0.0;
    }
    let attack = (age / PING_ATTACK).clamp(0.0, 1.0);
    let left = 1.0 - age / PING_LIFE;
    attack * left * left
}

/// Whose ears these are: whoever the camera is on. Deliberately the same choice
/// `render::camera_follow` makes rather than `MatchRoom::me` — the arcs are
/// drawn around the pawn on screen, and hearing for one soldier while looking
/// through another's eyes would read as the effect being broken.
fn listening(local: Option<&LocalPlayers>, spectating: &Spectating) -> Option<usize> {
    spectating.watching.or_else(|| local?.0.first().copied())
}

fn at(pos: &Pos) -> Vec2 {
    let (x, y) = pos.to_f32();
    Vec2::new(x, y)
}

/// Notice everyone else's shots and throw an arc for each.
#[allow(clippy::too_many_arguments)]
pub fn hear_gunfire(
    mut commands: Commands,
    mut heard: ResMut<Heard>,
    frame: Option<Res<RollbackFrameCount>>,
    quad: Option<Res<PingQuad>>,
    mut materials: ResMut<Assets<SoundMaterial>>,
    pawns: Query<(&Player, &Pos, &Cooldown)>,
    local: Option<Res<LocalPlayers>>,
    spectating: Res<Spectating>,
) {
    let (Some(frame), Some(quad)) = (frame, quad) else {
        return;
    };
    let Some(me) = listening(local.as_deref(), &spectating) else {
        return;
    };
    let Some(here) = pawns.iter().find(|(p, ..)| p.handle == me).map(|(_, pos, _)| at(pos))
    else {
        return;
    };

    for (player, pos, cooldown) in &pawns {
        // A cooldown of zero says only "not recently", which is every pawn
        // standing about; the window this can be seen through is the
        // FIRE_COOLDOWN ticks after the trigger, which at 60Hz is a fifth of a
        // second of frames to catch it in.
        if player.handle == me || cooldown.0 == 0 {
            continue;
        }
        let fired_on = frame.0 - (FIRE_COOLDOWN as i32 - cooldown.0 as i32);
        // Same shot as last frame — or the same shot again after a rollback
        // re-simulated the frame it was fired on, which is the case this whole
        // scheme exists for.
        if heard.shots.insert(player.handle, fired_on) == Some(fired_on) {
            continue;
        }
        let source = at(pos);
        let dist = here.distance(source);
        if dist > HEAR_RANGE {
            continue;
        }

        // Carry on walking the error this source already had, unless they have
        // been quiet long enough for this to be a new engagement.
        let fresh = heard.wander.get(&player.handle).is_none_or(|w| w.quiet > QUIET_RESEED);
        if fresh {
            let start = heard.signed();
            let period = WALK_PERIOD.0 + heard.unit() * (WALK_PERIOD.1 - WALK_PERIOD.0);
            let backward = heard.unit() < 0.5;
            heard
                .wander
                .insert(player.handle, Wander::seeded(start, period, backward));
        } else if let Some(walk) = heard.wander.get_mut(&player.handle) {
            walk.quiet = 0.0;
        }

        let half = spread(dist);
        let grain = heard.unit() * 64.0;
        commands.spawn((
            Ping {
                source,
                from: player.handle,
                limit: half * OFFSET_SHARE,
                loud: loudness(dist),
                age: 0.0,
            },
            Mesh2d(quad.0.clone()),
            MeshMaterial2d(materials.add(SoundMaterial {
                tint: SOUND_COLOR.to_linear(),
                arc: Vec4::new(half, RING_INNER / RING_OUTER, 0.0, grain),
            })),
            Transform::from_xyz(here.x, here.y, Z_SOUND)
                .with_scale(Vec3::new(RING_OUTER, RING_OUTER, 1.0)),
        ));
    }
}

/// Age every arc, keep it centred on the listener and pointed down its
/// (wandering) bearing, and take it away when it has faded out.
#[allow(clippy::too_many_arguments)]
pub fn fade_pings(
    mut commands: Commands,
    time: Res<Time>,
    mut heard: ResMut<Heard>,
    mut materials: ResMut<Assets<SoundMaterial>>,
    mut pings: Query<(Entity, &mut Ping, &mut Transform, &MeshMaterial2d<SoundMaterial>)>,
    pawns: Query<(&Player, &Pos)>,
    local: Option<Res<LocalPlayers>>,
    spectating: Res<Spectating>,
) {
    let dt = time.delta_secs();
    for walk in heard.wander.values_mut() {
        walk.advance(dt);
    }
    let here = listening(local.as_deref(), &spectating)
        .and_then(|me| pawns.iter().find(|(p, _)| p.handle == me))
        .map(|(_, pos)| at(pos));

    for (entity, mut ping, mut transform, material) in &mut pings {
        ping.age += dt;
        let level = ping.loud * envelope(ping.age);
        if level <= 0.0 {
            // The material is one asset per arc and nothing else refers to it,
            // so it goes when the arc does.
            materials.remove(material.0.id());
            commands.entity(entity).despawn();
            continue;
        }
        let Some(here) = here else { continue };
        let drift = heard.wander.get(&ping.from).map_or(0.0, Wander::offset);
        transform.translation = here.extend(Z_SOUND);
        transform.rotation =
            Quat::from_rotation_z((ping.source - here).to_angle() + ping.limit * drift);
        if let Some(material) = materials.get_mut(material.0.id()) {
            material.arc.z = level;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The trade the whole cue is built on: walking toward gunfire tells you
    /// more about where it is, and it has to do that on both counts at once —
    /// a narrower wedge that got fainter would be a downgrade dressed as one.
    #[test]
    fn closing_the_distance_narrows_the_arc_and_brightens_it() {
        let mut last = (f32::MAX, f32::MIN);
        for dist in [40.0, 120.0, 250.0, 500.0, 900.0, HEAR_RANGE] {
            let (half, loud) = (spread(dist), loudness(dist));
            assert!(half >= last.0 || dist <= POINT_BLANK, "the arc narrowed at {dist}");
            assert!(loud <= last.1 || dist <= POINT_BLANK, "a further shot was louder at {dist}");
            last = (half, loud);
        }
        assert!(spread(40.0) < spread(900.0) / 3.0, "point blank is not tight enough to act on");
    }

    /// **The shooter is always inside the arc.** The cue is allowed to be vague
    /// and is not allowed to be wrong: an arc that could point somewhere the
    /// shot did not come from would teach players to ignore it.
    #[test]
    fn the_shot_is_always_somewhere_inside_the_arc() {
        let mut heard = Heard::default();
        for dist in [10.0, 100.0, 333.0, HEAR_RANGE] {
            let half = spread(dist);
            for _ in 0..500 {
                let walk = Wander::seeded(heard.signed(), 4.0, false);
                // The worst the walk can ever do, not just where it starts.
                for step in 0..200 {
                    let mut walk = walk;
                    walk.advance(step as f32 * 0.05);
                    let error = (half * OFFSET_SHARE * walk.offset()).abs();
                    assert!(error < half, "at {dist} the arc pointed {error} off a {half} wedge");
                }
            }
        }
    }

    /// A fresh error is uniform — anything peakier and the middle of the wedge
    /// becomes the best guess again, which is the one reading this withholds —
    /// and it then walks, so standing still and collecting a burst does not
    /// average it away.
    #[test]
    fn the_error_starts_uniform_and_then_walks() {
        for start in [-0.99, -0.5, 0.0, 0.37, 0.99] {
            for backward in [false, true] {
                let walk = Wander::seeded(start, 4.0, backward);
                assert!(
                    (walk.offset() - start).abs() < 1e-5,
                    "seeded at {start} and started at {}",
                    walk.offset()
                );
            }
        }

        // Uniform in the aggregate, not just at the ends: with a sine seeded by
        // phase instead of by `asin`, the outer fifths hold ~29% of the draws
        // each and the middle fifth ~13%.
        let mut heard = Heard::default();
        let mut bins = [0usize; 5];
        for _ in 0..20_000 {
            let start = Wander::seeded(heard.signed(), 4.0, false).offset();
            bins[(((start + 1.0) / 2.0 * 5.0) as usize).min(4)] += 1;
        }
        for (bin, count) in bins.iter().enumerate() {
            assert!(
                (3400..4600).contains(count),
                "bin {bin} took {count} of 20000 draws, which is not uniform: {bins:?}"
            );
        }

        // …and a second and a half later it is somewhere else.
        let mut walk = Wander::seeded(0.0, 4.0, false);
        walk.advance(1.5);
        assert!(walk.offset().abs() > 0.5, "the error barely moved: {}", walk.offset());
    }

    /// The fade: instant on the shot, gone by the end of its life, and never
    /// louder later than it was earlier.
    #[test]
    fn a_shot_arrives_at_once_and_fades_out() {
        assert_eq!(envelope(0.0), 0.0);
        assert!(envelope(PING_ATTACK) > 0.9);
        assert_eq!(envelope(PING_LIFE), 0.0);
        let mut last = f32::MAX;
        for step in 1..=60 {
            let level = envelope(PING_ATTACK + step as f32 * 0.02);
            assert!(level <= last, "the shot got louder as it aged");
            last = level;
        }
    }
}
