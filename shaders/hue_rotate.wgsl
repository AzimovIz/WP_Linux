// Continuously sweeps the image's hue back and forth -- `intensity`
// controls how far around the color wheel it swings (as a fraction of a
// full turn), `speed` how fast.

struct ShaderEffectParams {
    cursor: vec2<f32>,
    time: f32,
    canvas_aspect: f32,
    u_intensity: f32, // {"label": "Intensity", "default": 0.3, "range": [0.0, 1.0]}
    u_speed: f32, // {"label": "Speed", "default": 1.0, "range": [0.0, 4.0]}
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

fn rgb_to_hsv(c: vec3<f32>) -> vec3<f32> {
    let max_c = max(c.r, max(c.g, c.b));
    let min_c = min(c.r, min(c.g, c.b));
    let delta = max_c - min_c;
    var h = 0.0;
    if delta > 0.00001 {
        if max_c == c.r {
            h = ((c.g - c.b) / delta) % 6.0;
        } else if max_c == c.g {
            h = (c.b - c.r) / delta + 2.0;
        } else {
            h = (c.r - c.g) / delta + 4.0;
        }
        h = h / 6.0;
        if h < 0.0 {
            h = h + 1.0;
        }
    }
    var s = 0.0;
    if max_c > 0.00001 {
        s = delta / max_c;
    }
    return vec3<f32>(h, s, max_c);
}

fn hsv_to_rgb(hsv: vec3<f32>) -> vec3<f32> {
    let h = hsv.x * 6.0;
    let s = hsv.y;
    let v = hsv.z;
    let c = v * s;
    let x = c * (1.0 - abs((h % 2.0) - 1.0));
    let m = v - c;
    var rgb = vec3<f32>(0.0, 0.0, 0.0);
    if h < 1.0 {
        rgb = vec3<f32>(c, x, 0.0);
    } else if h < 2.0 {
        rgb = vec3<f32>(x, c, 0.0);
    } else if h < 3.0 {
        rgb = vec3<f32>(0.0, c, x);
    } else if h < 4.0 {
        rgb = vec3<f32>(0.0, x, c);
    } else if h < 5.0 {
        rgb = vec3<f32>(x, 0.0, c);
    } else {
        rgb = vec3<f32>(c, 0.0, x);
    }
    return rgb + vec3<f32>(m, m, m);
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let base = textureSample(input_texture, input_sampler, in.uv);
    var hsv = rgb_to_hsv(base.rgb);
    let shift = sin(params.time * params.u_speed) * params.u_intensity;
    hsv.x = fract(hsv.x + shift);
    return vec4<f32>(hsv_to_rgb(hsv), base.a);
}
