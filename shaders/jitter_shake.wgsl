// Whole-frame camera-shake: a new random offset every ~1/speed seconds
// (stepped, not smoothly interpolated -- that's what reads as a "shake"
// rather than a wobble).

struct ShaderEffectParams {
    cursor: vec2<f32>,
    time: f32,
    canvas_aspect: f32,
    u_intensity: f32, // {"label": "Intensity", "default": 0.01, "range": [0.0, 0.06]}
    u_speed: f32, // {"label": "Speed", "default": 6.0, "range": [0.5, 24.0]}
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

// Cheap deterministic hash (Dave Hoskins' `hash21`), reused by every
// shader in this library that needs pseudo-randomness -- each file is a
// standalone asset (no shared WGSL modules across files), so it's
// duplicated rather than imported.
fn hash21(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.x, p.y, p.x) * 0.1031);
    p3 = p3 + dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let step_index = floor(params.time * params.u_speed);
    let rand_x = hash21(vec2<f32>(step_index, step_index * 1.37));
    let rand_y = hash21(vec2<f32>(step_index * 2.13, step_index));
    let offset = (vec2<f32>(rand_x, rand_y) * 2.0 - 1.0) * params.u_intensity;
    return textureSample(input_texture, input_sampler, in.uv + offset);
}
