// Draws a single picture panned by `params.offset` (already eased and
// clamped to `[-strength, strength]` on the Rust side) and pre-zoomed by
// `params.zoom` so the pan never samples outside the source picture --
// see `zoom_for_strength` in lib.rs for the exact margin math.

struct ParallaxParams {
    offset: vec2<f32>,
    zoom: f32,
    _pad: f32,
};

@group(0) @binding(0) var scene_texture: texture_2d<f32>;
@group(0) @binding(1) var scene_sampler: sampler;
@group(0) @binding(2) var<uniform> params: ParallaxParams;

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let pos = positions[vertex_index];
    var out: VertexOutput;
    out.clip_position = vec4<f32>(pos, 0.0, 1.0);
    out.uv = vec2<f32>(pos.x * 0.5 + 0.5, 0.5 - pos.y * 0.5);
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let uv = (in.uv - vec2<f32>(0.5, 0.5)) / params.zoom + vec2<f32>(0.5, 0.5) + params.offset;
    return textureSample(scene_texture, scene_sampler, uv);
}
