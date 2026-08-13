//! Defense-in-depth session-type check, run before doing anything else
//! in `main`. `adapters/x11/install.sh`'s own `detect_session_type`
//! already decides *whether to install* this adapter's autostart entry
//! in the first place, but that decision is only re-evaluated when
//! `install.sh` runs again -- if a user later switches their default
//! session to Wayland without reinstalling, the autostart `.desktop`
//! file installed for X11 just keeps existing and would still fire.
//!
//! Importantly, this checks environment variables rather than simply
//! trying to connect to an X server and seeing if that succeeds: modern
//! GNOME/KWin Wayland sessions transparently start XWayland for
//! X11-app compatibility, so `x11rb::connect` can succeed there too --
//! a bare "can I connect" check would happily attach this adapter to
//! that compat layer and fight over the screen with the real Wayland
//! adapter (`adapters/gnome`/`adapters/kde`) that's already legitimately
//! running. Checking `$XDG_SESSION_TYPE` (what actually started this
//! session) sidesteps that entirely.

/// Same detection order as `adapters/x11/install.sh`'s own
/// `detect_session_type`, kept in sync by hand since one's shell and the
/// other's Rust -- if this ever needs to change, change both.
pub fn is_x11_session() -> bool {
    match std::env::var("XDG_SESSION_TYPE") {
        Ok(v) if v == "x11" => return true,
        Ok(v) if v == "wayland" => return false,
        _ => {}
    }
    // $XDG_SESSION_TYPE unset (e.g. a bare `startx` with no display
    // manager) -- fall back to which display socket is actually
    // present.
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        false
    } else {
        std::env::var_os("DISPLAY").is_some()
    }
}
