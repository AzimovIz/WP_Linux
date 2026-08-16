// Detects edges in the image (a small central-difference gradient of
// luminance) and adds a pulsing glow color along them.

struct ShaderEffectParams {
    cursor: vec2<f32>,
    time: f32,
    canvas_aspect: f32,
    u_intensity: f32, // {"label": "Intensity", "default": 0.8, "range": [0.0, 2.0]}
    u_speed: f32, // {"label": "Speed", "default": 1.5, "range": [0.0, 6.0]}
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

fn luminance(c: vec4<f32>) -> f32 {
    return dot(c.rgb, vec3<f32>(0.299, 0.587, 0.114));
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let base = textureSample(input_texture, input_sampler, in.uv);
    let e = 0.0025;
    let lx = luminance(textureSample(input_texture, input_sampler, in.uv + vec2<f32>(e, 0.0)))
        - luminance(textureSample(input_texture, input_sampler, in.uv - vec2<f32>(e, 0.0)));
    let ly = luminance(textureSample(input_texture, input_sampler, in.uv + vec2<f32>(0.0, e)))
        - luminance(textureSample(input_texture, input_sampler, in.uv - vec2<f32>(0.0, e)));
    let edge = clamp(sqrt(lx * lx + ly * ly) * 6.0, 0.0, 1.0);
    let pulse = 0.5 + 0.5 * sin(params.time * params.u_speed);
    let glow_color = vec3<f32>(0.3, 0.8, 1.0);
    return vec4<f32>(base.rgb + glow_color * edge * params.u_intensity * pulse, base.a);
}
