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

/// `preferences.json` 里侧车中继地址的键名。
///
/// 桌面壳（`desktop/src-tauri/src/bridge.rs`）直接读写同一个文件，故这里把键名
/// 公开出去，避免两边各写一串字面量。
pub const RELAY_URL_KEY: &str = "relay_url";

/// 读侧车要连的中继地址（桌面设置页写入）。
///
/// 缺文件 / 坏文件 / 缺键 / 非字符串 / 纯空白一律回退 `None`——坏偏好绝不阻断
/// 侧车启动，`None` 即今日的纯 stdio 行为。
pub fn load_relay_url(workdir: &Path) -> Option<String> {
    let path = preferences_path(workdir);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| {
            value
                .get(RELAY_URL_KEY)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|url| !url.is_empty())
                .map(str::to_string)
        })
}

/// 写侧车中继地址。`None` / 空白 → **删除**该键（回到纯 stdio），而不是留一个
/// 空串——否则「未配置」与「配置成空」在文件里长得一样。
///
/// 与 [`save`] 同一约定：读-改-写、保留无关键、原子替换。
pub fn save_relay_url(workdir: &Path, url: Option<&str>) -> std::io::Result<()> {
    let value = url
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(|url| serde_json::Value::String(url.to_string()));
    read_write_key(workdir, RELAY_URL_KEY, value)
}

/// 读 `<workdir>/.yi-agent/preferences.json` 的顶层对象、设置（或删除）一个键、
/// 原子替换。
///
/// [`save`]、[`save_watchman_enabled`] 与 [`save_relay_url`] 的唯一实现：三者
/// 共处同一文件，若各写各的读-改-写，后写的会把先写的键整个抹掉。`value` 为
/// `None` 表示删除该键（清理偏好用）。
fn read_write_key(
    workdir: &Path,
    key: &str,
    value: Option<serde_json::Value>,
) -> std::io::Result<()> {
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
    match value {
        Some(value) => {
            object.insert(key.to_string(), value);
        }
        None => {
            object.remove(key);
        }
    }
    let text = serde_json::to_string_pretty(&serde_json::Value::Object(object))
        .map_err(std::io::Error::other)?;
    // 临时名逐次唯一:同一目录可能有并发写者(runtime_prefs、kanban 的 scaffold
    // 写的是同一个 `preferences.json`),固定名会互相截断。
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp_path = temp_path_for(&dir, seq);
    std::fs::write(&tmp_path, &text)?;
    std::fs::rename(&tmp_path, &path)
}

/// 写入一个顶层键（保留其余键），[`read_write_key`] 的薄封装。
fn set_object_value(workdir: &Path, key: &str, value: serde_json::Value) -> std::io::Result<()> {
    read_write_key(workdir, key, Some(value))
}

/// 进程内递增序号,给每次写入的临时文件一个不同的后缀。
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 一次写入所用的临时文件名:`preferences.json.<pid>.<seq>.tmp`。
///
/// 与 `thread_store::write_atomic` 同一约定。`preferences.json` 是共享文件
/// (`runtime_prefs` 与 kanban 的 `scaffold` 也写它):固定的
/// `preferences.json.tmp` 会让并发写者互相截断、或把对方刚写的内容 rename
/// 成自己的结果。最终的 `preferences.json` 与「读-改-写 + rename」不变。
fn temp_path_for(dir: &Path, seq: u64) -> PathBuf {
    dir.join(format!(
        "preferences.json.{}.{}.tmp",
        std::process::id(),
        seq
    ))
}

/// 中继地址偏好（`relay_url`）的读写测试。
///
/// 该键是桌面设置页与侧车之间的唯一落点：侧车启动时经 `load_relay_url` 读它。
/// 与 `theme`/`board_watchman_enabled` 同处一个文件，故同样断言写不覆盖无关键。
#[cfg(test)]
mod relay_tests {
    use super::*;

    #[test]
    fn a_missing_file_has_no_relay_url() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(load_relay_url(dir.path()), None);
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::TempDir::new().unwrap();
        save_relay_url(dir.path(), Some("wss://relay.example.com/connect?session=x")).unwrap();
        assert_eq!(
            load_relay_url(dir.path()).as_deref(),
            Some("wss://relay.example.com/connect?session=x")
        );
    }

    /// 落盘前 trim：用户从浏览器地址栏复制过来的地址常带空白。
    #[test]
    fn a_stored_url_is_trimmed_on_save_and_load() {
        let dir = tempfile::TempDir::new().unwrap();
        save_relay_url(dir.path(), Some("  wss://relay.example.com/connect?session=x  ")).unwrap();
        assert_eq!(
            load_relay_url(dir.path()).as_deref(),
            Some("wss://relay.example.com/connect?session=x")
        );
        // 直接写带空白的值进文件，读出来同样 trim。
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        std::fs::write(preferences_path(dir.path()), r#"{"relay_url":"  wss://r/x  "}"#).unwrap();
        assert_eq!(load_relay_url(dir.path()).as_deref(), Some("wss://r/x"));
    }

    /// 空串 / 纯空白等价于未设置：清除该键，而不是留一个空值（空值仍会出现在
    /// `ui/settings/read` 里，让「未配置」长得像「配置成空」）。
    #[test]
    fn saving_an_empty_or_blank_url_clears_the_key() {
        for blank in ["", "   "] {
            let dir = tempfile::TempDir::new().unwrap();
            save_relay_url(dir.path(), Some("wss://relay.example.com/connect?session=x")).unwrap();
            save_relay_url(dir.path(), Some(blank)).unwrap();
            assert_eq!(load_relay_url(dir.path()), None, "blank: {blank:?}");

            let text = std::fs::read_to_string(preferences_path(dir.path())).unwrap();
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert!(
                value.get("relay_url").is_none(),
                "a cleared key must be removed, not left empty: {text}"
            );
        }
    }

    #[test]
    fn saving_none_clears_the_key() {
        let dir = tempfile::TempDir::new().unwrap();
        save_relay_url(dir.path(), Some("wss://relay.example.com/connect?session=x")).unwrap();
        save_relay_url(dir.path(), None).unwrap();
        assert_eq!(load_relay_url(dir.path()), None);
    }

    /// 与 `theme`/`board_watchman_enabled` 同处一个文件：三方写路径不得互相覆盖。
    #[test]
    fn saving_the_relay_url_preserves_unrelated_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        std::fs::write(
            preferences_path(dir.path()),
            r#"{"theme":"light","board_watchman_enabled":false,"subagent_runtime":"never"}"#,
        )
        .unwrap();
        save_relay_url(dir.path(), Some("wss://relay.example.com/connect?session=x")).unwrap();

        let text = std::fs::read_to_string(preferences_path(dir.path())).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["relay_url"], "wss://relay.example.com/connect?session=x");
        assert_eq!(value["theme"], "light");
        assert_eq!(value["board_watchman_enabled"], false);
        assert_eq!(value["subagent_runtime"], "never");

        // 反向：theme 的写不得抹掉 relay_url。
        save(dir.path(), Theme::Dark).unwrap();
        assert_eq!(
            load_relay_url(dir.path()).as_deref(),
            Some("wss://relay.example.com/connect?session=x")
        );
    }

    /// 坏文件一律回退「未设置」——坏偏好绝不阻断侧车启动。
    #[test]
    fn a_broken_file_falls_back_to_none() {
        for body in [
            "not json",
            r#"{"relay_url":5}"#,
            r#"{"relay_url":""}"#,
            r#"{"relay_url":"   "}"#,
            "[]",
            "{}",
        ] {
            let dir = tempfile::TempDir::new().unwrap();
            std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
            std::fs::write(preferences_path(dir.path()), body).unwrap();
            assert_eq!(load_relay_url(dir.path()), None, "body: {body}");
        }
    }

    /// 保存同样不留临时文件（走的是同一个原子落盘辅助）。
    #[test]
    fn saving_the_relay_url_leaves_no_temp_file_behind() {
        let dir = tempfile::TempDir::new().unwrap();
        save_relay_url(dir.path(), Some("wss://relay.example.com/connect?session=x")).unwrap();
        let stray: Vec<String> = std::fs::read_dir(dir.path().join(".yi-agent"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != "preferences.json")
            .collect();
        assert!(stray.is_empty(), "no temp file may survive a save: {stray:?}");
    }
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

    /// `save` 之后目录里只该剩最终的 `preferences.json`,不留临时文件。
    #[test]
    fn saving_leaves_no_temp_file_behind() {
        let dir = tempfile::TempDir::new().unwrap();
        save(dir.path(), Theme::Light).unwrap();
        save(dir.path(), Theme::Dark).unwrap();
        assert_eq!(load(dir.path()), Theme::Dark);
        let stray: Vec<String> = std::fs::read_dir(dir.path().join(".yi-agent"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != "preferences.json")
            .collect();
        assert!(stray.is_empty(), "no temp file may survive a save: {stray:?}");
    }

    /// 每次写入用的临时名必须唯一:固定名会让同一目录的并发写者互相截断。
    ///
    /// 断言的是 `save` 真正调用的 [`temp_path_for`] 本身,不是复制一份命名规则。
    #[test]
    fn each_write_uses_a_unique_temp_name() {
        let dir = tempfile::TempDir::new().unwrap();
        let first = temp_path_for(dir.path(), 0);
        let second = temp_path_for(dir.path(), 1);
        assert_ne!(
            first, second,
            "the temp name must differ per write or concurrent writers clobber each other"
        );
        let name = first.to_string_lossy().into_owned();
        assert!(
            name.contains(&std::process::id().to_string()),
            "the temp name must carry the pid: {name}"
        );
        assert!(name.ends_with(".tmp"), "the temp name must stay *.tmp: {name}");
        assert_ne!(
            first,
            preferences_path(dir.path()),
            "the temp name must not be the final file name"
        );
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

/// 值守偏好（`board_watchman_enabled`）的读写测试。与 `theme` 同处一个
/// `preferences.json`，所以这里同样断言两条写路径互不覆盖。
#[cfg(test)]
mod watchman_tests {
    use super::*;

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
