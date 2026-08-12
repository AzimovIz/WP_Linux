// Separable 9-tap Gaussian blur -- one direction per pass. Unlike
// Vignette/ColorAdjust (one pass straight into the layer's scratch
// texture), a Blur effect needs its own internal two-pass ping-pong
// before the generic mask-blend step ever runs: a horizontal pass
// (reads the layer's accumulator, writes the chain's dedicated
// `blur_temp` texture) followed by a vertical pass (reads `blur_temp`,
// writes scratch) -- see `record_draw`'s `LoadedEffectKind::Blur`
// handling. This shader itself doesn't know which pass it's running;
// `direction` is (1, 0) or (0, 1), baked into each pass's own uniform
// buffer at load/update time.

struct BlurParams {
    radius: f32, // {"label": "Radius", "default": 0.02, "range": [0.0, 0.1]}
    direction_x: f32,
    direction_y: f32,
    _pad0: f32,
};

@group(0) @binding(0) var input_texture: texture_2d<f32>;
@group(0) @binding(1) var input_sampler: sampler;
@group(0) @binding(2) var<uniform> params: BlurParams;

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
    let dir = vec2<f32>(params.direction_x, params.direction_y);
    var color = vec4<f32>(0.0);
    var total_weight = 0.0;
    // Fixed 9-tap kernel spanning the full `radius` (in UV units) on
    // either side of the center texel -- sigma chosen so the kernel's
    // own edges are close to zero weight instead of visibly clipping.
    for (var i = -4; i <= 4; i++) {
        let t = f32(i) / 4.0;
        let weight = exp(-(t * t) / (2.0 * 0.4 * 0.4));
        let offset = dir * params.radius * t;
        color += textureSample(input_texture, input_sampler, in.uv + offset) * weight;
        total_weight += weight;
    }
    return color / total_weight;
}
