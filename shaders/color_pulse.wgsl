// Pulses a layer's color between its own original color and a chosen
// tint, oscillating over time.

struct ShaderEffectParams {
    cursor: vec2<f32>,
    time: f32,
    canvas_aspect: f32,
    u_speed: f32, // {"label": "Speed", "default": 2.0, "range": [0.1, 10.0]}
    u_tint_r: f32, // {"label": "Tint red", "default": 1.0, "range": [0.0, 1.0]}
    u_tint_g: f32, // {"label": "Tint green", "default": 0.6, "range": [0.0, 1.0]}
    u_tint_b: f32, // {"label": "Tint blue", "default": 0.2, "range": [0.0, 1.0]}
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
    let base = textureSample(input_texture, input_sampler, in.uv);
    let tint = vec3<f32>(params.u_tint_r, params.u_tint_g, params.u_tint_b);
    let t = sin(params.time * params.u_speed) * 0.5 + 0.5;
    return vec4<f32>(mix(base.rgb, tint, t), base.a);
}
