//! Renders a wallpaper project's layer stack with wgpu and serves the
//! composited result to the Plasma QML wallpaper plugin over a tiny
//! local HTTP API:
//!
//!   POST /project   body = absolute path to a project directory -> loads it
//!   POST /cursor     body = "x,y" (normalized 0..1) or "none" -> cursor position
//!   GET  /frame      -> current frame, PNG (static project) or BMP (dynamic)
//!   GET  /meta       -> {"ready": bool, "frame_id": u64, "needs_cursor": bool}
//!
//! Deliberately started by hand for now (see crates/cursor-bridge for the
//! same "figure out autostart later" note).
//!
//! Layers that don't react to the cursor or animate on their own (plain
//! `Image`) are rendered once and left alone. Layers that do (`Xray`,
//! `Gif`) put the server into a continuous render loop, capped at
//! ~30fps, whose frames go out as uncompressed BMP -- on localhost,
//! bandwidth is free but PNG's deflate compression is not, and BMP
//! decode in Qt is essentially a memcpy.
//!
//! Loading a project and rendering every tick both happen exclusively on
//! the dedicated render thread, which never holds a lock while doing GPU
//! work -- HTTP handling only ever touches small, cheap-to-lock fields
//! (`SharedState::output`/`cursor_uv`/`pending_project`). Sharing one
//! mutex between "the render loop's GPU work" and "every HTTP response"
//! was the earlier design's mistake: a render taking tens of
//! milliseconds serialized every /meta, /frame and /cursor request
//! behind it, since tiny_http handles requests one at a time on a single
//! thread.

mod renderer;

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use image::AnimationDecoder;
use project_format::{Layer, Project};
use renderer::{Canvas, DrawLayer, ImageLayer, Renderer, XrayLayer};
use tiny_http::{Method, Response, Server};

const HTTP_ADDR: &str = "127.0.0.1:47824";
/// Used only before any project has been loaded yet -- once a project
/// loads, its own `fps` field (see project-format) drives the tick rate.
const IDLE_TICK_INTERVAL: Duration = Duration::from_millis(33);

struct GifFrame {
    rgba: Vec<u8>,
    delay_ms: u64,
}

enum LoadedLayer {
    Image(ImageLayer),
    Xray(XrayLayer),
    Gif {
        image: ImageLayer,
        frames: Vec<GifFrame>,
        current: usize,
        elapsed_ms: u64,
        width: u32,
        height: u32,
    },
}

struct LoadedProject {
    layers: Vec<LoadedLayer>,
    canvas: Canvas,
    canvas_width: u32,
    canvas_height: u32,
    dynamic: bool,
    needs_cursor: bool,
    fps: u32,
    tick_interval: Duration,
}

#[derive(Default)]
struct FrameOutput {
    ready: bool,
    needs_cursor: bool,
    fps: u32,
    frame_bytes: Vec<u8>,
    frame_content_type: &'static str,
    frame_id: u64,
}

#[derive(Default)]
struct SharedState {
    /// Written by the render thread after every frame; read by
    /// `/frame` and `/meta`. Never held while doing GPU work.
    output: Mutex<FrameOutput>,
    /// Normalized (0..1) cursor position within the wallpaper item, as
    /// last reported by the QML side -- `None` means the pointer isn't
    /// currently over it. Written by `/cursor`, read by the render
    /// thread.
    cursor_uv: Mutex<Option<(f32, f32)>>,
    /// Set by `/project`, taken (and cleared) by the render thread on
    /// its next tick.
    pending_project: Mutex<Option<PathBuf>>,
}

fn main() {
    eprintln!("render-server: starting (pid {})", std::process::id());
    eprintln!("render-server: initializing wgpu...");
    let renderer = Arc::new(Renderer::new());
    eprintln!("render-server: wgpu ready");

    let state = Arc::new(SharedState::default());

    {
        let renderer = Arc::clone(&renderer);
        let state = Arc::clone(&state);
        std::thread::spawn(move || render_tick_loop(&renderer, &state));
    }

    let server =
        Server::http(HTTP_ADDR).unwrap_or_else(|e| panic!("failed to bind {HTTP_ADDR}: {e}"));
    eprintln!("render-server: serving http://{HTTP_ADDR} (no project loaded yet)");

    for mut request in server.incoming_requests() {
        let method = request.method().clone();
        // request.url() is the raw request-target and includes the query
        // string (e.g. "/frame?t=123"); strip it before matching routes.
        let full_url = request.url().to_string();
        let path = full_url.split('?').next().unwrap_or(&full_url).to_string();

        match (&method, path.as_str()) {
            (Method::Post, "/project") => {
                let mut body = String::new();
                if let Err(e) = request.as_reader().read_to_string(&mut body) {
                    let _ =
                        request.respond(text_response(400, &format!("bad request body: {e}")));
                    continue;
                }
                *state.pending_project.lock().unwrap() = Some(PathBuf::from(body.trim()));
                let _ = request.respond(text_response(200, "ok"));
            }
            (Method::Post, "/cursor") => {
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                *state.cursor_uv.lock().unwrap() = parse_cursor(body.trim());
                let _ = request.respond(text_response(200, "ok"));
            }
            (Method::Get, "/frame") => {
                let output = state.output.lock().unwrap();
                if output.frame_bytes.is_empty() {
                    let _ = request.respond(text_response(404, "no project loaded"));
                } else {
                    let response = Response::from_data(output.frame_bytes.clone()).with_header(
                        tiny_http::Header::from_bytes(
                            &b"Content-Type"[..],
                            output.frame_content_type.as_bytes(),
                        )
                        .unwrap(),
                    );
                    let _ = request.respond(response);
                }
            }
            (Method::Get, "/meta") => {
                let output = state.output.lock().unwrap();
                let body = format!(
                    "{{\"ready\":{},\"frame_id\":{},\"needs_cursor\":{},\"fps\":{}}}",
                    output.ready, output.frame_id, output.needs_cursor, output.fps
                );
                let response = Response::from_string(body).with_header(
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                        .unwrap(),
                );
                let _ = request.respond(response);
            }
            _ => {
                let _ = request.respond(text_response(404, "not found"));
            }
        }
    }
}

/// Runs forever in its own thread: picks up newly-posted project paths,
/// re-composites dynamic projects (gif animation, xray cursor reaction)
/// at a capped rate, and renders a static project exactly once. Idles
/// almost for free when nothing is loaded or the current project is
/// fully static. Never holds `state`'s locks while doing GPU work.
fn render_tick_loop(renderer: &Renderer, state: &SharedState) {
    let mut current: Option<LoadedProject> = None;
    let mut needs_render = false;
    // Last cursor position we actually rendered with -- lets us skip a
    // tick entirely (no GPU work, no encode, no output update) when
    // nothing that could change the picture actually changed, e.g. an
    // xray layer with a stationary cursor, or a gif sitting inside a
    // multi-second inter-frame delay.
    let mut last_rendered_cursor_uv: Option<(f32, f32)> = None;
    let mut tick_interval = IDLE_TICK_INTERVAL;

    loop {
        std::thread::sleep(tick_interval);

        if let Some(project_dir) = state.pending_project.lock().unwrap().take() {
            match load_project(renderer, &project_dir) {
                Ok(loaded) => {
                    eprintln!(
                        "render-server: loaded project {:?} ({} layer(s), dynamic = {}, fps target {:?})",
                        project_dir,
                        loaded.layers.len(),
                        loaded.dynamic,
                        loaded.tick_interval,
                    );
                    *state.cursor_uv.lock().unwrap() = None;
                    tick_interval = loaded.tick_interval;
                    current = Some(loaded);
                    needs_render = true;
                    last_rendered_cursor_uv = None;
                }
                Err(e) => {
                    eprintln!("render-server: failed to load project {project_dir:?}: {e}");
                }
            }
        }

        let Some(project) = current.as_mut() else {
            continue;
        };
        if !project.dynamic && !needs_render {
            continue;
        }

        let gif_changed = if project.dynamic {
            advance_gif_frames(renderer, project, tick_interval.as_millis() as u64)
        } else {
            false
        };

        let cursor_uv = *state.cursor_uv.lock().unwrap();
        let cursor_changed = project.needs_cursor && cursor_uv != last_rendered_cursor_uv;

        if !needs_render && !gif_changed && !cursor_changed {
            continue;
        }
        last_rendered_cursor_uv = cursor_uv;

        let (frame_bytes, content_type) = compose_and_encode(renderer, project, cursor_uv);

        let mut output = state.output.lock().unwrap();
        output.ready = true;
        output.needs_cursor = project.needs_cursor;
        output.fps = project.fps;
        output.frame_bytes = frame_bytes;
        output.frame_content_type = content_type;
        output.frame_id += 1;
        drop(output);

        needs_render = false;
    }
}

/// Advances any gif layers by `elapsed_ms` and returns whether any of
/// them actually landed on a new frame (as opposed to still being inside
/// their current frame's delay).
fn advance_gif_frames(renderer: &Renderer, project: &mut LoadedProject, elapsed_ms: u64) -> bool {
    let mut any_changed = false;
    for layer in &mut project.layers {
        if let LoadedLayer::Gif {
            image,
            frames,
            current,
            elapsed_ms: layer_elapsed,
            width,
            height,
        } = layer
        {
            *layer_elapsed += elapsed_ms;
            let mut changed = false;
            while *layer_elapsed >= frames[*current].delay_ms {
                *layer_elapsed -= frames[*current].delay_ms;
                *current = (*current + 1) % frames.len();
                changed = true;
            }
            if changed {
                renderer.update_image_layer(image, &frames[*current].rgba, *width, *height);
                any_changed = true;
            }
        }
    }
    any_changed
}

fn compose_and_encode(
    renderer: &Renderer,
    project: &LoadedProject,
    cursor_uv: Option<(f32, f32)>,
) -> (Vec<u8>, &'static str) {
    let cursor_px = cursor_uv
        .filter(|_| project.needs_cursor)
        .map(|(u, v)| {
            (
                u * project.canvas_width as f32,
                v * project.canvas_height as f32,
            )
        })
        .unwrap_or((-1.0e6, -1.0e6));

    let mut draw_layers = Vec::with_capacity(project.layers.len());
    for layer in &project.layers {
        match layer {
            LoadedLayer::Image(image) => draw_layers.push(DrawLayer::Image(image)),
            LoadedLayer::Gif { image, .. } => draw_layers.push(DrawLayer::Image(image)),
            LoadedLayer::Xray(xray) => {
                renderer.update_xray_cursor(xray, cursor_px);
                draw_layers.push(DrawLayer::Xray(xray));
            }
        }
    }

    let rgba = renderer.render_frame(&project.canvas, &draw_layers);

    if project.dynamic {
        (
            encode(
                &rgba,
                project.canvas_width,
                project.canvas_height,
                image::ImageFormat::Bmp,
            ),
            "image/bmp",
        )
    } else {
        (
            encode(
                &rgba,
                project.canvas_width,
                project.canvas_height,
                image::ImageFormat::Png,
            ),
            "image/png",
        )
    }
}

fn load_project(renderer: &Renderer, project_dir: &Path) -> Result<LoadedProject, String> {
    let (project, project_dir) = Project::load(project_dir).map_err(|e| e.to_string())?;
    if project.layers.is_empty() {
        return Err("project has no layers".to_string());
    }

    let mut loaded_layers = Vec::with_capacity(project.layers.len());
    let mut canvas_size: Option<(u32, u32)> = None;

    for layer in &project.layers {
        match layer {
            Layer::Image { path } => {
                let (rgba, width, height) = open_rgba(&project_dir.join(path))?;
                canvas_size.get_or_insert((width, height));
                loaded_layers.push(LoadedLayer::Image(
                    renderer.create_image_layer(&rgba, width, height),
                ));
            }
            Layer::Xray {
                base,
                overlay,
                radius,
            } => {
                let (base_rgba, base_width, base_height) = open_rgba(&project_dir.join(base))?;
                let (overlay_rgba, overlay_width, overlay_height) =
                    open_rgba(&project_dir.join(overlay))?;
                canvas_size.get_or_insert((base_width, base_height));
                loaded_layers.push(LoadedLayer::Xray(renderer.create_xray_layer(
                    &base_rgba,
                    base_width,
                    base_height,
                    &overlay_rgba,
                    overlay_width,
                    overlay_height,
                    *radius,
                )));
            }
            Layer::Gif { path } => {
                let (frames, width, height) = decode_gif(&project_dir.join(path))?;
                canvas_size.get_or_insert((width, height));
                let image = renderer.create_image_layer(&frames[0].rgba, width, height);
                loaded_layers.push(LoadedLayer::Gif {
                    image,
                    frames,
                    current: 0,
                    elapsed_ms: 0,
                    width,
                    height,
                });
            }
        }
    }

    let (canvas_width, canvas_height) =
        canvas_size.ok_or_else(|| "project has no layers".to_string())?;
    let dynamic = project.layers.iter().any(Layer::is_dynamic);
    let needs_cursor = project
        .layers
        .iter()
        .any(|l| matches!(l, Layer::Xray { .. }));
    let canvas = renderer.create_canvas(canvas_width, canvas_height);
    let fps = project.fps.clamp(1, 60);
    let tick_interval = Duration::from_millis(1000 / u64::from(fps));

    Ok(LoadedProject {
        layers: loaded_layers,
        canvas,
        canvas_width,
        canvas_height,
        dynamic,
        needs_cursor,
        fps,
        tick_interval,
    })
}

fn open_rgba(path: &Path) -> Result<(Vec<u8>, u32, u32), String> {
    let image = image::open(path)
        .map_err(|e| format!("failed to open image {path:?}: {e}"))?
        .into_rgba8();
    let (width, height) = image.dimensions();
    Ok((image.into_raw(), width, height))
}

fn decode_gif(path: &Path) -> Result<(Vec<GifFrame>, u32, u32), String> {
    let file = File::open(path).map_err(|e| format!("failed to open gif {path:?}: {e}"))?;
    let decoder = image::codecs::gif::GifDecoder::new(BufReader::new(file))
        .map_err(|e| format!("failed to decode gif {path:?}: {e}"))?;
    let frames = decoder
        .into_frames()
        .collect_frames()
        .map_err(|e| format!("failed to decode gif frames {path:?}: {e}"))?;
    if frames.is_empty() {
        return Err(format!("gif {path:?} has no frames"));
    }

    let (width, height) = frames[0].buffer().dimensions();
    let gif_frames = frames
        .into_iter()
        .map(|frame| {
            let (numer, denom) = frame.delay().numer_denom_ms();
            let delay_ms = if denom == 0 {
                100
            } else {
                (numer / denom).max(20) as u64
            };
            GifFrame {
                rgba: frame.into_buffer().into_raw(),
                delay_ms,
            }
        })
        .collect();
    Ok((gif_frames, width, height))
}

fn parse_cursor(body: &str) -> Option<(f32, f32)> {
    if body == "none" || body.is_empty() {
        return None;
    }
    let (x, y) = body.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

fn encode(rgba: &[u8], width: u32, height: u32, format: image::ImageFormat) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut bytes);
    image::write_buffer_with_format(
        &mut cursor,
        rgba,
        width,
        height,
        image::ExtendedColorType::Rgba8,
        format,
    )
    .expect("frame encoding failed");
    bytes
}

fn text_response(code: u16, body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body).with_status_code(tiny_http::StatusCode(code))
}
