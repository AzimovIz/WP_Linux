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

mod library;
mod monitors_config;
mod push;
mod trust_store;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
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

fn main() -> eframe::Result {
    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([1200.0, 600.0]),
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

/// Number of tiles per row in the "Wallpapers" tab's wallpaper grid.
const WALLPAPER_GRID_COLUMNS: usize = 4;
/// Size of each tile's preview image (name label sits below it, outside
/// this rect).
const WALLPAPER_TILE_SIZE: eframe::egui::Vec2 = eframe::egui::Vec2::new(220.0, 130.0);
/// Size of each tile's always-visible Edit/Apply icon
/// buttons, pinned to the top-right corner of the thumbnail.
const WALLPAPER_ICON_SIZE: f32 = 24.0;
const WALLPAPER_ICON_MARGIN: f32 = 6.0;
const WALLPAPER_ICON_GAP: f32 = 4.0;

enum EditorLayer {
    Image {
        path: Option<PathBuf>,
    },
    Xray {
        base: Option<PathBuf>,
        overlay: Option<PathBuf>,
        radius: f32,
    },
    Gif {
        path: Option<PathBuf>,
    },
    Parallax {
        path: Option<PathBuf>,
        strength: f32,
        smoothing: f32,
    },
    Text {
        // Normalized (0.0..=1.0) canvas fractions -- see
        // `project_format::Layer::Text`.
        x: f32,
        y: f32,
        font_size: f32,
        color: [f32; 4],
        source: EditorTextSource,
    },
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

impl EditorLayer {
    fn label(&self) -> &'static str {
        match self {
            EditorLayer::Image { .. } => "Image",
            EditorLayer::Xray { .. } => "Xray",
            EditorLayer::Gif { .. } => "Gif",
            EditorLayer::Parallax { .. } => "Parallax",
            EditorLayer::Text { .. } => "Text",
        }
    }

    fn is_complete(&self) -> bool {
        match self {
            EditorLayer::Image { path } => path.is_some(),
            EditorLayer::Xray { base, overlay, .. } => base.is_some() && overlay.is_some(),
            EditorLayer::Gif { path } => path.is_some(),
            EditorLayer::Parallax { path, .. } => path.is_some(),
            // No external asset -- always ready to preview/save, even
            // with an empty string.
            EditorLayer::Text { .. } => true,
        }
    }
}

/// What a `Preview` was last built from -- resolved asset paths only, not
/// radius (see `Preview::sync_radii`) or fps (gif timing is derived from
/// each gif's own per-frame delays, never from the project's target fps --
/// see project-format's `Project::fps` doc comment). Compared each frame
/// so a rebuild (re-decoding images, re-uploading GPU textures) only
/// happens on an actual structural change, not every frame of, say, a
/// slider drag.
#[derive(Clone, PartialEq, Eq)]
enum LayerSignature {
    Image(PathBuf),
    Xray(PathBuf, PathBuf),
    Gif(PathBuf),
    Parallax(PathBuf),
    // No payload -- unlike every other layer, nothing about a Text
    // layer ever requires a GPU reload; every property (including the
    // text itself) is updatable live via `SceneRenderer::set_text_params`
    // (see `Preview::sync_live_params`), so signature equality alone
    // (Text always equals Text) is enough to skip `ensure_scene`'s
    // reload path for it.
    Text,
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
                EditorLayer::Xray { radius, .. } => loaded.set_xray_radius(*radius * scale),
                EditorLayer::Parallax {
                    strength,
                    smoothing,
                    ..
                } => {
                    loaded.set_parallax_params(*strength, *smoothing);
                }
                EditorLayer::Text {
                    x,
                    y,
                    font_size,
                    color,
                    source,
                } => {
                    // Always trusted: the editor is the self-authoring
                    // context (the user typed the command themselves),
                    // matching the auto-trust-on-save policy in the Save
                    // button handler.
                    loaded.set_text_params(&source.to_project(), *x, *y, *font_size, *color, true);
                }
                EditorLayer::Image { .. } | EditorLayer::Gif { .. } => {}
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
            // `Path::new("")` as the base works out.
            EditorLayer::Image { path } => project_format::Layer::Image {
                path: path
                    .as_ref()
                    .expect("checked complete above")
                    .display()
                    .to_string(),
            },
            EditorLayer::Xray {
                base,
                overlay,
                radius,
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
            },
            EditorLayer::Gif { path } => project_format::Layer::Gif {
                path: path
                    .as_ref()
                    .expect("checked complete above")
                    .display()
                    .to_string(),
            },
            EditorLayer::Parallax {
                path,
                strength,
                smoothing,
            } => project_format::Layer::Parallax {
                path: path
                    .as_ref()
                    .expect("checked complete above")
                    .display()
                    .to_string(),
                strength: *strength,
                smoothing: *smoothing,
            },
            EditorLayer::Text {
                x,
                y,
                font_size,
                color,
                source,
            } => project_format::Layer::Text {
                x: *x,
                y: *y,
                font_size: *font_size,
                color: *color,
                source: source.to_project(),
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
            EditorLayer::Image { path } => {
                LayerSignature::Image(path.clone().expect("checked complete by caller"))
            }
            EditorLayer::Xray { base, overlay, .. } => LayerSignature::Xray(
                base.clone().expect("checked complete by caller"),
                overlay.clone().expect("checked complete by caller"),
            ),
            EditorLayer::Gif { path } => {
                LayerSignature::Gif(path.clone().expect("checked complete by caller"))
            }
            EditorLayer::Parallax { path, .. } => {
                LayerSignature::Parallax(path.clone().expect("checked complete by caller"))
            }
            EditorLayer::Text { .. } => LayerSignature::Text,
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

    // -- "Wallpapers" tab state --
    monitors: Vec<MonitorInfo>,
    /// Mirrors `monitors_config`'s on-disk file -- monitor id -> project dir.
    assignments: HashMap<String, PathBuf>,
    library: Vec<library::LibraryEntry>,
    /// Lazily-loaded preview textures, keyed by each entry's `preview_path`.
    thumbnails: HashMap<PathBuf, eframe::egui::TextureHandle>,
    pusher: Box<dyn ProjectPusher>,
    /// Project dir of the wallpaper whose "Apply" overlay (monitor
    /// picker) is currently open, if any.
    apply_overlay: Option<PathBuf>,

    // -- "Editor" tab state --
    layers: Vec<EditorLayer>,
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
            monitors: Vec::new(),
            assignments: monitors_config::load(),
            library: library::scan(),
            thumbnails: HashMap::new(),
            pusher: Box::new(TcpPusher),
            apply_overlay: None,
            layers: Vec::new(),
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
            });
            ui.add_space(4.0);
        });

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
                eframe::egui::Panel::left("preview_panel")
                    .resizable(true)
                    .default_size(760.0)
                    .size_range(320.0..=980.0)
                    .show(ui, |ui| {
                        self.show_preview(ui, render_state.as_ref());
                    });

                eframe::egui::CentralPanel::default().show(ui, |ui| {
                    self.show_editor_tab(ui);
                });
            }
        }
    }
}

impl EditorApp {
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
        });
        ui.add_space(8.0);

        if self.library.is_empty() {
            ui.label("No wallpapers in the library yet -- save one from the Editor tab.");
        }

        eframe::egui::ScrollArea::vertical().show(ui, |ui| {
            let entries = self.library.clone();
            for row in entries.chunks(WALLPAPER_GRID_COLUMNS) {
                ui.horizontal(|ui| {
                    for entry in row {
                        self.show_wallpaper_tile(ui, entry);
                    }
                });
                ui.add_space(12.0);
            }
        });

        self.show_apply_overlay(ui.ctx());
    }

    /// One tile in the "Wallpapers" grid: a fixed-size preview (or a plain
    /// placeholder if this entry has no `preview.png` yet) with
    /// always-visible Edit/Apply icon buttons pinned to its
    /// top-right corner.
    fn show_wallpaper_tile(&mut self, ui: &mut eframe::egui::Ui, entry: &library::LibraryEntry) {
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
            ui.set_width(WALLPAPER_TILE_SIZE.x);
            let (rect, response) =
                ui.allocate_exact_size(WALLPAPER_TILE_SIZE, eframe::egui::Sense::hover());

            if let Some(preview_path) = &entry.preview_path {
                let texture = self
                    .thumbnails
                    .entry(preview_path.clone())
                    .or_insert_with(|| load_thumbnail_texture(ui.ctx(), preview_path));
                ui.put(
                    rect,
                    eframe::egui::Image::from_texture((texture.id(), WALLPAPER_TILE_SIZE)),
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

    /// Resets the editor tab back to a blank, unsaved project -- the same
    /// state `EditorApp::default()` starts in, so "New" undoes whatever
    /// `open_library_entry`/layer edits did without needing to relaunch
    /// the app. `current_project_id` going back to `None` means the next
    /// Save writes a fresh library entry rather than overwriting whatever
    /// was open before.
    fn new_project(&mut self) {
        self.layers = Vec::new();
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

    fn show_editor_tab(&mut self, ui: &mut eframe::egui::Ui) {
        // New/Open/Save first -- the actions that reset, load, or persist
        // everything below them, so they lead rather than trail the form.
        ui.horizontal(|ui| {
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

        ui.add_space(8.0);

        ui.horizontal(|ui| {
            ui.label("Name:");
            ui.text_edit_singleline(&mut self.current_project_name);
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

        ui.add_space(2.0);

        ui.horizontal(|ui| {
                ui.label("Target FPS (animated/cursor layers only):")
                    .on_hover_text("Used by render-server once this project is loaded there -- doesn't affect the preview on the left, which always redraws at a fixed rate.");
                scroll_slider(ui, &mut self.fps, 1..=60);
            });

        ui.add_space(8.0);
        ui.label("Layers are drawn bottom to top -- the first one in the list is furthest back.");
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            if ui.button("+ Image").clicked() {
                self.layers.push(EditorLayer::Image { path: None });
            }
            if ui.button("+ Xray").clicked() {
                self.layers.push(EditorLayer::Xray {
                    base: None,
                    overlay: None,
                    radius: 200.0,
                });
            }
            if ui.button("+ Gif").clicked() {
                self.layers.push(EditorLayer::Gif { path: None });
            }
            if ui.button("+ Parallax").clicked() {
                self.layers.push(EditorLayer::Parallax {
                    path: None,
                    strength: 0.05,
                    smoothing: 0.15,
                });
            }
            if ui.button("+ Text").clicked() {
                self.layers.push(EditorLayer::Text {
                    x: 0.5,
                    y: 0.5,
                    font_size: 0.05,
                    color: [1.0, 1.0, 1.0, 1.0],
                    source: EditorTextSource::Literal(String::new()),
                });
            }
        });

        ui.add_space(8.0);

        // A `Panel`, not a fixed pixel guess -- it measures its own
        // content's actual height each frame and reserves exactly that
        // much, so the status line it holds always stays fully visible
        // and pinned to the bottom regardless of the window's size (a
        // hand-picked constant here previously drifted out of sync the
        // moment spacing/padding changed, e.g. from `apply_style`,
        // silently clipping it below the window). Declared *before* the
        // layer list below on purpose: like every other `Panel`, it
        // claims its slice of `ui`'s remaining space first, leaving
        // whatever's left to the list's `ScrollArea`.
        eframe::egui::Panel::bottom("editor_footer")
            .show_separator_line(false)
            .show(ui, |ui| {
                if !self.status.is_empty() {
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(8.0);
                    ui.label(&self.status);
                    ui.add_space(4.0);
                }
            });

        let mut move_up = None;
        let mut move_down = None;
        let mut remove = None;

        eframe::egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (index, layer) in self.layers.iter_mut().enumerate() {
                    ui.group(|ui| {
                        // Match the group to whatever width the
                        // (resizable) panel actually has instead of
                        // sizing to content -- otherwise a group can
                        // only ever grow to fit its widest child and
                        // never shrinks back down when the window is
                        // narrowed.
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.strong(format!("#{} {}", index + 1, layer.label()));
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

                        show_layer_panel(ui, layer);
                    });
                }
            });

        if let Some(index) = remove {
            self.layers.remove(index);
        }
        if let Some(index) = move_up
            && index > 0
        {
            self.layers.swap(index, index - 1);
        }
        if let Some(index) = move_down
            && index + 1 < self.layers.len()
        {
            self.layers.swap(index, index + 1);
        }
    }

    fn show_preview(
        &mut self,
        ui: &mut eframe::egui::Ui,
        render_state: Option<&eframe::egui_wgpu::RenderState>,
    ) {
        ui.heading("Preview");
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

/// Property panel for one layer in the editor's layer list --
/// dispatches to a per-variant panel below, mirroring `EditorLayer`'s
/// own shape one level down.
fn show_layer_panel(ui: &mut eframe::egui::Ui, layer: &mut EditorLayer) {
    match layer {
        EditorLayer::Image { path } => show_image_panel(ui, path),
        EditorLayer::Xray {
            base,
            overlay,
            radius,
        } => show_xray_panel(ui, base, overlay, radius),
        EditorLayer::Gif { path } => show_gif_panel(ui, path),
        EditorLayer::Parallax {
            path,
            strength,
            smoothing,
        } => show_parallax_panel(ui, path, strength, smoothing),
        EditorLayer::Text {
            x,
            y,
            font_size,
            color,
            source,
        } => show_text_panel(ui, x, y, font_size, color, source),
    }
}

fn show_image_panel(ui: &mut eframe::egui::Ui, path: &mut Option<PathBuf>) {
    path_picker(ui, "Picture", path, &["png", "jpg", "jpeg", "webp"]);
}

fn show_xray_panel(
    ui: &mut eframe::egui::Ui,
    base: &mut Option<PathBuf>,
    overlay: &mut Option<PathBuf>,
    radius: &mut f32,
) {
    path_picker(ui, "Base picture", base, &["png", "jpg", "jpeg", "webp"]);
    path_picker(
        ui,
        "Overlay picture (shown near cursor)",
        overlay,
        &["png", "jpg", "jpeg", "webp"],
    );
    ui.horizontal(|ui| {
        ui.label("Radius (px):");
        scroll_slider(ui, radius, 20.0..=800.0);
    });
}

fn show_gif_panel(ui: &mut eframe::egui::Ui, path: &mut Option<PathBuf>) {
    path_picker(ui, "Gif file", path, &["gif"]);
}

fn show_parallax_panel(
    ui: &mut eframe::egui::Ui,
    path: &mut Option<PathBuf>,
    strength: &mut f32,
    smoothing: &mut f32,
) {
    path_picker(
        ui,
        "Picture (bigger than your desktop resolution works best)",
        path,
        &["png", "jpg", "jpeg", "webp"],
    );
    ui.horizontal(|ui| {
        ui.label("Strength:")
            .on_hover_text("How far the layer pans at the screen edge, as a fraction of its own size. Negative pans towards the cursor instead of away from it.");
        scroll_slider(ui, strength, -0.4..=0.4);
    });
    ui.horizontal(|ui| {
        ui.label("Smoothing (s):").on_hover_text(
            "How long the pan takes to ease towards the cursor. 0 = track instantly.",
        );
        scroll_slider(ui, smoothing, 0.0..=1.0);
    });
}

fn show_text_panel(
    ui: &mut eframe::egui::Ui,
    x: &mut f32,
    y: &mut f32,
    font_size: &mut f32,
    color: &mut [f32; 4],
    source: &mut EditorTextSource,
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
    ui.horizontal(|ui| {
        ui.label("Font size:")
            .on_hover_text("Fraction of canvas height -- resolution-independent.");
        scroll_slider(ui, font_size, 0.01..=0.3);
    });
    ui.horizontal(|ui| {
        ui.label("Color:");
        ui.color_edit_button_rgba_unmultiplied(color);
    });
    ui.horizontal(|ui| {
        ui.label("X:");
        scroll_slider(ui, x, 0.0..=1.0);
    });
    ui.horizontal(|ui| {
        ui.label("Y:");
        scroll_slider(ui, y, 0.0..=1.0);
    });
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
/// `ScrollArea` -- see `show_editor_tab`'s layer list, which relies on
/// this to let wheel-over-a-slider and wheel-over-the-list-background
/// do two different things instead of both firing at once.
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
            project_format::Layer::Image { path } => EditorLayer::Image {
                path: Some(project_dir.join(path)),
            },
            project_format::Layer::Xray {
                base,
                overlay,
                radius,
            } => EditorLayer::Xray {
                base: Some(project_dir.join(base)),
                overlay: Some(project_dir.join(overlay)),
                radius,
            },
            project_format::Layer::Gif { path } => EditorLayer::Gif {
                path: Some(project_dir.join(path)),
            },
            project_format::Layer::Parallax {
                path,
                strength,
                smoothing,
            } => EditorLayer::Parallax {
                path: Some(project_dir.join(path)),
                strength,
                smoothing,
            },
            project_format::Layer::Text {
                x,
                y,
                font_size,
                color,
                source,
            } => EditorLayer::Text {
                x,
                y,
                font_size,
                color,
                source: EditorTextSource::from_project(source),
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
                EditorLayer::Image { path } => {
                    let path = path.as_ref().expect("save is disabled until complete");
                    let file_name = stage(path, &format!("layer_{index}_image"))?;
                    project_format::Layer::Image { path: file_name }
                }
                EditorLayer::Xray {
                    base,
                    overlay,
                    radius,
                } => {
                    let base = base.as_ref().expect("save is disabled until complete");
                    let overlay = overlay.as_ref().expect("save is disabled until complete");
                    let base_name = stage(base, &format!("layer_{index}_xray_base"))?;
                    let overlay_name = stage(overlay, &format!("layer_{index}_xray_overlay"))?;
                    project_format::Layer::Xray {
                        base: base_name,
                        overlay: overlay_name,
                        radius: *radius,
                    }
                }
                EditorLayer::Gif { path } => {
                    let path = path.as_ref().expect("save is disabled until complete");
                    let file_name = stage(path, &format!("layer_{index}_anim"))?;
                    project_format::Layer::Gif { path: file_name }
                }
                EditorLayer::Parallax {
                    path,
                    strength,
                    smoothing,
                } => {
                    let path = path.as_ref().expect("save is disabled until complete");
                    let file_name = stage(path, &format!("layer_{index}_parallax"))?;
                    project_format::Layer::Parallax {
                        path: file_name,
                        strength: *strength,
                        smoothing: *smoothing,
                    }
                }
                // No asset to stage.
                EditorLayer::Text {
                    x,
                    y,
                    font_size,
                    color,
                    source,
                } => project_format::Layer::Text {
                    x: *x,
                    y: *y,
                    font_size: *font_size,
                    color: *color,
                    source: source.to_project(),
                },
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
            },
            EditorLayer::Image {
                path: Some(project_dir.join("layer_0_image.png")),
            },
            EditorLayer::Image {
                path: Some(project_dir.join("layer_1_image.png")),
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
}
