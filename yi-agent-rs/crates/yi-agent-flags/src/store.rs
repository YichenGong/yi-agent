//! `flags.json` 的读写，以及「默认 → 文件 → 环境变量 → 构建类型硬闸」的取值链。

use std::path::{Path, PathBuf};

use crate::error::FlagError;
use crate::registry::{REGISTRY, find};

/// 一个取值来自哪一层。`flags status` 要把它打出来——值不对时八成是环境变量盖了文件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Default,
    File,
    Env,
    /// 构建类型不是 dev，硬闸把所有开关注销。
    BuildKind,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::File => "file",
            Source::Env => "env",
            Source::BuildKind => "build-kind",
        }
    }
}

/// `flags/list` 与服务端表格共用的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlagView {
    pub name: String,
    pub summary: String,
    pub enabled: bool,
    pub expires_on: String,
    pub source: Source,
}

/// `$YI_AGENT_HOME/flags.json`。
pub fn flags_path() -> Option<PathBuf> {
    yi_agent_store::paths::home_join("flags.json")
}

/// 文件里只存开着的键与否；缺省一律视为关。
fn load_map(path: &Path) -> serde_json::Map<String, serde_json::Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Default::default();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        // 坏文件不阻断启动；下一次写入会把它覆盖成合法结构。
        return Default::default();
    };
    value
        .get("flags")
        .and_then(|f| f.as_object())
        .cloned()
        .unwrap_or_default()
}

/// 读一个键的落盘值。未注册的名字一律当作不存在。
pub fn read_at(path: &Path, name: &str) -> Result<Option<bool>, FlagError> {
    if find(name).is_none() && !crate::registry::is_test_known(name) {
        return Ok(None);
    }
    Ok(load_map(path).get(name).and_then(|v| v.as_bool()))
}

/// 「默认 → 文件 → 环境变量 → 构建类型」的完整取值链。
pub fn status_at(
    path: &Path,
    name: &str,
    build_kind: &str,
    env_override: Option<&str>,
) -> (bool, Source) {
    if build_kind != "dev" {
        return (false, Source::BuildKind);
    }
    if let Some(raw) = env_override {
        return (raw == "1", Source::Env);
    }
    match read_at(path, name) {
        Ok(Some(value)) => (value, Source::File),
        _ => (false, Source::Default),
    }
}

/// 生产入口：用真实构建类型与真实环境变量。
pub fn is_enabled(name: &str) -> bool {
    status(name).map(|(enabled, _)| enabled).unwrap_or(false)
}

pub fn status(name: &str) -> Option<(bool, Source)> {
    let path = flags_path()?;
    let env = env_override_for(name);
    Some(status_at(&path, name, crate::build_kind(), env.as_deref()))
}

/// `YI_AGENT_FLAG_<NAME>`：临时覆盖，用于「不改进程配置就试一下开关」。
fn env_override_for(name: &str) -> Option<String> {
    let key = format!(
        "YI_AGENT_FLAG_{}",
        name.to_ascii_uppercase().replace('-', "_")
    );
    std::env::var(&key).ok().filter(|v| !v.is_empty())
}

/// 写一个键。原子替换：先写临时文件再 rename，避免读到半截 JSON。
pub fn write_at(path: &Path, name: &str, enabled: bool) -> Result<(), FlagError> {
    if find(name).is_none() && !crate::registry::is_test_known(name) {
        return Err(FlagError::UnknownFlag(name.to_string()));
    }
    let mut map = load_map(path);
    map.insert(name.to_string(), serde_json::Value::Bool(enabled));
    let document = serde_json::json!({ "version": 1, "flags": map });
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(FlagError::Io)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&document).unwrap())
        .map_err(FlagError::Io)?;
    std::fs::rename(&tmp, path).map_err(FlagError::Io)
}

/// 列出全部注册项及其生效值。
pub fn all_at(path: &Path, build_kind: &str) -> Vec<FlagView> {
    REGISTRY
        .iter()
        .map(|def| {
            let env = env_override_for(def.name);
            let (enabled, source) = status_at(path, def.name, build_kind, env.as_deref());
            FlagView {
                name: def.name.to_string(),
                summary: def.summary.to_string(),
                enabled,
                expires_on: def.expires_on.to_string(),
                source,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // 说明：以下用例通过 `set_at` / `read_at` 注入路径，不改进程环境变量
    // （cargo 并行跑测试，全局量会让用例互相竞态）。唯一例外是环境变量层，
    // 那几个用例用 `serial` 风格的互斥量串行。

    #[test]
    fn a_missing_file_means_everything_is_off() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        assert_eq!(read_at(&path, "known").unwrap(), None);
    }

    #[test]
    fn a_saved_value_round_trips() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        write_at(&path, "known", true).unwrap();
        assert_eq!(read_at(&path, "known").unwrap(), Some(true));
        write_at(&path, "known", false).unwrap();
        assert_eq!(read_at(&path, "known").unwrap(), Some(false));
    }

    #[test]
    fn a_corrupt_file_reads_as_empty_instead_of_failing_startup() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(read_at(&path, "known").unwrap(), None);
    }

    #[test]
    fn writing_an_unregistered_name_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        let err = write_at(&path, "no_such_flag", true).unwrap_err();
        assert!(matches!(err, FlagError::UnknownFlag(_)), "got {err:?}");
    }

    #[test]
    fn unknown_keys_in_the_file_are_ignored() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        std::fs::write(&path, r#"{"version":1,"flags":{"bogus_key":true}}"#).unwrap();
        assert_eq!(read_at(&path, "bogus_key").unwrap(), None);
    }

    #[test]
    fn the_build_kind_gate_wins_over_everything() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        write_at(&path, "known", true).unwrap();
        let (enabled, source) = status_at(&path, "known", "release", Some("1"));
        assert!(!enabled, "a release build must not honour any flag");
        assert_eq!(source, Source::BuildKind);
    }

    #[test]
    fn a_dev_build_reads_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        write_at(&path, "known", true).unwrap();
        let (enabled, source) = status_at(&path, "known", "dev", None);
        assert!(enabled);
        assert_eq!(source, Source::File);
    }

    #[test]
    fn an_env_override_beats_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        write_at(&path, "known", true).unwrap();
        let (enabled, source) = status_at(&path, "known", "dev", Some("0"));
        assert!(!enabled);
        assert_eq!(source, Source::Env);
    }

    #[test]
    fn an_absent_flag_reports_the_default_source() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        let (enabled, source) = status_at(&path, "known", "dev", None);
        assert!(!enabled);
        assert_eq!(source, Source::Default);
    }

    #[test]
    fn a_nogit_build_is_treated_like_release() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        write_at(&path, "known", true).unwrap();
        let (enabled, source) = status_at(&path, "known", "nogit", None);
        assert!(!enabled);
        assert_eq!(source, Source::BuildKind);
    }
}
