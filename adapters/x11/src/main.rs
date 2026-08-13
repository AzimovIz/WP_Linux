//! X11 adapter for WP Linux -- the X11 equivalent of `adapters/kde` and
//! `adapters/gnome`, see this crate's README for the full picture. Talks
//! to `crates/render-server`'s already desktop-agnostic HTTP/D-Bus
//! contract (see its own module doc comment) the exact same way those
//! two do; nothing in render-server or `wp_linux_editor` changes for
//! this to work.
//!
//! Unlike KDE (two components: a Plasma plugin + a KWin script) or
//! GNOME (one Shell extension doing both jobs from inside `gnome-shell`),
//! this is a plain standalone process: X11 has no per-desktop-environment
//! scripting surface to hook into in the first place, and doesn't need
//! one here -- an ordinary client already has enough privilege to both
//! query the global cursor (see `cursor.rs`) and paint a window
//! (`window.rs`) at the bottom of the screen, which is exactly why this
//! same one process works unmodified under Cinnamon, MATE, XFCE, or any
//! other X11 window manager that honors `override_redirect` (i.e. all of
//! them, since it's core X11 protocol behavior, not a WM feature).
//!
//! One `MonitorWindow` per currently-connected RandR output (see
//! `randr.rs`), rebuilt from scratch on any `ScreenChangeNotify` (monitor
//! hot-plug/resize) instead of surgically diffing the old set against
//! the new one -- hotplug is rare enough that the brief flash while
//! windows are torn down and recreated isn't worth the extra complexity
//! of in-place updates.
//!
//! `adapters/x11/install.sh` only ever registers this to autostart when
//! the session was X11 *at install time*; `session::is_x11_session`
//! re-checks that independently on every launch, see its own doc
//! comment for why that still matters afterwards.

mod cursor;
mod pixfmt;
mod randr;
mod render_server;
mod session;
mod window;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use x11rb::connection::Connection;
use x11rb::protocol::Event;

use window::MonitorWindow;

fn main() {
    if !session::is_x11_session() {
        // Expected, not exceptional -- e.g. this process's autostart
        // entry lingering after switching to a Wayland session where
        // adapters/gnome or adapters/kde is now the one legitimately
        // running. See session.rs's doc comment for why this check
        // matters (XWayland) and can't just be "did connect() succeed."
        eprintln!(
            "wp-linux-x11-adapter: not an X11 session ($XDG_SESSION_TYPE/$WAYLAND_DISPLAY/$DISPLAY say so) -- exiting quietly"
        );
        return;
    }

    let (conn, screen_num) = match x11rb::connect(None) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("wp-linux-x11-adapter: couldn't connect to the X server: {e}");
            std::process::exit(1);
        }
    };
    let conn = Arc::new(conn);
    let root = conn.setup().roots[screen_num].root;

    let format = {
        let screen = &conn.setup().roots[screen_num];
        match pixfmt::detect(conn.setup(), screen) {
            Ok(format) => format,
            Err(e) => {
                eprintln!(
                    "wp-linux-x11-adapter: couldn't make sense of this X server's pixel format \
                     ({e}) -- this adapter only supports an 8-bit-per-channel TrueColor visual \
                     at 24 or 32 bits per pixel, which covers essentially every real X11 setup; \
                     please report this if you're seeing it on real hardware"
                );
                std::process::exit(1);
            }
        }
    };

    if let Err(e) = randr::watch_for_changes(&conn, root) {
        eprintln!(
            "wp-linux-x11-adapter: couldn't subscribe to RandR change notifications ({e}) -- \
             monitor hot-plug/resize won't be picked up automatically; restart this process \
             after reconfiguring monitors"
        );
    }

    eprintln!("wp-linux-x11-adapter: starting (pid {})", std::process::id());

    let cursor_exit = Arc::new(AtomicBool::new(false));
    {
        let conn = Arc::clone(&conn);
        let cursor_exit = Arc::clone(&cursor_exit);
        std::thread::spawn(move || cursor::run(&conn, root, &cursor_exit));
    }

    let mut monitors = spawn_all_monitors(&conn, screen_num, format, root);

    loop {
        match conn.wait_for_event() {
            Ok(Event::RandrScreenChangeNotify(_)) => {
                eprintln!("wp-linux-x11-adapter: monitor layout changed, refreshing");
                for m in monitors.drain(..) {
                    m.stop();
                }
                monitors = spawn_all_monitors(&conn, screen_num, format, root);
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("wp-linux-x11-adapter: X connection error, exiting: {e}");
                break;
            }
        }
    }
}

fn spawn_all_monitors(
    conn: &Arc<x11rb::rust_connection::RustConnection>,
    screen_num: usize,
    format: pixfmt::NativeFormat,
    root: u32,
) -> Vec<MonitorWindow> {
    let outputs = randr::active_outputs(conn, root);
    eprintln!("wp-linux-x11-adapter: {} active output(s)", outputs.len());

    outputs
        .into_iter()
        .filter_map(|geometry| {
            let name = geometry.name.clone();
            match MonitorWindow::spawn(Arc::clone(conn), screen_num, format, geometry) {
                Ok(m) => {
                    eprintln!("wp-linux-x11-adapter: showing monitor {name:?}");
                    Some(m)
                }
                Err(e) => {
                    eprintln!("wp-linux-x11-adapter: couldn't set up monitor {name:?}: {e}");
                    None
                }
            }
        })
        .collect()
}
