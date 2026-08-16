//! Wallpaper project editor: build a layer stack (picture, xray, gif
//! animation), see it composited live, then save it as a project folder
//! that render-server can load.
//!
//! The preview on the left uses the exact same GPU compositor as
//! render-server/player -- `player::SceneRenderer` -- drawn straight into
//! a small offscreen texture registered directly with egui-wgpu's
//! renderer (`register_native_texture`). No CPU readback: the preview
//! panel just points egui at a texture this crate keeps drawing into, so
//! what you see here is produced by the same pipelines/shaders
//! render-server uses, not a separate reimplementation that could drift
//! out of sync with what actually ships.

mod autostart;
mod library;
mod shaders_library;
mod monitors_config;
mod push;
mod trust_store;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use player::wgpu;
use player::{LoadedLayer, SceneRenderer};
use push::{ProjectPusher, TcpPusher};

/// The offscreen texture itself stays modest -- it's an authoring aid,
/// not a full-resolution look at the wallpaper (that's what running the
/// real player/render-server is for), and keeping the render cheap
/// matters more than sharpness here. The panel it's *displayed* in can
/// still be as large as the user likes -- see `show_preview`, which
/// stretches this texture to fill the available space rather than
/// showing it at native size.
const PREVIEW_MAX_WIDTH: u32 = 480;

/// Release tag this binary was built from, e.g. `2026.08.10` -- `"dev"`
/// for local/dev builds (see `build.rs`). Shown in the About window.
const APP_VERSION: &str = env!("WP_LINUX_VERSION");

/// Short hash of the commit this binary was built from, or `"unknown"`
/// if `build.rs` couldn't run `git` (see `build.rs`). Shown in the About
/// window.
const APP_GIT_HASH: &str = env!("WP_LINUX_GIT_HASH");

fn main() -> eframe::Result {
    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([1200.0, 600.0]),
        wgpu_options: eframe::egui_wgpu::WgpuConfiguration {
            wgpu_setup: eframe::egui_wgpu::WgpuSetup::CreateNew(
                eframe::egui_wgpu::WgpuSetupCreateNew {
                    // Avoids waking (and rendering through) a runtime-suspended
                    // discrete GPU on hybrid-GPU laptops -- confirmed fix for a
                    // real bug where the editor's whole window would render
                    // corrupted after sitting backgrounded for a while, once
                    // something (e.g. another window's minimize animation)
                    // caused the compositor to touch it again. Same reasoning
                    // as `player::pick_adapter`, which render-server already
                    // uses for this -- kept separate rather than shared since
                    // this one has to run synchronously over the adapter list
                    // egui_wgpu already enumerated, and has to defer to it for
                    // surface-compatibility filtering instead of picking an
                    // adapter blind.
                    power_preference: wgpu::PowerPreference::LowPower,
                    native_adapter_selector: gpu_override_from_env(),
                    ..eframe::egui_wgpu::WgpuSetupCreateNew::without_display_handle()
                },
            ),
            ..Default::default()
        },
        ..Default::default()
    };
    eframe::run_native(
        "WP Linux Editor",
        native_options,
        Box::new(|cc| {
            apply_style(&cc.egui_ctx);
            Ok(Box::new(EditorApp::default()))
        }),
    )
}

/// `WPLINUX_GPU=<substring>` forces a specific adapter (case-insensitive
/// match against its name) instead of the default `LowPower` preference --
/// same env var `player::pick_adapter`'s doc comment documents for
/// render-server, so a user comparing GPUs only has to learn it once.
/// Returns `None` when unset, which leaves selection to `power_preference`
/// via egui_wgpu's normal (surface-compatibility-aware) adapter request.
fn gpu_override_from_env() -> Option<eframe::egui_wgpu::NativeAdapterSelectorMethod> {
    let want = std::env::var("WPLINUX_GPU").ok()?;
    let want_lower = want.to_lowercase();
    Some(Arc::new(
        move |adapters: &[wgpu::Adapter], _surface: Option<&wgpu::Surface<'_>>| {
            if let Some(adapter) = adapters
                .iter()
                .find(|a| a.get_info().name.to_lowercase().contains(&want_lower))
            {
                return Ok(adapter.clone());
            }
            eprintln!(
                "wp_linux_editor: WPLINUX_GPU={want:?} matched no candidate adapter, \
                 falling back to the integrated GPU if available"
            );
            adapters
                .iter()
                .min_by_key(|a| adapter_rank(a.get_info().device_type))
                .cloned()
                .ok_or_else(|| "no GPU adapters available".to_string())
        },
    ))
}

/// Lower sorts first, i.e. gets picked -- integrated over discrete over
/// everything else, software/CPU dead last. Mirrors
/// `player::pick_adapter`'s `adapter_rank`, kept local rather than shared
/// (see `gpu_override_from_env`'s doc comment).
fn adapter_rank(device_type: wgpu::DeviceType) -> u8 {
    match device_type {
        wgpu::DeviceType::IntegratedGpu => 0,
        wgpu::DeviceType::DiscreteGpu => 1,
        wgpu::DeviceType::VirtualGpu => 2,
        wgpu::DeviceType::Other => 3,
        wgpu::DeviceType::Cpu => 4,
    }
}

/// Softens up egui's stock look (sharp corners, tight spacing) into
/// something a bit more current -- rounded widgets, a bit more breathing
/// room, and one accent color used consistently for selection/hover/
/// active states instead of the default flat gray. Applied once at
/// startup rather than per-frame since nothing here is dynamic (no
/// light/dark toggle in this app).
fn apply_style(ctx: &eframe::egui::Context) {
    // Applied to both the light and dark `Style` egui keeps internally
    // (it can switch between them to follow the system theme) rather
    // than whichever happens to be active right now.
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = eframe::egui::vec2(8.0, 10.0);
        style.spacing.button_padding = eframe::egui::vec2(10.0, 6.0);
        style.spacing.window_margin = eframe::egui::Margin::same(12);
        style.spacing.menu_margin = eframe::egui::Margin::same(8);
        style.spacing.interact_size.y = 24.0;
        // Wider than the default 100px -- a slider spanning a large
        // range (e.g. 20..=800) gets noticeably more precision per pixel
        // of drag just from this, on top of `scroll_slider`'s wheel
        // support below.
        style.spacing.slider_width = 180.0;

        let accent = eframe::egui::Color32::from_rgb(94, 129, 244);
        let corner_radius = eframe::egui::CornerRadius::from(6);

        let visuals = &mut style.visuals;
        visuals.window_corner_radius = eframe::egui::CornerRadius::from(10);
        visuals.menu_corner_radius = eframe::egui::CornerRadius::from(8);
        for widgets in [
            &mut visuals.widgets.noninteractive,
            &mut visuals.widgets.inactive,
            &mut visuals.widgets.hovered,
            &mut visuals.widgets.active,
            &mut visuals.widgets.open,
        ] {
            widgets.corner_radius = corner_radius;
        }
        visuals.widgets.hovered.bg_stroke = eframe::egui::Stroke::new(1.0, accent);
        visuals.widgets.active.bg_stroke = eframe::egui::Stroke::new(1.5, accent);
        visuals.selection.bg_fill = accent;
        visuals.selection.stroke = eframe::egui::Stroke::new(1.0, eframe::egui::Color32::WHITE);
        visuals.hyperlink_color = accent;
    });
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Wallpapers,
    Discover,
    Editor,
}

/// One connected output, as reported by winit -- `name` is the same
/// `wl_output` protocol name Qt's `Screen.name` (in main.qml) and
/// render-server's `?monitor=` query param already use, so it can be used
/// as a monitor id with no translation.
#[derive(Clone)]
struct MonitorInfo {
    name: String,
    width: u32,
    height: u32,
}

/// Number of tiles per row in the "Wallpapers" tab's wallpaper grid --
/// always exactly this many, regardless of window width: tiles resize
/// (see `show_wallpapers_tab`'s per-frame `tile_size` calculation) to
/// fill whatever width is available instead of the column count
/// changing.
const WALLPAPER_GRID_COLUMNS: usize = 4;
/// Reference aspect ratio for a tile's preview image (name label sits
/// below it, outside this rect) -- the actual on-screen size is derived
/// from this ratio and the available width each frame, not used
/// directly.
const WALLPAPER_TILE_SIZE: eframe::egui::Vec2 = eframe::egui::Vec2::new(220.0, 130.0);
/// Size of each tile's always-visible Edit/Apply icon
/// buttons, pinned to the top-right corner of the thumbnail.
const WALLPAPER_ICON_SIZE: f32 = 24.0;
const WALLPAPER_ICON_MARGIN: f32 = 6.0;
const WALLPAPER_ICON_GAP: f32 = 4.0;

enum EditorLayer {
    Image {
        path: Option<PathBuf>,
        effects: Vec<EditorEffect>,
    },
    Xray {
        base: Option<PathBuf>,
        overlay: Option<PathBuf>,
        radius: f32,
        effects: Vec<EditorEffect>,
    },
    Gif {
        path: Option<PathBuf>,
        effects: Vec<EditorEffect>,
    },
    Parallax {
        path: Option<PathBuf>,
        strength: f32,
        smoothing: f32,
        effects: Vec<EditorEffect>,
    },
    Text {
        // Normalized (0.0..=1.0) canvas fractions -- see
        // `project_format::Layer::Text`.
        x: f32,
        y: f32,
        font_size: f32,
        color: [f32; 4],
        source: EditorTextSource,
        font: EditorTextFont,
    },
    /// No picture/path of its own -- see
    /// `project_format::Layer::Adjustment`'s doc comment.
    Adjustment { effects: Vec<EditorEffect> },
}

/// Mirrors `project_format::TextSource`'s variants -- kept as a separate
/// type (rather than using the schema type directly here) purely so the
/// property panel has somewhere to keep an in-progress format string
/// without it round-tripping through the preview/save path on every
/// keystroke; converted to/from `project_format::TextSource` at the
/// `open_project`/`save_project`/`build_preview_project` boundaries.
enum EditorTextSource {
    Literal(String),
    Clock { format: String },
    Command { command: String, interval_secs: u32 },
}

impl EditorTextSource {
    /// Whether this source ever shells out -- used by the Save button
    /// handler to decide whether this project's id needs
    /// `trust_store::mark_trusted`.
    fn is_command(&self) -> bool {
        matches!(self, EditorTextSource::Command { .. })
    }

    fn to_project(&self) -> project_format::TextSource {
        match self {
            EditorTextSource::Literal(text) => {
                project_format::TextSource::Literal { text: text.clone() }
            }
            EditorTextSource::Clock { format } => project_format::TextSource::Clock {
                format: format.clone(),
            },
            EditorTextSource::Command {
                command,
                interval_secs,
            } => project_format::TextSource::Command {
                command: command.clone(),
                interval_secs: *interval_secs,
            },
        }
    }

    fn from_project(source: project_format::TextSource) -> Self {
        match source {
            project_format::TextSource::Literal { text } => EditorTextSource::Literal(text),
            project_format::TextSource::Clock { format } => EditorTextSource::Clock { format },
            project_format::TextSource::Command {
                command,
                interval_secs,
            } => EditorTextSource::Command {
                command,
                interval_secs,
            },
        }
    }
}

/// Mirrors `project_format::TextFont`'s variants -- kept separate purely
/// so `Custom`'s path can be an `Option<PathBuf>` fed to `path_picker`,
/// like every other asset path in this file, rather than
/// `project_format::TextFont::Custom`'s `String` (a relative,
/// staged-on-save path that doesn't exist until the project itself is
/// saved). Converted to/from `project_format::TextFont` at the
/// `open_project`/`save_project`/`build_preview_project` boundaries,
/// same as `EditorMask`.
///
/// Unlike `EditorLayer::Text`'s x/y/font_size/color/source, this is a
/// *structural* property, not a live one -- switching fonts needs a real
/// scene reload (a different file to read, a different glyphon family to
/// shape against), the same way `EditorMask::Texture`'s own path does.
/// See `LayerSignature::Text`'s payload; `Preview::sync_live_params`'s
/// Text arm deliberately never reads this field.
#[derive(Clone, Default)]
enum EditorTextFont {
    #[default]
    Bundled,
    Custom {
        path: Option<PathBuf>,
    },
}

impl EditorTextFont {
    /// Mirrors `EditorMask::Texture`'s own path-must-be-picked
    /// completeness check.
    fn is_complete(&self) -> bool {
        match self {
            EditorTextFont::Bundled => true,
            EditorTextFont::Custom { path } => path.is_some(),
        }
    }

    /// See `EditorMask::to_project`'s doc comment on why an absolute
    /// path here (as opposed to `save_project`'s staged relative one) is
    /// exactly what a preview-only `Project` needs.
    fn to_project(&self) -> project_format::TextFont {
        match self {
            EditorTextFont::Bundled => project_format::TextFont::Bundled,
            EditorTextFont::Custom { path } => project_format::TextFont::Custom {
                path: path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            },
        }
    }
}

/// One entry in a layer's effect stack, editor-side -- mirrors
/// `project_format::Effect`. `kind` used to reuse `project_format::
/// EffectKind` directly (plain numeric params, nothing an editing
/// session needs to buffer) until `Shader` was added -- its `wgsl_path`
/// needs the same `Option<PathBuf>`-for-`path_picker` treatment as every
/// other asset path in this file, so `kind` now goes through
/// `EditorEffectKind` below, same rationale as the mask's own `Texture`
/// path already going through `EditorMask` instead of
/// `project_format::Mask` directly.
struct EditorEffect {
    kind: EditorEffectKind,
    mask: EditorMask,
    enabled: bool,
}

impl EditorEffect {
    /// Mirrors `EditorLayer::is_complete` -- an effect with a `Texture`
    /// mask, or a `Shader` kind, isn't saveable/previewable until a file's
    /// been picked for it, same as any other asset path in this file.
    fn is_complete(&self) -> bool {
        let mask_complete = match &self.mask {
            EditorMask::Texture { path, .. } => path.is_some(),
            EditorMask::None | EditorMask::Circle { .. } | EditorMask::Gradient { .. } => true,
        };
        let kind_complete = match &self.kind {
            EditorEffectKind::Shader { wgsl_path, .. } => wgsl_path.is_some(),
            EditorEffectKind::Vignette { .. }
            | EditorEffectKind::ColorAdjust { .. }
            | EditorEffectKind::Blur { .. }
            | EditorEffectKind::Smoke { .. } => true,
        };
        mask_complete && kind_complete
    }

    fn to_project(&self) -> project_format::Effect {
        project_format::Effect {
            kind: self.kind.to_project(),
            mask: self.mask.to_project(),
            enabled: self.enabled,
        }
    }
}

/// Mirrors `project_format::EffectKind`'s variants -- kept separate
/// purely so `Shader`'s `wgsl_path` can be an `Option<PathBuf>` fed to
/// `path_picker`, like every other asset path in this file, rather than
/// `project_format::EffectKind`'s `String` (a relative, staged-on-save
/// path that doesn't exist until the project itself is saved). Every
/// other variant is a plain field-for-field mirror -- converted to/from
/// `project_format::EffectKind` at the `open_project`/`save_project`/
/// `build_preview_project` boundaries, same as `EditorMask`.
enum EditorEffectKind {
    Vignette {
        strength: f32,
        softness: f32,
    },
    ColorAdjust {
        brightness: f32,
        contrast: f32,
        saturation: f32,
    },
    Blur {
        radius: f32,
    },
    Smoke {
        color: [f32; 4],
        decay: f32,
        radius: f32,
    },
    Shader {
        wgsl_path: Option<PathBuf>,
        /// Same order/meaning as `project_format::EffectKind::Shader::
        /// params` -- kept in sync with whatever `wgsl_path`'s file
        /// currently declares by `show_effect_kind_panel`'s Shader arm,
        /// which re-parses it every frame (see its own doc comment).
        params: Vec<f32>,
    },
}

impl EditorEffectKind {
    /// See `EditorMask::to_project`'s doc comment on why an absolute
    /// path here (as opposed to `save_project`'s staged relative one) is
    /// exactly what a preview-only `Project` needs.
    fn to_project(&self) -> project_format::EffectKind {
        match self {
            EditorEffectKind::Vignette { strength, softness } => {
                project_format::EffectKind::Vignette {
                    strength: *strength,
                    softness: *softness,
                }
            }
            EditorEffectKind::ColorAdjust {
                brightness,
                contrast,
                saturation,
            } => project_format::EffectKind::ColorAdjust {
                brightness: *brightness,
                contrast: *contrast,
                saturation: *saturation,
            },
            EditorEffectKind::Blur { radius } => project_format::EffectKind::Blur { radius: *radius },
            EditorEffectKind::Smoke {
                color,
                decay,
                radius,
            } => project_format::EffectKind::Smoke {
                color: *color,
                decay: *decay,
                radius: *radius,
            },
            EditorEffectKind::Shader { wgsl_path, params } => project_format::EffectKind::Shader {
                wgsl_path: wgsl_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                params: params.clone(),
            },
        }
    }
}

/// Mirrors `project_format::Mask`'s variants -- kept separate purely so
/// `Texture`'s path can be an `Option<PathBuf>` fed to `path_picker`,
/// like every other asset path in this file (`EditorLayer`'s own
/// `path`/`base`/`overlay`), rather than `project_format::Mask`'s
/// `String` (a relative, staged-on-save path that doesn't exist until
/// the project itself is saved). Converted to/from
/// `project_format::Mask` at the `open_project`/`save_project`/
/// `build_preview_project` boundaries, same as `EditorTextSource`.
enum EditorMask {
    None,
    Circle {
        transform: project_format::Transform2D,
        feather: f32,
        invert: bool,
    },
    Gradient {
        transform: project_format::Transform2D,
        feather: f32,
        invert: bool,
    },
    Texture {
        path: Option<PathBuf>,
        invert: bool,
        /// Brush-painted content, editor-only -- M8, see `Ideas.md`,
        /// "Кисть для маски". `None` for a plain picked-file mask that's
        /// never been painted on; `Some` from the moment paint mode is
        /// first entered on this mask onward (see `show_mask_panel`'s
        /// Texture arm), whether it started blank or was normalized
        /// down from an existing picture. Once `Some`, this buffer --
        /// not whatever `path`'s file happens to contain on disk at any
        /// given moment -- is the live source of truth; `write_texture`-
        /// per-stroke GPU updates (`LoadedLayer::write_mask_paint`)
        /// never touch the file, only `stage_effect` (at save) writes
        /// it back out. Single channel, `PAINT_MASK_RESOLUTION`^2
        /// bytes, row-major, mask value 0..255 per pixel -- `mask_texture`
        /// only ever samples the R channel (see `mask_blend.wgsl`), so
        /// there's no reason to carry three redundant color channels
        /// through painting.
        paint: Option<Vec<u8>>,
        /// Whether the brush-paint interaction is currently capturing
        /// drags on the preview for this mask -- kept alongside `paint`
        /// (not a loose `EditorApp` field) so switching to a different
        /// effect/mask can't leave a stale "still painting" flag pointed
        /// at the wrong one.
        painting: bool,
        /// Brush size/edge softness, same units as everywhere else in
        /// this file that measures something against canvas size
        /// (fraction of canvas, 0.0..=1.0) -- see `stamp_paint_buffer`.
        brush_radius: f32,
        brush_softness: f32,
        /// UV position of the last stamp this stroke, so
        /// `stamp_paint_buffer` can interpolate between it and the
        /// current position instead of leaving gaps on a fast mouse
        /// move (same technique as M6's smoke splat trail). `None`
        /// between strokes (pointer up, or paint mode just turned on)
        /// so a new stroke's first stamp doesn't interpolate all the
        /// way from wherever the previous one ended.
        last_paint_pos: Option<(f32, f32)>,
    },
}

/// Fixed working resolution every brush-painted mask gets normalized to
/// the moment paint mode is first entered on it, regardless of the
/// picture's own resolution it may have started from (or lack of one) --
/// see `Ideas.md`, "Кисть для маски", on why repainting at a source
/// picture's full resolution (which could be 4K) every stroke would be
/// too expensive, and a fixed size with linear filtering at sample time
/// is plenty for a soft mask.
const PAINT_MASK_RESOLUTION: u32 = 1024;
const DEFAULT_BRUSH_RADIUS: f32 = 0.05;
const DEFAULT_BRUSH_SOFTNESS: f32 = 0.5;

impl EditorMask {
    /// See `build_preview_project`'s doc comment on why an absolute
    /// path here (as opposed to `save_project`'s staged relative one)
    /// is exactly what a preview-only `Project` needs.
    fn to_project(&self) -> project_format::Mask {
        match self {
            EditorMask::None => project_format::Mask::None,
            EditorMask::Circle {
                transform,
                feather,
                invert,
            } => project_format::Mask::Circle {
                transform: *transform,
                feather: *feather,
                invert: *invert,
            },
            EditorMask::Gradient {
                transform,
                feather,
                invert,
            } => project_format::Mask::Gradient {
                transform: *transform,
                feather: *feather,
                invert: *invert,
            },
            EditorMask::Texture { path, invert, .. } => project_format::Mask::Texture {
                path: path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                invert: *invert,
            },
        }
    }
}

/// Resolves a saved project's `Effect` list back into editor state,
/// same rationale as `open_project`'s per-layer conversion -- a
/// `Mask::Texture` path is relative to `project_dir`, same as any
/// other asset path there.
fn editor_effects_from_project(
    effects: Vec<project_format::Effect>,
    project_dir: &Path,
) -> Vec<EditorEffect> {
    effects
        .into_iter()
        .map(|effect| EditorEffect {
            kind: match effect.kind {
                project_format::EffectKind::Vignette { strength, softness } => {
                    EditorEffectKind::Vignette { strength, softness }
                }
                project_format::EffectKind::ColorAdjust {
                    brightness,
                    contrast,
                    saturation,
                } => EditorEffectKind::ColorAdjust {
                    brightness,
                    contrast,
                    saturation,
                },
                project_format::EffectKind::Blur { radius } => {
                    EditorEffectKind::Blur { radius }
                }
                project_format::EffectKind::Smoke {
                    color,
                    decay,
                    radius,
                } => EditorEffectKind::Smoke {
                    color,
                    decay,
                    radius,
                },
                project_format::EffectKind::Shader { wgsl_path, params } => {
                    EditorEffectKind::Shader {
                        wgsl_path: Some(project_dir.join(wgsl_path)),
                        params,
                    }
                }
            },
            mask: match effect.mask {
                project_format::Mask::None => EditorMask::None,
                project_format::Mask::Circle {
                    transform,
                    feather,
                    invert,
                } => EditorMask::Circle {
                    transform,
                    feather,
                    invert,
                },
                project_format::Mask::Gradient {
                    transform,
                    feather,
                    invert,
                } => EditorMask::Gradient {
                    transform,
                    feather,
                    invert,
                },
                project_format::Mask::Texture { path, invert } => EditorMask::Texture {
                    path: Some(project_dir.join(path)),
                    invert,
                    // A painted mask is byte-for-byte a normal
                    // `Mask::Texture` PNG once saved (see `stage_effect`)
                    // -- reading it back into `paint` for further
                    // painting only happens lazily, the first time paint
                    // mode is (re-)entered on it (`show_mask_panel`'s
                    // Texture arm already does this for any file, picked
                    // or reopened, so there's nothing special to do
                    // here).
                    paint: None,
                    painting: false,
                    brush_radius: DEFAULT_BRUSH_RADIUS,
                    brush_softness: DEFAULT_BRUSH_SOFTNESS,
                    last_paint_pos: None,
                },
            },
            enabled: effect.enabled,
        })
        .collect()
}

impl EditorLayer {
    fn label(&self) -> &'static str {
        match self {
            EditorLayer::Image { .. } => "Image",
            EditorLayer::Xray { .. } => "Xray",
            EditorLayer::Gif { .. } => "Gif",
            EditorLayer::Parallax { .. } => "Parallax",
            EditorLayer::Text { .. } => "Text",
            EditorLayer::Adjustment { .. } => "Adjustment",
        }
    }

    fn is_complete(&self) -> bool {
        match self {
            EditorLayer::Image { path, effects } => {
                path.is_some() && effects.iter().all(EditorEffect::is_complete)
            }
            EditorLayer::Xray {
                base,
                overlay,
                effects,
                ..
            } => base.is_some() && overlay.is_some() && effects.iter().all(EditorEffect::is_complete),
            EditorLayer::Gif { path, effects } => {
                path.is_some() && effects.iter().all(EditorEffect::is_complete)
            }
            EditorLayer::Parallax { path, effects, .. } => {
                path.is_some() && effects.iter().all(EditorEffect::is_complete)
            }
            // No external asset of its own to preview/save, aside from
            // its optional custom font.
            EditorLayer::Text { font, .. } => font.is_complete(),
            // No external asset either -- ready as soon as its own
            // effects are (an empty stack is trivially "all complete").
            EditorLayer::Adjustment { effects } => effects.iter().all(EditorEffect::is_complete),
        }
    }

    /// This layer's own effects list, if it has one -- every layer kind
    /// but `Text` does. Pulled out to replace a five-armed match
    /// (`Image`/`Xray`/`Gif`/`Parallax`/`Adjustment` all resolving to
    /// `Some(effects)`, `Text` to `None`) that otherwise turns up
    /// wherever code only cares about the effects list, independent of
    /// which layer kind owns it (mask gizmos, painted-mask restore).
    fn effects(&self) -> Option<&Vec<EditorEffect>> {
        match self {
            EditorLayer::Image { effects, .. }
            | EditorLayer::Xray { effects, .. }
            | EditorLayer::Gif { effects, .. }
            | EditorLayer::Parallax { effects, .. }
            | EditorLayer::Adjustment { effects } => Some(effects),
            EditorLayer::Text { .. } => None,
        }
    }

    /// Mutable counterpart of [`EditorLayer::effects`] -- see its doc
    /// comment.
    fn effects_mut(&mut self) -> Option<&mut Vec<EditorEffect>> {
        match self {
            EditorLayer::Image { effects, .. }
            | EditorLayer::Xray { effects, .. }
            | EditorLayer::Gif { effects, .. }
            | EditorLayer::Parallax { effects, .. }
            | EditorLayer::Adjustment { effects } => Some(effects),
            EditorLayer::Text { .. } => None,
        }
    }
}

/// What one effect's *shape* was last built from -- not its numeric
/// params (strength, transform, feather, ...), which are all
/// live-updatable in place via `LoadedLayer::set_effect_params` (see
/// `Preview::sync_live_params`) and so deliberately excluded here, same
/// rationale as `LayerSignature` excluding radius/strength/smoothing.
/// What *does* need a reload: adding/removing/reordering an effect,
/// switching its kind (different pipeline/bind-group-layout) or its
/// mask's variant, or repointing a `Mask::Texture` at a different file
/// (a real texture load, not just a uniform-buffer write).
#[derive(Clone, PartialEq, Eq)]
struct EffectSignature {
    kind: EffectKindSignature,
    mask: MaskSignature,
}

#[derive(Clone, PartialEq, Eq)]
enum EffectKindSignature {
    Vignette,
    ColorAdjust,
    Blur,
    Smoke,
    // Both the file and the param *count* are structural -- a different
    // count means a different uniform buffer size and (once the file
    // changed) quite possibly a different pipeline too, neither of which
    // `set_effect_params`' plain `write_buffer` can accommodate in
    // place. The param *values* stay excluded, same as every other
    // kind's numeric fields.
    Shader(PathBuf, usize),
}

#[derive(Clone, PartialEq, Eq)]
enum MaskSignature {
    None,
    Circle,
    Gradient,
    Texture(PathBuf),
}

fn effect_signature(effect: &EditorEffect) -> EffectSignature {
    EffectSignature {
        kind: match &effect.kind {
            EditorEffectKind::Vignette { .. } => EffectKindSignature::Vignette,
            EditorEffectKind::ColorAdjust { .. } => EffectKindSignature::ColorAdjust,
            EditorEffectKind::Blur { .. } => EffectKindSignature::Blur,
            EditorEffectKind::Smoke { .. } => EffectKindSignature::Smoke,
            EditorEffectKind::Shader { wgsl_path, params } => {
                EffectKindSignature::Shader(wgsl_path.clone().unwrap_or_default(), params.len())
            }
        },
        mask: match &effect.mask {
            EditorMask::None => MaskSignature::None,
            EditorMask::Circle { .. } => MaskSignature::Circle,
            EditorMask::Gradient { .. } => MaskSignature::Gradient,
            EditorMask::Texture { path, .. } => {
                MaskSignature::Texture(path.clone().unwrap_or_default())
            }
        },
    }
}

/// What a `Preview` was last built from -- resolved asset paths (plus
/// each effect's shape, see `EffectSignature`) only, not radius (see
/// `Preview::sync_radii`) or fps (gif timing is derived from each gif's
/// own per-frame delays, never from the project's target fps -- see
/// project-format's `Project::fps` doc comment). Compared each frame
/// so a rebuild (re-decoding images, re-uploading GPU textures) only
/// happens on an actual structural change, not every frame of, say, a
/// slider drag.
#[derive(Clone, PartialEq, Eq)]
enum LayerSignature {
    Image(PathBuf, Vec<EffectSignature>),
    Xray(PathBuf, PathBuf, Vec<EffectSignature>),
    Gif(PathBuf, Vec<EffectSignature>),
    Parallax(PathBuf, Vec<EffectSignature>),
    // No path -- an Adjustment layer has no asset of its own, just its
    // own effects list, same structural-signature treatment as any
    // other layer's `effects`.
    Adjustment(Vec<EffectSignature>),
    // Unlike every other Text property (updatable live via
    // `SceneRenderer::set_text_params` -- see `Preview::
    // sync_live_params`), the font is structural: switching it needs a
    // real reload (see `EditorTextFont`'s doc comment), so it's the one
    // payload this signature carries.
    Text(TextFontSignature),
}

#[derive(Clone, PartialEq, Eq)]
enum TextFontSignature {
    Bundled,
    Custom(PathBuf),
}

/// Live GPU preview of the current layer stack.
struct Preview {
    renderer: SceneRenderer,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    texture_id: eframe::egui::TextureId,
    width: u32,
    height: u32,
    layers: Vec<LoadedLayer>,
    loaded_at: Instant,
    // Elapsed time (since `loaded_at`) as of the last parallax ease step
    // -- mirrors `player::main`'s `last_parallax_update_ms`, see there
    // for why this needs a delta rather than `loaded_at.elapsed()`
    // directly.
    last_parallax_update_ms: u64,
    signature: Option<Vec<LayerSignature>>,
}

impl Preview {
    fn new(render_state: &eframe::egui_wgpu::RenderState) -> Self {
        let renderer = SceneRenderer::from_existing(
            render_state.device.clone(),
            render_state.queue.clone(),
            render_state.adapter.clone(),
            player::OFFSCREEN_FORMAT,
        );
        let (width, height) = (PREVIEW_MAX_WIDTH, PREVIEW_MAX_WIDTH * 9 / 16);
        let (texture, view) = create_preview_texture(&renderer, width, height);
        let texture_id = render_state.renderer.write().register_native_texture(
            &renderer.device,
            &view,
            wgpu::FilterMode::Linear,
        );
        Self {
            renderer,
            texture,
            view,
            texture_id,
            width,
            height,
            layers: Vec::new(),
            loaded_at: Instant::now(),
            last_parallax_update_ms: 0,
            signature: None,
        }
    }

    /// Reloads the GPU scene if `editor_layers` changed since the last
    /// call (see `LayerSignature`). Resizes (and re-points the already
    /// registered `texture_id` at) the preview texture if the new scene's
    /// canvas aspect ratio differs from the current one.
    fn ensure_scene(
        &mut self,
        render_state: &eframe::egui_wgpu::RenderState,
        editor_layers: &[EditorLayer],
    ) -> Result<(), String> {
        let Some(project) = build_preview_project(editor_layers) else {
            self.layers.clear();
            self.signature = None;
            return Ok(());
        };
        let signature = build_signature(editor_layers);
        if self.signature.as_ref() == Some(&signature) {
            return Ok(());
        }

        // Always trusted -- see `sync_live_params`'s Text arm for the
        // same self-authoring rationale.
        let mut layers = self.renderer.load_scene(Path::new(""), &project, true)?;
        // A reload always rebuilds every texture from whatever's on
        // disk right now (see `create_loaded_mask`) -- but a painted
        // mask's on-disk file only reflects its content as of whenever
        // paint mode was last (re)entered on it (`EditorMask::Texture::
        // paint`'s doc comment), since live strokes only ever reach the
        // GPU directly (`write_mask_paint`), never the file. Without
        // this, a reload triggered by *anything* structural elsewhere
        // in the project -- entering paint mode on a *different* mask
        // for the first time, adding/removing an effect, anything that
        // changes `LayerSignature` -- would silently roll back every
        // other mask's in-progress strokes to that stale snapshot. So
        // every reload re-applies every currently-painted mask's true
        // live buffer right back on top, immediately.
        restore_painted_masks(&layers, editor_layers, &self.renderer.queue);
        let (natural_width, natural_height) =
            layers.first().map(LoadedLayer::size).unwrap_or((16, 9));
        let (width, height) = preview_size(natural_width, natural_height);
        if width != self.width || height != self.height {
            let (texture, view) = create_preview_texture(&self.renderer, width, height);
            render_state
                .renderer
                .write()
                .update_egui_texture_from_wgpu_texture(
                    &self.renderer.device,
                    &view,
                    wgpu::FilterMode::Linear,
                    self.texture_id,
                );
            self.texture = texture;
            self.view = view;
            self.width = width;
            self.height = height;
        }
        // Text renders directly into this preview texture, at *its*
        // pixel size -- not `natural_width`/`natural_height`, which is
        // only used above to pick an aspect-correct preview size.
        self.renderer.set_text_viewport(&mut layers, width, height);

        self.layers = layers;
        self.loaded_at = Instant::now();
        self.last_parallax_update_ms = 0;
        self.signature = Some(signature);
        Ok(())
    }

    /// Applies each Xray/Parallax layer's *current* (possibly
    /// still-being-dragged) params. Xray's radius is scaled down to this
    /// preview's resolution -- radii in project.json are tuned against
    /// the layer's native resolution, so using them unscaled against a
    /// much smaller preview canvas would draw a wildly oversized mask.
    /// Parallax's strength/smoothing are already resolution-independent
    /// (a fraction of the layer's own size, a duration), so they pass
    /// straight through.
    fn sync_live_params(&mut self, editor_layers: &[EditorLayer]) {
        if self.layers.is_empty() {
            return;
        }
        let (natural_width, _) = self.layers.first().map(LoadedLayer::size).unwrap_or((1, 1));
        let scale = self.width as f32 / natural_width.max(1) as f32;
        for (loaded, editor_layer) in self.layers.iter_mut().zip(editor_layers) {
            match editor_layer {
                EditorLayer::Xray { radius, effects, .. } => {
                    loaded.set_xray_radius(*radius * scale);
                    sync_effect_params(loaded, &self.renderer.queue, effects);
                }
                EditorLayer::Parallax {
                    strength,
                    smoothing,
                    effects,
                    ..
                } => {
                    loaded.set_parallax_params(*strength, *smoothing);
                    sync_effect_params(loaded, &self.renderer.queue, effects);
                }
                EditorLayer::Text {
                    x,
                    y,
                    font_size,
                    color,
                    source,
                    ..
                } => {
                    // Always trusted: the editor is the self-authoring
                    // context (the user typed the command themselves),
                    // matching the auto-trust-on-save policy in the Save
                    // button handler. `font` isn't live-updatable -- see
                    // `EditorTextFont`'s doc comment -- so it's not read
                    // here; a font change goes through `ensure_scene`'s
                    // reload path instead.
                    loaded.set_text_params(&source.to_project(), *x, *y, *font_size, *color, true);
                }
                EditorLayer::Image { effects, .. }
                | EditorLayer::Gif { effects, .. }
                | EditorLayer::Adjustment { effects } => {
                    sync_effect_params(loaded, &self.renderer.queue, effects);
                }
            }
        }
    }

    /// Advances gif frames, updates the xray cursor and parallax pan, and
    /// redraws into the preview texture. `cursor_local` is in
    /// preview-texture pixel coordinates (i.e. already scaled -- see the
    /// caller). Returns whether the scene has any layer that needs
    /// continuous repainting.
    fn redraw(&mut self, cursor_local: Option<(f32, f32)>) -> bool {
        let elapsed_ms = self.loaded_at.elapsed().as_millis() as u64;
        self.renderer.advance_gifs(&mut self.layers, elapsed_ms);
        let cursor_px = cursor_local.unwrap_or((-1.0e6, -1.0e6));
        self.renderer.update_xray_cursors(&self.layers, cursor_px);
        self.renderer.update_smoke_cursors(&self.layers, cursor_px);
        self.renderer
            .update_shader_effects(&self.layers, cursor_px, elapsed_ms as f32 / 1000.0);
        let parallax_dt_ms = elapsed_ms.saturating_sub(self.last_parallax_update_ms);
        self.last_parallax_update_ms = elapsed_ms;
        self.renderer
            .update_parallax(&mut self.layers, cursor_px, parallax_dt_ms);
        self.renderer
            .advance_text_sources(&mut self.layers, chrono::Local::now());
        self.renderer
            .render_to_texture(&self.view, &self.layers, wgpu::Color::TRANSPARENT);

        self.layers.iter().any(LoadedLayer::is_dynamic)
    }
}

/// Converts `effects` to `project_format::Effect` and pushes them into
/// `loaded` via `LoadedLayer::set_effect_params` -- the per-layer-kind
/// arms of `Preview::sync_live_params` all do exactly this on top of
/// their own kind-specific live params, so it's pulled out here rather
/// than repeated three times. A no-op if `loaded`'s chain isn't shaped
/// like `effects` yet (see `set_effect_params`'s doc comment) -- that
/// only happens for the one frame between an edit that needs a reload
/// (see `EffectSignature`) and `ensure_scene` actually landing it.
fn sync_effect_params(loaded: &mut LoadedLayer, queue: &wgpu::Queue, effects: &[EditorEffect]) {
    let projected: Vec<project_format::Effect> =
        effects.iter().map(EditorEffect::to_project).collect();
    loaded.set_effect_params(queue, &projected);
}

/// Re-applies every currently-painted mask's live in-memory buffer onto
/// a freshly (re)loaded scene -- see the call site in `Preview::
/// ensure_scene` for why a reload otherwise silently discards any
/// painted mask's in-progress strokes. `layers`/`editor_layers` must
/// already be the same length and in the same order (true right after
/// `SceneRenderer::load_scene` builds `layers` from `editor_layers` via
/// `build_preview_project`).
fn restore_painted_masks(layers: &[LoadedLayer], editor_layers: &[EditorLayer], queue: &wgpu::Queue) {
    for (layer, editor_layer) in layers.iter().zip(editor_layers) {
        let Some(effects) = editor_layer.effects() else {
            continue;
        };
        for (effect_index, effect) in effects.iter().enumerate() {
            if let EditorMask::Texture {
                paint: Some(buffer),
                ..
            } = &effect.mask
            {
                let rgba = expand_gray_to_rgba(buffer);
                layer.write_mask_paint(
                    queue,
                    effect_index,
                    &rgba,
                    PAINT_MASK_RESOLUTION,
                    PAINT_MASK_RESOLUTION,
                );
            }
        }
    }
}

fn create_preview_texture(
    renderer: &SceneRenderer,
    width: u32,
    height: u32,
) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = renderer.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("editor-preview"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: player::OFFSCREEN_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}

/// Caps width at `PREVIEW_MAX_WIDTH` (and at the source's own resolution,
/// no point upscaling a preview past native size) and derives height to
/// preserve aspect ratio.
fn preview_size(natural_width: u32, natural_height: u32) -> (u32, u32) {
    if natural_width == 0 || natural_height == 0 {
        return (PREVIEW_MAX_WIDTH, PREVIEW_MAX_WIDTH * 9 / 16);
    }
    let width = PREVIEW_MAX_WIDTH.min(natural_width).max(1);
    let height = ((width as u64 * natural_height as u64) / natural_width as u64).max(1) as u32;
    (width, height)
}

/// `None` if there's nothing sensible to preview yet (no layers, or one
/// still missing a file) -- mirrors the same completeness check that
/// gates the "Save project as..." button.
fn build_preview_project(layers: &[EditorLayer]) -> Option<project_format::Project> {
    if layers.is_empty() || !layers.iter().all(EditorLayer::is_complete) {
        return None;
    }
    let converted = layers
        .iter()
        .map(|layer| match layer {
            // Paths here are absolute (straight from the file picker, no
            // project directory exists until the project is saved) --
            // `SceneRenderer::load_scene` joins them onto a base
            // directory, and `Path::join` with an absolute right-hand
            // side just returns that absolute path unchanged, so passing
            // `Path::new("")` as the base works out. The same trick
            // applies to a `Mask::Texture`'s path inside `effects` below
            // -- `EditorMask::to_project` always emits an absolute
            // display-string path, unconditionally, so it needs no
            // special-casing here either.
            EditorLayer::Image { path, effects } => project_format::Layer::Image {
                path: path
                    .as_ref()
                    .expect("checked complete above")
                    .display()
                    .to_string(),
                effects: effects.iter().map(EditorEffect::to_project).collect(),
            },
            EditorLayer::Xray {
                base,
                overlay,
                radius,
                effects,
            } => project_format::Layer::Xray {
                base: base
                    .as_ref()
                    .expect("checked complete above")
                    .display()
                    .to_string(),
                overlay: overlay
                    .as_ref()
                    .expect("checked complete above")
                    .display()
                    .to_string(),
                radius: *radius,
                effects: effects.iter().map(EditorEffect::to_project).collect(),
            },
            EditorLayer::Gif { path, effects } => project_format::Layer::Gif {
                path: path
                    .as_ref()
                    .expect("checked complete above")
                    .display()
                    .to_string(),
                effects: effects.iter().map(EditorEffect::to_project).collect(),
            },
            EditorLayer::Parallax {
                path,
                strength,
                smoothing,
                effects,
            } => project_format::Layer::Parallax {
                path: path
                    .as_ref()
                    .expect("checked complete above")
                    .display()
                    .to_string(),
                strength: *strength,
                smoothing: *smoothing,
                effects: effects.iter().map(EditorEffect::to_project).collect(),
            },
            EditorLayer::Text {
                x,
                y,
                font_size,
                color,
                source,
                font,
            } => project_format::Layer::Text {
                x: *x,
                y: *y,
                font_size: *font_size,
                color: *color,
                source: source.to_project(),
                font: font.to_project(),
            },
            EditorLayer::Adjustment { effects } => project_format::Layer::Adjustment {
                effects: effects.iter().map(EditorEffect::to_project).collect(),
            },
        })
        .collect();
    Some(project_format::Project {
        // Irrelevant to the preview itself -- this `Project` only exists
        // to hand to `load_scene`, which never reads `name`/`description`
        // at all.
        name: String::new(),
        description: String::new(),
        layers: converted,
        // Irrelevant to the preview itself (see `PREVIEW_REPAINT_INTERVAL`)
        // -- this `Project` only exists to hand to `load_scene`, which
        // never reads `fps` at all, so any value would do.
        fps: 30,
    })
}

fn build_signature(layers: &[EditorLayer]) -> Vec<LayerSignature> {
    layers
        .iter()
        .map(|layer| match layer {
            EditorLayer::Image { path, effects } => LayerSignature::Image(
                path.clone().expect("checked complete by caller"),
                effects.iter().map(effect_signature).collect(),
            ),
            EditorLayer::Xray {
                base,
                overlay,
                effects,
                ..
            } => LayerSignature::Xray(
                base.clone().expect("checked complete by caller"),
                overlay.clone().expect("checked complete by caller"),
                effects.iter().map(effect_signature).collect(),
            ),
            EditorLayer::Gif { path, effects } => LayerSignature::Gif(
                path.clone().expect("checked complete by caller"),
                effects.iter().map(effect_signature).collect(),
            ),
            EditorLayer::Parallax { path, effects, .. } => LayerSignature::Parallax(
                path.clone().expect("checked complete by caller"),
                effects.iter().map(effect_signature).collect(),
            ),
            EditorLayer::Text { font, .. } => LayerSignature::Text(match font {
                EditorTextFont::Bundled => TextFontSignature::Bundled,
                EditorTextFont::Custom { path } => {
                    TextFontSignature::Custom(path.clone().expect("checked complete by caller"))
                }
            }),
            EditorLayer::Adjustment { effects } => {
                LayerSignature::Adjustment(effects.iter().map(effect_signature).collect())
            }
        })
        .collect()
}

/// How often the *editor's own preview* redraws itself -- fixed,
/// independent of the project's `fps` field (the slider below, saved
/// into project.json and used by render-server to throttle its own
/// re-rendering; see `Project::fps`'s doc comment). The two are
/// unrelated: gif timing comes from each gif's own per-frame delays and
/// xray reacts to real cursor input, so nothing about how the preview
/// actually looks depends on the project's configured fps -- dragging
/// that slider has no effect on preview smoothness, by design.
const PREVIEW_REPAINT_INTERVAL: Duration = Duration::from_millis(1000 / 30);

struct EditorApp {
    tab: Tab,
    previous_tab: Tab,

    // -- About window state --
    /// Toggled by the "?" button and F1; the window closes itself
    /// (writes `false` back) when its own close button is clicked.
    show_about: bool,
    /// Lazily loaded on first open rather than eagerly at startup, since
    /// most sessions never open the About window at all.
    about_icon: Option<eframe::egui::TextureHandle>,

    // -- "Wallpapers" tab state --
    monitors: Vec<MonitorInfo>,
    /// Mirrors `monitors_config`'s on-disk file -- monitor id -> project dir.
    assignments: HashMap<String, PathBuf>,
    library: Vec<library::LibraryEntry>,
    /// Lazily-loaded preview textures, keyed by each entry's `preview_path`.
    thumbnails: HashMap<PathBuf, eframe::egui::TextureHandle>,
    /// Mirrors whether `autostart::desktop_file_path()` currently exists --
    /// read once at startup, then only ever changed by the checkbox itself
    /// (see `show_wallpapers_tab`), never re-polled from disk every frame.
    autostart_enabled: bool,
    pusher: Box<dyn ProjectPusher>,
    /// Project dir of the wallpaper whose "Apply" overlay (monitor
    /// picker) is currently open, if any.
    apply_overlay: Option<PathBuf>,
    /// Project dir of the wallpaper pending delete confirmation, if any.
    delete_overlay: Option<PathBuf>,

    // -- "Editor" tab state --
    layers: Vec<EditorLayer>,
    /// Index into `layers` of whichever one is currently selected in the
    /// list -- `None` until the user clicks one. Drives which layer (if
    /// any) shows a drag handle on top of the preview in `show_preview`;
    /// kept in sync with `layers` itself by every mutation site (add/
    /// remove/reorder/reset) so it never points past the end or at the
    /// wrong layer after a reorder.
    selected: Option<usize>,
    /// Index into the *selected layer's* `effects` list of whichever
    /// effect is currently active for mask editing -- `None` until the
    /// user clicks one in `show_effects_panel`. Drives the mask gizmo
    /// (Circle/Gradient's `Transform2D`) drawn on top of the preview in
    /// `show_preview_content`, same rationale as `selected` driving
    /// Text's own gizmo. Reset to `None` any time `selected` itself
    /// changes to point at a genuinely different layer (a new layer's
    /// effect stack has nothing to do with whatever index was active on
    /// the old one) -- but *not* on a plain layer reorder, which leaves
    /// the selected layer's own identity (and its effects list) alone.
    selected_effect: Option<usize>,
    fps: u32,
    status: String,
    preview: Option<Preview>,
    preview_error: Option<String>,
    /// Which library entry the editor tab is currently editing, if any --
    /// `None` until the first save of a brand-new project. Nothing
    /// remembered this before: every save used to open a fresh folder
    /// dialog with no memory of what was last opened.
    current_project_id: Option<String>,
    current_project_name: String,
    current_project_description: String,
}

impl Default for EditorApp {
    fn default() -> Self {
        Self {
            tab: Tab::Wallpapers,
            previous_tab: Tab::Wallpapers,
            show_about: false,
            about_icon: None,
            monitors: Vec::new(),
            assignments: monitors_config::load(),
            library: library::scan(),
            thumbnails: HashMap::new(),
            autostart_enabled: autostart::is_enabled(),
            pusher: Box::new(TcpPusher),
            apply_overlay: None,
            delete_overlay: None,
            layers: Vec::new(),
            selected: None,
            selected_effect: None,
            fps: 30,
            status: String::new(),
            preview: None,
            preview_error: None,
            current_project_id: None,
            current_project_name: String::new(),
            current_project_description: String::new(),
        }
    }
}

impl eframe::App for EditorApp {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, frame: &mut eframe::Frame) {
        let render_state = frame.wgpu_render_state().cloned();

        if ui.ctx().input(|i| i.key_pressed(eframe::egui::Key::F1)) {
            self.show_about = !self.show_about;
        }

        // Cheap (no IPC round trip -- just already-known compositor state)
        // to recompute every frame, so monitor hot-plug just shows up on
        // the next repaint with no manual "refresh" needed. Deduped by
        // name -- winit's Wayland backend has been observed to list the
        // same output more than once in a single `available_monitors()`
        // call, which without this showed e.g. 4 entries for 2 real
        // monitors.
        if let Some(window) = frame.winit_window() {
            let mut seen_names = std::collections::HashSet::new();
            self.monitors = window
                .available_monitors()
                .filter_map(|m| {
                    let name = m.name()?;
                    if !seen_names.insert(name.clone()) {
                        return None;
                    }
                    let size = m.size();
                    Some(MonitorInfo {
                        name,
                        width: size.width,
                        height: size.height,
                    })
                })
                .collect();
        }

        eframe::egui::Panel::top("tabs").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Wallpapers, "Wallpapers");
                ui.selectable_value(&mut self.tab, Tab::Discover, "Discover");
                ui.selectable_value(&mut self.tab, Tab::Editor, "Editor");

                ui.with_layout(
                    eframe::egui::Layout::right_to_left(eframe::egui::Align::Center),
                    |ui| {
                        if ui
                            .button("?")
                            .on_hover_text("About WP Linux (F1)")
                            .clicked()
                        {
                            self.show_about = true;
                        }
                    },
                );
            });
            ui.add_space(4.0);
        });

        self.show_about_window(ui.ctx());

        // Auto-rescan whenever the Wallpapers tab gains focus (e.g. a
        // project was dropped into the library by hand while this tab
        // wasn't showing) -- the Rescan button inside the tab itself is a
        // fallback/confidence action, not the only trigger.
        if self.previous_tab != Tab::Wallpapers && self.tab == Tab::Wallpapers {
            self.rescan_library();
        }
        self.previous_tab = self.tab;

        match self.tab {
            Tab::Wallpapers => {
                eframe::egui::CentralPanel::default().show(ui, |ui| {
                    self.show_wallpapers_tab(ui);
                });
            }
            Tab::Discover => {
                eframe::egui::CentralPanel::default().show(ui, |ui| {
                    self.show_discover_tab(ui);
                });
            }
            Tab::Editor => {
                eframe::egui::Panel::right("editor_sidebar")
                    .resizable(true)
                    .default_size(300.0)
                    .size_range(220.0..=900.0)
                    .show(ui, |ui| {
                        self.show_layers_panel(ui);
                    });

                eframe::egui::CentralPanel::default().show(ui, |ui| {
                    self.show_preview(ui, render_state.as_ref());
                });
            }
        }
    }
}

impl EditorApp {
    /// Renders the About window when `show_about` is set (a no-op
    /// otherwise -- `egui::Window::open` skips the contents closure
    /// entirely when its `open` flag is already false). Called every
    /// frame from `ui` rather than gated by an `if`, so the window's own
    /// close button and F1 both just flip `show_about` and this picks it
    /// up next frame either way.
    fn show_about_window(&mut self, ctx: &eframe::egui::Context) {
        let icon = self
            .about_icon
            .get_or_insert_with(|| load_about_icon(ctx));
        let icon_id = icon.id();

        let mut open = self.show_about;
        eframe::egui::Window::new("About WP Linux")
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add(eframe::egui::Image::from_texture((
                        icon_id,
                        eframe::egui::Vec2::splat(96.0),
                    )));
                    ui.add_space(8.0);
                    ui.heading("WP Linux Editor");
                    ui.label(
                        "An animated, interactive wallpaper engine for KDE Plasma 6 on Wayland.",
                    );
                    ui.add_space(8.0);
                    ui.label(format!("Version {APP_VERSION} ({APP_GIT_HASH})"));
                    ui.label("License: MIT");
                    ui.add_space(8.0);
                    ui.hyperlink_to("Repository", "https://github.com/AzimovIz/WP_Linux");
                    ui.hyperlink_to(
                        "Report an issue",
                        "https://github.com/AzimovIz/WP_Linux/issues",
                    );
                });
            });
        self.show_about = open;
    }

    fn show_discover_tab(&mut self, ui: &mut eframe::egui::Ui) {
        ui.centered_and_justified(|ui| {
            ui.heading("Discover — coming soon");
        });
    }

    fn rescan_library(&mut self) {
        self.library = library::scan();
        // Clear cached thumbnails too -- otherwise a just-resaved
        // project's changed preview.png would keep showing the old
        // texture, still cached under the same path.
        self.thumbnails.clear();
    }

    /// The three effects of picking a wallpaper for a monitor: remember it
    /// locally, persist it (so render-server picks it up on its next
    /// startup even if it's not running right now), and try to push it to
    /// a currently-running render-server so the change applies live.
    fn assign(&mut self, monitor_id: &str, project_dir: &Path) {
        self.assignments
            .insert(monitor_id.to_string(), project_dir.to_path_buf());
        if let Err(e) = monitors_config::save(&self.assignments) {
            self.status = format!("Failed to save monitor assignment: {e}");
            return;
        }
        match self.pusher.push(monitor_id, project_dir) {
            Ok(()) => self.status = format!("Assigned {monitor_id} -> {}", project_dir.display()),
            Err(e) => {
                self.status = format!("Assigned locally, but couldn't reach render-server: {e}");
            }
        }
    }

    fn show_wallpapers_tab(&mut self, ui: &mut eframe::egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Wallpapers");
            if ui.button("Rescan").clicked() {
                self.rescan_library();
            }

            ui.with_layout(
                eframe::egui::Layout::right_to_left(eframe::egui::Align::Center),
                |ui| {
                    let resp = ui
                        .checkbox(&mut self.autostart_enabled, "Launch at login")
                        .on_hover_text(
                            "Adds or removes a .desktop file in ~/.config/autostart so \
                             render-server starts automatically when you log in.",
                        );
                    if resp.changed()
                        && let Err(e) = autostart::set_enabled(self.autostart_enabled)
                    {
                        // Reflect what's actually on disk, not what the
                        // click asked for -- a failed write/remove
                        // shouldn't leave the checkbox lying about state.
                        self.autostart_enabled = autostart::is_enabled();
                        self.status = format!("Failed to update autostart: {e}");
                    }
                },
            );
        });
        ui.add_space(8.0);

        if self.library.is_empty() {
            ui.label("No wallpapers in the library yet -- save one from the Editor tab.");
        }

        eframe::egui::ScrollArea::vertical().show(ui, |ui| {
            // Always exactly `WALLPAPER_GRID_COLUMNS` tiles per row, but
            // resized every frame to fill the actually available width
            // instead of a fixed pixel size -- that fixed size used to
            // leave empty space on the right in a wide window and push
            // tiles past the right edge in a narrow one. Height scales
            // with width to keep `WALLPAPER_TILE_SIZE`'s aspect ratio.
            let columns = WALLPAPER_GRID_COLUMNS as f32;
            let spacing = ui.spacing().item_spacing.x;
            let tile_width = (ui.available_width() - spacing * (columns - 1.0)) / columns;
            let tile_size = eframe::egui::vec2(
                tile_width,
                tile_width * (WALLPAPER_TILE_SIZE.y / WALLPAPER_TILE_SIZE.x),
            );

            let entries = self.library.clone();
            for row in entries.chunks(WALLPAPER_GRID_COLUMNS) {
                ui.horizontal(|ui| {
                    for entry in row {
                        self.show_wallpaper_tile(ui, entry, tile_size);
                    }
                });
                ui.add_space(12.0);
            }
        });

        self.show_apply_overlay(ui.ctx());
        self.show_delete_overlay(ui.ctx());
    }

    /// One tile in the "Wallpapers" grid: a `tile_size`-sized preview (or
    /// a plain placeholder if this entry has no `preview.png` yet) with
    /// always-visible Edit/Apply icon buttons pinned to its
    /// top-right corner.
    fn show_wallpaper_tile(
        &mut self,
        ui: &mut eframe::egui::Ui,
        entry: &library::LibraryEntry,
        tile_size: eframe::egui::Vec2,
    ) {
        let display_name = if entry.name.is_empty() {
            entry.id.as_str()
        } else {
            entry.name.as_str()
        };

        let hover_text = format!(
            "{display_name} -- {} layer(s), {} fps",
            entry.layer_count, entry.fps
        );

        ui.vertical(|ui| {
            ui.set_width(tile_size.x);
            let (rect, response) =
                ui.allocate_exact_size(tile_size, eframe::egui::Sense::hover());

            if let Some(preview_path) = &entry.preview_path {
                let texture = self
                    .thumbnails
                    .entry(preview_path.clone())
                    .or_insert_with(|| load_thumbnail_texture(ui.ctx(), preview_path));
                ui.put(
                    rect,
                    eframe::egui::Image::from_texture((texture.id(), tile_size)),
                );
            } else {
                ui.painter()
                    .rect_filled(rect, 4.0, ui.visuals().faint_bg_color);
            }
            response.on_hover_text(hover_text);

            // Always rendered, not hover-revealed -- an earlier version
            // only drew these inside `if response.hovered()`, but they
            // sit inside the same rect that response's own hover check
            // covers, so gating their existence on it toggled them in
            // and out every time the pointer crossed their own bounds:
            // visible flicker, and clicks landing on a frame where the
            // button didn't exist yet. Always rendering them (as small
            // pinned icons instead of full-width labels, so they don't
            // dominate the thumbnail) sidesteps that entirely.
            let icon_size = eframe::egui::Vec2::splat(WALLPAPER_ICON_SIZE);
            let apply_rect = eframe::egui::Rect::from_min_size(
                eframe::egui::pos2(
                    rect.max.x - WALLPAPER_ICON_MARGIN - WALLPAPER_ICON_SIZE,
                    rect.min.y + WALLPAPER_ICON_MARGIN,
                ),
                icon_size,
            );
            let edit_rect = eframe::egui::Rect::from_min_size(
                apply_rect.min - eframe::egui::vec2(WALLPAPER_ICON_GAP + WALLPAPER_ICON_SIZE, 0.0),
                icon_size,
            );
            let delete_rect = eframe::egui::Rect::from_min_size(
                edit_rect.min - eframe::egui::vec2(WALLPAPER_ICON_GAP + WALLPAPER_ICON_SIZE, 0.0),
                icon_size,
            );
            if ui
                .put(delete_rect, eframe::egui::Button::new("🗑"))
                .on_hover_text("Delete")
                .clicked()
            {
                self.delete_overlay = Some(entry.dir.clone());
            }
            if ui
                .put(edit_rect, eframe::egui::Button::new("✏"))
                .on_hover_text("Edit")
                .clicked()
            {
                self.open_library_entry(entry);
            }
            if ui
                .put(apply_rect, eframe::egui::Button::new("🖥"))
                .on_hover_text("Apply")
                .clicked()
            {
                self.apply_overlay = Some(entry.dir.clone());
            }

            // Each `ui.put()` above (being an absolutely-positioned
            // child ui) advances this `ui`'s cursor to *its own* rect,
            // not the image's -- egui's cursor placement isn't clamped
            // to "never move backward", so after the icon buttons
            // (small rects near the *top* of the image) the cursor is
            // left sitting inside the image rather than below it. Left
            // alone, the label added next would start from there,
            // landing near the middle of the thumbnail instead of under
            // it. Re-advancing past the full image `rect` one more time
            // puts the cursor back where it belongs first.
            ui.advance_cursor_after_rect(rect);

            ui.label(display_name);
        });
    }

    /// The "Apply" modal: pick which monitor `self.apply_overlay`'s
    /// wallpaper should be assigned to, or cancel. Closes on Cancel, on a
    /// backdrop click, or on Escape (the latter two via `Modal`'s own
    /// `should_close`).
    fn show_apply_overlay(&mut self, ctx: &eframe::egui::Context) {
        let Some(dir) = self.apply_overlay.clone() else {
            return;
        };

        let modal_response =
            eframe::egui::Modal::new(eframe::egui::Id::new("apply_overlay")).show(ctx, |ui| {
                ui.heading("Apply to monitor");
                ui.add_space(8.0);

                if self.monitors.is_empty() {
                    ui.label("No monitors detected.");
                }
                for monitor in self.monitors.clone() {
                    if ui
                        .button(format!(
                            "{} ({}x{})",
                            monitor.name, monitor.width, monitor.height
                        ))
                        .clicked()
                    {
                        self.assign(&monitor.name, &dir);
                        self.apply_overlay = None;
                    }
                }

                ui.add_space(8.0);
                if ui.button("Cancel").clicked() {
                    self.apply_overlay = None;
                }
            });

        if modal_response.should_close() {
            self.apply_overlay = None;
        }
    }

    /// The delete confirmation modal: asks before permanently removing
    /// `self.delete_overlay`'s project directory from disk. Closes without
    /// deleting on Cancel, on a backdrop click, or on Escape (via
    /// `Modal`'s own `should_close`).
    fn show_delete_overlay(&mut self, ctx: &eframe::egui::Context) {
        let Some(dir) = self.delete_overlay.clone() else {
            return;
        };

        let display_name = self
            .library
            .iter()
            .find(|entry| entry.dir == dir)
            .map(|entry| {
                if entry.name.is_empty() {
                    entry.id.clone()
                } else {
                    entry.name.clone()
                }
            })
            .unwrap_or_else(|| dir.display().to_string());

        let modal_response =
            eframe::egui::Modal::new(eframe::egui::Id::new("delete_overlay")).show(ctx, |ui| {
                ui.heading("Delete wallpaper?");
                ui.add_space(8.0);
                ui.label(format!(
                    "\"{display_name}\" will be permanently deleted from disk. This can't be undone."
                ));
                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        self.delete_overlay = None;
                    }
                    if ui.button("Delete").clicked() {
                        match library::delete(&dir) {
                            Ok(()) => {
                                self.status = format!("Deleted {}", dir.display());
                                self.rescan_library();
                            }
                            Err(e) => {
                                self.status = format!("Failed to delete {}: {e}", dir.display());
                            }
                        }
                        self.delete_overlay = None;
                    }
                });
            });

        if modal_response.should_close() {
            self.delete_overlay = None;
        }
    }

    /// Resets the editor tab back to a blank, unsaved project -- the same
    /// state `EditorApp::default()` starts in, so "New" undoes whatever
    /// `open_library_entry`/layer edits did without needing to relaunch
    /// the app. `current_project_id` going back to `None` means the next
    /// Save writes a fresh library entry rather than overwriting whatever
    /// was open before.
    fn new_project(&mut self) {
        self.layers = Vec::new();
        self.selected = None;
        self.selected_effect = None;
        self.fps = 30;
        self.current_project_id = None;
        self.current_project_name = String::new();
        self.current_project_description = String::new();
        self.status = String::new();
    }

    /// Loads a library entry into the editor tab and switches to it --
    /// shared by the Wallpapers tab's "Edit" button and the
    /// Editor tab's own "Open" picker.
    fn open_library_entry(&mut self, entry: &library::LibraryEntry) {
        match open_project(&entry.dir) {
            Ok((layers, fps, description)) => {
                self.layers = layers;
                self.selected = None;
                self.selected_effect = None;
                self.fps = fps;
                self.current_project_id = Some(entry.id.clone());
                self.current_project_name = entry.name.clone();
                self.current_project_description = description;
                self.status = format!("Opened {}", entry.dir.display());
                self.tab = Tab::Editor;
            }
            Err(e) => {
                self.status = format!("Failed to open {}: {e}", entry.dir.display());
            }
        }
    }

    /// Left sidebar for the Editor tab: project-level actions (New/Open/
    /// Save, name/description/FPS), the "+ layer" buttons, and a compact
    /// list of the project's layers -- name plus reorder/remove/select
    /// only, no per-layer settings. Settings for whichever layer is
    /// `selected` live in the separate `show_layer_settings_panel`
    /// instead, master-detail style, so the list stays scannable
    /// regardless of how many sliders (and, once M4 lands, how deep an
    /// effect stack) any single layer's own settings need.
    fn show_layers_panel(&mut self, ui: &mut eframe::egui::Ui) {
        // Name and FPS share a row -- FPS's label shortened to just
        // "FPS" (the fuller explanation moved to a tooltip, so nothing
        // is actually lost) since there's no longer room for its old
        // "Target FPS (animated/cursor layers only):" wording next to
        // Name on the same line. New/Open/Save used to lead here, but
        // moved up to sit level with the "Preview" heading instead (see
        // `show_preview_content`) -- freeing this whole row was the
        // point, not just relabeling FPS.
        ui.horizontal(|ui| {
            ui.label("Name:");
            ui.add(
                eframe::egui::TextEdit::singleline(&mut self.current_project_name)
                    .desired_width(120.0),
            );
            ui.add_space(8.0);
            ui.label("FPS:").on_hover_text("Used by render-server once this project is loaded there -- doesn't affect the preview on the left, which always redraws at a fixed rate.");
            scroll_slider(ui, &mut self.fps, 1..=60);
        });

        ui.add_space(2.0);

        ui.label("Description:");
        // Capped to ~4 lines regardless of how much text is in there --
        // `TextEdit::multiline` alone has no such limit (it just grows to
        // fit everything typed into it), so the cap comes from wrapping
        // it in its own small `ScrollArea` instead. The height is an
        // approximation of 4 rows plus the text edit's own frame margin,
        // not pixel-exact -- being off by a line or two here just means
        // the 5th line peeks in before scrolling kicks in, not a real
        // layout problem the way the footer's old fixed-height guess was.
        eframe::egui::ScrollArea::vertical()
            .id_salt("description_scroll")
            .max_height(ui.text_style_height(&eframe::egui::TextStyle::Body) * 4.5)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.add(
                    eframe::egui::TextEdit::multiline(&mut self.current_project_description)
                        .desired_rows(4)
                        .desired_width(f32::INFINITY),
                );
            });

        ui.add_space(4.0);
        ui.label("Layers are drawn bottom to top -- the first one in the list is furthest back.");
        ui.add_space(2.0);

        // Wrapped, not a plain `horizontal` -- five un-wrapped buttons
        // would need more width than this panel's own configured
        // minimum (220px), which would silently override that minimum
        // and stop the panel from ever shrinking down to it.
        ui.horizontal_wrapped(|ui| {
            if ui.button("+ Image").clicked() {
                self.layers.push(EditorLayer::Image {
                    path: None,
                    effects: Vec::new(),
                });
                self.selected = Some(self.layers.len() - 1);
                self.selected_effect = None;
            }
            if ui.button("+ Xray").clicked() {
                self.layers.push(EditorLayer::Xray {
                    base: None,
                    overlay: None,
                    radius: 200.0,
                    effects: Vec::new(),
                });
                self.selected = Some(self.layers.len() - 1);
                self.selected_effect = None;
            }
            if ui.button("+ Gif").clicked() {
                self.layers.push(EditorLayer::Gif {
                    path: None,
                    effects: Vec::new(),
                });
                self.selected = Some(self.layers.len() - 1);
                self.selected_effect = None;
            }
            if ui.button("+ Parallax").clicked() {
                self.layers.push(EditorLayer::Parallax {
                    path: None,
                    strength: 0.05,
                    smoothing: 0.15,
                    effects: Vec::new(),
                });
                self.selected = Some(self.layers.len() - 1);
                self.selected_effect = None;
            }
            if ui.button("+ Text").clicked() {
                self.layers.push(EditorLayer::Text {
                    x: 0.5,
                    y: 0.5,
                    font_size: 0.05,
                    color: [1.0, 1.0, 1.0, 1.0],
                    source: EditorTextSource::Literal(String::new()),
                    font: EditorTextFont::Bundled,
                });
                // Selected immediately, not just appended -- a freshly
                // added Text layer's drag handle should be visible on
                // the preview right away, without the user needing to
                // separately discover that clicking a layer's row below
                // selects it.
                self.selected = Some(self.layers.len() - 1);
                self.selected_effect = None;
            }
            if ui.button("+ Adjustment").clicked() {
                self.layers.push(EditorLayer::Adjustment {
                    effects: Vec::new(),
                });
                self.selected = Some(self.layers.len() - 1);
                self.selected_effect = None;
            }
        });

        ui.add_space(4.0);

        let mut move_up = None;
        let mut move_down = None;
        let mut remove = None;

        // Tighter than the ambient default (`ui.spacing().item_spacing.y`,
        // ~3px) -- with only ~4 rows visible at once before scrolling,
        // every pixel of gap between them is one the user has to scroll
        // an extra layer's worth to get back. `list_height` uses the
        // same constant so the ScrollArea's own height stays sized for
        // exactly 4 rows at this tighter spacing, not the looser default.
        const LAYER_LIST_ROW_SPACING: f32 = 1.0;

        // Capped to ~4 rows regardless of how many layers there are --
        // same reasoning and the same kind of approximation as the
        // description box above (a hand-measured row height, not
        // pixel-exact), but here *not* shrinking below that when there
        // are fewer than 4 layers either (`auto_shrink`'s height axis is
        // `false`, not `true`): a list that resized itself shorter every
        // time a layer was removed would shove the settings panel below
        // it up and down on every edit, which is worse than a little
        // fixed blank space at the bottom sometimes.
        let row_height = ui
            .spacing()
            .interact_size
            .y
            .max(ui.text_style_height(&eframe::egui::TextStyle::Body));
        let list_height = (row_height + LAYER_LIST_ROW_SPACING) * 4.0;

        eframe::egui::ScrollArea::vertical()
            .id_salt("layer_list_scroll")
            .max_height(list_height)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = LAYER_LIST_ROW_SPACING;
                // `iter()`, not `iter_mut()` -- this row no longer draws
                // the layer's own settings (that moved to
                // `show_layer_settings_panel`), so nothing here needs to
                // mutate an individual layer, only `self.layers` as a
                // whole via `remove`/`move_up`/`move_down` below.
                for (index, layer) in self.layers.iter().enumerate() {
                    ui.horizontal(|ui| {
                        // Selectable, not just a label -- clicking it is
                        // how `selected` gets set, which drives both the
                        // settings panel on the right and the drag
                        // handle on the preview (currently only wired up
                        // for Text -- see `show_preview`).
                        if ui
                            .selectable_label(
                                self.selected == Some(index),
                                format!("#{} {}", index + 1, layer.label()),
                            )
                            .on_hover_text("Select this layer to edit its settings")
                            .clicked()
                        {
                            self.selected = Some(index);
                            self.selected_effect = None;
                        }
                        if ui.small_button("up").clicked() {
                            move_up = Some(index);
                        }
                        if ui.small_button("down").clicked() {
                            move_down = Some(index);
                        }
                        if ui.small_button("remove").clicked() {
                            remove = Some(index);
                        }
                    });
                }
            });

        if let Some(index) = remove {
            self.layers.remove(index);
            // Keep `selected` pointing at the same layer it did before
            // the removal (shifted down by one once its own index is
            // gone), rather than left dangling past the end of the
            // shrunk `layers` or silently pointing at whatever layer
            // slid into the removed slot.
            self.selected = match self.selected {
                Some(selected) if selected == index => None,
                Some(selected) if selected > index => Some(selected - 1),
                other => other,
            };
            // Only reset if the removed layer *was* the selected one
            // (`selected` just went to `None` above) -- a shift-down or
            // an unrelated layer's removal leaves the still-selected
            // layer's own effects list, and therefore this index into
            // it, untouched.
            if self.selected.is_none() {
                self.selected_effect = None;
            }
        }
        if let Some(index) = move_up
            && index > 0
        {
            self.layers.swap(index, index - 1);
            self.selected = match self.selected {
                Some(selected) if selected == index => Some(index - 1),
                Some(selected) if selected == index - 1 => Some(index),
                other => other,
            };
        }
        if let Some(index) = move_down
            && index + 1 < self.layers.len()
        {
            self.layers.swap(index, index + 1);
            self.selected = match self.selected {
                Some(selected) if selected == index => Some(index + 1),
                Some(selected) if selected == index + 1 => Some(index),
                other => other,
            };
        }

        ui.add_space(2.0);
        ui.separator();
        ui.add_space(2.0);

        // Settings for whichever layer is selected above -- stacked
        // below the list in this same sidebar column (master-detail),
        // filling whatever vertical space the list's fixed ~4 rows left
        // behind instead of a separate side-by-side panel.
        self.show_layer_settings_panel(ui);
    }

    /// The master-detail counterpart to the layer list above it in
    /// `show_layers_panel`: settings for whichever layer is currently
    /// `selected`, and nothing else. Empty state (`selected` is `None`,
    /// or stale past the end of `layers`, which list mutations should
    /// never actually leave it -- see the bookkeeping in
    /// `show_layers_panel`) shows a prompt instead of a panicking
    /// `layers[index]`.
    fn show_layer_settings_panel(&mut self, ui: &mut eframe::egui::Ui) {
        let Some(index) = self.selected else {
            ui.heading("Layer settings");
            ui.add_space(2.0);
            ui.label("Select a layer on the left to edit its settings.");
            return;
        };
        let Some(layer) = self.layers.get_mut(index) else {
            self.selected = None;
            ui.heading("Layer settings");
            ui.add_space(2.0);
            ui.label("Select a layer on the left to edit its settings.");
            return;
        };

        // Combined into one heading (rather than a generic "Layer
        // settings" heading followed by a separate "#N Kind" line) --
        // the layer list above already shows which one's selected, so
        // repeating it on its own line was just extra vertical space
        // for no new information.
        ui.heading(format!("Layer settings \u{2014} #{} {}", index + 1, layer.label()));
        ui.add_space(2.0);
        ui.separator();
        ui.add_space(2.0);

        // Fills whatever's left in the sidebar column (`auto_shrink`'s
        // height axis is `false`, not `true`) rather than shrinking to
        // its content -- this is the last thing in the column, so
        // nothing else needs the space back.
        eframe::egui::ScrollArea::vertical()
            .id_salt("layer_settings_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                show_layer_panel(ui, layer, &mut self.selected_effect);
            });
    }

    fn show_preview(
        &mut self,
        ui: &mut eframe::egui::Ui,
        render_state: Option<&eframe::egui_wgpu::RenderState>,
    ) {
        // Reserved as a bottom panel *before* the central content below is
        // laid out, so the status line stays pinned to the bottom of the
        // preview column with the image sized to whatever's left above it,
        // rather than trailing immediately after the image with a ragged
        // gap underneath (the previous plain top-down `ui.label` did the
        // latter).
        if !self.status.is_empty() {
            eframe::egui::Panel::bottom("preview_status")
                .show_separator_line(false)
                .show(ui, |ui| {
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(8.0);
                    ui.label(&self.status);
                });
        }

        eframe::egui::CentralPanel::default()
            .frame(eframe::egui::Frame::NONE)
            .show(ui, |ui| {
                self.show_preview_content(ui, render_state);
            });
    }

    fn show_preview_content(
        &mut self,
        ui: &mut eframe::egui::Ui,
        render_state: Option<&eframe::egui_wgpu::RenderState>,
    ) {
        // New/Open/Save share the heading's row now, rather than
        // leading `show_layers_panel`'s own column below -- they used
        // to cost that column a whole row, which mattered once the
        // sidebar started running out of vertical room for the layer
        // list/settings/effects. The actions themselves (reset, load,
        // persist everything in `self.layers`) are unchanged, just
        // relocated.
        ui.horizontal(|ui| {
            ui.heading("Preview");
            ui.add_space(12.0);
            if ui.button("New").clicked() {
                self.new_project();
            }

            ui.menu_button("Open", |ui| {
                if self.library.is_empty() {
                    ui.label("Library is empty.");
                }
                for entry in self.library.clone() {
                    let display_name = if entry.name.is_empty() {
                        entry.id.clone()
                    } else {
                        entry.name.clone()
                    };
                    if ui.button(display_name).clicked() {
                        self.open_library_entry(&entry);
                        ui.close();
                    }
                }
            });

            let can_save =
                !self.layers.is_empty() && self.layers.iter().all(EditorLayer::is_complete);
            let save_label = if self.current_project_id.is_some() {
                "Save"
            } else {
                "Save (new)"
            };
            if ui
                .add_enabled(can_save, eframe::egui::Button::new(save_label))
                .clicked()
            {
                let id = self
                    .current_project_id
                    .clone()
                    .unwrap_or_else(library::new_project_id);
                let dir = library::project_dir(&id);
                match save_project(
                    &dir,
                    &self.layers,
                    self.fps,
                    &self.current_project_name,
                    &self.current_project_description,
                ) {
                    Ok(()) => {
                        // Auto-trust: the user just typed this command
                        // themselves and saved it, which is exactly the
                        // self-authoring context that makes a separate
                        // consent dialog unnecessary in this pass (see
                        // `trust_store`'s module doc comment). Without
                        // this, render-server would refuse to ever run
                        // it, even though it was authored here.
                        let has_command_layer = self.layers.iter().any(|layer| {
                            matches!(layer, EditorLayer::Text { source, .. } if source.is_command())
                        });
                        if has_command_layer && let Err(e) = trust_store::mark_trusted(&id) {
                            eprintln!("editor: failed to update the trust store for {id:?}: {e}");
                        }
                        self.current_project_id = Some(id);
                        if let Some(preview) = &self.preview
                            && let Err(e) =
                                generate_thumbnail(preview, &dir.join(library::PREVIEW_FILE_NAME))
                        {
                            eprintln!("editor: failed to generate thumbnail for {dir:?}: {e}");
                        }
                        self.rescan_library();
                        self.status = format!("Saved to {}", dir.display());
                    }
                    Err(e) => self.status = format!("Failed to save: {e}"),
                }
            }
        });
        ui.add_space(4.0);

        let Some(render_state) = render_state else {
            ui.label("GPU preview unavailable (eframe isn't running on wgpu).");
            return;
        };

        let preview = self
            .preview
            .get_or_insert_with(|| Preview::new(render_state));

        match preview.ensure_scene(render_state, &self.layers) {
            Ok(()) => self.preview_error = None,
            Err(e) => self.preview_error = Some(e),
        }

        if preview.layers.is_empty() {
            ui.label("Add layers with files set to see a preview.");
        } else {
            preview.sync_live_params(&self.layers);

            // Fit the texture into whatever space the (resizable) panel
            // gives us, preserving aspect ratio -- the texture itself
            // stays small and cheap to render (see `PREVIEW_MAX_WIDTH`),
            // the GPU just upscales it for display like any other image.
            let available = ui.available_size();
            let aspect = preview.width as f32 / preview.height as f32;
            let display_size = if available.x / aspect <= available.y {
                eframe::egui::vec2(available.x, available.x / aspect)
            } else {
                eframe::egui::vec2(available.y * aspect, available.y)
            };
            let response = ui.add(eframe::egui::Image::from_texture((
                preview.texture_id,
                display_size,
            )));

            // The selected layer (if it's a Text layer -- the only kind
            // with a position today) gets a dashed bounding box overlaid
            // on the preview, as an on-canvas alternative to its x/y and
            // font_size sliders: drag anywhere inside it to move, drag
            // the small handle at its top-right corner to resize.
            if let Some(index) = self.selected
                && let Some(EditorLayer::Text { x, y, font_size, .. }) =
                    self.layers.get_mut(index)
            {
                // Screen-space top-left corner -- `response.rect` rescaled
                // by (x, y), both already the same normalized 0.0..=1.0
                // fractions the shader/glyphon layout itself uses (see
                // `Layer::Text`'s doc comment); the exact inverse of
                // `cursor_local`'s conversion below.
                let origin = eframe::egui::pos2(
                    response.rect.min.x + *x * response.rect.width(),
                    response.rect.min.y + *y * response.rect.height(),
                );

                // The shaped text's own size lives in canvas-pixel space
                // (the same units `x`/`y` are multiplied into for the
                // glyphon layout itself) -- rescale into screen-pixel
                // space the same way `origin` above already is. A
                // minimum keeps the box (and its resize handle) grabbable
                // even for an empty/just-added string, which shapes to
                // zero size.
                let (text_w, text_h) = preview
                    .layers
                    .get(index)
                    .and_then(LoadedLayer::text_size)
                    .unwrap_or((0.0, 0.0));
                let to_screen_x = response.rect.width() / preview.width.max(1) as f32;
                let to_screen_y = response.rect.height() / preview.height.max(1) as f32;
                let size = eframe::egui::vec2(
                    (text_w * to_screen_x).max(20.0),
                    (text_h * to_screen_y).max(20.0),
                );
                let body_rect = eframe::egui::Rect::from_min_size(origin, size);

                // Whole box moves the layer -- registered *before* the
                // resize handle below so the handle wins hit-testing
                // where the two overlap (egui breaks position ties in
                // favor of whichever `interact` was called last, i.e.
                // "topmost"; see `hit_test::hit_test`'s doc comment).
                let body_response = ui.interact(
                    body_rect,
                    ui.id().with("layer_gizmo_body"),
                    eframe::egui::Sense::drag(),
                );
                if body_response.dragged() {
                    let delta = body_response.drag_delta();
                    *x = (*x + delta.x / response.rect.width().max(1.0)).clamp(0.0, 1.0);
                    *y = (*y + delta.y / response.rect.height().max(1.0)).clamp(0.0, 1.0);
                }

                // Small handle at the bottom-right corner resizes -- the
                // box is anchored at its top-left (x, y) origin and grows
                // down/right as `font_size` grows, so the bottom-right
                // corner is the one that actually moves with it. Drag
                // down to grow, up to shrink, matching `font_size`'s own
                // definition as a fraction of canvas *height* (see
                // `Layer::Text`'s doc comment).
                let handle_pos = eframe::egui::pos2(body_rect.right(), body_rect.bottom());
                let handle_rect = eframe::egui::Rect::from_center_size(
                    handle_pos,
                    eframe::egui::vec2(12.0, 12.0),
                );
                let handle_response = ui
                    .interact(
                        handle_rect,
                        ui.id().with("layer_gizmo_resize"),
                        eframe::egui::Sense::drag(),
                    )
                    .on_hover_and_drag_cursor(eframe::egui::CursorIcon::ResizeSouthEast)
                    .on_hover_text("Drag to change font size");
                if handle_response.dragged() {
                    let delta = handle_response.drag_delta();
                    *font_size = (*font_size + delta.y / response.rect.height().max(1.0))
                        .clamp(0.01, 0.5);
                }

                let color = if body_response.dragged() || handle_response.dragged() {
                    eframe::egui::Color32::YELLOW
                } else {
                    eframe::egui::Color32::WHITE
                };
                let corners = [
                    body_rect.left_top(),
                    body_rect.right_top(),
                    body_rect.right_bottom(),
                    body_rect.left_bottom(),
                    body_rect.left_top(),
                ];
                ui.painter().extend(eframe::egui::Shape::dashed_line(
                    &corners,
                    eframe::egui::Stroke::new(1.5, color),
                    6.0,
                    4.0,
                ));
                ui.painter().circle_filled(handle_pos, 4.0, color);
            }

            // The selected layer's selected effect (if any, and if its
            // mask has a `Transform2D` at all -- `None`/`Texture` don't)
            // gets its own gizmo on the preview, same on-canvas-instead-
            // of-just-sliders rationale as Text's gizmo above, just
            // editing a mask's transform instead of a layer's position.
            if let Some(layer_index) = self.selected
                && let Some(effect_index) = self.selected_effect
                && let Some(layer) = self.layers.get_mut(layer_index)
                && let Some(mask) = layer
                    .effects_mut()
                    .and_then(|effects| effects.get_mut(effect_index))
                    .map(|effect| &mut effect.mask)
            {
                match mask {
                    EditorMask::Circle { transform, .. } => {
                        show_circle_mask_gizmo(ui, &response, transform);
                    }
                    EditorMask::Gradient { transform, .. } => {
                        show_gradient_mask_gizmo(ui, &response, transform);
                    }
                    EditorMask::Texture {
                        painting: true,
                        paint: Some(buffer),
                        brush_radius,
                        brush_softness,
                        last_paint_pos,
                        ..
                    } => {
                        // A dedicated interact region over the whole
                        // preview, same pattern the gizmos above use
                        // for their own handles -- `response` itself
                        // only senses hover (see its own
                        // construction above), not drag.
                        let paint_interact = ui.interact(
                            response.rect,
                            ui.id().with("mask_paint"),
                            eframe::egui::Sense::drag(),
                        );
                        if let Some(pos) = paint_interact.interact_pointer_pos() {
                            let uv = (
                                (pos.x - response.rect.min.x) / response.rect.width().max(1.0),
                                (pos.y - response.rect.min.y)
                                    / response.rect.height().max(1.0),
                            );
                            let erase = ui.input(|i| i.modifiers.shift);
                            stamp_paint_buffer(
                                buffer,
                                PAINT_MASK_RESOLUTION,
                                *last_paint_pos,
                                uv,
                                *brush_radius,
                                *brush_softness,
                                erase,
                            );
                            *last_paint_pos = Some(uv);
                            if let Some(loaded) = preview.layers.get(layer_index) {
                                let rgba = expand_gray_to_rgba(buffer);
                                loaded.write_mask_paint(
                                    &preview.renderer.queue,
                                    effect_index,
                                    &rgba,
                                    PAINT_MASK_RESOLUTION,
                                    PAINT_MASK_RESOLUTION,
                                );
                            }
                        } else {
                            *last_paint_pos = None;
                        }
                        // Brush-size feedback ring at the cursor,
                        // aspect-corrected the same way
                        // `show_circle_mask_gizmo` already corrects
                        // its own radius -- painting is *not*
                        // aspect-corrected in the buffer itself (see
                        // `stamp_paint_buffer`'s doc comment), but
                        // the on-screen cursor still ought to show
                        // roughly the shape that'll land given the
                        // canvas's own aspect ratio isn't 1:1.
                        if let Some(pos) = response.hover_pos() {
                            let color = if ui.input(|i| i.modifiers.shift) {
                                eframe::egui::Color32::LIGHT_RED
                            } else {
                                eframe::egui::Color32::WHITE
                            };
                            ui.painter().circle_stroke(
                                pos,
                                *brush_radius * response.rect.height(),
                                eframe::egui::Stroke::new(1.5, color),
                            );
                        }
                    }
                    EditorMask::None | EditorMask::Texture { .. } => {}
                }
            }

            // Displayed size and actual texture size differ (the image is
            // stretched to fill the panel), so a hover position in screen
            // space needs rescaling back down to texture pixel space --
            // that's the coordinate system the xray shader's cursor
            // uniform and `sync_radii`'s scaling both operate in.
            let cursor_local = response.hover_pos().map(|pos| {
                let local = pos - response.rect.min;
                let scale_x = preview.width as f32 / response.rect.width().max(1.0);
                let scale_y = preview.height as f32 / response.rect.height().max(1.0);
                (local.x * scale_x, local.y * scale_y)
            });

            let has_dynamic = preview.redraw(cursor_local);
            if has_dynamic {
                ui.ctx().request_repaint_after(PREVIEW_REPAINT_INTERVAL);
            }
        }

        if let Some(err) = &self.preview_error {
            ui.add_space(4.0);
            ui.colored_label(eframe::egui::Color32::RED, err);
        }
    }
}

/// Drag-to-move + drag-to-resize gizmo for a Circle mask's
/// `Transform2D`, drawn over `response` (the preview image). Doesn't
/// touch `transform.rotation` -- a circle is rotationally symmetric,
/// and `mask_blend.wgsl`'s Circle branch never reads it.
///
/// Mirrors the geometry `mask_blend.wgsl` itself computes: `radius =
/// transform.scale * 0.5`, aspect-corrected against the canvas's own
/// width/height so it reads as a true circle rather than an ellipse on
/// a non-square canvas. Since `response.rect` already preserves the
/// canvas's real aspect ratio (see `show_preview_content`'s
/// `display_size`), that correction collapses into a single
/// screen-space radius using `rect.height()` as the reference axis --
/// the same derivation the aspect-corrected UV distance in the shader
/// works out to once both axes are converted through a rect that's
/// already proportioned to match.
fn show_circle_mask_gizmo(
    ui: &mut eframe::egui::Ui,
    response: &eframe::egui::Response,
    transform: &mut project_format::Transform2D,
) {
    let center = eframe::egui::pos2(
        response.rect.min.x + transform.x * response.rect.width(),
        response.rect.min.y + transform.y * response.rect.height(),
    );
    let screen_radius = (transform.scale * 0.5) * response.rect.height();
    let edge_pos = center + eframe::egui::vec2(screen_radius, 0.0);

    // Center handle moves the mask -- registered before the edge
    // handle so the edge handle wins hit-testing on overlap (small
    // `scale` puts them close together), same "last interact() wins"
    // rationale as the Text gizmo above.
    let center_response = ui.interact(
        eframe::egui::Rect::from_center_size(center, eframe::egui::vec2(14.0, 14.0)),
        ui.id().with("mask_gizmo_circle_center"),
        eframe::egui::Sense::drag(),
    );
    if center_response.dragged() {
        let delta = center_response.drag_delta();
        transform.x = (transform.x + delta.x / response.rect.width().max(1.0)).clamp(0.0, 1.0);
        transform.y = (transform.y + delta.y / response.rect.height().max(1.0)).clamp(0.0, 1.0);
    }

    let edge_response = ui
        .interact(
            eframe::egui::Rect::from_center_size(edge_pos, eframe::egui::vec2(12.0, 12.0)),
            ui.id().with("mask_gizmo_circle_edge"),
            eframe::egui::Sense::drag(),
        )
        .on_hover_and_drag_cursor(eframe::egui::CursorIcon::ResizeHorizontal)
        .on_hover_text("Drag to resize");
    if edge_response.dragged() {
        let delta = edge_response.drag_delta();
        let new_screen_radius = (screen_radius + delta.x).max(1.0);
        transform.scale = ((new_screen_radius / response.rect.height().max(1.0)) * 2.0)
            .clamp(0.05, 3.0);
    }

    let color = if center_response.dragged() || edge_response.dragged() {
        eframe::egui::Color32::YELLOW
    } else {
        eframe::egui::Color32::WHITE
    };
    const CIRCLE_SEGMENTS: usize = 48;
    let points: Vec<eframe::egui::Pos2> = (0..=CIRCLE_SEGMENTS)
        .map(|i| {
            let angle = (i as f32 / CIRCLE_SEGMENTS as f32) * std::f32::consts::TAU;
            center + eframe::egui::vec2(angle.cos(), angle.sin()) * screen_radius
        })
        .collect();
    ui.painter().extend(eframe::egui::Shape::dashed_line(
        &points,
        eframe::egui::Stroke::new(1.5, color),
        6.0,
        4.0,
    ));
    ui.painter().circle_filled(center, 4.0, color);
    ui.painter().circle_filled(edge_pos, 4.0, color);
}

/// Drag-to-move + drag-to-rotate-and-scale gizmo for a Gradient mask's
/// `Transform2D`. `mask_blend.wgsl`'s Gradient branch measures its
/// `dot(centered, dir)` span in *raw* UV space, not aspect-corrected
/// like Circle -- so this gizmo deliberately converts `x`/`y`/rotation/
/// scale to screen space through the same non-uniform (`rect.width()`
/// vs `rect.height()`) scaling the shader's own UV space implies,
/// rather than correcting it into a visually "true" angle. On a
/// non-square canvas the on-screen handle will look slightly skewed
/// from its numeric rotation as a result -- that's the gizmo faithfully
/// showing what actually renders, not a bug to square away.
fn show_gradient_mask_gizmo(
    ui: &mut eframe::egui::Ui,
    response: &eframe::egui::Response,
    transform: &mut project_format::Transform2D,
) {
    let to_screen = |uv: eframe::egui::Vec2| -> eframe::egui::Pos2 {
        eframe::egui::pos2(
            response.rect.min.x + uv.x * response.rect.width(),
            response.rect.min.y + uv.y * response.rect.height(),
        )
    };
    let position_uv = eframe::egui::vec2(transform.x, transform.y);
    let angle_rad = transform.rotation.to_radians();
    let end_uv = position_uv + eframe::egui::vec2(angle_rad.cos(), angle_rad.sin()) * transform.scale;

    let center = to_screen(position_uv);
    let end_pos = to_screen(end_uv);

    // Center handle moves the mask -- registered before the end handle
    // so the end handle wins hit-testing on overlap (small `scale`
    // puts them close together), same convention as every other gizmo
    // here.
    let center_response = ui.interact(
        eframe::egui::Rect::from_center_size(center, eframe::egui::vec2(14.0, 14.0)),
        ui.id().with("mask_gizmo_gradient_center"),
        eframe::egui::Sense::drag(),
    );
    if center_response.dragged() {
        let delta = center_response.drag_delta();
        transform.x = (transform.x + delta.x / response.rect.width().max(1.0)).clamp(0.0, 1.0);
        transform.y = (transform.y + delta.y / response.rect.height().max(1.0)).clamp(0.0, 1.0);
    }

    let end_response = ui
        .interact(
            eframe::egui::Rect::from_center_size(end_pos, eframe::egui::vec2(12.0, 12.0)),
            ui.id().with("mask_gizmo_gradient_end"),
            eframe::egui::Sense::drag(),
        )
        .on_hover_text("Drag to rotate/resize the gradient");
    if end_response.dragged() {
        let delta = end_response.drag_delta();
        let delta_uv = eframe::egui::vec2(
            delta.x / response.rect.width().max(1.0),
            delta.y / response.rect.height().max(1.0),
        );
        let to_end = (end_uv + delta_uv) - position_uv;
        let new_rotation = to_end.y.atan2(to_end.x).to_degrees().rem_euclid(360.0);
        transform.scale = to_end.length().max(0.0001).clamp(0.05, 3.0);
        transform.rotation = new_rotation.clamp(0.0, 360.0);
    }

    let color = if center_response.dragged() || end_response.dragged() {
        eframe::egui::Color32::YELLOW
    } else {
        eframe::egui::Color32::WHITE
    };
    ui.painter().line_segment(
        [center, end_pos],
        eframe::egui::Stroke::new(1.5, color),
    );
    ui.painter().circle_filled(center, 4.0, color);
    ui.painter().circle_filled(end_pos, 4.0, color);
}

/// Property panel for one layer in the editor's layer list --
/// dispatches to a per-variant panel below, mirroring `EditorLayer`'s
/// own shape one level down.
fn show_layer_panel(
    ui: &mut eframe::egui::Ui,
    layer: &mut EditorLayer,
    selected_effect: &mut Option<usize>,
) {
    match layer {
        EditorLayer::Image { path, effects } => {
            show_image_panel(ui, path, effects, selected_effect)
        }
        EditorLayer::Xray {
            base,
            overlay,
            radius,
            effects,
        } => show_xray_panel(ui, base, overlay, radius, effects, selected_effect),
        EditorLayer::Gif { path, effects } => show_gif_panel(ui, path, effects, selected_effect),
        EditorLayer::Parallax {
            path,
            strength,
            smoothing,
            effects,
        } => show_parallax_panel(ui, path, strength, smoothing, effects, selected_effect),
        EditorLayer::Text {
            x,
            y,
            font_size,
            color,
            source,
            font,
        } => show_text_panel(ui, x, y, font_size, color, source, font),
        EditorLayer::Adjustment { effects } => show_adjustment_panel(ui, effects, selected_effect),
    }
}

fn show_image_panel(
    ui: &mut eframe::egui::Ui,
    path: &mut Option<PathBuf>,
    effects: &mut Vec<EditorEffect>,
    selected_effect: &mut Option<usize>,
) {
    path_picker(ui, "Picture", path, &["png", "jpg", "jpeg", "webp"]);
    show_effects_panel(ui, effects, selected_effect);
}

/// No path/other fields to show -- an Adjustment layer is entirely its
/// own effects stack, applied to the composite of every layer below it
/// (see `project_format::Layer::Adjustment`'s doc comment). Reuses
/// `show_effects_panel` as-is; the only thing specific to this layer
/// kind is the explanatory label.
fn show_adjustment_panel(
    ui: &mut eframe::egui::Ui,
    effects: &mut Vec<EditorEffect>,
    selected_effect: &mut Option<usize>,
) {
    ui.label("Applies to everything below this layer in the list.");
    show_effects_panel(ui, effects, selected_effect);
}

fn show_xray_panel(
    ui: &mut eframe::egui::Ui,
    base: &mut Option<PathBuf>,
    overlay: &mut Option<PathBuf>,
    radius: &mut f32,
    effects: &mut Vec<EditorEffect>,
    selected_effect: &mut Option<usize>,
) {
    path_picker(ui, "Base picture", base, &["png", "jpg", "jpeg", "webp"]);
    path_picker(
        ui,
        "Overlay picture (shown near cursor)",
        overlay,
        &["png", "jpg", "jpeg", "webp"],
    );
    labeled_slider(ui, "Radius (px):", radius, 20.0..=800.0);
    show_effects_panel(ui, effects, selected_effect);
}

fn show_gif_panel(
    ui: &mut eframe::egui::Ui,
    path: &mut Option<PathBuf>,
    effects: &mut Vec<EditorEffect>,
    selected_effect: &mut Option<usize>,
) {
    path_picker(ui, "Gif file", path, &["gif"]);
    show_effects_panel(ui, effects, selected_effect);
}

fn show_parallax_panel(
    ui: &mut eframe::egui::Ui,
    path: &mut Option<PathBuf>,
    strength: &mut f32,
    smoothing: &mut f32,
    effects: &mut Vec<EditorEffect>,
    selected_effect: &mut Option<usize>,
) {
    path_picker(
        ui,
        "Picture (bigger than your desktop resolution works best)",
        path,
        &["png", "jpg", "jpeg", "webp"],
    );
    labeled_slider(ui, "Strength:", strength, -0.4..=0.4)
        .on_hover_text("How far the layer pans at the screen edge, as a fraction of its own size. Negative pans towards the cursor instead of away from it.");
    labeled_slider(ui, "Smoothing (s):", smoothing, 0.0..=1.0).on_hover_text(
        "How long the pan takes to ease towards the cursor. 0 = track instantly.",
    );
    show_effects_panel(ui, effects, selected_effect);
}

/// Add/remove/reorder/configure a layer's post-processing stack --
/// shared by every layer kind that has one (Image/Xray/Gif/Parallax;
/// Text never does, see `project_format::Layer::Text`'s doc comment).
/// Mirrors `show_layers_panel`'s own list -- a compact row per effect
/// (enabled checkbox, kind label, reorder/remove) with the effect's own
/// param sliders and mask config nested inside via `ui.group`.
fn show_effects_panel(
    ui: &mut eframe::egui::Ui,
    effects: &mut Vec<EditorEffect>,
    selected_effect: &mut Option<usize>,
) {
    ui.add_space(2.0);
    ui.separator();
    ui.add_space(2.0);
    ui.strong("Effects");
    ui.add_space(2.0);

    ui.horizontal_wrapped(|ui| {
        if ui.button("+ Vignette").clicked() {
            effects.push(EditorEffect {
                kind: EditorEffectKind::Vignette {
                    strength: 0.5,
                    softness: 0.5,
                },
                mask: EditorMask::None,
                enabled: true,
            });
        }
        if ui.button("+ Color adjust").clicked() {
            effects.push(EditorEffect {
                kind: EditorEffectKind::ColorAdjust {
                    brightness: 0.0,
                    contrast: 0.0,
                    saturation: 0.0,
                },
                mask: EditorMask::None,
                enabled: true,
            });
        }
        if ui.button("+ Blur").clicked() {
            effects.push(EditorEffect {
                kind: EditorEffectKind::Blur { radius: 0.02 },
                mask: EditorMask::None,
                enabled: true,
            });
        }
        if ui.button("+ Smoke").clicked() {
            effects.push(EditorEffect {
                kind: EditorEffectKind::Smoke {
                    color: [0.6, 0.3, 0.9, 1.0],
                    decay: 0.97,
                    radius: 0.05,
                },
                mask: EditorMask::None,
                enabled: true,
            });
        }
        if ui.button("+ Shader").clicked() {
            effects.push(EditorEffect {
                kind: EditorEffectKind::Shader {
                    wgsl_path: None,
                    params: Vec::new(),
                },
                mask: EditorMask::None,
                enabled: true,
            });
        }
    });

    let mut move_up = None;
    let mut move_down = None;
    let mut remove = None;

    for (index, effect) in effects.iter_mut().enumerate() {
        ui.add_space(2.0);
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.checkbox(&mut effect.enabled, "");
                let label = format!("#{} {}", index + 1, effect_kind_label(&effect.kind));
                // Selectable, not just a label -- clicking it is how
                // `selected_effect` gets set, which drives the mask
                // gizmo on the preview (Circle/Gradient only -- see
                // `show_preview_content`), same pattern as the layer
                // list's own selectable rows driving `selected`.
                if ui
                    .selectable_label(*selected_effect == Some(index), label)
                    .on_hover_text("Select to edit this effect's mask on the preview")
                    .clicked()
                {
                    *selected_effect = Some(index);
                }
                ui.with_layout(
                    eframe::egui::Layout::right_to_left(eframe::egui::Align::Center),
                    |ui| {
                        if ui.small_button("remove").clicked() {
                            remove = Some(index);
                        }
                        if ui.small_button("down").clicked() {
                            move_down = Some(index);
                        }
                        if ui.small_button("up").clicked() {
                            move_up = Some(index);
                        }
                    },
                );
            });
            ui.add_space(2.0);
            show_effect_kind_panel(ui, &mut effect.kind);
            ui.add_space(2.0);
            show_mask_panel(ui, &mut effect.mask, selected_effect, index);
        });
    }

    if let Some(index) = remove {
        effects.remove(index);
        // Same bookkeeping as `show_layers_panel`'s own remove/reorder
        // handling for `selected`, one level down.
        *selected_effect = match *selected_effect {
            Some(selected) if selected == index => None,
            Some(selected) if selected > index => Some(selected - 1),
            other => other,
        };
    }
    if let Some(index) = move_up
        && index > 0
    {
        effects.swap(index, index - 1);
        *selected_effect = match *selected_effect {
            Some(selected) if selected == index => Some(index - 1),
            Some(selected) if selected == index - 1 => Some(index),
            other => other,
        };
    }
    if let Some(index) = move_down
        && index + 1 < effects.len()
    {
        effects.swap(index, index + 1);
        *selected_effect = match *selected_effect {
            Some(selected) if selected == index => Some(index + 1),
            Some(selected) if selected == index + 1 => Some(index),
            other => other,
        };
    }
}

fn effect_kind_label(kind: &EditorEffectKind) -> &'static str {
    match kind {
        EditorEffectKind::Vignette { .. } => "Vignette",
        EditorEffectKind::ColorAdjust { .. } => "Color adjust",
        EditorEffectKind::Blur { .. } => "Blur",
        EditorEffectKind::Smoke { .. } => "Smoke",
        EditorEffectKind::Shader { .. } => "Shader",
    }
}

fn show_effect_kind_panel(ui: &mut eframe::egui::Ui, kind: &mut EditorEffectKind) {
    match kind {
        EditorEffectKind::Vignette { strength, softness } => {
            labeled_slider(ui, "Strength:", strength, 0.0..=1.0);
            labeled_slider(ui, "Softness:", softness, 0.0..=1.0);
        }
        EditorEffectKind::ColorAdjust {
            brightness,
            contrast,
            saturation,
        } => {
            labeled_slider(ui, "Brightness:", brightness, -1.0..=1.0);
            labeled_slider(ui, "Contrast:", contrast, -1.0..=1.0);
            labeled_slider(ui, "Saturation:", saturation, -1.0..=1.0);
        }
        EditorEffectKind::Blur { radius } => {
            labeled_slider(ui, "Radius:", radius, 0.0..=0.1);
        }
        EditorEffectKind::Smoke {
            color,
            decay,
            radius,
        } => {
            ui.horizontal(|ui| {
                ui.label("Color:");
                ui.color_edit_button_rgba_unmultiplied(color);
            });
            labeled_slider(ui, "Decay:", decay, 0.8..=0.999).on_hover_text(
                "Fraction of the trail kept each frame -- closer to 1.0 fades slower.",
            );
            labeled_slider(ui, "Splat radius:", radius, 0.0..=0.3);
        }
        EditorEffectKind::Shader { wgsl_path, params } => {
            show_shader_effect_panel(ui, wgsl_path, params);
        }
    }
}

/// "Select" (bundled-library dropdown) + "Browse..." (arbitrary file) row
/// for a `Shader` effect's `wgsl_path`. Both just assign into `wgsl_path`
/// -- `show_shader_effect_panel` below can't tell, and doesn't need to,
/// which one a given path came from.
fn show_shader_path_picker(ui: &mut eframe::egui::Ui, wgsl_path: &mut Option<PathBuf>) {
    ui.horizontal(|ui| {
        ui.label("Shader:");

        // Scanned fresh every frame this row is shown -- a handful of
        // files, same "recompute, don't cache" call `library::scan` and
        // this panel's own param re-parse below already make.
        let entries = shaders_library::scan();
        let selected_entry = entries.iter().find(|e| Some(&e.path) == wgsl_path.as_ref());
        let selected_text = selected_entry
            .map(|e| e.name.clone())
            .or_else(|| {
                wgsl_path
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "Select...".to_string());
        eframe::egui::ComboBox::from_id_salt("shader-library-select")
            .selected_text(selected_text)
            .show_ui(ui, |ui| {
                if entries.is_empty() {
                    ui.weak("No bundled shaders installed");
                }
                for entry in &entries {
                    let is_selected = Some(entry) == selected_entry;
                    if ui.selectable_label(is_selected, &entry.name).clicked() {
                        *wgsl_path = Some(entry.path.clone());
                    }
                }
            });
        ui.label("or");
        if ui.button("Browse...").clicked()
            && let Some(chosen) = rfd::FileDialog::new()
                .add_filter("Files", &["wgsl"])
                .pick_file()
        {
            *wgsl_path = Some(chosen);
        }
    });

    let name = wgsl_path
        .as_ref()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| "not set".to_string());
    let response = ui.add(eframe::egui::Label::new(name).truncate());
    if let Some(p) = wgsl_path {
        response.on_hover_text(p.display().to_string());
    }
}

/// `Shader` effect's own panel -- a picker for its `.wgsl` asset, plus one
/// widget per parameter it declares. The picker is two ways to reach the
/// same `wgsl_path`, not two different things: "Select" lists whatever is
/// installed under `shaders_library::shaders_root()` (the bundled
/// library), "Browse..." is the same `rfd` file dialog every other asset
/// picker in this file uses -- see `path_picker`, not reused directly
/// here since it always renders its own "Browse..." button and this panel
/// needs a second widget in front of it. Re-reads and re-parses
/// `wgsl_path`'s file (via `project_format::parse_shader_params`) every
/// single frame this panel is shown, rather than caching the parsed
/// param list anywhere on `EditorEffectKind` -- the same "recompute, don't
/// cache" choice this file already makes for e.g. `cursor_local`, and it
/// buys two things here specifically: browsing to a new file needs no
/// separate change-detection hook (the newly parsed param count is just
/// compared against `params.len()` below, every frame, whichever path is
/// current), and hand-editing the `.wgsl` file in an external editor
/// while this panel is open is picked up live instead of needing a
/// reopen. A `.wgsl` file is at most a few KB of text, so re-parsing it
/// at UI framerate is not a real cost.
fn show_shader_effect_panel(
    ui: &mut eframe::egui::Ui,
    wgsl_path: &mut Option<PathBuf>,
    params: &mut Vec<f32>,
) {
    show_shader_path_picker(ui, wgsl_path);

    let Some(path) = wgsl_path.as_ref() else {
        ui.colored_label(
            eframe::egui::Color32::YELLOW,
            "Pick a .wgsl file to configure this effect.",
        );
        return;
    };
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(e) => {
            ui.colored_label(eframe::egui::Color32::RED, format!("Failed to read: {e}"));
            return;
        }
    };
    let specs = match project_format::parse_shader_params(&source) {
        Ok(specs) => specs,
        Err(e) => {
            ui.colored_label(eframe::egui::Color32::RED, e);
            return;
        }
    };

    // The file's own param list is the source of truth -- any mismatch
    // (a freshly picked file, or one edited since) resets to its
    // defaults rather than trying to carry old values over positionally,
    // which could silently attach the wrong value to the wrong param.
    if specs.len() != params.len() {
        *params = specs.iter().map(|spec| spec.default).collect();
    }

    for (spec, value) in specs.iter().zip(params.iter_mut()) {
        labeled_slider(
            ui,
            &format!("{}:", spec.label),
            value,
            spec.range.0..=spec.range.1,
        );
    }
}

/// Mask config for one effect -- type selector (pill buttons, same
/// pattern as `show_text_panel`'s source selector) plus whatever fields
/// that type needs. `None` has none; `Circle`/`Gradient` share a
/// transform/feather/invert; `Texture` is a picked picture instead of a
/// transform (see `EditorMask`'s doc comment).
///
/// `selected_effect`/`effect_index` are only here for the Paint button
/// below -- entering paint mode has to also select this effect
/// (`show_preview_content`'s brush interaction, like the Circle/Gradient
/// gizmos before it, only acts on `self.selected_effect`'s mask), or
/// clicking "Paint" here without separately clicking this effect's own
/// row above would silently paint nothing: the button's own label still
/// flips to "Stop painting" (that only reads this mask's own `painting`
/// field, not the selection), so there'd be no visible sign anything was
/// wrong.
fn show_mask_panel(
    ui: &mut eframe::egui::Ui,
    mask: &mut EditorMask,
    selected_effect: &mut Option<usize>,
    effect_index: usize,
) {
    ui.horizontal(|ui| {
        ui.label("Mask:");
        let is_none = matches!(mask, EditorMask::None);
        if ui.selectable_label(is_none, "None").clicked() && !is_none {
            *mask = EditorMask::None;
        }
        let is_circle = matches!(mask, EditorMask::Circle { .. });
        if ui.selectable_label(is_circle, "Circle").clicked() && !is_circle {
            *mask = EditorMask::Circle {
                transform: project_format::Transform2D::default(),
                feather: 0.2,
                invert: false,
            };
        }
        let is_gradient = matches!(mask, EditorMask::Gradient { .. });
        if ui.selectable_label(is_gradient, "Gradient").clicked() && !is_gradient {
            *mask = EditorMask::Gradient {
                transform: project_format::Transform2D::default(),
                feather: 0.2,
                invert: false,
            };
        }
        let is_texture = matches!(mask, EditorMask::Texture { .. });
        if ui.selectable_label(is_texture, "Texture").clicked() && !is_texture {
            *mask = EditorMask::Texture {
                path: None,
                invert: false,
                paint: None,
                painting: false,
                brush_radius: DEFAULT_BRUSH_RADIUS,
                brush_softness: DEFAULT_BRUSH_SOFTNESS,
                last_paint_pos: None,
            };
        }
    });
    match mask {
        EditorMask::None => {}
        EditorMask::Circle {
            transform,
            feather,
            invert,
        }
        | EditorMask::Gradient {
            transform,
            feather,
            invert,
        } => {
            show_transform_panel(ui, transform);
            labeled_slider(ui, "Feather:", feather, 0.0..=1.0);
            ui.checkbox(invert, "Invert");
        }
        EditorMask::Texture {
            path,
            invert,
            paint,
            painting,
            brush_radius,
            brush_softness,
            last_paint_pos,
        } => {
            path_picker(
                ui,
                "Mask picture (brightness = mask)",
                path,
                &["png", "jpg", "jpeg", "webp"],
            );
            ui.checkbox(invert, "Invert");
            ui.horizontal(|ui| {
                let label = if *painting {
                    "Stop painting"
                } else {
                    "Paint"
                };
                if ui.button(label).clicked() {
                    if !*painting && paint.is_none() {
                        // First time entering paint mode on this mask --
                        // normalize whatever it currently points at (or
                        // start blank, if nothing's picked yet) down to
                        // the fixed paint working resolution. See
                        // `PAINT_MASK_RESOLUTION`'s doc comment for why
                        // every painted mask lives at this size
                        // regardless of its origin picture's own size.
                        let buffer = path
                            .as_ref()
                            .and_then(|p| load_paint_buffer_from_file(p).ok())
                            .unwrap_or_else(|| {
                                vec![0u8; (PAINT_MASK_RESOLUTION * PAINT_MASK_RESOLUTION) as usize]
                            });
                        let temp_path = temp_paint_mask_path();
                        match save_paint_buffer_png(&buffer, &temp_path) {
                            Ok(()) => {
                                *path = Some(temp_path);
                                *paint = Some(buffer);
                            }
                            Err(e) => {
                                eprintln!(
                                    "editor: failed to write temp paint mask {temp_path:?}: {e}"
                                );
                            }
                        }
                    }
                    *painting = !*painting;
                    *last_paint_pos = None;
                    if *painting {
                        // See this function's own doc comment -- without
                        // this, painting would silently do nothing
                        // unless the user had *also* separately clicked
                        // this effect's row to select it.
                        *selected_effect = Some(effect_index);
                    }
                }
                if paint.is_some() {
                    ui.label("Drag on the preview to paint, hold Shift to erase.");
                }
            });
            if paint.is_some() {
                labeled_slider(ui, "Brush size:", brush_radius, 0.01..=0.3);
                labeled_slider(ui, "Brush softness:", brush_softness, 0.0..=1.0);
            }
        }
    }
}

/// Loads `path`'s picture, converts it to grayscale, and resamples it to
/// `PAINT_MASK_RESOLUTION`^2 -- the one-time normalization
/// `show_mask_panel`'s Texture arm runs the first time paint mode is
/// entered on a mask that started from a picked file, so painting
/// afterwards always works at the same fixed resolution regardless of
/// what the source picture's own size was.
fn load_paint_buffer_from_file(path: &Path) -> Result<Vec<u8>, String> {
    let image = image::open(path).map_err(|e| e.to_string())?.to_luma8();
    let resized = image::imageops::resize(
        &image,
        PAINT_MASK_RESOLUTION,
        PAINT_MASK_RESOLUTION,
        image::imageops::FilterType::Triangle,
    );
    Ok(resized.into_raw())
}

/// Encodes a single-channel paint buffer as a grayscale PNG -- shared by
/// `show_mask_panel`'s Texture arm (the temp file written when paint mode
/// starts) and `stage_effect` (refreshing that file from the live buffer
/// right before it gets staged into the saved project).
fn save_paint_buffer_png(buffer: &[u8], path: &Path) -> Result<(), String> {
    image::GrayImage::from_raw(PAINT_MASK_RESOLUTION, PAINT_MASK_RESOLUTION, buffer.to_vec())
        .ok_or_else(|| "paint buffer size doesn't match PAINT_MASK_RESOLUTION".to_string())?
        .save(path)
        .map_err(|e| e.to_string())
}

/// A fresh, never-before-used path under the system temp dir for a
/// from-scratch or just-normalized paint buffer to live at until the
/// project is saved -- same "absolute path outside the project dir"
/// convention every other not-yet-saved asset in this file already uses
/// (`path_picker`'s own picked files), just auto-generated instead of
/// user-chosen. Not cleaned up on its own -- consistent with this file
/// not otherwise tracking temp-file lifetimes (e.g. `unique_temp_dir` in
/// `render-server`'s own tests has the same non-guarantee) and out of
/// scope for M8.
fn temp_paint_mask_path() -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("wplinux-paint-mask-{}-{n}.png", std::process::id()))
}

/// Expands a single-channel paint buffer into 4-bytes/pixel RGBA (gray
/// replicated into R/G/B, full alpha) for GPU upload -- `write_texture`
/// (and every other texture upload in this codebase, via `player`'s own
/// `create_texture`) always deals in RGBA8, even though `mask_blend.wgsl`
/// only ever reads the R channel of a texture mask.
fn expand_gray_to_rgba(gray: &[u8]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(gray.len() * 4);
    for &g in gray {
        rgba.extend_from_slice(&[g, g, g, 255]);
    }
    rgba
}

/// Stamps a soft circular brush along the segment from `last` (or just a
/// single stamp at `to`, if this is a stroke's first point) to `to`, both
/// in normalized 0.0..=1.0 UV space -- interpolated at a fixed
/// screen-space step so a fast mouse movement doesn't leave gaps between
/// egui frames, the same technique M6's smoke splat trail already uses
/// for the same reason. Accumulates via `max()` (never additive --
/// passing the brush over the same spot twice must not push the mask
/// value past full strength), or subtracts when `erase` is set.
///
/// Not aspect-corrected -- a stamp is circular in the buffer's own
/// (always square, `PAINT_MASK_RESOLUTION`^2) pixel space, which
/// `mask_blend.wgsl` then samples in raw UV without any aspect
/// correction (unlike its `Circle` mask mode, which does correct). On a
/// non-square canvas this reads as a very slightly elliptical brush --
/// the same accepted simplification M6's smoke splat already makes for
/// the same reason (see `smoke.wgsl`'s doc comment).
fn stamp_paint_buffer(
    buffer: &mut [u8],
    resolution: u32,
    last: Option<(f32, f32)>,
    to: (f32, f32),
    radius: f32,
    softness: f32,
    erase: bool,
) {
    let from = last.unwrap_or(to);
    let dx = (to.0 - from.0) * resolution as f32;
    let dy = (to.1 - from.1) * resolution as f32;
    let segment_len = (dx * dx + dy * dy).sqrt();
    // A stamp roughly every 2 buffer pixels along the segment -- dense
    // enough that consecutive stamps overlap even for a small brush.
    let steps = (segment_len / 2.0).ceil().max(1.0) as u32;
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let point = (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t);
        stamp_once(buffer, resolution, point, radius, softness, erase);
    }
}

fn stamp_once(
    buffer: &mut [u8],
    resolution: u32,
    center_uv: (f32, f32),
    radius: f32,
    softness: f32,
    erase: bool,
) {
    let resolution_f = resolution as f32;
    let cx = center_uv.0 * resolution_f;
    let cy = center_uv.1 * resolution_f;
    let r_px = (radius * resolution_f).max(1.0);
    let inner = r_px * (1.0 - softness.clamp(0.0, 1.0));
    let min_x = (cx - r_px).floor().max(0.0) as u32;
    let max_x = (cx + r_px).ceil().min(resolution_f - 1.0) as u32;
    let min_y = (cy - r_px).floor().max(0.0) as u32;
    let max_y = (cy + r_px).ceil().min(resolution_f - 1.0) as u32;
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let px = x as f32 + 0.5 - cx;
            let py = y as f32 + 0.5 - cy;
            let dist = (px * px + py * py).sqrt();
            if dist > r_px {
                continue;
            }
            let strength = if dist <= inner {
                1.0
            } else {
                1.0 - (dist - inner) / (r_px - inner).max(0.0001)
            };
            let idx = (y * resolution + x) as usize;
            buffer[idx] = if erase {
                (buffer[idx] as f32 - strength * 255.0).max(0.0) as u8
            } else {
                buffer[idx].max((strength * 255.0) as u8)
            };
        }
    }
}

fn show_transform_panel(ui: &mut eframe::egui::Ui, transform: &mut project_format::Transform2D) {
    labeled_slider(ui, "X:", &mut transform.x, 0.0..=1.0);
    labeled_slider(ui, "Y:", &mut transform.y, 0.0..=1.0);
    labeled_slider(ui, "Scale:", &mut transform.scale, 0.05..=3.0);
    labeled_slider(ui, "Rotation (\u{b0}):", &mut transform.rotation, 0.0..=360.0);
}

fn show_text_panel(
    ui: &mut eframe::egui::Ui,
    x: &mut f32,
    y: &mut f32,
    font_size: &mut f32,
    color: &mut [f32; 4],
    source: &mut EditorTextSource,
    font: &mut EditorTextFont,
) {
    ui.horizontal(|ui| {
        ui.label("Source:");
        let is_literal = matches!(source, EditorTextSource::Literal(_));
        if ui.selectable_label(is_literal, "Text").clicked() && !is_literal {
            *source = EditorTextSource::Literal(String::new());
        }
        let is_clock = matches!(source, EditorTextSource::Clock { .. });
        if ui.selectable_label(is_clock, "Clock").clicked() && !is_clock {
            *source = EditorTextSource::Clock {
                format: "%H:%M".to_string(),
            };
        }
        let is_command = source.is_command();
        if ui.selectable_label(is_command, "Command").clicked() && !is_command {
            *source = EditorTextSource::Command {
                command: String::new(),
                interval_secs: 60,
            };
        }
    });
    match source {
        EditorTextSource::Literal(text) => {
            ui.horizontal(|ui| {
                ui.label("Text:");
                ui.text_edit_singleline(text);
            });
        }
        EditorTextSource::Clock { format } => {
            ui.horizontal(|ui| {
                ui.label("Format:").on_hover_text(
                    "strftime-style -- e.g. %H:%M for a 24h clock, %Y-%m-%d for a date.",
                );
                ui.text_edit_singleline(format);
            });
        }
        EditorTextSource::Command {
            command,
            interval_secs,
        } => {
            ui.horizontal(|ui| {
                ui.label("Command:").on_hover_text(
                    "Run through sh -c. Failure, timeout (5s) or empty \
                     output all just show \"NULL\" -- check render-server's \
                     own log for the real reason if that happens.",
                );
                ui.text_edit_singleline(command);
            });
            ui.horizontal(|ui| {
                ui.label("Interval (s):");
                ui.add(eframe::egui::DragValue::new(interval_secs).range(1..=86400));
            });
        }
    }
    labeled_slider(ui, "Font size:", font_size, 0.01..=0.3)
        .on_hover_text("Fraction of canvas height -- resolution-independent.");
    ui.horizontal(|ui| {
        ui.label("Color:");
        ui.color_edit_button_rgba_unmultiplied(color);
    });
    labeled_slider(ui, "X:", x, 0.0..=1.0);
    labeled_slider(ui, "Y:", y, 0.0..=1.0);

    ui.horizontal(|ui| {
        ui.label("Font:");
        let is_bundled = matches!(font, EditorTextFont::Bundled);
        if ui.selectable_label(is_bundled, "Bundled").clicked() && !is_bundled {
            *font = EditorTextFont::Bundled;
        }
        let is_custom = matches!(font, EditorTextFont::Custom { .. });
        if ui.selectable_label(is_custom, "Custom").clicked() && !is_custom {
            *font = EditorTextFont::Custom { path: None };
        }
    });
    if let EditorTextFont::Custom { path } = font {
        path_picker(ui, "Font file:", path, &["ttf", "otf"]);
    }
}

/// Thumbnails saved into the library -- deliberately smaller than the
/// live preview texture, since these only need to look decent shrunk down
/// to a picker button, never fill a resizable panel.
const THUMBNAIL_WIDTH: u32 = 320;

/// Renders one more offscreen frame from the already-live editor preview
/// scene and encodes it as `dest` (a PNG). Reuses `Preview`'s
/// `SceneRenderer`/loaded layers, which are already up to date with
/// `self.layers` by the time Save is clicked (`show_preview` runs earlier
/// in the same frame, in `Tab::Editor`'s left panel).
///
/// Mirrors the GPU-readback technique `render-server/src/renderer.rs`'s
/// `create_canvas`/`render_frame` already use -- duplicated here rather
/// than shared, since this is a one-shot call (once per save) with a
/// different lifetime than that reusable per-tick `Canvas`; hoisting a
/// shared `player::render_to_rgba`-style helper out of render-server is a
/// reasonable future cleanup once there's a second caller, not needed yet.
fn generate_thumbnail(preview: &Preview, dest: &Path) -> Result<(), String> {
    if preview.layers.is_empty() {
        return Err("no scene loaded".to_string());
    }
    let (natural_width, natural_height) = preview
        .layers
        .first()
        .map(LoadedLayer::size)
        .unwrap_or((16, 9));
    let width = THUMBNAIL_WIDTH.min(natural_width).max(1);
    let height =
        ((width as u64 * natural_height as u64) / natural_width.max(1) as u64).max(1) as u32;

    let texture = preview
        .renderer
        .device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some("editor-thumbnail"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: player::OFFSCREEN_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    // Rows in a copy-to-buffer destination must be padded to a multiple
    // of COPY_BYTES_PER_ROW_ALIGNMENT; strip the padding back out below.
    let unpadded_bytes_per_row = 4 * width;
    let padding = (wgpu::COPY_BYTES_PER_ROW_ALIGNMENT
        - unpadded_bytes_per_row % wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        % wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded_bytes_per_row = unpadded_bytes_per_row + padding;

    let readback_buffer = preview
        .renderer
        .device
        .create_buffer(&wgpu::BufferDescriptor {
            label: Some("editor-thumbnail-readback"),
            size: (padded_bytes_per_row * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

    let mut encoder = preview
        .renderer
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    preview.renderer.record_draw(
        &mut encoder,
        &view,
        &preview.layers,
        wgpu::Color::TRANSPARENT,
    );
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback_buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_bytes_per_row),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    preview.renderer.queue.submit(Some(encoder.finish()));

    let (tx, rx) = std::sync::mpsc::channel();
    readback_buffer.map_async(wgpu::MapMode::Read, .., move |result| {
        let _ = tx.send(result);
    });
    preview
        .renderer
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("device poll failed");
    rx.recv()
        .expect("map_async callback never fired")
        .expect("failed to map readback buffer");

    let mut pixels = Vec::with_capacity((unpadded_bytes_per_row * height) as usize);
    {
        let mapped = readback_buffer.get_mapped_range(..);
        for row in 0..height {
            let start = (row * padded_bytes_per_row) as usize;
            let end = start + unpadded_bytes_per_row as usize;
            pixels.extend_from_slice(&mapped[start..end]);
        }
    }
    readback_buffer.unmap();

    image::RgbaImage::from_raw(width, height, pixels)
        .ok_or_else(|| "bad thumbnail dimensions".to_string())?
        .save(dest)
        .map_err(|e| e.to_string())
}

/// Loads the app icon for the About window. Bundled into the binary via
/// `include_bytes!` (rather than read from an installed data path at
/// runtime, like `load_thumbnail_texture` does) since this asset always
/// ships with the binary and has no meaningful "missing" case to handle.
fn load_about_icon(ctx: &eframe::egui::Context) -> eframe::egui::TextureHandle {
    const ICON_BYTES: &[u8] = include_bytes!("../../../assets/wp_linux_editor_128.png");
    let rgba = image::load_from_memory(ICON_BYTES)
        .expect("bundled about-icon PNG should always decode")
        .into_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    let color_image = eframe::egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
    ctx.load_texture(
        "about-icon",
        color_image,
        eframe::egui::TextureOptions::default(),
    )
}

/// Loads a library entry's `preview.png` as an egui texture for the
/// Wallpapers tab's picker grid -- decode failures fall back to a 1x1
/// placeholder rather than erroring the whole tab out.
fn load_thumbnail_texture(ctx: &eframe::egui::Context, path: &Path) -> eframe::egui::TextureHandle {
    let rgba = image::open(path)
        .map(|img| img.into_rgba8())
        .unwrap_or_else(|_| image::RgbaImage::new(1, 1));
    let size = [rgba.width() as usize, rgba.height() as usize];
    let color_image = eframe::egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
    ctx.load_texture(
        path.display().to_string(),
        color_image,
        eframe::egui::TextureOptions::default(),
    )
}

/// A `Slider` that also responds to the mouse wheel while hovered, as a
/// deliberately small nudge -- one thousandth of the value per scroll
/// input (one whole unit for integer sliders, e.g. FPS), never a jump
/// proportional to the slider's range or on-screen width. This is
/// "fine-tuning" only, for the last bit of precision a ~180px-wide
/// slider spanning a wide range can't give by mouse movement alone;
/// bigger moves are still a drag.
///
/// The value display is a separate `DragValue` placed after the slider
/// rather than the slider's own built-in one (`Slider::show_value`,
/// disabled here) specifically so hovering *it* -- as opposed to the
/// slider track/handle itself -- is unaffected by any of this and stays
/// a plain drag/type-to-edit field.
///
/// Consumes the scroll delta it acts on (see the `smooth_scroll_delta`
/// reset below) so it doesn't *also* get read by an enclosing
/// `ScrollArea` -- see `show_layer_settings_panel`'s `ScrollArea`, which
/// relies on this to let wheel-over-a-slider and wheel-over-the-
/// settings-background do two different things instead of both firing
/// at once.
fn scroll_slider<Num: eframe::egui::emath::Numeric>(
    ui: &mut eframe::egui::Ui,
    value: &mut Num,
    range: std::ops::RangeInclusive<Num>,
) -> eframe::egui::Response {
    let slider_response = ui.add(eframe::egui::Slider::new(value, range.clone()).show_value(false));
    if slider_response.hovered() {
        let scroll_y = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll_y != 0.0 {
            let step = if Num::INTEGRAL { 1.0 } else { 0.001 };
            let (min, max) = (range.start().to_f64(), range.end().to_f64());
            let new_value = (value.to_f64() + step * scroll_y.signum() as f64).clamp(min, max);
            *value = Num::from_f64(new_value);
            ui.input_mut(|i| i.smooth_scroll_delta.y = 0.0);
        }
    }
    ui.add(eframe::egui::DragValue::new(value));
    slider_response
}

/// `ui.label(label)` followed by [`scroll_slider`] on the same row -- the
/// shape a labeled slider takes at every one of this file's ~20 call
/// sites. Returns the label's own `Response`, not the slider's, so a
/// caller that documents a non-obvious unit or behavior can still chain
/// `.on_hover_text(...)` exactly as if it had written `ui.label` by hand.
fn labeled_slider<Num: eframe::egui::emath::Numeric>(
    ui: &mut eframe::egui::Ui,
    label: &str,
    value: &mut Num,
    range: std::ops::RangeInclusive<Num>,
) -> eframe::egui::Response {
    let mut label_response = None;
    ui.horizontal(|ui| {
        label_response = Some(ui.label(label));
        scroll_slider(ui, value, range);
    });
    label_response.expect("the horizontal closure above always runs exactly once")
}

/// Shows `label`, a Browse button, and the chosen file's name -- full
/// absolute paths (what's actually stored) can easily run past 500px and
/// are why layer rows used to force the whole window wider than it
/// should be. The button is placed before the path text specifically so
/// the path's `.truncate()` sees accurate remaining space (it eats
/// whatever's left in the row instead of its own unbounded natural
/// width) and elides with "..." instead of pushing the row wider; the
/// full path is still available as a tooltip on hover.
fn path_picker(
    ui: &mut eframe::egui::Ui,
    label: &str,
    path: &mut Option<PathBuf>,
    filter: &[&str],
) {
    ui.horizontal(|ui| {
        ui.label(label);
        if ui.button("Browse...").clicked()
            && let Some(chosen) = rfd::FileDialog::new()
                .add_filter("Files", filter)
                .pick_file()
        {
            *path = Some(chosen);
        }
        let name = path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(str::to_string)
            .unwrap_or_else(|| "not set".to_string());
        let response = ui.add(eframe::egui::Label::new(name).truncate());
        if let Some(p) = path {
            response.on_hover_text(p.display().to_string());
        }
    });
}

/// Loads an existing project folder back into the editor's layer list,
/// resolving each layer's relative asset paths to absolute ones so
/// `path_picker` has something to display.
fn open_project(project_dir: &Path) -> Result<(Vec<EditorLayer>, u32, String), String> {
    let (project, project_dir) =
        project_format::Project::load(project_dir).map_err(|e| e.to_string())?;

    let layers = project
        .layers
        .into_iter()
        .map(|layer| match layer {
            project_format::Layer::Image { path, effects } => EditorLayer::Image {
                path: Some(project_dir.join(path)),
                effects: editor_effects_from_project(effects, &project_dir),
            },
            project_format::Layer::Xray {
                base,
                overlay,
                radius,
                effects,
            } => EditorLayer::Xray {
                base: Some(project_dir.join(base)),
                overlay: Some(project_dir.join(overlay)),
                radius,
                effects: editor_effects_from_project(effects, &project_dir),
            },
            project_format::Layer::Gif { path, effects } => EditorLayer::Gif {
                path: Some(project_dir.join(path)),
                effects: editor_effects_from_project(effects, &project_dir),
            },
            project_format::Layer::Parallax {
                path,
                strength,
                smoothing,
                effects,
            } => EditorLayer::Parallax {
                path: Some(project_dir.join(path)),
                strength,
                smoothing,
                effects: editor_effects_from_project(effects, &project_dir),
            },
            project_format::Layer::Text {
                x,
                y,
                font_size,
                color,
                source,
                font,
            } => EditorLayer::Text {
                x,
                y,
                font_size,
                color,
                source: EditorTextSource::from_project(source),
                font: match font {
                    project_format::TextFont::Bundled => EditorTextFont::Bundled,
                    project_format::TextFont::Custom { path } => EditorTextFont::Custom {
                        path: Some(project_dir.join(path)),
                    },
                },
            },
            project_format::Layer::Adjustment { effects } => EditorLayer::Adjustment {
                effects: editor_effects_from_project(effects, &project_dir),
            },
        })
        .collect();

    Ok((layers, project.fps, project.description))
}

/// Saves in two passes -- stage every source asset into a scratch
/// directory first, then move the staged files into their final
/// `layer_N_*` names only once every source has been read.
///
/// A single pass that copies straight into final names isn't safe:
/// those names are derived from each layer's *current* position in
/// `layers`, so reordering layers and saving back into the *same*
/// project directory can make one layer's final name equal another,
/// not-yet-processed layer's source path (an existing file inside this
/// same project). Copying layer-by-layer would then overwrite that
/// source with unrelated content before it's ever read -- silently
/// losing that layer's picture. Staging first means every source is
/// safely read into the scratch directory before any final name is
/// touched, so the order layers happen to be processed in can't matter.
fn save_project(
    project_dir: &Path,
    layers: &[EditorLayer],
    fps: u32,
    name: &str,
    description: &str,
) -> Result<(), String> {
    std::fs::create_dir_all(project_dir).map_err(|e| e.to_string())?;

    let staging_dir = project_dir.join(".wplinux-staging");
    std::fs::create_dir_all(&staging_dir).map_err(|e| e.to_string())?;

    let result = (|| {
        let mut saved_layers = Vec::with_capacity(layers.len());
        let mut staged_names = Vec::new();
        for (index, layer) in layers.iter().enumerate() {
            let mut stage = |source: &Path, stem: &str| -> Result<String, String> {
                let file_name = stage_asset(&staging_dir, source, stem)?;
                staged_names.push(file_name.clone());
                Ok(file_name)
            };

            let saved = match layer {
                EditorLayer::Image { path, effects } => {
                    let path = path.as_ref().expect("save is disabled until complete");
                    let file_name = stage(path, &format!("layer_{index}_image"))?;
                    let effects = effects
                        .iter()
                        .enumerate()
                        .map(|(effect_index, effect)| {
                            stage_effect(&mut stage, index, effect_index, effect)
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    project_format::Layer::Image {
                        path: file_name,
                        effects,
                    }
                }
                EditorLayer::Xray {
                    base,
                    overlay,
                    radius,
                    effects,
                } => {
                    let base = base.as_ref().expect("save is disabled until complete");
                    let overlay = overlay.as_ref().expect("save is disabled until complete");
                    let base_name = stage(base, &format!("layer_{index}_xray_base"))?;
                    let overlay_name = stage(overlay, &format!("layer_{index}_xray_overlay"))?;
                    let effects = effects
                        .iter()
                        .enumerate()
                        .map(|(effect_index, effect)| {
                            stage_effect(&mut stage, index, effect_index, effect)
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    project_format::Layer::Xray {
                        base: base_name,
                        overlay: overlay_name,
                        radius: *radius,
                        effects,
                    }
                }
                EditorLayer::Gif { path, effects } => {
                    let path = path.as_ref().expect("save is disabled until complete");
                    let file_name = stage(path, &format!("layer_{index}_anim"))?;
                    let effects = effects
                        .iter()
                        .enumerate()
                        .map(|(effect_index, effect)| {
                            stage_effect(&mut stage, index, effect_index, effect)
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    project_format::Layer::Gif {
                        path: file_name,
                        effects,
                    }
                }
                EditorLayer::Parallax {
                    path,
                    strength,
                    smoothing,
                    effects,
                } => {
                    let path = path.as_ref().expect("save is disabled until complete");
                    let file_name = stage(path, &format!("layer_{index}_parallax"))?;
                    let effects = effects
                        .iter()
                        .enumerate()
                        .map(|(effect_index, effect)| {
                            stage_effect(&mut stage, index, effect_index, effect)
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    project_format::Layer::Parallax {
                        path: file_name,
                        strength: *strength,
                        smoothing: *smoothing,
                        effects,
                    }
                }
                EditorLayer::Text {
                    x,
                    y,
                    font_size,
                    color,
                    source,
                    font,
                } => {
                    let font = match font {
                        EditorTextFont::Bundled => project_format::TextFont::Bundled,
                        EditorTextFont::Custom { path } => {
                            let path = path.as_ref().expect("save is disabled until complete");
                            let file_name = stage(path, &format!("layer_{index}_font"))?;
                            project_format::TextFont::Custom { path: file_name }
                        }
                    };
                    project_format::Layer::Text {
                        x: *x,
                        y: *y,
                        font_size: *font_size,
                        color: *color,
                        source: source.to_project(),
                        font,
                    }
                }
                // No layer-level asset to stage -- just its own effects,
                // same as every other layer kind's `effects` field.
                EditorLayer::Adjustment { effects } => {
                    let effects = effects
                        .iter()
                        .enumerate()
                        .map(|(effect_index, effect)| {
                            stage_effect(&mut stage, index, effect_index, effect)
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    project_format::Layer::Adjustment { effects }
                }
            };
            saved_layers.push(saved);
        }

        // Every source has been read into the staging directory now --
        // safe to promote the staged files into their final names, even
        // where a reorder made one layer's final name equal another
        // layer's original source path.
        for staged_name in &staged_names {
            std::fs::rename(staging_dir.join(staged_name), project_dir.join(staged_name))
                .map_err(|e| e.to_string())?;
        }

        let project = project_format::Project {
            name: name.to_string(),
            description: description.to_string(),
            layers: saved_layers,
            fps,
        };
        project.save(project_dir).map_err(|e| e.to_string())
    })();

    let _ = std::fs::remove_dir_all(&staging_dir);
    result
}

/// Converts one effect to its saved form, staging its mask's picture
/// (if it has one -- only `Mask::Texture` does) through `stage` the
/// same way its owning layer's own picture(s) already are. `stage` is
/// generic over the closure type rather than a `Box<dyn FnMut>` since
/// each of `save_project`'s four call sites constructs its own
/// distinctly-typed closure capturing that layer's `staged_names`/
/// `staging_dir` -- `impl Trait` here just monomorphizes per call site,
/// same as passing any other closure through a helper.
fn stage_effect(
    stage: &mut impl FnMut(&Path, &str) -> Result<String, String>,
    layer_index: usize,
    effect_index: usize,
    effect: &EditorEffect,
) -> Result<project_format::Effect, String> {
    let mask = match &effect.mask {
        EditorMask::None => project_format::Mask::None,
        EditorMask::Circle {
            transform,
            feather,
            invert,
        } => project_format::Mask::Circle {
            transform: *transform,
            feather: *feather,
            invert: *invert,
        },
        EditorMask::Gradient {
            transform,
            feather,
            invert,
        } => project_format::Mask::Gradient {
            transform: *transform,
            feather: *feather,
            invert: *invert,
        },
        EditorMask::Texture {
            path,
            invert,
            paint,
            ..
        } => {
            let path = path.as_ref().expect("save is disabled until complete");
            if let Some(buffer) = paint {
                // The on-disk file only reflects the buffer's content as
                // of whenever paint mode was last (re)entered --
                // `LoadedLayer::write_mask_paint` pushes every stroke
                // straight to the GPU and never touches disk (see
                // `EditorMask::Texture::paint`'s doc comment), so it has
                // to be refreshed from the live buffer right before
                // staging picks it up, or a save right after painting
                // would silently persist stale (or blank) content.
                save_paint_buffer_png(buffer, path)?;
            }
            let file_name = stage(
                path,
                &format!("layer_{layer_index}_effect_{effect_index}_mask"),
            )?;
            project_format::Mask::Texture {
                path: file_name,
                invert: *invert,
            }
        }
    };
    let kind = match &effect.kind {
        EditorEffectKind::Shader { wgsl_path, params } => {
            let wgsl_path = wgsl_path.as_ref().expect("save is disabled until complete");
            let file_name = stage(
                wgsl_path,
                &format!("layer_{layer_index}_effect_{effect_index}_shader"),
            )?;
            project_format::EffectKind::Shader {
                wgsl_path: file_name,
                params: params.clone(),
            }
        }
        other => other.to_project(),
    };
    Ok(project_format::Effect {
        kind,
        mask,
        enabled: effect.enabled,
    })
}

/// Copies `source` into `staging_dir` under `stem` plus `source`'s
/// extension, returning the file name (shared between the staging
/// directory and, once `save_project` promotes it, the project
/// directory) that got written.
fn stage_asset(staging_dir: &Path, source: &Path, stem: &str) -> Result<String, String> {
    let extension = source.extension().and_then(|e| e.to_str()).unwrap_or("png");
    let file_name = format!("{stem}.{extension}");
    std::fs::copy(source, staging_dir.join(&file_name)).map_err(|e| e.to_string())?;
    Ok(file_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_temp_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wplinux-editor-test-{}-{}-{n}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A single stamp at a buffer's center should max out right at that
    /// pixel and leave a far corner (well outside the brush radius)
    /// completely untouched -- the same "does it land where it should,
    /// and stay contained" shape of check every GPU effect test in
    /// `render-server` runs, just against the CPU-side buffer directly
    /// since this logic never touches the GPU.
    #[test]
    fn stamp_once_paints_a_solid_circle_and_leaves_the_far_corner_alone() {
        let resolution = 64u32;
        let mut buffer = vec![0u8; (resolution * resolution) as usize];
        stamp_once(&mut buffer, resolution, (0.5, 0.5), 0.1, 0.5, false);

        let center = buffer[(32 * resolution + 32) as usize];
        let corner = buffer[0];

        assert_eq!(center, 255, "the stamp's own center should be full strength");
        assert_eq!(corner, 0, "a far corner outside the brush radius should stay untouched");
    }

    /// Painting twice over the same spot must not push the value past
    /// full strength (accumulation is `max()`, not additive -- see
    /// `stamp_paint_buffer`'s doc comment) -- and erasing afterwards
    /// should bring it back down, not go negative/wrap.
    #[test]
    fn repeated_stamps_saturate_instead_of_overflowing_and_erase_reduces_it() {
        let resolution = 64u32;
        let mut buffer = vec![0u8; (resolution * resolution) as usize];
        stamp_once(&mut buffer, resolution, (0.5, 0.5), 0.1, 0.0, false);
        stamp_once(&mut buffer, resolution, (0.5, 0.5), 0.1, 0.0, false);
        let center_index = (32 * resolution + 32) as usize;
        assert_eq!(buffer[center_index], 255);

        stamp_once(&mut buffer, resolution, (0.5, 0.5), 0.1, 0.0, true);
        assert!(
            buffer[center_index] < 255,
            "erasing should reduce a previously fully-painted pixel, got {}",
            buffer[center_index]
        );
    }

    /// `stamp_paint_buffer` interpolates between `last` and `to` rather
    /// than only stamping the endpoint -- otherwise a fast drag would
    /// leave gaps instead of a continuous stroke. A long segment should
    /// paint somewhere in its own middle, not just at its two ends.
    #[test]
    fn stamp_paint_buffer_interpolates_along_a_fast_drag() {
        let resolution = 64u32;
        let mut buffer = vec![0u8; (resolution * resolution) as usize];
        stamp_paint_buffer(
            &mut buffer,
            resolution,
            Some((0.1, 0.5)),
            (0.9, 0.5),
            0.05,
            0.0,
            false,
        );
        let midpoint = buffer[(32 * resolution + 32) as usize];
        assert!(
            midpoint > 0,
            "a stroke from one edge to the other should paint through its own midpoint too"
        );
    }

    #[test]
    fn expand_gray_to_rgba_replicates_into_rgb_with_full_alpha() {
        let gray = [0u8, 128, 255];
        let rgba = expand_gray_to_rgba(&gray);
        assert_eq!(
            rgba,
            vec![0, 0, 0, 255, 128, 128, 128, 255, 255, 255, 255, 255]
        );
    }

    /// Round-trips a painted buffer through `save_paint_buffer_png` and
    /// `load_paint_buffer_from_file` -- already at `PAINT_MASK_RESOLUTION`,
    /// so the resample step is an identity and this mainly proves the
    /// PNG encode/decode + grayscale conversion don't lose or shift data.
    #[test]
    fn paint_buffer_png_round_trips() {
        let mut buffer = vec![0u8; (PAINT_MASK_RESOLUTION * PAINT_MASK_RESOLUTION) as usize];
        stamp_once(
            &mut buffer,
            PAINT_MASK_RESOLUTION,
            (0.5, 0.5),
            0.1,
            0.0,
            false,
        );

        let dir = unique_temp_dir();
        let path = dir.join("mask.png");
        save_paint_buffer_png(&buffer, &path).expect("save should succeed");
        let loaded = load_paint_buffer_from_file(&path).expect("load should succeed");

        assert_eq!(loaded.len(), buffer.len());
        let center_index =
            ((PAINT_MASK_RESOLUTION / 2) * PAINT_MASK_RESOLUTION + PAINT_MASK_RESOLUTION / 2)
                as usize;
        assert_eq!(loaded[center_index], 255);
    }

    /// Reproduces the bug report: open a 3-layer project (as `open_project`
    /// would leave it -- each layer's `path` already pointing inside
    /// `project_dir`, all sharing the same extension so reordering makes
    /// their final names collide with each other's *original* names
    /// too), reorder the layers, and save back into the *same* directory.
    /// A single-pass copy-straight-into-final-name save corrupts this:
    /// the layer that lands on index 0 gets copied into `layer_0_image.*`
    /// before the layer that used to *be* index 0 (and still has
    /// `layer_0_image.*` as its source path) has been read, silently
    /// replacing its picture with the wrong one -- and the next step
    /// reads that already-corrupted file, cascading the corruption
    /// through the rest of the layers.
    #[test]
    fn save_survives_reorder_into_same_directory() {
        let project_dir = unique_temp_dir();

        // Three distinct "pictures" at their `open_project`-assigned
        // paths. Plain marker bytes are enough -- the save path never
        // decodes them, only copies bytes.
        let originals: [&[u8]; 3] = [b"AAAA", b"BBBB", b"CCCC"];
        for (index, bytes) in originals.iter().enumerate() {
            std::fs::write(project_dir.join(format!("layer_{index}_image.png")), bytes).unwrap();
        }

        // Rotate: what was layer 2 moves to index 0, layer 0 moves to
        // index 1, layer 1 moves to index 2.
        let reordered = vec![
            EditorLayer::Image {
                path: Some(project_dir.join("layer_2_image.png")),
                effects: Vec::new(),
            },
            EditorLayer::Image {
                path: Some(project_dir.join("layer_0_image.png")),
                effects: Vec::new(),
            },
            EditorLayer::Image {
                path: Some(project_dir.join("layer_1_image.png")),
                effects: Vec::new(),
            },
        ];

        save_project(&project_dir, &reordered, 30, "Test Project", "")
            .expect("save_project failed");

        // Each index's *new* final name should hold whatever that layer's
        // source actually was, not whatever the earlier single-pass copy
        // happened to leave behind.
        assert_eq!(
            std::fs::read(project_dir.join("layer_0_image.png")).unwrap(),
            b"CCCC"
        );
        assert_eq!(
            std::fs::read(project_dir.join("layer_1_image.png")).unwrap(),
            b"AAAA"
        );
        assert_eq!(
            std::fs::read(project_dir.join("layer_2_image.png")).unwrap(),
            b"BBBB"
        );

        std::fs::remove_dir_all(&project_dir).ok();
    }

    /// M9: an `Adjustment` layer has no asset of its own to stage, just
    /// its own effects -- `save_project`/`open_project` should round-trip
    /// it exactly like any other layer's `effects` list, and
    /// `build_signature` should give it a real structural signature (not
    /// panic, not silently drop it) so `ensure_scene` can tell when its
    /// effects change shape.
    #[test]
    fn adjustment_layer_round_trips_through_save_and_open() {
        let project_dir = unique_temp_dir();
        let layers = vec![
            EditorLayer::Image {
                path: {
                    let path = project_dir.join("source.png");
                    image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]))
                        .save(&path)
                        .unwrap();
                    Some(path)
                },
                effects: Vec::new(),
            },
            EditorLayer::Adjustment {
                effects: vec![EditorEffect {
                    kind: EditorEffectKind::Vignette {
                        strength: 0.5,
                        softness: 0.2,
                    },
                    mask: EditorMask::None,
                    enabled: true,
                }],
            },
        ];

        assert_eq!(build_signature(&layers).len(), 2);

        save_project(&project_dir, &layers, 30, "Test Project", "").expect("save_project failed");
        let (reopened, _fps, _description) =
            open_project(&project_dir).expect("open_project failed");

        assert_eq!(reopened.len(), 2);
        match &reopened[1] {
            EditorLayer::Adjustment { effects } => {
                assert_eq!(effects.len(), 1);
                assert!(matches!(
                    effects[0].kind,
                    EditorEffectKind::Vignette { strength, .. } if strength == 0.5
                ));
            }
            other => panic!("expected EditorLayer::Adjustment, got a different variant: {}", other.label()),
        }

        std::fs::remove_dir_all(&project_dir).ok();
    }
}
