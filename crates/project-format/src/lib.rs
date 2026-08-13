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
    Image {
        path: String,
        /// Post-processing stack applied to this layer's own rendered
        /// output, bottom to top -- see [`Effect`]. Empty for any
        /// project.json that predates this field.
        #[serde(default)]
        effects: Vec<Effect>,
    },
    /// A base picture with a second picture ("overlay") only visible in
    /// a circle around the cursor -- e.g. a night-vision/x-ray effect.
    Xray {
        base: String,
        overlay: String,
        radius: f32,
        #[serde(default)]
        effects: Vec<Effect>,
    },
    /// A looping GIF animation, relative path within the project dir.
    Gif {
        path: String,
        #[serde(default)]
        effects: Vec<Effect>,
    },
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
        #[serde(default)]
        effects: Vec<Effect>,
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
    /// Not a picture of its own -- takes whatever every layer *below* it
    /// in this project's layer list has already composited into and
    /// runs its own post-processing stack on that, same generic
    /// `Effect`/`Mask` machinery as any other layer. See `Ideas.md`,
    /// "Adjustment-слой". Like `Text`, it has an exception to the usual
    /// "layers stack in list order" rule: a `Text` layer always draws
    /// on top of *everything*, including any `Adjustment` layer above
    /// it in the list, so an `Adjustment` layer can never affect text
    /// (see player's `record_draw`).
    Adjustment {
        #[serde(default)]
        effects: Vec<Effect>,
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
    /// This layer's post-processing stack, or an empty slice for `Text`
    /// (the one variant that doesn't have one -- see `Text`'s own doc
    /// comment). Lets callers that need to look at effects regardless
    /// of which layer variant they're attached to (e.g. checking for an
    /// enabled `EffectKind::Smoke`, see `is_dynamic` below) do so
    /// without repeating this match themselves.
    pub fn effects(&self) -> &[Effect] {
        match self {
            Layer::Image { effects, .. }
            | Layer::Xray { effects, .. }
            | Layer::Gif { effects, .. }
            | Layer::Parallax { effects, .. }
            | Layer::Adjustment { effects, .. } => effects,
            Layer::Text { .. } => &[],
        }
    }

    /// Whether this layer needs continuous re-rendering (as opposed to
    /// being renderable once and left alone): anything that reacts to
    /// the cursor or animates on its own. An otherwise-static `Image`
    /// becomes dynamic too once it carries an enabled `Smoke` or `Shader`
    /// effect -- unlike every effect kind above them (which only
    /// reshapes whatever the layer already draws, as a pure function of
    /// its current content), `Smoke` has its own persistent GPU state
    /// that evolves every frame regardless of what's underneath it, and
    /// `Shader` gets a live cursor/time uniform every frame whether or
    /// not its particular WGSL happens to read them -- there's no way to
    /// know from here which it is, so it's always treated as animated.
    pub fn is_dynamic(&self) -> bool {
        let dynamic_regardless_of_effects = match self {
            Layer::Xray { .. } | Layer::Gif { .. } | Layer::Parallax { .. } => true,
            Layer::Text { source, .. } => source.is_dynamic(),
            Layer::Image { .. } | Layer::Adjustment { .. } => false,
        };
        dynamic_regardless_of_effects
            || self.effects().iter().any(|effect| {
                effect.enabled
                    && matches!(
                        effect.kind,
                        EffectKind::Smoke { .. } | EffectKind::Shader { .. }
                    )
            })
    }
}

/// Position/scale/rotation for something drawn on top of a layer's own
/// content -- currently only a [`Mask`]'s shape. `x`/`y` are the same
/// normalized (0.0..=1.0) fraction-of-canvas convention `Layer::Text`
/// already uses, `rotation` is in degrees (matches what an editor slider
/// shows a human, converted to radians only at the point a shader
/// actually needs it).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Transform2D {
    pub x: f32,
    pub y: f32,
    pub scale: f32,
    pub rotation: f32,
}

impl Default for Transform2D {
    /// Centered, unscaled, unrotated -- the sane starting point for a
    /// freshly added mask, before a user has dragged its gizmo at all.
    fn default() -> Self {
        Transform2D {
            x: 0.5,
            y: 0.5,
            scale: 1.0,
            rotation: 0.0,
        }
    }
}

/// Where an [`Effect`] applies, independently of what the effect itself
/// does -- a generic mechanism shared by every effect kind, not something
/// each effect's own shader implements (see `Ideas.md`, "Развилка 2").
/// The engine blends the effect's output back against its input by this
/// mask's value (0.0 = original, 1.0 = fully effect) in one fixed pass,
/// regardless of `kind`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Mask {
    /// Effect applies to the whole layer, unmasked.
    #[default]
    None,
    Circle {
        transform: Transform2D,
        /// Fraction of the circle's radius over which the mask eases
        /// from 1.0 to 0.0 at its edge, rather than cutting off sharply.
        feather: f32,
        invert: bool,
    },
    Gradient {
        transform: Transform2D,
        feather: f32,
        invert: bool,
    },
    /// A hand-authored or hand-picked grayscale image, relative path
    /// within the project dir -- brightness is the mask value. Produced
    /// either by `path_picker` or by painting it directly in the editor;
    /// the two are indistinguishable once saved (see `Ideas.md`, "Кисть
    /// для маски").
    Texture { path: String, invert: bool },
}

/// Which built-in effect this is and its own parameters -- deliberately
/// a small closed set for now (Фаза 1/2 of the library in `Ideas.md`),
/// not yet the generic user-authored `Shader` variant planned for Фаза
/// 3.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectKind {
    Vignette { strength: f32, softness: f32 },
    ColorAdjust {
        brightness: f32,
        contrast: f32,
        saturation: f32,
    },
    Blur { radius: f32 },
    /// A colored trail that persistently follows the cursor, fading
    /// over time (Фаза 2 of the library -- the first *stateful* effect:
    /// unlike every kind above, its GPU-side output isn't a pure
    /// function of the layer's current content each frame, it carries
    /// its own accumulated state forward frame to frame. See `Ideas.md`,
    /// "Техника отрисовки для стейтфул-эффектов".
    Smoke {
        /// RGBA, same 0.0..=1.0-per-channel convention as `Layer::Text`'s
        /// `color`. Splatted at the cursor position and faded by `decay`
        /// every frame -- not a flat tint.
        color: [f32; 4],
        /// Fraction of the trail's own color kept each frame -- close to
        /// 1.0 (e.g. 0.97) for a long, slow-fading trail; lower fades
        /// faster. Values at or above 1.0 never fade at all.
        decay: f32,
        /// Splat radius, as a fraction of canvas size (not
        /// aspect-corrected -- see `smoke.wgsl`'s doc comment for why
        /// that's an acceptable simplification for a first version).
        radius: f32,
    },
    /// A user-authored `.wgsl` effect, loaded as a project asset and
    /// compiled at runtime -- Фаза 3 of the library in `Ideas.md`,
    /// "Кастомные шейдерные слои": the point where a new visual effect no
    /// longer requires a new engine release. Reuses the exact same
    /// single-input-texture pipeline shape every other single-pass
    /// effect kind already uses (see `build_effect_bind_group_layout` in
    /// `player`) -- only the shader module and the tail of its uniform
    /// buffer are shader-specific.
    Shader {
        /// Relative path (project-dir-relative once saved, same
        /// convention as every other asset path in this schema) to the
        /// `.wgsl` source. See [`parse_shader_params`] for the parameter
        /// annotation format this file's own uniform struct carries.
        wgsl_path: String,
        /// Current value of each user-declared param, flattened, in the
        /// same order [`parse_shader_params`] returns them in when it
        /// scans `wgsl_path`'s source -- *not* keyed by name, since the
        /// order is exactly what the shader author's own struct layout
        /// (and therefore the uniform buffer's byte layout) already
        /// depends on. The engine's own fields (cursor, time, canvas
        /// aspect) are never stored here -- they're computed fresh every
        /// frame by whichever caller has the live cursor position on
        /// hand (`player::SceneRenderer::update_shader_effects`, the
        /// same pattern `Smoke`'s cursor already uses) and always
        /// written *before* this array's bytes.
        params: Vec<f32>,
    },
}

/// One user-declared parameter of a [`EffectKind::Shader`] effect, parsed
/// out of its `.wgsl` source by [`parse_shader_params`] -- purely a
/// description of what widget the editor should draw and what its
/// current value means; the live value itself lives in
/// `EffectKind::Shader::params`, not here.
#[derive(Debug, Clone, PartialEq)]
pub struct ShaderParamSpec {
    /// The WGSL struct field's own name (e.g. `u_hue_shift`) -- not
    /// otherwise used by the engine (params are matched up by position,
    /// see `EffectKind::Shader::params`'s doc comment), but useful for
    /// error messages and as a widget label fallback.
    pub name: String,
    /// Human-readable label from the annotation's `"label"` key, or
    /// `name` itself if that key is omitted.
    pub label: String,
    pub default: f32,
    /// Inclusive slider bounds, from the annotation's `"range"` key --
    /// required (unlike `label`), since there's no sane default range
    /// for an arbitrary shader param.
    pub range: (f32, f32),
}

/// Scans `source` (a `.wgsl` file's full text) for every uniform-struct
/// field carrying a trailing `// {"label": ..., "default": ..., "range":
/// [...]}` JSON annotation, in the order they appear in the file -- see
/// `Ideas.md`, "Параметры шейдера: как редактор узнаёт, что и как
/// показывать". Only `f32` fields are recognized in this version (not
/// yet `vec4<f32>`/color or texture/sampler params -- see the plan file's
/// M7 section for why: an annotated non-`f32` field is a hard error
/// rather than silently ignored, since a silently-skipped field would
/// desync every later param's position from what the engine actually
/// writes into the uniform buffer).
///
/// A line is only treated as an annotation attempt if its trailing
/// comment, once trimmed, starts with `{` -- an ordinary `// explanation`
/// comment elsewhere in the file is left alone. Once a line does look
/// like an annotation attempt, any failure (bad JSON, missing key, wrong
/// field type) is a hard `Err` -- never silently skipped -- for the same
/// position-desync reason above.
pub fn parse_shader_params(source: &str) -> Result<Vec<ShaderParamSpec>, String> {
    let mut specs = Vec::new();
    for (line_number, line) in source.lines().enumerate() {
        let Some(comment_start) = line.find("//") else {
            continue;
        };
        let (code, comment) = line.split_at(comment_start);
        let comment = comment[2..].trim();
        if !comment.starts_with('{') {
            continue;
        }
        let line_number = line_number + 1;

        let field = code.trim().trim_end_matches(',').trim();
        let (name, ty) = field.split_once(':').ok_or_else(|| {
            format!("shader param annotation on line {line_number} isn't attached to a `name: type` field declaration")
        })?;
        let name = name.trim().to_string();
        let ty = ty.trim();
        if ty != "f32" {
            return Err(format!(
                "shader param '{name}' on line {line_number} has type '{ty}' -- only f32 params are supported for now"
            ));
        }

        let json: serde_json::Value = serde_json::from_str(comment).map_err(|e| {
            format!("shader param '{name}' on line {line_number} has an invalid annotation: {e}")
        })?;
        let label = json
            .get("label")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| name.clone());
        let default = json.get("default").and_then(|v| v.as_f64()).ok_or_else(|| {
            format!("shader param '{name}' on line {line_number} is missing a numeric \"default\"")
        })? as f32;
        let range = json
            .get("range")
            .and_then(|v| v.as_array())
            .filter(|a| a.len() == 2)
            .and_then(|a| Some((a[0].as_f64()?, a[1].as_f64()?)))
            .ok_or_else(|| {
                format!(
                    "shader param '{name}' on line {line_number} is missing a two-number \"range\""
                )
            })?;

        specs.push(ShaderParamSpec {
            name,
            label,
            default,
            range: (range.0 as f32, range.1 as f32),
        });
    }
    Ok(specs)
}

/// One entry in a layer's post-processing stack -- see `Ideas.md`,
/// "Эффекты как стек пост-обработки на слое".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Effect {
    pub kind: EffectKind,
    #[serde(default)]
    pub mask: Mask,
    pub enabled: bool,
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

    #[test]
    fn image_layer_with_effects_round_trips() {
        let layer = Layer::Image {
            path: "bg.png".to_string(),
            effects: vec![Effect {
                kind: EffectKind::Vignette {
                    strength: 0.6,
                    softness: 0.3,
                },
                mask: Mask::Circle {
                    transform: Transform2D {
                        x: 0.5,
                        y: 0.5,
                        scale: 1.2,
                        rotation: 0.0,
                    },
                    feather: 0.2,
                    invert: false,
                },
                enabled: true,
            }],
        };
        let json = serde_json::to_string(&layer).unwrap();
        let round_tripped: Layer = serde_json::from_str(&json).unwrap();
        match round_tripped {
            Layer::Image { path, effects } => {
                assert_eq!(path, "bg.png");
                assert_eq!(effects.len(), 1);
                assert!(effects[0].enabled);
                assert!(matches!(effects[0].kind, EffectKind::Vignette { .. }));
                assert!(matches!(effects[0].mask, Mask::Circle { .. }));
            }
            _ => panic!("expected Layer::Image"),
        }
    }

    #[test]
    fn smoke_effect_round_trips_and_makes_its_layer_dynamic() {
        let layer = Layer::Image {
            path: "bg.png".to_string(),
            effects: vec![Effect {
                kind: EffectKind::Smoke {
                    color: [0.6, 0.3, 0.9, 1.0],
                    decay: 0.97,
                    radius: 0.05,
                },
                mask: Mask::None,
                enabled: true,
            }],
        };
        assert!(layer.is_dynamic());

        let json = serde_json::to_string(&layer).unwrap();
        let round_tripped: Layer = serde_json::from_str(&json).unwrap();
        match round_tripped {
            Layer::Image { effects, .. } => {
                assert!(matches!(
                    effects[0].kind,
                    EffectKind::Smoke { decay, .. } if decay == 0.97
                ));
            }
            _ => panic!("expected Layer::Image"),
        }
    }

    #[test]
    fn a_disabled_smoke_effect_does_not_make_its_layer_dynamic() {
        let layer = Layer::Image {
            path: "bg.png".to_string(),
            effects: vec![Effect {
                kind: EffectKind::Smoke {
                    color: [0.6, 0.3, 0.9, 1.0],
                    decay: 0.97,
                    radius: 0.05,
                },
                mask: Mask::None,
                enabled: false,
            }],
        };
        assert!(!layer.is_dynamic());
    }

    #[test]
    fn shader_effect_round_trips_and_makes_its_layer_dynamic() {
        let layer = Layer::Image {
            path: "bg.png".to_string(),
            effects: vec![Effect {
                kind: EffectKind::Shader {
                    wgsl_path: "hue_shift.wgsl".to_string(),
                    params: vec![0.3],
                },
                mask: Mask::None,
                enabled: true,
            }],
        };
        assert!(layer.is_dynamic());

        let json = serde_json::to_string(&layer).unwrap();
        let round_tripped: Layer = serde_json::from_str(&json).unwrap();
        match round_tripped {
            Layer::Image { effects, .. } => match &effects[0].kind {
                EffectKind::Shader { wgsl_path, params } => {
                    assert_eq!(wgsl_path, "hue_shift.wgsl");
                    assert_eq!(params, &[0.3]);
                }
                _ => panic!("expected EffectKind::Shader"),
            },
            _ => panic!("expected Layer::Image"),
        }
    }

    #[test]
    fn parse_shader_params_reads_annotated_f32_fields_in_order() {
        let source = r#"
struct ShaderEffectParams {
    cursor: vec2<f32>,
    time: f32,
    canvas_aspect: f32,
    u_hue_shift: f32, // {"label": "Hue shift", "default": 0.25, "range": [-1.0, 1.0]}
    u_strength: f32, // {"default": 1.0, "range": [0.0, 2.0]}
};
"#;
        let specs = parse_shader_params(source).expect("should parse");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "u_hue_shift");
        assert_eq!(specs[0].label, "Hue shift");
        assert_eq!(specs[0].default, 0.25);
        assert_eq!(specs[0].range, (-1.0, 1.0));
        // No "label" key -- falls back to the field's own name.
        assert_eq!(specs[1].name, "u_strength");
        assert_eq!(specs[1].label, "u_strength");
    }

    #[test]
    fn parse_shader_params_ignores_ordinary_comments() {
        let source = r#"
struct ShaderEffectParams {
    cursor: vec2<f32>, // just the cursor, nothing to see here
    time: f32,
};
"#;
        assert_eq!(parse_shader_params(source).unwrap(), Vec::new());
    }

    #[test]
    fn parse_shader_params_rejects_a_non_f32_annotated_field() {
        let source = r#"u_tint: vec4<f32>, // {"label": "Tint", "default": 0.0, "range": [0.0, 1.0]}"#;
        let err = parse_shader_params(source).unwrap_err();
        assert!(err.contains("u_tint"), "error should name the field: {err}");
        assert!(
            err.contains("vec4"),
            "error should mention the unsupported type: {err}"
        );
    }

    #[test]
    fn parse_shader_params_rejects_malformed_json_annotation() {
        let source = r#"u_amount: f32, // {"label": "Amount", "default": }"#;
        assert!(parse_shader_params(source).is_err());
    }

    #[test]
    fn parse_shader_params_rejects_annotation_missing_range() {
        let source = r#"u_amount: f32, // {"label": "Amount", "default": 0.5}"#;
        let err = parse_shader_params(source).unwrap_err();
        assert!(err.contains("range"), "error should mention range: {err}");
    }

    #[test]
    fn layer_without_effects_field_defaults_to_empty_for_pre_existing_project_json() {
        let layer: Layer =
            serde_json::from_str(r#"{"type":"image","path":"bg.png"}"#).expect("should still parse");
        match layer {
            Layer::Image { effects, .. } => assert!(effects.is_empty()),
            _ => panic!("expected Layer::Image"),
        }
    }

    #[test]
    fn adjustment_layer_round_trips_and_exposes_its_effects() {
        let layer = Layer::Adjustment {
            effects: vec![Effect {
                kind: EffectKind::Vignette {
                    strength: 0.6,
                    softness: 0.3,
                },
                mask: Mask::None,
                enabled: true,
            }],
        };
        assert_eq!(layer.effects().len(), 1);
        assert!(!layer.is_dynamic());

        let json = serde_json::to_string(&layer).unwrap();
        let round_tripped: Layer = serde_json::from_str(&json).unwrap();
        match round_tripped {
            Layer::Adjustment { effects } => {
                assert_eq!(effects.len(), 1);
                assert!(matches!(effects[0].kind, EffectKind::Vignette { .. }));
            }
            _ => panic!("expected Layer::Adjustment"),
        }
    }

    #[test]
    fn an_enabled_shader_effect_makes_an_adjustment_layer_dynamic() {
        let layer = Layer::Adjustment {
            effects: vec![Effect {
                kind: EffectKind::Shader {
                    wgsl_path: "pulse.wgsl".to_string(),
                    params: vec![1.0],
                },
                mask: Mask::None,
                enabled: true,
            }],
        };
        assert!(layer.is_dynamic());
    }

    #[test]
    fn adjustment_layer_without_effects_field_defaults_to_empty() {
        let layer: Layer =
            serde_json::from_str(r#"{"type":"adjustment"}"#).expect("should still parse");
        assert!(layer.effects().is_empty());
    }
}
