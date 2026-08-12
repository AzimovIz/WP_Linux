// Generic mask application, shared by every effect kind -- not something
// each effect's own shader (e.g. vignette.wgsl) implements itself. See
// `Ideas.md`, "Развилка 2": masking is engine infrastructure, decoupled
// from what a specific effect actually does.
//
// Reads only the effect's raw output (`effect_texture`, from the
// preceding effect pass) -- deliberately does NOT sample the
// pre-effect texture at all. Instead this pass renders onto the
// pre-effect texture directly (`LoadOp::Load`, see `record_draw`) and
// relies on the pipeline's own `BlendState::ALPHA_BLENDING` to do the
// mixing: outputting `vec4(effect_color.rgb, mask)` makes the GPU's
// fixed-function blend unit compute exactly `mix(before, effect_color,
// mask)` for us, with no need to bind "before" as a second input
// texture at all (which would otherwise force a third offscreen buffer,
// since a texture can't be bound as both a sampled input and the
// current render target in the same pass).

struct MaskParams {
    transform_x: f32,
    transform_y: f32,
    transform_scale: f32,
    transform_rotation: f32,
    feather: f32,
    invert: f32,       // 0.0 = false, 1.0 = true
    mask_mode: f32,    // 0 = none, 1 = circle, 2 = gradient, 3 = texture
    canvas_aspect: f32, // canvas_width / canvas_height, corrects circle/gradient math for non-square canvases
};

@group(0) @binding(0) var effect_texture: texture_2d<f32>;
@group(0) @binding(1) var effect_sampler: sampler;
@group(0) @binding(2) var<uniform> params: MaskParams;
@group(0) @binding(3) var mask_texture: texture_2d<f32>;

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

const MASK_NONE: f32 = 0.0;
const MASK_CIRCLE: f32 = 1.0;
const MASK_GRADIENT: f32 = 2.0;
const MASK_TEXTURE: f32 = 3.0;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let effect_color = textureSample(effect_texture, effect_sampler, in.uv);
    let centered = in.uv - vec2<f32>(params.transform_x, params.transform_y);

    var mask: f32 = 1.0;
    if params.mask_mode == MASK_NONE {
        mask = 1.0;
    } else if params.mask_mode == MASK_CIRCLE {
        // Aspect-corrected so the shape reads round on a non-square
        // canvas instead of stretching into an ellipse.
        let corrected = vec2<f32>(centered.x * params.canvas_aspect, centered.y);
        let dist = length(corrected);
        let radius = max(params.transform_scale * 0.5, 0.0001);
        let inner = radius * (1.0 - clamp(params.feather, 0.0, 1.0));
        mask = 1.0 - smoothstep(inner, radius, dist);
    } else if params.mask_mode == MASK_GRADIENT {
        // Linear ramp along the transform's rotation direction, from
        // fully on at the transform's own position to fully off at
        // `transform_scale` UV units away, feathered over the last
        // `feather` fraction of that span.
        let angle = radians(params.transform_rotation);
        let dir = vec2<f32>(cos(angle), sin(angle));
        let span = max(params.transform_scale, 0.0001);
        let d = dot(centered, dir);
        let feather_span = max(clamp(params.feather, 0.0, 1.0), 0.0001) * span;
        mask = 1.0 - smoothstep(span - feather_span, span, d);
    } else {
        mask = textureSample(mask_texture, effect_sampler, in.uv).r;
    }

    if params.invert > 0.5 {
        mask = 1.0 - mask;
    }

    return vec4<f32>(effect_color.rgb, mask * effect_color.a);
}
