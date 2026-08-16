// Old-projector look: brightness flicker plus animated grain -- two
// independent params so each can be dialed in separately.

struct ShaderEffectParams {
    cursor: vec2<f32>,
    time: f32,
    canvas_aspect: f32,
    u_flicker: f32, // {"label": "Flicker", "default": 0.15, "range": [0.0, 0.5]}
    u_grain: f32, // {"label": "Grain", "default": 0.08, "range": [0.0, 0.3]}
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

fn hash21(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.x, p.y, p.x) * 0.1031);
    p3 = p3 + dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let base = textureSample(input_texture, input_sampler, in.uv);
    let flicker_wave = sin(params.time * 37.0) * 0.5 + hash21(vec2<f32>(floor(params.time * 24.0), 1.0)) * 0.5;
    let brightness = 1.0 - params.u_flicker * 0.5 * (0.5 + 0.5 * flicker_wave);
    let grain = (hash21(in.uv * 800.0 + params.time * 60.0) - 0.5) * params.u_grain;
    return vec4<f32>(base.rgb * brightness + grain, base.a);
}
