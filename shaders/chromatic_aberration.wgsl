// RGB channel split that grows toward the edges of the frame and breathes
// in strength over time.

struct ShaderEffectParams {
    cursor: vec2<f32>,
    time: f32,
    canvas_aspect: f32,
    u_intensity: f32, // {"label": "Intensity", "default": 0.015, "range": [0.0, 0.06]}
    u_speed: f32, // {"label": "Speed", "default": 1.0, "range": [0.0, 5.0]}
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
    let delta = in.uv - vec2<f32>(0.5, 0.5);
    let pulse = 0.5 + 0.5 * sin(params.time * params.u_speed);
    let shift = delta * params.u_intensity * pulse;
    let r = textureSample(input_texture, input_sampler, in.uv + shift).r;
    let g = textureSample(input_texture, input_sampler, in.uv).g;
    let b = textureSample(input_texture, input_sampler, in.uv - shift).b;
    let a = textureSample(input_texture, input_sampler, in.uv).a;
    return vec4<f32>(r, g, b, a);
}
