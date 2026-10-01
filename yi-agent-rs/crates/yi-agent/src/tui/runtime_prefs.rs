//! Project-level TUI preferences persisted under `<workdir>/.yi-agent/`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Whether the TUI should offer to start the local subagent runtime on launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimePreference {
    #[default]
    Ask,
    Always,
    Never,
}

/// On-disk shape. A wrapper object (not a bare string) so more preferences can
/// be added later without breaking existing files.
#[derive(Debug, Default, Serialize, Deserialize)]
struct PreferencesFile {
    #[serde(default)]
    subagent_runtime: RuntimePreference,
}

/// Path of the project-level preference file: `<workdir>/.yi-agent/preferences.json`.
pub fn preferences_path(workdir: &Path) -> PathBuf {
    workdir.join(".yi-agent").join("preferences.json")
}

/// Load the preference.
///
/// A missing, unreadable, or malformed file yields `Ask`: a broken preference
/// must never block startup or silently disable delegation.
pub fn load(workdir: &Path) -> RuntimePreference {
    let path = preferences_path(workdir);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return RuntimePreference::Ask;
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                path = %path.display(),
                "could not read TUI preferences; using defaults"
            );
            return RuntimePreference::Ask;
        }
    };
    match serde_json::from_str::<PreferencesFile>(&text) {
        Ok(file) => file.subagent_runtime,
        Err(error) => {
            tracing::warn!(
                error = %error,
                path = %path.display(),
                "invalid TUI preferences; using defaults"
            );
            RuntimePreference::Ask
        }
    }
}

/// Persist the preference atomically: write a sibling temp file, then rename.
///
/// Mirrors `yi-agent-core/src/permission.rs` (`permissions.toml`), where rename
/// within one filesystem is atomic — a crash cannot leave a half-written file.
///
/// The write is a **read-modify-write**: `preferences.json` is shared with other
/// writers (notably the Superpowers Kanban switch, which stores
/// `superpowers_kanban`), so saving the runtime preference must preserve every
/// unrelated key instead of replacing the file with a single-key object.
pub fn save(workdir: &Path, pref: RuntimePreference) -> std::io::Result<()> {
    let dir = workdir.join(".yi-agent");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("preferences.json");
    let mut object = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default(),
        Err(_) => serde_json::Map::new(),
    };
    let value = serde_json::to_value(pref).map_err(std::io::Error::other)?;
    object.insert("subagent_runtime".to_string(), value);
    let text = serde_json::to_string_pretty(&serde_json::Value::Object(object))
        .map_err(std::io::Error::other)?;
    let tmp_path = dir.join("preferences.json.tmp");
    std::fs::write(&tmp_path, &text)?;
    std::fs::rename(&tmp_path, &path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_defaults_to_ask() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(load(dir.path()), RuntimePreference::Ask);
    }

    #[test]
    fn save_then_load_round_trips_every_state() {
        for pref in [
            RuntimePreference::Ask,
            RuntimePreference::Always,
            RuntimePreference::Never,
        ] {
            let dir = tempfile::TempDir::new().unwrap();
            save(dir.path(), pref).unwrap();
            assert_eq!(load(dir.path()), pref);
        }
    }

    #[test]
    fn malformed_and_unknown_values_fall_back_to_ask() {
        for body in [
            "not json at all",
            "{\"subagent_runtime\":\"bogus\"}",
            "[]",
            "{\"subagent_runtime\":5}",
        ] {
            let dir = tempfile::TempDir::new().unwrap();
            std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
            std::fs::write(preferences_path(dir.path()), body).unwrap();
            assert_eq!(load(dir.path()), RuntimePreference::Ask, "body: {body}");
        }
    }

    #[test]
    fn empty_object_defaults_to_ask() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        std::fs::write(preferences_path(dir.path()), "{}").unwrap();
        assert_eq!(load(dir.path()), RuntimePreference::Ask);
    }

    #[test]
    fn save_creates_directory_and_leaves_no_temp_file() {
        let dir = tempfile::TempDir::new().unwrap();
        save(dir.path(), RuntimePreference::Never).unwrap();
        assert!(dir.path().join(".yi-agent/preferences.json").exists());
        assert!(!dir.path().join(".yi-agent/preferences.json.tmp").exists());
    }

    #[test]
    fn saving_the_runtime_preference_preserves_unrelated_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        std::fs::write(
            preferences_path(dir.path()),
            // The legacy key still resolves during the migration window.
            r#"{"superpowers_board":true}"#,
        )
        .unwrap();
        save(dir.path(), RuntimePreference::Never).unwrap();
        let text = std::fs::read_to_string(preferences_path(dir.path())).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["subagent_runtime"], "never");
        assert_eq!(
            value["superpowers_board"], true,
            "saving the runtime preference must not drop the board switch"
        );
    }

    #[test]
    fn saving_over_a_corrupt_file_replaces_it_with_a_valid_one() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        std::fs::write(preferences_path(dir.path()), "{ not json").unwrap();
        save(dir.path(), RuntimePreference::Always).unwrap();
        assert_eq!(
            load(dir.path()),
            RuntimePreference::Always,
            "a corrupt file must not block the save"
        );
    }
}
