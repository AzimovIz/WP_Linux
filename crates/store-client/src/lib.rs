//! Blocking client for the WP Linux Store API (schema: `openapi.json` at
//! the repo root). Read-only for now -- listing/fetching/downloading
//! wallpapers -- accounts come later.
//!
//! This is plain blocking `ureq`, not async: the only consumer today is
//! `wp_linux_editor`, an egui app with no async runtime, and every call
//! here does real network I/O. Callers on a UI thread must run these from
//! a background thread themselves (e.g. `std::thread::spawn` + a channel
//! back to the UI) -- this crate makes no attempt to be non-blocking on
//! its own, it just makes sure a stuck connection can't hang a caller
//! forever (see `CONNECT_TIMEOUT`/`REQUEST_TIMEOUT`).

use std::io::Cursor;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use ureq::typestate::WithoutBody;

/// API base URL baked in at build time (see `build.rs`) -- `http://host:port`
/// or `https://host`, no trailing slash.
pub const DEFAULT_API_URL: &str = env!("WP_LINUX_API_URL");

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Wallpaper archives can carry several sizable image/video assets --
/// ureq's own default body-read limit (10MB) is too small for that.
const MAX_ARCHIVE_BYTES: u64 = 500 * 1024 * 1024;
const MAX_PREVIEW_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("request failed: {0}")]
    Request(#[from] ureq::Error),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("bad archive: {0}")]
    Zip(#[from] zip::result::ZipError),
}

/// Cheap to clone (`ureq::Agent` is internally `Arc`-backed and shares its
/// connection pool across clones) -- the natural way to hand a caller's own
/// background thread its own handle without re-resolving `base_url`/`token`.
#[derive(Clone)]
pub struct Client {
    agent: ureq::Agent,
    base_url: String,
    /// Bearer token for endpoints that need one -- unused today (every
    /// endpoint this crate calls works without one against the current
    /// server), wired in so account support has somewhere to plug in
    /// later instead of needing a signature change.
    token: Option<String>,
}

impl Default for Client {
    /// Points at [`DEFAULT_API_URL`], the address baked in at build time.
    fn default() -> Self {
        Self::new(DEFAULT_API_URL)
    }
}

impl Client {
    pub fn new(base_url: impl Into<String>) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_global(Some(REQUEST_TIMEOUT))
            .build();
        Self {
            agent: config.into(),
            base_url: base_url.into(),
            token: None,
        }
    }

    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    fn authed(
        &self,
        mut builder: ureq::RequestBuilder<WithoutBody>,
    ) -> ureq::RequestBuilder<WithoutBody> {
        if let Some(token) = &self.token {
            builder = builder.header("Authorization", format!("Bearer {token}"));
        }
        builder
    }

    pub fn list_wallpapers(&self, params: &ListParams) -> Result<WallpaperListOut, Error> {
        let mut req = self
            .agent
            .get(format!("{}/wallpapers", self.base_url))
            .query("skip", params.skip.to_string())
            .query("limit", params.limit.to_string());
        if let Some(max_rating) = params.max_rating {
            req = req.query("max_rating", max_rating.as_str());
        }
        for tag in &params.tags {
            req = req.query("tags", tag);
        }
        Ok(self.authed(req).call()?.body_mut().read_json()?)
    }

    pub fn get_wallpaper(&self, id: &str) -> Result<WallpaperOut, Error> {
        let req = self.agent.get(format!("{}/wallpapers/{id}", self.base_url));
        Ok(self.authed(req).call()?.body_mut().read_json()?)
    }

    /// Downloads the wallpaper's project archive (the `.zip` a store
    /// upload was built from) as raw bytes. Pass the result to
    /// [`extract_archive`] to unpack it into a library entry.
    pub fn download_archive(&self, id: &str) -> Result<Vec<u8>, Error> {
        let req = self
            .agent
            .get(format!("{}/wallpapers/{id}/download", self.base_url));
        Ok(self
            .authed(req)
            .call()?
            .body_mut()
            .with_config()
            .limit(MAX_ARCHIVE_BYTES)
            .read_to_vec()?)
    }

    pub fn download_preview(&self, id: &str) -> Result<Vec<u8>, Error> {
        let req = self
            .agent
            .get(format!("{}/wallpapers/{id}/preview", self.base_url));
        Ok(self
            .authed(req)
            .call()?
            .body_mut()
            .with_config()
            .limit(MAX_PREVIEW_BYTES)
            .read_to_vec()?)
    }
}

/// Extracts an archive downloaded via [`Client::download_archive`] into
/// `dest_dir` (created if missing), so `dest_dir` ends up holding
/// `project.json`/assets/`preview.png` directly -- ready for
/// `project_format::Project::load` or `library::scan`, same shape as any
/// locally-authored project. Uses `extract_unwrapped_root_dir` rather than
/// plain `extract`: the server zips a whole project directory (its own
/// internal id as the single top-level folder), and that wrapping folder
/// name is meaningless to us -- the caller already picked `dest_dir` as
/// this project's id/location.
///
/// Path traversal / symlink escape from a malicious entry is `zip`'s own
/// problem to guard against -- extraction sanitizes each entry's path via
/// `enclosed_name` and only follows symlinks whose target canonicalizes
/// inside `dest_dir` -- worth relying on here since these archives come
/// from other users' uploads via the store, not just from this machine's
/// own editor.
pub fn extract_archive(bytes: &[u8], dest_dir: &Path) -> Result<(), Error> {
    std::fs::create_dir_all(dest_dir)?;
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
    archive.extract_unwrapped_root_dir(dest_dir, zip::read::root_dir_common_filter)?;
    Ok(())
}

/// Query params for [`Client::list_wallpapers`]. Defaults match the
/// server's own (`skip=0`, `limit=20`).
#[derive(Debug, Clone)]
pub struct ListParams {
    pub tags: Vec<String>,
    pub max_rating: Option<ContentRating>,
    pub skip: u32,
    pub limit: u32,
}

impl Default for ListParams {
    fn default() -> Self {
        Self {
            tags: Vec::new(),
            max_rating: None,
            skip: 0,
            limit: 20,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentRating {
    #[serde(rename = "safe")]
    Safe,
    #[serde(rename = "pg")]
    Pg,
    #[serde(rename = "pg13")]
    Pg13,
    #[serde(rename = "r")]
    R,
    #[serde(rename = "x")]
    X,
    #[serde(rename = "xxx")]
    Xxx,
}

impl ContentRating {
    fn as_str(self) -> &'static str {
        match self {
            ContentRating::Safe => "safe",
            ContentRating::Pg => "pg",
            ContentRating::Pg13 => "pg13",
            ContentRating::R => "r",
            ContentRating::X => "x",
            ContentRating::Xxx => "xxx",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WallpaperStatus {
    #[serde(rename = "pending_review")]
    PendingReview,
    #[serde(rename = "published")]
    Published,
    #[serde(rename = "rejected")]
    Rejected,
    #[serde(rename = "removed")]
    Removed,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WallpaperListOut {
    pub items: Vec<WallpaperOut>,
    pub total: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WallpaperOut {
    pub id: String,
    pub title: String,
    pub description: String,
    pub author_id: String,
    pub author_username: String,
    pub tags: Vec<String>,
    pub rating: ContentRating,
    pub status: WallpaperStatus,
    pub format_version: Option<String>,
    /// The project's `project.json`, mirrored here so the store list can
    /// show layer/fps details without downloading the archive. Kept as
    /// loose JSON rather than `project_format::Project` directly: this
    /// field is informational (the archive itself is the source of truth,
    /// read back through `project_format::Project::load` the same way any
    /// local project is once `extract_archive` unpacks it), so one entry
    /// whose manifest doesn't match this build's `project_format` version
    /// shouldn't fail parsing the whole listing. Use [`WallpaperOut::manifest`]
    /// for a best-effort typed view.
    pub project_manifest: Option<serde_json::Value>,
    pub archive_size_bytes: u64,
    pub archive_checksum_sha256: String,
    pub downloads: u64,
    pub likes: u64,
    pub created_at: String,
    pub updated_at: String,
}

impl WallpaperOut {
    /// Best-effort typed parse of `project_manifest` -- `None` if absent
    /// or if it doesn't match this build's `project_format::Project`
    /// schema (e.g. a newer server introduced a layer type this binary
    /// doesn't know yet). Not needed for the download flow -- only for
    /// previewing details before downloading.
    pub fn manifest(&self) -> Option<project_format::Project> {
        self.project_manifest
            .as_ref()
            .and_then(|value| serde_json::from_value(value.clone()).ok())
    }
}
