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
//! # The arc IS the probability distribution
//!
//! The wedge does not point at the shooter. It points somewhere within an error
//! it announces — and **what it is drawn with is the likelihood of every bearing
//! inside it**. The brightness at an angle is proportional to the chance the shot
//! really came from that angle, so a player reading the picture the obvious way
//! (bright means probably, faint means possibly, dark means no) is reading it
//! correctly. Three parts make that true together:
//!
//! * **The opacity across the wedge is a bell** — [`bell`], a half-cosine that is
//!   full at the centre and reaches exactly zero at both rims, so the arc has no
//!   edge to be startled by and no direction it claims that it does not mean.
//! * **The error is distributed as that same bell** ([`toward_the_middle`]). This
//!   is a REVERSAL of what shipped first, and worth understanding rather than
//!   just reading: the first version drew a flat plateau and displaced it
//!   uniformly, on the argument that a peak in the middle would hand the player
//!   back the bearing the cue exists to withhold. That is true of a peak the
//!   statistics do not support. Make the two agree instead and the display stops
//!   being a bluff: the centre really is the best guess, the wedge really is how
//!   wide the doubt is, and nothing is being hidden — the information is simply
//!   imprecise, which is the honest thing for a noise heard through a field.
//! * **The wedge's half-angle IS the error's whole budget.** There is no separate
//!   share of it held back, because the bell already vanishes at the rim: the
//!   shot is always inside the arc, and the extremes of the arc are exactly the
//!   bearings it says are barely possible.
//!
//! # Where the error comes from
//!
//! **It is a function of the OFFSET between the two pawns, not of the clock**
//! ([`error_at`]). It is not random and it does not tick: a plain triangle wave
//! over the relative position, so the error is fixed while neither of you moves
//! and slides as soon as either of you does, in any direction. A listener who
//! stands still and collects six shots from one rifle gets six arcs that agree —
//! the error is a property of where the two of you are standing, and the way to
//! change it is to move. Independent per-shot draws are what this replaced, and
//! they were worse in both directions: they let a stationary listener average
//! the error away, and they made a stationary shooter's arcs jitter for no
//! reason a player could see.
//!
//! The triangle sweeps EVENLY, which is what makes the bell above come out
//! right: even sweep in, [`toward_the_middle`] out, and the density that falls
//! out the far side is `bell` exactly rather than approximately. Read the other
//! way round, which is how it was asked for: the drawn bearing moves SLOWLY
//! through the angles the bell says are likely and hurries through the ones it
//! says are not, because the rate is one over the density by construction.
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

use std::f32::consts::{FRAC_2_PI, FRAC_PI_2, TAU};

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
/// what makes the effect readable — a wedge that touched the soldier would be
/// read as something happening TO him, and it would sit under the one sprite the
/// player is actually watching. The rim stays inside half the short side of a
/// portrait phone, so an arc is never clipped by the screen edge on the platform
/// this game is for.
///
/// It is a THIN band on purpose: a fat one is a blob, and what the eye reads off
/// a blob is its bulk rather than its bearing. Five units of arc is a stroke, and
/// a stroke has a direction.
const RING_INNER: f32 = 47.5;
const RING_OUTER: f32 = 52.5;

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

/// How long one shot stays on screen, seconds, and how much of that is the snap
/// up to full. It is a FLASH — a bang and a muzzle flare are momentary, and an
/// arc that lingered would still be sitting there when the next round arrived,
/// so a burst would read as one continuous glow instead of as gunfire. The rise
/// is nearly instant and the fall takes the rest, which is the asymmetry
/// `Aim::sway` is built on for the same reason: a bang arrives all at once and
/// its memory does not.
const PING_LIFE: f32 = 0.4;
const PING_ATTACK: f32 = 0.04;

/// How far the two of you have to move RELATIVE TO EACH OTHER, in x and in y,
/// for the bearing error to run through its whole range — world units. At
/// walking pace (120 units a second) that is about a second of movement for a
/// full sweep, so a couple of paces visibly shifts the arc and standing still
/// does not shift it at all.
///
/// The two are deliberately incommensurate: equal pitches would make the error
/// constant along the diagonal, which is a direction a player could learn.
const ERROR_PITCH: (f32, f32) = (150.0, 95.0);

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

/// **The one curve.** How opaque the wedge is `share` of the way from its centre
/// to its rim, AND — the whole point — how likely the shot is to have come from
/// that bearing. Full in the middle, exactly zero at ±1, no edge either way.
///
/// **`sound.wgsl` draws this same curve and there is no way to make it share the
/// code**, so the two are one constant written twice: a half-cosine. If it
/// changes here it must change there, and `the_arc_is_the_probability_it_looks_
/// like` is what catches the day it doesn't — it histograms the error field and
/// holds the result against this function.
/// Nothing at runtime calls this — the shader is what paints the curve, and the
/// client only ever needs its inverse CDF. It exists so the shape has an
/// executable copy on this side of the boundary for the test to hold the error
/// field against, which is the only check that the two halves still agree.
#[allow(dead_code)]
fn bell(share: f32) -> f32 {
    // Clamped up off zero because `cos(FRAC_PI_2)` is -4.4e-8 in f32 and a
    // negative opacity is not a thing; the shader clamps for the same reason.
    (FRAC_PI_2 * share.clamp(-1.0, 1.0)).cos().max(0.0)
}

/// A triangle wave of `phase`, -1..=1 and swept EVENLY.
///
/// A triangle and not a sine, and everything below depends on it: an even sweep
/// is a uniform variable, which is the one input [`toward_the_middle`] can turn
/// into a known distribution. `sin` of the same phase is already arcsine-shaped
/// and would pile the error up at the rim — the exact opposite of what the bell
/// wants. It is continuous, so the error never jumps.
fn triangle(phase: f32) -> f32 {
    4.0 * ((phase / TAU + 0.25).rem_euclid(1.0) - 0.5).abs() - 1.0
}

/// Bend an evenly swept `-1..=1` into one distributed as [`bell`].
///
/// This is the inverse CDF of the half-cosine, which is exactly `asin` scaled —
/// the one line that makes the picture honest, since a uniform input through it
/// comes out with density `cos(pi/2 u)`, the curve the wedge is painted with.
///
/// Equivalently, and this is how it was asked for: **the drawn bearing crawls
/// through the middle of the wedge and hurries through the edges**, because
/// `d(out)/d(in)` here is one over the bell. It spends its time where the light
/// is, so the light is where it spends its time.
fn toward_the_middle(even: f32) -> f32 {
    even.clamp(-1.0, 1.0).asin() * FRAC_2_PI
}

/// How far off the truth an arc points, as a signed share of the wedge's own
/// half-angle (-1..=1), for a shooter this much [`Pos`] away from the listener.
///
/// A plain plane wave over the RELATIVE position rather than a clock or a die:
/// two pawns standing still see the same error however many rounds go off, and
/// any movement by either of them — theirs or yours, along any axis — slides it.
/// The player-facing consequence is that moving is what resolves a bearing,
/// which is the same bargain everything else in this game makes.
fn error_at(offset: Vec2) -> f32 {
    let phase = TAU * (offset.x / ERROR_PITCH.0 + offset.y / ERROR_PITCH.1);
    toward_the_middle(triangle(phase))
}

/// What this client remembers about other people's gunfire: which shot of theirs
/// it has already thrown an arc for, and nothing else — the error itself is a
/// function of where the two of you are standing and is not stored anywhere.
/// Render-only state: nothing in here is rolled back, checksummed or sent.
#[derive(Resource, Default)]
pub struct Heard {
    /// Per source: the frame its most recently noticed shot was fired on.
    shots: HashMap<usize, i32>,
}

/// One shot, on screen. `source` is where it was fired from rather than a
/// bearing, so the arc keeps pointing at the PLACE as the listener walks — which
/// is what a player expects of a noise that has already happened, and what makes
/// walking two paces and looking again worth doing.
#[derive(Component)]
pub struct Ping {
    source: Vec2,
    /// The wedge's half-angle, radians — fixed at the range the bang happened
    /// at, along with everything else distance decides. It is BOTH how wide the
    /// arc is drawn and the whole budget the error is allowed to spend, because
    /// the bell that fades the rim is the same curve the error is drawn from.
    half: f32,
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
    // The fall is measured from the END of the rise, not from the shot, so the
    // arc reaches its full strength rather than whatever is left of it after the
    // attack has been spent. At a life this short that is the difference between
    // a flash and a smudge — 0.81 of full, in the version this replaced.
    let left = ((PING_LIFE - age) / (PING_LIFE - PING_ATTACK)).clamp(0.0, 1.0);
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

        let half = spread(dist);
        // The grain is the only thing left that wants to differ between one arc
        // and the next, and the frame it was fired on is a perfectly good
        // arbitrary number — so there are no dice anywhere in this module.
        let grain = fired_on.rem_euclid(97) as f32 * 0.37;
        commands.spawn((
            Ping {
                source,
                half,
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

/// Age every arc, keep it centred on the listener and pointed down the bearing
/// its offset says to draw, and take it away when it has faded out.
pub fn fade_pings(
    mut commands: Commands,
    time: Res<Time>,
    mut materials: ResMut<Assets<SoundMaterial>>,
    mut pings: Query<(Entity, &mut Ping, &mut Transform, &MeshMaterial2d<SoundMaterial>)>,
    pawns: Query<(&Player, &Pos)>,
    local: Option<Res<LocalPlayers>>,
    spectating: Res<Spectating>,
) {
    let dt = time.delta_secs();
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
        // Both halves of the bearing are recomputed from where the two of you
        // are RIGHT NOW: the direction to the place the shot came from, and the
        // error the offset between you happens to carry. So walking two paces
        // swings the arc twice over — once because the place moved round you,
        // and once because you are somewhere else in the error field.
        let offset = ping.source - here;
        transform.translation = here.extend(Z_SOUND);
        transform.rotation =
            Quat::from_rotation_z(offset.to_angle() + ping.half * error_at(offset));
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

    /// Every offset a pawn could stand at, a hex apart, out to the whole arena
    /// and past it.
    fn offsets() -> impl Iterator<Item = Vec2> {
        (-60..=60).flat_map(|ix| {
            (-60..=60).map(move |iy| Vec2::new(ix as f32 * 16.0, iy as f32 * 16.0))
        })
    }

    /// **The shooter is always inside the arc.** The cue is allowed to be vague
    /// and is not allowed to be wrong: an arc that could point somewhere the
    /// shot did not come from would teach players to ignore it. Checked over
    /// every offset rather than a sample, since the error is a field and a field
    /// can be checked exhaustively.
    #[test]
    fn the_shot_is_always_somewhere_inside_the_arc() {
        for offset in offsets() {
            let error = error_at(offset);
            assert!(
                error.abs() <= 1.0,
                "the error field left its own range at {offset}: {error}"
            );
            for dist in [10.0, 100.0, 333.0, HEAR_RANGE] {
                let half = spread(dist);
                let off = (half * error).abs();
                assert!(off <= half, "at {dist} the arc pointed {off} off a {half} wedge");
            }
        }
        // The budget IS the wedge now, so the rim is reachable — and it has to
        // be drawn as impossible when it is, which is the bell's job.
        assert_eq!(bell(1.0), 0.0);
        assert_eq!(bell(-1.0), 0.0);
    }

    /// **What is painted is the likelihood.** The wedge's opacity across its own
    /// width and the distribution of the bearing error have to be the SAME
    /// curve, or the picture is quietly lying about the odds — and they are
    /// written twice, once here and once in `sound.wgsl`, with nothing but this
    /// holding them together.
    ///
    /// Histogram the error over every offset on the map, and each bin must hold
    /// the share of the field that `bell` says it should.
    #[test]
    fn the_arc_is_the_probability_it_looks_like() {
        const BINS: usize = 20;
        let mut seen = [0usize; BINS];
        let mut total = 0usize;
        for offset in offsets() {
            let bin = ((error_at(offset) + 1.0) / 2.0 * BINS as f32) as usize;
            seen[bin.min(BINS - 1)] += 1;
            total += 1;
        }
        // What the curve claims, normalised over the same bins.
        let middle = |bin: usize| (bin as f32 + 0.5) / BINS as f32 * 2.0 - 1.0;
        let want: Vec<f32> = (0..BINS).map(|bin| bell(middle(bin))).collect();
        let scale: f32 = want.iter().sum();
        for bin in 0..BINS {
            let (got, expected) = (seen[bin] as f32 / total as f32, want[bin] / scale);
            assert!(
                (got - expected).abs() < 0.006,
                "bin {bin} ({:.2}) holds {got:.4} of the field, but the bell it is \
                 painted with says {expected:.4} — the picture and the odds have come apart",
                middle(bin)
            );
        }
        // …and the shape really is a bell rather than the flat plateau this
        // replaced: the middle fifth must outweigh the outer one substantially.
        let share = |lo: usize, hi: usize| seen[lo..hi].iter().sum::<usize>() as f32;
        assert!(
            share(8, 12) > 2.0 * share(0, 4),
            "the error is not concentrated in the middle: {seen:?}"
        );
    }

    /// The error is a FIELD OVER THE OFFSET, not a clock and not a die: it holds
    /// still while the two of you do, and slides as soon as either of you moves
    /// in any direction.
    #[test]
    fn the_error_is_a_function_of_where_the_two_of_you_stand() {
        // Standing still: the tenth round from one rifle is drawn exactly where
        // the first was. This is what makes the arc a reading of the ground
        // rather than a flicker to be averaged out.
        let offset = Vec2::new(-137.0, 61.0);
        assert_eq!(error_at(offset), error_at(offset));

        // …and it does not matter WHICH of you moved, since only the offset is
        // read: a step by the shooter and the opposite step by the listener are
        // the same event as far as this is concerned.
        let (source, here, step) =
            (Vec2::new(120.0, -40.0), Vec2::new(-30.0, 55.0), Vec2::new(23.0, -14.0));
        assert_eq!(error_at((source + step) - here), error_at(source - (here - step)));

        // A pace in ANY direction moves it, diagonals included — the two pitches
        // are incommensurate precisely so there is no heading a player could
        // walk to keep the error where it is. Averaged over the field rather
        // than sampled at one point: this is a smooth field, so it has
        // stationary points, and hitting one is not a bug — a whole direction
        // being flat would be.
        for step in [
            Vec2::X,
            Vec2::NEG_X,
            Vec2::Y,
            Vec2::NEG_Y,
            Vec2::new(1.0, 1.0).normalize(),
            Vec2::new(1.0, -1.0).normalize(),
        ] {
            let moved = 30.0 * step;
            let mut sum = 0.0;
            let mut n = 0.0;
            for offset in offsets() {
                sum += (error_at(offset + moved) - error_at(offset)).abs();
                n += 1.0;
            }
            let mean = sum / n;
            assert!(mean > 0.2, "walking {step} 30 units barely moved the error: mean {mean}");
        }

        // The PHASE is what sweeps evenly, and it has to, or the bell the error
        // is bent into comes out as something else — the shape of the result is
        // checked by `the_arc_is_the_probability_it_looks_like`, and this is the
        // input it depends on.
        let mut bins = [0usize; 5];
        let mut total = 0usize;
        for offset in offsets() {
            let phase = TAU * (offset.x / ERROR_PITCH.0 + offset.y / ERROR_PITCH.1);
            bins[(((triangle(phase) + 1.0) / 2.0 * 5.0) as usize).min(4)] += 1;
            total += 1;
        }
        for (bin, count) in bins.iter().enumerate() {
            let share = *count as f32 / total as f32;
            assert!(
                (0.17..0.23).contains(&share),
                "fifth {bin} covers {share} of the field, so the sweep is not even: {bins:?}"
            );
        }
    }

    /// The fade: instant on the shot, gone by the end of its life, and never
    /// louder later than it was earlier.
    #[test]
    fn a_shot_arrives_at_once_and_fades_out() {
        assert_eq!(envelope(0.0), 0.0);
        assert!((envelope(PING_ATTACK) - 1.0).abs() < 1e-6, "the arc never reaches full");
        assert_eq!(envelope(PING_LIFE), 0.0);
        let mut last = f32::MAX;
        for step in 1..=60 {
            let level = envelope(PING_ATTACK + step as f32 * 0.02);
            assert!(level <= last, "the shot got louder as it aged");
            last = level;
        }
    }
}
