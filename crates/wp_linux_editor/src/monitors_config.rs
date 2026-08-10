//! Monitor -> project assignment file, `$XDG_CONFIG_HOME/wp_linux/monitors.json`
//! (default `~/.config/...`). `editor` is the sole writer -- render-server
//! only ever reads its own copy of this reader at startup (see its
//! `monitors_config` module) and never watches or writes it. Kept as user
//! *configuration* (not `$XDG_STATE_HOME`): which wallpaper is assigned to
//! which monitor is a meaningful, explicit choice the user would be
//! unhappy to lose, not low-stakes view/history state.
//!
//! This is a deliberate small duplicate of render-server's read-only half
//! rather than a shared crate -- the format is ~15 lines of `serde_json`
//! over a `HashMap`, stable enough that sharing would only add cross-crate
//! coupling for no real benefit.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

const CONFIG_SUBDIR: &str = "wp_linux";
const CONFIG_FILE_NAME: &str = "monitors.json";

fn config_path() -> PathBuf {
    dirs::config_dir()
        .expect("no config dir (HOME unset?)")
        .join(CONFIG_SUBDIR)
        .join(CONFIG_FILE_NAME)
}

/// Missing or unparseable file reads as "nothing assigned yet" (empty
/// map), not an error -- there's nothing to recover into on first run.
pub fn load() -> HashMap<String, PathBuf> {
    load_from(&config_path())
}

fn load_from(path: &Path) -> HashMap<String, PathBuf> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Always writes the *entire* map -- the app holds the full picture of
/// every monitor's assignment at all times, so a partial merge is never
/// needed (and would risk resurrecting an assignment the user just
/// intentionally cleared).
pub fn save(assignments: &HashMap<String, PathBuf>) -> Result<(), String> {
    save_to(&config_path(), assignments)
}

fn save_to(path: &Path, assignments: &HashMap<String, PathBuf>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(assignments).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "wplinux-monitors-config-test-{}-{:?}.json",
            std::process::id(),
            std::time::SystemTime::now()
        ))
    }

    #[test]
    fn missing_file_is_an_empty_map() {
        assert!(load_from(&unique_temp_path()).is_empty());
    }

    #[test]
    fn round_trips_a_saved_map() {
        let path = unique_temp_path();
        let mut assignments = HashMap::new();
        assignments.insert("DP-1".to_string(), PathBuf::from("/tmp/some/project"));
        assignments.insert("HDMI-A-1".to_string(), PathBuf::from("/tmp/other/project"));

        save_to(&path, &assignments).expect("save_to failed");
        assert_eq!(load_from(&path), assignments);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn save_overwrites_rather_than_merges() {
        let path = unique_temp_path();
        let mut first = HashMap::new();
        first.insert("DP-1".to_string(), PathBuf::from("/tmp/a"));
        save_to(&path, &first).unwrap();

        let mut second = HashMap::new();
        second.insert("HDMI-A-1".to_string(), PathBuf::from("/tmp/b"));
        save_to(&path, &second).unwrap();

        assert_eq!(load_from(&path), second);
        std::fs::remove_file(&path).ok();
    }
}
