//! Bridges the global cursor position (as seen by KWin, via a companion
//! KWin script calling us over D-Bus) to a tiny local HTTP endpoint that a
//! Plasma QML wallpaper plugin can poll.
//!
//! This exists because a Plasma wallpaper's `WallpaperItem` never sees
//! pointer motion while the desktop's Folder View is active: Folder View's
//! `MouseEventListener` sits above it in the scene and consumes hover
//! before it reaches the wallpaper. KWin itself always knows the true
//! cursor position regardless of which QML item currently owns input, so
//! we get it from there instead.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

use zbus::{blocking::connection, interface};

const BUS_NAME: &str = "dev.wplinux.CursorBridge";
const OBJECT_PATH: &str = "/dev/wplinux/CursorBridge";
const HTTP_ADDR: &str = "127.0.0.1:47823";

#[derive(Clone, Default)]
struct CursorState(Arc<(AtomicI32, AtomicI32)>);

impl CursorState {
    fn set(&self, x: i32, y: i32) {
        self.0 .0.store(x, Ordering::Relaxed);
        self.0 .1.store(y, Ordering::Relaxed);
    }

    fn get(&self) -> (i32, i32) {
        (
            self.0 .0.load(Ordering::Relaxed),
            self.0 .1.load(Ordering::Relaxed),
        )
    }
}

struct CursorService {
    state: CursorState,
}

#[interface(name = "dev.wplinux.CursorBridge")]
impl CursorService {
    #[zbus(name = "SetCursorPosition")]
    fn set_cursor_position(&mut self, x: i32, y: i32) {
        eprintln!("cursor-bridge: SetCursorPosition({x}, {y})");
        self.state.set(x, y);
    }
}

fn main() {
    eprintln!("cursor-bridge: starting (pid {})", std::process::id());

    let state = CursorState::default();

    let service = CursorService {
        state: state.clone(),
    };
    eprintln!("cursor-bridge: connecting to session D-Bus...");
    let _conn = connection::Builder::session()
        .expect("failed to connect to session D-Bus")
        .name(BUS_NAME)
        .expect("failed to request bus name -- is another instance already running? (pgrep -af cursor-bridge)")
        .serve_at(OBJECT_PATH, service)
        .expect("failed to register D-Bus object")
        .build()
        .expect("failed to build D-Bus connection");
    eprintln!("cursor-bridge: registered {BUS_NAME}{OBJECT_PATH}, waiting for SetCursorPosition calls");

    let server = tiny_http::Server::http(HTTP_ADDR)
        .unwrap_or_else(|e| panic!("failed to bind {HTTP_ADDR}: {e}"));
    eprintln!("cursor-bridge: serving http://{HTTP_ADDR}/cursor");

    let mut request_count: u64 = 0;
    for request in server.incoming_requests() {
        request_count += 1;
        let (x, y) = state.get();
        if request_count % 60 == 0 {
            eprintln!(
                "cursor-bridge: served {request_count} HTTP requests so far, current value ({x}, {y})"
            );
        }
        let body = format!("{{\"x\":{x},\"y\":{y}}}");
        let response = tiny_http::Response::from_string(body).with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
        );
        let _ = request.respond(response);
    }
}
