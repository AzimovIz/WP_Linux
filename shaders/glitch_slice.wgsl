// Horizontal-slice glitch: occasional bands of the image jump sideways,
// with a small RGB channel split for a broken-signal look.

struct ShaderEffectParams {
    cursor: vec2<f32>,
    time: f32,
    canvas_aspect: f32,
    u_intensity: f32, // {"label": "Intensity", "default": 0.03, "range": [0.0, 0.15]}
    u_speed: f32, // {"label": "Speed", "default": 4.0, "range": [0.0, 20.0]}
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
    let step_index = floor(params.time * params.u_speed);
    let band = floor(in.uv.y * 24.0);
    let band_rand = hash21(vec2<f32>(band, step_index));
    let is_glitching = step(0.8, hash21(vec2<f32>(step_index, 0.0)));
    let slice_offset = (band_rand * 2.0 - 1.0) * params.u_intensity * is_glitching;

    let split = params.u_intensity * 0.3 * is_glitching;
    let r = textureSample(input_texture, input_sampler, in.uv + vec2<f32>(slice_offset + split, 0.0)).r;
    let g = textureSample(input_texture, input_sampler, in.uv + vec2<f32>(slice_offset, 0.0)).g;
    let b = textureSample(input_texture, input_sampler, in.uv + vec2<f32>(slice_offset - split, 0.0)).b;
    let a = textureSample(input_texture, input_sampler, in.uv + vec2<f32>(slice_offset, 0.0)).a;
    return vec4<f32>(r, g, b, a);
}
