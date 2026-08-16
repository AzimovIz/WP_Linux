// Falling rain streaks -- each vertical column gets its own speed and
// phase so the streaks don't all fall in lockstep.

struct ShaderEffectParams {
    cursor: vec2<f32>,
    time: f32,
    canvas_aspect: f32,
    u_intensity: f32, // {"label": "Intensity", "default": 0.5, "range": [0.0, 1.0]}
    u_speed: f32, // {"label": "Speed", "default": 1.5, "range": [0.1, 6.0]}
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
    let base = textureSample(input_texture, input_sampler, in.uv);
    let columns = 70.0;
    let col = floor(in.uv.x * columns);
    let col_rand = hash21(vec2<f32>(col, 0.0));
    let column_active = step(0.5, hash21(vec2<f32>(col, 1.0)));
    let fall_speed = 0.3 + col_rand * 0.7;
    let y = fract(in.uv.y - params.time * params.u_speed * fall_speed + col_rand * 10.0);
    let head = smoothstep(0.0, 0.05, y) * (1.0 - smoothstep(0.05, 0.35, y));
    let streak_color = vec3<f32>(0.6, 0.7, 0.9);
    return vec4<f32>(base.rgb + streak_color * head * column_active * params.u_intensity, base.a);
}
