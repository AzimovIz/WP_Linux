// VHS tracking-error look: occasional scanline bands jitter sideways,
// plus a faint chroma split that scales with the same intensity.

struct ShaderEffectParams {
    cursor: vec2<f32>,
    time: f32,
    canvas_aspect: f32,
    u_intensity: f32, // {"label": "Intensity", "default": 0.02, "range": [0.0, 0.1]}
    u_speed: f32, // {"label": "Speed", "default": 3.0, "range": [0.0, 12.0]}
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
    let band = floor(in.uv.y * 60.0);
    let step_index = floor(params.time * params.u_speed);
    let band_active = step(0.9, hash21(vec2<f32>(band * 7.0, step_index)));
    let jitter = (hash21(vec2<f32>(band, step_index)) * 2.0 - 1.0) * params.u_intensity * band_active;

    let chroma = params.u_intensity * 0.3;
    let sample_uv = in.uv + vec2<f32>(jitter, 0.0);
    let r = textureSample(input_texture, input_sampler, sample_uv + vec2<f32>(chroma, 0.0)).r;
    let g = textureSample(input_texture, input_sampler, sample_uv).g;
    let b = textureSample(input_texture, input_sampler, sample_uv - vec2<f32>(chroma, 0.0)).b;
    let a = textureSample(input_texture, input_sampler, sample_uv).a;
    return vec4<f32>(r, g, b, a);
}
