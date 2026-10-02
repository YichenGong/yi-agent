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
///
/// 与 `yi-agent/src/tui/runtime_prefs.rs` 同一约定:缺文件是正常情形,不打日志;
/// 文件存在却读不出来或解析不了则 `tracing::warn!`(两类措辞不同),否则
/// 「我的主题总是复位」在日志里无从诊断。回退值不变。
pub fn load(workdir: &Path) -> Theme {
    let path = preferences_path(workdir);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        // 缺文件是首次运行的正常情形,不是故障,不打日志。
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Theme::Dark,
        Err(error) => {
            tracing::warn!(
                error = %error,
                path = %path.display(),
                "could not read the theme preference; using the default"
            );
            return Theme::Dark;
        }
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(value) => value
            .get("theme")
            .and_then(|v| v.as_str())
            .map(Theme::parse)
            .unwrap_or(Theme::Dark),
        Err(error) => {
            tracing::warn!(
                error = %error,
                path = %path.display(),
                "invalid theme preference; using the default"
            );
            Theme::Dark
        }
    }
}

/// 写主题：读-改-写，保留无关的顶层键，最后原子替换。
pub fn save(workdir: &Path, theme: Theme) -> std::io::Result<()> {
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
    object.insert(
        "theme".to_string(),
        serde_json::Value::String(theme.as_str().to_string()),
    );
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
}

/// `load` 的日志回归测试。
///
/// 缺文件是正常情形,不该有告警;存在却读不出来(权限/是目录)或损坏(非法 JSON)
/// 才必须留下 warning——否则「我的主题总是复位」在日志里毫无线索。捕获手法与
/// `yi-agent-store` 的 `error_logging_tests` 一致(`tracing_subscriber::fmt` +
/// 自定义 writer,`set_default` 只在当前线程生效),不引入全局 subscriber。
#[cfg(test)]
mod warning_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// 在捕获 subscriber 下跑 `f`,返回结果与格式化后的日志文本。
    fn capture_logs<T>(f: impl FnOnce() -> T) -> (T, String) {
        let captured = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&captured));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let out = f();
        let logged = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
        (out, logged)
    }

    /// 缺文件是正常的首次运行,不能告警(否则日志被噪声淹没)。
    #[test]
    fn a_missing_file_is_not_logged() {
        let dir = tempfile::TempDir::new().unwrap();
        let (theme, logged) = capture_logs(|| load(dir.path()));
        assert_eq!(theme, Theme::Dark);
        assert!(
            logged.is_empty(),
            "a genuinely missing file must not warn: {logged}"
        );
    }

    /// 存在却读不出来(这里把 `preferences.json` 造成目录):必须 warn 且指明路径。
    #[test]
    fn an_unreadable_file_is_logged_and_falls_back_to_dark() {
        let dir = tempfile::TempDir::new().unwrap();
        let prefs = dir.path().join(".yi-agent").join("preferences.json");
        std::fs::create_dir_all(&prefs).unwrap(); // 目录 → read_to_string 失败且非 NotFound
        let (theme, logged) = capture_logs(|| load(dir.path()));
        assert_eq!(theme, Theme::Dark);
        assert!(logged.contains("WARN"), "must be a warning: {logged}");
        assert!(
            logged.contains("preferences.json"),
            "must name the file: {logged}"
        );
        assert!(
            logged.contains("could not read"),
            "must distinguish the read failure: {logged}"
        );
        assert!(
            !logged.contains("invalid"),
            "a read failure is not a parse failure: {logged}"
        );
    }

    /// 存在但损坏(非法 JSON):必须 warn 且与「读失败」措辞可区分。
    #[test]
    fn a_corrupt_file_is_logged_and_falls_back_to_dark() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        std::fs::write(preferences_path(dir.path()), "not json at all").unwrap();
        let (theme, logged) = capture_logs(|| load(dir.path()));
        assert_eq!(theme, Theme::Dark);
        assert!(logged.contains("WARN"), "must be a warning: {logged}");
        assert!(
            logged.contains("preferences.json"),
            "must name the file: {logged}"
        );
        assert!(
            logged.contains("invalid"),
            "must distinguish the parse failure: {logged}"
        );
        assert!(
            !logged.contains("could not read"),
            "a parse failure is not a read failure: {logged}"
        );
    }
}
