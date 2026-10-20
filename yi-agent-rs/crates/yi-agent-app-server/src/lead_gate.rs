//! 组长功能的开关判定。
//!
//! 组长是一整套跨层能力（7 个工具、1 个 RPC、惰性建会话、后台 tick）。它们必须
//! 由**同一个**判定控制——散落各处各写一遍 `is_enabled("lead_agent")` 时，抄错
//! 一个字符串就是静默失效，而且没有任何测试会失败。
//!
//! 判定**不读进程级全局量**：`yi_agent_flags::is_enabled` 依赖 `YI_AGENT_HOME`
//! 与进程级 `build_kind`，在并行测试里会互相污染。真实取值在 `serve()` 入口由
//! [`resolve_from_store`] 解析一次，之后随实例向下传。

use std::path::Path;

/// 组长总开关的名字。与注册表同源：`the_flag_name_matches_the_registry` 钉住一致。
pub const LEAD_AGENT_FLAG: &str = "lead_agent";

/// 从 `flags.json` + 构建类型解析生效值。**每个实例调用一次**（`serve()` 入口）。
///
/// 不叫 `is_enabled` 是为了避免与 `yi_agent_flags::is_enabled` 混淆：后者读进程
/// 级 `YI_AGENT_HOME`，本函数只读调用方给的路径。
pub fn resolve_from_store(flags_path: &Path, build_kind: &str) -> bool {
    // 复用库的取值链（默认 → 文件 → 构建类型硬闸），但不走它的环境变量层：
    // 环境变量同样是进程级的，会让并行测试互相污染。生产侧若要临时覆盖，
    // 在 `serve()` 入口自己读环境变量。
    matches!(
        yi_agent_flags::store::status_at(flags_path, LEAD_AGENT_FLAG, build_kind, None),
        (true, _)
    )
}

/// 判定入口。`None` = 装配方漏传，**失败关闭**。
///
/// 失败关闭是刻意的：默认打开一个未验证的功能，代价远大于默认关闭。
pub fn is_lead_agent_enabled(flag_value: Option<bool>) -> bool {
    flag_value == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_injected_value_wins() {
        assert!(is_lead_agent_enabled(Some(true)));
        assert!(!is_lead_agent_enabled(Some(false)));
    }

    #[test]
    fn a_missing_value_means_off() {
        // 生产路径永远显式给 `Some`；`None` 是「装配方漏传」的兜底，
        // 必须失败关闭，不能默认打开一个未验证的功能。
        assert!(!is_lead_agent_enabled(None));
    }

    #[test]
    fn the_flag_name_matches_the_registry() {
        assert_eq!(LEAD_AGENT_FLAG, "lead_agent");
        assert!(
            yi_agent_flags::find(LEAD_AGENT_FLAG).is_some(),
            "the gate and the registry must agree on the name"
        );
    }

    #[test]
    fn a_stable_build_always_reports_off() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        std::fs::write(&path, r#"{"version":1,"flags":{"lead_agent":true}}"#).unwrap();
        assert!(
            !resolve_from_store(&path, "release"),
            "a stable build must ignore the file entirely"
        );
        assert!(
            resolve_from_store(&path, "dev"),
            "a dev build honors an explicit on"
        );
    }

    #[test]
    fn a_dev_build_without_the_file_reports_off() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        assert!(!resolve_from_store(&path, "dev"));
    }
}
