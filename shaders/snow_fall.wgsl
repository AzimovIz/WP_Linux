// Soft falling snow -- two independent layers at different scales/speeds
// for a sense of depth (near flakes bigger and faster, far flakes
// smaller and slower).

struct ShaderEffectParams {
    cursor: vec2<f32>,
    time: f32,
    canvas_aspect: f32,
    u_intensity: f32, // {"label": "Intensity", "default": 0.7, "range": [0.0, 1.0]}
    u_speed: f32, // {"label": "Speed", "default": 1.0, "range": [0.1, 4.0]}
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

fn snow_layer(uv: vec2<f32>, scale: f32, fall_speed: f32, drift: f32, time: f32) -> f32 {
    let moved = vec2<f32>(uv.x + sin(time * 0.5 + uv.y * 10.0) * drift, uv.y - time * fall_speed);
    let cell = floor(moved * scale);
    let local = fract(moved * scale) - vec2<f32>(0.5, 0.5);
    let r = hash21(cell);
    let flake_pos = vec2<f32>(r - 0.5, 0.0) * 0.6;
    let d = length(local - flake_pos);
    let present = step(0.85, hash21(cell + vec2<f32>(7.0, 7.0)));
    return smoothstep(0.22, 0.0, d) * present;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let base = textureSample(input_texture, input_sampler, in.uv);
    let near = snow_layer(in.uv, 18.0, 0.12 * params.u_speed, 0.02, params.time);
    let far = snow_layer(in.uv, 34.0, 0.06 * params.u_speed, 0.015, params.time) * 0.6;
    let snow = clamp(near + far, 0.0, 1.0) * params.u_intensity;
    return vec4<f32>(base.rgb + vec3<f32>(1.0, 1.0, 1.0) * snow, base.a);
}
