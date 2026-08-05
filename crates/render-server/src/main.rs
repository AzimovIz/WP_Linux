//! Renders a wallpaper project (currently: a single static image) with
//! wgpu and serves the result to the Plasma QML wallpaper plugin over a
//! tiny local HTTP API:
//!
//!   POST /project   body = absolute path to a project directory -> loads it
//!   GET  /frame      -> current frame, PNG
//!   GET  /meta       -> {"ready": bool, "cursor_glow": bool}
//!
//! Deliberately started by hand for now (see crates/cursor-bridge for the
//! same "figure out autostart later" note) -- this is the renderer half,
//! not the cursor-position half.

mod renderer;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use renderer::Renderer;
use tiny_http::{Method, Response, Server};

const HTTP_ADDR: &str = "127.0.0.1:47824";

#[derive(Default)]
struct State {
    project: Option<project_format::Project>,
    frame_png: Vec<u8>,
}

fn main() {
    eprintln!("render-server: starting (pid {})", std::process::id());
    eprintln!("render-server: initializing wgpu...");
    let renderer = Renderer::new();
    eprintln!("render-server: wgpu ready");

    let state = Mutex::new(State::default());

    let server = Server::http(HTTP_ADDR)
        .unwrap_or_else(|e| panic!("failed to bind {HTTP_ADDR}: {e}"));
    eprintln!("render-server: serving http://{HTTP_ADDR} (no project loaded yet)");

    for mut request in server.incoming_requests() {
        let method = request.method().clone();
        let url = request.url().to_string();

        match (&method, url.as_str()) {
            (Method::Post, "/project") => {
                let mut body = String::new();
                if let Err(e) = request.as_reader().read_to_string(&mut body) {
                    let _ = request.respond(text_response(400, &format!("bad request body: {e}")));
                    continue;
                }
                let path = PathBuf::from(body.trim());
                match load_project(&renderer, &path) {
                    Ok((project, frame_png)) => {
                        eprintln!(
                            "render-server: loaded project {:?} (image = {}, cursor_glow = {})",
                            path, project.image, project.cursor_glow
                        );
                        let mut state = state.lock().unwrap();
                        state.project = Some(project);
                        state.frame_png = frame_png;
                        let _ = request.respond(text_response(200, "ok"));
                    }
                    Err(e) => {
                        eprintln!("render-server: failed to load project {path:?}: {e}");
                        let _ = request.respond(text_response(422, &e));
                    }
                }
            }
            (Method::Get, "/frame") => {
                let state = state.lock().unwrap();
                if state.frame_png.is_empty() {
                    let _ = request.respond(text_response(404, "no project loaded"));
                } else {
                    let response = Response::from_data(state.frame_png.clone()).with_header(
                        tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"image/png"[..])
                            .unwrap(),
                    );
                    let _ = request.respond(response);
                }
            }
            (Method::Get, "/meta") => {
                let state = state.lock().unwrap();
                let ready = state.project.is_some();
                let cursor_glow = state.project.as_ref().is_some_and(|p| p.cursor_glow);
                let body = format!("{{\"ready\":{ready},\"cursor_glow\":{cursor_glow}}}");
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

fn load_project(
    renderer: &Renderer,
    project_dir: &Path,
) -> Result<(project_format::Project, Vec<u8>), String> {
    let (project, image_path) =
        project_format::Project::load(project_dir).map_err(|e| e.to_string())?;

    let image = image::open(&image_path)
        .map_err(|e| format!("failed to open image {image_path:?}: {e}"))?
        .into_rgba8();
    let (width, height) = image.dimensions();

    let frame_png = renderer.render_to_png(image.as_raw(), width, height);
    Ok((project, frame_png))
}

fn text_response(code: u16, body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body).with_status_code(tiny_http::StatusCode(code))
}
