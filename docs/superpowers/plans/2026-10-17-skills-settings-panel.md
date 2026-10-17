# 设置页 Skill 面板 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在设置页新增「技能」分类，按项目/用户/系统三层列出所有已安装 skill，点开可读其 `SKILL.md` 正文，并说明如何添加 skill；手机端顺带把设置页从 Tab 横条改成「分类列表 → 点进内容 → 返回」两级导航。

**Architecture:** 后端新增 `skill_rpc.rs`（纯函数，对齐 `model_rpc.rs` 的「生产包装 + 可注入核心」形态），经 `server.rs` 方法表以两个只读 RPC（`skill/list`、`skill/read`，均 `Observe` 档）暴露；`home` 经 `RuntimeAttachments.skills_home` 注入，测试绝不碰真实 `~/.yi-agent`。前端新增 `SettingsSkillsTab`，结构照 `SettingsPluginsTab`（含抽出的共享项目选择器），正文复用既有 `MarkdownText`。手机端导航解耦：桌面仍是竖栏 `role="tablist"`，手机端渲染分类列表与子页，外观覆写全部留在 `index.css` 的 `[data-mobile="true"]` 前缀规则里。

**Tech Stack:** Rust（tokio、serde_json、walkdir 经 `yi-agent-skills`）、React 19 + TypeScript、Tailwind CSS v4、Vitest + @testing-library/react（jsdom）。

**Spec:** `docs/superpowers/specs/2026-10-17-skills-settings-panel-design.md`

## Global Constraints

- **禁止在 `main` 分支改动**：全部工作在 worktree `.worktrees/skills-settings-panel`（分支 `docs/skills-settings-panel`，已由 spec 阶段创建）内执行。
- **桌面零回归**：桌面端导航仍是 `.settings-nav` 竖栏 `role="tablist"`、`aria-orientation="vertical"`、roving tabIndex、方向键/Home/End 导航。桌面端 CSS 规则一律不改（新增「技能」Tab 本身除外）。既有 `SettingsDialog.test.tsx`、`SettingsPluginsTab.test.tsx`、`SettingsModelsTab.test.tsx` 用例必须全绿。
- **手机端不再渲染 tablist**：这是本次唯一破坏性变更，3 处既有断言必须改写（见 Task 7）。
- **两个 RPC 都只加进 `Observe` 档**，**不得**加入 `ADMIN_METHODS`（iOS 设备默认 `Control`，加闸即手机端不可用）。
- **`skill/read` 必须限根**：文件名须为 `SKILL.md`，且 `canonicalize` 后落在三类合法根之一以内，否则 `-32602`。
- **测试不得触碰真实 `~/.yi-agent`**：后端一律用 tempdir 注入。
- 所有注释与文案用中文，风格与仓库现有注释一致（说清「为什么」）。
- 提交前在 `yi-agent-rs/` 跑 `cargo fmt --all`。
- commit message 用 conventional commits，**不写** `Co-Authored-By` 行。
- 跑 cargo 前先 `ps aux | grep cargo` 确认无残留进程；不并行跑多个 worktree 的测试。
- 前端命令一律用 `export PATH=/opt/homebrew/bin:$PATH`（本机 node 在 `/opt/homebrew/bin`）。

---

### Task 1: `skill_rpc.rs` —— 列表与读取核心

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/skill_rpc.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`（加 `pub mod skill_rpc;`）

**Interfaces:**
- Consumes: `yi_agent_skills::{SkillsService, SkillMetadata, SkillScope, install_system_skills}`；`crate::protocol::RpcError`。
- Produces: `pub fn handle_skill_request_at(host_workdir: &Path, home: &Path, method: &str, params: &Value) -> Result<Value, RpcError>`。Task 2 只调这一个函数。

- [ ] **Step 1: 写失败测试**

创建 `yi-agent-rs/crates/yi-agent-app-server/src/skill_rpc.rs`，**只写模块文档、`use` 与下方测试**（实现留空，编译会先失败——这是预期的红灯）：

```rust
//! `skill/*` RPC：设置页「技能」面板的只读接缝。
//!
//! 与 `plugins/list` 同类：**按项目解析根目录**，缺省回落宿主 workdir。差别在于
//! 它读的是 skill 三层根（项目 / 用户 / 系统），而不是插件清单。
//!
//! 纯函数、无进程内状态：宿主 workdir 与 `home` 都由调用方注入。之所以不让测试
//! 改 `HOME`：cargo 并行跑测试，进程全局变量会让用例互相竞态（与 `model_rpc` 同源
//! 理由）。
//!
//! 权限档位：两个方法都只需 `Observe`——只读、不碰凭据、不落盘，与 `config/read`、
//! `plugins/list` 同档。iOS 设备默认 `Control`，故手机端可用。

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use yi_agent_skills::{SkillMetadata, SkillScope, SkillsService};

use crate::protocol::RpcError;

/// 项目/用户/系统三层的固定顺序，响应与前端都按它渲染。
const SCOPES: [(&str, SkillScope); 3] = [
    ("project", SkillScope::Project),
    ("user", SkillScope::User),
    ("system", SkillScope::System),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// 造一个 skill 目录：`<root>/<name>/SKILL.md`。
    fn make_skill(root: &Path, name: &str, desc: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("SKILL.md");
        fs::write(
            &path,
            format!("---\nname: {name}\ndescription: {desc}\n---\n# {name}\n\n正文"),
        )
        .unwrap();
        path
    }

    /// 隔离的「家目录」：`<home>/.yi-agent/skills`（用户层）与 `.system`（系统层）。
    struct Fixture {
        _tmp: TempDir,
        home: PathBuf,
        project: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path().join("home");
            let project = tmp.path().join("proj");
            fs::create_dir_all(home.join(".yi-agent/skills")).unwrap();
            fs::create_dir_all(project.join(".yi-agent/skills")).unwrap();
            Self { _tmp: tmp, home, project }
        }

        fn user_root(&self) -> PathBuf {
            self.home.join(".yi-agent/skills")
        }

        fn project_root(&self) -> PathBuf {
            self.project.join(".yi-agent/skills")
        }

        fn system_root(&self) -> PathBuf {
            self.user_root().join(".system")
        }

        /// 装上系统内置 skill（走真实安装路径，与 `skill/list` 同一条）。
        fn install_system(&self) {
            yi_agent_skills::install_system_skills(&self.system_root()).unwrap();
        }

        fn list(&self, project: Option<&Path>) -> Value {
            let params = match project {
                Some(p) => json!({ "project": p.to_string_lossy() }),
                None => json!({}),
            };
            handle_skill_request_at(&self.project, &self.home, "skill/list", &params).unwrap()
        }

        /// 某一层里某个 skill 是否在（按 scope 找组，不按下标——空组会被丢掉，
        /// 下标不稳；系统组还总有内置的 3 个）。
        fn has_skill(&self, project: Option<&Path>, scope: &str, name: &str) -> bool {
            self.list(project)["groups"]
                .as_array()
                .unwrap()
                .iter()
                .find(|g| g["scope"] == scope)
                .map(|g| {
                    g["skills"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|s| s["name"] == name)
                })
                .unwrap_or(false)
        }

        /// 某层是否存在（用于断言「根重合时不输出项目组」）。
        fn has_scope(&self, project: Option<&Path>, scope: &str) -> bool {
            self.list(project)["groups"]
                .as_array()
                .unwrap()
                .iter()
                .any(|g| g["scope"] == scope)
        }
    }

    #[test]
    fn lists_all_three_scopes() {
        let f = Fixture::new();
        make_skill(&f.project_root(), "proj-skill", "项目级。");
        make_skill(&f.user_root(), "user-skill", "用户级。");
        make_skill(&f.system_root(), "sys-skill", "系统级。");

        let listed = f.list(Some(&f.project));
        let scopes: Vec<&str> = listed["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["scope"].as_str().unwrap())
            .collect();
        assert_eq!(scopes, vec!["project", "user", "system"]);
        assert!(f.has_skill(Some(&f.project), "project", "proj-skill"));
        assert!(f.has_skill(Some(&f.project), "user", "user-skill"));
        // 系统组里除自造的这一个，还有 list 路径幂等装上的内置 3 个，故只断言包含。
        assert!(f.has_skill(Some(&f.project), "system", "sys-skill"));
    }

    #[test]
    fn entry_carries_name_description_and_path() {
        let f = Fixture::new();
        let path = make_skill(&f.user_root(), "user-skill", "用户级。");
        let listed = f.list(Some(&f.project));
        // 按 scope 找组，不按下标：空组会被丢掉，下标不稳。
        let entry = listed["groups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["scope"] == "user")
            .expect("user group")["skills"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "user-skill")
            .expect("seeded skill");
        assert_eq!(entry["name"], "user-skill");
        assert_eq!(entry["description"], "用户级。");
        assert_eq!(entry["path"], path.to_string_lossy().as_ref());
        // 正文绝不随列表下发：点开才读（见 `skill/read`）。
        assert!(entry.get("body").is_none());
    }

    #[test]
    fn missing_project_falls_back_to_host_workdir() {
        let f = Fixture::new();
        make_skill(&f.project_root(), "proj-skill", "项目级。");
        // 不传 project → 用宿主 workdir（fixture 里就是 `project`）。
        assert!(f.has_skill(None, "project", "proj-skill"));
    }

    #[test]
    fn system_root_is_not_double_counted_by_the_user_scope() {
        // 依赖 `discover_skills` 跳过隐藏目录（`.system` 以点开头）：这条钉住该依赖，
        // 一旦 discovery 的 `is_hidden` 行为变化，用户层会把系统内置的 3 个重复列一遍。
        let f = Fixture::new();
        f.install_system();
        assert!(f.has_scope(Some(&f.project), "system"));
        assert!(!f.has_skill(Some(&f.project), "user", "skill-creator"));
    }

    #[test]
    fn a_fresh_home_lists_the_bundled_system_skills() {
        // 全新机器上用户还没开过任何对话：`skill/list` 自己必须把内置 skill 装上，
        // 否则「当前系统支持了哪些 skill」这个首要问题答不出来。
        let f = Fixture::new();
        assert!(f.has_skill(Some(&f.project), "system", "skill-creator"));
        assert!(f.has_skill(Some(&f.project), "system", "skill-installer"));
        assert!(f.has_skill(Some(&f.project), "system", "git-diff-review"));
    }

    #[test]
    fn reinstalling_system_skills_is_idempotent() {
        let f = Fixture::new();
        f.install_system();
        let path = f.system_root().join("skill-creator/SKILL.md");
        let first = fs::metadata(&path).unwrap().modified().unwrap();
        // 再列一次：安装按 SHA-256 跳过同内容文件，mtime 不变。
        let _ = f.list(Some(&f.project));
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), first);
    }

    #[test]
    fn identical_roots_do_not_produce_a_project_group() {
        // 桌面侧车的 workdir 是 home：未指定 project 时三层里的「项目根」与「用户根」
        // 是同一个目录。此时必须判定「不存在项目层」——否则同一 skill 出现两次，
        // 且用户级 skill 会被错标成项目级。
        let f = Fixture::new();
        make_skill(&f.user_root(), "user-skill", "用户级。");
        let listed = handle_skill_request_at(&f.home, &f.home, "skill/list", &json!({})).unwrap();
        let scopes: Vec<&str> = listed["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["scope"].as_str().unwrap())
            .collect();
        assert!(!scopes.contains(&"project"), "{scopes:?}");
        // 只在用户层出现一次。（系统层仍有内置的 3 个——`skill/list` 会幂等安装它们，
        // 那是另一回事，与这里要钉的重合无关。）
        let names: Vec<&str> = listed["groups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["skills"].as_array().unwrap().iter())
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names.iter().filter(|n| **n == "user-skill").count(),
            1,
            "{names:?}"
        );
    }

    #[test]
    fn same_name_in_two_different_roots_stays_in_both_groups() {
        // 两个**不同**目录里的同名 skill 各归各层，不擅自合并——与 `render_catalog`
        // 的既有语义一致（其 `same_name_different_scope_both_listed` 用例钉住了这点）。
        let f = Fixture::new();
        make_skill(&f.project_root(), "dup", "项目级。");
        make_skill(&f.user_root(), "dup", "用户级。");
        assert!(f.has_skill(Some(&f.project), "project", "dup"));
        assert!(f.has_skill(Some(&f.project), "user", "dup"));
    }

    #[test]
    fn roots_are_reported_for_the_help_copy() {
        let f = Fixture::new();
        let listed = f.list(Some(&f.project));
        assert_eq!(
            listed["roots"]["user"],
            f.user_root().to_string_lossy().as_ref()
        );
        assert_eq!(
            listed["roots"]["system"],
            f.system_root().to_string_lossy().as_ref()
        );
        assert_eq!(
            listed["roots"]["project"],
            f.project_root().to_string_lossy().as_ref()
        );
    }

    #[test]
    fn read_returns_the_body_without_frontmatter() {
        let f = Fixture::new();
        let path = make_skill(&f.user_root(), "user-skill", "用户级。");
        let read = handle_skill_request_at(
            &f.project,
            &f.home,
            "skill/read",
            &json!({ "path": path.to_string_lossy() }),
        )
        .unwrap();
        assert_eq!(read["body"], "# user-skill\n\n正文");
    }

    #[test]
    fn read_refuses_a_file_that_is_not_skill_md() {
        let f = Fixture::new();
        let path = make_skill(&f.user_root(), "user-skill", "用户级。");
        let other = path.parent().unwrap().join("notes.md");
        fs::write(&other, "x").unwrap();
        let err = handle_skill_request_at(
            &f.project,
            &f.home,
            "skill/read",
            &json!({ "path": other.to_string_lossy() }),
        )
        .unwrap_err();
        assert_eq!(err.code, -32602);
    }

    #[test]
    fn read_refuses_a_path_outside_every_root() {
        let f = Fixture::new();
        let outside = f.home.join("secrets/SKILL.md");
        fs::create_dir_all(outside.parent().unwrap()).unwrap();
        fs::write(&outside, "---\nname: x\ndescription: y\n---\nsecret").unwrap();
        let err = handle_skill_request_at(
            &f.project,
            &f.home,
            "skill/read",
            &json!({ "path": outside.to_string_lossy() }),
        )
        .unwrap_err();
        assert_eq!(err.code, -32602);
    }

    #[test]
    fn read_refuses_a_dotdot_traversal() {
        let f = Fixture::new();
        make_skill(&f.user_root(), "user-skill", "用户级。");
        let sneaky = f
            .user_root()
            .join("user-skill/../../../../etc/SKILL.md")
            .to_string_lossy()
            .to_string();
        let err = handle_skill_request_at(
            &f.project,
            &f.home,
            "skill/read",
            &json!({ "path": sneaky }),
        )
        .unwrap_err();
        assert_eq!(err.code, -32602);
    }

    #[test]
    fn read_reports_a_missing_file_as_invalid_params() {
        let f = Fixture::new();
        let missing = f.user_root().join("nope/SKILL.md");
        let err = handle_skill_request_at(
            &f.project,
            &f.home,
            "skill/read",
            &json!({ "path": missing.to_string_lossy() }),
        )
        .unwrap_err();
        assert_eq!(err.code, -32602);
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let f = Fixture::new();
        let err = handle_skill_request_at(&f.project, &f.home, "skill/nope", &json!({}))
            .unwrap_err();
        assert_eq!(err.code, -32601);
    }
}
```

在 `lib.rs` 的 `pub mod model_rpc;` 之后加一行（保持字母序）：

```rust
pub mod skill_rpc;
```

- [ ] **Step 2: 跑测试确认失败**

```bash
export PATH=/opt/homebrew/bin:$PATH
ps aux | grep -v grep | grep cargo || true
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/skills-settings-panel/yi-agent-rs
cargo test -p yi-agent-app-server skill_rpc 2>&1 | tail -20
```

Expected: 编译失败，报 `cannot find function 'handle_skill_request_at'` 等（实现尚未写）。

- [ ] **Step 3: 写实现**

在 `skill_rpc.rs` 的 `use` 与 `const SCOPES` 之后、`#[cfg(test)]` 之前插入：

```rust
/// 生产入口：宿主 workdir 与真实 `home`。测试一律用 [`handle_skill_request_at`]。
pub fn handle_skill_request(
    host_workdir: &Path,
    home: &Path,
    method: &str,
    params: &Value,
) -> Result<Value, RpcError> {
    handle_skill_request_at(host_workdir, home, method, params)
}

/// 可测核心：宿主 workdir 与 `home` 均由调用方注入。
pub fn handle_skill_request_at(
    host_workdir: &Path,
    home: &Path,
    method: &str,
    params: &Value,
) -> Result<Value, RpcError> {
    match method {
        "skill/list" => list(host_workdir, home, params),
        "skill/read" => read(host_workdir, home, params),
        _ => Err(RpcError::method_not_found(method)),
    }
}

/// 三层根。顺序即 `SCOPES` 的顺序。
///
/// 系统根放在用户根**之内**（`.system`）而不是并列，是既有布局
/// （`bootstrap::build_skills_service`）；`discover_skills` 会跳过以点开头的目录，
/// 故用户层不会把系统内置的重复扫一遍。
fn roots(project: &Path, home: &Path) -> Vec<(PathBuf, SkillScope)> {
    let user = home.join(".yi-agent/skills");
    vec![
        (project.join(".yi-agent/skills"), SkillScope::Project),
        (user.clone(), SkillScope::User),
        (user.join(".system"), SkillScope::System),
    ]
}

/// 调用方给了非空 `project` 就用它，否则回落宿主 workdir——语义与 `plugin_scope` 一致，
/// 理由也一致：桌面侧车的 workdir 是 home，设置页必须能把作用域指到它要看的项目。
fn project_scope(host_workdir: &Path, params: &Value) -> PathBuf {
    params
        .get("project")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| host_workdir.to_path_buf())
}

/// canonicalize，失败则原样返回（路径不存在时仍要能比较出「不相等」）。
fn canonical_or_self(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn list(host_workdir: &Path, home: &Path, params: &Value) -> Result<Value, RpcError> {
    let project = project_scope(host_workdir, params);
    let roots = roots(&project, home);

    let project_root = canonical_or_self(&roots[0].0);
    let user_root = canonical_or_self(&roots[1].0);
    // 项目根与用户根是同一个目录 ⇒ 不存在项目层。桌面侧车（workdir = home）且未指定
    // project 时正是这种情况：不合并的话同一 skill 会出现两次，且用户级 skill 会被
    // 错标成项目级。
    let project_coincides_with_user = project_root == user_root;

    // 系统内置 skill 在 list 路径上幂等安装：全新机器、用户还没开过对话时也能列出，
    // 否则「当前系统支持了哪些 skill」答不出来。失败只告警——列表本身仍要可用，
    // 与 `build_skills_service` 的降级策略一致。
    let system_root = &roots[2].0;
    if let Err(error) = yi_agent_skills::install_system_skills(system_root) {
        tracing::warn!("skills: failed to install bundled skills: {error}");
    }

    let active: Vec<(PathBuf, SkillScope)> = roots
        .iter()
        .filter(|(_, scope)| !(project_coincides_with_user && *scope == SkillScope::Project))
        .cloned()
        .collect();

    // 直接构造 `SkillsService` 而不复用启动时那份：这里的根按**调用方的 project**
    // 解析，与进程启动时的 `cfg.workdir` 未必同一个。服务自带缓存，但本函数只调用
    // 一次，故一次性快照即可。
    let service = SkillsService::new(active.clone());
    let discovered = service.snapshot().map_err(|error| {
        RpcError::internal(format!("skill discovery failed: {error}"))
    })?;

    let groups: Vec<Value> = active
        .iter()
        .filter_map(|(root, scope)| {
            let name = SCOPES
                .iter()
                .find(|(_, s)| s == scope)
                .map(|(n, _)| *n)?;
            let mut skills: Vec<&SkillMetadata> = discovered
                .iter()
                .filter(|s| s.scope == *scope && s.path.starts_with(root))
                .collect();
            if skills.is_empty() {
                return None; // 空分组整组不输出
            }
            skills.sort_by(|a, b| (&a.name, &a.path).cmp(&(&b.name, &b.path)));
            Some(json!({
                "scope": name,
                "skills": skills
                    .into_iter()
                    .map(|s| json!({
                        "name": s.name,
                        // 描述可能被 loader 截断（上限 1024B）；面板只做展示。
                        "description": s.description,
                        "path": s.path.to_string_lossy(),
                    }))
                    .collect::<Vec<_>>(),
            }))
        })
        .collect();

    Ok(json!({
        "groups": groups,
        "roots": {
            "project": roots[0].0.to_string_lossy(),
            "user": roots[1].0.to_string_lossy(),
            "system": roots[2].0.to_string_lossy(),
        },
    }))
}

fn read(host_workdir: &Path, home: &Path, params: &Value) -> Result<Value, RpcError> {
    let raw = params
        .get("path")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| RpcError::invalid_params("path required"))?;

    let project = project_scope(host_workdir, params);
    let path = resolve_within(raw, &roots(&project, home))?;

    // 复用既有的正文读取（它会剥掉 frontmatter）。这里只读单个文本文件，故不需要
    // `image/read` 那套「先按元数据设闸再分片」的内存放大防护。
    let service = SkillsService::new(Vec::new());
    let body = service
        .load_skill_body(&path.to_string_lossy())
        .map_err(|error| RpcError::invalid_params(error.to_string()))?;
    Ok(json!({ "body": body }))
}

/// 把调用方给的路径收到合法 skill 根之内。
///
/// 网络面必须自己设限：`SkillsService::load_skill_body` 刻意不做校验（它的既定理由
/// 是「模型可能拼错路径，路径穿越风险与 ReadTool 等同」），那是**进程内工具**的语境。
/// 手法照抄既有先例 `server.rs` 的 `image/read`：`canonicalize` 后要求 `starts_with`
/// 某个根——`canonicalize` 会解掉 `..` 与符号链接，故逃逸一定现形。
fn resolve_within(raw: &str, roots: &[(PathBuf, SkillScope)]) -> Result<PathBuf, RpcError> {
    let candidate = PathBuf::from(raw);
    if candidate.file_name().and_then(|n| n.to_str()) != Some("SKILL.md") {
        return Err(RpcError::invalid_params(
            "path must name a SKILL.md file".to_string(),
        ));
    }
    let canonical = candidate.canonicalize().map_err(|error| {
        RpcError::invalid_params(format!("cannot read skill: {error}"))
    })?;
    let allowed = roots
        .iter()
        .any(|(root, _)| canonical.starts_with(canonical_or_self(root)));
    if !allowed {
        return Err(RpcError::invalid_params(
            "path escapes the skill roots".to_string(),
        ));
    }
    Ok(canonical)
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/skills-settings-panel/yi-agent-rs
cargo test -p yi-agent-app-server skill_rpc 2>&1 | tail -25
```

Expected: PASS，15 个用例全绿。

- [ ] **Step 5: 格式与警告检查**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/skills-settings-panel/yi-agent-rs
cargo fmt --all
cargo test -p yi-agent-app-server skill_rpc 2>&1 | tail -5
cargo clippy -p yi-agent-app-server 2>&1 | grep -E "^(warning|error)" | head -20
```

Expected: 测试 PASS；clippy 无**新增**警告（`project_coincides_with_user` 与 `canonical_or_self` 都必须真的被用到，不得留下未使用项）。

- [ ] **Step 6: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/skills-settings-panel
git add yi-agent-rs/crates/yi-agent-app-server/src/skill_rpc.rs yi-agent-rs/crates/yi-agent-app-server/src/lib.rs
git commit -m "feat(app-server): skill/list and skill/read RPC core

Read-only seam for the settings skills panel: three scoped roots
(project/user/system), a merged project group when the project root
coincides with the user root, bundled-skill install on the list path,
and path containment for the network-facing read."
```

---

### Task 2: 接进 `server.rs` 方法表与 `skills_home` 注入

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`RuntimeAttachments` 结构体、`run()`、`serve_stdio_with_relay()`、方法表、测试 harness）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`#[cfg(test)]` 内新增用例）

**Interfaces:**
- Consumes: Task 1 的 `handle_skill_request(host_workdir: &Path, home: &Path, method: &str, params: &Value) -> Result<Value, RpcError>`。
- Produces: RPC 方法 `skill/list`、`skill/read`；`RuntimeAttachments` 新字段 `skills_home: PathBuf`。

- [ ] **Step 1: 写失败测试**

在 `server.rs` 的 `#[cfg(test)] mod tests` 内，找 `plugin_scope` 那组测试（`fn plugin_scope_...`，约 8620 行）之后追加：

```rust
    /// 设置页「技能」面板的两个 RPC：三层分组、正文读取、越界拒绝，且都必须落在
    /// `Observe` 档（iOS 设备默认 Control，加闸即手机端不可用）。
    mod skill_rpc_tests {
        use super::*;

        /// 在隔离 home 里造一个用户级 skill，返回它的 SKILL.md 路径。
        fn seed_user_skill(h: &tests::Harness, name: &str) -> std::path::PathBuf {
            let dir = h.skills_home().join(".yi-agent/skills").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("SKILL.md");
            std::fs::write(
                &path,
                format!("---\nname: {name}\ndescription: 测试用。\n---\n# {name}\n\n正文"),
            )
            .unwrap();
            path
        }

        /// 握手后再发请求并取回该 id 的响应——未 initialize 的客户端除
        /// `initialize` 外一律被拒，少了这一步所有用例都会红在 `-32010`。
        async fn call(h: &mut tests::Harness, id: i64, method: &str, params: &str) -> serde_json::Value {
            h.send(&format!(
                r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#
            ))
            .await;
            tests::read_response(h, id).await
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn skill_list_is_available_at_observe_scope() {
            let mut h = tests::Harness::with_scope(Scope::Observe).await;
            tests::initialize(&mut h).await;
            let resp = call(&mut h, 1, "skill/list", "{}").await;
            assert!(resp.get("error").is_none(), "observe must list skills: {resp}");
            assert!(resp["result"]["groups"].is_array());
            h.shutdown().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn skill_read_is_available_at_observe_scope() {
            let mut h = tests::Harness::with_scope(Scope::Observe).await;
            let path = seed_user_skill(&h, "user-skill");
            tests::initialize(&mut h).await;
            let resp = call(
                &mut h,
                2,
                "skill/read",
                &format!(r#"{{"path":"{}"}}"#, path.to_string_lossy()),
            )
            .await;
            assert!(resp.get("error").is_none(), "observe must read skills: {resp}");
            h.shutdown().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn skill_list_groups_the_installed_skills() {
            let mut h = tests::Harness::new();
            seed_user_skill(&h, "user-skill");
            tests::initialize(&mut h).await;
            let resp = call(&mut h, 3, "skill/list", "{}").await;
            let groups = resp["result"]["groups"].as_array().unwrap();
            let user = groups
                .iter()
                .find(|g| g["scope"] == "user")
                .expect("user group present");
            assert!(user["skills"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["name"] == "user-skill"));
            h.shutdown().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn skill_read_returns_the_body() {
            let mut h = tests::Harness::new();
            let path = seed_user_skill(&h, "user-skill");
            tests::initialize(&mut h).await;
            let resp = call(
                &mut h,
                4,
                "skill/read",
                &format!(r#"{{"path":"{}"}}"#, path.to_string_lossy()),
            )
            .await;
            assert!(resp.get("error").is_none(), "read failed: {resp}");
            assert_eq!(resp["result"]["body"], "# user-skill\n\n正文");
            h.shutdown().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn skill_read_refuses_a_path_outside_the_roots() {
            let mut h = tests::Harness::new();
            let outside = h.skills_home().join("outside/SKILL.md");
            std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
            std::fs::write(&outside, "---\nname: x\ndescription: y\n---\nsecret").unwrap();
            tests::initialize(&mut h).await;
            let resp = call(
                &mut h,
                5,
                "skill/read",
                &format!(r#"{{"path":"{}"}}"#, outside.to_string_lossy()),
            )
            .await;
            assert_eq!(resp["error"]["code"], -32602, "{resp}");
            h.shutdown().await;
        }
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
export PATH=/opt/homebrew/bin:$PATH
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/skills-settings-panel/yi-agent-rs
cargo test -p yi-agent-app-server skill_rpc_tests 2>&1 | tail -20
```

Expected: 编译失败 —— `no method named 'skills_home' found for struct 'Harness'`，以及 `skill/list` 目前会回 `-32601`。

- [ ] **Step 3: 给 `RuntimeAttachments` 加 `skills_home` 字段**

在 `RuntimeAttachments` 的 `pub(crate) plugin_command: PathBuf` 之前插入：

```rust
    /// 拼 skill 根目录用的「家目录」。Injectable for the same reason `board_dir`
    /// is: `skill/list` / `skill/read` must be testable without reading the
    /// developer's real `~/.yi-agent/skills`.
    pub(crate) skills_home: PathBuf,
```

在 `run()` 里 `plugin_command:` 那一行之前插入（生产填真实 home）：

```rust
            skills_home: home_dir(),
```

在 `serve_stdio_with_relay()` 里同样位置插入一行：

```rust
            skills_home: home_dir(),
```

- [ ] **Step 4: 加方法表分支**

在 `"plugins/list"` 分支**之前**插入（与既有只读 RPC 相邻，保持方法表的可读顺序）：

```rust
                    // 设置页「技能」面板：枚举三层 skill 根 + 按需读正文。只读，
                    // 故与 `config/read` / `plugins/list` 同档只需 `Observe`——
                    // iOS 设备默认 Control，加闸即手机端结构性不可用。
                    "skill/list" | "skill/read" => {
                        match crate::skill_rpc::handle_skill_request(
                            theme_handle.workdir(),
                            &skills_home,
                            method.as_str(),
                            &req.params,
                        ) {
                            Ok(value) => {
                                write_response(&hub, &client, ok_response(id, value)).await?
                            }
                            Err(error) => {
                                write_response(&hub, &client, err_response(id, error)).await?
                            }
                        }
                    }
```

`skills_home` 需要从 `RuntimeAttachments` 解构出来。在该函数已有的解构块里（`let RuntimeAttachments { ... } = attachments;` 那一处，含 `plugin_command,` 等字段）加一行：

```rust
        skills_home,
```

> 若该函数未用解构块而逐字段访问，则改为在方法表前加 `let skills_home = attachments.skills_home.clone();`。以编译结果为准。

- [ ] **Step 5: 测试 harness 注入 tempdir 并暴露句柄**

在 `Harness` 结构体里 `pub(crate) _watchman_home: tempfile::TempDir,` 之后加：

```rust
        /// 隔离的「家目录」：`skill/list` / `skill/read` 从这里派生三层根，绝不读
        /// 开发者真实的 `~/.yi-agent/skills`。字段同时用于持有 tempdir。
        pub(crate) skills_home_dir: tempfile::TempDir,
```

在 `Harness::with_config_and_scope_and_watchman`（约 10673 行，三层构造链的最后一层）里，`let watchman_home = tempfile::TempDir::new().unwrap();` 之后加：

```rust
            // skill 根同样落在隔离目录里:`skill/list` 不得枚举开发者真实的
            // `~/.yi-agent/skills`。
            let skills_home = tempfile::TempDir::new().unwrap();
```

在 `RuntimeAttachments { ... }` 字面量里 `plugin_command:` 之前加：

```rust
                    skills_home: skills_home.path().to_path_buf(),
```

在 `Self { ... }` 构造里 `_watchman_home: watchman_home,` 之后加：

```rust
                skills_home_dir: skills_home,
```

在 `impl Harness` 里加访问器（与 `pairing()` 相邻）：

```rust
        /// 隔离 home 的路径:测试据此播种 skill 与断言路径包含关系。
        pub(crate) fn skills_home(&self) -> std::path::PathBuf {
            self.skills_home_dir.path().to_path_buf()
        }
```

> `Harness::with_scope(scope)` **已经存在**（`server.rs:10606`），本任务直接复用，不要新造。测试里用既有的 `initialize(&mut h)`、`read_response(&mut h, id)`、`h.shutdown()` 辅助函数（分别见 `server.rs:10826` / `8758` / `10813`）。

- [ ] **Step 6: 跑测试确认通过**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/skills-settings-panel/yi-agent-rs
cargo test -p yi-agent-app-server skill_rpc_tests 2>&1 | tail -15
```

Expected: PASS，4 个用例全绿。

- [ ] **Step 7: 跑整个 app-server 测试确认无回归**

```bash
cargo test -p yi-agent-app-server 2>&1 | tail -15
```

Expected: PASS。`RuntimeAttachments` 加字段会让所有构造点编译失败，编译器会逐个点出来——按 Step 3 的形态补齐；**不要**用 `..Default::default()` 之类的兜底绕过。

- [ ] **Step 8: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/skills-settings-panel
cargo fmt --all --manifest-path yi-agent-rs/Cargo.toml
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): expose skill/list and skill/read

Both are Observe-scope reads so the paired iPhone (Control) can use the
settings panel. The skill home is injected through RuntimeAttachments,
so tests never enumerate a real ~/.yi-agent/skills."
```

---

### Task 3: 前端 `lib/skills.ts`

**Files:**
- Create: `desktop/src/lib/skills.ts`
- Test: `desktop/src/lib/skills.test.ts`

**Interfaces:**
- Produces:
  - `type SkillsRpc = <T = unknown>(method: string, params: unknown) => Promise<T>`
  - `type SkillScope = "project" | "user" | "system"`
  - `interface SkillSummary { name: string; description: string; path: string }`
  - `interface SkillGroup { scope: SkillScope; skills: SkillSummary[] }`
  - `interface SkillRoots { project: string; user: string; system: string }`
  - `interface SkillList { groups: SkillGroup[]; roots: SkillRoots }`
  - `listSkills(rpc: SkillsRpc, project?: string): Promise<SkillList>`
  - `readSkill(rpc: SkillsRpc, path: string, project?: string): Promise<string>`
  - `SCOPE_LABELS: Record<SkillScope, string>`

- [ ] **Step 1: 写失败测试**

创建 `desktop/src/lib/skills.test.ts`：

```ts
/** @vitest-environment node */
import { describe, expect, it } from "vitest";
import { listSkills, readSkill, SCOPE_LABELS, type SkillsRpc } from "./skills";

/** 非泛型测试接缝 → `SkillsRpc`：与 `pluginSettings.test.ts` 同一约定（那里是 `as unknown as Rpc`）。 */
const rpc = (fn: (method: string, params: unknown) => Promise<unknown>) =>
  fn as unknown as SkillsRpc;

/** 服务端原样形状的响应（snake 无、都是既有字段名）。 */
const response = {
  groups: [
    { scope: "project", skills: [{ name: "p", description: "项目级。", path: "/proj/p/SKILL.md" }] },
    { scope: "user", skills: [{ name: "u", description: "用户级。", path: "/home/u/SKILL.md" }] },
    { scope: "system", skills: [{ name: "s", description: "系统级。", path: "/home/.system/s/SKILL.md" }] },
  ],
  roots: { project: "/proj/.yi-agent/skills", user: "/home/.yi-agent/skills", system: "/home/.yi-agent/skills/.system" },
};

describe("listSkills", () => {
  it("keeps the server's group order and fields", async () => {
    const seen: Array<{ method: string; params: unknown }> = [];
    const client = async (method: string, params: unknown) => {
      seen.push({ method, params });
      return response;
    };
    const listed = await listSkills(rpc(client));
    expect(seen[0].method).toBe("skill/list");
    expect(listed.groups.map((g) => g.scope)).toEqual(["project", "user", "system"]);
    expect(listed.groups[1].skills[0].name).toBe("u");
    expect(listed.roots.user).toBe("/home/.yi-agent/skills");
  });

  it("passes the chosen project through", async () => {
    const seen: string[] = [];
    const client = async (_method: string, params: unknown) => {
      seen.push((params as { project?: string }).project ?? "");
      return response;
    };
    await listSkills(rpc(client), "/proj");
    expect(seen).toEqual(["/proj"]);
  });

  it("drops malformed rows instead of crashing", async () => {
    // 坏数据宁可少列，也不能让整个面板崩掉——与 parseDays 同一取向。
    const client = async () => ({
      groups: [
        { scope: "project", skills: [{ name: "ok", description: "x", path: "/p" }, { description: "no name" }] },
        { scope: "nonsense", skills: [{ name: "z", description: "x", path: "/z" }] },
      ],
      roots: {},
    });
    const listed = await listSkills(rpc(client));
    expect(listed.groups.map((g) => g.scope)).toEqual(["project"]);
    expect(listed.groups[0].skills.map((s) => s.name)).toEqual(["ok"]);
  });

  it("returns an empty list when the server sends nothing", async () => {
    const client = async () => ({});
    const listed = await listSkills(rpc(client));
    expect(listed.groups).toEqual([]);
    expect(listed.roots).toEqual({ project: "", user: "", system: "" });
  });
});

describe("readSkill", () => {
  it("returns the body", async () => {
    const client = (method: string, params: unknown) => {
      expect(method).toBe("skill/read");
      expect((params as { path: string }).path).toBe("/home/u/SKILL.md");
      return Promise.resolve({ body: "# u\n\n正文" });
    };
    expect(await readSkill(rpc(client), "/home/u/SKILL.md")).toBe("# u\n\n正文");
  });

  it("throws when the server sends no body", async () => {
    const client = async () => ({});
    await expect(readSkill(rpc(client), "/x/SKILL.md")).rejects.toThrow(/no body/);
  });
});

describe("SCOPE_LABELS", () => {
  it("names every scope in Chinese", () => {
    expect(SCOPE_LABELS.project).toBe("项目");
    expect(SCOPE_LABELS.user).toBe("用户");
    expect(SCOPE_LABELS.system).toBe("系统内置");
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

```bash
export PATH=/opt/homebrew/bin:$PATH
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/skills-settings-panel/desktop
npx vitest run src/lib/skills.test.ts
```

Expected: FAIL —— 找不到模块 `./skills`。

- [ ] **Step 3: 写实现**

创建 `desktop/src/lib/skills.ts`：

```ts
/**
 * 桌面与宿主之间的 skill 通道。形状与 `PluginRpc` / `ModelRpc` 一致。
 *
 * 服务端返回的字段名保持原样（`groups` / `skills` / `roots` 与 `scope`），本模块
 * 只做一次形状收敛：坏行丢掉、缺失字段补空，绝不把半成品往下传——面板宁可少列，
 * 也不能因为一条脏数据整页崩掉（与 `pluginSettings.ts` 的 `parseDays` 同一取向）。
 */
export type SkillsRpc = <T = unknown>(method: string, params: unknown) => Promise<T>;

/**
 * 非泛型的宿主/测试接缝（与 `PluginsCall` 同形的写法）。
 *
 * `SkillsRpc` 的「调用方自选返回类型」在编译期很好用，但 `vi.fn(async () => ...)`
 * 推断不出泛型函数，直接赋给 `SkillsRpc` 会报 TS2322。测试与宿主用这个非泛型形状，
 * 面板内部一次性断言补回泛型能力——与 `SettingsPluginsTab` 的既有做法一致。
 */
export type SkillsCall = (method: string, params: unknown) => Promise<unknown>;

/** 三层 skill 根，与服务端 `roots` 的键一一对应。 */
export type SkillScope = "project" | "user" | "system";

/** 分组标题：系统内置的 3 个不是用户装的，措辞上要区分开。 */
export const SCOPE_LABELS: Record<SkillScope, string> = {
  project: "项目",
  user: "用户",
  system: "系统内置",
};

export interface SkillSummary {
  name: string;
  description: string;
  path: string;
}

export interface SkillGroup {
  scope: SkillScope;
  skills: SkillSummary[];
}

export interface SkillRoots {
  project: string;
  user: string;
  system: string;
}

export interface SkillList {
  groups: SkillGroup[];
  roots: SkillRoots;
}

function asScope(value: unknown): SkillScope | undefined {
  return value === "project" || value === "user" || value === "system" ? value : undefined;
}

function asSkill(row: unknown): SkillSummary | undefined {
  if (typeof row !== "object" || row === null) return undefined;
  const r = row as Record<string, unknown>;
  if (typeof r.name !== "string" || r.name === "") return undefined;
  return {
    name: r.name,
    description: typeof r.description === "string" ? r.description : "",
    path: typeof r.path === "string" ? r.path : "",
  };
}

/** 枚举 skill。`project` 指定看哪个项目的项目级根；缺省由服务端回落宿主 workdir。 */
export async function listSkills(rpc: SkillsRpc, project?: string): Promise<SkillList> {
  const result = await rpc<{ groups?: unknown; roots?: unknown }>("skill/list", {
    ...(project ? { project } : {}),
  });
  const rows = Array.isArray(result?.groups) ? result.groups : [];
  const groups: SkillGroup[] = [];
  for (const row of rows) {
    if (typeof row !== "object" || row === null) continue;
    const r = row as Record<string, unknown>;
    const scope = asScope(r.scope);
    if (!scope) continue;
    const skills = (Array.isArray(r.skills) ? r.skills : [])
      .map(asSkill)
      .filter((s): s is SkillSummary => s !== undefined);
    groups.push({ scope, skills });
  }
  const rawRoots = (typeof result?.roots === "object" && result.roots !== null
    ? result.roots
    : {}) as Record<string, unknown>;
  const text = (value: unknown) => (typeof value === "string" ? value : "");
  return {
    groups,
    roots: {
      project: text(rawRoots.project),
      user: text(rawRoots.user),
      system: text(rawRoots.system),
    },
  };
}

/** 读某个 SKILL.md 的正文（服务端已剥掉 frontmatter）。 */
export async function readSkill(
  rpc: SkillsRpc,
  path: string,
  project?: string,
): Promise<string> {
  const result = await rpc<{ body?: unknown }>("skill/read", {
    path,
    ...(project ? { project } : {}),
  });
  if (typeof result?.body !== "string") throw new Error("the server returned no body");
  return result.body;
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/skills-settings-panel/desktop
npx vitest run src/lib/skills.test.ts
```

Expected: PASS，7 个用例全绿。

- [ ] **Step 5: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/skills-settings-panel
git add desktop/src/lib/skills.ts desktop/src/lib/skills.test.ts
git commit -m "feat(desktop): skill channel client with shape hardening

Drops malformed rows instead of throwing, so one bad entry cannot take
down the whole panel."
```

---

### Task 4: 抽出共享项目选择器

**Files:**
- Create: `desktop/src/components/SettingsProjectPicker.tsx`
- Modify: `desktop/src/components/SettingsPluginsTab.tsx`

**Interfaces:**
- Produces: `SettingsProjectPicker({ options, selected, onSelect, ariaLabel }: { options: string[]; selected: string; onSelect: (project: string) => void; ariaLabel: string })`，`options` 为空时渲染 `null`。Task 5 直接复用。
- 前置事实：`SettingsPluginsTab` 现在的选择器标记、`options` 去重逻辑（候选 ∪ 默认）与 `aria-label="插件设置项目"` 都要**逐字保留**，其既有测试不改动。

- [ ] **Step 1: 抽出组件**

创建 `desktop/src/components/SettingsProjectPicker.tsx`：

```tsx
/**
 * 设置页各 Tab 共用的「在配置哪个项目」选择器。
 *
 * 插件清单与 skill 的三层根都按项目解析（`<项目>/.yi-agent/...`），而桌面侧车
 * app-server 的 workdir 是用户 home，所以每个这类面板都必须让用户明确选项目，
 * 并把它随每次 RPC 传下去——少了这一步，看板跑着、设置页却是空的。
 *
 * `ariaLabel` 由调用方给：两个 Tab 的可访问名不同，测试也按各自的名字查询。
 */
export function SettingsProjectPicker({
  options,
  selected,
  onSelect,
  ariaLabel,
}: {
  /** 候选项目（绝对路径）。空表时不渲染——没有可选项时给个空下拉框只会误导。 */
  options: string[];
  selected: string;
  onSelect: (project: string) => void;
  ariaLabel: string;
}) {
  if (options.length === 0) return null;
  return (
    <label className="flex items-center gap-3 border-b border-line p-4 text-sm">
      <span>项目</span>
      <select
        aria-label={ariaLabel}
        className="min-w-0 flex-1 rounded border border-line bg-surface px-2 py-1 font-mono text-xs"
        value={selected}
        onChange={(e) => onSelect(e.target.value)}
      >
        {options.map((project) => (
          <option key={project} value={project}>
            {project}
          </option>
        ))}
      </select>
    </label>
  );
}
```

- [ ] **Step 2: 让插件 Tab 改用它**

在 `SettingsPluginsTab.tsx` 里：

1. 顶部加 `import { SettingsProjectPicker } from "./SettingsProjectPicker";`
2. 把 `return (...)` 里那段 `{options.length > 0 && (<label ...>...</label>)}` 整体替换为：

```tsx
      <SettingsProjectPicker
        options={options}
        selected={selected}
        onSelect={setSelected}
        ariaLabel="插件设置项目"
      />
```

- [ ] **Step 3: 跑既有测试确认无回归**

```bash
export PATH=/opt/homebrew/bin:$PATH
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/skills-settings-panel/desktop
npx vitest run src/components/SettingsPluginsTab.test.tsx src/components/SettingsDialog.test.tsx
npx tsc --noEmit
```

Expected: PASS，且**未改动任何测试文件**。这是「抽取没抽坏」的证明——若有用例变红，说明标记或可访问名变了，回到 Step 2 改回逐字一致。

- [ ] **Step 4: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/skills-settings-panel
git add desktop/src/components/SettingsProjectPicker.tsx desktop/src/components/SettingsPluginsTab.tsx
git commit -m "refactor(desktop): share the settings project picker

The skills panel needs the same picker; extracting it keeps one
definition of when it shows and how it labels itself."
```

---

### Task 5: `SettingsSkillsTab`

**Files:**
- Create: `desktop/src/components/SettingsSkillsTab.tsx`
- Test: `desktop/src/components/SettingsSkillsTab.test.tsx`

**Interfaces:**
- Consumes: Task 3 的 `listSkills` / `readSkill` / `SCOPE_LABELS` / `SkillsRpc`；Task 4 的 `SettingsProjectPicker`。
- Produces: `SettingsSkillsTab({ call, projects, defaultProject }: { call?: SkillsRpc; projects: string[]; defaultProject?: string })`。Task 6 据此接线。

- [ ] **Step 1: 写失败测试**

创建 `desktop/src/components/SettingsSkillsTab.test.tsx`：

```tsx
/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SettingsSkillsTab } from "./SettingsSkillsTab";
import type { SkillsCall } from "../lib/skills";

/** `vi.fn` 推断不出泛型签名，按既有约定一次性断言（同 `pluginSettings.test.ts`）。 */
const seam = (fn: unknown) => fn as unknown as SkillsCall;

afterEach(cleanup);

const LIST = {
  groups: [
    { scope: "project", skills: [{ name: "proj-skill", description: "项目级。", path: "/proj/SKILL.md" }] },
    { scope: "user", skills: [{ name: "user-skill", description: "用户级。", path: "/home/u/SKILL.md" }] },
    { scope: "system", skills: [{ name: "skill-creator", description: "内置。", path: "/home/.system/c/SKILL.md" }] },
  ],
  roots: {
    project: "/proj/.yi-agent/skills",
    user: "/home/.yi-agent/skills",
    system: "/home/.yi-agent/skills/.system",
  },
};

describe("SettingsSkillsTab", () => {
  it("lists every group with its Chinese heading", async () => {
    const call = vi.fn(async () => LIST);
    render(<SettingsSkillsTab call={seam(call)} projects={[]} />);
    expect(await screen.findByText("proj-skill")).toBeTruthy();
    expect(screen.getByText("项目")).toBeTruthy();
    expect(screen.getByText("用户")).toBeTruthy();
    expect(screen.getByText("系统内置")).toBeTruthy();
  });

  it("hides a group that has no skills", async () => {
    const call = vi.fn(async () => ({ ...LIST, groups: [LIST.groups[1]] }));
    render(<SettingsSkillsTab call={seam(call)} projects={[]} />);
    expect(await screen.findByText("user-skill")).toBeTruthy();
    expect(screen.queryByText("项目")).toBeNull();
  });

  it("shows an empty state when nothing is installed", async () => {
    const call = vi.fn(async () => ({ groups: [], roots: LIST.roots }));
    render(<SettingsSkillsTab call={seam(call)} projects={[]} />);
    expect(await screen.findByText("还没有安装任何技能")).toBeTruthy();
  });

  it("reports a list failure without crashing", async () => {
    const call = vi.fn(async () => {
      throw { code: -32603, message: "boom" };
    });
    render(<SettingsSkillsTab call={seam(call)} projects={[]} />);
    expect(await screen.findByRole("alert")).toBeTruthy();
  });

  it("renders nothing but a notice without a host seam", async () => {
    render(<SettingsSkillsTab projects={[]} />);
    expect(await screen.findByText("当前连接无法读取技能列表")).toBeTruthy();
  });

  it("fetches the body only when a row is expanded", async () => {
    const call = vi.fn(async (method: string) => (method === "skill/list" ? LIST : { body: "# user-skill\n\n正文" }));
    render(<SettingsSkillsTab call={seam(call)} projects={[]} />);
    const row = await screen.findByRole("button", { name: /user-skill/ });
    expect(row.getAttribute("aria-expanded")).toBe("false");
    // 折叠时正文不在 DOM 里，也从未请求过。
    expect(call.mock.calls.filter(([m]) => m === "skill/read")).toHaveLength(0);

    fireEvent.click(row);
    expect(await screen.findByText("正文")).toBeTruthy();
    expect(call.mock.calls.filter(([m]) => m === "skill/read")).toHaveLength(1);
    expect(row.getAttribute("aria-expanded")).toBe("true");
  });

  it("does not re-fetch after collapsing and expanding again", async () => {
    const call = vi.fn(async (method: string) => (method === "skill/list" ? LIST : { body: "正文" }));
    render(<SettingsSkillsTab call={seam(call)} projects={[]} />);
    const row = await screen.findByRole("button", { name: /user-skill/ });
    fireEvent.click(row);
    await screen.findByText("正文");
    fireEvent.click(row);
    fireEvent.click(row);
    await waitFor(() =>
      expect(call.mock.calls.filter(([m]) => m === "skill/read")).toHaveLength(1),
    );
  });

  it("surfaces a read failure instead of showing an empty body", async () => {
    const call = vi.fn(async (method: string) => {
      if (method === "skill/list") return LIST;
      throw { code: -32602, message: "path escapes the skill roots" };
    });
    render(<SettingsSkillsTab call={seam(call)} projects={[]} />);
    fireEvent.click(await screen.findByRole("button", { name: /user-skill/ }));
    expect(await screen.findByText(/无法读取该技能/)).toBeTruthy();
  });

  it("reloads when the project changes", async () => {
    // 显式声明两个参数:否则 `vi.fn` 的 calls 元组是空类型,下面的 `[m]`/`[, p]` 取不到。
    const call = vi.fn(async (_method: string, _params: unknown) => LIST);
    render(<SettingsSkillsTab call={seam(call)} projects={["/a", "/b"]} defaultProject="/a" />);
    await screen.findByText("user-skill");
    fireEvent.change(screen.getByLabelText("技能设置项目"), { target: { value: "/b" } });
    await waitFor(() => {
      const scopes = call.mock.calls
        .filter(([m]) => m === "skill/list")
        .map(([, p]) => (p as { project?: string }).project);
      expect(scopes).toEqual(["/a", "/b"]);
    });
  });

  it("explains how to add a skill, using the server's real paths", async () => {
    const call = vi.fn(async () => LIST);
    render(<SettingsSkillsTab call={seam(call)} projects={[]} />);
    const heading = await screen.findByText("如何添加 Skill");
    // 断言整段帮助区的文本：三条目录行都含 `/home/.yi-agent/skills` 前缀，逐个查会
    // 命中多个元素；这里要验的是「路径确实来自服务端响应，而不是硬编码 `~`」。
    const help = heading.closest("section")?.textContent ?? "";
    expect(help).toContain("/proj/.yi-agent/skills");
    expect(help).toContain("/home/.yi-agent/skills");
    expect(help).toContain("/home/.yi-agent/skills/.system");
    expect(help).toContain("skill-creator");
    expect(help).toContain("skill-installer");
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/skills-settings-panel/desktop
npx vitest run src/components/SettingsSkillsTab.test.tsx
```

Expected: FAIL —— 找不到模块 `./SettingsSkillsTab`。

- [ ] **Step 3: 写实现**

创建 `desktop/src/components/SettingsSkillsTab.tsx`：

```tsx
import { useCallback, useEffect, useRef, useState } from "react";
import {
  listSkills,
  readSkill,
  SCOPE_LABELS,
  type SkillGroup,
  type SkillList,
  type SkillsCall,
  type SkillsRpc,
} from "../lib/skills";
import { SettingsProjectPicker } from "./SettingsProjectPicker";
import { MarkdownText } from "./MarkdownText";

const EMPTY: SkillList = {
  groups: [],
  roots: { project: "", user: "", system: "" },
};

/**
 * 「技能」Tab。
 *
 * 三层根（项目 / 用户 / 系统）由服务端按 `project` 解析——桌面侧车的 workdir 是
 * home，所以必须让用户明确「在看哪个项目」，理由与「插件」Tab 完全相同。
 *
 * 正文**点开才拉**：列表响应里不含 body，一条 skill 的正文可能有十几 KB，全部预取
 * 只会让打开设置变慢。
 */
export function SettingsSkillsTab({
  call,
  projects,
  defaultProject,
}: {
  call?: SkillsCall;
  /** 可选项目（绝对路径）。空表时面板回落宿主 workdir。 */
  projects: string[];
  /** 首选项目（当前对话的工作目录）。不在 `projects` 里时回落到第一个。 */
  defaultProject?: string;
}) {
  const [listed, setListed] = useState<SkillList>(EMPTY);
  const [loading, setLoading] = useState(true);
  const [listError, setListError] = useState<string | null>(null);
  const [selected, setSelected] = useState<string>(() => defaultProject ?? projects[0] ?? "");
  /** path → 正文。折叠不丢，避免反复展开重复拉取。 */
  const [bodies, setBodies] = useState<Record<string, string>>({});
  /** path → 读取失败原因。 */
  const [bodyErrors, setBodyErrors] = useState<Record<string, string>>({});
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});

  const rpc = call as SkillsRpc | undefined;

  // 选项 = 候选 ∪ 默认项目（当前对话目录可能不在候选里，只列候选就无法选中）。
  const options = (() => {
    const seen = new Set<string>();
    const out: string[] = [];
    for (const path of [defaultProject, ...projects]) {
      const trimmed = path?.trim();
      if (!trimmed || seen.has(trimmed)) continue;
      seen.add(trimmed);
      out.push(trimmed);
    }
    return out;
  })();

  // `project` 进 ref，理由同插件面板：宿主每次重渲染可能换新接缝身份，但我们只在
  // 「项目真的换了」时重新枚举。
  const projectRef = useRef(selected);
  projectRef.current = selected;

  // 项目迟到（workspaces 异步到达）时补一个默认值；用户已选定的不覆盖。
  useEffect(() => {
    if (selected !== "") return;
    const next = defaultProject ?? projects[0] ?? "";
    if (next !== "") setSelected(next);
  }, [defaultProject, projects, selected]);

  useEffect(() => {
    if (!rpc) {
      setLoading(false);
      return;
    }
    let cancelled = false;
    setLoading(true);
    setListError(null);
    void (async () => {
      try {
        const result = await listSkills(rpc, projectRef.current || undefined);
        if (!cancelled) setListed(result);
      } catch {
        if (!cancelled) setListError("无法读取技能列表");
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [rpc, selected]);

  /** 展开时按需拉正文；已有正文或已有错误则不重复请求。 */
  const toggle = useCallback(
    (path: string) => {
      const willExpand = !expanded[path];
      setExpanded((e) => ({ ...e, [path]: willExpand }));
      if (!willExpand || !rpc || bodies[path] !== undefined || bodyErrors[path] !== undefined) {
        return;
      }
      void (async () => {
        try {
          const body = await readSkill(rpc, path, projectRef.current || undefined);
          setBodies((b) => ({ ...b, [path]: body }));
        } catch {
          setBodyErrors((e) => ({ ...e, [path]: "无法读取该技能（文件可能已被删除或移动）" }));
        }
      })();
    },
    [expanded, rpc, bodies, bodyErrors],
  );

  if (!call) {
    return <p className="p-4 text-xs text-fg-subtle">当前连接无法读取技能列表</p>;
  }
  if (listError) {
    return (
      <p role="alert" className="p-4 text-xs text-amber-500">
        {listError}
      </p>
    );
  }

  const visible: SkillGroup[] = listed.groups.filter((g) => g.skills.length > 0);

  return (
    <div>
      <SettingsProjectPicker
        options={options}
        selected={selected}
        onSelect={setSelected}
        ariaLabel="技能设置项目"
      />
      {loading ? (
        <p className="p-4 text-xs text-fg-subtle">正在读取技能…</p>
      ) : visible.length === 0 ? (
        <p className="p-4 text-sm text-fg-muted">还没有安装任何技能</p>
      ) : (
        <div className="divide-y divide-line">
          {visible.map((group) => (
            <section key={group.scope} className="p-4">
              <h3 className="text-xs font-medium text-fg-subtle">{SCOPE_LABELS[group.scope]}</h3>
              <ul className="mt-2">
                {group.skills.map((skill) => {
                  const open = expanded[skill.path] === true;
                  return (
                    <li key={skill.path} className="border-b border-line/50 last:border-b-0">
                      {/* 折叠范式与 ChatView 的「思考」块一致：aria-expanded + ▸/▾。 */}
                      <button
                        type="button"
                        aria-expanded={open}
                        onClick={() => toggle(skill.path)}
                        className="flex w-full items-start gap-2 px-1 py-2 text-left"
                      >
                        <span aria-hidden="true" className="text-fg-subtle">
                          {open ? "▾" : "▸"}
                        </span>
                        <span className="min-w-0">
                          <span className="block text-sm font-medium text-fg">{skill.name}</span>
                          <span className="block text-xs text-fg-subtle">{skill.description}</span>
                        </span>
                      </button>
                      {open && (
                        <div className="px-1 pb-3 pl-6">
                          {bodyErrors[skill.path] ? (
                            <p role="alert" className="text-xs text-amber-500">
                              {bodyErrors[skill.path]}
                            </p>
                          ) : bodies[skill.path] === undefined ? (
                            <p className="text-xs text-fg-subtle">正在读取…</p>
                          ) : (
                            <div className="prose prose-sm max-w-none text-fg">
                              <MarkdownText text={bodies[skill.path]} />
                            </div>
                          )}
                        </div>
                      )}
                    </li>
                  );
                })}
              </ul>
            </section>
          ))}
        </div>
      )}
      <AddSkillHelp roots={listed.roots} />
    </div>
  );
}

/**
 * 「如何添加 Skill」说明。
 *
 * 路径一律取服务端返回的**真实解析值**，不硬编码 `~/.yi-agent/skills`：路径规则若
 * 变动，硬编码的文案会与事实脱节。
 */
function AddSkillHelp({ roots }: { roots: SkillList["roots"] }) {
  // 标签带「目录」后缀：上方的分组标题已经占用了「系统内置」，重名会让两者在
  // 可访问树与测试查询里无法区分。
  const directories = [
    { label: "项目级目录", path: roots.project, note: "只对这个项目生效" },
    { label: "用户级目录", path: roots.user, note: "对本机所有项目生效" },
    { label: "系统内置目录", path: roots.system, note: "随应用安装，升级时会被覆盖" },
  ].filter((d) => d.path !== "");

  return (
    <section className="border-t border-line p-4">
      <h3 className="text-sm font-medium text-fg">如何添加 Skill</h3>
      <p className="mt-2 text-xs text-fg-muted">
        把一个名为 <code className="font-mono">SKILL.md</code> 的文件放进下面任一层目录的
        子目录里即可生效（子目录名就是技能名）。下一轮对话开始时自动加载，无需重启。
      </p>
      <ul className="mt-3 space-y-1">
        {directories.map((d) => (
          <li key={d.label} className="text-xs text-fg-subtle">
            <span className="text-fg-muted">{d.label}</span>：
            <code className="font-mono">{d.path}/&lt;技能名&gt;/SKILL.md</code>
            <span className="ml-2">{d.note}</span>
          </li>
        ))}
      </ul>
      <p className="mt-3 text-xs text-fg-muted">
        技能名只能用小写字母、数字与连字符（如 <code className="font-mono">my-skill</code>
        ），最长 64 个字符。文件开头必须有 frontmatter：
      </p>
      <pre className="mt-2 overflow-x-auto rounded border border-line bg-surface p-2 font-mono text-xs text-fg-muted">
{`---
name: my-skill
description: 一句话说明这个技能做什么、什么时候该用。
---

正文：模型加载这个技能后会照做的操作步骤。`}
      </pre>
      <p className="mt-3 text-xs text-fg-subtle">
        也可以直接对话让 Yi-Agent 帮你写：用内置的 <code className="font-mono">skill-creator</code>
        {" "}创建新技能，或用内置的 <code className="font-mono">skill-installer</code>
        {" "}从外部仓库安装现成的技能。
      </p>
    </section>
  );
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/skills-settings-panel/desktop
npx vitest run src/components/SettingsSkillsTab.test.tsx
```

Expected: PASS，10 个用例全绿。

- [ ] **Step 5: 类型检查**

```bash
npx tsc --noEmit
```

Expected: 无错误。

- [ ] **Step 6: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/skills-settings-panel
git add desktop/src/components/SettingsSkillsTab.tsx desktop/src/components/SettingsSkillsTab.test.tsx
git commit -m "feat(desktop): skills tab with lazy body loading

Lists the three scopes, expands one skill at a time (body is fetched on
first expand and cached), and explains how to add a skill using the
paths the server actually resolved."
```

---

### Task 6: 桌面端接入「技能」分类

**Files:**
- Modify: `desktop/src/components/SettingsDialog.tsx`
- Test: `desktop/src/components/SettingsDialog.test.tsx`

**Interfaces:**
- Consumes: Task 5 的 `SettingsSkillsTab`、`SkillsCall`。
- Produces: `SettingsDialog` 新 prop `skillsCall?: SkillsCall`、`skillsProjects?: string[]`、`skillsDefaultProject?: string`。Task 8 据此接线。

- [ ] **Step 1: 写失败测试**

在 `SettingsDialog.test.tsx` 的 `describe("SettingsDialog tabs")` 内、`模型` 那条用例之后追加：

```tsx
  it("shows a 技能 tab backed by the injected skillsCall", async () => {
    const skillsCall = async (method: string) =>
      method === "skill/list"
        ? {
            groups: [{ scope: "user", skills: [{ name: "user-skill", description: "用户级。", path: "/u/SKILL.md" }] }],
            roots: { project: "/p", user: "/u", system: "/s" },
          }
        : { body: "" };
    render(
      <SettingsDialog
        open
        theme="dark"
        onThemeChange={() => {}}
        onClose={() => {}}
        skillsCall={skillsCall}
      />,
    );
    fireEvent.click(screen.getByRole("tab", { name: "技能" }));
    expect(await screen.findByText("user-skill")).toBeTruthy();
  });

  it("lists all five categories on the desktop", () => {
    render(<SettingsDialog open theme="dark" onThemeChange={() => {}} onClose={() => {}} />);
    const names = screen.getAllByRole("tab").map((t) => t.textContent);
    expect(names).toEqual(["通用", "远程访问", "插件", "模型", "技能"]);
  });
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/skills-settings-panel/desktop
npx vitest run src/components/SettingsDialog.test.tsx -t "技能"
```

Expected: FAIL —— 找不到名为「技能」的 tab。

- [ ] **Step 3: 加分类、prop 与面板渲染**

在 `SettingsDialog.tsx` 里：

1. 顶部加 `import { SettingsSkillsTab } from "./SettingsSkillsTab";
import type { SkillsCall } from "../lib/skills";`
2. `TABS` 加一项（放最后，顺序即界面顺序）：

```tsx
const TABS = [
  { id: "general", label: "通用" },
  { id: "remote", label: "远程访问" },
  { id: "plugins", label: "插件" },
  { id: "models", label: "模型" },
  { id: "skills", label: "技能" },
] as const;
```

3. 签名加三个 prop（放在 `modelCall` 之后）：

```tsx
  /** 「技能」Tab 的宿主 RPC 接缝（skill/list、skill/read）。 */
  skillsCall?: SkillsCall;
  /** 「技能」Tab 可选的配置项目（绝对路径）。 */
  skillsProjects?: string[];
  /** 「技能」Tab 首选项目（当前对话的工作目录）。 */
  skillsDefaultProject?: string;
```

4. 面板渲染链（现在以 `models` 结尾）改为：

```tsx
            ) : active === "models" ? (
              <SettingsModelsTab call={modelCall} isMobile={isMobile} />
            ) : (
              <SettingsSkillsTab
                call={skillsCall}
                projects={skillsProjects ?? []}
                defaultProject={skillsDefaultProject}
              />
            )}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/skills-settings-panel/desktop
npx vitest run src/components/SettingsDialog.test.tsx
npx tsc --noEmit
```

Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/skills-settings-panel
git add desktop/src/components/SettingsDialog.tsx desktop/src/components/SettingsDialog.test.tsx
git commit -m "feat(desktop): add the 技能 category to settings"
```

---

### Task 7: 手机端两级导航（含既有断言改写）

**Files:**
- Modify: `desktop/src/components/SettingsDialog.tsx`
- Modify: `desktop/src/index.css`
- Test: `desktop/src/components/SettingsDialog.test.tsx`
- Test: `desktop/src/mobileLayout.test.ts`

**Interfaces:**
- Consumes: Task 6 的 `TABS` 与 `SettingsDialog` 结构。
- Produces: 手机端 DOM 契约——分类列表容器 `.settings-categories`，其中每行 `data-settings-category`；子页的返回按钮 `aria-label="返回设置分类"`。桌面端 DOM 不变。

- [ ] **Step 1: 改写既有手机端断言（先让它们钉住新形态）**

`SettingsDialog.test.tsx` 的 `describe("SettingsDialog phone layout hooks")` 里，把「flips the tablist orientation to horizontal on the phone」与「renders the four class hooks the phone CSS targets, on both platforms」两条**替换**为：

```tsx
  /** 手机上按分类 id 取那一行（文本里含 `›` 装饰，故不按文本查）。 */
  const categoryRow = (id: string) =>
    document.querySelector(`[data-settings-category="${id}"]`) as HTMLElement;

  it("renders a category list, not a tablist, on the phone", () => {
    // 手机上分类很多，等宽横条放不下（5 格时 320pt 每格仅 64px，「远程访问」四字
    // 会溢出）。改成「分类列表 → 点进内容」的两级结构，横向宽度不再是约束。
    // 同时必须真的没有 tablist：留着一个隐藏的 tablist 会与可见列表争可访问名。
    render(
      <SettingsDialog open isMobile theme="dark" onThemeChange={() => {}} onClose={() => {}} />,
    );
    expect(screen.queryByRole("tablist")).toBeNull();
    expect(screen.queryByRole("tab")).toBeNull();
    expect(document.querySelector(".settings-categories")).toBeTruthy();
    const rows = Array.from(document.querySelectorAll("[data-settings-category]"));
    expect(rows.map((r) => r.getAttribute("data-settings-category"))).toEqual([
      "general",
      "remote",
      "plugins",
      "models",
      "skills",
    ]);
  });

  it("drills into a category and comes back", () => {
    render(
      <SettingsDialog open isMobile theme="dark" onThemeChange={() => {}} onClose={() => {}} />,
    );
    fireEvent.click(categoryRow("general"));
    // 子页：返回按钮 + 该分类的面板（「通用」里是主题选择）。
    expect(screen.getByRole("button", { name: "返回设置分类" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "深色" })).toBeTruthy();
    // 列表已不在。
    expect(document.querySelector(".settings-categories")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "返回设置分类" }));
    expect(document.querySelector(".settings-categories")).toBeTruthy();
  });

  it("returns focus to the category row it came from", () => {
    render(
      <SettingsDialog open isMobile theme="dark" onThemeChange={() => {}} onClose={() => {}} />,
    );
    fireEvent.click(categoryRow("models"));
    fireEvent.click(screen.getByRole("button", { name: "返回设置分类" }));
    // 返回后焦点落在刚才那一行，键盘/读屏用户不用重新找位置。
    expect(document.activeElement).toBe(categoryRow("models"));
  });

  it("starts on the category list every time settings opens", () => {
    // 上次停在哪个子页与本次打开无关；留着会让人以为设置只有那一页。
    const { rerender } = render(
      <SettingsDialog open isMobile theme="dark" onThemeChange={() => {}} onClose={() => {}} />,
    );
    fireEvent.click(categoryRow("models"));
    expect(document.querySelector(".settings-categories")).toBeNull();
    rerender(
      <SettingsDialog open={false} isMobile theme="dark" onThemeChange={() => {}} onClose={() => {}} />,
    );
    rerender(
      <SettingsDialog open isMobile theme="dark" onThemeChange={() => {}} onClose={() => {}} />,
    );
    expect(document.querySelector(".settings-categories")).toBeTruthy();
  });

  it("renders the class hooks the phone CSS targets, on both platforms", () => {
    const { container, rerender } = render(
      <SettingsDialog open isMobile theme="dark" onThemeChange={() => {}} onClose={() => {}} />,
    );
    // 手机端首页是分类列表：shell 与 header 齐备，导航条与面板都不渲染
    // （面板要等点进某个分类才出现，见下一条用例）。
    expect(container.querySelector(".settings-shell")).toBeTruthy();
    expect(container.querySelector(".settings-header")).toBeTruthy();
    expect(container.querySelector(".settings-nav")).toBeNull();
    expect(container.querySelector(".settings-panel")).toBeNull();
    expect(container.querySelector(".settings-categories")).toBeTruthy();

    // 桌面端四类挂点齐备（`.settings-categories` 是手机专属，桌面不渲染）。
    rerender(<SettingsDialog open theme="dark" onThemeChange={() => {}} onClose={() => {}} />);
    expect(container.querySelector(".settings-shell")).toBeTruthy();
    expect(container.querySelector(".settings-header")).toBeTruthy();
    expect(container.querySelector(".settings-nav")).toBeTruthy();
    expect(container.querySelector(".settings-panel")).toBeTruthy();
    expect(container.querySelector(".settings-categories")).toBeNull();
  });
```

另外，`describe("SettingsDialog wires phone layout down to the models tab")` 里那条
「passes isMobile to the models panel, so it renders cards not a table」也依赖手机端
tablist，必须把它的进入方式改成走分类列表（断言部分不变，仍然是「手机上出卡片、
不出表格」）：

```tsx
    // 手机端没有 tablist，改从分类列表进入「模型」。
    fireEvent.click(document.querySelector('[data-settings-category="models"]') as HTMLElement);
    await screen.findByText("••••1234");
    expect(container.querySelector("[data-model-card]")).toBeTruthy();
    expect(container.querySelector("table")).toBeNull();
```

`mobileLayout.test.ts` 里还有一条「puts the settings header above the tab bar on the phone」，
它断言的是已被删除的 `order` 机制。手机端不再渲染 nav 后，`.settings-body { display: contents }`
让 header 与 panel 直接升为 shell 的 flex item，**DOM 顺序即视觉顺序** `[header, panel]`，
不需要 `order`。把该条**替换**为：

```ts
  it("keeps the settings header above the panel on the phone", () => {
    // 手机端不渲染 nav，`.settings-body { display: contents }` 让 header 与 panel 直接
    // 升为 shell 的 flex item，DOM 顺序即视觉顺序 [header, panel]——不需要 `order`。
    // 顶部安全区内边距（刘海）仍挂在 header 上，故它必须排在最前，否则那段留白会落到
    // 屏幕中部、面板顶到状态栏下面。
    const bodyRule =
      /\[data-mobile="true"\]\s+\.settings-body\s*\{([^}]*)\}/.exec(css())?.[1] ?? "";
    expect(bodyRule).toMatch(/display:\s*contents/);

    // 同一选择器会有多条规则块（顶部安全区、flex-shrink），必须汇总全部声明再断言；
    // 只取第一条会漏掉 `flex-shrink`。
    const head = [
      ...css().matchAll(/\[data-mobile="true"\]\s+\.settings-header\s*\{([^}]*)\}/g),
    ]
      .map((m) => m[1])
      .join("\n");
    // 升为 flex item 后必须锁住高度，否则会被纵向 flex 压缩。
    expect(head).toMatch(/flex-shrink:\s*0/);
    // 旧机制必须彻底消失：留着 `order` 只会在没有 nav 的页面上造成误解。
    expect(css()).not.toMatch(/\[data-mobile="true"\]\s+\.settings-header\s*\{[^}]*order:/);
    expect(css()).not.toMatch(/\[data-mobile="true"\]\s+\.settings-nav\s*\{/);
  });
```

`mobileLayout.test.ts` 里把「turns the phone settings into a full-screen page with a top tab bar」整条**替换**为：

```ts
  it("turns the phone settings into a full-screen page with a category list", () => {
    // 「设置区域过于小」的根因：弹窗 `w-[70vw]` 在 390pt 上只有 273px，左侧竖栏又是
    // 写死的 `w-40`(160px)，占去 59%，内容面板只剩 111px 且横向溢出滚不到。手机端
    // 必须整屏（`position: fixed` + `inset: 0`），导航改横向**列表**才拿得到全部宽度。
    const shell =
      /\[data-mobile="true"\]\s+\.settings-shell\s*\{([^}]*)\}/.exec(css())?.[1] ?? "";
    expect(shell).toMatch(/position:\s*fixed/);
    expect(shell).toMatch(/inset:\s*0/);
    expect(shell).toMatch(/flex-direction:\s*column/);
    // 弹窗那套尺寸必须被解除，否则 fixed 了却仍是 70vw/70vh 的小盒子。
    expect(shell).toMatch(/width:\s*auto/);
    expect(shell).toMatch(/max-width:\s*none/);
    expect(shell).toMatch(/height:\s*auto/);

    // 分类列表：纵向一列、每行满宽，因此不存在横向溢出。
    const list =
      /\[data-mobile="true"\]\s+\.settings-categories\s*\{([^}]*)\}/.exec(css())?.[1] ?? "";
    expect(list).toMatch(/flex-direction:\s*column/);
    expect(list).toMatch(/overflow-y:\s*auto/);

    // 每行 44px 触控下限（Apple HIG）：上下各 12px 内边距 + 行高。
    const row =
      /\[data-mobile="true"\]\s+\.settings-categories\s*>\s*\[data-settings-category\]\s*\{([^}]*)\}/.exec(
        css(),
      )?.[1] ?? "";
    expect(row).toMatch(/padding-top:\s*12px/);
    expect(row).toMatch(/padding-bottom:\s*12px/);
    expect(row).toMatch(/white-space:\s*nowrap/);

    // 旧横条那套规则必须彻底消失：留着它会在手机端重新挤出横向滚动。
    expect(css()).not.toMatch(/\[data-mobile="true"\]\s+\.settings-nav\s*\{/);
    expect(css()).not.toMatch(/\[data-mobile="true"\]\s+\.settings-nav\s*>/);
  });
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/skills-settings-panel/desktop
npx vitest run src/components/SettingsDialog.test.tsx src/mobileLayout.test.ts
```

Expected: FAIL —— 手机端仍是 tablist，`.settings-categories` 不存在。

- [ ] **Step 3: 改 `SettingsDialog` 的渲染结构**

在 `SettingsDialog.tsx` 里：

1. state 与 ref：在 `const [active, setActive] = useState<TabId>("general");` 之后加：

```tsx
  /**
   * 手机端的两级导航位置。桌面端恒为 `"panel"`（从不渲染分类列表）。
   *
   * 手机上分类可能有 5 个以上，等宽横条在 320pt 上放不下（第 5 格只剩 64px，
   * 「远程访问」四字溢出）。列表 → 子页的结构把横向宽度从约束里摘掉。
   */
  const [phonePane, setPhonePane] = useState<"list" | "panel">("panel");
  /** 从子页返回后要重新落焦的分类。`null` = 无需落焦。 */
  const [pendingFocus, setPendingFocus] = useState<TabId | null>(null);
  /** 分类行的 DOM 句柄，返回时把焦点还给对应行。 */
  const categoryRefs = useRef<Partial<Record<TabId, HTMLButtonElement | null>>>({});
  /** 子页的返回按钮，进入子页时落焦于此。 */
  const backRef = useRef<HTMLButtonElement | null>(null);
```

2. 打开时重置导航位置（与既有打开副作用并列，放在同一个 `useEffect` 内、
   `tabRefs.current[...].focus()` 之前）。**必须按 `isMobile` 取值**：桌面端恒为
   `"panel"`（从不渲染分类列表），手机端每次打开都从分类列表起步——上次停在哪个
   子页与本次打开无关，留着会让人以为设置只有那一页：

```tsx
    setPhonePane(isMobile ? "list" : "panel");
```

   同时把 `isMobile` 加进该 `useEffect` 的依赖数组（`[open, isMobile]`）。

3. 手机端子页落焦（新增一个 effect，放在打开副作用之后）：

```tsx
  // 手机端进子页后落焦在「返回」：这是该页唯一的导航控件，键盘/读屏用户一进来就能
  // 退回去。桌面端没有返回按钮，该 effect 自然不生效。
  useEffect(() => {
    if (!open || !isMobile || phonePane !== "panel") return;
    backRef.current?.focus();
  }, [open, isMobile, phonePane, active]);

  // 返回列表后把焦点还给来源行。用 effect 而不是在 onClick 里直接 focus：那一刻该行
  // 还没挂载。`pendingFocus` 用完即清，故 effect 不会重复触发。掉到 body 上会让
  // 键盘/读屏用户丢掉位置。
  useEffect(() => {
    if (pendingFocus === null) return;
    categoryRefs.current[pendingFocus]?.focus();
    setPendingFocus(null);
  }, [pendingFocus]);
```

4. `return` 内，把「导航 nav + 包裹层」整段替换为下列结构（`role="dialog"` 那一层的内容）：

```tsx
        {/*
          桌面：左侧竖栏 tablist（`aria-orientation="vertical"` + roving tabIndex）。
          手机：不渲染 tablist——分类列表与子页二选一。手机上留一个隐藏 tablist 会与
          可见列表争可访问名，读屏会念出两套导航。
        */}
        {!isMobile && (
          <nav
            role="tablist"
            aria-orientation="vertical"
            onKeyDown={onTablistKeyDown}
            className="settings-nav flex w-40 shrink-0 flex-col border-r border-line bg-surface p-2"
          >
            {TABS.map((t) => (
              <button
                key={t.id}
                id={tabId(t.id)}
                ref={(el) => {
                  tabRefs.current[t.id] = el;
                }}
                role="tab"
                aria-selected={t.id === active}
                aria-controls={panelId(t.id)}
                tabIndex={t.id === active ? 0 : -1}
                onClick={() => setActive(t.id)}
                className="rounded px-3 py-1.5 text-left text-sm text-fg-muted hover:bg-raised/50 aria-selected:text-fg"
              >
                {t.label}
              </button>
            ))}
          </nav>
        )}
        {/*
          手机端要靠 `order` 把标题栏排到 Tab 条之前（DOM 里 nav 是 shell 的第一个
          子元素，标题栏却嵌在这个包裹层里）；跨层级无法直接重排，故手机端给这一层
          `display: contents` 让它「消失」，其子元素升为 shell 的 flex item。桌面端
          该类无任何样式命中，DOM 与观感逐字节不变。
        */}
        <div className="settings-body flex min-w-0 flex-1 flex-col">
          <div className="settings-header flex items-center justify-between border-b border-line px-4 py-2">
            <div className="flex items-center gap-2">
              {/* 手机端子页的返回：44px 触控目标，与关闭按钮同一标准。 */}
              {isMobile && phonePane === "panel" && (
                <button
                  ref={backRef}
                  type="button"
                  aria-label="返回设置分类"
                  onClick={() => setPhonePane("list")}
                  className="flex size-11 shrink-0 items-center justify-center rounded text-fg-subtle hover:text-fg"
                >
                  ‹
                </button>
              )}
              <span className="text-xs text-fg-subtle">设置</span>
            </div>
            {/* 手机端给足 44px 触控目标（Apple HIG，与抽屉按钮同一标准）——手机上
                遮罩被 CSS 隐藏，关闭只能靠这颗按钮或 Escape。桌面沿用原样（`px-2`），
                保持「桌面逐字节不变」这条硬不变量。 */}
            <button
              type="button"
              aria-label="关闭设置"
              onClick={onClose}
              className={`${
                isMobile ? "flex size-11 shrink-0 items-center justify-center" : "px-2"
              } rounded text-fg-subtle hover:text-fg`}
            >
              ×
            </button>
          </div>
          {isMobile && phonePane === "list" ? (
            // 分类列表：每行一个分类，点进去才渲染面板。
            <nav className="settings-categories flex min-h-0 flex-1 flex-col">
              {TABS.map((t) => (
                <button
                  key={t.id}
                  ref={(el) => {
                    categoryRefs.current[t.id] = el;
                  }}
                  type="button"
                  data-settings-category={t.id}
                  onClick={() => {
                    setActive(t.id);
                    setPhonePane("panel");
                  }}
                  className="flex items-center justify-between border-b border-line px-4 text-left text-sm text-fg"
                >
                  {t.label}
                  <span aria-hidden="true" className="text-fg-subtle">
                    ›
                  </span>
                </button>
              ))}
            </nav>
          ) : (
            // 一次只渲染选中的面板；id/aria-labelledby 在桌面端与 tablist 成对，
            // 手机端没有 tablist，故那对属性不再有匹配目标（无害，保留以免两端分叉）。
            <div
              role="tabpanel"
              id={panelId(active)}
              aria-labelledby={isMobile ? undefined : tabId(active)}
              className="settings-panel min-h-0 flex-1 overflow-y-auto"
            >
              {active === "general" ? (
                <SettingsGeneralTab theme={theme} onThemeChange={onThemeChange} />
              ) : active === "remote" ? (
                <SettingsRemoteTab
                  call={remoteCall}
                  initialRelayUrl={relayUrl}
                  saveRelayUrl={saveRelayUrl}
                />
              ) : active === "plugins" ? (
                <SettingsPluginsTab
                  call={pluginCall}
                  projects={pluginProjects ?? []}
                  defaultProject={pluginDefaultProject}
                />
              ) : active === "models" ? (
                <SettingsModelsTab call={modelCall} isMobile={isMobile} />
              ) : (
                <SettingsSkillsTab
                  call={skillsCall}
                  projects={skillsProjects ?? []}
                  defaultProject={skillsDefaultProject}
                />
              )}
            </div>
          )}
        </div>
```

5. 返回时把焦点还给来源行：把 `setPhonePane("list")` 的 `onClick` 改为：

```tsx
                  onClick={() => {
                    // 记下来源分类，等列表挂载后由上面的 effect 落焦。
                    setPendingFocus(active);
                    setPhonePane("list");
                  }}
```

> 落焦走 effect（`pendingFocus`）而不是 `requestAnimationFrame` / `setTimeout`：`fireEvent` 是同步派发，effect 在 React 的 `act` 内同步 flush，测试可直接断言 `document.activeElement`；定时器方案在 jsdom 里既不可靠也要额外等帧。

- [ ] **Step 4: 改 `index.css`**

删除 `[data-mobile="true"] .settings-nav { ... }` 与 `[data-mobile="true"] .settings-nav > [role="tab"] { ... }` 两条规则（约 355-373 行，含上方注释），替换为：

```css
/*
 * 手机端设置首页：分类列表（纵向一列，每行满宽）。
 *
 * 取代原先的等宽横条——分类到 5 个时 320pt 每格只剩 64px，「远程访问」四字会溢出。
 * 列表把横向宽度从约束里彻底摘掉，以后加分类不必再调尺寸。
 */
[data-mobile="true"] .settings-categories {
  flex-direction: column;
  overflow-y: auto;
  overscroll-behavior: contain;
}
[data-mobile="true"] .settings-categories > [data-settings-category] {
  /* 单行 ≈ 44px：上下各 12px 内边距 + 行高，达到 Apple HIG 触控下限。
     `min-height` 兜住大字号设置下的极端情况。 */
  min-height: 44px;
  padding-top: 12px;
  padding-bottom: 12px;
  white-space: nowrap;
}
```

再删除 `[data-mobile="true"] .settings-body` 块里的 `order` 依赖说明与 `[data-mobile="true"] .settings-nav { order: 2; }` 那条（手机端已不渲染 nav，`order` 无对象），并把 `.settings-header` 的注释改为：

```css
/*
 * 手机端的层级重排：手机端不渲染 nav，`.settings-body { display: contents }` 让
 * header 与 panel 直接升为 shell 的 flex item，DOM 顺序即视觉顺序 [header, panel]，
 * 因此不再需要 `order`。顶部安全区（刘海）内边距仍挂在 header 上。
 */
[data-mobile="true"] .settings-body {
  display: contents;
}
[data-mobile="true"] .settings-header {
  /* 升为 flex item 后必须锁住自身高度，否则会被纵向 flex 压缩。 */
  flex-shrink: 0;
}
```

- [ ] **Step 5: 跑测试确认通过**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/skills-settings-panel/desktop
npx vitest run src/components/SettingsDialog.test.tsx src/mobileLayout.test.ts
npx tsc --noEmit
```

Expected: PASS。

- [ ] **Step 6: 跑前端全量测试确认无回归**

```bash
npx vitest run
```

Expected: PASS。若有别的用例断言手机端 tablist，按本次契约改写它（`queryByRole("tablist")` 为 null）。

- [ ] **Step 7: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/skills-settings-panel
git add desktop/src/components/SettingsDialog.tsx desktop/src/components/SettingsDialog.test.tsx desktop/src/index.css desktop/src/mobileLayout.test.ts
git commit -m "feat(desktop): two-level settings navigation on the phone

Five categories no longer fit an equal-width tab strip on a 320pt
screen (64px per cell vs an 80px label). The phone now shows a category
list and drills into one panel, which removes horizontal width as a
constraint. Desktop keeps the vertical tablist untouched."
```

---

### Task 8: `App.tsx` 接线

**Files:**
- Modify: `desktop/src/App.tsx`
- Test: `desktop/src/App.test.tsx`（仅在既有用例覆盖该 prop 传递时才动）

**Interfaces:**
- Consumes: Task 6 的 `skillsCall` / `skillsProjects` / `skillsDefaultProject`。
- Produces: 无新增导出。

- [ ] **Step 1: 加载前确认项目候选的现有来源**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/skills-settings-panel/desktop
grep -n "pluginProjectPaths" -A 6 src/App.tsx | head -20
```

Expected: 看到 `pluginProjects={pluginProjectPaths(workspaces, boards)}` 与
`pluginDefaultProject={current?.info?.cwd}`（约 2475 行）。skill 面板复用**同一份**候选与默认值——两个面板解析的都是「项目目录下的 `.yi-agent`」，候选来源不该分叉。

- [ ] **Step 2: 加稳定接缝**

在 `modelCall` 的 `useCallback` 之后追加：

```tsx
  /**
   * 「技能」Tab 的宿主接缝。理由同 `pluginCall`：只闭合 `clientRef`，空依赖即稳定；
   * 若每次渲染换新身份，技能面板的读取 effect 会跟着重跑，把已展开的正文与滚动
   * 位置一起冲掉。
   */
  const skillsCall = useCallback(
    (method: string, params: unknown): Promise<unknown> => {
      const c = clientRef.current;
      if (!c) return Promise.reject(new Error("not connected"));
      return c.request(method, params);
    },
    [],
  );
```

- [ ] **Step 3: 传给 `SettingsDialog`**

在 `modelCall={modelCall}` 之后追加（注释里的理由与插件那两行同源）：

```tsx
        // 技能三层根同样按项目解析（`<项目>/.yi-agent/skills`），候选与默认值直接
        // 复用插件那份——两者指的都是「项目目录下的 .yi-agent」，来源分叉会让用户
        // 在两个面板里看到不同的项目列表。
        skillsCall={skillsCall}
        skillsProjects={pluginProjectPaths(workspaces, boards)}
        skillsDefaultProject={current?.info?.cwd}
```

- [ ] **Step 4: 验证**

```bash
export PATH=/opt/homebrew/bin:$PATH
npx tsc --noEmit && npx vitest run
```

Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/skills-settings-panel
git add desktop/src/App.tsx
git commit -m "feat(desktop): wire the skills channel into settings"
```

---

### Task 9: 端到端验证与合并前检查

**Files:** 无新增；仅验证。

- [ ] **Step 1: 后端全量 + 格式**

```bash
export PATH=/opt/homebrew/bin:$PATH
ps aux | grep -v grep | grep cargo || true   # 必须为空
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/skills-settings-panel/yi-agent-rs
cargo fmt --all
cargo test -p yi-agent-app-server 2>&1 | tail -8
cargo test -p yi-agent-skills 2>&1 | tail -8
cargo test -p yi-agent-runtime 2>&1 | tail -8
```

Expected: 三组全 PASS，`git diff --exit-code` 在 `cargo fmt` 后无输出（即已格式化）。

- [ ] **Step 2: 前端全量 + 类型**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/skills-settings-panel/desktop
npx vitest run 2>&1 | tail -20
npx tsc --noEmit
```

Expected: PASS，无类型错误。

- [ ] **Step 3: 手工核对桌面硬不变量**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/skills-settings-panel
git diff main -- desktop/src/index.css | grep -E "^[-+].*settings-nav" | head -20
```

Expected: 只有**删除**手机上那两条 `.settings-nav` 规则，**没有**任何改动桌面的 `.settings-nav` 规则（桌面那条规则在 `SettingsDialog.tsx` 的 Tailwind 类里，CSS 文件里本就没有桌面专属的 `.settings-nav` 规则）。

- [ ] **Step 4: 核对 spec 覆盖**

逐条对照 `docs/superpowers/specs/2026-10-17-skills-settings-panel-design.md` §2 目标与 §4 测试清单，确认：

- 三层分组列出（Task 1/2/5）
- 点开展开只读主文件正文（Task 1/3/5）
- 添加 skill 的说明（Task 5）
- 只读、非特权、iOS 可用（Task 2 的 `Observe` 测试）
- 桌面逐字节不变（Task 7 契约 + Task 9 Step 3）
- 手机端两级导航（Task 7）

- [ ] **Step 5: 合并回 main**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
git checkout main
git merge --no-ff docs/skills-settings-panel
```

Expected: 无冲突（该分支只碰本计划列出的文件）。合并后按 CLAUDE.md 删除分支与 worktree：

```bash
git branch -d docs/skills-settings-panel
git worktree remove .worktrees/skills-settings-panel
```

---

## 附：需要留意的既有代码事实

- `SkillMetadata` 含 `body`，但 `skill/list` **不**下发它——正文走 `skill/read`。
- `discover_skills` 跳过以 `.` 开头的目录，因此 `.system` 不会被用户层重复发现（Task 1 有用例钉住）。
- `SkillsService::snapshot()` 有缓存；`skill/list` 每次新建实例故每次重扫——面板打开时才调用，代价可接受，且能立刻看到用户刚放进目录的 skill。
- `install_system_skills` 按 SHA-256 跳过同内容文件，故重复调用不改变 mtime（Task 1 有用例钉住）。
- 手机端 `.settings-body { display: contents }` 是既有机制，本次只去掉它的 `order` 依赖（手机端已无 nav）。
