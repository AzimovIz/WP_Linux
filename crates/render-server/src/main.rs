//! Renders a wallpaper project's layer stack with wgpu and serves the
//! composited result to the Plasma QML wallpaper plugin over a tiny
//! local HTTP API:
//!
//!   POST /project    body = absolute path to a project directory -> loads it
//!   POST /geometry   body = "virtualX,virtualY,width,height" -> the wallpaper
//!                    item's placement on the virtual desktop, used to turn
//!                    the global cursor position (see below) into local,
//!                    normalized coordinates for xray layers
//!   GET  /frame      -> current frame, PNG (static project) or BMP (dynamic)
//!   GET  /meta       -> {"ready": bool, "frame_id": u64, "needs_cursor": bool, "fps": u32}
//!
//! Cursor position itself does NOT come in over HTTP: this process also
//! registers the `dev.wplinux.CursorBridge` D-Bus service (formerly a
//! separate `cursor-bridge` binary) that a companion KWin script feeds
//! with the true, compositor-level pointer position -- see
//! plasma/kwin-script/package. Folding it in here means one less process to
//! start by hand, and lets the render loop react to cursor movement
//! without an HTTP round-trip through QML at all.
//!
//! Deliberately started by hand for now (autostart is still a TODO).
//!
//! Layers that don't react to the cursor or animate on their own (plain
//! `Image`) are rendered once and left alone. Layers that do (`Xray`,
//! `Gif`) put the server into a continuous render loop, capped at the
//! project's configured fps, whose frames go out as uncompressed BMP --
//! on localhost, bandwidth is free but PNG's deflate compression is not,
//! and BMP decode in Qt is essentially a memcpy.
//!
//! Loading a project and rendering every tick both happen exclusively on
//! the dedicated render thread, which never holds a lock while doing GPU
//! work -- HTTP handling only ever touches small, cheap-to-lock fields
//! (`SharedState::output`/`global_cursor`/`geometry`/`pending_project`).
//! Sharing one mutex between "the render loop's GPU work" and "every
//! HTTP response" was the earlier design's mistake: a render taking tens
//! of milliseconds serialized every /meta and /frame request behind it,
//! since tiny_http handles requests one at a time on a single thread.

mod renderer;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use player::{LoadedLayer, SceneRenderer};
use project_format::{Layer, Project};
use renderer::{Canvas, CANVAS_FORMAT};
use tiny_http::{Method, Response, Server};
use zbus::blocking::connection;

const CURSOR_BUS_NAME: &str = "dev.wplinux.CursorBridge";
const CURSOR_OBJECT_PATH: &str = "/dev/wplinux/CursorBridge";

const HTTP_ADDR: &str = "127.0.0.1:47824";
/// Used only before any project has been loaded yet -- once a project
/// loads, its own `fps` field (see project-format) drives the tick rate.
const IDLE_TICK_INTERVAL: Duration = Duration::from_millis(33);

/// A wallpaper freezing on its last frame for up to this long after the
/// system actually enters power-saver mode is an imperceptible tradeoff
/// for not pulling in zbus's signal-stream machinery just to watch one
/// property -- see `run_power_profile_watcher`.
const POWER_PROFILE_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Read-only view of `net.hadess.PowerProfiles` (power-profiles-daemon),
/// the freedesktop-standard power profile service -- also what KDE's own
/// Power Management settings page reads and writes. Lets
/// `run_power_profile_watcher` ask "is the user in power-saver mode?"
/// without hand-rolling a `Properties.Get` call.
#[zbus::proxy(
    interface = "net.hadess.PowerProfiles",
    default_service = "net.hadess.PowerProfiles",
    default_path = "/net/hadess/PowerProfiles"
)]
trait PowerProfiles {
    #[zbus(property)]
    fn active_profile(&self) -> zbus::Result<String>;
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
    // Gif frame timing is time-based (see `SceneRenderer::advance_gifs`),
    // not tick-accumulator-based, so all it needs from us is "how long
    // has this project been loaded" -- no per-layer state to track here.
    loaded_at: Instant,
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
    /// Global (virtual-desktop) cursor position in pixels, as last
    /// reported by the KWin script over D-Bus. `None` until the first
    /// report arrives.
    global_cursor: Mutex<Option<(i32, i32)>>,
    /// The wallpaper item's placement on the virtual desktop --
    /// (virtualX, virtualY, width, height) -- as last pushed by QML via
    /// `/geometry`. Needed to turn `global_cursor` into local,
    /// normalized coordinates; only QML knows this, since render-server
    /// has no Wayland/Qt connection of its own.
    geometry: Mutex<Option<(f64, f64, f64, f64)>>,
    /// Set by `/project`, taken (and cleared) by the render thread on
    /// its next tick.
    pending_project: Mutex<Option<PathBuf>>,
    /// Mirrors whether power-profiles-daemon currently reports the
    /// "power-saver" profile, as last polled by
    /// `run_power_profile_watcher`. While set, the render loop skips all
    /// continuous re-rendering (gif frames, cursor reaction) and just
    /// keeps serving the last frame it already produced -- see
    /// `render_tick_loop`.
    power_saver: AtomicBool,
}

/// D-Bus object backing `dev.wplinux.CursorBridge` -- the KWin script
/// (plasma/kwin-script/package) calls `SetCursorPosition` on this every time
/// `workspace.cursorPosChanged` fires.
struct CursorDbusService {
    state: Arc<SharedState>,
}

#[zbus::interface(name = "dev.wplinux.CursorBridge")]
impl CursorDbusService {
    #[zbus(name = "SetCursorPosition")]
    fn set_cursor_position(&mut self, x: i32, y: i32) {
        *self.state.global_cursor.lock().unwrap() = Some((x, y));
    }
}

/// Registers the cursor D-Bus service and then parks forever, keeping
/// the connection alive for the life of the process. Runs in its own
/// thread so a D-Bus setup failure (e.g. another instance of this
/// service already running) can't take down the HTTP server or the
/// render loop -- it just means xray layers won't see cursor movement.
fn run_cursor_dbus_service(state: Arc<SharedState>) {
    let service = CursorDbusService { state };
    let _conn = connection::Builder::session()
        .expect("failed to connect to session D-Bus")
        .name(CURSOR_BUS_NAME)
        .expect(
            "failed to request bus name -- is another instance of this service already running?",
        )
        .serve_at(CURSOR_OBJECT_PATH, service)
        .expect("failed to register D-Bus object")
        .build()
        .expect("failed to build D-Bus connection");
    eprintln!(
        "render-server: registered {CURSOR_BUS_NAME}{CURSOR_OBJECT_PATH}, waiting for SetCursorPosition calls"
    );
    loop {
        std::thread::park();
    }
}

/// Polls power-profiles-daemon for the active power profile and mirrors
/// "is it power-saver?" into `state.power_saver` (see its doc comment for
/// what that flag actually does to the render loop). Runs in its own
/// thread, entirely best-effort: distros/DEs that don't ship
/// power-profiles-daemon at all just leave `power_saver` permanently
/// false, i.e. render-server behaves exactly as if this feature didn't
/// exist. Polls a plain property instead of subscribing to
/// PropertiesChanged -- see `POWER_PROFILE_POLL_INTERVAL`.
fn run_power_profile_watcher(state: Arc<SharedState>) {
    let conn = match zbus::blocking::Connection::system() {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!(
                "render-server: couldn't connect to the system D-Bus ({e}), power-saver \
                 detection disabled -- render-server will keep rendering continuously \
                 regardless of power profile"
            );
            return;
        }
    };
    let proxy = match PowerProfilesProxyBlocking::new(&conn) {
        Ok(proxy) => proxy,
        Err(e) => {
            eprintln!(
                "render-server: couldn't reach power-profiles-daemon ({e}), power-saver \
                 detection disabled -- is it installed/running? render-server will keep \
                 rendering continuously regardless of power profile"
            );
            return;
        }
    };

    loop {
        match proxy.active_profile() {
            Ok(profile) => {
                let power_saving = profile == "power-saver";
                let was_power_saving = state.power_saver.swap(power_saving, Ordering::Relaxed);
                if power_saving != was_power_saving {
                    eprintln!(
                        "render-server: power profile is now {profile:?} -- {}",
                        if power_saving {
                            "freezing continuous rendering on the last frame"
                        } else {
                            "resuming continuous rendering"
                        }
                    );
                }
            }
            Err(e) => {
                eprintln!("render-server: couldn't read active power profile ({e})");
            }
        }
        std::thread::sleep(POWER_PROFILE_POLL_INTERVAL);
    }
}

/// KWin scripts normally need installing as a KPackage
/// (`kpackagetool6 --type=KWin/Script`) plus enabling in kwinrc. KWin
/// also exposes `org.kde.kwin.Scripting` on `/Scripting`, meant for
/// loading an unpackaged script file on the fly (this is how KWin's own
/// scripting console and dev tools work) -- so we use that instead to
/// load our companion script straight from the repo checkout, no
/// packaging/installation step required. Best-effort: if this fails
/// (e.g. a KWin version where the interface differs), the cursor script
/// can still be installed the old way by hand.
fn ensure_kwin_cursor_script_loaded() {
    const PLUGIN_NAME: &str = "dev.wplinux.cursorbridge";
    const SCRIPT_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../plasma/kwin-script/package/contents/code/main.js"
    );

    if !std::path::Path::new(SCRIPT_PATH).exists() {
        eprintln!(
            "render-server: kwin cursor script not found at {SCRIPT_PATH}, skipping auto-load \
             (install it by hand with kpackagetool6 if you moved this binary elsewhere)"
        );
        return;
    }

    let load = || -> zbus::Result<bool> {
        let conn = zbus::blocking::Connection::session()?;

        let already_loaded: bool = conn
            .call_method(
                Some("org.kde.KWin"),
                "/Scripting",
                Some("org.kde.kwin.Scripting"),
                "isScriptLoaded",
                &(PLUGIN_NAME,),
            )?
            .body()
            .deserialize()?;
        if already_loaded {
            return Ok(false);
        }

        conn.call_method(
            Some("org.kde.KWin"),
            "/Scripting",
            Some("org.kde.kwin.Scripting"),
            "loadScript",
            &(SCRIPT_PATH, PLUGIN_NAME),
        )?;
        conn.call_method(
            Some("org.kde.KWin"),
            "/Scripting",
            Some("org.kde.kwin.Scripting"),
            "start",
            &(),
        )?;
        Ok(true)
    };

    match load() {
        Ok(true) => eprintln!("render-server: loaded and started kwin cursor script from {SCRIPT_PATH}"),
        Ok(false) => eprintln!("render-server: kwin cursor script already loaded"),
        Err(e) => eprintln!(
            "render-server: could not auto-load kwin cursor script ({e}) -- \
             install it by hand: kpackagetool6 --type=KWin/Script --install plasma/kwin-script/package"
        ),
    }
}

/// Computes the current normalized (0..1) cursor position within the
/// wallpaper item from the last-known global cursor position and the
/// item's last-reported screen geometry. `None` if either piece is
/// missing, or the cursor is outside the item's bounds.
fn compute_cursor_uv(state: &SharedState) -> Option<(f32, f32)> {
    let (gx, gy) = (*state.global_cursor.lock().unwrap())?;
    let (vx, vy, w, h) = (*state.geometry.lock().unwrap())?;
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let local_x = gx as f64 - vx;
    let local_y = gy as f64 - vy;
    if local_x < 0.0 || local_x > w || local_y < 0.0 || local_y > h {
        return None;
    }
    Some(((local_x / w) as f32, (local_y / h) as f32))
}

fn main() {
    eprintln!("render-server: starting (pid {})", std::process::id());
    eprintln!("render-server: initializing wgpu...");
    let renderer = Arc::new(pollster::block_on(SceneRenderer::new_headless(CANVAS_FORMAT)));
    eprintln!("render-server: wgpu ready");

    let state = Arc::new(SharedState::default());

    {
        let renderer = Arc::clone(&renderer);
        let state = Arc::clone(&state);
        std::thread::spawn(move || render_tick_loop(&renderer, &state));
    }

    {
        let state = Arc::clone(&state);
        std::thread::spawn(move || run_cursor_dbus_service(state));
    }

    {
        let state = Arc::clone(&state);
        std::thread::spawn(move || run_power_profile_watcher(state));
    }

    ensure_kwin_cursor_script_loaded();

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
                eprintln!("render-server: received /project {project_dir:?}");
                *state.pending_project.lock().unwrap() = Some(project_dir);
                let _ = request.respond(text_response(200, "ok"));
            }
            (Method::Post, "/geometry") => {
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                *state.geometry.lock().unwrap() = parse_geometry(body.trim());
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
fn render_tick_loop(renderer: &SceneRenderer, state: &SharedState) {
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
        // Timed from here, not from the end of the previous sleep -- the
        // work below (GPU render, encode) is not instant, and sleeping
        // the full tick_interval regardless of how long that work took
        // would make the real cycle length tick_interval + work_time
        // instead of tick_interval, silently undershooting the
        // project's configured fps by however long a frame takes to
        // produce.
        let cycle_start = Instant::now();

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

        // While the system is in the power-saver profile, treat this
        // exactly like a static project: don't advance gifs, don't react
        // to the cursor, don't push anything to the GPU. `needs_render`
        // (set on load) still gets one frame out below, so a freshly
        // loaded project isn't left blank -- it just freezes on that
        // frame instead of animating, same as a plain `Image` layer
        // always has. See `run_power_profile_watcher`.
        let power_saving = state.power_saver.load(Ordering::Relaxed);

        if let Some(project) = current.as_mut() {
            let gif_changed = if project.dynamic && !power_saving {
                let elapsed_ms = project.loaded_at.elapsed().as_millis() as u64;
                renderer.advance_gifs(&mut project.layers, elapsed_ms)
            } else {
                false
            };

            let cursor_uv = if project.needs_cursor && !power_saving {
                compute_cursor_uv(state)
            } else {
                None
            };
            let cursor_changed =
                project.needs_cursor && !power_saving && cursor_uv != last_rendered_cursor_uv;

            if needs_render || gif_changed || cursor_changed {
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

        // No point waking up at the project's animation fps just to find
        // power_saver still set and do nothing -- but still check often
        // enough that leaving power-saver mode resumes animation quickly.
        let sleep_interval = if power_saving {
            POWER_PROFILE_POLL_INTERVAL
        } else {
            tick_interval
        };
        std::thread::sleep(sleep_interval.saturating_sub(cycle_start.elapsed()));
    }
}

fn compose_and_encode(
    renderer: &SceneRenderer,
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

    renderer.update_xray_cursors(&project.layers, cursor_px);
    let rgba = renderer::render_frame(renderer, &project.canvas, &project.layers);

    if project.dynamic {
        (
            encode_bmp(&rgba, project.canvas_width, project.canvas_height),
            "image/bmp",
        )
    } else {
        (
            encode_png(&rgba, project.canvas_width, project.canvas_height),
            "image/png",
        )
    }
}

fn load_project(renderer: &SceneRenderer, project_dir: &Path) -> Result<LoadedProject, String> {
    let (project, project_dir) = Project::load(project_dir).map_err(|e| e.to_string())?;
    if project.layers.is_empty() {
        return Err("project has no layers".to_string());
    }

    let loaded_layers = renderer.load_scene(&project_dir, &project)?;
    // Whichever layer happens to load first sets the canvas size, same as
    // this always worked before `load_scene` moved into `player`.
    let (canvas_width, canvas_height) = loaded_layers
        .first()
        .map(LoadedLayer::size)
        .expect("checked non-empty above");

    let dynamic = project.layers.iter().any(Layer::is_dynamic);
    let needs_cursor = project
        .layers
        .iter()
        .any(|l| matches!(l, Layer::Xray { .. }));
    let canvas = renderer::create_canvas(renderer, canvas_width, canvas_height);
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
        loaded_at: Instant::now(),
    })
}

/// Parses "virtualX,virtualY,width,height" as pushed by the QML side.
fn parse_geometry(body: &str) -> Option<(f64, f64, f64, f64)> {
    let mut parts = body.split(',');
    let vx = parts.next()?.trim().parse().ok()?;
    let vy = parts.next()?.trim().parse().ok()?;
    let w = parts.next()?.trim().parse().ok()?;
    let h = parts.next()?.trim().parse().ok()?;
    Some((vx, vy, w, h))
}

fn encode_png(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut bytes);
    image::write_buffer_with_format(
        &mut cursor,
        rgba,
        width,
        height,
        image::ExtendedColorType::Rgba8,
        image::ImageFormat::Png,
    )
    .expect("frame encoding failed");
    bytes
}

/// Hand-rolled BMP encoder for dynamic (gif/xray) frames. `image`'s own
/// BMP encoder writes every pixel through a separate `write_all` call
/// (byte-swapping RGBA into BGRA one pixel at a time), which dominated
/// frame time even in release builds. BMP's BITMAPV4HEADER lets you
/// declare arbitrary per-channel bitmasks instead of assuming BGRA, so
/// we point the masks straight at our buffer's actual R/G/B/A byte
/// layout and copy each row wholesale -- no per-pixel work at all.
fn encode_bmp(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    const FILE_HEADER_SIZE: u32 = 14;
    const DIB_HEADER_SIZE: u32 = 108; // BITMAPV4HEADER
    const LCS_SRGB: u32 = 0x7352_4742;

    let row_bytes = width as usize * 4;
    let pixel_data_size = row_bytes * height as usize;
    let pixel_data_offset = FILE_HEADER_SIZE + DIB_HEADER_SIZE;
    let file_size = pixel_data_offset + pixel_data_size as u32;

    let mut out = Vec::with_capacity(pixel_data_offset as usize + pixel_data_size);

    // BITMAPFILEHEADER
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&file_size.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&pixel_data_offset.to_le_bytes());

    // BITMAPV4HEADER
    out.extend_from_slice(&DIB_HEADER_SIZE.to_le_bytes());
    out.extend_from_slice(&(width as i32).to_le_bytes());
    out.extend_from_slice(&(height as i32).to_le_bytes()); // positive => bottom-up rows
    out.extend_from_slice(&1u16.to_le_bytes()); // color planes
    out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
    out.extend_from_slice(&3u32.to_le_bytes()); // BI_BITFIELDS -- use the masks below
    out.extend_from_slice(&(pixel_data_size as u32).to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes()); // x pixels/meter
    out.extend_from_slice(&0i32.to_le_bytes()); // y pixels/meter
    out.extend_from_slice(&0u32.to_le_bytes()); // colors used
    out.extend_from_slice(&0u32.to_le_bytes()); // important colors
    out.extend_from_slice(&0x0000_00FFu32.to_le_bytes()); // red   = byte 0
    out.extend_from_slice(&0x0000_FF00u32.to_le_bytes()); // green = byte 1
    out.extend_from_slice(&0x00FF_0000u32.to_le_bytes()); // blue  = byte 2
    out.extend_from_slice(&0xFF00_0000u32.to_le_bytes()); // alpha = byte 3
    out.extend_from_slice(&LCS_SRGB.to_le_bytes());
    out.extend_from_slice(&[0u8; 36]); // CIEXYZTRIPLE endpoints, unused for sRGB
    out.extend_from_slice(&[0u8; 12]); // gamma red/green/blue, unused for sRGB

    debug_assert_eq!(out.len(), pixel_data_offset as usize);

    // BMP rows are stored bottom-up; our buffer is top-down, so walk the
    // source rows in reverse. Each row's bytes already match our chosen
    // masks exactly, so this is a straight copy, no per-pixel work.
    for row in (0..height as usize).rev() {
        let start = row * row_bytes;
        out.extend_from_slice(&rgba[start..start + row_bytes]);
    }

    out
}

fn text_response(code: u16, body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body).with_status_code(tiny_http::StatusCode(code))
}

#[cfg(test)]
mod tests {
    use super::encode_bmp;

    /// Round-trips a small, non-uniform RGBA buffer through our BMP
    /// encoder and `image`'s own BMP decoder (a completely independent
    /// implementation) to catch any mistake in the header/mask/row-order
    /// hand-rolling -- the encoder produces byte-for-byte pixel identity
    /// with the source, or this fails.
    #[test]
    fn encode_bmp_round_trips_through_image_crate() {
        let width = 3u32;
        let height = 2u32;
        // Distinct, non-symmetric R/G/B/A per pixel so a channel-order or
        // row-order bug can't accidentally still pass.
        let rgba: Vec<u8> = (0..(width * height))
            .flat_map(|i| {
                let i = i as u8;
                [i, i.wrapping_add(50), i.wrapping_add(100), i.wrapping_add(150)]
            })
            .collect();

        let bmp_bytes = encode_bmp(&rgba, width, height);

        let decoded = image::load_from_memory_with_format(&bmp_bytes, image::ImageFormat::Bmp)
            .expect("image crate failed to decode our BMP output")
            .into_rgba8();

        assert_eq!(decoded.dimensions(), (width, height));
        assert_eq!(decoded.into_raw(), rgba);
    }
}
