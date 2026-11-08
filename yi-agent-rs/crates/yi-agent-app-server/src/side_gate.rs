//! 旁问功能的开关判定。
//!
//! 形状与 [`crate::lead_gate`] 一致：取值链复用 `yi-agent-flags`，但**不**读进程
//! 级全局量。区别在于旁问必须**运行中可关**，所以生产侧提供的是每次调用都重读
//! 的 [`from_process_environment`]，而不是装配时算一次的 `bool`。
//!
//! 不叫 `is_enabled` 的那个入口（[`resolve_from_store`]）是为了让测试能把
//! `flags.json` 指到 tempdir：`yi_agent_flags::is_enabled` 读的是进程级
//! `YI_AGENT_HOME`，并行测试互相污染。

use std::path::Path;
use std::sync::Arc;

/// 旁问开关的名字。与注册表同源：`the_flag_name_matches_the_registry` 钉住一致。
pub const SIDE_CONVERSATION_FLAG: &str = "side_conversation";

/// 每请求判定旁问是否可用。
///
/// 是闭包而不是 `bool`：用户的开关值可能在运行中改变，装配时算一次的快照会让
/// 「关掉开关」在已运行的宿主上完全无效——那正是本功能要消除的状态。
pub type SideGate = Arc<dyn Fn() -> bool + Send + Sync>;

/// 从 `flags.json` + 构建类型解析生效值。**不读环境变量**：那是进程级的，会让
/// 并行测试互相污染（与 `lead_gate::resolve_from_store` 同一理由）。
pub fn resolve_from_store(flags_path: &Path, build_kind: &str) -> bool {
    matches!(
        yi_agent_flags::store::status_at(flags_path, SIDE_CONVERSATION_FLAG, build_kind, None),
        (true, _)
    )
}

/// 生产入口：每次调用都从真实环境重读。
///
/// 这里**刻意**读环境变量（与 `resolve_from_store` 相反）：生产进程里那正是
/// `YI_AGENT_FLAG_SIDE_CONVERSATION` 这条临时覆盖的入口。
pub fn from_process_environment() -> bool {
    yi_agent_flags::is_enabled(SIDE_CONVERSATION_FLAG)
}

/// 判定入口。`None` = 装配方漏传，**失败关闭**。
pub fn is_enabled(flag_value: Option<bool>) -> bool {
    flag_value == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_name_matches_the_registry() {
        assert!(
            yi_agent_flags::find(SIDE_CONVERSATION_FLAG).is_some(),
            "the gate and the registry must agree on the name"
        );
    }

    #[test]
    fn an_injected_value_wins() {
        assert!(is_enabled(Some(true)));
        assert!(!is_enabled(Some(false)));
    }

    #[test]
    fn a_missing_value_means_off() {
        assert!(!is_enabled(None), "a missing value must fail closed");
    }

    #[test]
    fn a_release_build_never_enables_it() {
        // 硬闸的鉴别测试：文件里写着 true，release 也必须关。
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        yi_agent_flags::write_at(&path, SIDE_CONVERSATION_FLAG, true).unwrap();
        assert!(
            !resolve_from_store(&path, "release"),
            "release must hard-close every flag"
        );
    }

    #[test]
    fn a_dev_build_follows_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        assert!(!resolve_from_store(&path, "dev"), "absent = off");
        yi_agent_flags::write_at(&path, SIDE_CONVERSATION_FLAG, true).unwrap();
        assert!(resolve_from_store(&path, "dev"));
        yi_agent_flags::write_at(&path, SIDE_CONVERSATION_FLAG, false).unwrap();
        assert!(!resolve_from_store(&path, "dev"));
    }
}
