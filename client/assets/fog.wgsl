// Line-of-sight fog (see client/src/vision.rs).
//
// Vertex colors carry the tint and the front-to-back terminator; UV carries the
// sideways falloff — `uv.x` is the lateral fraction across the shadow (-1 and
// +1 are its two boundaries) and `uv.y` is how much of that half width should
// be gradient. Doing the falloff here rather than per vertex means the edge can
// be as tight as we like without the mesh needing a matching ray count.
//
// This exists at all because bevy's stock `ColorMaterial` gates vertex colors
// behind a `VERTEX_COLORS` shader def that is decided once, when the entity is
// first specialized — and a mesh that grows its color attribute later never
// re-specializes, so the def stays off and every shadow renders flat.
// Declaring the locations unconditionally here (with a matching vertex layout
// forced in `FogMaterial::specialize`) sidesteps that entirely.

#import bevy_sprite::mesh2d_functions::{get_world_from_local, mesh2d_position_local_to_clip}

struct Vertex {
    @builtin(instance_index) instance_index: u32,
    @location(0) position: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(4) color: vec4<f32>,
};

struct FogVertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) uv: vec2<f32>,
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
    out.uv = vertex.uv;
    return out;
}

@fragment
fn fragment(in: FogVertexOutput) -> @location(0) vec4<f32> {
    // Feather inward from the boundary: zero exactly on it, full one gradient
    // width inside. Never outward, or the shadow bleeds onto lit ground.
    let edge = max(in.uv.y, 0.0001);
    let k = clamp((1.0 - abs(in.uv.x)) / edge, 0.0, 1.0);
    return vec4<f32>(in.color.rgb, in.color.a * k * k * (3.0 - 2.0 * k));
}
