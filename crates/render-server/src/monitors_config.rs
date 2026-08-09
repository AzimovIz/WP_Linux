//! Read-only half of the monitor -> project assignment file. `editor` is
//! the sole writer (see its own `monitors_config` module); render-server
//! only ever reads this once, at startup, to seed its in-memory
//! `pending_project` map so it doesn't depend on anyone re-pushing
//! assignments after a restart. Never watched, never written here.

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

/// Monitor id -> project directory path. A missing or unparseable file is
/// treated as "nothing configured yet" (empty map) rather than a startup
/// error -- this process never writes the file, so a bad file can only
/// mean it doesn't exist yet or was hand-edited, and losing those entries
/// back to "unconfigured" is a safe, recoverable failure mode.
pub fn load() -> HashMap<String, PathBuf> {
    load_from(&config_path())
}

fn load_from(path: &Path) -> HashMap<String, PathBuf> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_an_empty_map() {
        let dir = std::env::temp_dir().join(format!(
            "wp_linux_monitors_config_test_missing_{:?}",
            std::thread::current().id()
        ));
        assert!(load_from(&dir.join("monitors.json")).is_empty());
    }

    #[test]
    fn round_trips_a_written_map() {
        let dir = std::env::temp_dir().join(format!(
            "wp_linux_monitors_config_test_roundtrip_{:?}",
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("monitors.json");
        let mut written = HashMap::new();
        written.insert("DP-1".to_string(), PathBuf::from("/tmp/some/project"));
        std::fs::write(&path, serde_json::to_string_pretty(&written).unwrap()).unwrap();

        assert_eq!(load_from(&path), written);

        std::fs::remove_dir_all(&dir).ok();
    }
}
