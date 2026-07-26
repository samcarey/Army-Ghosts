// Line-of-sight fog (see client/src/vision.rs).
//
// Per-tile shading, interpolated: each hex is a flat plateau of its own value
// with a rim blending to values shared with its neighbours, so the fog is
// quantized to the tile grid — what's hidden reads as a place on the map — but
// without hard-edged polygons. All of that lives in the vertex colors; this
// shader just passes them through.
//
// This exists at all because bevy's stock `ColorMaterial` gates vertex colors
// behind a `VERTEX_COLORS` shader def that is decided once, when the entity is
// first specialized — and a mesh that grows its color attribute later never
// re-specializes, so the def stays off and every tile renders flat at the
// material color. Declaring the location unconditionally here (with a matching
// vertex layout forced in `FogMaterial::specialize`) sidesteps that entirely.

#import bevy_sprite::mesh2d_functions::{get_world_from_local, mesh2d_position_local_to_clip}

struct Vertex {
    @builtin(instance_index) instance_index: u32,
    @location(0) position: vec3<f32>,
    @location(4) color: vec4<f32>,
};

struct FogVertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vertex(vertex: Vertex) -> FogVertexOutput {
    var out: FogVertexOutput;
    let world_from_local = get_world_from_local(vertex.instance_index);
    out.clip_position = mesh2d_position_local_to_clip(
        world_from_local,
        vec4<f32>(vertex.position, 1.0),
    );
    out.color = vertex.color;
    return out;
}

@fragment
fn fragment(in: FogVertexOutput) -> @location(0) vec4<f32> {
    return in.color;
}
