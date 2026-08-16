// A vortex twist centered on the canvas, its strength fading out toward
// the edges and oscillating back and forth over time.

struct ShaderEffectParams {
    cursor: vec2<f32>,
    time: f32,
    canvas_aspect: f32,
    u_intensity: f32, // {"label": "Intensity", "default": 1.5, "range": [0.0, 6.0]}
    u_speed: f32, // {"label": "Speed", "default": 1.0, "range": [0.0, 4.0]}
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
    let falloff = 1.0 - smoothstep(0.0, 0.6, radius);
    let angle = params.u_intensity * falloff * sin(params.time * params.u_speed);
    let s = sin(angle);
    let c = cos(angle);
    let rotated = vec2<f32>(delta.x * c - delta.y * s, delta.x * s + delta.y * c);
    return textureSample(input_texture, input_sampler, center + rotated);
}
