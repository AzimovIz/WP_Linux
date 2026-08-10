//! Best-effort live push of a monitor's project assignment to the running
//! render-server, so a change made here applies immediately instead of
//! waiting for render-server's next restart to pick it up from
//! `monitors.json` (see `monitors_config`, which is the actual durable
//! source of truth -- this is purely a "make it happen now, if possible"
//! nicety on top of that).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

const RENDER_SERVER_ADDR: &str = "127.0.0.1:47824";
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

/// Abstracts the network call so `EditorApp`'s assignment logic (update
/// state -> save monitors.json -> push) can be unit-tested without a real
/// TCP connection or a running render-server -- exactly the situation in
/// this container, and generally true whenever render-server just isn't
/// up yet (an expected, non-fatal case, not an error condition to design
/// around).
pub trait ProjectPusher {
    fn push(&self, monitor_id: &str, project_dir: &Path) -> Result<(), String>;
}

pub struct TcpPusher;

impl ProjectPusher for TcpPusher {
    fn push(&self, monitor_id: &str, project_dir: &Path) -> Result<(), String> {
        push_project(monitor_id, project_dir)
    }
}

/// POSTs `project_dir` to render-server's `/project?monitor=<id>`,
/// mirroring exactly what the QML plugin's own `XMLHttpRequest` used to do
/// (body = raw path, trimmed server-side) -- hand-rolled instead of a new
/// HTTP client crate, matching this codebase's existing convention of
/// hand-rolling small protocol bits for narrow needs (see render-server's
/// own hand-rolled BMP encoder and query-string parser). No retry: a
/// connection failure just means render-server isn't running right now,
/// which `monitors_config` already covers for next startup.
fn push_project(monitor_id: &str, project_dir: &Path) -> Result<(), String> {
    let body = project_dir.display().to_string();
    let request = format!(
        "POST /project?monitor={monitor_id} HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len(),
    );

    let addr = RENDER_SERVER_ADDR
        .parse()
        .map_err(|e| format!("bad address: {e}"))?;
    let mut stream =
        TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| format!("connect: {e}"))?;
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("write: {e}"))?;

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| format!("read: {e}"))?;
    if response.starts_with("HTTP/1.1 200") {
        Ok(())
    } else {
        Err(format!(
            "render-server responded: {}",
            response.lines().next().unwrap_or("")
        ))
    }
}

#[cfg(test)]
pub mod test_support {
    use super::ProjectPusher;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// Records every call instead of touching the network -- lets tests
    /// assert what `EditorApp::assign` *tried* to push without needing a
    /// real render-server (which can't run headlessly in this container
    /// anyway).
    #[derive(Default)]
    pub struct RecordingPusher {
        pub calls: Mutex<Vec<(String, PathBuf)>>,
    }

    impl ProjectPusher for RecordingPusher {
        fn push(&self, monitor_id: &str, project_dir: &Path) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push((monitor_id.to_string(), project_dir.to_path_buf()));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ProjectPusher;
    use super::test_support::RecordingPusher;
    use std::path::PathBuf;

    #[test]
    fn recording_pusher_records_calls_without_touching_the_network() {
        let pusher = RecordingPusher::default();
        pusher
            .push("DP-1", &PathBuf::from("/tmp/some/project"))
            .unwrap();
        pusher
            .push("HDMI-A-1", &PathBuf::from("/tmp/other/project"))
            .unwrap();

        let calls = pusher.calls.lock().unwrap();
        assert_eq!(
            *calls,
            vec![
                ("DP-1".to_string(), PathBuf::from("/tmp/some/project")),
                ("HDMI-A-1".to_string(), PathBuf::from("/tmp/other/project")),
            ]
        );
    }
}
