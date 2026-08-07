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

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use player::wgpu;
use player::{LoadedLayer, SceneRenderer};

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
        Box::new(|_cc| Ok(Box::new(EditorApp::default()))),
    )
}

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
}

impl EditorLayer {
    fn label(&self) -> &'static str {
        match self {
            EditorLayer::Image { .. } => "Image",
            EditorLayer::Xray { .. } => "Xray",
            EditorLayer::Gif { .. } => "Gif",
            EditorLayer::Parallax { .. } => "Parallax",
        }
    }

    fn is_complete(&self) -> bool {
        match self {
            EditorLayer::Image { path } => path.is_some(),
            EditorLayer::Xray { base, overlay, .. } => base.is_some() && overlay.is_some(),
            EditorLayer::Gif { path } => path.is_some(),
            EditorLayer::Parallax { path, .. } => path.is_some(),
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

        let layers = self.renderer.load_scene(Path::new(""), &project)?;
        let (natural_width, natural_height) =
            layers.first().map(LoadedLayer::size).unwrap_or((16, 9));
        let (width, height) = preview_size(natural_width, natural_height);
        if width != self.width || height != self.height {
            let (texture, view) = create_preview_texture(&self.renderer, width, height);
            render_state.renderer.write().update_egui_texture_from_wgpu_texture(
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
                EditorLayer::Parallax { strength, smoothing, .. } => {
                    loaded.set_parallax_params(*strength, *smoothing);
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
        self.renderer.update_parallax(&mut self.layers, cursor_px, parallax_dt_ms);
        self.renderer.render_to_texture(&self.view, &self.layers, wgpu::Color::TRANSPARENT);

        self.layers.iter().any(|l| {
            matches!(l, LoadedLayer::Gif { .. } | LoadedLayer::Xray(_) | LoadedLayer::Parallax(_))
        })
    }
}

fn create_preview_texture(renderer: &SceneRenderer, width: u32, height: u32) -> (wgpu::Texture, wgpu::TextureView) {
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
                path: path.as_ref().expect("checked complete above").display().to_string(),
            },
            EditorLayer::Xray { base, overlay, radius } => project_format::Layer::Xray {
                base: base.as_ref().expect("checked complete above").display().to_string(),
                overlay: overlay
                    .as_ref()
                    .expect("checked complete above")
                    .display()
                    .to_string(),
                radius: *radius,
            },
            EditorLayer::Gif { path } => project_format::Layer::Gif {
                path: path.as_ref().expect("checked complete above").display().to_string(),
            },
            EditorLayer::Parallax { path, strength, smoothing } => project_format::Layer::Parallax {
                path: path.as_ref().expect("checked complete above").display().to_string(),
                strength: *strength,
                smoothing: *smoothing,
            },
        })
        .collect();
    Some(project_format::Project {
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
    layers: Vec<EditorLayer>,
    fps: u32,
    status: String,
    preview: Option<Preview>,
    preview_error: Option<String>,
}

impl Default for EditorApp {
    fn default() -> Self {
        Self {
            layers: Vec::new(),
            fps: 30,
            status: String::new(),
            preview: None,
            preview_error: None,
        }
    }
}

impl eframe::App for EditorApp {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, frame: &mut eframe::Frame) {
        let render_state = frame.wgpu_render_state().cloned();

        eframe::egui::Panel::left("preview_panel")
            .resizable(true)
            .default_size(760.0)
            .size_range(320.0..=980.0)
            .show(ui, |ui| {
                self.show_preview(ui, render_state.as_ref());
            });

        eframe::egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("New wallpaper project");
            ui.label("Layers are drawn bottom to top -- the first one in the list is furthest back.");
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                if ui.button("Open project...").clicked() {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        match open_project(&dir) {
                            Ok((layers, fps)) => {
                                self.layers = layers;
                                self.fps = fps;
                                self.status = format!("Opened {}", dir.display());
                            }
                            Err(e) => {
                                self.status = format!("Failed to open {}: {e}", dir.display());
                            }
                        }
                    }
                }
            });

            ui.add_space(8.0);

            ui.horizontal(|ui| {
                ui.label("Target FPS (animated/cursor layers only):")
                    .on_hover_text("Used by render-server once this project is loaded there -- doesn't affect the preview on the left, which always redraws at a fixed rate.");
                ui.add(eframe::egui::Slider::new(&mut self.fps, 1..=60));
            });

            ui.add_space(8.0);

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
            });

            ui.add_space(8.0);

            let mut move_up = None;
            let mut move_down = None;
            let mut remove = None;

            for (index, layer) in self.layers.iter_mut().enumerate() {
                ui.group(|ui| {
                    // Match the group to whatever width the (resizable)
                    // panel actually has instead of sizing to content --
                    // otherwise a group can only ever grow to fit its
                    // widest child and never shrinks back down when the
                    // window is narrowed.
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

                    match layer {
                        EditorLayer::Image { path } => {
                            path_picker(ui, "Picture", path, &["png", "jpg", "jpeg", "webp"]);
                        }
                        EditorLayer::Xray {
                            base,
                            overlay,
                            radius,
                        } => {
                            path_picker(ui, "Base picture", base, &["png", "jpg", "jpeg", "webp"]);
                            path_picker(
                                ui,
                                "Overlay picture (shown near cursor)",
                                overlay,
                                &["png", "jpg", "jpeg", "webp"],
                            );
                            ui.horizontal(|ui| {
                                ui.label("Radius (px):");
                                ui.add(eframe::egui::Slider::new(radius, 20.0..=800.0));
                            });
                        }
                        EditorLayer::Gif { path } => {
                            path_picker(ui, "Gif file", path, &["gif"]);
                        }
                        EditorLayer::Parallax { path, strength, smoothing } => {
                            path_picker(
                                ui,
                                "Picture (bigger than your desktop resolution works best)",
                                path,
                                &["png", "jpg", "jpeg", "webp"],
                            );
                            ui.horizontal(|ui| {
                                ui.label("Strength:")
                                    .on_hover_text("How far the layer pans at the screen edge, as a fraction of its own size. Negative pans towards the cursor instead of away from it.");
                                ui.add(eframe::egui::Slider::new(strength, -0.4..=0.4));
                            });
                            ui.horizontal(|ui| {
                                ui.label("Smoothing (s):")
                                    .on_hover_text("How long the pan takes to ease towards the cursor. 0 = track instantly.");
                                ui.add(eframe::egui::Slider::new(smoothing, 0.0..=1.0));
                            });
                        }
                    }
                });
            }

            if let Some(index) = remove {
                self.layers.remove(index);
            }
            if let Some(index) = move_up {
                if index > 0 {
                    self.layers.swap(index, index - 1);
                }
            }
            if let Some(index) = move_down {
                if index + 1 < self.layers.len() {
                    self.layers.swap(index, index + 1);
                }
            }

            ui.add_space(16.0);
            ui.separator();
            ui.add_space(8.0);

            let can_save = !self.layers.is_empty() && self.layers.iter().all(EditorLayer::is_complete);
            if ui
                .add_enabled(can_save, eframe::egui::Button::new("Save project as..."))
                .clicked()
            {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    self.status = match save_project(&dir, &self.layers, self.fps) {
                        Ok(()) => format!("Saved to {}", dir.display()),
                        Err(e) => format!("Failed to save: {e}"),
                    };
                }
            }

            if !self.status.is_empty() {
                ui.add_space(8.0);
                ui.label(&self.status);
            }
        });
    }
}

impl EditorApp {
    fn show_preview(&mut self, ui: &mut eframe::egui::Ui, render_state: Option<&eframe::egui_wgpu::RenderState>) {
        ui.heading("Preview");
        ui.add_space(4.0);

        let Some(render_state) = render_state else {
            ui.label("GPU preview unavailable (eframe isn't running on wgpu).");
            return;
        };

        let preview = self.preview.get_or_insert_with(|| Preview::new(render_state));

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
            let response = ui.add(eframe::egui::Image::from_texture((preview.texture_id, display_size)));

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

/// Shows `label`, a Browse button, and the chosen file's name -- full
/// absolute paths (what's actually stored) can easily run past 500px and
/// are why layer rows used to force the whole window wider than it
/// should be. The button is placed before the path text specifically so
/// the path's `.truncate()` sees accurate remaining space (it eats
/// whatever's left in the row instead of its own unbounded natural
/// width) and elides with "..." instead of pushing the row wider; the
/// full path is still available as a tooltip on hover.
fn path_picker(ui: &mut eframe::egui::Ui, label: &str, path: &mut Option<PathBuf>, filter: &[&str]) {
    ui.horizontal(|ui| {
        ui.label(label);
        if ui.button("Browse...").clicked() {
            if let Some(chosen) = rfd::FileDialog::new().add_filter("Files", filter).pick_file() {
                *path = Some(chosen);
            }
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
fn open_project(project_dir: &Path) -> Result<(Vec<EditorLayer>, u32), String> {
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
            project_format::Layer::Parallax { path, strength, smoothing } => EditorLayer::Parallax {
                path: Some(project_dir.join(path)),
                strength,
                smoothing,
            },
        })
        .collect();

    Ok((layers, project.fps))
}

fn save_project(project_dir: &Path, layers: &[EditorLayer], fps: u32) -> Result<(), String> {
    std::fs::create_dir_all(project_dir).map_err(|e| e.to_string())?;

    let mut saved_layers = Vec::with_capacity(layers.len());
    for (index, layer) in layers.iter().enumerate() {
        let saved = match layer {
            EditorLayer::Image { path } => {
                let path = path.as_ref().expect("save is disabled until complete");
                let file_name = copy_asset(project_dir, path, &format!("layer_{index}_image"))?;
                project_format::Layer::Image { path: file_name }
            }
            EditorLayer::Xray {
                base,
                overlay,
                radius,
            } => {
                let base = base.as_ref().expect("save is disabled until complete");
                let overlay = overlay.as_ref().expect("save is disabled until complete");
                let base_name = copy_asset(project_dir, base, &format!("layer_{index}_xray_base"))?;
                let overlay_name =
                    copy_asset(project_dir, overlay, &format!("layer_{index}_xray_overlay"))?;
                project_format::Layer::Xray {
                    base: base_name,
                    overlay: overlay_name,
                    radius: *radius,
                }
            }
            EditorLayer::Gif { path } => {
                let path = path.as_ref().expect("save is disabled until complete");
                let file_name = copy_asset(project_dir, path, &format!("layer_{index}_anim"))?;
                project_format::Layer::Gif { path: file_name }
            }
            EditorLayer::Parallax { path, strength, smoothing } => {
                let path = path.as_ref().expect("save is disabled until complete");
                let file_name = copy_asset(project_dir, path, &format!("layer_{index}_parallax"))?;
                project_format::Layer::Parallax {
                    path: file_name,
                    strength: *strength,
                    smoothing: *smoothing,
                }
            }
        };
        saved_layers.push(saved);
    }

    let project = project_format::Project {
        layers: saved_layers,
        fps,
    };
    project.save(project_dir).map_err(|e| e.to_string())
}

/// Copies `source` into `project_dir` under `stem` plus `source`'s
/// extension, returning the file name (relative to `project_dir`) that
/// got written.
fn copy_asset(project_dir: &Path, source: &Path, stem: &str) -> Result<String, String> {
    let extension = source.extension().and_then(|e| e.to_str()).unwrap_or("png");
    let file_name = format!("{stem}.{extension}");
    let dest = project_dir.join(&file_name);

    // If you opened a project and are saving back into the same folder,
    // an untouched layer's source path already points at `dest` --
    // fs::copy truncates the destination before it's done reading the
    // source, so copying a file onto itself would zero it out.
    let same_file = source
        .canonicalize()
        .ok()
        .zip(dest.canonicalize().ok())
        .is_some_and(|(a, b)| a == b);

    if !same_file {
        std::fs::copy(source, &dest).map_err(|e| e.to_string())?;
    }
    Ok(file_name)
}
