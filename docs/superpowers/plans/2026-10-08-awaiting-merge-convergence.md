# `awaiting_merge` 收敛边 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 卡片分支已并入 base 后，插件 tick 自动把 `awaiting_merge` 的实现卡收敛为 `done`；分支缺失时提示人工确认一次，不误判。

**Architecture:** 三处切分。①**纯判据**（`superpowers-kanban-core/src/converge.rs`）：只吃「分支是否存在 / 是否已并入」两个布尔事实，做三态分类 + 候选集筛选，无 I/O。②**扫描与报告**（`superpowers-kanban-runner/src/reconcile.rs`）：git 访问经 `GitFacts` 探针抽象，产出结构化报告，不改状态、不打印。③**接线**（`service.rs` 持锁落状态与去重、`main.rs` tick 打印）。顺带退役已被会话驱动取代的「自动派生合并卡」。

**Tech Stack:** Rust（edition 2024）、`serde_json`、`chrono`、`git` CLI（经既有 `merge.rs` 助手）、`HashSet`。

**依据 spec:** `docs/superpowers/specs/2026-10-08-awaiting-merge-convergence-design.md`（§3 决策 D1–D10、§4 扫描、§5 退役、§7 验收）。

## Global Constraints

- 包管理/构建在 `plugins/superpowers-kanban/` 下：`cd plugins/superpowers-kanban && cargo fmt --all && cargo test`。**不要并发跑多个 `cargo test`**（同一 target 目录会互相锁）。
- `superpowers-kanban-core` **必须保持无 I/O**：状态机、纯判断留在 core；`git`/文件/socket 一律留在 runner。core 不得 `use std::process`。
- **不新增第三方依赖**：只用现有 crate 与 `std`。
- 新增 `Card` 字段才需要 `#[serde(default)]`；本计划**不改** `Card` schema（去重集是内存态）。
- 测试放在文件底部 `#[cfg(test)] mod tests`。
- **插件（Rust）测试名用英文 snake_case**，配中文 `///` 文档注释说明「锁住的契约」——与 `service.rs`、`board.rs` 既有测试逐字同风格。
- 注释中文，讲「为什么」而不是「是什么」。
- commit 用 conventional commits，正文中文，**不写 `Co-Authored-By`**。
- 收敛扫描**不占**任何并发名额（`occupies_slot()` 仍只认 `Running`）。

---

## File Structure

| 文件 | 职责 | 动作 |
|---|---|---|
| `crates/superpowers-kanban-core/src/converge.rs` | 纯判据 `Converge` + 候选集 `candidates`，无 I/O | 新建 |
| `crates/superpowers-kanban-core/src/lib.rs` | 导出 `converge` 模块 | 改 |
| `crates/superpowers-kanban-runner/src/reconcile.rs` | `GitFacts` 探针 + `RealGit` + `Reconcile` 报告 + `plan` + `report_line` | 新建 |
| `crates/superpowers-kanban-runner/src/lib.rs` | 导出 `reconcile` 模块 | 改 |
| `crates/superpowers-kanban-runner/src/service.rs` | `Inner.notified_missing_branch` + `reconcile_merged()`；Task 5 删派生块 | 改 |
| `crates/superpowers-kanban-runner/src/main.rs` | tick 接线 + 打印 | 改 |

依赖方向（严格单向，无环）：`main.rs` → `service.rs` → `reconcile.rs` → `core::converge`。

---

### Task 1: core 纯判据与候选集

**Files:**
- Create: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/converge.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/lib.rs`
- Test: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/converge.rs`（文件底部 `mod tests`）

**Interfaces:**
- Consumes: `crate::board::Board`、`crate::card::{CardId, CardKind, CardState}`（均已存在）。
- Produces:
  - `pub enum Converge { Done, Wait, MissingBranch }`（`Debug, Clone, Copy, PartialEq, Eq`）
  - `pub fn classify(existing: bool, merged: bool) -> Converge`
  - `pub fn candidates(board: &Board) -> Vec<CardId>`

- [ ] **Step 1: 写失败测试**

创建 `crates/superpowers-kanban-core/src/converge.rs`，先只放测试（模块本体留空占位，让编译能走到断言）：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::Board;
    use crate::card::{CardId, CardKind, CardState};
    use chrono::{Local, TimeZone};
    fn at(hour: u32) -> chrono::DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 10, 1, hour, 0, 0)
            .single()
            .unwrap()
    }

    /// 一张停在 `awaiting_merge` 的实现卡（走真实迁移路径）。
    fn board_with_awaiting(id: &str) -> Board {
        let mut board = Board::new();
        let card = CardId::new(id);
        board.enqueue(
            card.clone(),
            format!("{id}.spec.md").into(),
            format!("{id}.plan.md").into(),
            at(0),
        );
        board.transition(&card, CardState::Running).unwrap();
        board.transition(&card, CardState::AwaitingMerge).unwrap();
        board
    }

    #[test]
    fn a_missing_branch_wins_over_any_merge_flag() {
        // 分支不存在时「是否已并入」无意义：绝不用「分支没了」推断「已合并」。
        assert_eq!(classify(false, true), Converge::MissingBranch);
        assert_eq!(classify(false, false), Converge::MissingBranch);
    }

    #[test]
    fn an_existing_merged_branch_converges() {
        assert_eq!(classify(true, true), Converge::Done);
    }

    #[test]
    fn an_existing_unmerged_branch_waits() {
        assert_eq!(classify(true, false), Converge::Wait);
    }

    #[test]
    fn candidates_are_only_awaiting_merge_implementation_cards() {
        let board = board_with_awaiting("a");
        assert_eq!(candidates(&board), vec![CardId::new("a")]);
    }

    #[test]
    fn candidates_skip_needs_you_cards() {
        // needs_you 的语义是「等人决定」，不该被自动终态化。
        let mut board = board_with_awaiting("a");
        board
            .transition(&CardId::new("a"), CardState::NeedsYou)
            .unwrap();
        assert!(candidates(&board).is_empty());
    }

    #[test]
    fn candidates_skip_archived_cards() {
        // awaiting_merge 不是活跃态，可被归档（隐藏）；归档卡不再参与收敛。
        let mut board = board_with_awaiting("a");
        board.archive(&CardId::new("a")).unwrap();
        assert!(candidates(&board).is_empty());
    }

    #[test]
    fn candidates_skip_queued_merge_cards() {
        // 合并卡（kind=Merge）走自己的合并轮通路，绝不进收敛候选。
        let mut board = Board::new();
        board.enqueue_merge(
            CardId::new("m1"),
            "kanban/x".into(),
            "main".into(),
            None,
            at(0),
        );
        assert!(candidates(&board).is_empty());
        assert_eq!(
            board.get(&CardId::new("m1")).unwrap().kind,
            CardKind::Merge,
            "前置：这确实是一张合并卡"
        );
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core converge`
Expected: FAIL —— 编译错误 `cannot find function 'classify' in this scope` / `cannot find function 'candidates'`（模块为空占位时）。

- [ ] **Step 3: 写实现**

把 `crates/superpowers-kanban-core/src/converge.rs` 顶部补上模块本体（测试保持不变，放在文件末尾）：

```rust
//! `awaiting_merge` 收敛的纯判据与候选集筛选。
//!
//! 分支是否已并入 base 是 git 事实，本模块不碰 I/O：调用方（runner）把
//! 「分支是否存在」「是否已并入」两个布尔事实喂进来，这里只做分类。
//! 如此三态分类与候选集口径可脱离 git 单测。

use crate::board::Board;
use crate::card::{CardId, CardKind, CardState};

/// 一张 `awaiting_merge` 实现卡（按 git 事实）的三种去向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Converge {
    /// 分支已并入 base → 收敛为 `done`。
    Done,
    /// 分支存在但未并入 → 保持 `awaiting_merge`。
    Wait,
    /// 分支不存在 → 无法判定，交人工确认。
    MissingBranch,
}

/// 纯判据：只吃两个布尔事实。
///
/// `existing == false` 时 `merged` 无意义（不可能并入一个不存在的分支），
/// 恒判 `MissingBranch`——绝不用「分支不存在」推断「已合并」。
pub fn classify(existing: bool, merged: bool) -> Converge {
    if !existing {
        Converge::MissingBranch
    } else if merged {
        Converge::Done
    } else {
        Converge::Wait
    }
}

/// 待收敛的实现卡 id：`awaiting_merge`、`implementation`、未归档。
///
/// 排除 `kind == Merge`（合并卡另有合并轮通路）与已归档卡；
/// `needs_you` 不在其列——它的语义是「等人决定」，不自动终态化。
pub fn candidates(board: &Board) -> Vec<CardId> {
    board
        .cards()
        .iter()
        .filter(|card| {
            !card.archived
                && card.state == CardState::AwaitingMerge
                && card.kind == CardKind::Implementation
        })
        .map(|card| card.id.clone())
        .collect()
}
```

同时把 `crates/superpowers-kanban-core/src/lib.rs` 的模块声明补一行（按字母序插在 `pub mod card_id;` 之后、`pub mod inbox;` 之前）：

```rust
pub mod converge;
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core`
Expected: PASS（`converge` 的 7 个用例全过，且该 crate 既有用例零回归）。

- [ ] **Step 5: Commit**

```bash
cd plugins/superpowers-kanban
git add crates/superpowers-kanban-core/src/converge.rs crates/superpowers-kanban-core/src/lib.rs
git commit -m "feat(kanban-core): awaiting_merge 收敛的纯判据与候选集

分支是否存在/是否已并入两个布尔事实做三态分类；候选集只收
awaiting_merge + implementation + 未归档。core 保持无 I/O。"
```

---

### Task 2: runner 扫描与报告

**Files:**
- Create: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/reconcile.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/lib.rs`
- Test: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/reconcile.rs`（文件底部 `mod tests`）

**Interfaces:**
- Consumes: `superpowers_kanban_core::converge::{classify, candidates, Converge}`（Task 1）；`crate::merge::{default_branch, source_branch_exists, branch_merged_into}`、`crate::worktree::slugify`（均已存在）。
- Produces:
  - `pub trait GitFacts { fn default_branch(&self) -> String; fn branch_exists(&self, source: &str) -> bool; fn branch_merged(&self, source: &str, base: &str) -> bool; }`
  - `pub struct RealGit<'a> { pub project_root: &'a Path }`（实现 `GitFacts`）
  - `pub enum Reconcile { Converged { id: CardId, source: String, base: String }, MissingBranch { id: CardId, source: String, workdir: Option<PathBuf> } }`（`Debug, Clone, PartialEq, Eq`）
  - `pub fn source_for(id: &CardId) -> String`
  - `pub fn plan<G: GitFacts>(board: &Board, git: &G) -> Vec<Reconcile>`
  - `pub fn report_line(r: &Reconcile) -> String`

  注：`plan` **不做去重**——去重集归属 `BoardService`（Task 3）。这样 `plan` 只需
  对 `board` 的共享借用，不会与去重集的可变借用冲突。

- [ ] **Step 1: 写失败测试**

创建 `crates/superpowers-kanban-runner/src/reconcile.rs`，先只放测试：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use superpowers_kanban_core::board::Board;
    use superpowers_kanban_core::card::{CardId, CardState};
    use chrono::{Local, TimeZone};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn at() -> chrono::DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 10, 1, 0, 0, 0)
            .single()
            .unwrap()
    }

    /// 一张停在 `awaiting_merge` 的实现卡，带一个 workdir（供缺分支提示）。
    fn board_with_awaiting(id: &str) -> Board {
        let mut board = Board::new();
        let card = CardId::new(id);
        board.enqueue(
            card.clone(),
            format!("{id}.spec.md").into(),
            format!("{id}.plan.md").into(),
            at(),
        );
        board.set_workdir(&card, PathBuf::from(format!("/w/{id}"))).unwrap();
        board.transition(&card, CardState::Running).unwrap();
        board.transition(&card, CardState::AwaitingMerge).unwrap();
        board
    }

    /// 假探针：`facts[source] = (exists, merged)`。分类逻辑因此无需真实仓库。
    struct FakeGit {
        base: String,
        facts: HashMap<String, (bool, bool)>,
    }

    impl GitFacts for FakeGit {
        fn default_branch(&self) -> String {
            self.base.clone()
        }
        fn branch_exists(&self, source: &str) -> bool {
            self.facts.get(source).map(|(e, _)| *e).unwrap_or(false)
        }
        fn branch_merged(&self, source: &str, _base: &str) -> bool {
            self.facts.get(source).map(|(_, m)| *m).unwrap_or(false)
        }
    }

    fn fake(source: &str, exists: bool, merged: bool) -> FakeGit {
        let mut facts = HashMap::new();
        facts.insert(source.to_string(), (exists, merged));
        FakeGit {
            base: "main".to_string(),
            facts,
        }
    }

    #[test]
    fn a_merged_branch_produces_a_converged_report() {
        let board = board_with_awaiting("card-1");
        let git = fake("kanban/card-1", true, true);
        let reports = plan(&board, &git);
        assert_eq!(
            reports,
            vec![Reconcile::Converged {
                id: CardId::new("card-1"),
                source: "kanban/card-1".into(),
                base: "main".into(),
            }]
        );
    }

    #[test]
    fn an_existing_unmerged_branch_produces_no_report() {
        let board = board_with_awaiting("card-1");
        let git = fake("kanban/card-1", true, false);
        assert!(plan(&board, &git).is_empty());
    }

    #[test]
    fn a_missing_branch_is_reported_with_its_workdir() {
        // plan 自身不去重：同一输入两次都产出报告，去重是 BoardService 的职责。
        let board = board_with_awaiting("card-1");
        let git = fake("kanban/card-1", false, false);
        let expected = vec![Reconcile::MissingBranch {
            id: CardId::new("card-1"),
            source: "kanban/card-1".into(),
            workdir: Some(PathBuf::from("/w/card-1")),
        }];
        assert_eq!(plan(&board, &git), expected);
        assert_eq!(plan(&board, &git), expected, "plan 本身无记忆");
    }

    #[test]
    fn the_source_is_derived_from_the_slugified_card_id() {
        assert_eq!(source_for(&CardId::new("Card 2 foo/bar")), "kanban/card-2-foo-bar");
    }

    #[test]
    fn report_lines_are_actionable() {
        let converged = report_line(&Reconcile::Converged {
            id: CardId::new("card-1"),
            source: "kanban/card-1".into(),
            base: "main".into(),
        });
        assert!(converged.contains("auto-converged to done"), "{converged}");
        assert!(converged.contains("kanban/card-1"), "{converged}");

        let missing = report_line(&Reconcile::MissingBranch {
            id: CardId::new("card-1"),
            source: "kanban/card-1".into(),
            workdir: Some(PathBuf::from("/w/card-1")),
        });
        assert!(missing.contains("superpowers-kanban done card-1"), "{missing}");
        assert!(missing.contains("/w/card-1"), "workdir 要带上：{missing}");
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner reconcile`
Expected: FAIL —— 编译错误 `cannot find function 'plan'` / `cannot find type 'Reconcile'`。

- [ ] **Step 3: 写实现**

把 `crates/superpowers-kanban-runner/src/reconcile.rs` 顶部补上模块本体（测试保持在文件末尾）：

```rust
//! 收敛扫描：把候选卡按 git 事实分类，产出「该冒出水面」的报告。
//!
//! git 访问经 `GitFacts` 抽象，故分类逻辑可用假探针单测；生产用 `RealGit`。
//! 本模块**不**改看板状态、**不**打印——状态迁移与去重在 `BoardService`，
//! 打印在 tick 循环。

use std::path::{Path, PathBuf};

use superpowers_kanban_core::board::Board;
use superpowers_kanban_core::card::CardId;
use superpowers_kanban_core::converge::{self, Converge};

use crate::merge;
use crate::worktree::slugify;

/// 卡片推导出的源分支：与 `command_done` / `merge_request` 逐字同规则。
pub fn source_for(id: &CardId) -> String {
    format!("kanban/{}", slugify(id))
}

/// git 事实的探针。抽象出来是为了让分类逻辑可脱离真实仓库单测。
pub trait GitFacts {
    fn default_branch(&self) -> String;
    fn branch_exists(&self, source: &str) -> bool;
    fn branch_merged(&self, source: &str, base: &str) -> bool;
}

/// 真实 git 实现；全部委托既有的 `merge.rs` 助手，不另造 git 调用。
pub struct RealGit<'a> {
    pub project_root: &'a Path,
}

impl GitFacts for RealGit<'_> {
    fn default_branch(&self) -> String {
        merge::default_branch(self.project_root)
    }
    fn branch_exists(&self, source: &str) -> bool {
        merge::source_branch_exists(self.project_root, source)
    }
    fn branch_merged(&self, source: &str, base: &str) -> bool {
        merge::branch_merged_into(self.project_root, source, base)
    }
}

/// 一条该冒出水面的收敛结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reconcile {
    /// 分支已并入 base：卡将落 `done`。
    Converged {
        id: CardId,
        source: String,
        base: String,
    },
    /// 分支缺失且本 daemon 尚未提示过：等人工 `done`。
    MissingBranch {
        id: CardId,
        source: String,
        workdir: Option<PathBuf>,
    },
}

/// 扫描候选卡，返回该报告/该落地的结论。**不去重**（去重归 `BoardService`）。
///
/// `Converge::Wait` 不产出任何报告。分支不存在时不调用 `branch_merged`
/// （省一次 git 调用，且语义上无意义）。
pub fn plan<G: GitFacts>(board: &Board, git: &G) -> Vec<Reconcile> {
    let base = git.default_branch();
    let mut reports = Vec::new();
    for id in converge::candidates(board) {
        let source = source_for(&id);
        let existing = git.branch_exists(&source);
        let merged = existing && git.branch_merged(&source, &base);
        match converge::classify(existing, merged) {
            Converge::Done => reports.push(Reconcile::Converged {
                id,
                source,
                base: base.clone(),
            }),
            Converge::Wait => {}
            Converge::MissingBranch => {
                let workdir = board.get(&id).and_then(|card| card.workdir.clone());
                reports.push(Reconcile::MissingBranch { id, source, workdir });
            }
        }
    }
    reports
}

/// 报告行文案（不含 `superpowers-kanban: ` 前缀，由打印侧统一加）。
pub fn report_line(r: &Reconcile) -> String {
    match r {
        Reconcile::Converged { id, source, base } => {
            format!("card {id} auto-converged to done ({source} is merged into {base})")
        }
        Reconcile::MissingBranch {
            id,
            source,
            workdir,
        } => {
            let workdir = workdir
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "unknown".to_string());
            format!(
                "card {id} is awaiting_merge but {source} is gone; \
                 run `superpowers-kanban done {id}` to confirm (worktree: {workdir})"
            )
        }
    }
}
```

同时把 `crates/superpowers-kanban-runner/src/lib.rs` 补一行（插在 `pub mod merge_lock;` 之后、`pub mod runner;` 之前）：

```rust
pub mod reconcile;
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner reconcile`
Expected: PASS（5 个用例全过）。

- [ ] **Step 5: Commit**

```bash
cd plugins/superpowers-kanban
git add crates/superpowers-kanban-runner/src/reconcile.rs crates/superpowers-kanban-runner/src/lib.rs
git commit -m "feat(kanban): 收敛扫描与报告

GitFacts 探针抽象 git 事实，分类逻辑用假探针单测；RealGit 委托既有
merge.rs 助手。报告只产「已收敛」与「首次缺分支」两类，不打印、不改状态。"
```

---

### Task 3: `BoardService::reconcile_merged` 与去重集

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`
  - `struct Inner`（现约 75-84 行）加字段
  - `BoardService::new`（现约 100-119 行）初始化该字段
  - 新增 `reconcile_merged` 方法（插在 `merge_finish` 之后、`list` 之前，现约 579 行）
  - 测试模块内新增 `state_of` 辅助 + 5 个用例
- Test: 同上（`service.rs` 底部 `mod tests`）

**Interfaces:**
- Consumes: `crate::reconcile::{plan, Reconcile, RealGit}`（Task 2）；既有测试辅助 `project_with_worktree`、`service_with_awaiting_card`、`git_run`（均在 `service.rs` 的 `mod tests` 内）。
- Produces: `pub fn reconcile_merged(&self) -> Vec<crate::reconcile::Reconcile>`

- [ ] **Step 1: 写失败测试**

在 `service.rs` 的 `mod tests` 内，紧接 `git_run` 辅助函数之后追加（`state_of` 辅助 + 5 个用例）：

```rust
    /// 读一张卡的当前状态字符串（经 `list`，与桌面/宿主同口径）。
    fn state_of(service: &BoardService, id: &str) -> String {
        service.list()["cards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|card| card["id"] == id)
            .map(|card| card["state"].as_str().unwrap().to_string())
            .unwrap_or_else(|| panic!("card {id} not on the board"))
    }

    /// 分支已并入 base（分支指向与 main 同一提交）→ 卡落 `done`。
    #[test]
    fn reconcile_converges_a_merged_card_to_done() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        git_run(&project, &["branch", &source]);

        let reports = service.reconcile_merged();
        assert_eq!(reports.len(), 1, "{reports:?}");
        assert!(
            matches!(reports[0], crate::reconcile::Reconcile::Converged { .. }),
            "{reports:?}"
        );
        assert_eq!(state_of(&service, "card-1"), "done");
    }

    /// 分支存在但未并入 → 卡保持 `awaiting_merge`，不产报告。
    #[test]
    fn reconcile_waits_on_an_existing_unmerged_card() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        // 分支上多一个未进 main 的提交。
        git_run(&project, &["checkout", "-q", "-b", &source]);
        std::fs::write(project.join("more.txt"), "wip").unwrap();
        git_run(&project, &["add", "-A"]);
        git_run(&project, &["commit", "-q", "-m", "wip"]);
        git_run(&project, &["checkout", "-q", "main"]);

        assert!(service.reconcile_merged().is_empty());
        assert_eq!(state_of(&service, "card-1"), "awaiting_merge");
    }

    /// 分支缺失 → 卡保持 `awaiting_merge`，且只提示一次。
    #[test]
    fn reconcile_reports_a_missing_branch_once() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_awaiting_card(&project);
        // 不建分支 → 缺分支。

        let first = service.reconcile_merged();
        assert_eq!(first.len(), 1, "{first:?}");
        assert!(
            matches!(first[0], crate::reconcile::Reconcile::MissingBranch { .. }),
            "{first:?}"
        );
        assert_eq!(state_of(&service, "card-1"), "awaiting_merge");
        // 去重：第二次不再报告。
        assert!(service.reconcile_merged().is_empty());
    }

    /// `needs_you` 的卡不是候选：无论如何都不动。
    #[test]
    fn reconcile_leaves_a_needs_you_card_alone() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        service.mark_running("card-1", "thread-2").unwrap();
        service.mark_terminal("card-1", "needs_you", None).unwrap();
        git_run(&project, &["branch", &source]); // 即使分支已并入
        assert!(service.reconcile_merged().is_empty());
        assert_eq!(state_of(&service, "card-1"), "needs_you");
    }

    /// 合并卡不是候选：收敛扫描绝不碰它。
    #[test]
    fn reconcile_leaves_merge_cards_alone() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        service.enqueue_merge_card_for_test("m1", "kanban/card-1", "main", None);
        assert!(service.reconcile_merged().is_empty());
        assert_eq!(state_of(&service, "m1"), "queued");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner reconcile_`
Expected: FAIL —— 编译错误 `no method named 'reconcile_merged'`。

- [ ] **Step 3: 加 `Inner` 字段与初始化**

在 `service.rs` 的 `struct Inner` 内，`leases_dir` 字段之后追加：

```rust
    /// 已提示过「分支缺失」的卡：同一 daemon 生命周期内每卡至多提示一次。
    /// 纯内存、不落盘——重启后重发一次是可接受的（该提示要求人工动作）。
    notified_missing_branch: std::collections::HashSet<CardId>,
```

在 `BoardService::new` 的 `Inner { ... }` 字面量里，`leases_dir,` 之后追加：

```rust
                notified_missing_branch: Default::default(),
```

- [ ] **Step 4: 写 `reconcile_merged` 实现**

在 `service.rs` 的 `merge_finish` 方法之后、`list` 方法之前插入：

```rust
    /// 收敛扫描：把 `awaiting_merge` 的实现卡按 git 事实落地。
    ///
    /// 分支已并入 base → `done`；分支缺失 → 记入去重集（每 daemon 至多提示一次）。
    /// 返回**该打印的报告行**；本方法不打印（打印在 tick 循环），故报告可直接单测。
    /// 不占任何并发名额——收敛是簿记，`occupies_slot()` 仍只认 `Running`。
    pub fn reconcile_merged(&self) -> Vec<crate::reconcile::Reconcile> {
        use crate::reconcile::{self, RealGit, Reconcile};
        let mut inner = self.lock();
        let git = RealGit {
            project_root: &self.project_root,
        };
        // 1) 在共享借用 `inner.board` 内算候选报告；此步不碰去重集（借用规则）。
        let candidates = reconcile::plan(&inner.board, &git);
        // 2) 去重缺分支提示：命中即视为已提示，不再上报。
        let mut reports: Vec<Reconcile> = Vec::new();
        for report in candidates {
            match report {
                Reconcile::MissingBranch { id, .. } => {
                    if inner.notified_missing_branch.insert(id) {
                        reports.push(report);
                    }
                }
                other => reports.push(other),
            }
        }
        // 3) 落地 `done`（此时文档里只剩该上报的报告）。
        let mut changed = false;
        for report in &reports {
            if let Reconcile::Converged { id, .. } = report {
                if inner.board.transition(id, CardState::Done).is_ok() {
                    changed = true;
                }
            }
        }
        if changed {
            self.save(&inner);
        }
        reports
    }
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner reconcile_`
Expected: PASS（5 个用例全过）。

- [ ] **Step 6: 跑该 crate 全量测试确认零回归**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner`
Expected: PASS（既有用例全过；含既有的 `merge_request_*` / `mark_terminal` 用例）。

- [ ] **Step 7: Commit**

```bash
cd plugins/superpowers-kanban
git add crates/superpowers-kanban-runner/src/service.rs
git commit -m "feat(kanban): BoardService 收敛扫描与缺分支提示去重

reconcile_merged 持锁落状态并维护内存去重集；报告返回给调用方打印。
分支缺失只在本 daemon 生命周期内提示一次。"
```

---

### Task 4: tick 接线与打印

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs`
  - `run_daemon` 的推进循环（`consume_inbox` 循环之后、`merge_next` 之前，现约 828-838 行）

**Interfaces:**
- Consumes: `BoardService::reconcile_merged()`（Task 3）、`superpowers_kanban_runner::reconcile::report_line`（Task 2）。
- Produces: 无对外接口；本任务是用户可见效果（tick 每轮收敛并打印）。

- [ ] **Step 1: 接线**

在 `main.rs` 的 `run_daemon` 推进循环里，`for outcome in service.consume_inbox(...) { ... }` 整段之后、`// 合并与实现共用一个 tick` 注释之前，插入：

```rust
        // 收敛：分支已并入 base 的 awaiting_merge 卡落 done；分支缺失则提示一次。
        // 放在归档之前——刚 done 的卡 terminal_at 才写下，不会被同拍归档。
        for report in service.reconcile_merged() {
            eprintln!(
                "superpowers-kanban: {}",
                superpowers_kanban_runner::reconcile::report_line(&report)
            );
        }
```

- [ ] **Step 2: 编译并跑全量测试**

Run: `cd plugins/superpowers-kanban && cargo fmt --all && cargo test`
Expected: PASS（两个 crate 全过；`fmt` 不产生 diff）。

- [ ] **Step 3: 手工验证日志确实打印（可选但推荐）**

Run（在仓库根，用临时 state-dir 起一次 `run`，观察 stdout/stderr 一行）:
```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
timeout 6 plugins/superpowers-kanban/target/debug/superpowers-kanban run \
  --runtime-dir .yi-agent/runtime \
  --state-dir .yi-agent/superpowers-kanban \
  --project-root . 2>&1 | head -20
```
Expected: 输出里出现 `superpowers-kanban: card <id> auto-converged to done (...)` 与/或
`... is awaiting_merge but ... is gone; run \`superpowers-kanban done <id>\` to confirm ...`
（对本项目三张卡：两张自动 `done`，board-as-thread-page 那张提示人工 `done`）。

- [ ] **Step 4: Commit**

```bash
cd plugins/superpowers-kanban
git add crates/superpowers-kanban-runner/src/main.rs
git commit -m "feat(kanban): tick 接入收敛扫描并打印报告

每轮推进消费 inbox 后、归档前做一次收敛；报告行经 report_line 统一
加 superpowers-kanban: 前缀，与既有 tick 日志同渠道。"
```

---

### Task 5: 退役自动派生合并卡

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`
  - `mark_terminal`（现约 267-309 行）删除派生块
  - 测试 `awaiting_merge_derives_a_merge_card_only_when_the_preference_is_on`（现约 1040-1072 行）替换为反向断言

**Interfaces:**
- Consumes: 无新依赖。
- Produces: `mark_terminal` 语义收窄为「只落状态 + 释放槽位」；不再有生产代码读 `board_auto_merge`。

**背景（为什么删而不是留）：** 该派生块**能工作且被测试覆盖**，不是死代码。移除它的理由是被更新决策取代——`spec 2026-10-08` D2/D3 已把合并定为「手动、会话驱动」；保留它会让同一目标存在两条自动路径（本计划的收敛扫描 + 派生卡的 `merge_next`），二者都可能在无人发话时改 main，且互相竞争。`merge_next` / `claim_next_merge` / `CardKind::Merge` / `add-merge` **全部保留**（存量独立合并卡仍按原逻辑收尾）。

- [ ] **Step 1: 写失败测试（替换原用例）**

在 `service.rs` 的 `mod tests` 内，找到 `awaiting_merge_derives_a_merge_card_only_when_the_preference_is_on`（测试名全文见上），用下面整个函数替换它：

```rust
    /// `board_auto_merge` 打开也不再派生合并卡：合并已改为会话驱动（手动），
    /// 自动派生会让同一目标出现第二条无人发话就能改 main 的路径。
    #[test]
    fn awaiting_merge_never_derives_a_merge_card_even_with_the_preference_on() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        // 打开开关：旧行为会在这里派生一张合并卡。
        let pref = superpowers_kanban_core::layout::project_preferences_path(&service.state_dir);
        superpowers_kanban_core::switch::write_bool(&pref, "board_auto_merge", true).unwrap();

        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        service
            .mark_terminal(&claim.card_id, "awaiting_merge", None)
            .unwrap();

        let cards = service.list()["cards"].as_array().unwrap().clone();
        assert_eq!(cards.len(), 1, "只该有那张实现卡：{cards:?}");
        assert_eq!(cards[0]["kind"], "implementation");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner awaiting_merge_never_derives`
Expected: FAIL —— `assertion left == right failed: 只该有那张实现卡`，实际有 2 张（含派生的合并卡）。

- [ ] **Step 3: 删除派生块**

在 `service.rs` 的 `mark_terminal` 内，删除下面整段（从 `// 自动派生：` 注释起，到该 `if` 块结束的 `}` 为止）：

```rust
        // 自动派生：实现卡到达 AwaitingMerge 且偏好开启时，就地排一张配对的合并卡。
        if next == CardState::AwaitingMerge {
            let pref = superpowers_kanban_core::layout::project_preferences_path(&self.state_dir);
            if superpowers_kanban_core::switch::read_bool(&pref, "board_auto_merge") == Some(true) {
                if let Some(card) = inner.board.get(&id).cloned() {
                    if card.kind == CardKind::Implementation {
                        let source = format!("kanban/{}", crate::worktree::slugify(&card.id));
                        let base = crate::merge::default_branch(&self.project_root);
                        let merge_id = inner.board.next_free_merge_id(&source, &base);
                        inner.board.enqueue_merge(
                            merge_id,
                            source,
                            base,
                            Some(id.clone()),
                            chrono::Local::now(),
                        );
                    }
                }
            }
        }
```

删除后，`mark_terminal` 的结尾应直接是 `self.save(&inner); Ok(())`。同时把该方法的文档注释从 `/// 会话收尾：迁移到某个非终态结果并释放槽位。` 保持不变——它本来就准确。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner`
Expected: PASS（新用例通过；其余用例零回归）。

- [ ] **Step 5: 确认无残留引用与未用导入**

Run:
```bash
cd plugins/superpowers-kanban
grep -rn "board_auto_merge" crates/ || echo "no production/test reference left"
cargo build --all-targets 2>&1 | grep -i "unused\|warning: unused" || echo "no unused-import warnings"
```
Expected: `board_auto_merge` 在生产与测试代码中**零引用**（`switch.rs` 里作为通用读取器的定义与它自己的单测除外）；无 unused 警告。

- [ ] **Step 6: Commit**

```bash
cd plugins/superpowers-kanban
git add crates/superpowers-kanban-runner/src/service.rs
git commit -m "refactor(kanban): 退役自动派生合并卡

合并已改为会话驱动（手动），保留派生会让同一目标存在两条自动改 main
的路径并与收敛扫描竞争。保留 merge_next/claim_next_merge/add-merge
与 CardKind::Merge——存量独立合并卡仍按原逻辑收尾。"
```

---

## 验收

全部 Task 完成后，逐条核对 spec §7：

1. `cd plugins/superpowers-kanban && cargo test` 全绿（两 crate）。
2. `cd plugins/superpowers-kanban && cargo fmt --all -- --check` 无 diff。
3. 对本项目三张卡起一次 tick（Task 4 Step 3）：两张分支已并入的自动 `done`；
   board-as-thread-page 那张（分支已删）打印一次 `done <id>` 提示、保持 `awaiting_merge`；
   手跑 `superpowers-kanban done <id>` 后归位。
4. `superpowers-kanban list` 中三张卡不再显示为 `awaiting_merge`。
5. 反向：`grep -rn board_auto_merge crates/` 在生产代码零引用。

## 说明：spec 与实现的一处措辞差异

spec §4.3 写 `reconcile_merged(&mut self)`；本计划用 `&self`——`BoardService`
的状态全在 `Mutex<Inner>` 之后，与既有的 `mark_terminal`/`archive_due` 同形
（它们都是 `&self`）。行为与 spec 一致，仅签名用词更贴合既有代码。
