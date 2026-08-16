// Same fullscreen-triangle shape as shader.wgsl, but the sampled UV is
// first remapped through `cover`'s scale/offset -- computed on the Rust
// side (see `cover_uv_scale_offset`) to implement CSS `background-size:
// cover`: scale this layer's own picture uniformly until it fully covers
// the canvas, cropping whatever overflows past each edge. Kept as its own
// shader/pipeline/bind-group-layout rather than folding into shader.wgsl,
// since that one is also used for the plain accumulator-to-target blit
// (`record_draw`) and an effect chain's own composite step, neither of
// which wants this remapping -- both already sample a same-sized surface.

struct CoverUniform {
    uv_scale: vec2<f32>,
    uv_offset: vec2<f32>,
};

@group(0) @binding(0) var scene_texture: texture_2d<f32>;
@group(0) @binding(1) var scene_sampler: sampler;
@group(0) @binding(2) var<uniform> cover: CoverUniform;

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
    let uv = in.uv * cover.uv_scale + cover.uv_offset;
    return textureSample(scene_texture, scene_sampler, uv);
}
