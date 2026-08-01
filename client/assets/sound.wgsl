// A gunshot you heard but did not see (see client/src/sound.rs).
//
// One quad per shot, centred on the listener and rotated so +x local points
// down the bearing the sound is CLAIMED to have come from. Everything that
// makes it a slice of pie with the middle cut out happens here, per pixel,
// because the thing being drawn is a soft glow: doing it with geometry would
// need a fan fine enough to hide its own facets and would still band across the
// gradient.
//
// The local quad runs -1..1 on both axes, so radius is `length(local)` and 1.0
// is the outer rim. `arc` carries the rest:
//   x  half-angle of the sector, radians
//   y  inner rim, as a fraction of the outer (the hole the listener stands in)
//   z  intensity — distance and how long ago, multiplied together in the client
//   w  a per-shot grain seed, so two arcs on top of each other do not moiré

#import bevy_sprite::mesh2d_functions::{get_world_from_local, mesh2d_position_local_to_clip}

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> tint: vec4<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var<uniform> arc: vec4<f32>;

/// Half of pi, for the bell below.
const HALF_PI: f32 = 1.5707964;
/// How much of the radial band is spent fading in off the inner rim and out at
/// the outer one. Two soft edges rather than a ring with a hard cut.
const RIM_FADE: f32 = 0.34;
/// Grain: cells across the quad, and how far the value swings either side of 1.
const GRAIN_CELLS: f32 = 9.0;
const GRAIN_SWING: f32 = 0.45;

fn hash21(p: vec2<f32>) -> f32 {
    var q = fract(p * vec2<f32>(123.34, 345.45));
    q += dot(q, q + 34.345);
    return fract(q.x * q.y);
}

/// Value noise: hashed lattice, smoothstepped between. The same trick the sim
/// builds its grass field out of, in floats because nothing here is simulated.
fn noise(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);
    return mix(
        mix(hash21(i), hash21(i + vec2<f32>(1.0, 0.0)), u.x),
        mix(hash21(i + vec2<f32>(0.0, 1.0)), hash21(i + vec2<f32>(1.0, 1.0)), u.x),
        u.y,
    );
}

struct SoundVertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) local: vec2<f32>,
};

struct Vertex {
    @builtin(instance_index) instance_index: u32,
    @location(0) position: vec3<f32>,
};

@vertex
fn vertex(vertex: Vertex) -> SoundVertexOutput {
    var out: SoundVertexOutput;
    let world_from_local = get_world_from_local(vertex.instance_index);
    out.clip_position = mesh2d_position_local_to_clip(
        world_from_local,
        vec4<f32>(vertex.position, 1.0),
    );
    out.local = vertex.position.xy;
    return out;
}

@fragment
fn fragment(in: SoundVertexOutput) -> @location(0) vec4<f32> {
    let inner = arc.y;
    let radius = length(in.local);
    // The band, as 0..1 across the annulus. Outside it there is nothing at all —
    // including the hole in the middle, which is what keeps the glow off the
    // soldier it is being drawn around.
    let band = (radius - inner) / max(1.0 - inner, 0.001);
    if (band <= 0.0 || band >= 1.0) {
        return vec4<f32>(0.0);
    }
    let radial = smoothstep(0.0, RIM_FADE, band) * (1.0 - smoothstep(1.0 - RIM_FADE, 1.0, band));

    // Local +x IS the bearing being claimed — the quad is rotated to it — so
    // the angle off centre is just the atan2 of the local point.
    //
    // **THE BELL. This half-cosine is `sound.rs`'s `bell()` and the two are one
    // curve written twice** — there is no way to share the code across the
    // shader boundary. It is not a taste in falloff: the client draws the
    // bearing error from a distribution shaped exactly like this, so what is
    // painted here IS the likelihood of every bearing in the wedge. Change one
    // and the picture starts lying about the odds; `the_arc_is_the_probability_
    // it_looks_like` in sound.rs is what notices.
    let off = abs(atan2(in.local.y, in.local.x)) / max(arc.x, 0.001);
    let sector = select(0.0, max(cos(off * HALF_PI), 0.0), off < 1.0);

    // …and a little grain over the top, in LOCAL space so it turns with the
    // sector instead of crawling across it as the arc wanders.
    let grain = 1.0 + GRAIN_SWING * (noise(in.local * GRAIN_CELLS + arc.w) - 0.5) * 2.0;

    return vec4<f32>(tint.rgb, tint.a * arc.z * radial * sector * grain);
}
