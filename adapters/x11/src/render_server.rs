//! Minimal blocking HTTP/1.0 client for render-server's local API (see
//! `crates/render-server/src/main.rs`'s module doc comment for the full
//! `/geometry`, `/meta`, `/frame` contract this adapter talks to).
//! Hand-rolled instead of pulling in a full HTTP client crate: the
//! protocol is tiny, fixed, and we control both ends of it -- same
//! reasoning render-server itself already applies to hand-writing its
//! own JSON bodies and query-string parsing instead of using `serde_json`
//! for those.
//!
//! HTTP/1.0 (not 1.1) is deliberate: 1.0's default is "close after this
//! response," so a plain `read_to_end()` on the socket is always
//! correct without having to parse `Content-Length` or worry about
//! keep-alive leaving us blocked waiting for bytes that aren't coming.
//! A fresh TCP connection per request is not free, but this is loopback
//! traffic to a process we spawned ourselves, polled at most a couple
//! hundred times a second -- not worth the complexity of a persistent,
//! reusable connection.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const RENDER_SERVER_ADDR: &str = "127.0.0.1:47824";
/// Same 2-second budget adapters/gnome/extension/renderServerClient.js
/// uses for its libsoup session -- local-only traffic to a process we
/// just spawned ourselves, no reason to ever wait longer than this.
const TIMEOUT: Duration = Duration::from_secs(2);

/// `GET path` (already including any query string), returning the
/// response body on any 2xx status, or `None` on a connection failure,
/// timeout, or non-2xx status -- callers treat that uniformly as "try
/// again next poll," same as every adapter's HTTP client already does.
pub fn get(path: &str) -> Option<Vec<u8>> {
    request("GET", path, None)
}

/// `POST path` with a small plain-text body. Fire-and-forget in the same
/// sense as `renderServerClient.js`'s `postText` -- callers only care
/// whether it succeeded, not the response body.
pub fn post(path: &str, body: &str) -> bool {
    request("POST", path, Some(body)).is_some()
}

fn request(method: &str, path: &str, body: Option<&str>) -> Option<Vec<u8>> {
    let mut stream = TcpStream::connect(RENDER_SERVER_ADDR).ok()?;
    stream.set_read_timeout(Some(TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(TIMEOUT)).ok()?;

    let body_bytes = body.map(str::as_bytes).unwrap_or(&[]);
    let mut head = format!(
        "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\r\n",
        body_bytes.len()
    )
    .into_bytes();
    head.extend_from_slice(body_bytes);
    stream.write_all(&head).ok()?;
    stream.flush().ok()?;

    let mut raw = Vec::new();
    // Ignore the error from a timed-out/reset read: whatever was already
    // appended to `raw` before that point is discarded below anyway
    // since we can't tell it apart from a truncated response -- treated
    // the same as any other "couldn't fetch this tick" failure.
    stream.read_to_end(&mut raw).ok()?;

    let header_end = find_double_crlf(&raw)?;
    let header_text = std::str::from_utf8(&raw[..header_end]).ok()?;
    let status_code: u16 = header_text
        .split("\r\n")
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    if !(200..300).contains(&status_code) {
        return None;
    }

    Some(raw[header_end + 4..].to_vec())
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// The fixed shape of `GET /meta`'s JSON body -- hand-parsed for the same
/// reason the request layer above is hand-rolled rather than pulling in
/// `serde_json` for one tiny, self-controlled, fixed-key object.
#[derive(Debug, Clone, Copy, Default)]
pub struct Meta {
    pub ready: bool,
    pub frame_id: u64,
    pub fps: u32,
    pub has_geometry: bool,
}

pub fn parse_meta(json: &str) -> Option<Meta> {
    Some(Meta {
        ready: json_bool(json, "\"ready\":")?,
        frame_id: json_u64(json, "\"frame_id\":")?,
        fps: json_u64(json, "\"fps\":")? as u32,
        has_geometry: json_bool(json, "\"has_geometry\":")?,
    })
}

fn json_field_start<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let idx = json.find(key)?;
    Some(&json[idx + key.len()..])
}

fn json_bool(json: &str, key: &str) -> Option<bool> {
    let rest = json_field_start(json, key)?;
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

fn json_u64(json: &str, key: &str) -> Option<u64> {
    let rest = json_field_start(json, key)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::parse_meta;

    #[test]
    fn parses_a_real_meta_response() {
        let json = r#"{"ready":true,"frame_id":42,"needs_cursor":false,"fps":30,"has_geometry":true}"#;
        let meta = parse_meta(json).expect("should parse");
        assert!(meta.ready);
        assert_eq!(meta.frame_id, 42);
        assert_eq!(meta.fps, 30);
        assert!(meta.has_geometry);
    }

    #[test]
    fn not_ready_yet_response() {
        let json = r#"{"ready":false,"frame_id":0,"needs_cursor":false,"fps":0,"has_geometry":false}"#;
        let meta = parse_meta(json).expect("should parse");
        assert!(!meta.ready);
        assert!(!meta.has_geometry);
    }
}
