// Mirrors the image into rotating radial wedges around its center.
// `segments` is rounded to the nearest whole wedge count.

struct ShaderEffectParams {
    cursor: vec2<f32>,
    time: f32,
    canvas_aspect: f32,
    u_segments: f32, // {"label": "Segments", "default": 6.0, "range": [2.0, 16.0]}
    u_speed: f32, // {"label": "Speed", "default": 0.5, "range": [0.0, 3.0]}
};

@group(0) @binding(0) var input_texture: texture_2d<f32>;
@group(0) @binding(1) var input_sampler: sampler;
@group(0) @binding(2) var<uniform> params: ShaderEffectParams;

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
    let center = vec2<f32>(0.5, 0.5);
    let delta = in.uv - center;
    let radius = length(delta);
    let segments = max(round(params.u_segments), 2.0);
    let wedge = 6.283185307 / segments;
    var angle = atan2(delta.y, delta.x) + params.time * params.u_speed;
    angle = abs((angle % wedge) - wedge * 0.5);
    let sample_uv = center + vec2<f32>(cos(angle), sin(angle)) * radius;
    return textureSample(input_texture, input_sampler, sample_uv);
}
