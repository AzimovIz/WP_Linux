// A soft spotlight that follows the cursor -- everything outside its
// radius dims down, everything inside stays at full brightness.

struct ShaderEffectParams {
    cursor: vec2<f32>,
    time: f32,
    canvas_aspect: f32,
    u_radius: f32, // {"label": "Radius", "default": 0.3, "range": [0.05, 0.8]}
    u_softness: f32, // {"label": "Softness", "default": 0.4, "range": [0.05, 1.0]}
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
    let dist = distance(in.uv, params.cursor);
    let inner = params.u_radius * (1.0 - params.u_softness);
    let light = 1.0 - smoothstep(inner, params.u_radius, dist);
    let dim = mix(0.15, 1.0, light);
    return vec4<f32>(base.rgb * dim, base.a);
}
