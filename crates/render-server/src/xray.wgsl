// Draws a base picture with a second ("overlay") picture only visible in
// a circle around the cursor. `params.cursor` and the fragment's
// `clip_position` are both in canvas pixel coordinates, so the mask
// radius stays a circle regardless of the canvas aspect ratio.

struct XrayParams {
    cursor: vec2<f32>,
    radius: f32,
    _pad: f32,
};

@group(0) @binding(0) var base_texture: texture_2d<f32>;
@group(0) @binding(1) var overlay_texture: texture_2d<f32>;
@group(0) @binding(2) var tex_sampler: sampler;
@group(0) @binding(3) var<uniform> params: XrayParams;

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
    let base = textureSample(base_texture, tex_sampler, in.uv);
    let overlay = textureSample(overlay_texture, tex_sampler, in.uv);
    let dist = distance(in.clip_position.xy, params.cursor);
    let mask = 1.0 - smoothstep(params.radius * 0.85, params.radius, dist);
    return mix(base, overlay, mask);
}
