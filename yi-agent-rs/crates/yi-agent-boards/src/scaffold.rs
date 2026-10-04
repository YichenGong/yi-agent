//! Per-project scaffolding: the supervisor manifest and the preference switch.

use std::path::{Path, PathBuf};

/// Name of the supervisor manifest, the preference key it toggles, and the
/// plugin executable the host must resolve to an absolute path.
const SUPERVISOR_NAME: &str = "superpowers-kanban";
const SWITCH_KEY: &str = "superpowers_kanban";

/// Template for `<project>/.yi-agent/supervisors/superpowers-kanban.json`.
///
/// Byte-for-byte the manifest shipped with the plugin
/// (`plugins/superpowers-kanban/supervisors/superpowers-kanban.json`), except
/// that `command` is substituted at install time: a bare `superpowers-kanban`
/// is not a valid `Command` for the supervisor when the app carries a different
/// `PATH`. `{command}` is not a placeholder the supervisor expands — only
/// `{workdir}`, `{state_dir}`, and `{runtime_dir}` are — so the substituted
/// absolute path passes through untouched.
///
/// `stop_when_disabled` is `false` because the plugin serves the query channel
/// the board UI talks to: stopping it on switch-off would take away the only
/// way to read the switch and turn it back on. The plugin gates its own work on
/// the switch instead (see `run_daemon`), so off still means "stop advancing".
///
/// The manifest does not pass `--interval-secs`: the plugin reads its tick
/// interval from the plugin settings each tick, so a config change takes effect
/// without a restart, and the plugin's own default bounds how long an orphaned
/// plugin can hold the single-instance lock before it notices the daemon is
/// gone (threshold 3 × interval ≈ 30s at the default).
const MANIFEST_TEMPLATE: &str = r#"{
  "name": "superpowers-kanban",
  "command": "{command}",
  "args": [
    "run",
    "--runtime-dir",
    "{runtime_dir}",
    "--state-dir",
    "{state_dir}",
    "--project-root",
    "{workdir}"
  ],
  "switch_key": "superpowers_kanban",
  "stop_when_disabled": false,
  "restart_backoff_ms": 1000,
  "restart_backoff_max_ms": 30000,
  "query_socket": "{state_dir}/superpowers-kanban.sock"
}
"#;

/// The plugin this module scaffolds, as the supervisor and the daemon's plugin
/// registry name it. A query addressed to any other name is refused by the
/// daemon, so the lifecycle must ask for exactly this one.
pub fn plugin_name() -> &'static str {
    SUPERVISOR_NAME
}

/// Absolute path of the plugin executable the project's supervisor should run.
///
/// The Homebrew prefix is the supported installation; when the plugin is
/// somewhere else (a `cargo install`, a dev build) fall back to the first
/// executable named `superpowers-kanban` on `PATH`.
fn plugin_command() -> std::io::Result<PathBuf> {
    let homebrew = PathBuf::from("/opt/homebrew/bin").join(SUPERVISOR_NAME);
    if homebrew.is_file() {
        return Ok(homebrew);
    }
    let path = std::env::var_os("PATH").ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("cannot locate the `{SUPERVISOR_NAME}` executable: PATH is not set"),
        )
    })?;
    std::env::split_paths(&path)
        .map(|entry| entry.join(SUPERVISOR_NAME))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("cannot locate the `{SUPERVISOR_NAME}` executable on PATH"),
            )
        })
}

/// Write `<project>/.yi-agent/supervisors/superpowers-kanban.json` (idempotent).
pub fn install_manifest(project: &Path) -> std::io::Result<()> {
    let command = plugin_command()?;
    let dir = project.join(".yi-agent").join("supervisors");
    std::fs::create_dir_all(&dir)?;
    let text = MANIFEST_TEMPLATE.replace("{command}", &command.to_string_lossy());
    std::fs::write(dir.join(format!("{SUPERVISOR_NAME}.json")), text)
}

/// Set `superpowers_kanban` to `true` in `<project>/.yi-agent/preferences.json`,
/// preserving every unrelated key.
///
/// `preferences.json` is shared with other writers (notably the runtime
/// preference), so this is a read-modify-write followed by a sibling `tmp` +
/// `rename` — mirroring `runtime_prefs::save`.
pub fn enable_switch(project: &Path) -> std::io::Result<()> {
    let dir = project.join(".yi-agent");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("preferences.json");
    let mut object = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default(),
        Err(_) => serde_json::Map::new(),
    };
    object.insert(SWITCH_KEY.to_string(), serde_json::Value::Bool(true));
    let text = serde_json::to_string_pretty(&serde_json::Value::Object(object))
        .map_err(std::io::Error::other)?;
    let tmp_path = dir.join("preferences.json.tmp");
    std::fs::write(&tmp_path, &text)?;
    std::fs::rename(&tmp_path, &path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn enabling_the_switch_preserves_unrelated_preferences() {
        let dir = tempfile::tempdir().unwrap();
        let prefs = dir.path().join(".yi-agent");
        std::fs::create_dir_all(&prefs).unwrap();
        std::fs::write(
            prefs.join("preferences.json"),
            r#"{"subagent_runtime":"always","superpowers_kanban":false}"#,
        )
        .unwrap();

        enable_switch(dir.path()).unwrap();

        let text = std::fs::read_to_string(prefs.join("preferences.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["superpowers_kanban"], true);
        assert_eq!(value["subagent_runtime"], "always", "无关的键必须保留");
    }

    #[test]
    fn installing_the_manifest_is_idempotent_and_uses_the_absolute_command() {
        let dir = tempfile::tempdir().unwrap();
        install_manifest(dir.path()).unwrap();
        install_manifest(dir.path()).unwrap();
        let path = dir
            .path()
            .join(".yi-agent/supervisors/superpowers-kanban.json");
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(value["name"], "superpowers-kanban");
        assert_eq!(value["switch_key"], "superpowers_kanban");
        // The plugin serves the query channel, so it must survive switch-off to
        // stay re-enable-able; the supervisor must not kill it.
        assert_eq!(
            value["stop_when_disabled"], false,
            "看板插件必须声明开关关闭时不停止"
        );
        assert_eq!(value["query_socket"], "{state_dir}/superpowers-kanban.sock");
        let command = value["command"].as_str().unwrap();
        assert!(command.ends_with("superpowers-kanban"), "got {command}");
        assert!(
            Path::new(command).is_absolute(),
            "命令必须绝对路径：{command}"
        );
    }

    #[test]
    fn enabling_the_switch_creates_the_file_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        enable_switch(dir.path()).unwrap();
        let prefs = dir.path().join(".yi-agent");
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(prefs.join("preferences.json")).unwrap())
                .unwrap();
        assert_eq!(value[SWITCH_KEY], true);
        assert!(!prefs.join("preferences.json.tmp").exists());
    }

    #[test]
    fn the_installed_manifest_expands_to_the_plugin_arguments() {
        let dir = tempfile::tempdir().unwrap();
        install_manifest(dir.path()).unwrap();
        let path = dir
            .path()
            .join(".yi-agent/supervisors/superpowers-kanban.json");
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(
            value["args"],
            serde_json::json!([
                "run",
                "--runtime-dir",
                "{runtime_dir}",
                "--state-dir",
                "{state_dir}",
                "--project-root",
                "{workdir}"
            ])
        );
        assert_eq!(value["restart_backoff_ms"], 1000);
        assert_eq!(value["restart_backoff_max_ms"], 30000);
    }
}
