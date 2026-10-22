//! 会话分组（group）的开关判定。
//!
//! 与 `lead_gate` / `plugin_gate` 同一手法：**只在构造点算一次，随实例下传**。
//! 不在请求路径上调 `yi_agent_flags::status`——它读进程级 `YI_AGENT_HOME` 与
//! 进程级 `build_kind`，并行测试里会互相污染。
//!
//! 分组是纯视图能力，所以闸门很窄，只有两件事要做：**不下发**组表、**不执行**
//! 写操作。会话本身完全不受影响（关掉后它们照常出现在列表里，只是不在框内）。

use std::path::Path;

/// 分组总开关的名字。与注册表同源：`the_flag_name_matches_the_registry` 钉住一致。
pub const SESSION_GROUPS_FLAG: &str = "session_groups";

/// 从 `flags.json` + 构建类型解析生效值。**每个实例调用一次**（`serve()` 入口）。
///
/// 不走库的环境变量层：环境变量同样是进程级的，会让并行测试互相污染。
pub fn resolve_from_store(flags_path: &Path, build_kind: &str) -> bool {
    matches!(
        yi_agent_flags::store::status_at(flags_path, SESSION_GROUPS_FLAG, build_kind, None),
        (true, _)
    )
}

/// 生产入口：从真实环境解析一次。
pub fn from_process_environment() -> bool {
    match yi_agent_flags::store::flags_path() {
        Some(path) => resolve_from_store(&path, yi_agent_flags::build_kind()),
        None => false,
    }
}

/// 判定入口。`None` = 装配方漏传，**失败关闭**。
pub fn is_session_groups_enabled(flag_value: Option<bool>) -> bool {
    flag_value == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_name_matches_the_registry() {
        assert_eq!(SESSION_GROUPS_FLAG, "session_groups");
        assert!(
            yi_agent_flags::find(SESSION_GROUPS_FLAG).is_some(),
            "session_groups must be registered, otherwise flags/set rejects it"
        );
    }

    #[test]
    fn an_injected_value_wins() {
        assert!(is_session_groups_enabled(Some(true)));
        assert!(!is_session_groups_enabled(Some(false)));
    }

    #[test]
    fn a_missing_value_means_off() {
        // 默认打开一个未验证的功能，代价远大于默认关闭。
        assert!(!is_session_groups_enabled(None));
    }

    #[test]
    fn a_release_build_never_enables_it() {
        // 构建类型硬闸：stable 上即便文件里写着 true，也不生效。
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        std::fs::write(&path, r#"{"version":1,"flags":{"session_groups":true}}"#).unwrap();
        assert!(resolve_from_store(&path, "dev"));
        assert!(!resolve_from_store(&path, "release"));
    }
}
