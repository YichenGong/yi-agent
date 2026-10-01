use std::path::{Path, PathBuf};

use serde_json::Value;

/// 生效的开关值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardSwitch {
    Enabled,
    Disabled,
}

impl BoardSwitch {
    pub fn is_enabled(self) -> bool {
        matches!(self, BoardSwitch::Enabled)
    }
}

/// 生效值来自哪一层，供 UI 显示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchSource {
    Project,
    Global,
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedSwitch {
    pub value: BoardSwitch,
    pub source: SwitchSource,
}

/// 全局层路径：`~/.yi-agent/preferences.json`。
pub fn global_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join(".yi-agent")
            .join("preferences.json")
    })
}

/// 项目层路径：`<workdir>/.yi-agent/preferences.json`。
pub fn project_path(workdir: &Path) -> PathBuf {
    workdir.join(".yi-agent").join("preferences.json")
}

/// 读一层。文件缺失、不可读、损坏或缺键一律返回 `None`（视作该层未设置）。
pub fn read_layer(path: &Path) -> Option<BoardSwitch> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(error = %error, path = %path.display(), "invalid preferences; treating the layer as unset");
            return None;
        }
    };
    let key = |name: &str| value.get(name).and_then(Value::as_bool);
    match key("superpowers_kanban").or_else(|| key("superpowers_board")) {
        Some(true) => Some(BoardSwitch::Enabled),
        Some(false) => Some(BoardSwitch::Disabled),
        None => None,
    }
}

/// 两层解析：项目层覆盖全局层；两层都缺 → 默认关闭。
pub fn resolve(global: Option<BoardSwitch>, project: Option<BoardSwitch>) -> ResolvedSwitch {
    if let Some(value) = project {
        return ResolvedSwitch {
            value,
            source: SwitchSource::Project,
        };
    }
    if let Some(value) = global {
        return ResolvedSwitch {
            value,
            source: SwitchSource::Global,
        };
    }
    ResolvedSwitch {
        value: BoardSwitch::Disabled,
        source: SwitchSource::Default,
    }
}

/// 写一层：读-改-写整个 JSON 对象，保留其他键；临时文件 + rename 原子替换。
pub fn write_layer(path: &Path, value: BoardSwitch) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut object = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    object.insert(
        "superpowers_kanban".to_string(),
        Value::Bool(value.is_enabled()),
    );
    let body =
        serde_json::to_string_pretty(&Value::Object(object)).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_project_layer_wins_over_the_global_layer() {
        let resolved = resolve(Some(BoardSwitch::Enabled), Some(BoardSwitch::Disabled));
        assert_eq!(resolved.value, BoardSwitch::Disabled);
        assert_eq!(resolved.source, SwitchSource::Project);

        let resolved = resolve(Some(BoardSwitch::Disabled), Some(BoardSwitch::Enabled));
        assert_eq!(resolved.value, BoardSwitch::Enabled);
        assert_eq!(resolved.source, SwitchSource::Project);
    }

    #[test]
    fn a_missing_project_layer_inherits_the_global_layer() {
        let resolved = resolve(Some(BoardSwitch::Enabled), None);
        assert_eq!(resolved.value, BoardSwitch::Enabled);
        assert_eq!(resolved.source, SwitchSource::Global);
    }

    #[test]
    fn both_layers_missing_defaults_to_disabled() {
        let resolved = resolve(None, None);
        assert_eq!(resolved.value, BoardSwitch::Disabled);
        assert_eq!(resolved.source, SwitchSource::Default);
    }

    #[test]
    fn a_written_layer_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        write_layer(&path, BoardSwitch::Enabled).unwrap();
        assert_eq!(read_layer(&path), Some(BoardSwitch::Enabled));
        write_layer(&path, BoardSwitch::Disabled).unwrap();
        assert_eq!(read_layer(&path), Some(BoardSwitch::Disabled));
    }

    #[test]
    fn writing_preserves_unrelated_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(&path, r#"{"subagent_runtime":"always"}"#).unwrap();

        write_layer(&path, BoardSwitch::Enabled).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["subagent_runtime"], "always");
        assert_eq!(value["superpowers_kanban"], true);
    }

    #[test]
    fn reads_the_legacy_switch_key_when_the_new_one_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(&path, r#"{"superpowers_board":true}"#).unwrap();
        assert_eq!(read_layer(&path), Some(BoardSwitch::Enabled));
    }

    #[test]
    fn the_new_key_wins_over_the_legacy_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(
            &path,
            r#"{"superpowers_board":true,"superpowers_kanban":false}"#,
        )
        .unwrap();
        assert_eq!(read_layer(&path), Some(BoardSwitch::Disabled));
    }

    #[test]
    fn writes_only_the_new_key_and_preserves_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(&path, r#"{"subagent_runtime":"always"}"#).unwrap();
        write_layer(&path, BoardSwitch::Enabled).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["superpowers_kanban"], true);
        assert_eq!(value["subagent_runtime"], "always");
        assert!(value.get("superpowers_board").is_none());
    }

    #[test]
    fn a_broken_file_reads_as_none_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(read_layer(&path), None);
    }

    #[test]
    fn a_missing_file_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_layer(&dir.path().join("absent.json")), None);
    }

    #[test]
    fn writing_is_atomic_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        write_layer(&path, BoardSwitch::Enabled).unwrap();
        assert!(!dir.path().join("preferences.json.tmp").exists());
    }

    #[test]
    fn the_project_path_lives_under_the_workdir() {
        assert_eq!(
            project_path(Path::new("/project")),
            PathBuf::from("/project/.yi-agent/preferences.json")
        );
    }

    // 以下两个用例超出 brief 的 9 个用例，仍只覆盖同一份公开 API。

    #[test]
    fn writing_disabled_persists_false() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        write_layer(&path, BoardSwitch::Disabled).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["superpowers_kanban"], false);
        assert_eq!(read_layer(&path), Some(BoardSwitch::Disabled));
    }

    #[test]
    fn a_file_without_the_key_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(&path, r#"{"subagent_runtime":"always"}"#).unwrap();
        assert_eq!(read_layer(&path), None);
    }
}
