//! Renders each monitor's own wallpaper project's layer stack with wgpu
//! and serves the composited result to the Plasma QML wallpaper plugin
//! over a tiny local HTTP API. Every route takes a `?monitor=<id>` query
//! parameter (the QML side sends `Screen.name`) identifying which
//! monitor the request is for -- one process still runs, but it now
//! tracks one independently-loaded project per monitor instead of a
//! single project shared by the whole desktop:
//!
//!   POST /project?monitor=ID    body = absolute path to a project directory -> loads it
//!   POST /geometry?monitor=ID   body = "virtualX,virtualY,width,height" -> that
//!                    monitor's wallpaper item placement on the virtual desktop,
//!                    used to turn the global cursor position (see below) into
//!                    local, normalized coordinates for xray/parallax layers
//!   GET  /frame?monitor=ID      -> current frame, PNG (static project) or BMP (dynamic)
//!   GET  /meta?monitor=ID       -> {"ready": bool, "frame_id": u64, "needs_cursor": bool, "fps": u32, "has_geometry": bool}
//!
//! Cursor position itself does NOT come in over HTTP: this process also
//! registers the `dev.wplinux.CursorBridge` D-Bus service (formerly a
//! separate `cursor-bridge` binary) that a desktop-specific adapter feeds
//! with the true, compositor-level pointer position -- see
//! adapters/kde/kwin-script/package for KDE's implementation of that (a
//! KWin script). This process itself has no idea which desktop
//! environment it's running under -- every DE-specific integration lives
//! under adapters/, never here. Folding the D-Bus service in here means
//! one less process to start by hand, and lets the render loop react to
//! cursor movement without an HTTP round-trip through QML at all.
//!
//! Deliberately started by hand for now (autostart is still a TODO).
//!
//! Layers that don't react to the cursor or animate on their own (plain
//! `Image`) are rendered once and left alone. Layers that do (`Xray`,
//! `Gif`, `Parallax`) put the server into a continuous render loop, capped at the
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

mod monitors_config;
mod renderer;
mod trust_store;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use player::{LoadedLayer, SceneRenderer};
use project_format::{Layer, Project};
use renderer::{CANVAS_FORMAT, Canvas};
use tiny_http::{Method, Request, Response, Server};
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
    // Parallax easing needs a delta, not a running total (see
    // `SceneRenderer::update_parallax`) -- this is `loaded_at.elapsed()`
    // as of the last tick that actually called it.
    last_parallax_update_ms: u64,
    // Set on load, cleared once the first frame goes out -- makes sure a
    // freshly (re)loaded monitor gets one frame out immediately instead
    // of waiting for a gif/cursor change to trigger it. Was a loop-local
    // variable back when there was only ever one project; now one project
    // can finish loading mid-cycle while others are already rendering, so
    // it has to travel with its own `LoadedProject`.
    needs_render: bool,
    // Cursor UV this monitor's own last emitted frame was actually
    // rendered with -- lets the render loop skip a monitor's tick
    // entirely (no GPU work, no encode) when nothing that could change
    // its picture changed, e.g. an xray layer with a stationary cursor.
    // Same reasoning as `needs_render` for why this moved from a
    // loop-local into per-monitor state.
    last_rendered_cursor_uv: Option<(f32, f32)>,
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
    /// `/frame` and `/meta`. Never held while doing GPU work. Keyed by
    /// monitor id (the `Screen.name` QML sends as `?monitor=`) -- each
    /// monitor gets its own independently loaded project now instead of
    /// the whole desktop sharing one.
    output: Mutex<HashMap<String, FrameOutput>>,
    /// Global (virtual-desktop) cursor position in pixels, as last
    /// reported by the KWin script over D-Bus. `None` until the first
    /// report arrives. There is exactly one physical pointer for the
    /// whole desktop, so unlike `output`/`geometry` this stays a single
    /// value rather than being keyed by monitor.
    global_cursor: Mutex<Option<(i32, i32)>>,
    /// Each wallpaper item's placement on the virtual desktop --
    /// (virtualX, virtualY, width, height) -- as last pushed by QML via
    /// `/geometry`, keyed by monitor id. Needed to turn `global_cursor`
    /// into local, normalized coordinates for that specific monitor;
    /// only QML knows this, since render-server has no Wayland/Qt
    /// connection of its own.
    geometry: Mutex<HashMap<String, (f64, f64, f64, f64)>>,
    /// Set by `/project`, taken (and cleared) by the render thread on
    /// its next tick. Keyed by monitor id -- the latest POST for a given
    /// monitor wins, same as the old single-project behavior, just
    /// per-monitor instead of process-wide.
    pending_project: Mutex<HashMap<String, PathBuf>>,
    /// Mirrors whether power-profiles-daemon currently reports the
    /// "power-saver" profile, as last polled by
    /// `run_power_profile_watcher`. While set, the render loop skips all
    /// continuous re-rendering (gif frames, cursor reaction) and just
    /// keeps serving the last frame it already produced -- see
    /// `render_tick_loop`.
    power_saver: AtomicBool,
}

/// D-Bus object backing `dev.wplinux.CursorBridge` -- on KDE, the KWin
/// script (adapters/kde/kwin-script/package) calls `SetCursorPosition` on
/// this every time `workspace.cursorPosChanged` fires. Any other
/// desktop's adapter is free to call the same method the same way.
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

/// How long to wait between attempts to register the cursor D-Bus
/// service after a failed one -- see `run_cursor_dbus_service`.
const CURSOR_DBUS_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Registers the cursor D-Bus service and then parks forever, keeping
/// the connection alive for the life of the process. Runs in its own
/// thread so a D-Bus setup failure can't take down the HTTP server or
/// the render loop -- it just means xray layers won't see cursor
/// movement until registration succeeds.
///
/// Retries on failure instead of giving up after one attempt: the two
/// realistic failure causes -- a stale instance of this same service
/// still holding the bus name (e.g. a previous render-server process
/// that didn't exit cleanly), or the session D-Bus not being fully up
/// yet this early after login/reboot -- both tend to resolve themselves
/// within a few seconds. A one-shot `.expect()` here meant either of
/// those left cursor input permanently dead for the rest of the
/// process's life, with no way to recover short of restarting it by
/// hand.
fn run_cursor_dbus_service(state: Arc<SharedState>) {
    let connect = || -> zbus::Result<zbus::blocking::Connection> {
        let service = CursorDbusService {
            state: Arc::clone(&state),
        };
        connection::Builder::session()?
            .name(CURSOR_BUS_NAME)?
            .serve_at(CURSOR_OBJECT_PATH, service)?
            .build()
    };

    loop {
        match connect() {
            Ok(_conn) => {
                eprintln!(
                    "render-server: registered {CURSOR_BUS_NAME}{CURSOR_OBJECT_PATH}, waiting for SetCursorPosition calls"
                );
                loop {
                    std::thread::park();
                }
            }
            Err(e) => {
                eprintln!(
                    "render-server: failed to register {CURSOR_BUS_NAME} ({e}) -- retrying in \
                     {}s (xray/parallax layers won't see cursor movement until this succeeds)",
                    CURSOR_DBUS_RETRY_INTERVAL.as_secs(),
                );
                std::thread::sleep(CURSOR_DBUS_RETRY_INTERVAL);
            }
        }
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

/// Computes the current normalized (0..1) cursor position within one
/// specific monitor's wallpaper item, from the last-known global cursor
/// position and that monitor's last-reported screen geometry. `None` if
/// either piece is missing, that monitor has no geometry on file yet, or
/// the cursor is outside the item's bounds.
fn compute_cursor_uv(state: &SharedState, monitor_id: &str) -> Option<(f32, f32)> {
    let (gx, gy) = (*state.global_cursor.lock().unwrap())?;
    let (vx, vy, w, h) = *state.geometry.lock().unwrap().get(monitor_id)?;
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

    let state = Arc::new(SharedState::default());

    // Bind (and start listening on) the HTTP socket before doing anything
    // slow -- wgpu init below can take a real amount of time on a cold GPU
    // right after boot. The OS queues incoming TCP connections in its
    // accept backlog as soon as a socket is listening, even before we ever
    // call `incoming_requests()`, so binding first closes the startup race
    // that used to cost QML's one-shot `/geometry` POST (see main.qml's
    // own comment on why it has no retry): a request arriving in the old
    // window between process start and the HTTP bind got connection
    // refused, and for `/geometry` specifically was never retried --
    // leaving xray/parallax layers permanently blind to the cursor until
    // something recreated the wallpaper item. `/project` no longer needs a
    // retry at all (from QML or otherwise): its assignments are persisted
    // to disk by the editor app and reloaded below on every startup.
    let server =
        Server::http(HTTP_ADDR).unwrap_or_else(|e| panic!("failed to bind {HTTP_ADDR}: {e}"));
    eprintln!("render-server: listening on http://{HTTP_ADDR}");

    // Seed pending_project from disk before the render thread starts, so
    // its very first tick already has every monitor's last-known
    // assignment to load -- exactly the same map/lock `POST /project`
    // writes into at runtime, just pre-populated instead of waiting for an
    // HTTP push. editor is the sole writer of this file; this process only
    // ever reads it, once, here (see monitors_config's own doc comment).
    {
        let assignments = monitors_config::load();
        let count = assignments.len();
        state.pending_project.lock().unwrap().extend(assignments);
        eprintln!("render-server: loaded {count} monitor assignment(s) from disk");
    }

    eprintln!("render-server: initializing wgpu...");
    let renderer = Arc::new(pollster::block_on(SceneRenderer::new_headless(
        CANVAS_FORMAT,
    )));
    eprintln!("render-server: wgpu ready");

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

    eprintln!("render-server: serving http://{HTTP_ADDR} (no project loaded yet)");

    for request in server.incoming_requests() {
        let method = request.method().clone();
        // request.url() is the raw request-target and includes the query
        // string (e.g. "/frame?t=123&monitor=DP-1"); split path (for
        // routing) from query (for the monitor id) instead of discarding
        // the latter.
        let full_url = request.url().to_string();
        let (path, query) = full_url.split_once('?').unwrap_or((full_url.as_str(), ""));
        let monitor_id = query_param(query, "monitor");

        match (&method, path) {
            (Method::Post, "/project") => handle_project(request, monitor_id, &state),
            (Method::Post, "/geometry") => handle_geometry(request, monitor_id, &state),
            (Method::Get, "/frame") => handle_frame(request, monitor_id, &state),
            (Method::Get, "/meta") => handle_meta(request, monitor_id, &state),
            _ => {
                let _ = request.respond(text_response(404, "not found"));
            }
        }
    }
}

/// `POST /project?monitor=ID`, body = absolute path to a project
/// directory. Only records the path for the render thread to pick up on
/// its next tick (see `render_tick_loop`) -- never touches the GPU on
/// the HTTP thread.
fn handle_project(mut request: Request, monitor_id: Option<&str>, state: &SharedState) {
    let Some(monitor_id) = monitor_id else {
        let _ = request.respond(missing_monitor_response());
        return;
    };
    let mut body = String::new();
    if let Err(e) = request.as_reader().read_to_string(&mut body) {
        let _ = request.respond(text_response(400, &format!("bad request body: {e}")));
        return;
    }
    let project_dir = PathBuf::from(body.trim());
    eprintln!("render-server: received /project {project_dir:?} for monitor {monitor_id:?}");
    state
        .pending_project
        .lock()
        .unwrap()
        .insert(monitor_id.to_string(), project_dir);
    let _ = request.respond(text_response(200, "ok"));
}

/// `POST /geometry?monitor=ID`, body = "virtualX,virtualY,width,height".
fn handle_geometry(mut request: Request, monitor_id: Option<&str>, state: &SharedState) {
    let Some(monitor_id) = monitor_id else {
        let _ = request.respond(missing_monitor_response());
        return;
    };
    let mut body = String::new();
    let _ = request.as_reader().read_to_string(&mut body);
    match parse_geometry(body.trim()) {
        // Only overwrite this monitor's geometry on a successfully
        // parsed body -- a malformed POST (shouldn't happen, but was
        // previously handled by silently blanking geometry to `None`)
        // now just leaves whatever geometry this monitor already had, so
        // a bad request can't regress a working cursor.
        Some(geometry) => {
            eprintln!("render-server: received /geometry {geometry:?} for monitor {monitor_id:?}");
            state
                .geometry
                .lock()
                .unwrap()
                .insert(monitor_id.to_string(), geometry);
            let _ = request.respond(text_response(200, "ok"));
        }
        None => {
            eprintln!("render-server: bad /geometry body {body:?} for monitor {monitor_id:?}");
            let _ = request.respond(text_response(400, "bad geometry body"));
        }
    }
}

/// `GET /frame?monitor=ID` -> current frame, PNG (static project) or BMP
/// (dynamic), or 404 if nothing has ever rendered for this monitor yet.
fn handle_frame(request: Request, monitor_id: Option<&str>, state: &SharedState) {
    let Some(monitor_id) = monitor_id else {
        let _ = request.respond(missing_monitor_response());
        return;
    };
    let output = state.output.lock().unwrap();
    match output.get(monitor_id) {
        Some(output) if !output.frame_bytes.is_empty() => {
            let response = Response::from_data(output.frame_bytes.clone()).with_header(
                tiny_http::Header::from_bytes(
                    &b"Content-Type"[..],
                    output.frame_content_type.as_bytes(),
                )
                .unwrap(),
            );
            let _ = request.respond(response);
        }
        _ => {
            let _ = request.respond(text_response(404, "no project loaded for this monitor"));
        }
    }
}

/// `GET /meta?monitor=ID` -> `{"ready", "frame_id", "needs_cursor", "fps", "has_geometry"}`.
fn handle_meta(request: Request, monitor_id: Option<&str>, state: &SharedState) {
    let Some(monitor_id) = monitor_id else {
        let _ = request.respond(missing_monitor_response());
        return;
    };
    // No entry yet (monitor never loaded / render-server was restarted)
    // reports the same all-defaults "not ready" shape a fresh
    // `FrameOutput::default()` always did -- still a 200, not a 404, so
    // QML's "resend the project path until it sticks" retry in
    // `sceneMeta.poll()` keeps working exactly as it did for the
    // single-project case.
    let (ready, frame_id, needs_cursor, fps) = {
        let output = state.output.lock().unwrap();
        let entry = output.get(monitor_id);
        (
            entry.is_some_and(|o| o.ready),
            entry.map_or(0, |o| o.frame_id),
            entry.is_some_and(|o| o.needs_cursor),
            entry.map_or(0, |o| o.fps),
        )
    };
    // Same idea as the project-path retry mentioned above, but for
    // `/geometry`: it's normally pushed once, reactively, by QML (see
    // main.qml), with no retry of its own. Telling QML here whether we
    // actually have geometry on file for this monitor lets its poll loop
    // resend if a one-shot push got lost (e.g. it fired before this
    // process had even bound its HTTP port yet).
    let has_geometry = state.geometry.lock().unwrap().contains_key(monitor_id);
    let body = format!(
        "{{\"ready\":{ready},\"frame_id\":{frame_id},\"needs_cursor\":{needs_cursor},\"fps\":{fps},\"has_geometry\":{has_geometry}}}"
    );
    let response = Response::from_string(body).with_header(
        tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
    );
    let _ = request.respond(response);
}

/// Extracts the value of `key` from a raw query string
/// ("t=123&monitor=DP-1"), the same hand-rolled-parser style as
/// `parse_geometry` below -- monitor ids (`DP-1`, `eDP-1`, `HDMI-A-1`,
/// ...) are plain ASCII, so there's nothing a real URL-decoding crate
/// would buy here.
fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(v)
    })
}

fn missing_monitor_response() -> Response<std::io::Cursor<Vec<u8>>> {
    text_response(400, "missing ?monitor=<id> query parameter")
}

/// Runs forever in its own thread: picks up newly-posted project paths
/// for any monitor, re-composites each dynamic monitor's project (gif
/// animation, xray/parallax cursor reaction) at a capped rate, and
/// renders a static project exactly once. Idles almost for free when
/// nothing is loaded or every loaded monitor is fully static. Never
/// holds `state`'s locks while doing GPU work.
///
/// Deliberately one thread iterating over every monitor each cycle,
/// not one thread per monitor: `renderer::render_frame` does a
/// blocking `device.poll(wait_indefinitely())` per call, so N threads
/// hitting the same shared `wgpu::Device` concurrently would just
/// serialize on the device anyway -- a single loop gets the same
/// throughput with none of the added synchronization risk.
fn render_tick_loop(renderer: &SceneRenderer, state: &SharedState) {
    let mut monitors: HashMap<String, LoadedProject> = HashMap::new();

    loop {
        // Timed from here, not from the end of the previous sleep -- the
        // work below (GPU render, encode) is not instant, and sleeping
        // the full tick_interval regardless of how long that work took
        // would make the real cycle length tick_interval + work_time
        // instead of tick_interval, silently undershooting the fastest
        // loaded monitor's configured fps by however long a cycle takes
        // to run.
        let cycle_start = Instant::now();

        for (monitor_id, project_dir) in state.pending_project.lock().unwrap().drain() {
            match load_project(renderer, &project_dir) {
                Ok(loaded) => {
                    eprintln!(
                        "render-server: loaded project {:?} for monitor {monitor_id:?} \
                         ({} layer(s), dynamic = {}, fps target {:?})",
                        project_dir,
                        loaded.layers.len(),
                        loaded.dynamic,
                        loaded.tick_interval,
                    );
                    monitors.insert(monitor_id, loaded);
                }
                Err(e) => {
                    eprintln!(
                        "render-server: failed to load project {project_dir:?} for monitor \
                         {monitor_id:?}: {e}"
                    );
                }
            }
        }

        // While the system is in the power-saver profile, treat every
        // monitor exactly like a static project: don't advance gifs,
        // don't react to the cursor, don't push anything to the GPU.
        // `needs_render` (set on load) still gets one frame out below,
        // so a freshly loaded monitor isn't left blank -- it just
        // freezes on that frame instead of animating, same as a plain
        // `Image` layer always has. See `run_power_profile_watcher`.
        let power_saving = state.power_saver.load(Ordering::Relaxed);

        // Recomputed every cycle from whichever monitors are currently
        // loaded and dynamic -- e.g. a 60fps gif on one monitor must not
        // get throttled to a 5fps xray monitor's rate or vice versa.
        // Every monitor is still visited every cycle regardless (see the
        // loop below); this only controls how eagerly the *next* cycle
        // starts, and `advance_gifs`/`update_parallax` are cheap no-ops
        // when called more often than a given monitor actually needs, so
        // visiting a slow monitor at a fast monitor's rate costs a little
        // extra bookkeeping, not extra GPU work or over-fast animation.
        let mut next_tick_interval = IDLE_TICK_INTERVAL;

        for (monitor_id, project) in monitors.iter_mut() {
            if project.dynamic && !power_saving {
                next_tick_interval = next_tick_interval.min(project.tick_interval);
            }

            let gif_changed = if project.dynamic && !power_saving {
                let elapsed_ms = project.loaded_at.elapsed().as_millis() as u64;
                renderer.advance_gifs(&mut project.layers, elapsed_ms)
            } else {
                false
            };

            // Unlike gif/parallax, a Clock text layer needs real
            // wall-clock time, not elapsed time since `loaded_at` -- read
            // fresh every cycle. Its actual on-screen update cadence is
            // still bounded by this project's own `fps` (the same value
            // throttling gif/xray/parallax above), so a 1fps project with
            // a seconds-resolution clock will visibly stutter -- not a
            // bug, just a consequence of sharing that knob.
            let text_changed = if project.dynamic && !power_saving {
                renderer.advance_text_sources(&mut project.layers, chrono::Local::now())
            } else {
                false
            };

            let cursor_uv = if project.needs_cursor && !power_saving {
                compute_cursor_uv(state, monitor_id)
            } else {
                None
            };
            let cursor_changed = project.needs_cursor
                && !power_saving
                && cursor_uv != project.last_rendered_cursor_uv;

            // Unlike xray, parallax has state that eases towards a
            // target over several ticks -- it can still be moving after
            // the cursor itself has stopped, so `cursor_changed` alone
            // can't tell us whether a redraw is warranted. Advance it
            // unconditionally (like `advance_gifs` above) and trust its
            // own answer instead.
            let parallax_changed = if project.needs_cursor && !power_saving {
                let elapsed_ms = project.loaded_at.elapsed().as_millis() as u64;
                let dt_ms = elapsed_ms.saturating_sub(project.last_parallax_update_ms);
                project.last_parallax_update_ms = elapsed_ms;
                let cursor_px =
                    cursor_px_from_uv(cursor_uv, project.canvas_width, project.canvas_height);
                renderer.update_parallax(&mut project.layers, cursor_px, dt_ms)
            } else {
                false
            };

            if project.needs_render
                || gif_changed
                || cursor_changed
                || parallax_changed
                || text_changed
            {
                project.last_rendered_cursor_uv = cursor_uv;

                let (frame_bytes, content_type) = compose_and_encode(renderer, project, cursor_uv);

                let mut output = state.output.lock().unwrap();
                let entry = output.entry(monitor_id.clone()).or_default();
                entry.ready = true;
                entry.needs_cursor = project.needs_cursor;
                entry.fps = project.fps;
                entry.frame_bytes = frame_bytes;
                entry.frame_content_type = content_type;
                entry.frame_id += 1;
                drop(output);

                project.needs_render = false;
            }
        }

        // No point waking up at the fastest loaded monitor's animation
        // fps just to find power_saver still set and do nothing -- but
        // still check often enough that leaving power-saver mode resumes
        // animation quickly.
        let sleep_interval = if power_saving {
            POWER_PROFILE_POLL_INTERVAL
        } else {
            next_tick_interval
        };
        std::thread::sleep(sleep_interval.saturating_sub(cycle_start.elapsed()));
    }
}

/// Converts a normalized (0..1) cursor position within the wallpaper item
/// into canvas pixel coordinates, or a sentinel far off-canvas if there
/// currently isn't one (matches `player`'s own convention for "cursor
/// isn't anywhere near this layer" -- see `App::pointer_frame`'s `Leave`
/// handling).
fn cursor_px_from_uv(
    cursor_uv: Option<(f32, f32)>,
    canvas_width: u32,
    canvas_height: u32,
) -> (f32, f32) {
    cursor_uv
        .map(|(u, v)| (u * canvas_width as f32, v * canvas_height as f32))
        .unwrap_or((-1.0e6, -1.0e6))
}

fn compose_and_encode(
    renderer: &SceneRenderer,
    project: &LoadedProject,
    cursor_uv: Option<(f32, f32)>,
) -> (Vec<u8>, &'static str) {
    let cursor_px = cursor_px_from_uv(
        cursor_uv.filter(|_| project.needs_cursor),
        project.canvas_width,
        project.canvas_height,
    );

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

    // The library's own convention is already "id = the project
    // directory's own name" -- reusing it here means trust doesn't need
    // a dedicated id field threaded through `Project`/the HTTP endpoint.
    let project_id = project_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let allow_commands = trust_store::is_trusted(project_id);

    let mut loaded_layers = renderer.load_scene(&project_dir, &project, allow_commands)?;
    // Whichever layer happens to load first sets the canvas size, same as
    // this always worked before `load_scene` moved into `player`.
    let (canvas_width, canvas_height) = loaded_layers
        .first()
        .map(LoadedLayer::size)
        .expect("checked non-empty above");
    renderer.set_text_viewport(&mut loaded_layers, canvas_width, canvas_height);

    let dynamic = project.layers.iter().any(Layer::is_dynamic);
    let needs_cursor = project
        .layers
        .iter()
        .any(|l| matches!(l, Layer::Xray { .. } | Layer::Parallax { .. }));
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
        last_parallax_update_ms: 0,
        needs_render: true,
        last_rendered_cursor_uv: None,
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
    use crate::renderer;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_temp_dir() -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wplinux-render-server-test-{}-{}-{n}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Full round-trip through the *real* rendering code
    /// (`SceneRenderer::load_scene`/`record_draw`), headless (no display
    /// needed -- `SceneRenderer::new_headless` is exactly what this
    /// process itself uses), reading pixels back to actually verify the
    /// effect chain (M2) does something, not just that it compiles: a
    /// vignette over a solid white picture should leave the center
    /// close to white but darken the corner well below it.
    #[test]
    fn vignette_effect_darkens_corners_relative_to_center() {
        let project_dir = unique_temp_dir();
        let width = 64u32;
        let height = 64u32;
        image::RgbaImage::from_pixel(width, height, image::Rgba([255, 255, 255, 255]))
            .save(project_dir.join("bg.png"))
            .expect("failed to write test fixture PNG");

        let project = project_format::Project {
            name: String::new(),
            description: String::new(),
            fps: 30,
            layers: vec![project_format::Layer::Image {
                path: "bg.png".to_string(),
                effects: vec![project_format::Effect {
                    kind: project_format::EffectKind::Vignette {
                        strength: 1.0,
                        softness: 0.0,
                    },
                    mask: project_format::Mask::None,
                    enabled: true,
                }],
            }],
        };

        let scene_renderer =
            pollster::block_on(super::SceneRenderer::new_headless(renderer::CANVAS_FORMAT));
        let layers = scene_renderer
            .load_scene(&project_dir, &project, true)
            .expect("load_scene failed");

        let canvas = renderer::create_canvas(&scene_renderer, width, height);
        let pixels = renderer::render_frame(&scene_renderer, &canvas, &layers);

        let pixel_at = |x: u32, y: u32| -> [u8; 4] {
            let i = ((y * width + x) * 4) as usize;
            [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
        };
        let center = pixel_at(width / 2, height / 2);
        let corner = pixel_at(0, 0);

        assert!(
            center[0] > 200,
            "center should stay close to the original white, got {center:?}"
        );
        assert!(
            corner[0] < center[0],
            "corner should be darkened by the vignette relative to the center -- corner={corner:?} center={center:?}"
        );
    }

    /// The other half of the M2 regression check: a layer with an empty
    /// `effects` list must render byte-for-byte as if the whole effect
    /// chain machinery didn't exist -- `record_draw`'s fast path for the
    /// common (no-effects) case is the point of `EffectChain` being
    /// `Option`-al in the first place.
    #[test]
    fn layer_without_effects_is_pixel_identical_to_its_source() {
        let project_dir = unique_temp_dir();
        let width = 16u32;
        let height = 16u32;
        image::RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, 255]))
            .save(project_dir.join("bg.png"))
            .expect("failed to write test fixture PNG");

        let project = project_format::Project {
            name: String::new(),
            description: String::new(),
            fps: 30,
            layers: vec![project_format::Layer::Image {
                path: "bg.png".to_string(),
                effects: Vec::new(),
            }],
        };

        let scene_renderer =
            pollster::block_on(super::SceneRenderer::new_headless(renderer::CANVAS_FORMAT));
        let layers = scene_renderer
            .load_scene(&project_dir, &project, true)
            .expect("load_scene failed");

        let canvas = renderer::create_canvas(&scene_renderer, width, height);
        let pixels = renderer::render_frame(&scene_renderer, &canvas, &layers);

        assert_eq!(&pixels[0..4], &[10, 20, 30, 255]);
    }

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
                [
                    i,
                    i.wrapping_add(50),
                    i.wrapping_add(100),
                    i.wrapping_add(150),
                ]
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
