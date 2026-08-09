//! Command-layer trust store, `$XDG_CONFIG_HOME/wp_linux/trusted.json` --
//! a set of project ids render-server is allowed to run
//! `TextSource::Command` shell commands for (see render-server's own
//! read-only `trust_store` module, which this mirrors the file format
//! of). `editor` is the sole writer, and auto-trusts a project the
//! moment it saves one containing any Command-sourced text layer (see
//! the Save button handler in `main.rs`) -- the editor is the
//! self-authoring context, so there's no separate consent dialog in this
//! pass; a real prompt only becomes necessary once a "Discover"
//! import feature exists (out of scope, unbuilt).
//!
//! Deliberately **additive** (read-modify-write), unlike
//! `monitors_config`'s "always overwrite the whole file" convention:
//! trust needs to accumulate across every project ever saved with a
//! Command layer, not just reflect whatever's currently open in the
//! editor. Overwriting here would un-trust every previously-saved
//! project the moment a *different* one is saved, silently breaking
//! their Command layers next time render-server (re)loads them.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

const CONFIG_SUBDIR: &str = "wp_linux";
const CONFIG_FILE_NAME: &str = "trusted.json";

fn config_path() -> PathBuf {
    dirs::config_dir()
        .expect("no config dir (HOME unset?)")
        .join(CONFIG_SUBDIR)
        .join(CONFIG_FILE_NAME)
}

fn load_from(path: &Path) -> HashSet<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_to(path: &Path, trusted: &HashSet<String>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(trusted).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| e.to_string())
}

/// Adds `project_id` to the trust store, leaving every other id already
/// in it untouched -- see the module doc comment for why this can't just
/// overwrite like `monitors_config::save` does. A no-op (not an error)
/// if `project_id` is already trusted.
pub fn mark_trusted(project_id: &str) -> Result<(), String> {
    mark_trusted_in(&config_path(), project_id)
}

fn mark_trusted_in(path: &Path, project_id: &str) -> Result<(), String> {
    let mut trusted = load_from(path);
    trusted.insert(project_id.to_string());
    save_to(path, &trusted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "wplinux-editor-trust-store-test-{}-{:?}.json",
            std::process::id(),
            std::time::SystemTime::now()
        ))
    }

    #[test]
    fn marking_trusted_is_readable_back() {
        let path = unique_temp_path();
        mark_trusted_in(&path, "project-a").unwrap();
        assert!(load_from(&path).contains("project-a"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn marking_a_second_project_trusted_does_not_forget_the_first() {
        let path = unique_temp_path();
        mark_trusted_in(&path, "project-a").unwrap();
        mark_trusted_in(&path, "project-b").unwrap();

        let trusted = load_from(&path);
        assert!(trusted.contains("project-a"));
        assert!(trusted.contains("project-b"));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn marking_the_same_project_trusted_twice_is_a_no_op() {
        let path = unique_temp_path();
        mark_trusted_in(&path, "project-a").unwrap();
        mark_trusted_in(&path, "project-a").unwrap();
        assert_eq!(load_from(&path).len(), 1);
        std::fs::remove_file(&path).ok();
    }
}
