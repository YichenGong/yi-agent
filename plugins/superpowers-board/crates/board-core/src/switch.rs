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

/// 从 `preferences.json` 文本里读出 `superpowers_board` 键。
///
/// 文件损坏、缺键或类型不对一律返回 `None`（调用方视作"该层未设置"），绝不 panic。
/// 其他键（如 `subagent_runtime`）被忽略，因此可以安全读取现有偏好文件。
pub fn parse_switch_json(text: &str) -> Option<SwitchValue> {
    #[derive(Deserialize)]
    struct Preferences {
        superpowers_board: Option<bool>,
    }
    let parsed: Preferences = serde_json::from_str(text).ok()?;
    match parsed.superpowers_board? {
        true => Some(SwitchValue::Enabled),
        false => Some(SwitchValue::Disabled),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
