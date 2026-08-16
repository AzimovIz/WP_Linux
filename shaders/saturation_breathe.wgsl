// Saturation oscillates between grayscale and boosted color.

struct ShaderEffectParams {
    cursor: vec2<f32>,
    time: f32,
    canvas_aspect: f32,
    u_intensity: f32, // {"label": "Intensity", "default": 0.8, "range": [0.0, 1.0]}
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
    let base = textureSample(input_texture, input_sampler, in.uv);
    let gray = dot(base.rgb, vec3<f32>(0.299, 0.587, 0.114));
    let saturation = 1.0 + sin(params.time * params.u_speed) * params.u_intensity;
    let color = vec3<f32>(gray, gray, gray) + (base.rgb - vec3<f32>(gray, gray, gray)) * saturation;
    return vec4<f32>(color, base.a);
}
