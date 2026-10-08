# 卡片会话与 worktree 显式回收 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 删除卡片会话时，经既有 `force` 两步确认连带删除其 worktree；无损删除静默完成，会丢数据或该保留的一律要让用户看见。

**Architecture:** 三处切分。①**判定与执行模块**（`yi-agent-app-server/src/worktree_reclaim.rs`）：`decide()` 用 git 事实判「可无损删 / 会丢失 / 硬保留」，`remove()` 执行 `git worktree remove`；分类逻辑抽成纯函数以便脱 git 单测。②**RPC 接线**（`server.rs` 的 `thread/delete`）：把判定结果翻成 `needs_confirmation` 的新 `worktree` 字段，或在 force 后执行删除。③**桌面确认框**（`desktop/src/App.tsx` 的 `deleteThread`）：把 worktree 后果渲染进 `window.confirm`。

**Tech Stack:** Rust（edition 2024）、`serde_json`、`git` CLI（`std::process::Command`）；桌面 TypeScript + React 19 + vitest。

**依据 spec:** `docs/superpowers/specs/2026-10-08-card-session-worktree-reclaim-design.md`（§3 决策 D1–D10、§4 触发与确认、§5 判定、§8/§11 测试）。

## Global Constraints

- **不改插件**（`plugins/superpowers-kanban/`）：本设计只读会话 meta 自带的 `board_project`/`card_id`，不调 `board.*` RPC。
- **不为一个功能动 `yi-agent-store` 的公共面**：新模块留在 `yi-agent-app-server` 内。
- 普通会话（`card_id` 为 `None`）的 cwd 是用户项目目录，**任何情况**都不做 worktree 删除（D7）。
- worktree 删除只在**三个条件同时成立**时允许：`card_id` 非空 **且** cwd 落在 `<board_project>/.worktrees/kanban/` 之下 **且** 该路径出现在 `git worktree list` 中（D6）。
- 一切判定用 **git 事实**，不引入任何"模型/前端自述"。
- Rust 注释中文、讲「为什么」；测试名英文 snake_case，配中文 `///` 说明锁住的契约。
- 桌面测试：`export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run <file>`；类型检查 `npx tsc --noEmit`。组件测试文件首行 `/** @vitest-environment jsdom */`（`App.test.tsx` 已有）。
- commit 用 conventional commits，正文中文，**不写 `Co-Authored-By`**。
- Rust 测试：`cd yi-agent-rs && cargo test -p yi-agent-app-server <name>`（不要并发跑多个 `cargo test`）。

---

## File Structure

| 文件 | 职责 | 动作 |
|---|---|---|
| `yi-agent-rs/crates/yi-agent-app-server/src/worktree_reclaim.rs` | `WorktreeReclaim` 类型 + 纯分类 `classify` + git 判定 `decide` + 执行 `remove` + `slugify` | 新建 |
| `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs` | 注册 `pub(crate) mod worktree_reclaim;` | 改 |
| `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` | `thread/delete` 分支接入判定与执行 | 改 |
| `desktop/src/App.tsx` | `deleteThread` 渲染 worktree 后果 | 改 |
| `desktop/src/App.test.tsx` | 新增两条 worktree 确认用例 | 改 |

依赖方向：`server.rs` → `worktree_reclaim.rs`（单向）。

---

### Task 1: `worktree_reclaim` 判定与执行模块

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/worktree_reclaim.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`
- Test: `worktree_reclaim.rs` 底部 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `std::process::Command`、`std::path::{Path, PathBuf}`（无 crate 内部依赖）。
- Produces:
  - `pub enum WorktreeReclaim { None, Delete { path: PathBuf }, DestructiveDelete { path: PathBuf, reason: String }, Keep { path: PathBuf, reason: String } }`（`Debug, Clone, PartialEq, Eq`）
  - `pub fn slugify(card_id: &str) -> String`
  - `pub fn default_branch(project: &Path) -> String`
  - `pub fn classify(is_card_session: bool, under_kanban_dir: bool, in_worktree_list: bool, dirty: bool, branch: BranchState, path: PathBuf) -> WorktreeReclaim`
  - `pub enum BranchState { Missing, Merged, Unmerged }`
  - `pub fn decide(card_id: Option<&str>, board_project: Option<&str>, cwd: &Path) -> WorktreeReclaim`
  - `pub fn remove(path: &Path, project: &Path, force: bool) -> Result<(), String>`

- [ ] **Step 1: 写失败测试**

创建 `src/worktree_reclaim.rs`，先只放测试（模块本体留空，令编译走到断言）：

```rust
#[cfg(test)]
mod tests {
    // 测试里用简名 `Reclaim` 指代 `WorktreeReclaim`，与 spec 措辞一致。
    use super::WorktreeReclaim as Reclaim;
    use super::*;
    use std::process::Command;

    /// 建一个真 git 仓库（main 分支，一次提交），返回其 canonical 路径。
    fn init_repo(dir: &Path) -> PathBuf {
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(dir)
                    .args(args)
                    .status()
                    .unwrap()
                    .success(),
                "git {args:?}"
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        // 与 Task 2 的 `git_project` 同一理由：真实项目的 `.yi-agent/` 是 gitignored 的，
        // 复刻它，判定才是按生产事实走（见 Task 2 里的详细说明）。
        std::fs::write(dir.join(".gitignore"), ".yi-agent/\n.worktrees/\n").unwrap();
        std::fs::write(dir.join("f.txt"), "hi").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "init"]);
        dir.canonicalize().unwrap()
    }

    /// 在 `<project>/.worktrees/kanban/<slug>` 建一块真 worktree，分支 `kanban/<slug>`。
    fn add_kanban_worktree(project: &Path, card_id: &str) -> PathBuf {
        let slug = slugify(card_id);
        let path = project.join(".worktrees/kanban").join(&slug);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let branch = format!("kanban/{slug}");
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(project)
                .args(["worktree", "add", "-q"])
                .arg(&path)
                .args(["-b", &branch])
                .status()
                .unwrap()
                .success()
        );
        path
    }

    fn git(project: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(project)
                .args(args)
                .status()
                .unwrap()
                .success(),
            "git {args:?}"
        );
    }

    #[test]
    fn slugify_matches_the_plugin_rule() {
        // 必须与插件 `worktree::slugify` 逐字一致，否则"分支名"对不上。
        assert_eq!(slugify("card-1"), "card-1");
        assert_eq!(slugify("Card 2 foo/bar"), "card-2-foo-bar");
        assert_eq!(slugify("///"), "card");
    }

    /// `classify` 的最后一个参数是路径，分类本身不看它——给个占位即可。
    fn wt() -> PathBuf {
        PathBuf::from("/tmp/wt")
    }

    #[test]
    fn classify_prefers_no_reclaim_for_a_plain_session() {
        assert_eq!(
            classify(false, true, true, false, BranchState::Merged, wt()),
            Reclaim::None,
            "普通会话永不回收 worktree"
        );
    }

    #[test]
    fn classify_keeps_when_not_a_kanban_worktree() {
        // 在 worktree list 里但不是看板 worktree 路径 → 硬保留。
        assert!(matches!(
            classify(true, false, true, false, BranchState::Merged, wt()),
            Reclaim::Keep { .. }
        ));
        // 在 kanban 目录下但 git 不认它是 worktree → 也硬保留。
        assert!(matches!(
            classify(true, true, false, false, BranchState::Merged, wt()),
            Reclaim::Keep { .. }
        ));
    }

    #[test]
    fn classify_deletes_silently_when_clean_and_merged() {
        assert!(matches!(
            classify(true, true, true, false, BranchState::Merged, wt()),
            Reclaim::Delete { .. }
        ));
    }

    #[test]
    fn classify_marks_dirty_or_unmerged_or_missing_as_destructive() {
        assert!(matches!(
            classify(true, true, true, true, BranchState::Merged, wt()),
            Reclaim::DestructiveDelete { .. }
        ));
        assert!(matches!(
            classify(true, true, true, false, BranchState::Unmerged, wt()),
            Reclaim::DestructiveDelete { .. }
        ));
        assert!(matches!(
            classify(true, true, true, false, BranchState::Missing, wt()),
            Reclaim::DestructiveDelete { .. }
        ));
    }

    /// 端到端（真 git）：干净且已并入 → 静默可删。
    #[test]
    fn decide_deletes_a_clean_merged_card_worktree() {
        let dir = tempfile::TempDir::new().unwrap();
        let project = init_repo(&dir.path().join("proj").tap_mkdir());
        let wt = add_kanban_worktree(&project, "card-1");
        let p = project.to_string_lossy().to_string();
        let got = decide(Some("card-1"), Some(&p), &wt);
        assert!(matches!(got, Reclaim::Delete { .. }), "{got:?}");
    }

    /// 端到端：脏 worktree → 破坏性（需确认）。
    #[test]
    fn decide_flags_a_dirty_card_worktree() {
        let dir = tempfile::TempDir::new().unwrap();
        let project = init_repo(&dir.path().join("proj").tap_mkdir());
        let wt = add_kanban_worktree(&project, "card-1");
        std::fs::write(wt.join("scratch.txt"), "wip").unwrap();
        let p = project.to_string_lossy().to_string();
        let got = decide(Some("card-1"), Some(&p), &wt);
        match got {
            Reclaim::DestructiveDelete { reason, .. } => {
                assert!(reason.contains("未提交"), "{reason}")
            }
            other => panic!("expected DestructiveDelete, got {other:?}"),
        }
    }

    /// 端到端：分支未并入 → 破坏性。
    #[test]
    fn decide_flags_an_unmerged_branch() {
        let dir = tempfile::TempDir::new().unwrap();
        let project = init_repo(&dir.path().join("proj").tap_mkdir());
        let wt = add_kanban_worktree(&project, "card-1");
        // 分支上多一个未并入 main 的提交。
        std::fs::write(wt.join("more.txt"), "x").unwrap();
        git(&wt, &["add", "-A"]);
        git(&wt, &["commit", "-q", "-m", "wip"]);
        let p = project.to_string_lossy().to_string();
        match decide(Some("card-1"), Some(&p), &wt) {
            Reclaim::DestructiveDelete { reason, .. } => {
                assert!(reason.contains("未并入"), "{reason}")
            }
            other => panic!("expected DestructiveDelete, got {other:?}"),
        }
    }

    /// 端到端：普通会话（card_id=None）→ None。
    #[test]
    fn decide_returns_none_for_a_plain_session() {
        let dir = tempfile::TempDir::new().unwrap();
        let project = init_repo(&dir.path().join("proj").tap_mkdir());
        assert_eq!(
            decide(None, Some(&project.to_string_lossy()), &project),
            Reclaim::None
        );
    }

    /// `remove`：真删一块 worktree；再删一次失败（已不存在）。
    #[test]
    fn remove_deletes_a_linked_worktree() {
        let dir = tempfile::TempDir::new().unwrap();
        let project = init_repo(&dir.path().join("proj").tap_mkdir());
        let wt = add_kanban_worktree(&project, "card-1");
        remove(&wt, &project, false).expect("clean removal succeeds");
        assert!(!wt.exists(), "worktree dir is gone");
        assert!(remove(&wt, &project, false).is_err(), "second removal fails");
    }

    /// 小工具：把路径当目录建出来，便于链式表达式里构造 temp 子目录。
    trait TapMkdir {
        fn tap_mkdir(&self) -> PathBuf;
    }
    impl TapMkdir for Path {
        fn tap_mkdir(&self) -> PathBuf {
            std::fs::create_dir_all(self).unwrap();
            self.to_path_buf()
        }
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server worktree_reclaim`
Expected: FAIL —— 编译错误 `cannot find function 'slugify'` / `cannot find type 'Reclaim'`（模块本体为空时）。

- [ ] **Step 3: 写实现**

把模块本体写在文件顶部（`#[cfg(test)] mod tests` 保持在文件末尾，Step 1 已建好）。实现里统一用真名 `WorktreeReclaim`；测试为贴合 spec 措辞用简名 `Reclaim`，靠 Step 1 已在 `mod tests` 顶部放好的 `use super::WorktreeReclaim as Reclaim;` 对齐：

```rust
//! 卡片会话 worktree 的判定与删除。
//!
//! 判定只吃 git 事实：`decide` 读「是否看板 worktree / 是否干净 / 分支状态」，
//! 分类抽成纯函数 `classify` 以便脱 git 单测。只服务卡片会话（`card_id` 非空）；
//! 普通会话的 cwd 是用户项目目录，任何情况都不删。

use std::path::{Path, PathBuf};
use std::process::Command;

/// `kanban/<slug>` 的 slug：与插件 `worktree::slugify` 逐字一致。
///
/// 只保留 ASCII 字母数字（小写化），其余折叠为单个 `-`，首尾 `-` 去掉；
/// 全空时退化为 `card`。不一致会让宿主演化出与插件不同的分支名，
/// 从而把"已并入"误判为"分支缺失"（安全侧失败，但会漏删）。
pub fn slugify(card_id: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in card_id.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "card".to_string()
    } else {
        trimmed
    }
}

/// 项目默认分支：`origin/HEAD` 退化到 `main`（与插件 `merge::default_branch` 同口径）。
pub fn default_branch(project: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(project)
        .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .output();
    if let Ok(out) = out {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if let Some(name) = text.strip_prefix("origin/") {
                if !name.is_empty() {
                    return name.to_string();
                }
            }
        }
    }
    "main".to_string()
}

/// 源分支相对默认分支的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchState {
    /// `kanban/<slug>` 不存在：无法判定是否已并入。
    Missing,
    /// 存在且已并入默认分支。
    Merged,
    /// 存在但未并入。
    Unmerged,
}

/// 一次 worktree 回收的判定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeReclaim {
    /// 不适用（普通会话）：不做任何 worktree 动作，响应也不带该字段。
    None,
    /// 可无损删除（干净且已并入）：无需确认，直接删。
    Delete { path: PathBuf },
    /// 确认后会永久丢弃内容：需两步确认。
    DestructiveDelete { path: PathBuf, reason: String },
    /// 即便 force 也不删（硬拒绝），需让用户知道工作被保留及原因。
    Keep { path: PathBuf, reason: String },
}

/// 纯分类：只吃布尔事实与分支状态，便于脱 git 单测。
///
/// 顺序即优先级：非卡片会话 → 先排除；不是看板 worktree → 硬保留；
/// 干净且已并入 → 可删；其余（脏/未并入/分支缺失）→ 破坏性。
pub fn classify(
    is_card_session: bool,
    under_kanban_dir: bool,
    in_worktree_list: bool,
    dirty: bool,
    branch: BranchState,
    path: PathBuf,
) -> WorktreeReclaim {
    if !is_card_session {
        return WorktreeReclaim::None;
    }
    if !under_kanban_dir || !in_worktree_list {
        return WorktreeReclaim::Keep {
            path,
            reason: "cwd 不是本项目登记在册的看板 worktree，拒绝删除".to_string(),
        };
    }
    if !dirty && branch == BranchState::Merged {
        return WorktreeReclaim::Delete { path };
    }
    let reason = if dirty {
        "worktree 有未提交改动，确认后将永久丢弃".to_string()
    } else if branch == BranchState::Missing {
        "源分支已不存在，无法确认已并入，确认后将永久丢弃".to_string()
    } else {
        "源分支未并入默认分支，确认后将永久丢弃".to_string()
    };
    WorktreeReclaim::DestructiveDelete { path, reason }
}

/// git 探针：判定一张卡片会话的 worktree 该删、该确认、还是该留。
pub fn decide(card_id: Option<&str>, board_project: Option<&str>, cwd: &Path) -> WorktreeReclaim {
    let Some(card_id) = card_id.filter(|id| !id.is_empty()) else {
        return WorktreeReclaim::None;
    };
    let Some(project) = board_project.filter(|p| !p.is_empty()) else {
        // 卡片会话必然有项目根；缺失时无从判定，安全侧保留。
        return WorktreeReclaim::Keep {
            path: cwd.to_path_buf(),
            reason: "会话未记录所属项目根，拒绝删除".to_string(),
        };
    };
    let project_path = Path::new(project);
    let cwd_canon = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let kanban_dir = project_path.join(".worktrees").join("kanban");
    let kanban_canon = kanban_dir
        .canonicalize()
        .unwrap_or_else(|_| kanban_dir.clone());
    let under = cwd_canon.starts_with(&kanban_canon);
    let in_list = worktree_list_contains(project_path, &cwd_canon);
    let dirty = !git_ok(cwd, &["status", "--porcelain"])
        .unwrap_or_default()
        .trim()
        .is_empty();
    let branch = format!("kanban/{}", slugify(card_id));
    let base = default_branch(project_path);
    let branch_state = if !git_ok(
        project_path,
        &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")],
    )
    .is_some()
    {
        BranchState::Missing
    } else if git_status_ok(project_path, &["merge-base", "--is-ancestor", &branch, &base]) {
        BranchState::Merged
    } else {
        BranchState::Unmerged
    };
    classify(true, under, in_list, dirty, branch_state, cwd_canon)
}

/// 删除一块 worktree：`git -C <project> worktree remove [--force] <path>`。
pub fn remove(path: &Path, project: &Path, force: bool) -> Result<(), String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(project).args(["worktree", "remove"]);
    if force {
        cmd.arg("--force");
    }
    cmd.arg(path);
    let out = cmd
        .output()
        .map_err(|error| format!("could not run git: {error}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// `git -C <dir> <args>` 成功时返回 stdout（失败/未安装 → None）。
fn git_ok(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

/// 只看退出码（不关心输出）。
fn git_status_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// `cwd` 是否出现在 `<project>` 的 `git worktree list --porcelain` 里。
fn worktree_list_contains(project: &Path, cwd: &Path) -> bool {
    let Some(text) = git_ok(project, &["worktree", "list", "--porcelain"]) else {
        return false;
    };
    let target = cwd.to_string_lossy();
    text.lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .any(|listed| {
            let listed_path = Path::new(listed.trim());
            listed_path.canonicalize().unwrap_or_else(|_| listed_path.to_path_buf())
                == Path::new(target.as_ref())
        })
}
```

- [ ] **Step 4: 对齐测试里的简名并跑通**

别名 `use super::WorktreeReclaim as Reclaim;` 在 Step 1 建 `mod tests` 时就已放在顶部，此处只需确认它在（若 Step 1 漏了会编译报 `Reclaim` 未定义）。

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server worktree_reclaim`
Expected: PASS（`slugify`/`classify`/`decide`/`remove` 共 10 个用例全过）。

- [ ] **Step 5: 注册模块并确认编译无警告**

在 `src/lib.rs` 的模块声明中插入一行（与 `pub(crate) mod git_diff;` 同款可见性，按字母序放在 `pub mod ws;` 之前）：

```rust
pub(crate) mod worktree_reclaim;
```

Run: `cd yi-agent-rs && cargo build -p yi-agent-app-server 2>&1 | grep -i "warning" || echo "no warnings"`
Expected: `no warnings`（若新模块有未用项，据此清掉）。

- [ ] **Step 6: Commit**

```bash
cd yi-agent-rs
git add crates/yi-agent-app-server/src/worktree_reclaim.rs crates/yi-agent-app-server/src/lib.rs
git commit -m "feat(app-server): 卡片 worktree 回收的判定与删除

decide 以 git 事实判定：非看板 worktree 硬保留、干净且已并入可无损删、
脏/未并入/分支缺失为破坏性删除。classify 为纯函数可脱 git 单测，
slugify 与插件逐字一致。"
```

---

### Task 2: `thread/delete` 接入判定与执行

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`
  - `thread/delete` 分支（现 `3567-3669`）：读 meta → 判定 → 组合响应 → force 后执行删除
  - 测试模块：新增 4 个用例 + 复用既有 `write_meta`/`add_workspace`/`initialize` 辅助
- Test: 同上（`server.rs` 的 `mod tests`）

**Interfaces:**
- Consumes: `crate::worktree_reclaim::{decide, remove, WorktreeReclaim}`（Task 1）；既有 `store_lookup`、`cancel_thread_children`、`ThreadStore::load`。
- Produces: `thread/delete` 的 `needs_confirmation` 响应新增 `worktree: { path, action, reason }`；force 成功后按判定删除 worktree。

- [ ] **Step 1: 写失败测试**

在 `server.rs` 的 `mod tests` 内、`write_meta` 辅助之后追加下列辅助与 4 个用例：

```rust
    /// 建一个真 git 仓库（main 分支 + 一次提交），返回 canonical 路径。
    fn git_project(dir: &Path) -> PathBuf {
        let run = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(dir)
                    .args(args)
                    .status()
                    .unwrap()
                    .success(),
                "git {args:?}"
            );
        };
        std::fs::create_dir_all(dir).unwrap();
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        // 复刻真实项目的 .gitignore：宿主运行态 `.yi-agent/` 本就被忽略（见仓库根
        // .gitignore）。少了它，`write_meta` 写下的 `<cwd>/.yi-agent/…` 在 worktree
        // 内是 untracked——git 会判"脏"，`worktree remove` 也会被拒。这不是测试造作，
        // 而是生产里同样被忽略：不补它，"干净且已并入 → 静默删"那条根本走不到。
        std::fs::write(dir.join(".gitignore"), ".yi-agent/\n.worktrees/\n").unwrap();
        std::fs::write(dir.join("f.txt"), "hi").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "init"]);
        dir.canonicalize().unwrap()
    }

    /// 在 `<project>/.worktrees/kanban/<slug>` 建真 worktree，返回其 canonical 路径。
    fn git_kanban_worktree(project: &Path, card_id: &str) -> PathBuf {
        let slug = crate::worktree_reclaim::slugify(card_id);
        let path = project.join(".worktrees/kanban").join(&slug);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(project)
                .args(["worktree", "add", "-q"])
                .arg(&path)
                .args(["-b", &format!("kanban/{slug}")])
                .status()
                .unwrap()
                .success()
        );
        path.canonicalize().unwrap()
    }

    /// 卡片会话（cwd 在 worktree、board_project=项目）删除：干净且已并入 → 会话与 worktree 一并消失。
    #[tokio::test(flavor = "multi_thread")]
    async fn deleting_a_card_session_also_removes_its_worktree() {
        let project_dir = tempfile::TempDir::new().unwrap();
        let project = git_project(&project_dir.path().join("proj"));
        let wt = git_kanban_worktree(&project, "card-1");
        write_meta(&wt, "t-card", "看板 · card-1",
            Some(&project.to_string_lossy()), Some("card-1"));

        let mut h = Harness::new();
        initialize(&mut h).await;
        add_workspace(&mut h, 11, &wt.to_string_lossy()).await;

        h.send(r#"{"jsonrpc":"2.0","id":5,"method":"thread/delete","params":{"threadId":"t-card"}}"#)
            .await;
        let v = read_response(&mut h, 5).await;
        assert!(v.get("error").is_none(), "delete must succeed: {v}");
        assert!(!wt.join(".yi-agent/threads/t-card.meta.json").exists(), "session gone");
        assert!(!wt.exists(), "clean+merged worktree must be removed");
        h.shutdown().await;
    }

    /// 脏 worktree：未 force 返回 needs_confirmation 且 worktree.action="remove"；会话仍在。
    #[tokio::test(flavor = "multi_thread")]
    async fn deleting_a_card_session_with_dirty_worktree_needs_confirmation() {
        let project_dir = tempfile::TempDir::new().unwrap();
        let project = git_project(&project_dir.path().join("proj"));
        let wt = git_kanban_worktree(&project, "card-1");
        std::fs::write(wt.join("scratch.txt"), "wip").unwrap();
        write_meta(&wt, "t-card", "看板 · card-1",
            Some(&project.to_string_lossy()), Some("card-1"));

        let mut h = Harness::new();
        initialize(&mut h).await;
        add_workspace(&mut h, 11, &wt.to_string_lossy()).await;

        h.send(r#"{"jsonrpc":"2.0","id":5,"method":"thread/delete","params":{"threadId":"t-card"}}"#)
            .await;
        let v = read_response(&mut h, 5).await;
        assert_eq!(v["result"]["status"], "needs_confirmation", "{v}");
        assert_eq!(v["result"]["worktree"]["action"], "remove", "{v}");
        assert!(
            v["result"]["worktree"]["reason"].as_str().unwrap().contains("未提交"),
            "{v}"
        );
        assert!(wt.join(".yi-agent/threads/t-card.meta.json").exists(), "session untouched");

        // force 重发 → 一并删除。
        h.send(r#"{"jsonrpc":"2.0","id":6,"method":"thread/delete","params":{"threadId":"t-card","force":true}}"#)
            .await;
        let v = read_response(&mut h, 6).await;
        assert!(v.get("error").is_none(), "forced delete succeeds: {v}");
        assert!(!wt.exists(), "worktree destroyed after confirmation");
        h.shutdown().await;
    }

    /// 普通会话：响应不含 worktree 字段，cwd 目录原样存在（D7 的钉子）。
    #[tokio::test(flavor = "multi_thread")]
    async fn deleting_a_plain_session_leaves_its_directory_alone() {
        let dir = tempfile::TempDir::new().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        write_meta(&cwd, "t-plain", "plain", None, None);
        let marker = cwd.join("keep-me.txt");
        std::fs::write(&marker, "x").unwrap();

        let mut h = Harness::new();
        initialize(&mut h).await;
        add_workspace(&mut h, 11, &cwd.to_string_lossy()).await;

        h.send(r#"{"jsonrpc":"2.0","id":5,"method":"thread/delete","params":{"threadId":"t-plain"}}"#)
            .await;
        let v = read_response(&mut h, 5).await;
        assert!(v.get("error").is_none(), "{v}");
        assert!(v["result"].get("worktree").is_none(), "no worktree field for plain sessions: {v}");
        assert!(marker.exists(), "the user's own directory must be untouched");
        h.shutdown().await;
    }

    /// 防呆：cwd 在项目根而非看板 worktree 路径 → 即便带 card_id 也硬保留。
    #[tokio::test(flavor = "multi_thread")]
    async fn deleting_a_card_session_outside_the_kanban_dir_keeps_the_directory() {
        let project_dir = tempfile::TempDir::new().unwrap();
        let project = git_project(&project_dir.path().join("proj"));
        // cwd = 项目根（不是 .worktrees/kanban/... 下的 worktree）。
        write_meta(&project, "t-card", "看板 · odd", Some(&project.to_string_lossy()), Some("card-1"));
        let marker = project.join("keep-me.txt");
        std::fs::write(&marker, "x").unwrap();

        let mut h = Harness::new();
        initialize(&mut h).await;
        add_workspace(&mut h, 11, &project.to_string_lossy()).await;

        h.send(r#"{"jsonrpc":"2.0","id":5,"method":"thread/delete","params":{"threadId":"t-card"}}"#)
            .await;
        let v = read_response(&mut h, 5).await;
        assert_eq!(v["result"]["worktree"]["action"], "keep", "{v}");
        // force 也不删。
        h.send(r#"{"jsonrpc":"2.0","id":6,"method":"thread/delete","params":{"threadId":"t-card","force":true}}"#)
            .await;
        let _ = read_response(&mut h, 6).await;
        assert!(marker.exists() && project.join("f.txt").exists(), "hard-refused: dir intact");
        h.shutdown().await;
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server deleting_a_`
Expected: FAIL —— 三条卡片会话用例（`deleting_a_card_session*`）因 worktree 未被删／响应无 `worktree` 字段而失败；`deleting_a_plain_session...` 通过（既有行为）。

- [ ] **Step 3: 改造 `thread/delete` 分支**

在 `server.rs` 的 `"thread/delete"` 分支内，把 `let force = ...` 之后、`match cancel_thread_children(...)` 之前的代码，替换为下面这段（它先算 worktree 判定，并把判定要用到的 `board_project` 一并留到局部变量——会话文件删掉后就再也 `load` 不到了）：

```rust
                        let force = req
                            .params
                            .get("force")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false);
                        // 卡片会话的 worktree 判定：只读会话自带 meta，不碰插件。
                        // 顺手把 board_project 拷到局部变量：会话文件随后即被删除，
                        // 到执行删除时再 load 只会失败，故必须在此刻留下。
                        let mut board_project: Option<String> = None;
                        let reclaim = match thread_store.load(&thread_id) {
                            Ok(Some(loaded)) => {
                                board_project = loaded.meta.board_project.clone();
                                crate::worktree_reclaim::decide(
                                    loaded.meta.card_id.as_deref(),
                                    loaded.meta.board_project.as_deref(),
                                    Path::new(&loaded.meta.cwd),
                                )
                            }
                            _ => crate::worktree_reclaim::WorktreeReclaim::None,
                        };
```

接着，把原来的 `match cancel_thread_children(&runtimes, &threads, &thread_id, force) { ... }` 整段**替换**为下面这版：未 force 且（有活跃子代理 **或** worktree 处置需确认）时，统一返回 `needs_confirmation`；force 时才真正回收子代理。

```rust
                        // 「需确认」= 有活跃子代理（既有）或 worktree 处置为破坏性/硬保留（新）。
                        let children = if force {
                            None
                        } else {
                            match cancel_thread_children(&runtimes, &threads, &thread_id, false) {
                                Ok(ThreadCancelOutcome::NeedsConfirmation(count)) => Some(count),
                                // 无事可取消时按 0 处理，交给 worktree 判定决定是否确认。
                                Ok(ThreadCancelOutcome::Cancelled(_)) => Some(0),
                                // 查询失败（如会话不在内存）按 0 处理，但照旧记日志——与既有行为一致。
                                Err(cause) => {
                                    eprintln!(
                                        "[app-server] could not cancel subagents for {thread_id}: {cause}"
                                    );
                                    Some(0)
                                }
                            }
                        };
                        let worktree_needs_confirm = matches!(
                            reclaim,
                            crate::worktree_reclaim::WorktreeReclaim::DestructiveDelete { .. }
                                | crate::worktree_reclaim::WorktreeReclaim::Keep { .. }
                        );
                        if !force && (children.unwrap_or(0) > 0 || worktree_needs_confirm) {
                            let mut payload = json!({
                                "status": "needs_confirmation",
                                "active_children": children.unwrap_or(0),
                            });
                            if let Some(worktree) = worktree_json(&reclaim) {
                                payload["worktree"] = worktree;
                            }
                            write_response(&hub, &client, ok_response(id, payload)).await?;
                            continue;
                        }
                        if force {
                            // force 下真的回收子代理（既有语义）。
                            match cancel_thread_children(&runtimes, &threads, &thread_id, true) {
                                Ok(ThreadCancelOutcome::Cancelled(cancelled)) if cancelled > 0 => {
                                    eprintln!(
                                        "[app-server] cancelled {cancelled} subagent task(s) for {thread_id}"
                                    );
                                }
                                Err(cause) => eprintln!(
                                    "[app-server] could not cancel subagents for {thread_id}: {cause}"
                                ),
                                _ => {}
                            }
                        }
```

然后，在该分支**末尾**、删掉会话文件之后的 `write_response(... ok_response(id, json!({})) ...)` **之前**，插入 worktree 的删除执行。此刻会话已摘除、watch 已停，按判定处置 worktree 才是安全的：

```rust
                        // 会话文件已摘除：按判定处置 worktree。删除失败只记日志，不阻断会话删除（D9）。
                        // board_project 已在进分支时留好——此处绝不能再 load（会话文件已不存在）。
                        match (&reclaim, board_project.as_deref()) {
                            (
                                crate::worktree_reclaim::WorktreeReclaim::Delete { path }
                                | crate::worktree_reclaim::WorktreeReclaim::DestructiveDelete {
                                    path, ..
                                },
                                Some(project),
                            ) => {
                                if let Err(error) = crate::worktree_reclaim::remove(
                                    path,
                                    Path::new(project),
                                    force,
                                ) {
                                    eprintln!(
                                        "[app-server] could not remove the worktree for {thread_id}: {error}"
                                    );
                                }
                            }
                            _ => {}
                        }
```

Task 1 已保证：`Delete`/`DestructiveDelete` 之外的判定（`None`/`Keep`）必然携带 `None` 的 `board_project` 也照样不匹配此 `match`，故不会误删。删除路径无需再读第二次会话文件。
- [ ] **Step 4: 加 `worktree_json` 辅助**

在 `server.rs` 的 `thread_summary_json` 附近新增一个把判定翻成 wire 形状的辅助（`worktree` 字段仅对卡片会话存在）：

```rust
/// 把 worktree 判定渲染成 `thread/delete` 的 `needs_confirmation.worktree` 字段。
///
/// `None` 表示"不适用/无需呈现"（普通会话、或不回收）——此时响应不带该字段，
/// 普通会话的既有响应因此零变化。
fn worktree_json(reclaim: &crate::worktree_reclaim::WorktreeReclaim) -> Option<serde_json::Value> {
    use crate::worktree_reclaim::WorktreeReclaim;
    match reclaim {
        WorktreeReclaim::None => None,
        WorktreeReclaim::Delete { path } => Some(json!({
            "path": path.to_string_lossy(),
            "action": "remove",
            "reason": "干净且源分支已并入，删除不会丢失工作",
        })),
        WorktreeReclaim::DestructiveDelete { path, reason } => Some(json!({
            "path": path.to_string_lossy(),
            "action": "remove",
            "reason": reason,
        })),
        WorktreeReclaim::Keep { path, reason } => Some(json!({
            "path": path.to_string_lossy(),
            "action": "keep",
            "reason": reason,
        })),
    }
}
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server deleting_a_`
Expected: PASS（4 个新用例全过）。

- [ ] **Step 6: 跑 `thread/delete` 既有用例确认零回归**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server thread_delete`
Expected: PASS，包含既有 `a_control_client_cannot_delete_a_thread`、`an_admin_client_may_delete_a_thread` 及附件清理等用例；若某用例断言了 `needs_confirmation` 响应的**精确**形状，按"仅放宽新增字段"微调。

- [ ] **Step 7: Commit**

```bash
cd yi-agent-rs
git add crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): thread/delete 连带显式回收卡片 worktree

未 force 且 worktree 处置为破坏性/硬保留时，纳入既有 needs_confirmation
（新增 worktree 字段）；force 完成会话删除后按判定删/留 worktree，
删除失败只记日志不阻断会话删除。普通会话响应零变化。"
```

---

### Task 3: 桌面确认框渲染 worktree 后果

**Files:**
- Modify: `desktop/src/App.tsx`（`deleteThread`，现 `845-867`）
- Test: `desktop/src/App.test.tsx`（在既有 `thread/delete` 用例旁，现 `1850`/`1876`）

**Interfaces:**
- Consumes: 后端新增的 `worktree: { path, action, reason }`（Task 2）。
- Produces: 无对外接口；本任务让用户看见 worktree 后果。

- [ ] **Step 1: 写失败测试**

`App.test.tsx` 里现有两条 `thread/delete` 用例，位于 `describe("deleting a thread that still runs subagents", ...)`（约 `1826-1894`）。该 describe 内已定义两个辅助：一个取删除按钮的 `deleteButton()`，一个 `clickDelete()`——它 `render(<App />)`、等 `thread/listAll` 发出、按标题 `"one"` 找到侧栏行、`mouseEnter` 唤出控件、点删除。

在这两条既有用例之后、**同一个 describe 内**追加下面两条（复用现成的 `clickDelete`，不新增任何辅助）：

```tsx
  it("把 worktree 将被删除写进确认框（即便没有子代理）", async () => {
    state.threads = [{ thread_id: "t1", title: "one", permission_mode: "normal" }];
    // active_children 为 0：这一回要求确认的是 worktree，而非子代理。
    state.dataSources["thread/delete"] = () => ({
      status: "needs_confirmation",
      active_children: 0,
      worktree: {
        path: "/proj/.worktrees/kanban/card-1",
        action: "remove",
        reason: "worktree 有未提交改动，确认后将永久丢弃",
      },
    });
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(true);
    try {
      await clickDelete();

      await waitFor(() => expect(confirm).toHaveBeenCalled());
      const message = String(confirm.mock.calls[0][0]);
      // 用户必须看见：删的是哪个目录、为什么、以及不可逆。
      expect(message).toContain("/proj/.worktrees/kanban/card-1");
      expect(message).toContain("永久丢弃");
      expect(message).toContain("继续？");

      // 同意后照旧以 force 重发。
      await waitFor(() =>
        expect(clients[0].requests).toContainEqual({
          method: "thread/delete",
          params: { threadId: "t1", force: true },
        }),
      );
    } finally {
      confirm.mockRestore();
    }
  });

  it("worktree 将被保留时如实告知，且不谎称删除", async () => {
    state.threads = [{ thread_id: "t1", title: "one", permission_mode: "normal" }];
    state.dataSources["thread/delete"] = () => ({
      status: "needs_confirmation",
      active_children: 0,
      worktree: {
        path: "/proj/not-a-worktree",
        action: "keep",
        reason: "cwd 不是本项目登记在册的看板 worktree，拒绝删除",
      },
    });
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    try {
      await clickDelete();

      await waitFor(() => expect(confirm).toHaveBeenCalled());
      const message = String(confirm.mock.calls[0][0]);
      // 必须说"保留"，且绝不能反过来说"将一并删除"。
      expect(message).toContain("将保留");
      expect(message).not.toContain("将一并删除");
      expect(message).toContain("继续？");
    } finally {
      confirm.mockRestore();
    }
  });
```

这两条与紧邻的既有用例共用同一套手法（`state.threads` / `state.dataSources[...]` 桩 / `vi.spyOn(window, "confirm")` / `clickDelete()`），无需新增辅助；`confirm` 必须在 `finally` 里 `mockRestore`，与既有用例保持一致。

- [ ] **Step 2: 跑测试确认失败**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/App.test.tsx`
Expected: FAIL —— 新用例的消息断言不成立（当前确认框只讲子代理数）。

- [ ] **Step 3: 改 `deleteThread`**

把 `App.tsx` 的 `deleteThread`（`845-867`）中从 `const first = await c.request<...>` 到 `if (!ok) return;` 之间替换为：

```tsx
      const first = await c.request<{
        status?: string;
        active_children?: number;
        worktree?: { path: string; action: "remove" | "keep"; reason: string };
      }>("thread/delete", { threadId: id });
      if (first?.status === "needs_confirmation") {
        const count = first.active_children ?? 0;
        const lines: string[] = [];
        if (count > 0) {
          lines.push(`这个会话还有 ${count} 个子代理正在运行，删除会一并终止它们。`);
        }
        if (first.worktree) {
          lines.push(
            first.worktree.action === "remove"
              ? `将一并删除卡片 worktree：${first.worktree.path}（${first.worktree.reason}）`
              : `卡片 worktree 将保留：${first.worktree.reason}`,
          );
        }
        // 兜底：后端返回了需要确认、却没给出任何可读原因时，绝不弹空框。
        if (lines.length === 0) lines.push("删除不可逆。");
        lines.push("继续？");
        const ok = window.confirm(lines.join("\n"));
        if (!ok) return;
        await c.request("thread/delete", { threadId: id, force: true });
      }
```

- [ ] **Step 4: 跑测试与类型检查**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/App.test.tsx && npx tsc --noEmit`
Expected: PASS（含既有两条 `:1850`/`:1876` 与新增两条），tsc 无错误。

- [ ] **Step 5: Commit**

```bash
cd desktop
git add src/App.tsx src/App.test.tsx
git commit -m "feat(desktop): 删除确认框如实呈现 worktree 后果

needs_confirmation 现在可能因 worktree 而非子代理出现（active_children=0）；
确认框分两段拼装（子代理/ worktree），并加空消息兜底，绝不弹空框。"
```

---

## 验收

全部 Task 完成后，逐条核对 spec §8：

1. `cd yi-agent-rs && cargo test -p yi-agent-app-server` 全绿（含既有 `thread/delete` 族）。
2. `cd desktop && npx tsc --noEmit && npx vitest run` 全绿。
3. 人可验证：在一张卡片会话上删除，确认框写明 worktree 将删/将留及原因；同意后会话与
   `.worktrees/kanban/<slug>` 一并消失；删除一张**未合并**卡的会话时 worktree 被保留并说明原因；
   删手工会话时项目根毫发无损。
4. 反向：`grep -rn "worktree" yi-agent-rs/crates/yi-agent-app-server/src/server.rs` 中对 worktree 的处理
   只出现在 `thread/delete` 路径与新辅助，不扩散到别处。

## 说明：两处刻意的实现选择

- **`decide` 在 force 与非 force 各跑一次**：与 spec §9「删前重算判据」一致——两次调用相互独立，
  不缓存第一次结论。`thread/delete` 每次调用都会调 `decide`（只读 git，代价可接受）。
- **`thread_store.load` 读一次 meta**：删除路径非热路径，读全量 thread 只为取 meta 可接受；
  若后续有其他功能也需要"只读 meta"，再单独抽 `ThreadStore::meta(id)`，不在本计划里提前做（YAGNI）。
