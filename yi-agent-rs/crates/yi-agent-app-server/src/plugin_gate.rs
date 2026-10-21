//! 插件"被自己的 flag 关掉"的判定。
//!
//! 与 `lead_gate` 同一手法：**只在构造点算一次，随实例下传**。不在这里调
//! `yi_agent_flags::status`——它读进程级 `YI_AGENT_HOME` 与 `build_kind`，
//! 而测试里 `build_kind` 恒为 `release`（只有 `yi-agent/src/main.rs` 注入过），
//! 所有 `plugin/query` 用例会被判成禁用。
//!
//! 宿主不认插件名：判定只依据清单里的 `switch_key`。

use std::path::Path;

use yi_agent_supervisors::manifest::load_manifests;
use yi_agent_supervisors::supervisor::Layout;

/// 这个清单的 flag 现在是不是开的。
///
/// `switch_key` 在当前清单格式里必填且非空（`ManifestError::EmptySwitchKey`），
/// 所以不需要 `Option` 分支。
pub fn flag_is_on(flags_path: &Path, switch_key: &str, build_kind: &str) -> bool {
    yi_agent_flags::store::status_at(flags_path, switch_key, build_kind, None).0
}

/// 这个插件在清单里声明的 flag 名。没有清单（未安装）→ `None`。
///
/// 没有清单时**不判定**：交给后面的 `plugin_unavailable` / `plugin_not_installed`
/// ——"没装"和"关着"是两件事，混在一起会让用户去翻设置页找一个不存在的开关。
pub fn switch_key_for(project: &Path, plugin: &str) -> Option<String> {
    let layout = Layout::for_workdir(project);
    load_manifests(&layout.manifests_dir())
        .into_iter()
        .find(|manifest| manifest.name == plugin)
        .map(|manifest| manifest.switch_key)
}

/// 判定所需的全部上下文，**装配时算一次**。
///
/// 两个字段都必须显式给出：`build_kind` 决定 dev/stable 的语义，
/// `flags_path` 决定读哪个文件。测试用临时目录构造同名结构即可，不会碰到
/// 开发者真实的 `$HOME/.yi-agent/flags.json`。
#[derive(Debug, Clone)]
pub struct Gate {
    pub build_kind: &'static str,
    pub flags_path: std::path::PathBuf,
}

impl Gate {
    /// 生产装配：进程级构建类型 + 进程级 flags 路径。
    pub fn from_process_environment() -> Option<Self> {
        Some(Self {
            build_kind: yi_agent_flags::build_kind(),
            flags_path: yi_agent_flags::store::flags_path()?,
        })
    }

    /// 这个插件现在是不是被自己的 flag 关着。`Some(switch_key)` = 关着。
    pub fn disabled(&self, project: &Path, plugin: &str) -> Option<String> {
        let switch_key = switch_key_for(project, plugin)?;
        (!flag_is_on(&self.flags_path, &switch_key, self.build_kind)).then_some(switch_key)
    }
}

/// 这个插件现在是不是被自己的 flag 关着（读进程级 flags 文件）。
///
/// 生产路径走 [`Gate::from_process_environment`]；保留本函数供不需要注入的
/// 调用方使用。测试请用 [`Gate`] + 临时目录。
pub fn is_disabled(project: &Path, plugin: &str, build_kind: Option<&str>) -> Option<String> {
    let build_kind = build_kind?;
    let flags_path = yi_agent_flags::store::flags_path()?;
    is_disabled_at(project, plugin, Some(build_kind), &flags_path)
}

/// [`is_disabled`] 的显式路径版本。
///
/// 测试**必须**走这个：生产实现读的是 `$HOME/.yi-agent/flags.json`，测试若
/// 直接调 `is_disabled` 就会读写开发者的真实开关文件，并行跑还会互相污染。
pub fn is_disabled_at(
    project: &Path,
    plugin: &str,
    build_kind: Option<&str>,
    flags_path: &Path,
) -> Option<String> {
    let build_kind = build_kind?;
    let switch_key = switch_key_for(project, plugin)?;
    (!flag_is_on(flags_path, &switch_key, build_kind)).then_some(switch_key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dev_build_honors_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        std::fs::write(
            &path,
            r#"{"version":1,"flags":{"superpowers_kanban":true}}"#,
        )
        .unwrap();
        assert!(flag_is_on(&path, "superpowers_kanban", "dev"));
    }

    #[test]
    fn a_stable_build_ignores_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        std::fs::write(
            &path,
            r#"{"version":1,"flags":{"superpowers_kanban":true}}"#,
        )
        .unwrap();
        assert!(
            !flag_is_on(&path, "superpowers_kanban", "release"),
            "stable must never let a dev flag through"
        );
    }

    #[test]
    fn a_missing_file_is_off() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(!flag_is_on(
            &dir.path().join("flags.json"),
            "superpowers_kanban",
            "dev"
        ));
    }

    #[test]
    fn an_unregistered_name_is_off() {
        // `read_at` 对未注册的名字一律返回 `None`：只有注册表里的才能开。
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        std::fs::write(&path, r#"{"version":1,"flags":{"not_registered":true}}"#).unwrap();
        assert!(!flag_is_on(&path, "not_registered", "dev"));
    }

    #[test]
    fn a_plugin_without_a_manifest_is_not_judged() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(
            switch_key_for(dir.path(), "nobody"),
            None,
            "an uninstalled plugin has no manifest, so it has no switch to be off"
        );
    }

    #[test]
    fn a_missing_build_kind_never_judges() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(
            is_disabled(dir.path(), "nobody", None),
            None,
            "no build kind means the caller has nothing to judge with; guessing \
             release would silently disable everything in tests"
        );
    }
}
