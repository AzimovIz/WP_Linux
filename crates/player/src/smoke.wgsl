// Persistent cursor-trail state -- reads last frame's own result
// (copied into `input_texture` by `record_draw`'s per-frame
// `copy_texture_to_texture`, see `LoadedEffectKind::Smoke`'s doc
// comment in lib.rs) and writes this frame's result: the previous
// frame decayed, plus a soft splat centered on the cursor's current UV
// position. This *is* the effect's raw output too -- unlike every
// other effect kind, there's no separate scratch pass; `record_draw`
// feeds this same texture straight into the generic mask-blend pass.
//
// The splat is a plain (not aspect-corrected) Gaussian in UV space --
// unlike `mask_blend.wgsl`'s Circle mode, which corrects for a
// non-square canvas so it reads as a true circle, this trail is a soft
// blob to begin with, so a slight ellipse on a very non-square canvas
// isn't worth the extra uniform field and math. Revisit if that turns
// out to matter after looking at it (see `Ideas.md`, Milestone A: no
// noise/turbulence in the first version either, same reasoning).

struct SmokeParams {
    color: vec4<f32>,  // {"label": "Color", "default": [0.6, 0.3, 0.9, 1.0]}
    cursor: vec2<f32>, // written every frame by `update_smoke_cursors` -- not a user-facing param
    decay: f32,        // {"label": "Decay", "default": 0.97, "range": [0.8, 0.999]}
    radius: f32,       // {"label": "Splat radius", "default": 0.05, "range": [0.0, 0.3]}
};

@group(0) @binding(0) var input_texture: texture_2d<f32>;
@group(0) @binding(1) var input_sampler: sampler;
@group(0) @binding(2) var<uniform> params: SmokeParams;

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
    let previous = textureSample(input_texture, input_sampler, in.uv) * params.decay;

    let dist = distance(in.uv, params.cursor);
    let splat = params.color * exp(-(dist * dist) / max(params.radius * params.radius, 0.0001));

    return clamp(previous + splat, vec4<f32>(0.0), vec4<f32>(1.0));
}
