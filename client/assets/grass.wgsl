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

#import bevy_sprite::mesh2d_functions::{get_world_from_local, mesh2d_position_local_to_clip}

// `params.x` is how much of the second, finer octave to cross in: the ground
// sward wants it (see the fragment shader), the tuft sheet — whose quads carry
// real UVs into an atlas — must not have it at all.
@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> params: vec4<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var grass_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var grass_sampler: sampler;

struct Vertex {
    @builtin(instance_index) instance_index: u32,
    @location(0) position: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(4) color: vec4<f32>,
};

struct GrassVertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) uv: vec2<f32>,
};

@vertex
fn vertex(vertex: Vertex) -> GrassVertexOutput {
    var out: GrassVertexOutput;
    let world_from_local = get_world_from_local(vertex.instance_index);
    out.clip_position = mesh2d_position_local_to_clip(
        world_from_local,
        vec4<f32>(vertex.position, 1.0),
    );
    out.color = vertex.color;
    out.uv = vertex.uv;
    return out;
}

@fragment
fn fragment(in: GrassVertexOutput) -> @location(0) vec4<f32> {
    // Two crossed octaves of the same tile, at incommensurate scales, so the
    // 128px repeat doesn't read as a grid over an 800px arena. The second one
    // is FINER, not coarser: magnifying the tile turns its blades into soft
    // blobs, which is worse than the tiling it was meant to hide.
    let near = textureSample(grass_texture, grass_sampler, in.uv);
    let fine = textureSample(grass_texture, grass_sampler, in.uv * -2.31 + vec2(0.37, 0.19));
    let sward = mix(near, fine, params.x);
    return vec4<f32>(sward.rgb * in.color.rgb, sward.a * in.color.a);
}
