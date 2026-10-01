use std::path::Path;

use serde_json::Value;

/// 从一个 JSON 文件里读顶层布尔键。缺失、不可读、损坏或类型不符一律 `None`。
pub fn read_bool_key(path: &Path, key: &str) -> Option<bool> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value.get(key).and_then(Value::as_bool)
}

/// 两层解析：项目层显式设置则用项目层，否则用全局层，两层都缺省则关闭。
pub fn resolve_bool(global: Option<bool>, project: Option<bool>) -> bool {
    project.or(global).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_wins_over_global() {
        assert!(!resolve_bool(Some(true), Some(false)));
        assert!(resolve_bool(Some(false), Some(true)));
    }

    #[test]
    fn an_unset_project_layer_inherits_global() {
        assert!(resolve_bool(Some(true), None));
        assert!(!resolve_bool(Some(false), None));
    }

    #[test]
    fn both_unset_defaults_to_off() {
        assert!(!resolve_bool(None, None));
    }

    #[test]
    fn read_bool_key_returns_the_typed_value_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(&path, r#"{"demo_on":true,"other":"x"}"#).unwrap();
        assert_eq!(read_bool_key(&path, "demo_on"), Some(true));
        assert_eq!(read_bool_key(&path, "other"), None);
        assert_eq!(read_bool_key(&path, "missing"), None);
    }

    #[test]
    fn a_missing_or_corrupt_file_reads_as_unset() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_bool_key(&dir.path().join("nope.json"), "k"), None);
        let broken = dir.path().join("preferences.json");
        std::fs::write(&broken, "{ not json").unwrap();
        assert_eq!(read_bool_key(&broken, "k"), None);
    }
}
