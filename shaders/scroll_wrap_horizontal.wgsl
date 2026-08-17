// Continuous horizontal pixel scroll with seamless wraparound, looped
// within a chosen [u_start, u_start + u_length] slice of the image on the
// x axis rather than the full 0..1 canvas -- meant to be masked (Texture
// mask painted over a line/wire) so only that slice's pixels move, e.g.
// "beads" sliding along a straight data-flow line drawn on the image.
// Sign of `u_speed` picks the direction: positive scrolls right-to-left,
// negative scrolls left-to-right. Pixels outside [u_start, u_start +
// u_length] are unaffected (pass through unchanged), so u_start/u_length
// only need to loosely bound the line -- the mask does the real clipping.

struct ShaderEffectParams {
    cursor: vec2<f32>,
    time: f32,
    canvas_aspect: f32,
    u_speed: f32, // {"label": "Speed", "default": 0.15, "range": [-2.0, 2.0]}
    u_start: f32, // {"label": "Segment start", "default": 0.0, "range": [0.0, 1.0]}
    u_length: f32, // {"label": "Segment length", "default": 1.0, "range": [0.01, 1.0]}
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
    let local = (in.uv.x - params.u_start) / params.u_length;
    let looped = fract(local + params.time * params.u_speed);
    let scroll_x = params.u_start + looped * params.u_length;
    return textureSample(input_texture, input_sampler, vec2<f32>(scroll_x, in.uv.y));
}
