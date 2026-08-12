// Darkens the corners of whatever's already been drawn for this layer
// (the accumulator texture from an earlier pass in its effect chain --
// see `mask_blend.wgsl` and `record_draw`'s effect-chain loop for the
// two-pass-per-effect shape this is one half of). Writes its raw,
// unmasked result -- masking is a separate, generic pass shared by every
// effect kind, not something this shader knows about.

struct VignetteParams {
    strength: f32, // {"label": "Strength", "default": 0.5, "range": [0.0, 1.0]}
    softness: f32, // {"label": "Softness", "default": 0.5, "range": [0.0, 1.0]}
    _pad0: f32,
    _pad1: f32,
};

@group(0) @binding(0) var input_texture: texture_2d<f32>;
@group(0) @binding(1) var input_sampler: sampler;
@group(0) @binding(2) var<uniform> params: VignetteParams;

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
    let color = textureSample(input_texture, input_sampler, in.uv);

    // Distance from center in UV space -- 0 at the middle, ~0.7071
    // (sqrt(0.5^2 + 0.5^2)) at any corner, regardless of the canvas's
    // aspect ratio (a vignette following the picture's own shape, not
    // corrected into a perfect circle, is the standard look).
    let dist = distance(in.uv, vec2<f32>(0.5, 0.5));
    let corner_dist = 0.70710678;
    let inner_edge = params.softness * corner_dist;
    let falloff = smoothstep(inner_edge, corner_dist, dist);
    let brightness = clamp(1.0 - falloff * params.strength, 0.0, 1.0);

    return vec4<f32>(color.rgb * brightness, color.a);
}
