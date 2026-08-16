//! The bundled shader library: ready-made `.wgsl` effects for the
//! generic `EffectKind::Shader` (see `project_format::parse_shader_params`)
//! live under `$XDG_DATA_HOME/wp_linux/shaders/`, installed there by the
//! app's own install pipeline (build + move-into-place script) -- this
//! module only ever reads that directory, never writes to it. Mirrors
//! `library.rs`'s own scan-the-directory-every-time approach (no cached
//! index) for the same reason: cheap for a handful of files, one less
//! thing to keep in sync.
//!
//! A shader picked from this library and one picked via `Browse...` are
//! indistinguishable once chosen -- both just become the effect's
//! `wgsl_path`. This module exists purely to produce the list the "Select"
//! dropdown in `show_shader_effect_panel` shows; nothing downstream knows
//! or cares which path a `.wgsl` file's `wgsl_path` came from.

use std::path::{Path, PathBuf};

pub const SHADERS_SUBDIR: &str = "wp_linux/shaders";

/// `$XDG_DATA_HOME/wp_linux/shaders` (default `~/.local/share/...`).
pub fn shaders_root() -> PathBuf {
    dirs::data_dir()
        .expect("no data dir (HOME unset?)")
        .join(SHADERS_SUBDIR)
}

/// One `.wgsl` file found by scanning the shader library directory.
#[derive(Clone, PartialEq)]
pub struct ShaderLibraryEntry {
    pub name: String,
    pub path: PathBuf,
}

/// Real entry point -- scans `shaders_root()`.
pub fn scan() -> Vec<ShaderLibraryEntry> {
    scan_dir(&shaders_root())
}

/// Testable implementation: every immediate `*.wgsl` file directly under
/// `root`, alphabetically by filename. A missing directory (nothing
/// installed yet, or a from-source checkout that never ran the install
/// script) is an empty list, not an error -- same convention as
/// `library::scan_dir`.
fn scan_dir(root: &Path) -> Vec<ShaderLibraryEntry> {
    let Ok(read_dir) = std::fs::read_dir(root) else {
        return Vec::new();
    };

    let mut entries: Vec<ShaderLibraryEntry> = read_dir
        .flatten()
        .map(|dir_entry| dir_entry.path())
        .filter(|path| path.is_file())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("wgsl"))
        .filter_map(|path| {
            let name = path.file_stem()?.to_str()?.to_string();
            Some(ShaderLibraryEntry { name, path })
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_temp_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wplinux-shaders-library-test-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scan_of_missing_root_is_empty() {
        let root = std::env::temp_dir().join("wplinux-shaders-library-test-does-not-exist");
        assert!(scan_dir(&root).is_empty());
    }

    #[test]
    fn scan_finds_wgsl_files_sorted_and_skips_everything_else() {
        let root = unique_temp_dir();
        std::fs::write(root.join("fire_flicker.wgsl"), "").unwrap();
        std::fs::write(root.join("vignette_custom.wgsl"), "").unwrap();
        std::fs::write(root.join("readme.txt"), "").unwrap();
        std::fs::create_dir_all(root.join("subdir.wgsl")).unwrap();

        let entries = scan_dir(&root);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["fire_flicker", "vignette_custom"]);

        std::fs::remove_dir_all(&root).ok();
    }
}
