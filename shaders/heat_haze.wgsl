// Shimmering heat-haze distortion -- small, turbulent offsets in both
// axes built from a handful of mismatched sine waves, unlike the purely
// horizontal single-axis wobble of `fire_flicker.wgsl`/`wind_sway.wgsl`.

struct ShaderEffectParams {
    cursor: vec2<f32>,
    time: f32,
    canvas_aspect: f32,
    u_intensity: f32, // {"label": "Intensity", "default": 0.008, "range": [0.0, 0.05]}
    u_speed: f32, // {"label": "Speed", "default": 3.0, "range": [0.0, 10.0]}
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
    let offset_x = sin(in.uv.y * 25.0 + t) + sin(in.uv.x * 17.0 - t * 1.3) * 0.6;
    let offset_y = cos(in.uv.x * 22.0 + t * 0.8) * 0.6;
    let offset = vec2<f32>(offset_x, offset_y) * params.u_intensity;
    return textureSample(input_texture, input_sampler, in.uv + offset);
}
