//! 主题偏好：读写 `<workdir>/.yi-agent/preferences.json` 的 `theme` 键。
//!
//! 该文件是共享的（`subagent_runtime`、`superpowers_kanban` 也写它），因此
//! 写必须是读-改-写并保留无关键；落盘用「临时文件 + rename」保证原子性，
//! 与 `yi-agent/src/tui/runtime_prefs.rs` 同一约定。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 桌面端主题。默认深色（与改动前的现状一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

impl Theme {
    pub fn as_str(self) -> &'static str {
        match self {
            Theme::Dark => "dark",
            Theme::Light => "light",
        }
    }

    /// 大小写与空白不敏感；未知值一律回退 `Dark`。
    pub fn parse(s: &str) -> Theme {
        match s.trim().to_ascii_lowercase().as_str() {
            "light" => Theme::Light,
            _ => Theme::Dark,
        }
    }
}

/// `<workdir>/.yi-agent/preferences.json`。
pub fn preferences_path(workdir: &Path) -> PathBuf {
    workdir.join(".yi-agent").join("preferences.json")
}

/// 读主题。缺文件 / 不可读 / 损坏一律回退 `Dark`——坏偏好绝不阻断启动。
pub fn load(workdir: &Path) -> Theme {
    let path = preferences_path(workdir);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) => return Theme::Dark,
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(value) => value
            .get("theme")
            .and_then(|v| v.as_str())
            .map(Theme::parse)
            .unwrap_or(Theme::Dark),
        Err(_) => Theme::Dark,
    }
}

/// 写主题：读-改-写，保留无关的顶层键，最后原子替换。
pub fn save(workdir: &Path, theme: Theme) -> std::io::Result<()> {
    set_object_value(
        workdir,
        "theme",
        serde_json::Value::String(theme.as_str().to_string()),
    )
}

/// 后台值守（常驻 watchman / 登录自启）开关。缺省 `true`——用户没表态时按
/// 期望行为走，而不是静默地什么都不做。
///
/// 坏文件 / 缺键一律回退 `true`，与 [`load`] 对主题的容错策略一致。
pub fn load_watchman_enabled(workdir: &Path) -> bool {
    let path = preferences_path(workdir);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| {
            value
                .get(yi_agent_boards::watchman::PREFERENCE_KEY)
                .and_then(|v| v.as_bool())
        })
        .unwrap_or(true)
}

/// 写开关：与 [`save`] 同一约定——读-改-写、保留无关键、原子替换。
pub fn save_watchman_enabled(workdir: &Path, enabled: bool) -> std::io::Result<()> {
    set_object_value(
        workdir,
        yi_agent_boards::watchman::PREFERENCE_KEY,
        serde_json::Value::Bool(enabled),
    )
}

/// 读 `<workdir>/.yi-agent/preferences.json` 的顶层对象、插入一个键、原子替换。
///
/// [`save`] 与 [`save_watchman_enabled`] 的唯一实现：两者共处同一文件，若各写
/// 各的读-改-写，后写的会把先写的键整个抹掉。
fn set_object_value(workdir: &Path, key: &str, value: serde_json::Value) -> std::io::Result<()> {
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
    object.insert(key.to_string(), value);
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
    fn missing_file_defaults_to_dark() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(load(dir.path()), Theme::Dark);
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::TempDir::new().unwrap();
        save(dir.path(), Theme::Light).unwrap();
        assert_eq!(load(dir.path()), Theme::Light);
    }

    #[test]
    fn malformed_and_unknown_values_fall_back_to_dark() {
        for body in ["not json", "{\"theme\":\"bogus\"}", "[]", "{\"theme\":5}"] {
            let dir = tempfile::TempDir::new().unwrap();
            std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
            std::fs::write(preferences_path(dir.path()), body).unwrap();
            assert_eq!(load(dir.path()), Theme::Dark, "body: {body}");
        }
    }

    #[test]
    fn saving_theme_preserves_unrelated_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        std::fs::write(
            preferences_path(dir.path()),
            r#"{"subagent_runtime":"never","superpowers_kanban":true}"#,
        )
        .unwrap();
        save(dir.path(), Theme::Light).unwrap();
        let text = std::fs::read_to_string(preferences_path(dir.path())).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["theme"], "light");
        assert_eq!(value["subagent_runtime"], "never");
        assert_eq!(value["superpowers_kanban"], true);
    }

    #[test]
    fn parsing_is_case_insensitive() {
        assert_eq!(Theme::parse("LIGHT"), Theme::Light);
        assert_eq!(Theme::parse("dark"), Theme::Dark);
        assert_eq!(Theme::parse("  light "), Theme::Light);
        assert_eq!(Theme::parse(""), Theme::Dark);
    }

    #[test]
    fn the_watchman_preference_defaults_to_on_and_round_trips() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(load_watchman_enabled(dir.path()), "default is on");
        save_watchman_enabled(dir.path(), false).unwrap();
        assert!(!load_watchman_enabled(dir.path()));
        // 与 theme 共处一文件且互不覆盖。
        save(dir.path(), Theme::Light).unwrap();
        assert!(!load_watchman_enabled(dir.path()));
        assert_eq!(load(dir.path()), Theme::Light);
    }

    #[test]
    fn the_watchman_preference_falls_back_to_on_for_broken_files() {
        for body in [
            "not json",
            "{\"board_watchman_enabled\":\"yes\"}",
            "[]",
            "{}",
        ] {
            let dir = tempfile::TempDir::new().unwrap();
            std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
            std::fs::write(preferences_path(dir.path()), body).unwrap();
            assert!(load_watchman_enabled(dir.path()), "body: {body}");
        }
    }

    #[test]
    fn saving_the_watchman_preference_preserves_unrelated_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        std::fs::write(
            preferences_path(dir.path()),
            r#"{"theme":"light","subagent_runtime":"never"}"#,
        )
        .unwrap();
        save_watchman_enabled(dir.path(), false).unwrap();
        let text = std::fs::read_to_string(preferences_path(dir.path())).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["board_watchman_enabled"], false);
        assert_eq!(value["theme"], "light");
        assert_eq!(value["subagent_runtime"], "never");
    }
}
