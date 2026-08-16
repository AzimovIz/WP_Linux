// Fire-flicker effect for the generic Shader system (see
// project_format::parse_shader_params) -- displaces each row of pixels
// left/right by a sum of two sine waves at different frequency/phase, so
// the shift reads as an organic flicker rather than a rigid horizontal
// slide. Two exposed params: how far pixels move (intensity) and how fast
// the flicker animates (speed). Works on any image layer, not just
// flames -- the same technique also reads as heat-haze/water-shimmer at
// lower intensity.
//
// Sampling relies on the engine's default clamp-to-edge sampler (see
// `player`'s `scene-sampler`), so displaced edges stretch their edge
// pixel instead of wrapping -- no seam artifacts at the left/right
// border.

struct ShaderEffectParams {
    cursor: vec2<f32>,
    time: f32,
    canvas_aspect: f32,
    u_intensity: f32, // {"label": "Intensity", "default": 0.02, "range": [0.0, 0.15]}
    u_speed: f32, // {"label": "Speed", "default": 2.0, "range": [0.0, 10.0]}
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
    let t = params.time * params.u_speed;
    let wave = sin(in.uv.y * 18.0 + t) + sin(in.uv.y * 7.0 - t * 1.7) * 0.5;
    let offset = wave * params.u_intensity;
    let sample_uv = vec2<f32>(in.uv.x + offset, in.uv.y);
    return textureSample(input_texture, input_sampler, sample_uv);
}
