//! Owns one monitor's on-screen wallpaper window: creating it, keeping
//! it pinned to the bottom of the stacking order, and repeatedly
//! fetching+showing whatever render-server currently has ready for that
//! monitor -- the X11 equivalent of `adapters/kde/plasma-plugin`'s QML
//! `WallpaperItem` or `adapters/gnome/extension/monitorLayer.js`'s
//! `MonitorLayer`.
//!
//! ## Placement strategy
//!
//! This window is created `override_redirect = true`: by X11 protocol
//! definition, an override-redirect window is never touched by the
//! window manager at all (no reparenting into a decoration frame, no
//! stacking/placement policy, nothing) -- which also means setting EWMH
//! hints like `_NET_WM_WINDOW_TYPE_DESKTOP` on one would be silently
//! ignored, since only a WM-managed window's properties are ever read by
//! the WM. So this deliberately does *not* try that route. Instead, once
//! mapped, it asks the server directly to drop it to the very bottom of
//! the sibling stack under root (`ConfigureWindow` with
//! `stack_mode = Below`) -- the same technique `xwinwrap -ni` and
//! similar X11 "live wallpaper" tools use, and one that doesn't depend
//! on any particular window manager's cooperation. This is only done
//! once, right after creation; if some other client (most commonly a
//! desktop-icons manager like Nemo/Caja/PCManFM's desktop mode, which
//! usually also runs as its own always-on-top-of-wallpaper window) maps
//! *after* this and ends up stacked below us for some reason, there's no
//! periodic re-lowering here to fight over it -- not observed as a
//! problem in testing so far, but untested across window
//! managers/desktop-icon managers, see this crate's README.
//!
//! ## Repainting when uncovered
//!
//! No `Expose` handling here: `backing_store` is requested on creation
//! (best-effort -- the server is free to ignore it) specifically so the
//! X server itself restores previously-obscured pixels when something
//! that was overlapping this window moves away, without involving this
//! process at all. If a particular X server/driver combination doesn't
//! honor it, an animating (Gif/Xray/Parallax/etc.) wallpaper self-heals
//! on its own next frame regardless; only a perfectly static `Image`
//! project could show a stale patch where something used to overlap it,
//! and only until the next full redraw. See the crate README's known
//! limitations.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::{
    BackingStore, ConfigureWindowAux, ConnectionExt as XProtoConnectionExt, CreateGCAux,
    CreateWindowAux, Gcontext, ImageFormat, Screen, StackMode, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;

use crate::pixfmt::{self, NativeFormat};
use crate::randr::OutputGeometry;
use crate::render_server;

pub struct MonitorWindow {
    exit: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    name: String,
}

impl MonitorWindow {
    /// Creates and maps the X11 window synchronously (so by the time
    /// this returns, something is already on screen -- even if it's
    /// just the `background_pixel` fill until the first frame arrives),
    /// then spawns the poll/display loop on its own thread.
    pub fn spawn(
        conn: Arc<RustConnection>,
        screen_num: usize,
        format: NativeFormat,
        geometry: OutputGeometry,
    ) -> Result<Self, String> {
        let (window, gc) = {
            let screen = &conn.setup().roots[screen_num];
            create_and_map(&conn, screen, &format, &geometry)?
        };

        let exit = Arc::new(AtomicBool::new(false));
        let name = geometry.name.clone();
        let thread_exit = Arc::clone(&exit);
        let thread_name = name.clone();
        let thread = std::thread::Builder::new()
            .name(format!("wp-x11-{thread_name}"))
            .spawn(move || {
                run(&conn, window, gc, &format, &geometry, &thread_exit);
                let _ = conn.destroy_window(window);
                let _ = conn.free_gc(gc);
                let _ = conn.flush();
            })
            .map_err(|e| format!("failed to spawn poll thread: {e}"))?;

        Ok(Self {
            exit,
            thread: Some(thread),
            name,
        })
    }

    /// Signals the poll loop to stop and waits for it to tear the window
    /// down. Never panics on a poisoned thread -- a monitor's display
    /// thread dying is logged from inside `run` and simply leaves that
    /// monitor blank, not something that should take the whole adapter
    /// down when we're just trying to clean up during a hotplug refresh.
    pub fn stop(mut self) {
        self.exit.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        eprintln!("wp-linux-x11-adapter: stopped monitor {:?}", self.name);
    }
}

fn create_and_map(
    conn: &RustConnection,
    screen: &Screen,
    format: &NativeFormat,
    geometry: &OutputGeometry,
) -> Result<(Window, Gcontext), String> {
    let window = conn.generate_id().map_err(|e| e.to_string())?;
    let gc = conn.generate_id().map_err(|e| e.to_string())?;

    let aux = CreateWindowAux::default()
        .background_pixel(screen.black_pixel)
        .override_redirect(1)
        .backing_store(BackingStore::WHEN_MAPPED);

    conn.create_window(
        format.depth,
        window,
        screen.root,
        geometry.x,
        geometry.y,
        geometry.width,
        geometry.height,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &aux,
    )
    .map_err(|e| e.to_string())?
    .check()
    .map_err(|e| format!("CreateWindow failed: {e}"))?;

    conn.create_gc(gc, window, &CreateGCAux::default())
        .map_err(|e| e.to_string())?
        .check()
        .map_err(|e| format!("CreateGC failed: {e}"))?;

    conn.map_window(window)
        .map_err(|e| e.to_string())?
        .check()
        .map_err(|e| format!("MapWindow failed: {e}"))?;

    // See this module's doc comment -- pushed to the very bottom of the
    // stack once, right after mapping.
    conn.configure_window(window, &ConfigureWindowAux::new().stack_mode(StackMode::BELOW))
        .map_err(|e| e.to_string())?
        .check()
        .map_err(|e| format!("ConfigureWindow(stack_mode=Below) failed: {e}"))?;

    conn.flush().map_err(|e| e.to_string())?;

    Ok((window, gc))
}

/// Poll/display loop for one monitor -- pushes this monitor's geometry,
/// then repeatedly asks render-server whether a new frame is ready
/// (`GET /meta`) and fetches+shows it (`GET /frame`) when it is, at
/// whatever rate the loaded project's own `fps` calls for. Mirrors
/// `adapters/gnome/extension/monitorLayer.js`'s `_poll`/`_fetchFrame`
/// almost exactly, just in Rust talking directly to X11 instead of GJS
/// talking to Clutter/St.
fn run(
    conn: &RustConnection,
    window: Window,
    gc: Gcontext,
    format: &NativeFormat,
    geometry: &OutputGeometry,
    exit: &AtomicBool,
) {
    let monitor_query = format!("monitor={}", url_encode(&geometry.name));
    let max_request_bytes = conn.maximum_request_bytes();

    push_geometry(&monitor_query, geometry);

    let mut fps: u32 = 30;
    let mut last_frame_id: u64 = u64::MAX;

    while !exit.load(Ordering::Relaxed) {
        if let Some(meta) = fetch_meta(&monitor_query) {
            if meta.fps != 0 {
                fps = meta.fps;
            }
            if !meta.has_geometry {
                // render-server may have (re)started since we last
                // pushed this -- same "resend until it sticks" retry
                // every other adapter's client already does.
                push_geometry(&monitor_query, geometry);
            }
            if meta.ready && meta.frame_id != last_frame_id {
                last_frame_id = meta.frame_id;
                if let Some(frame_bytes) = render_server::get(&format!("/frame?{monitor_query}"))
                    && let Err(e) =
                        show_frame(conn, window, gc, format, geometry, max_request_bytes, &frame_bytes)
                    {
                        eprintln!(
                            "wp-linux-x11-adapter: monitor {:?} couldn't show a frame: {e}",
                            geometry.name
                        );
                    }
            }
        }

        let interval_ms = (1000 / u64::from(fps.max(1))).max(8);
        sleep_checking_exit(Duration::from_millis(interval_ms), exit);
    }
}

fn fetch_meta(monitor_query: &str) -> Option<render_server::Meta> {
    let bytes = render_server::get(&format!("/meta?{monitor_query}"))?;
    let text = std::str::from_utf8(&bytes).ok()?;
    render_server::parse_meta(text)
}

fn push_geometry(monitor_query: &str, geometry: &OutputGeometry) {
    let body = format!("{},{},{},{}", geometry.x, geometry.y, geometry.width, geometry.height);
    render_server::post(&format!("/geometry?{monitor_query}"), &body);
}

/// Decodes whatever render-server sent (PNG for a static project, BMP
/// for a dynamic one -- `image::load_from_memory` sniffs the format
/// itself, so this doesn't need to care which), stretches it to the
/// monitor's own pixel size if it doesn't already match (render-server
/// renders at the *project's* configured canvas resolution, not
/// necessarily the monitor's -- same "stretch, don't aspect-crop"
/// simplification `adapters/gnome` already documents as a known
/// limitation, see this crate's README), converts to the server's
/// native pixel layout, and blits it in via chunked `PutImage` calls
/// (chunked because a single request can't exceed
/// `maximum_request_bytes` -- large enough in practice that most frames
/// still go out as one or two calls once the BigRequests extension is
/// in play, which essentially every modern X server has).
fn show_frame(
    conn: &RustConnection,
    window: Window,
    gc: Gcontext,
    format: &NativeFormat,
    geometry: &OutputGeometry,
    max_request_bytes: usize,
    frame_bytes: &[u8],
) -> Result<(), String> {
    let decoded = image::load_from_memory(frame_bytes).map_err(|e| e.to_string())?;
    let mut rgba = decoded.into_rgba8();

    let (target_w, target_h) = (u32::from(geometry.width), u32::from(geometry.height));
    if rgba.dimensions() != (target_w, target_h) {
        rgba = image::imageops::resize(&rgba, target_w, target_h, image::imageops::FilterType::Nearest);
    }

    let native = pixfmt::convert_rgba(format, rgba.as_raw());

    let width = usize::from(geometry.width);
    let height = usize::from(geometry.height);
    let row_bytes = width * 4;
    if row_bytes == 0 {
        return Ok(());
    }
    // Conservative fixed overhead for PutImage's own request header
    // (24 bytes) plus a little slack -- doesn't need to be exact, just
    // safely under the real limit.
    const REQUEST_HEADER_SLACK: usize = 64;
    let rows_per_chunk = max_request_bytes
        .saturating_sub(REQUEST_HEADER_SLACK)
        .checked_div(row_bytes)
        .unwrap_or(0)
        .max(1);

    let mut y = 0usize;
    while y < height {
        let rows = rows_per_chunk.min(height - y);
        let start = y * row_bytes;
        let end = start + rows * row_bytes;
        conn.put_image(
            ImageFormat::Z_PIXMAP,
            window,
            gc,
            geometry.width,
            rows as u16,
            0,
            y as i16,
            0,
            format.depth,
            &native[start..end],
        )
        .map_err(|e| e.to_string())?;
        y += rows;
    }
    conn.flush().map_err(|e| e.to_string())?;
    Ok(())
}

/// `setInterval`-style sleep that still notices `exit` promptly instead
/// of blocking through the whole interval -- `stop()` shouldn't have to
/// wait up to a full frame period to take effect.
fn sleep_checking_exit(total: Duration, exit: &AtomicBool) {
    const STEP: Duration = Duration::from_millis(20);
    let mut remaining = total;
    while remaining > Duration::ZERO {
        if exit.load(Ordering::Relaxed) {
            return;
        }
        let step = remaining.min(STEP);
        std::thread::sleep(step);
        remaining -= step;
    }
}

/// Percent-encodes a monitor name for use in a query string -- RandR
/// output names are normally already plain ASCII (`eDP-1`, `DP-1`,
/// `HDMI-1`), this is just insurance against a stranger one, same
/// reasoning as `adapters/gnome`'s use of `encodeURIComponent` for the
/// same value.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
