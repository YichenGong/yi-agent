//! 上游依赖就绪诊断：缺依赖**不启动自动任务**，但允许看数据。
//!
//! # 为什么这不能是"整个插件不可用"
//!
//! 设计 §7.2：看板是上游工作流的消费者。缺 `superpowers` 这套 skill 时，用户
//! 仍然要能看见队列、看见卡跑到哪、把卡归档——这些都不需要上游。真正不能做的
//! 只有一件：**替用户起一个自动会话**，因为那个会话一开局就要用上游流程，
//! 起来也是白烧一次模型回合。
//!
//! 所以诊断返回的是一张**分项清单**，而不是一个布尔：
//! [`Readiness::blocks_automatic_tasks`] 只挡住 `Launch`，
//! [`Readiness::view_only_reason`] 给页面一句能显示的话。把两者合并成一个
//! `enabled: false` 会让"看看队列"也一起消失，那是把一个依赖问题放大成一次
//! 停电。
//!
//! # 声明来自清单，探测只认声明
//!
//! 必需项来自 `plugin.json` 的 `dependencies.workflows[]` 与 `skills[]`——
//! 不是这里硬编码的名字。插件声明的依赖由发布包决定，本模块只负责回答
//! "声明的东西现在在不在"。新增一个依赖只需改清单，不需要改这份代码。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 清单里声明的一项上游依赖。
///
/// 与 `plugins/contracts/manifest-v1.schema.json` 的 `dependencies.workflows[]`
/// 同形：`range` 缺省表示"任何版本都算满足"。普通文件依赖用能力/命令契约表达，
/// 不写成编造的 semver（G3 §7.2），所以这里**不做**版本比较——`range` 只作为
/// 诊断里的事实回显，判断依据是"这个名字的技能根在不在"。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredWorkflow {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<String>,
}

/// 一项依赖的探测结论。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DependencyStatus {
    /// 声明的名字。
    pub name: String,
    /// 声明的版本范围（没有就是 `None`，表示 any）。
    pub range: Option<String>,
    /// 找到它的地方；`None` 表示没找到。
    pub found_at: Option<PathBuf>,
}

impl DependencyStatus {
    pub fn is_satisfied(&self) -> bool {
        self.found_at.is_some()
    }
}

/// 一次就绪检查的完整结论。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Readiness {
    pub dependencies: Vec<DependencyStatus>,
    /// 同名技能在多个根里各有一份时登记的冲突（路径 + 各自的根）。
    ///
    /// 设计 §7.2：冲突要显示路径与来源，不覆盖用户文件；冲突解决前阻止自动
    /// 任务。这里只**报告**，一个字节都不改——覆盖用户的自定义 skill 是不可
    /// 撤销的损失。
    pub conflicts: Vec<SkillConflict>,
}

/// 一个同名技能的多份副本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillConflict {
    pub name: String,
    /// 每一份的落点，按探测顺序（项目层在前）。
    pub paths: Vec<PathBuf>,
}

impl Readiness {
    /// 一个依赖都不缺、也没有同名冲突。
    pub fn is_ready(&self) -> bool {
        self.dependencies.iter().all(|dep| dep.is_satisfied()) && self.conflicts.is_empty()
    }

    /// 缺的依赖名（按声明顺序）。
    pub fn missing(&self) -> Vec<&str> {
        self.dependencies
            .iter()
            .filter(|dep| !dep.is_satisfied())
            .map(|dep| dep.name.as_str())
            .collect()
    }

    /// 是否必须挡住自动任务。
    ///
    /// 这是**唯一**该被挡的东西：只读看板、归档、改设置都不经过这里。
    pub fn blocks_automatic_tasks(&self) -> bool {
        !self.is_ready()
    }

    /// 给页面的一句话诊断。`None` 表示一切就绪。
    ///
    /// 措辞里必须同时出现"缺什么"和"看板还能看"：只说"不可用"会让用户以为
    /// 数据也打不开了，而那正是这条设计要避免的误解。
    pub fn view_only_reason(&self) -> Option<String> {
        if self.is_ready() {
            return None;
        }
        let mut parts = Vec::new();
        let missing = self.missing();
        if !missing.is_empty() {
            parts.push(format!("missing upstream skill(s): {}", missing.join(", ")));
        }
        if !self.conflicts.is_empty() {
            let names: Vec<&str> = self.conflicts.iter().map(|c| c.name.as_str()).collect();
            parts.push(format!("duplicate skill(s): {}", names.join(", ")));
        }
        Some(format!(
            "{}; the board is view-only until this is resolved and no new card will be launched",
            parts.join("; ")
        ))
    }
}

/// 技能根，带来源标签（与宿主 `skill_rpc::roots` 同一批目录）。
///
/// 顺序即优先级：项目层覆盖用户层。冲突判定用**同一批根**，否则插件说
/// "没有冲突"而宿主说"有两份"，用户会看到两个互相矛盾的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRoot {
    pub path: PathBuf,
    pub scope: &'static str,
}

/// 按宿主 `skill_rpc::roots` 的同一布置列出三个根。
///
/// `home` 是**实例根的父目录**（`yi_agent_store::paths::instance_root()`），
/// 不是 `$HOME` 本身：实例根可能是 `~/.yi-agent-isolated`，拼错一层就会去
/// 用户的真实目录里找一个不存在的技能。
pub fn skill_roots(project: &Path, home: &Path) -> Vec<SkillRoot> {
    let user = home.join(".yi-agent").join("skills");
    vec![
        SkillRoot {
            path: project.join(".yi-agent").join("skills"),
            scope: "project",
        },
        SkillRoot {
            path: user.clone(),
            scope: "user",
        },
        SkillRoot {
            path: user.join(".system"),
            scope: "system",
        },
    ]
}

/// 探测声明过的依赖现在满不满足。
///
/// 一个技能算"在"的判据是 `<root>/<name>/SKILL.md` 存在——与宿主
/// `parse_skill_md` 的入口同一口径。只看目录存在会让一个空目录冒充"已安装"。
pub fn check(declared: &[DeclaredWorkflow], project: &Path, home: &Path) -> Readiness {
    let roots = skill_roots(project, home);
    let dependencies: Vec<DependencyStatus> = declared
        .iter()
        .map(|declared| {
            let found_at = roots
                .iter()
                .map(|root| root.path.join(&declared.name))
                .find(|dir| dir.join("SKILL.md").is_file());
            DependencyStatus {
                name: declared.name.clone(),
                range: declared.range.clone(),
                found_at,
            }
        })
        .collect();

    // 冲突只对**声明过的**名字判定。一个用户自己装的无关技能重名不是本插件的
    // 问题，报出来只会让诊断噪音盖过真正的阻塞项。
    let conflicts = declared
        .iter()
        .filter_map(|declared| {
            let paths: Vec<PathBuf> = roots
                .iter()
                .map(|root| root.path.join(&declared.name))
                .filter(|dir| dir.join("SKILL.md").is_file())
                .collect();
            (paths.len() > 1).then(|| SkillConflict {
                name: declared.name.clone(),
                paths,
            })
        })
        .collect();

    Readiness {
        dependencies,
        conflicts,
    }
}

/// 从清单原文读出声明的工作流依赖。
///
/// 解析失败**不是**"没有依赖"：把一份读不懂的清单当成空依赖，会让插件在
/// 缺依赖的情况下照样启动自动任务——那是这条诊断唯一要防的事。所以失败
/// 返回 `Err`，由调用方决定是拒绝启动还是降级为只读。
pub fn declared_workflows(manifest: &str) -> Result<Vec<DeclaredWorkflow>, String> {
    let value: serde_json::Value =
        serde_json::from_str(manifest).map_err(|error| format!("manifest is not JSON: {error}"))?;
    let workflows = value
        .get("dependencies")
        .and_then(|dependencies| dependencies.get("workflows"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "manifest has no dependencies.workflows array".to_string())?;

    workflows
        .iter()
        .map(|entry| {
            let name = entry
                .get("name")
                .and_then(serde_json::Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| "a declared workflow has no name".to_string())?;
            let range = entry
                .get("range")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            Ok(DeclaredWorkflow {
                name: name.to_string(),
                range,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install(root: &Path, name: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "---\nname: x\n---\n").unwrap();
    }

    fn declared(names: &[&str]) -> Vec<DeclaredWorkflow> {
        names
            .iter()
            .map(|name| DeclaredWorkflow {
                name: (*name).to_string(),
                range: None,
            })
            .collect()
    }

    #[test]
    fn a_missing_declared_skill_blocks_automatic_tasks_but_stays_reportable() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(home.join(".yi-agent/skills")).unwrap();

        let readiness = check(&declared(&["brainstorming"]), &project, &home);
        assert!(readiness.blocks_automatic_tasks());
        assert_eq!(readiness.missing(), vec!["brainstorming"]);
        // 诊断必须能说清"缺什么"且指明"还能看"。
        let reason = readiness.view_only_reason().expect("a reason");
        assert!(reason.contains("brainstorming"), "{reason}");
        assert!(reason.contains("view-only"), "{reason}");
    }

    #[test]
    fn a_present_declared_skill_allows_automatic_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        let home = dir.path().join("home");
        install(&home.join(".yi-agent/skills"), "brainstorming");

        let readiness = check(&declared(&["brainstorming"]), &project, &home);
        assert!(!readiness.blocks_automatic_tasks());
        assert!(readiness.view_only_reason().is_none());
    }

    #[test]
    fn a_project_level_copy_satisfies_the_declaration() {
        // 宿主三层根里项目层优先；只在项目层装过也要算满足。
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        let home = dir.path().join("home");
        install(&project.join(".yi-agent/skills"), "brainstorming");

        let readiness = check(&declared(&["brainstorming"]), &project, &home);
        assert!(readiness.dependencies[0].is_satisfied());
    }

    #[test]
    fn an_empty_directory_does_not_count_as_installed() {
        // 只看目录存在会让一个空壳冒充"已安装"，然后会话开局就找不到 SKILL.md。
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".yi-agent/skills/brainstorming")).unwrap();

        let readiness = check(&declared(&["brainstorming"]), &project, &home);
        assert!(!readiness.dependencies[0].is_satisfied());
    }

    #[test]
    fn a_duplicate_skill_is_reported_with_every_path_and_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        let home = dir.path().join("home");
        install(&project.join(".yi-agent/skills"), "brainstorming");
        install(&home.join(".yi-agent/skills"), "brainstorming");
        let user_copy = home.join(".yi-agent/skills/brainstorming/SKILL.md");
        let before = std::fs::read(&user_copy).unwrap();

        let readiness = check(&declared(&["brainstorming"]), &project, &home);
        assert_eq!(readiness.conflicts.len(), 1);
        assert_eq!(readiness.conflicts[0].paths.len(), 2);
        assert!(readiness.blocks_automatic_tasks());
        assert!(readiness.view_only_reason().unwrap().contains("duplicate"));
        // 用户文件字节未变。
        assert_eq!(std::fs::read(&user_copy).unwrap(), before);
    }

    #[test]
    fn an_undeclared_duplicate_is_not_reported() {
        // 用户自己装的无关技能重名不是本插件的阻塞项；报出来会淹没真正的缺失。
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        let home = dir.path().join("home");
        install(&project.join(".yi-agent/skills"), "some-other-skill");
        install(&home.join(".yi-agent/skills"), "some-other-skill");

        let readiness = check(&declared(&["brainstorming"]), &project, &home);
        assert!(readiness.conflicts.is_empty());
    }

    #[test]
    fn an_unreadable_manifest_is_an_error_not_an_empty_dependency_list() {
        // 读不懂的清单当成"没有依赖"，等于在缺依赖时照常启动自动任务。
        assert!(declared_workflows("{ not json").is_err());
        assert!(declared_workflows("{}").is_err());
    }

    #[test]
    fn the_shipped_manifest_declares_no_invented_version_ranges() {
        // G3 §7.2：普通文件依赖用能力/命令契约表达，不写编造的 semver。
        let manifest = include_str!("../../../plugin.json");
        let declared = declared_workflows(manifest).expect("the shipped manifest parses");
        for workflow in declared {
            assert!(
                workflow.range.is_none(),
                "`{}` declares a version range; a plain skill dependency must be expressed \
                 as a capability, not a fabricated semver",
                workflow.name
            );
        }
    }

    #[test]
    fn the_skill_roots_match_the_hosts_three_layers() {
        // 与 `yi-agent-app-server::skill_rpc::roots` 同一布置；不一致会让插件
        // 说"没装"而宿主说"装了两份"。
        let roots = skill_roots(Path::new("/p"), Path::new("/h"));
        assert_eq!(
            roots.iter().map(|r| r.scope).collect::<Vec<_>>(),
            vec!["project", "user", "system"]
        );
        assert_eq!(roots[0].path, PathBuf::from("/p/.yi-agent/skills"));
        assert_eq!(roots[1].path, PathBuf::from("/h/.yi-agent/skills"));
        assert_eq!(roots[2].path, PathBuf::from("/h/.yi-agent/skills/.system"));
    }
}
