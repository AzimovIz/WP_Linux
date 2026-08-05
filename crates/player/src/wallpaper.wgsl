struct Uniforms {
    resolution: vec2<f32>,
    mouse: vec2<f32>,
    time: f32,
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
};

@group(0) @binding(0)
var<uniform> u: Uniforms;

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
    let frag_coord = in.uv * u.resolution;
    let mouse_dist = distance(frag_coord, u.mouse) / max(u.resolution.x, u.resolution.y);
    let glow = smoothstep(0.35, 0.0, mouse_dist);

    let wave = 0.5 + 0.5 * sin(u.time * 0.6 + in.uv.x * 6.0 + in.uv.y * 3.0);
    let base = vec3<f32>(0.05, 0.08, 0.16) + vec3<f32>(0.10, 0.05, 0.25) * wave;
    let color = base + vec3<f32>(0.9, 0.6, 0.2) * glow;

    return vec4<f32>(color, 1.0);
}
