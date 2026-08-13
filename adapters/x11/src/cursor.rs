//! Forwards the true global cursor position to render-server's
//! `dev.wplinux.CursorBridge` D-Bus service -- the X11 equivalent of
//! `adapters/kde/kwin-script` and
//! `adapters/gnome/extension/cursorForwarder.js`.
//!
//! Unlike either of those, no compositor-side privilege is needed here:
//! on Wayland, per-surface pointer sandboxing means an ordinary client
//! only ever sees pointer coordinates local to its own surface and only
//! while it has pointer focus -- that's the whole reason KDE needs a
//! KWin script and GNOME needs a Shell extension, both running with the
//! compositor's own privileges, just to answer "where is the cursor
//! right now, regardless of what's under it." X11 has no such
//! restriction: any ordinary client can ask the server directly via
//! `QueryPointer` on the root window and get the true global position,
//! the same call `xdotool getmouselocation` makes. So this is a plain
//! polling loop in this same process, not a separate privileged script.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

const CURSOR_BUS_NAME: &str = "dev.wplinux.CursorBridge";
const CURSOR_OBJECT_PATH: &str = "/dev/wplinux/CursorBridge";
const CURSOR_INTERFACE: &str = "dev.wplinux.CursorBridge";

/// Same ~120Hz cap `adapters/kde/kwin-script` and
/// `adapters/gnome/extension/cursorForwarder.js` both use.
const POLL_INTERVAL: Duration = Duration::from_millis(8);

/// Runs forever (until `exit` is set), polling `QueryPointer` and
/// calling `SetCursorPosition` on render-server's D-Bus service whenever
/// the position actually changed. Reconnects to D-Bus on failure rather
/// than giving up permanently -- render-server (and the session bus
/// right after login) may not be up yet the first time this runs, same
/// reasoning render-server's own `run_cursor_dbus_service` already
/// documents for the other end of this same connection.
pub fn run(conn: &RustConnection, root: u32, exit: &AtomicBool) {
    let mut dbus: Option<zbus::blocking::Connection> = None;
    let mut last: Option<(i32, i32)> = None;

    while !exit.load(Ordering::Relaxed) {
        if dbus.is_none() {
            match zbus::blocking::Connection::session() {
                Ok(c) => dbus = Some(c),
                Err(e) => {
                    eprintln!(
                        "wp-linux-x11-adapter: couldn't connect to the session D-Bus ({e}), \
                         retrying -- cursor-reactive layers won't see movement until this succeeds"
                    );
                    std::thread::sleep(Duration::from_secs(5));
                    continue;
                }
            }
        }

        match conn.query_pointer(root).map(|c| c.reply()) {
            Ok(Ok(reply)) => {
                let pos = (i32::from(reply.root_x), i32::from(reply.root_y));
                if Some(pos) != last {
                    last = Some(pos);
                    if let Some(connection) = &dbus {
                        let result = connection.call_method(
                            Some(CURSOR_BUS_NAME),
                            CURSOR_OBJECT_PATH,
                            Some(CURSOR_INTERFACE),
                            "SetCursorPosition",
                            &(pos.0, pos.1),
                        );
                        // render-server may not be running (yet) -- best
                        // effort, same as every other adapter's cursor
                        // forwarder. A D-Bus-level failure (as opposed to
                        // this process's own connection dying) doesn't
                        // warrant reconnecting.
                        if let Err(e) = result {
                            eprintln!("wp-linux-x11-adapter: SetCursorPosition call failed: {e}");
                        }
                    }
                }
            }
            Ok(Err(e)) => {
                eprintln!("wp-linux-x11-adapter: QueryPointer reply failed: {e}");
            }
            Err(e) => {
                eprintln!("wp-linux-x11-adapter: QueryPointer failed: {e}");
            }
        }

        std::thread::sleep(POLL_INTERVAL);
    }
}
