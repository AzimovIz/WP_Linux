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

mod renderer;

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use image::AnimationDecoder;
use project_format::{Layer, Project};
use renderer::{DrawLayer, ImageLayer, Renderer, XrayLayer};
use tiny_http::{Method, Response, Server};

const HTTP_ADDR: &str = "127.0.0.1:47824";
const TICK_INTERVAL: Duration = Duration::from_millis(33);

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
    canvas_width: u32,
    canvas_height: u32,
    dynamic: bool,
    needs_cursor: bool,
}

#[derive(Default)]
struct SharedState {
    project: Option<LoadedProject>,
    frame_bytes: Vec<u8>,
    frame_content_type: &'static str,
    frame_id: u64,
    /// Normalized (0..1) cursor position within the wallpaper item, as
    /// last reported by the QML side -- `None` means the pointer isn't
    /// currently over it.
    cursor_uv: Option<(f32, f32)>,
}

fn main() {
    eprintln!("render-server: starting (pid {})", std::process::id());
    eprintln!("render-server: initializing wgpu...");
    let renderer = Arc::new(Renderer::new());
    eprintln!("render-server: wgpu ready");

    let state = Arc::new(Mutex::new(SharedState::default()));

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
                let project_dir = PathBuf::from(body.trim());
                match load_project(&renderer, &project_dir) {
                    Ok(mut loaded) => {
                        eprintln!(
                            "render-server: loaded project {:?} ({} layer(s), dynamic = {})",
                            project_dir,
                            loaded.layers.len(),
                            loaded.dynamic
                        );
                        let mut state = state.lock().unwrap();
                        let (frame_bytes, content_type) =
                            compose_and_encode(&renderer, &mut loaded, None);
                        state.project = Some(loaded);
                        state.cursor_uv = None;
                        state.frame_bytes = frame_bytes;
                        state.frame_content_type = content_type;
                        state.frame_id += 1;
                        drop(state);
                        let _ = request.respond(text_response(200, "ok"));
                    }
                    Err(e) => {
                        eprintln!("render-server: failed to load project {project_dir:?}: {e}");
                        let _ = request.respond(text_response(422, &e));
                    }
                }
            }
            (Method::Post, "/cursor") => {
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                let mut state = state.lock().unwrap();
                state.cursor_uv = parse_cursor(body.trim());
                let _ = request.respond(text_response(200, "ok"));
            }
            (Method::Get, "/frame") => {
                let state = state.lock().unwrap();
                if state.frame_bytes.is_empty() {
                    let _ = request.respond(text_response(404, "no project loaded"));
                } else {
                    let response = Response::from_data(state.frame_bytes.clone()).with_header(
                        tiny_http::Header::from_bytes(
                            &b"Content-Type"[..],
                            state.frame_content_type.as_bytes(),
                        )
                        .unwrap(),
                    );
                    let _ = request.respond(response);
                }
            }
            (Method::Get, "/meta") => {
                let state = state.lock().unwrap();
                let ready = state.project.is_some();
                let needs_cursor = state.project.as_ref().is_some_and(|p| p.needs_cursor);
                let body = format!(
                    "{{\"ready\":{ready},\"frame_id\":{},\"needs_cursor\":{needs_cursor}}}",
                    state.frame_id
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

/// Runs forever in its own thread, re-compositing dynamic projects (gif
/// animation, xray cursor reaction) at a capped rate. Idles almost for
/// free when nothing is loaded or the current project is fully static.
fn render_tick_loop(renderer: &Renderer, state: &Mutex<SharedState>) {
    loop {
        std::thread::sleep(TICK_INTERVAL);

        let mut state = state.lock().unwrap();
        let cursor_uv = state.cursor_uv;
        let Some(project) = state.project.as_mut() else {
            continue;
        };
        if !project.dynamic {
            continue;
        }

        advance_gif_frames(renderer, project, TICK_INTERVAL.as_millis() as u64);
        let (frame_bytes, content_type) = compose_and_encode(renderer, project, cursor_uv);
        state.frame_bytes = frame_bytes;
        state.frame_content_type = content_type;
        state.frame_id += 1;
    }
}

fn advance_gif_frames(renderer: &Renderer, project: &mut LoadedProject, elapsed_ms: u64) {
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
            }
        }
    }
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

    let rgba = renderer.render_frame(project.canvas_width, project.canvas_height, &draw_layers);

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

    Ok(LoadedProject {
        layers: loaded_layers,
        canvas_width,
        canvas_height,
        dynamic,
        needs_cursor,
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
