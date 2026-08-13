//! Monitor discovery via the RandR extension -- X11's own model of
//! physical outputs, the same information `xrandr --query` prints and
//! (via `winit`) what `wp_linux_editor`'s own `MonitorInfo` already uses
//! as a monitor id (see its doc comment) -- so an output's RandR name
//! here (`eDP-1`, `DP-1`, `HDMI-1`, ...) should line up with the
//! `?monitor=` id `wp_linux_editor` already assigns projects under, the
//! same assumption `adapters/gnome/extension/extension.js` already makes
//! for the Wayland connector name.

use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

#[derive(Debug, Clone, PartialEq)]
pub struct OutputGeometry {
    pub name: String,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

/// Every currently connected, active (has a CRTC with a non-zero mode)
/// output -- mirrors GNOME's `_syncMonitors` only iterating
/// `Main.layoutManager.monitors`, not every physical port the machine
/// happens to have. Best-effort: a single output's info/CRTC request
/// failing (e.g. it got unplugged between the two calls) just drops that
/// output for this cycle rather than aborting discovery entirely.
pub fn active_outputs(conn: &RustConnection, root: u32) -> Vec<OutputGeometry> {
    let mut result = Vec::new();

    let resources = match conn.randr_get_screen_resources_current(root) {
        Ok(cookie) => match cookie.reply() {
            Ok(resources) => resources,
            Err(e) => {
                eprintln!("wp-linux-x11-adapter: RRGetScreenResourcesCurrent reply failed: {e}");
                return result;
            }
        },
        Err(e) => {
            eprintln!("wp-linux-x11-adapter: RRGetScreenResourcesCurrent failed: {e}");
            return result;
        }
    };

    for output in resources.outputs {
        let Ok(Ok(info)) = conn
            .randr_get_output_info(output, resources.config_timestamp)
            .map(|cookie| cookie.reply())
        else {
            continue;
        };
        if info.connection != randr::Connection::CONNECTED || info.crtc == 0 {
            continue;
        }
        let Ok(Ok(crtc)) = conn
            .randr_get_crtc_info(info.crtc, resources.config_timestamp)
            .map(|cookie| cookie.reply())
        else {
            continue;
        };
        if crtc.width == 0 || crtc.height == 0 {
            continue;
        }
        result.push(OutputGeometry {
            name: String::from_utf8_lossy(&info.name).into_owned(),
            x: crtc.x,
            y: crtc.y,
            width: crtc.width,
            height: crtc.height,
        });
    }

    result
}

/// Subscribes to RandR's `ScreenChangeNotify` on the root window --
/// fires on monitor hot-plug/unplug and on any output being
/// resized/repositioned (external monitor plugged in, resolution
/// changed, laptop lid docking/undocking a second screen, ...). `main`
/// just calls `active_outputs` again from scratch every time this event
/// arrives rather than trying to diff what changed -- see its own doc
/// comment for why.
pub fn watch_for_changes(conn: &RustConnection, root: u32) -> Result<(), String> {
    randr::select_input(conn, root, randr::NotifyMask::SCREEN_CHANGE)
        .map_err(|e| e.to_string())?
        .check()
        .map_err(|e| e.to_string())
}
