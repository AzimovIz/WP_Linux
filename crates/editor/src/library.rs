//! The wallpaper library: every project lives self-contained under
//! `$XDG_DATA_HOME/wp_linux/wallpapers/<id>/` (its own `project.json`,
//! assets, and a generated `preview.png`). `editor` is the only process
//! that ever touches this directory directly -- render-server only ever
//! opens one specific project path it's told about (see
//! `monitors_config`), so this stays a plain module here rather than
//! shared crate: promoting it later, if a second consumer ever needs it,
//! is a trivial move since nothing here depends on anything editor-specific.
//!
//! No cached index file: the picker lives inside this app now (not QML,
//! which is why an index was worth considering earlier), so scanning the
//! directory and parsing each `project.json` directly, every time, is
//! simpler and one less thing to keep in sync -- cheap for a personal
//! library of dozens of entries.

use std::path::{Path, PathBuf};

pub const LIBRARY_SUBDIR: &str = "wp_linux/wallpapers";
pub const PREVIEW_FILE_NAME: &str = "preview.png";

/// `$XDG_DATA_HOME/wp_linux/wallpapers` (default `~/.local/share/...`) --
/// always goes through `dirs::data_dir()` rather than hardcoding
/// `~/.local/share`, so a user's `$XDG_DATA_HOME` override is respected.
pub fn library_root() -> PathBuf {
    dirs::data_dir()
        .expect("no data dir (HOME unset?)")
        .join(LIBRARY_SUBDIR)
}

pub fn project_dir(id: &str) -> PathBuf {
    library_root().join(id)
}

/// One entry as found by scanning the library directory.
#[derive(Clone)]
pub struct LibraryEntry {
    pub id: String,
    pub dir: PathBuf,
    pub name: String,
    pub fps: u32,
    pub layer_count: usize,
    /// `dir.join(PREVIEW_FILE_NAME)`, only if that file actually exists.
    pub preview_path: Option<PathBuf>,
}

/// Real entry point -- scans `library_root()`.
pub fn scan() -> Vec<LibraryEntry> {
    scan_dir(&library_root())
}

/// Testable implementation: scans `root`'s immediate subdirectories,
/// loading each as a project via the shared `project_format::Project::load`.
/// A subdirectory that isn't a valid project (missing/corrupt
/// `project.json`) is skipped and logged, not treated as a whole-scan
/// failure -- one broken entry shouldn't hide every other wallpaper.
fn scan_dir(root: &Path) -> Vec<LibraryEntry> {
    let Ok(read_dir) = std::fs::read_dir(root) else {
        return Vec::new(); // library doesn't exist yet -- empty, not an error
    };

    let mut entries = Vec::new();
    for dir_entry in read_dir.flatten() {
        let dir = dir_entry.path();
        if !dir.is_dir() {
            continue;
        }
        let id = dir_entry.file_name().to_string_lossy().into_owned();
        match project_format::Project::load(&dir) {
            Ok((project, dir)) => {
                let preview_path = dir.join(PREVIEW_FILE_NAME);
                entries.push(LibraryEntry {
                    id,
                    name: project.name,
                    fps: project.fps,
                    layer_count: project.layers.len(),
                    preview_path: preview_path.is_file().then_some(preview_path),
                    dir,
                });
            }
            Err(e) => eprintln!("editor: skipping library entry {dir:?}: {e}"),
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    entries
}

/// Mints a fresh, stable id for a brand-new project -- never renamed once
/// assigned. A short hex string derived from the current time is enough:
/// this is a single-user, local app, so real collision risk is a
/// non-issue and doesn't warrant a `uuid` dependency.
pub fn new_project_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{nanos:x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_temp_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("wplinux-library-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scan_of_missing_root_is_empty() {
        let root = std::env::temp_dir().join("wplinux-library-test-does-not-exist");
        assert!(scan_dir(&root).is_empty());
    }

    #[test]
    fn scan_of_empty_dir_is_empty() {
        let root = unique_temp_dir();
        assert!(scan_dir(&root).is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn scan_finds_a_valid_project_and_skips_a_malformed_one() {
        let root = unique_temp_dir();

        let good = root.join("good-id");
        std::fs::create_dir_all(&good).unwrap();
        std::fs::write(
            good.join("project.json"),
            r#"{"name":"My Wallpaper","layers":[{"type":"image","path":"a.png"}],"fps":30}"#,
        )
        .unwrap();

        let bad = root.join("bad-id");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("project.json"), "not json").unwrap();

        // No project.json at all -- also skipped, not a crash.
        std::fs::create_dir_all(root.join("empty-id")).unwrap();

        let entries = scan_dir(&root);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "good-id");
        assert_eq!(entries[0].name, "My Wallpaper");
        assert_eq!(entries[0].layer_count, 1);
        assert!(entries[0].preview_path.is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_name_falls_back_to_empty_string() {
        let root = unique_temp_dir();
        let dir = root.join("nameless-id");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("project.json"), r#"{"layers":[],"fps":30}"#).unwrap();

        let entries = scan_dir(&root);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn new_project_id_is_non_empty_hex() {
        let id = new_project_id();
        assert!(!id.is_empty());
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
