// The ground sward and the tufts standing in it (see client/src/grass.rs).
//
// Both are static meshes built once and drawn in one call each; the tufts are a
// mesh rather than ~1300 sprites because none of them ever move, and this is a
// phone-first game where per-frame sprite extraction is the scarce resource.
//
// The ground mesh: the texture is the grass detail, tiled
// in WORLD space (uv = world / GRASS_TEX_PX, so the tile never stretches with
// the mesh), and the vertex color is the per-area tint sampled from the sim's
// `grass_height`: rgb drifts from dry pale to lush, alpha is how completely the
// grass covers the dirt tile underneath. Interpolating that across the grid is
// what makes the transitions between areas smooth for free.
//
// Same reason as `fog.wgsl` for not using `ColorMaterial`: it gates vertex
// colors behind a shader def decided at first specialization, and a mesh that
// gains its color attribute later never re-specializes. The vertex layout is
// forced in `GrassMaterial::specialize`.
//
// It is also where the WIND happens, and that is not a decoration: see
// `client/src/wind.rs` for why foliage has to stir on its own, and for the
// field this file transcribes. The tufts are baked meshes that never change —
// which is the whole reason they are a mesh and not a million sprites — so the
// vertex shader is the only place they can be made to lean.

#import bevy_sprite::mesh2d_functions::{get_world_from_local, mesh2d_position_local_to_clip}

// `params` is (octave cross, sward ripple in uv, seconds, wind bearing):
//   x  how much of the second, finer octave to cross in: the ground sward
//      wants it (see the fragment shader), the tuft sheet — whose quads carry
//      real UVs into an atlas — must not have it at all.
//   y  how far the ground texture slides with the wind, in UV. Zero on the
//      tufts for the same atlas reason: sliding those samples the neighbour.
//   z  the clock.
//   w  which way the wind is blowing, radians. It is a function of the clock
//      and nothing else, so the CPU works it out once a frame rather than
//      every vertex working it out again — which also keeps `VEER_SWING` out
//      of this file entirely. Both written by `wind::drive_grass`.
@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> params: vec4<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var grass_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var grass_sampler: sampler;

// ── Wind ─────────────────────────────────────────────────────────────────────
// **`client/src/wind.rs` is the authority and this is a transcription** — the
// same split as `sound.rs`'s `bell` and `sound.wgsl`'s, for the same reason:
// there is no sharing code across the shader boundary, and the bushes are
// sprites the CPU moves while the grass is a mesh the GPU moves. The SHAPE
// below has to be read against that file by a human. The NUMBERS do not:
// `wind::tests::the_shader_runs_the_same_wind` reads this file and holds every
// one of them against its Rust twin, so the two can never drift in tuning.
const WIND_DIR_X: f32 = 0.9553;
const WIND_DIR_Y: f32 = -0.2955;
const GUST_CELL: f32 = 260.0;
const GUST_PERIOD: f32 = 11.0;
const GUST_LO: f32 = 0.15;
const GUST_HI: f32 = 0.44;
const FINE_CELL: f32 = 52.0;
const FINE_PERIOD: f32 = 2.4;
const FINE_DRIFT: f32 = 55.0;
const FINE_SHARE: f32 = 0.84;
const GUST_FLOOR: f32 = 0.05;
const LATTICE: f32 = 256.0;
const SLICE_X: f32 = 113.7;
const SLICE_Y: f32 = 271.3;
// The projection foreshortening (`grass::GRASS_VERT`), so a blade blown north
// travels less far up the screen than one blown east travels across it.
const GRASS_VERT: f32 = 0.6428;

fn hash21(p: vec2<f32>) -> f32 {
    var q = fract(p * vec2<f32>(123.34, 345.45));
    q += dot(q, q + 34.345);
    return fract(q.x * q.y);
}

/// Fold a lattice coordinate into `0..LATTICE`. The wrap is what lets the wind
/// run for hours: the fine term's sample point travels downwind forever, and
/// this hash multiplies by ~123 before taking a fractional part, so unbounded
/// coordinates run out of mantissa and the noise goes banded. See `wind.rs`.
fn wind_wrap(x: f32) -> f32 {
    return x - LATTICE * floor(x / LATTICE);
}

fn wind_noise(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);
    let a = hash21(vec2<f32>(wind_wrap(i.x), wind_wrap(i.y)));
    let b = hash21(vec2<f32>(wind_wrap(i.x + 1.0), wind_wrap(i.y)));
    let c = hash21(vec2<f32>(wind_wrap(i.x), wind_wrap(i.y + 1.0)));
    let d = hash21(vec2<f32>(wind_wrap(i.x + 1.0), wind_wrap(i.y + 1.0)));
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}

/// Value noise in three dimensions, as two 2D slices eased between — the third
/// axis being time. It is what stops the field being frozen turbulence merely
/// carried downwind: a gust can fade where it stands and another get up out of
/// nothing, so a lull is worth waiting for rather than worth calculating.
fn wind_noise3(p: vec2<f32>, t: f32) -> f32 {
    let i = floor(t);
    let f = t - i;
    let u = f * f * (3.0 - 2.0 * f);
    let step = vec2<f32>(SLICE_X, SLICE_Y);
    let a = wind_noise(p + step * wind_wrap(i));
    let b = wind_noise(p + step * wind_wrap(i + 1.0));
    return mix(a, b, u);
}

/// How far downwind whatever stands at `p` is leaning at time `t` — **0 for
/// upright, 1 for flat out, and never negative**, because air does not push
/// things upwind. `grain` is this clump's own offset into the fine term, in
/// cells, so neighbours catch the same gust differently.
///
/// The predecessor was a travelling sine, which spent half of every cycle
/// leaning blades INTO the wind and was reported from play as waves of water.
/// Two terms now, multiplied: a slow broad one saying whether it is blowing
/// here at all, and a fine quick one saying which blades are catching it.
fn wind_lean(p: vec2<f32>, t: f32, grain: f32) -> f32 {
    let prevailing = vec2<f32>(WIND_DIR_X, WIND_DIR_Y);
    // `gusting`, not `patch`, and only because **`patch` is a RESERVED KEYWORD
    // in WGSL** — naga refuses the whole file over it, and since a shader is
    // only compiled when something first draws with it, the report is a blank
    // sward at runtime rather than anything a build or a test would say.
    let gusting = smoothstep(GUST_LO, GUST_HI, wind_noise3(p / GUST_CELL, t / GUST_PERIOD));
    // Folded into one lattice period before use, which is exactly seamless
    // because the field repeats.
    let travelled = fract(FINE_DRIFT * t / (LATTICE * FINE_CELL)) * LATTICE;
    let dapple = wind_noise3(
        p / FINE_CELL - prevailing * travelled + vec2<f32>(grain, -grain),
        t / FINE_PERIOD,
    );
    let caught = 1.0 - FINE_SHARE + FINE_SHARE * dapple;
    return GUST_FLOOR + (1.0 - GUST_FLOOR) * gusting * caught;
}

struct Vertex {
    @builtin(instance_index) instance_index: u32,
    @location(0) position: vec3<f32>,
    @location(2) uv: vec2<f32>,
    // How this vertex takes the wind, baked in at `grass::tuft_bands`:
    //   x  px it travels at a full gust — the clump's height at its tip, zero
    //      at its root, which is what makes a clump bend rather than slide
    //   y  this clump's grain, in fine cells
    // The ground sward carries zeroes: it is a flat sheet with nothing to bend,
    // and it ripples in the fragment shader instead.
    @location(3) sway: vec2<f32>,
    @location(4) color: vec4<f32>,
};

struct GrassVertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) lean: f32,
    @location(3) downwind: vec2<f32>,
};

@vertex
fn vertex(vertex: Vertex) -> GrassVertexOutput {
    var out: GrassVertexOutput;
    let world_from_local = get_world_from_local(vertex.instance_index);
    let world = (world_from_local * vec4<f32>(vertex.position, 1.0)).xy;
    let w = wind_lean(world, params.z, vertex.sway.y);
    // Downwind at this instant, y foreshortened by the same projection the
    // sheets are drawn at. The bearing wanders (`wind::bearing`) and arrives
    // ready-made in `params.w`.
    let downwind = vec2<f32>(cos(params.w), sin(params.w) * GRASS_VERT);
    let shift = downwind * (w * vertex.sway.x);
    let leaned = vec3<f32>(vertex.position.x + shift.x, vertex.position.y + shift.y, vertex.position.z);
    out.clip_position = mesh2d_position_local_to_clip(world_from_local, vec4<f32>(leaned, 1.0));
    out.color = vertex.color;
    out.uv = vertex.uv;
    out.lean = w;
    out.downwind = downwind;
    return out;
}

@fragment
fn fragment(in: GrassVertexOutput) -> @location(0) vec4<f32> {
    // The sward slides a hair with the wind. Not trying to be blades — it is
    // the thatch seen from almost straight down — but a still backdrop under
    // moving tufts is exactly the still background this whole thing exists to
    // deny. `params.y` is zero on the tuft sheet, whose UVs are an atlas.
    // Note the flipped y: the field's v runs `-world.y / GRASS_TEX_PX`.
    let drift = vec2<f32>(in.downwind.x, -in.downwind.y) * (in.lean * params.y);
    let uv = in.uv + drift;

    // Two crossed octaves of the same tile, at incommensurate scales, so the
    // 128px repeat doesn't read as a grid over an 800px arena. The second one
    // is FINER, not coarser: magnifying the tile turns its blades into soft
    // blobs, which is worse than the tiling it was meant to hide.
    let near = textureSample(grass_texture, grass_sampler, uv);
    let fine = textureSample(grass_texture, grass_sampler, uv * -2.31 + vec2(0.37, 0.19));
    let sward = mix(near, fine, params.x);
    return vec4<f32>(sward.rgb * in.color.rgb, sward.a * in.color.a);
}
