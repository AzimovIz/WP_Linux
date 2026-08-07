//! The on-disk format for a wallpaper project: a directory containing a
//! `project.json` manifest plus whatever assets it references. Shared
//! between `editor` (writes it) and `render-server` (reads it) so the
//! schema only exists in one place.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const MANIFEST_FILE_NAME: &str = "project.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    /// Layers, bottom to top -- rendered in this order and alpha-blended
    /// on top of one another.
    pub layers: Vec<Layer>,
    /// Target render rate for animated/cursor-reactive layers (xray,
    /// gif). Irrelevant for a project made only of `Image` layers, which
    /// always render exactly once regardless of this value.
    #[serde(default = "default_fps")]
    pub fps: u32,
}

fn default_fps() -> u32 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Layer {
    /// A single static picture, relative path within the project dir.
    Image { path: String },
    /// A base picture with a second picture ("overlay") only visible in
    /// a circle around the cursor -- e.g. a night-vision/x-ray effect.
    Xray {
        base: String,
        overlay: String,
        radius: f32,
    },
    /// A looping GIF animation, relative path within the project dir.
    Gif { path: String },
    /// A single picture that pans opposite* the cursor to fake depth --
    /// stack several of these (background to foreground) with increasing
    /// `strength` for a full parallax effect. The renderer zooms the
    /// picture in just enough to cover the largest possible pan without
    /// exposing an edge, so any source picture works regardless of how
    /// much margin it actually has.
    Parallax {
        path: String,
        /// How far the layer pans, as a fraction of its own size, when
        /// the cursor is at the screen edge. 0.0 = static, and values
        /// much above ~0.45 make the auto-zoom very noticeable. Negative
        /// values pan the layer towards the cursor instead of away from
        /// it.
        strength: f32,
        /// Seconds for the pan to ease towards the cursor-driven target
        /// (exponential decay) -- 0.0 tracks the cursor instantly.
        smoothing: f32,
    },
}

impl Layer {
    /// Whether this layer needs continuous re-rendering (as opposed to
    /// being renderable once and left alone): anything that reacts to
    /// the cursor or animates on its own.
    pub fn is_dynamic(&self) -> bool {
        matches!(self, Layer::Xray { .. } | Layer::Gif { .. } | Layer::Parallax { .. })
    }
}

#[derive(Debug)]
pub enum LoadError {
    Io(io::Error),
    Json(serde_json::Error),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Io(e) => write!(f, "I/O error: {e}"),
            LoadError::Json(e) => write!(f, "invalid {MANIFEST_FILE_NAME}: {e}"),
        }
    }
}

impl std::error::Error for LoadError {}

impl From<io::Error> for LoadError {
    fn from(e: io::Error) -> Self {
        LoadError::Io(e)
    }
}

impl From<serde_json::Error> for LoadError {
    fn from(e: serde_json::Error) -> Self {
        LoadError::Json(e)
    }
}

impl Project {
    /// Loads `project.json` from inside `project_dir`. Layer asset paths
    /// inside the returned `Project` are still relative to `project_dir`
    /// -- resolve them yourself via the returned directory path.
    pub fn load(project_dir: &Path) -> Result<(Project, PathBuf), LoadError> {
        let manifest_path = project_dir.join(MANIFEST_FILE_NAME);
        let text = fs::read_to_string(&manifest_path)?;
        let project: Project = serde_json::from_str(&text)?;
        Ok((project, project_dir.to_path_buf()))
    }

    pub fn save(&self, project_dir: &Path) -> Result<(), LoadError> {
        fs::create_dir_all(project_dir)?;
        let text = serde_json::to_string_pretty(self)?;
        fs::write(project_dir.join(MANIFEST_FILE_NAME), text)?;
        Ok(())
    }
}
