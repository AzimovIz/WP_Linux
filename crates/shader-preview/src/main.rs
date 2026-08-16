//! Dev tool: renders a short looping GIF that shows what a
//! `player`/`wp_linux_editor` "Shader" effect (`.wgsl` file under
//! `EffectKind::Shader`, see `project_format::parse_shader_params`)
//! actually does, without opening the editor.
//!
//! Usage: `shader-preview <shader.wgsl> [image.png]`
//!
//! With no image, a synthetic checker-gradient is generated instead --
//! good enough to see both geometric distortion (checker grid) and color
//! effects (the gradient) at a glance, no bundled asset needed.
//!
//! Renders through the exact same `player::SceneRenderer` code path as
//! the real editor/render-server, headless (`renderer::create_canvas` --
//! see that module's doc comment for why it's a copy, not a shared dep).
//! Every param is driven by its own `default` from the shader's own
//! annotations (same ones the editor turns into sliders), and time is an
//! explicit, caller-controlled float (`update_shader_effects`'s
//! `time_seconds`) -- so frames are deterministic, not tied to wall-clock
//! timing between renders.

mod renderer;

use image::{Delay, Frame, RgbaImage};
use std::path::PathBuf;

const CANVAS_SIZE: u32 = 256;
const FPS: u32 = 20;
const DURATION_SECONDS: f32 = 2.0;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(shader_arg) = args.next() else {
        eprintln!("usage: shader-preview <shader.wgsl> [image.png]");
        std::process::exit(1);
    };
    let image_arg = args.next();

    let shader_path = canonicalize_or_exit(&shader_arg);
    let source = std::fs::read_to_string(&shader_path).unwrap_or_else(|e| {
        eprintln!("failed to read {}: {e}", shader_path.display());
        std::process::exit(1);
    });
    let specs = project_format::parse_shader_params(&source).unwrap_or_else(|e| {
        eprintln!("invalid shader param annotations in {}: {e}", shader_path.display());
        std::process::exit(1);
    });
    let params: Vec<f32> = specs.iter().map(|spec| spec.default).collect();

    let image_path = match image_arg {
        Some(path) => canonicalize_or_exit(&path),
        None => {
            // Left under the OS temp dir rather than cleaned up here --
            // same "staged, not deleted" approach the editor already
            // takes for its own not-yet-saved temp assets.
            let path = std::env::temp_dir().join(format!(
                "shader-preview-checker-gradient-{}.png",
                std::process::id()
            ));
            checker_gradient(CANVAS_SIZE, CANVAS_SIZE)
                .save(&path)
                .unwrap_or_else(|e| {
                    eprintln!("failed to write generated background: {e}");
                    std::process::exit(1);
                });
            path
        }
    };

    // `wgsl_path`/`Layer::Image::path` are resolved as `project_dir.join(path)`
    // (see `player::SceneRenderer::load_scene`) -- `Path::join` discards the
    // base entirely when the joined component is absolute, which both
    // `shader_path` and `image_path` are (both went through
    // `canonicalize_or_exit`/were just written under an absolute temp
    // dir), so the actual `project_dir` value is never read and doesn't
    // need to exist.
    let project_dir = std::env::temp_dir();

    let project = project_format::Project {
        name: String::new(),
        description: String::new(),
        fps: FPS,
        layers: vec![project_format::Layer::Image {
            path: image_path.to_string_lossy().into_owned(),
            effects: vec![project_format::Effect {
                kind: project_format::EffectKind::Shader {
                    wgsl_path: shader_path.to_string_lossy().into_owned(),
                    params,
                },
                mask: project_format::Mask::None,
                enabled: true,
            }],
        }],
    };

    let scene_renderer = pollster::block_on(player::SceneRenderer::new_headless(
        renderer::CANVAS_FORMAT,
    ));
    let mut layers = scene_renderer
        .load_scene(&project_dir, &project, false)
        .unwrap_or_else(|e| {
            eprintln!("failed to load shader: {e}");
            std::process::exit(1);
        });
    // Sets `glyphon.canvas_width/height`, which `update_shader_effects`
    // divides the pixel-space cursor by to get its UV -- skipping this
    // leaves it on a stale default and desyncs cursor-reading shaders
    // from this tool's actual canvas size (see `Ideas.md`'s M6 bugfix
    // note for the bug this exact step exists to avoid).
    scene_renderer.set_text_viewport(&mut layers, CANVAS_SIZE, CANVAS_SIZE);

    let canvas = renderer::create_canvas(&scene_renderer, CANVAS_SIZE, CANVAS_SIZE);

    let output_path = shader_path.with_extension("gif");
    let file = std::fs::File::create(&output_path).unwrap_or_else(|e| {
        eprintln!("failed to create {}: {e}", output_path.display());
        std::process::exit(1);
    });
    let mut encoder = image::codecs::gif::GifEncoder::new_with_speed(file, 10);
    encoder
        .set_repeat(image::codecs::gif::Repeat::Infinite)
        .expect("failed to set gif repeat mode");

    let frame_count = (FPS as f32 * DURATION_SECONDS).round() as u32;
    let cursor_px = (CANVAS_SIZE as f32 / 2.0, CANVAS_SIZE as f32 / 2.0);
    for i in 0..frame_count {
        let time_seconds = i as f32 / FPS as f32;
        scene_renderer.update_shader_effects(&layers, cursor_px, time_seconds);
        let pixels = renderer::render_frame(&scene_renderer, &canvas, &layers);
        let frame_image = RgbaImage::from_raw(CANVAS_SIZE, CANVAS_SIZE, pixels)
            .expect("render_frame returned a buffer that doesn't match the canvas size");
        let delay = Delay::from_numer_denom_ms(1000, FPS);
        encoder
            .encode_frame(Frame::from_parts(frame_image, 0, 0, delay))
            .unwrap_or_else(|e| {
                eprintln!("failed to encode frame {i}: {e}");
                std::process::exit(1);
            });
    }
    drop(encoder);

    println!("wrote {}", output_path.display());
}

fn canonicalize_or_exit(path: &str) -> PathBuf {
    PathBuf::from(path).canonicalize().unwrap_or_else(|e| {
        eprintln!("cannot find {path}: {e}");
        std::process::exit(1);
    })
}

/// Procedurally generated test backplate: a checkerboard whose two shades
/// each sample a diagonal color gradient, so both spatial distortion
/// (grid lines) and color-affecting effects (the gradient itself) are
/// visible in the same image without needing a bundled sample asset.
fn checker_gradient(width: u32, height: u32) -> RgbaImage {
    let cell = (width.max(height) / 8).max(1);
    RgbaImage::from_fn(width, height, |x, y| {
        let tx = x as f32 / width as f32;
        let ty = y as f32 / height as f32;
        let base = [
            lerp(40.0, 235.0, tx),
            lerp(60.0, 140.0, (tx + ty) * 0.5),
            lerp(235.0, 40.0, ty),
        ];
        let on_light_square = (x / cell + y / cell).is_multiple_of(2);
        let shade = if on_light_square { 1.0 } else { 0.55 };
        image::Rgba([
            (base[0] * shade) as u8,
            (base[1] * shade) as u8,
            (base[2] * shade) as u8,
            255,
        ])
    })
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t.clamp(0.0, 1.0)
}
