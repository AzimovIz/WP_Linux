// Brightness/contrast/saturation, single pass -- writes its raw,
// unmasked result into the layer's scratch texture (see `vignette.wgsl`'s
// module doc comment; masking is a separate, generic pass shared by
// every effect kind, not something this shader knows about).

struct ColorAdjustParams {
    brightness: f32, // {"label": "Brightness", "default": 0.0, "range": [-1.0, 1.0]}
    contrast: f32,   // {"label": "Contrast", "default": 0.0, "range": [-1.0, 1.0]}
    saturation: f32, // {"label": "Saturation", "default": 0.0, "range": [-1.0, 1.0]}
    _pad0: f32,
};

@group(0) @binding(0) var input_texture: texture_2d<f32>;
@group(0) @binding(1) var input_sampler: sampler;
@group(0) @binding(2) var<uniform> params: ColorAdjustParams;

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
    let color = textureSample(input_texture, input_sampler, in.uv);

    // Brightness: additive. Contrast: scale around the midpoint (0.5)
    // so 0 contrast is a no-op and negative contrast flattens towards
    // gray instead of towards black. Saturation: mix towards
    // Rec.709 luma; `1.0 + saturation` so 0 is a no-op, -1 is
    // grayscale, and above 0 boosts saturation past the source.
    var rgb = color.rgb + params.brightness;
    rgb = (rgb - 0.5) * (1.0 + params.contrast) + 0.5;
    let luma = dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    rgb = mix(vec3<f32>(luma), rgb, 1.0 + params.saturation);

    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), color.a);
}
