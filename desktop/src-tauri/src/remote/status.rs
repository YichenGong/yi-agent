//! `remote-status.json`：桥接层写、app-server 读的运行时状态旁路。
//!
//! **它与 `remote.json` 刻意分开**（spec §4.9）：一个是用户配置（需严格校验、
//! 原子性重要），一个是易失状态（高频更新、可丢）。合并会让状态的高频写污染
//! 配置的原子性。
//!
//! 为什么需要这个旁路本身：本机 app-server 完全不知道远端连没连——那状态活在
//! Tauri 层的槽位表里。但 `remote/relayTargets` 要在 app-server 进程里回答
//! `available`，所以桥接层得把状态写到 app-server 能读的地方。

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotStatus {
    Connected,
    Connecting,
    Disconnected,
}

impl SlotStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SlotStatus::Connected => "connected",
            SlotStatus::Connecting => "connecting",
            SlotStatus::Disconnected => "disconnected",
        }
    }

    /// 未知值**必须**落到 `Disconnected`：fail-closed。宁可让手机多试一次，
    /// 也不要报一个假的「可用」。
    ///
    /// 读取侧在 app-server 进程（`remote/relayTargets`）里，与本 crate 是两份
    /// 实现；这里保留同一套解析规则，供桌面侧调试与测试使用。
    #[allow(dead_code)]
    pub fn from_str(raw: &str) -> Self {
        match raw {
            "connected" => SlotStatus::Connected,
            "connecting" => SlotStatus::Connecting,
            _ => SlotStatus::Disconnected,
        }
    }
}

fn read_all(path: &Path) -> serde_json::Map<String, serde_json::Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.get("statuses").and_then(|s| s.as_object()).cloned())
        .unwrap_or_default()
}

/// 读回全部状态。桌面侧只在测试与排障时用；真正的消费方是 app-server。
#[allow(dead_code)]
pub fn read_status(path: &Path) -> HashMap<String, SlotStatus> {
    read_all(path)
        .into_iter()
        .map(|(key, value)| {
            let status = value
                .as_str()
                .map(SlotStatus::from_str)
                .unwrap_or(SlotStatus::Disconnected);
            (key, status)
        })
        .collect()
}

/// 读-改-写。写失败**只记日志**（调用方决定）：这是可丢的运行时状态，
/// 不该让连接流程因为它失败。
pub fn write_status(path: &Path, profile_id: &str, status: SlotStatus) -> io::Result<()> {
    let mut all = read_all(path);
    all.insert(profile_id.to_string(), serde_json::json!(status.as_str()));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(&serde_json::json!({ "statuses": all }))
        .map_err(io::Error::other)?;
    // 唯一临时名 + rename：状态写得频繁，并发写者不能共用临时路径。
    let tmp: PathBuf = path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_a_single_status() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("remote-status.json");
        write_status(&path, "proj-foo", SlotStatus::Connected).unwrap();
        assert_eq!(
            read_status(&path).get("proj-foo"),
            Some(&SlotStatus::Connected)
        );
    }

    /// 每个项目的状态是独立的：写 B 不能把 A 抹掉（那会让手机看到 A 掉线）。
    #[test]
    fn writing_one_profile_preserves_the_others() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("remote-status.json");
        write_status(&path, "a", SlotStatus::Connected).unwrap();
        write_status(&path, "b", SlotStatus::Connecting).unwrap();
        let map = read_status(&path);
        assert_eq!(map.get("a"), Some(&SlotStatus::Connected));
        assert_eq!(map.get("b"), Some(&SlotStatus::Connecting));
    }

    #[test]
    fn a_status_can_be_overwritten() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("remote-status.json");
        write_status(&path, "a", SlotStatus::Connected).unwrap();
        write_status(&path, "a", SlotStatus::Disconnected).unwrap();
        assert_eq!(read_status(&path).get("a"), Some(&SlotStatus::Disconnected));
    }

    #[test]
    fn unknown_values_fail_closed_to_disconnected() {
        assert_eq!(SlotStatus::from_str("weird"), SlotStatus::Disconnected);
        assert_eq!(SlotStatus::from_str(""), SlotStatus::Disconnected);
        // 大小写不宽容：文件是本机自己写的，出现别的写法就是损坏。
        assert_eq!(SlotStatus::from_str("Connected"), SlotStatus::Disconnected);
    }

    #[test]
    fn a_non_string_value_reads_as_disconnected() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("remote-status.json");
        std::fs::write(&path, r#"{"statuses":{"a":1}}"#).unwrap();
        assert_eq!(read_status(&path).get("a"), Some(&SlotStatus::Disconnected));
    }

    #[test]
    fn broken_file_reads_as_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("remote-status.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(read_status(&path).is_empty());
    }

    #[test]
    fn missing_file_reads_as_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(read_status(&dir.path().join("nope.json")).is_empty());
    }

    #[test]
    fn a_write_leaves_no_temp_file_behind() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("remote-status.json");
        write_status(&path, "a", SlotStatus::Connected).unwrap();
        let stray: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(stray.is_empty(), "temp files leaked: {stray:?}");
    }
}
