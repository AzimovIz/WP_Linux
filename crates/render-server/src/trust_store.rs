//! Read-only half of the Command-layer trust store,
//! `$XDG_CONFIG_HOME/wp_linux/trusted.json` -- a set of project ids
//! render-server is allowed to run `TextSource::Command` shell commands
//! for. `editor` is the sole writer (see its own `trust_store` module,
//! which -- unlike `monitors_config`'s "always overwrite" convention --
//! is additive, since trust needs to accumulate across every project
//! ever saved with a Command layer, not just reflect whatever's
//! currently open). Never watched, never written here; mirrors
//! `monitors_config.rs`'s exact shape.

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

/// Whether `project_id` (a project directory's own `file_name()`, the
/// library's existing id convention) is allowed to run Command-sourced
/// text layers. A missing or unparseable file means "nothing trusted
/// yet" -- the same safe, recoverable-failure-mode default
/// `monitors_config::load` uses for the same reasons.
pub fn is_trusted(project_id: &str) -> bool {
    is_trusted_in(&config_path(), project_id)
}

fn is_trusted_in(path: &Path, project_id: &str) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<HashSet<String>>(&text).ok())
        .unwrap_or_default()
        .contains(project_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "wplinux-trust-store-test-{}-{:?}.json",
            std::process::id(),
            std::time::SystemTime::now()
        ))
    }

    #[test]
    fn missing_file_trusts_nothing() {
        assert!(!is_trusted_in(&unique_temp_path(), "some-project"));
    }

    #[test]
    fn trusts_only_ids_present_in_the_file() {
        let path = unique_temp_path();
        let mut trusted = HashSet::new();
        trusted.insert("trusted-project".to_string());
        std::fs::write(&path, serde_json::to_string(&trusted).unwrap()).unwrap();

        assert!(is_trusted_in(&path, "trusted-project"));
        assert!(!is_trusted_in(&path, "some-other-project"));

        std::fs::remove_file(&path).ok();
    }
}
