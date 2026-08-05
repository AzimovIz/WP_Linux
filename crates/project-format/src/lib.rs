//! The on-disk format for a wallpaper project: a directory containing a
//! `project.json` manifest plus whatever assets it references. Shared
//! between `editor` (writes it) and `render-server` (reads it) so the
//! schema only exists in one place.

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

pub const MANIFEST_FILE_NAME: &str = "project.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    /// Path to the image, relative to the project directory.
    pub image: String,
    /// Whether the host should draw the cursor-following glow on top of
    /// this scene. Purely a presentation flag for now -- the image itself
    /// doesn't react to the cursor.
    #[serde(default)]
    pub cursor_glow: bool,
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
    /// Loads `project.json` from inside `project_dir` and resolves the
    /// image path relative to it. Returns the manifest and the absolute
    /// image path together, since callers always need both.
    pub fn load(project_dir: &Path) -> Result<(Project, std::path::PathBuf), LoadError> {
        let manifest_path = project_dir.join(MANIFEST_FILE_NAME);
        let text = fs::read_to_string(&manifest_path)?;
        let project: Project = serde_json::from_str(&text)?;
        let image_path = project_dir.join(&project.image);
        Ok((project, image_path))
    }

    pub fn save(&self, project_dir: &Path) -> Result<(), LoadError> {
        fs::create_dir_all(project_dir)?;
        let text = serde_json::to_string_pretty(self)?;
        fs::write(project_dir.join(MANIFEST_FILE_NAME), text)?;
        Ok(())
    }
}
