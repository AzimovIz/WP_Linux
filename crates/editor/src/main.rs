//! Minimal wallpaper project editor: build a layer stack (picture,
//! xray, gif animation), then save it as a project folder that
//! render-server can load.

use std::path::{Path, PathBuf};

fn main() -> eframe::Result {
    let native_options = eframe::NativeOptions::default();
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
}

impl EditorLayer {
    fn label(&self) -> &'static str {
        match self {
            EditorLayer::Image { .. } => "Image",
            EditorLayer::Xray { .. } => "Xray",
            EditorLayer::Gif { .. } => "Gif",
        }
    }

    fn is_complete(&self) -> bool {
        match self {
            EditorLayer::Image { path } => path.is_some(),
            EditorLayer::Xray { base, overlay, .. } => base.is_some() && overlay.is_some(),
            EditorLayer::Gif { path } => path.is_some(),
        }
    }
}

#[derive(Default)]
struct EditorApp {
    layers: Vec<EditorLayer>,
    status: String,
}

impl eframe::App for EditorApp {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        eframe::egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("New wallpaper project");
            ui.label("Layers are drawn bottom to top -- the first one in the list is furthest back.");
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                if ui.button("Open project...").clicked() {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        match open_project(&dir) {
                            Ok(layers) => {
                                self.layers = layers;
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
            });

            ui.add_space(8.0);

            let mut move_up = None;
            let mut move_down = None;
            let mut remove = None;

            for (index, layer) in self.layers.iter_mut().enumerate() {
                ui.group(|ui| {
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
                            path_picker(ui, "Picture", path, &["png", "jpg", "jpeg"]);
                        }
                        EditorLayer::Xray {
                            base,
                            overlay,
                            radius,
                        } => {
                            path_picker(ui, "Base picture", base, &["png", "jpg", "jpeg"]);
                            path_picker(
                                ui,
                                "Overlay picture (shown near cursor)",
                                overlay,
                                &["png", "jpg", "jpeg"],
                            );
                            ui.horizontal(|ui| {
                                ui.label("Radius (px):");
                                ui.add(eframe::egui::Slider::new(radius, 20.0..=800.0));
                            });
                        }
                        EditorLayer::Gif { path } => {
                            path_picker(ui, "Gif file", path, &["gif"]);
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
                    self.status = match save_project(&dir, &self.layers) {
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

fn path_picker(ui: &mut eframe::egui::Ui, label: &str, path: &mut Option<PathBuf>, filter: &[&str]) {
    ui.horizontal(|ui| {
        ui.label(label);
        match path {
            Some(p) => ui.label(p.display().to_string()),
            None => ui.label("not set"),
        };
        if ui.button("Browse...").clicked() {
            if let Some(chosen) = rfd::FileDialog::new().add_filter("Files", filter).pick_file() {
                *path = Some(chosen);
            }
        }
    });
}

/// Loads an existing project folder back into the editor's layer list,
/// resolving each layer's relative asset paths to absolute ones so
/// `path_picker` has something to display.
fn open_project(project_dir: &Path) -> Result<Vec<EditorLayer>, String> {
    let (project, project_dir) =
        project_format::Project::load(project_dir).map_err(|e| e.to_string())?;

    Ok(project
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
        })
        .collect())
}

fn save_project(project_dir: &Path, layers: &[EditorLayer]) -> Result<(), String> {
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
        };
        saved_layers.push(saved);
    }

    let project = project_format::Project {
        layers: saved_layers,
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
