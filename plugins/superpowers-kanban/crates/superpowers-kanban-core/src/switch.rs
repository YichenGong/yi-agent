use std::path::Path;

use serde::Deserialize;

/// 单层偏好里记录的开关值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchValue {
    Enabled,
    Disabled,
}

/// 解析后生效的开关。
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

/// 两层解析：项目层覆盖全局层；两层都缺失 → 默认关闭。
pub fn resolve(global: Option<SwitchValue>, project: Option<SwitchValue>) -> BoardSwitch {
    match project.or(global) {
        Some(SwitchValue::Enabled) => BoardSwitch::Enabled,
        Some(SwitchValue::Disabled) | None => BoardSwitch::Disabled,
    }
}

/// 从 `preferences.json` 文本里读出开关键：优先 `superpowers_kanban`，
/// 回退旧键 `superpowers_board`（迁移期兼容）。
///
/// 文件损坏、缺键或类型不对一律返回 `None`（调用方视作"该层未设置"），绝不 panic。
/// 其他键（如 `subagent_runtime`）被忽略，因此可以安全读取现有偏好文件。
pub fn parse_switch_json(text: &str) -> Option<SwitchValue> {
    #[derive(Deserialize)]
    struct Preferences {
        superpowers_kanban: Option<bool>,
        superpowers_board: Option<bool>,
    }
    let parsed: Preferences = serde_json::from_str(text).ok()?;
    match parsed.superpowers_kanban.or(parsed.superpowers_board)? {
        true => Some(SwitchValue::Enabled),
        false => Some(SwitchValue::Disabled),
    }
}

/// 读一层偏好：缺失 / 损坏 / 缺键一律 `None`（视作"该层未设置"）。
pub fn read_layer(path: &Path) -> Option<SwitchValue> {
    let text = std::fs::read_to_string(path).ok()?;
    parse_switch_json(&text)
}

/// 写一层偏好：读-改-写整个 JSON 对象（保留 `subagent_runtime` 等其他键），
/// 只设置新键 `superpowers_kanban`；temp + rename 原子替换。
///
/// 只写新键、不删旧键：迁移期旧键仍然可读，删它属于清理而非本工具的职责。
pub fn write_layer(path: &Path, value: SwitchValue) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut object = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    object.insert(
        "superpowers_kanban".to_string(),
        serde_json::Value::Bool(matches!(value, SwitchValue::Enabled)),
    );
    let body = serde_json::to_string_pretty(&serde_json::Value::Object(object))
        .map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_legacy_key_when_the_new_one_is_absent() {
        assert_eq!(
            parse_switch_json(r#"{"superpowers_board": true}"#),
            Some(SwitchValue::Enabled)
        );
    }

    #[test]
    fn the_new_key_wins_over_the_legacy_one() {
        assert_eq!(
            parse_switch_json(r#"{"superpowers_board": true, "superpowers_kanban": false}"#),
            Some(SwitchValue::Disabled)
        );
    }

    #[test]
    fn the_project_layer_overrides_the_global_layer() {
        assert_eq!(
            resolve(Some(SwitchValue::Enabled), Some(SwitchValue::Disabled)),
            BoardSwitch::Disabled
        );
        assert_eq!(
            resolve(Some(SwitchValue::Disabled), Some(SwitchValue::Enabled)),
            BoardSwitch::Enabled
        );
    }

    #[test]
    fn a_missing_project_layer_inherits_the_global_layer() {
        assert_eq!(
            resolve(Some(SwitchValue::Enabled), None),
            BoardSwitch::Enabled
        );
        assert_eq!(
            resolve(Some(SwitchValue::Disabled), None),
            BoardSwitch::Disabled
        );
    }

    #[test]
    fn both_layers_missing_defaults_to_disabled() {
        assert_eq!(resolve(None, None), BoardSwitch::Disabled);
    }

    #[test]
    fn json_parsing_reads_the_board_key() {
        assert_eq!(
            parse_switch_json(r#"{"superpowers_board": true}"#),
            Some(SwitchValue::Enabled)
        );
        assert_eq!(
            parse_switch_json(r#"{"superpowers_board": false}"#),
            Some(SwitchValue::Disabled)
        );
    }

    #[test]
    fn a_broken_or_irrelevant_file_yields_none_instead_of_panicking() {
        assert_eq!(parse_switch_json("not json"), None);
        assert_eq!(parse_switch_json("{}"), None);
        assert_eq!(parse_switch_json(r#"{"other": 1}"#), None);
    }

    #[test]
    fn a_wrong_typed_switch_value_is_treated_as_unset() {
        // 类型不对（字符串/数字）不是解析错误，而是"该层未设置"：返回 None 且绝不 panic。
        assert_eq!(parse_switch_json(r#"{"superpowers_board":"yes"}"#), None);
        assert_eq!(parse_switch_json(r#"{"superpowers_board":1}"#), None);
        assert_eq!(parse_switch_json(r#"{"superpowers_board":null}"#), None);
    }

    #[test]
    fn an_unset_layer_falls_through_to_the_other_layer() {
        // 项目层类型不对 → 视作未设置，必须继承全局层而不是被读成 false。
        assert_eq!(parse_switch_json(r#"{"superpowers_board":"yes"}"#), None);
        assert_eq!(
            resolve(Some(SwitchValue::Enabled), None),
            BoardSwitch::Enabled
        );
    }

    #[test]
    fn a_preferences_file_keeps_its_other_keys_untouched_on_read() {
        // 现有 preferences.json 里已有 subagent_runtime 等键，读取必须忽略它们。
        assert_eq!(
            parse_switch_json(r#"{"subagent_runtime":"always","superpowers_board":true}"#),
            Some(SwitchValue::Enabled)
        );
    }

    #[test]
    fn writing_sets_the_new_key_and_preserves_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(&path, r#"{"subagent_runtime":"always"}"#).unwrap();
        write_layer(&path, SwitchValue::Enabled).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["superpowers_kanban"], true);
        assert_eq!(value["subagent_runtime"], "always");
        assert_eq!(parse_switch_json(&text), Some(SwitchValue::Enabled));
    }

    #[test]
    fn writing_into_a_missing_directory_creates_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".yi-agent/preferences.json");
        write_layer(&path, SwitchValue::Disabled).unwrap();
        assert_eq!(read_layer(&path), Some(SwitchValue::Disabled));
    }

    #[test]
    fn writing_replaces_a_previous_value_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        write_layer(&path, SwitchValue::Enabled).unwrap();
        write_layer(&path, SwitchValue::Disabled).unwrap();
        assert_eq!(read_layer(&path), Some(SwitchValue::Disabled));
        assert!(!dir.path().join("preferences.json.tmp").exists());
    }

    #[test]
    fn a_broken_file_is_repaired_rather_than_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(&path, "not json").unwrap();
        write_layer(&path, SwitchValue::Enabled).unwrap();
        assert_eq!(read_layer(&path), Some(SwitchValue::Enabled));
    }

    #[test]
    fn read_layer_reports_none_for_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_layer(&dir.path().join("nope.json")), None);
    }
}
