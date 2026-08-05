//! Minimal wallpaper project editor. Not a scene composer yet -- just
//! enough to produce a `project.json` that render-server can load: pick
//! an image, toggle the cursor glow, save as a project folder.

use std::path::{Path, PathBuf};

fn main() -> eframe::Result {
    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "WP Linux Editor",
        native_options,
        Box::new(|_cc| Ok(Box::new(EditorApp::default()))),
    )
}

#[derive(Default)]
struct EditorApp {
    image_path: Option<PathBuf>,
    cursor_glow: bool,
    status: String,
}

impl eframe::App for EditorApp {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        eframe::egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("New wallpaper project");
            ui.add_space(8.0);

            if ui.button("Choose image...").clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Images", &["png", "jpg", "jpeg"])
                    .pick_file()
                {
                    self.image_path = Some(path);
                    self.status.clear();
                }
            }

            match &self.image_path {
                Some(path) => {
                    ui.label(format!("Image: {}", path.display()));
                }
                None => {
                    ui.label("No image chosen yet.");
                }
            }

            ui.add_space(8.0);
            ui.checkbox(&mut self.cursor_glow, "Show cursor glow");

            ui.add_space(16.0);
            ui.separator();
            ui.add_space(8.0);

            let can_save = self.image_path.is_some();
            if ui
                .add_enabled(can_save, eframe::egui::Button::new("Save project as..."))
                .clicked()
            {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    let image_path = self.image_path.clone().expect("button is disabled otherwise");
                    self.status = match save_project(&dir, &image_path, self.cursor_glow) {
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

fn save_project(project_dir: &Path, image_path: &Path, cursor_glow: bool) -> Result<(), String> {
    std::fs::create_dir_all(project_dir).map_err(|e| e.to_string())?;

    let extension = image_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png");
    let image_file_name = format!("image.{extension}");
    std::fs::copy(image_path, project_dir.join(&image_file_name)).map_err(|e| e.to_string())?;

    let project = project_format::Project {
        image: image_file_name,
        cursor_glow,
    };
    project.save(project_dir).map_err(|e| e.to_string())
}
