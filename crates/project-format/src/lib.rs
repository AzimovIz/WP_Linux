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
    /// Human-readable name shown in the library picker. Empty for any
    /// project.json that predates this field.
    #[serde(default)]
    pub name: String,
    /// Free-form notes about the project (e.g. source/license of the
    /// pictures used) -- purely descriptive, never read by player or
    /// render-server. Empty for any project.json that predates this
    /// field.
    #[serde(default)]
    pub description: String,
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
    /// A string of text drawn at an arbitrary position on the canvas --
    /// unlike every other layer, not a full-canvas effect. Always drawn
    /// on top of every other layer regardless of its own position in
    /// this project's layer list (see player's `record_draw`).
    Text {
        /// Position as a fraction of canvas width/height (0.0..=1.0),
        /// not pixels -- deliberately resolution-independent, so the
        /// same project reads correctly regardless of which monitor's
        /// resolution it ends up assigned to.
        x: f32,
        y: f32,
        /// Font size as a fraction of canvas *height* -- same
        /// resolution-independence rationale as x/y.
        font_size: f32,
        /// RGBA, each channel 0.0..=1.0 -- matches `wgpu::Color`'s own
        /// convention, already used elsewhere in this codebase.
        color: [f32; 4],
        source: TextSource,
    },
}

/// Where a `Layer::Text`'s displayed string comes from.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TextSource {
    /// A fixed string, never changes.
    Literal { text: String },
    /// The current local time, re-formatted every tick -- `format` is a
    /// chrono strftime-style format string (e.g. `"%H:%M"`).
    Clock { format: String },
    /// A shell command's stdout, re-run every `interval_secs` -- e.g. a
    /// temperature/CPU widget. Failure, timeout, untrusted-project
    /// refusal, and empty output all display literally as `"NULL"`
    /// (decided up front -- never a distinct error UI); the real reason
    /// is always logged server-side instead. Only executed by
    /// render-server for a project whose id is in the local trust store
    /// -- see `player`'s `load_scene`'s `allow_commands` parameter.
    Command { command: String, interval_secs: u32 },
}

impl TextSource {
    /// Whether this source can ever produce a different string over
    /// time (as opposed to `Literal`, which is fixed forever).
    pub fn is_dynamic(&self) -> bool {
        match self {
            TextSource::Literal { .. } => false,
            TextSource::Clock { .. } | TextSource::Command { .. } => true,
        }
    }
}

impl Layer {
    /// Whether this layer needs continuous re-rendering (as opposed to
    /// being renderable once and left alone): anything that reacts to
    /// the cursor or animates on its own.
    pub fn is_dynamic(&self) -> bool {
        match self {
            Layer::Xray { .. } | Layer::Gif { .. } | Layer::Parallax { .. } => true,
            Layer::Text { source, .. } => source.is_dynamic(),
            Layer::Image { .. } => false,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_defaults_to_empty_for_pre_existing_project_json() {
        let project: Project =
            serde_json::from_str(r#"{"layers":[],"fps":30}"#).expect("should still parse");
        assert_eq!(project.name, "");
    }

    #[test]
    fn text_layer_with_literal_source_round_trips() {
        let layer = Layer::Text {
            x: 0.5,
            y: 0.1,
            font_size: 0.05,
            color: [1.0, 1.0, 1.0, 1.0],
            source: TextSource::Literal {
                text: "hello".to_string(),
            },
        };
        let json = serde_json::to_string(&layer).unwrap();
        let round_tripped: Layer = serde_json::from_str(&json).unwrap();
        assert!(!round_tripped.is_dynamic());
        match round_tripped {
            Layer::Text {
                x,
                y,
                font_size,
                color,
                source: TextSource::Literal { text },
            } => {
                assert_eq!(
                    (x, y, font_size, color, text.as_str()),
                    (0.5, 0.1, 0.05, [1.0; 4], "hello")
                );
            }
            _ => panic!("expected Layer::Text"),
        }
    }

    #[test]
    fn text_layer_with_clock_source_round_trips_and_is_dynamic() {
        let layer = Layer::Text {
            x: 0.05,
            y: 0.05,
            font_size: 0.04,
            color: [1.0, 1.0, 1.0, 1.0],
            source: TextSource::Clock {
                format: "%H:%M".to_string(),
            },
        };
        let json = serde_json::to_string(&layer).unwrap();
        let round_tripped: Layer = serde_json::from_str(&json).unwrap();
        assert!(round_tripped.is_dynamic());
        match round_tripped {
            Layer::Text {
                source: TextSource::Clock { format },
                ..
            } => {
                assert_eq!(format, "%H:%M");
            }
            _ => panic!("expected Layer::Text with TextSource::Clock"),
        }
    }

    #[test]
    fn text_layer_with_command_source_round_trips_and_is_dynamic() {
        let layer = Layer::Text {
            x: 0.9,
            y: 0.05,
            font_size: 0.03,
            color: [1.0, 1.0, 1.0, 1.0],
            source: TextSource::Command {
                command: "date".to_string(),
                interval_secs: 60,
            },
        };
        let json = serde_json::to_string(&layer).unwrap();
        let round_tripped: Layer = serde_json::from_str(&json).unwrap();
        assert!(round_tripped.is_dynamic());
        match round_tripped {
            Layer::Text {
                source:
                    TextSource::Command {
                        command,
                        interval_secs,
                    },
                ..
            } => {
                assert_eq!((command.as_str(), interval_secs), ("date", 60));
            }
            _ => panic!("expected Layer::Text with TextSource::Command"),
        }
    }
}
