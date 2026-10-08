# 看板合并改为会话驱动 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让合并由卡片**原会话**里的一轮 turn 执行——合并是原卡片的一个阶段，每项目至多一个合并轮，手动经 skill 申请名额，成败一律以 git 复核判定。

**Architecture:** 把「插件自行跑 git」换成「插件发名额、会话跑 git、插件复核」。新增插件侧 `merge_request`/`merge_finish`（名额 + 复核），宿主侧把 `needs_you` 的再说话接成「重新申请名额」，会话侧由 skill 驱动。名额闸仍是每项目一把 `merge.lock`（flock，不改语义）。

**Tech Stack:** Rust（edition 2024，`rust-version = 1.85`）、`serde_json`、flock（`libc`）；宿主 app-server；桌面 TypeScript（仅状态文案）。

**依据 spec:** `docs/superpowers/specs/2026-10-08-board-merge-as-session-design.md` §3-§5、§6.2。

**前置：** `docs/superpowers/plans/2026-10-08-board-sidebar-hard-fixes.md` 应先落地（本计划的验收依赖侧栏能显示合并状态）。

## Global Constraints

- **插件不新增第三方依赖。** 只用现有 crate 与 `std`。
- `superpowers-kanban-core` **必须保持无 I/O**：状态机、纯判断留在 core；`git`/文件/socket 一律留在 runner。
- 插件命令：`cd plugins/superpowers-kanban && cargo fmt --all && cargo test`（**不要并发**跑多个 `cargo test`）。
- 新增 `Card` 字段必须 `#[serde(default)]`（旧 `board.json` 要能反序列化）。
- 测试放文件底部 `#[cfg(test)] mod tests`。
- 锁一律 flock，**不 unlink**。
- 注释中文、讲「为什么」；测试名中文、说清锁住的契约。
- commit 用 conventional commits，正文中文，**不写 `Co-Authored-By`**。
- 桌面命令（涉及 Task 9 时）：`export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx tsc --noEmit && npx vitest run`。

## 关键事实（实现前必读）

1. **合并 worktree 的落点受 git 约束。** `git worktree add <path> main` 在 main 已被某 worktree
   检出时会失败（`'main' is already used by worktree at ...`）。现有 `merge::prepare(project_root, base)`
   已经处理了这点：**base 已被检出的 worktree 就复用它**（通常是主检出），只有 base 没有任何
   worktree 检出时才新建 `<项目>/.worktrees/kanban-merge/<slug>`。本项目 base=main 由主检出占用，
   故合并轮**就发生在主检出**。这修正了 spec §D5 的「不碰主检出」——git 不允许同一分支检出两次。
   实现沿用 `merge::prepare`，不要另造 worktree。
2. **会话 cwd 创建时固定**：`prepare_turn_core`（`server.rs:6165`）不读 cwd，只有 `thread/start` 才定。
   故「复用原会话」意味着合并轮跑在该会话**原有**的 workdir 里——原会话当初跑在实现卡的 worktree 上，
   不是主检出。**因此合并的 git 动作必须在会话内显式 `cd` 到 `merge::prepare` 返回的 base worktree**，
   skill 文案必须把这一点写成硬要求。
3. `CardState::Merging` 已存在，且 `occupies_slot()` 只认 `Running`（不用改）。
4. 槽位记账：`mark_terminal` 里 `inner.leases.remove(&id)` 释放租约；合并卡**不占**会话槽位。

---

### Task 1: 状态机放行合并阶段的迁移

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card.rs`
  （`can_transition_to` 的 `match (self, next)`，现约 79-100 行；文件底部 `mod tests`）

**Interfaces:**
- Consumes: 无。
- Produces: `CardState::can_transition_to` 新增三条合法迁移：
  `(AwaitingMerge, Merging)`、`(NeedsYou, Merging)`；`(Merging, Done)`/`(Merging, NeedsYou)` 已存在。
  **保留** `(Merging, Failed)`——存量 `kind=merge` 独立卡仍走 `merge_next` 的 GitError 分支，
  删边会造成回归；新会话驱动路径从不产出该迁移（spec §D6 的「无自动失败边」按此理解）。

- [ ] **Step 1: 写失败测试**

在 `card.rs` 底部的 `mod tests` 里追加：

```rust
    #[test]
    fn a_card_may_enter_and_leave_the_merge_stage() {
        use CardState::*;
        // 实现卡停在 awaiting_merge，用户发话后进 merging；失败退回 needs_you 再来。
        assert!(AwaitingMerge.can_transition_to(Merging));
        assert!(NeedsYou.can_transition_to(Merging));
        assert!(Merging.can_transition_to(Done));
        assert!(Merging.can_transition_to(NeedsYou));
        // 合并阶段不占会话槽位（口径不变）。
        assert!(!Merging.occupies_slot());
        // 未到 awaiting_merge/needs_you 的卡不得直接进 merging。
        assert!(!Running.can_transition_to(Merging));
        assert!(!Paused.can_transition_to(Merging));
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core a_card_may_enter_and_leave_the_merge_stage`
Expected: FAIL —— `AwaitingMerge.can_transition_to(Merging)` 为 `false`。

- [ ] **Step 3: 改实现**

在 `card.rs` 的 `match (self, next)` 中，`(Running, AwaitingMerge) => true,` 之前插入两行：

```rust
            // 合并阶段：实现卡停在 awaiting_merge/needs_you，用户发话后进 merging。
            // 合并轮由卡片原会话里的一轮 turn 执行，故这一步是「人已发话」的记录。
            (AwaitingMerge, Merging) => true,
            (NeedsYou, Merging) => true,
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core`
Expected: PASS（该 crate 全部用例通过，含既有迁移表用例）。

- [ ] **Step 5: Commit**

```bash
git add plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card.rs
git commit -m "feat(kanban): 状态机放行 awaiting_merge/needs_you → merging

合并改为由原会话的一轮 turn 执行，需要从等待态进入 merging 阶段。
保留 merging → failed（存量合并卡的 GitError 分支仍依赖它）。"
```

---

### Task 2: 服务层新增名额申请与复核落状态

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`
  （`impl BoardService` 内新增两个方法；文件底部 `mod tests`）

**Interfaces:**
- Consumes: `crate::merge_lock::acquire`、`crate::merge::{prepare, branch_merged_into, BaseWorktree}`、
  `Inner`（既有私有结构）、`self.lock()`/`self.save()`。
- Produces:
  - `pub enum MergeRequest { Granted { workdir: PathBuf, source: String, base: String }, Busy, Denied(String) }`
  - `pub fn merge_request(&self, card_id: &str) -> Result<MergeRequest, String>`
  - `pub fn merge_finish(&self, card_id: &str) -> Result<MergeFinish, String>`
  - `pub enum MergeFinish { Done, NeedsYou, Cleared }`

- [ ] **Step 1: 写失败测试**

在 `service.rs` 的 `mod tests` 里追加（沿用该文件既有的真 git 仓库建法；若该文件没有
`repo_with_card` 之类助手，按下面内联建仓库）：

```rust
    /// 一个真 git 仓库 + 一张停在 `AwaitingMerge` 的实现卡（分支名按 slug 推导）。
    fn repo_with_awaiting_impl_card(card_id: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
        use superpowers_kanban_core::board::Board;
        use superpowers_kanban_core::card::{CardId, CardState};
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let run = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&project)
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
        std::fs::write(project.join("f.txt"), "hi").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "init"]);

        let state = dir.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let mut board = Board::new();
        let id = CardId::new(card_id);
        board.enqueue(id.clone(), "c.spec.md".into(), "c.plan.md".into(), chrono::Local::now());
        board.transition(&id, CardState::Running).unwrap();
        board.transition(&id, CardState::AwaitingMerge).unwrap();
        crate::persist::save_board(&state.join("board.json"), &board).unwrap();
        (dir, project, state)
    }

    #[test]
    fn merge_request_denies_a_card_that_is_not_waiting_to_merge() {
        let (dir, project, state) = repo_with_awaiting_impl_card("card-1");
        let service = BoardService::new(state.clone(), project.clone(), None);
        // 先把它挪出等待态。
        service.mark_terminal("card-1", "needs_you", None).unwrap();
        service.mark_running("card-1", "t1").unwrap(); // 回到 Running
        match service.merge_request("card-1").unwrap() {
            MergeRequest::Denied(reason) => assert!(reason.contains("awaiting_merge"), "{reason}"),
            other => panic!("expected Denied, got {other:?}"),
        }
        drop(dir);
    }

    #[test]
    fn merge_request_is_busy_while_another_merge_holds_the_gate() {
        let (dir, project, state) = repo_with_awaiting_impl_card("card-1");
        let service = BoardService::new(state.clone(), project.clone(), Some(dir.path().to_path_buf()));
        // 别的进程先占住 merge.lock。
        let held = crate::merge_lock::acquire(&state).expect("hold the gate");
        match service.merge_request("card-1").unwrap() {
            MergeRequest::Busy => {}
            other => panic!("expected Busy, got {other:?}"),
        }
        drop(held);
        drop(dir);
    }

    #[test]
    fn merge_finish_verifies_with_git_and_settles_the_card() {
        let (dir, project, state) = repo_with_awaiting_impl_card("card-1");
        let service = BoardService::new(state.clone(), project.clone(), None);
        // 建出 kanban/<slug> 分支、合进 main，再进 merging，复核应为 done。
        let slug = crate::worktree::slugify(&superpowers_kanban_core::card::CardId::new("card-1"));
        let branch = format!("kanban/{slug}");
        let run = |args: &[&str]| {
            assert!(
                std::process::Command::new("git").arg("-C").arg(&project).args(args)
                    .status().unwrap().success()
            );
        };
        run(&["checkout", "-q", "-b", &branch]);
        std::fs::write(project.join("g.txt"), "work").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "work"]);
        run(&["checkout", "-q", "main"]);
        run(&["merge", "--no-ff", "-m", "merge", &branch]);

        // 手工置 merging（不经名额，直接测复核）。
        service.force_state_for_test("card-1", superpowers_kanban_core::card::CardState::Merging);
        assert_eq!(service.merge_finish("card-1").unwrap(), MergeFinish::Done);
        drop(dir);
    }

    #[test]
    fn merge_finish_sends_an_unmerged_card_back_to_needs_you() {
        let (dir, project, state) = repo_with_awaiting_impl_card("card-1");
        let service = BoardService::new(state.clone(), project.clone(), None);
        let slug = crate::worktree::slugify(&superpowers_kanban_core::card::CardId::new("card-1"));
        let branch = format!("kanban/{slug}");
        let run = |args: &[&str]| {
            assert!(
                std::process::Command::new("git").arg("-C").arg(&project).args(args)
                    .status().unwrap().success()
            );
        };
        // 分支存在但**没**合进 main。
        run(&["checkout", "-q", "-b", &branch]);
        std::fs::write(project.join("g.txt"), "work").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "work"]);
        run(&["checkout", "-q", "main"]);

        service.force_state_for_test("card-1", superpowers_kanban_core::card::CardState::Merging);
        assert_eq!(service.merge_finish("card-1").unwrap(), MergeFinish::NeedsYou);
        drop(dir);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner merge_request_denies_a_card_that_is_not_waiting_to_merge`
Expected: FAIL —— `no variant or associated item named MergeRequest` / `no method named merge_request`。

- [ ] **Step 3: 写实现**

在 `service.rs` 顶部（`use` 之后、`struct Inner` 之前）新增类型：

```rust
/// 一次名额申请的结果。
///
/// `Granted` 是**授权**而不是「已经合并」：调用方（会话里的 agent）据 `workdir`
/// 去执行 `git merge --no-ff`，完成后必须回 `merge_finish` 让插件复核。
/// 名额本身由 `merge.lock`（flock）保证每项目一次一个——见 `merge_request`。
#[derive(Debug, PartialEq, Eq)]
pub enum MergeRequest {
    Granted {
        /// 执行合并的 base worktree（base 已被检出的那个，通常是主检出）。
        workdir: PathBuf,
        source: String,
        base: String,
    },
    /// 本项目已有合并在跑：卡不动，排队。
    Busy,
    /// 卡不在可合并状态（或 refs 不全）。
    Denied(String),
}

/// 一次合并轮收尾的复核结果。
#[derive(Debug, PartialEq, Eq)]
pub enum MergeFinish {
    /// git 复核确认 source 已并入 base。
    Done,
    /// git 复核不通过：卡退回 `needs_you` 等用户发话。
    NeedsYou,
    /// 卡不在 `Merging`（例如已被别处推进）：不落状态，交由调用方决定。
    Cleared,
}
```

在 `impl BoardService` 中新增：

```rust
    /// 申请一次合并名额。**不跨调用持有锁**：`merge.lock` 只用来判定
    /// 「此刻是否已有合并在跑」，拿到即释放。真正把「一次只有一个合并」
    /// 钉住的是「卡从 awaiting_merge 迁到 merging 后，第二次申请会因卡不在
    /// awaiting_merge 而 Denied」——名额的持久化载体是卡片状态，不是锁的持有期。
    ///
    /// （锁的持有期只有毫秒级，跨会话的合并轮无法靠 fd 保活。）
    pub fn merge_request(&self, card_id: &str) -> Result<MergeRequest, String> {
        // 名额闸：拿不到就是 Busy，绝不排队等待。
        let Some(gate) = crate::merge_lock::acquire(&self.state_dir) else {
            return Ok(MergeRequest::Busy);
        };
        let result = self.merge_request_inner(card_id);
        drop(gate);
        result
    }

    fn merge_request_inner(&self, card_id: &str) -> Result<MergeRequest, String> {
        let mut inner = self.lock();
        let id = CardId::new(card_id);
        let Some(card) = inner.board.get(&id).cloned() else {
            return Ok(MergeRequest::Denied(format!("unknown card: {card_id}")));
        };
        if !matches!(card.state, CardState::AwaitingMerge | CardState::NeedsYou) {
            return Ok(MergeRequest::Denied(format!(
                "card {card_id} is {:?}, not awaiting_merge/needs_you",
                card.state
            )));
        }
        // source/base：实现卡由 id 推导；合并卡用自带 refs。
        let (source, base) = match (card.source_ref.clone(), card.base_ref.clone()) {
            (Some(source), Some(base)) => (source, base),
            _ => {
                if card.kind != CardKind::Implementation {
                    return Ok(MergeRequest::Denied(format!(
                        "card {card_id} has no source/base to merge"
                    )));
                }
                (
                    format!("kanban/{}", crate::worktree::slugify(&card.id)),
                    crate::merge::default_branch(&self.project_root),
                )
            }
        };
        if !crate::merge::source_branch_exists(&self.project_root, &source) {
            return Ok(MergeRequest::Denied(format!(
                "source branch '{source}' does not exist"
            )));
        }
        let workdir = match crate::merge::prepare(&self.project_root, &base) {
            Ok(wt) => wt.path,
            Err(error) => return Ok(MergeRequest::Denied(error)),
        };
        inner
            .board
            .transition(&id, CardState::Merging)
            .map_err(|error| error.to_string())?;
        self.save(&inner);
        Ok(MergeRequest::Granted {
            workdir,
            source,
            base,
        })
    }

    /// 收尾一次合并轮：以 git 事实为准落状态。
    pub fn merge_finish(&self, card_id: &str) -> Result<MergeFinish, String> {
        let mut inner = self.lock();
        let id = CardId::new(card_id);
        let Some(card) = inner.board.get(&id).cloned() else {
            return Err(format!("unknown card: {card_id}"));
        };
        if card.state != CardState::Merging {
            return Ok(MergeFinish::Cleared);
        }
        let (source, base) = match (card.source_ref.clone(), card.base_ref.clone()) {
            (Some(source), Some(base)) => (source, base),
            _ => (
                format!("kanban/{}", crate::worktree::slugify(&card.id)),
                crate::merge::default_branch(&self.project_root),
            ),
        };
        let merged = crate::merge::source_branch_exists(&self.project_root, &source)
            && crate::merge::branch_merged_into(&self.project_root, &source, &base);
        let next = if merged {
            MergeFinish::Done
        } else {
            MergeFinish::NeedsYou
        };
        let state = if merged { CardState::Done } else { CardState::NeedsYou };
        inner
            .board
            .transition(&id, state)
            .map_err(|error| error.to_string())?;
        inner.leases.remove(&id);
        self.save(&inner);
        Ok(next)
    }
```

在 `mod tests` 内（仅测试可见）加一个直通助手：

```rust
    impl BoardService {
        /// 测试用：绕过名额把卡直接置到某状态（验证复核逻辑，不验证名额）。
        pub(crate) fn force_state_for_test(
            &self,
            card_id: &str,
            state: superpowers_kanban_core::card::CardState,
        ) {
            let mut inner = self.lock();
            inner
                .board
                .transition(&superpowers_kanban_core::card::CardId::new(card_id), state)
                .unwrap();
            self.save(&inner);
        }
    }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner merge_`
Expected: PASS（4 个新用例全过；`MergeRequest`/`MergeFinish` 需 `#[derive(Debug, PartialEq, Eq)]` 才能 `assert_eq!`/`panic!("{other:?}")`，已在上方声明）。

- [ ] **Step 5: 名额与槽位两条正交的验证（追加一个用例）**

在 `mod tests` 追加，锁住「合并不占会话槽位」：

```rust
    #[test]
    fn a_merging_card_does_not_consume_a_session_slot() {
        let (dir, project, state) = repo_with_awaiting_impl_card("card-1");
        let service = BoardService::new(state.clone(), project.clone(), None);
        let granted = service.merge_request("card-1").unwrap();
        assert!(matches!(granted, MergeRequest::Granted { .. }), "{granted:?}");
        // 全文唯一占槽判定仍是 Running。
        let board = crate::persist::load_board(&state.join("board.json"));
        let card = board.get(&superpowers_kanban_core::card::CardId::new("card-1")).unwrap();
        assert!(!card.state.occupies_slot());
        drop(dir);
    }
```

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner a_merging_card_does_not_consume_a_session_slot`
Expected: PASS。

- [ ] **Step 6: 全量插件测试**

Run: `cd plugins/superpowers-kanban && cargo test`
Expected: 全绿。

- [ ] **Step 7: Commit**

```bash
git add plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs
git commit -m "feat(kanban): 新增合并名额申请与 git 复核收尾

merge_request 判定卡是否可合并、试拿 merge.lock（拿不到即 Busy）、
准备 base worktree 并置卡为 merging；merge_finish 以
merge-base --is-ancestor 复核，真则 done、否则 needs_you。
名额的持久化载体是卡片状态（merging），锁只用于判定，不跨调用持有。"
```

---

### Task 3: dispatch 暴露 merge_request / merge_finish

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/dispatch.rs`
  （`match method` 内新增两个分支；文件底部 `mod tests`）

**Interfaces:**
- Consumes: Task 2 的 `BoardService::merge_request` / `merge_finish` 与
  `MergeRequest` / `MergeFinish`。
- Produces: 两个插件方法（宿主与 CLI 共用）：
  - `"merge_request"` params `{ card_id }` → `{"status":"granted","workdir":..,"source":..,"base":..}`
    | `{"status":"busy"}` | `{"status":"denied","reason":..}`
  - `"merge_finish"` params `{ card_id }` → `{"status":"done"|"needs_you"|"cleared"}`

- [ ] **Step 1: 写失败测试**

在 `dispatch.rs` 的 `mod tests` 追加：

```rust
    #[test]
    fn merge_request_reports_denied_for_an_unknown_card() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_dir(dir.path());
        let value = dispatch_with_global(
            &state,
            None,
            "merge_request",
            &json!({ "card_id": "nope" }),
        )
        .unwrap();
        assert_eq!(value["status"], "denied");
        assert!(value["reason"].as_str().unwrap().contains("nope"));
    }

    #[test]
    fn merge_finish_reports_cleared_when_the_card_is_not_merging() {
        let workdir = tempfile::tempdir().unwrap();
        let state = state_dir(workdir.path());
        let value = dispatch_with_global(
            &state,
            None,
            "merge_finish",
            &json!({ "card_id": "card-1" }),
        )
        .unwrap_err();
        // 卡根本不存在 → 明确报错，而不是静默 cleared。
        assert!(value.contains("unknown card"), "{value}");

        // 存在但不在 merging → cleared。
        let mut board = superpowers_kanban_core::board::Board::new();
        board.enqueue(
            superpowers_kanban_core::card::CardId::new("card-1"),
            "a.spec.md".into(),
            "a.plan.md".into(),
            chrono::Local::now(),
        );
        crate::persist::save_board(&state.join("board.json"), &board).unwrap();
        let value = dispatch_with_global(
            &state,
            None,
            "merge_finish",
            &json!({ "card_id": "card-1" }),
        )
        .unwrap();
        assert_eq!(value["status"], "cleared");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner merge_request_reports_denied_for_an_unknown_card`
Expected: FAIL —— 得到 `unknown method: merge_request`。

- [ ] **Step 3: 写实现**

在 `dispatch.rs` 的 `match method` 中，`"board.release" => { ... }` 之后插入：

```rust
        "merge_request" => {
            let card_id = params
                .get("card_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "merge_request needs a card_id".to_string())?;
            match service.merge_request(card_id)? {
                crate::service::MergeRequest::Granted {
                    workdir,
                    source,
                    base,
                } => Ok(json!({
                    "status": "granted",
                    "workdir": workdir.to_string_lossy(),
                    "source": source,
                    "base": base,
                })),
                crate::service::MergeRequest::Busy => Ok(json!({ "status": "busy" })),
                crate::service::MergeRequest::Denied(reason) => {
                    Ok(json!({ "status": "denied", "reason": reason }))
                }
            }
        }
        "merge_finish" => {
            let card_id = params
                .get("card_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "merge_finish needs a card_id".to_string())?;
            let status = match service.merge_finish(card_id)? {
                crate::service::MergeFinish::Done => "done",
                crate::service::MergeFinish::NeedsYou => "needs_you",
                crate::service::MergeFinish::Cleared => "cleared",
            };
            Ok(json!({ "status": status }))
        }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner merge_`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
git add plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/dispatch.rs
git commit -m "feat(kanban): dispatch 暴露 merge_request / merge_finish

宿主与 CLI 共用一个实现：申请名额返回 granted/busy/denied，
收尾返回 done/needs_you/cleared。"
```

---

### Task 4: CLI 子命令 merge-request / merge-finish

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs`
  （`enum Subcommand`、`parse_subcommand`、`USAGE`、`main` 的 `match`、`mod tests`）

**Interfaces:**
- Consumes: Task 3 的 dispatch 方法（经 `superpowers_kanban_runner::dispatch::dispatch_with_global`）。
- Produces:
  - `superpowers-kanban merge-request <card-id> [--state-dir <d>]`
    打印 `granted <workdir> (source -> base)` / `busy: another merge is running in this project`
    / `denied: <reason>`；`denied` 以退出码 1 结束。
  - `superpowers-kanban merge-finish <card-id> [--state-dir <d>]`
    打印 `done` / `needs_you` / `cleared`。

- [ ] **Step 1: 写失败测试**

在 `main.rs` 的 `mod tests` 追加：

```rust
    #[test]
    fn merge_request_and_finish_parse_one_card_id() {
        assert!(matches!(
            parse(&["merge-request", "card-1", "--state-dir", "/s"]),
            Ok(Subcommand::MergeRequest { .. })
        ));
        assert!(matches!(
            parse(&["merge-finish", "card-1", "--state-dir", "/s"]),
            Ok(Subcommand::MergeFinish { .. })
        ));
        assert!(parse(&["merge-request"]).is_err(), "缺 card id");
        assert!(parse(&["merge-finish"]).is_err(), "缺 card id");
        assert!(parse(&["merge-request", "a", "b"]).is_err(), "多参数");
    }

    #[test]
    fn merge_request_prints_denied_and_exits_nonzero_on_a_bad_card() {
        let dir = tempfile::tempdir().unwrap();
        let err = command_merge_request(dir.path(), "nope").unwrap_err();
        assert!(err.contains("denied"), "{err}");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner merge_request_and_finish_parse_one_card_id`
Expected: FAIL —— 未知子命令。

- [ ] **Step 3: 写实现**

在 `enum Subcommand` 中，`Done { .. }` 之后加：

```rust
    MergeRequest {
        state_dir: PathBuf,
        card_id: String,
    },
    MergeFinish {
        state_dir: PathBuf,
        card_id: String,
    },
```

在 `USAGE` 里加两行并更新首行清单：

```rust
const USAGE: &str =
    "usage: superpowers-kanban <run|add|add-merge|merge-request|merge-finish|done|archive|purge|list|on|off|workdir> [...]
  run           --runtime-dir <d> --state-dir <d> [--project-root <d>] [--interval-secs N]
  add           <spec> <plan> [--state-dir <d>]
  add-merge     <source> [--base <ref>] [--state-dir <d>]
  merge-request <card-id> [--state-dir <d>]
  merge-finish  <card-id> [--state-dir <d>]
  done          <card-id> [--state-dir <d>]
  archive       <card-id>|--all-terminal [--state-dir <d>]
  purge         <card-id>|--all-archived [--state-dir <d>]
  list          [--all] [--state-dir <d>]
  on|off        [--state-dir <d>]
  workdir       [--state-dir <d>]";
```

在 `parse_subcommand` 中，`Some("done") => { ... }` 之前加：

```rust
        Some(verb @ ("merge-request" | "merge-finish")) => {
            let (state_dir, rest) = parse_state_dir(args)?;
            let mut rest = rest.into_iter();
            let card_id = rest
                .next()
                .ok_or_else(|| format!("{verb} needs a card id\n{USAGE}"))?;
            if let Some(extra) = rest.next() {
                return Err(format!("{verb} takes one card id, got an extra: {extra}"));
            }
            Ok(if verb == "merge-request" {
                Subcommand::MergeRequest { state_dir, card_id }
            } else {
                Subcommand::MergeFinish { state_dir, card_id }
            })
        }
```

在 `main` 的 `match subcommand` 中，`Subcommand::Done { .. } => { ... }` 之后加：

```rust
        Subcommand::MergeRequest { state_dir, card_id } => {
            command_merge_request(&state_dir, &card_id)
        }
        Subcommand::MergeFinish { state_dir, card_id } => {
            command_merge_finish(&state_dir, &card_id)
        }
```

在 `command_done` 之前加两个命令实现：

```rust
/// `merge-request`：申请一次合并名额，把结果如实打印。
///
/// `granted` 打印执行合并的 base worktree 与 source→base；调用方（会话里的 agent）
/// 据此前去执行 `git merge --no-ff`，完成后必须回 `merge-finish`。
fn command_merge_request(state_dir: &std::path::Path, card_id: &str) -> Result<String, String> {
    let value = superpowers_kanban_runner::dispatch::dispatch(state_dir, "merge_request", &serde_json::json!({ "card_id": card_id }))
        .map_err(|error| error.to_string())?;
    match value.get("status").and_then(serde_json::Value::as_str) {
        Some("granted") => {
            let workdir = value.get("workdir").and_then(serde_json::Value::as_str).unwrap_or("");
            let source = value.get("source").and_then(serde_json::Value::as_str).unwrap_or("");
            let base = value.get("base").and_then(serde_json::Value::as_str).unwrap_or("");
            Ok(format!(
                "granted\nworkdir: {workdir}\nmerge {source} into {base} in that worktree, then run `superpowers-kanban merge-finish {card_id}`"
            ))
        }
        Some("busy") => Ok("busy: another merge is running in this project; wait for it to finish".to_string()),
        Some("denied") => Err(format!(
            "denied: {}",
            value.get("reason").and_then(serde_json::Value::as_str).unwrap_or("card cannot be merged")
        )),
        other => Err(format!("unexpected merge_request result: {other:?}")),
    }
}

/// `merge-finish`：让插件以 git 复核收尾。
fn command_merge_finish(state_dir: &std::path::Path, card_id: &str) -> Result<String, String> {
    let value = superpowers_kanban_runner::dispatch::dispatch(state_dir, "merge_finish", &serde_json::json!({ "card_id": card_id }))
        .map_err(|error| error.to_string())?;
    match value.get("status").and_then(serde_json::Value::as_str) {
        Some(status @ ("done" | "needs_you" | "cleared")) => Ok(status.to_string()),
        other => Err(format!("unexpected merge_finish result: {other:?}")),
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner merge_`
Expected: PASS。

- [ ] **Step 5: 全量插件测试 + fmt**

Run: `cd plugins/superpowers-kanban && cargo fmt --all && cargo test`
Expected: 全绿。

- [ ] **Step 6: Commit**

```bash
git add plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs
git commit -m "feat(kanban): CLI 新增 merge-request / merge-finish

会话里的 agent 靠这两条命令申请名额与回执；denied 以非零退出码
结束，busy 提示本项目已有合并在跑。"
```

---

### Task 5: skill 增加合并入口（会话内说人话）

**Files:**
- Modify: `plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md`

**Interfaces:**
- Consumes: Task 4 的两条 CLI 命令。
- Produces: skill 文档里一套可执行的合并流程（无需代码，故无单测；以「流程步骤可照做」为验收）。

- [ ] **Step 1: 改写「合并入队」一节**

把现有 `### 2b. 合并入队` 整节替换为下面两节（保留 `add-merge` 作为「新建独立合并卡」的入口，
新增「在卡片会话里推进合并」作为主路径）：

```markdown
### 2b. 把一张实现卡合并进主线（主路径）

用户在同一张卡片的**原会话**里说「合并 / 把这条合进 main / 这张卡可以合了」时，
你**不再**自己 `git merge`，而是走名额：

1. **申请名额**：

   ```bash
   superpowers-kanban merge-request <card-id>
   ```

   - 打印 `granted`：记下它给的 `workdir`，继续第 2 步。
   - 打印 `busy: another merge is running in this project`：本项目已有合并在跑。
     **如实告诉用户「排队中」**，停下来等用户稍后再说一次。不要轮询重试。
   - 报 `denied: …`：把原因原样转述（多半是卡不在 `awaiting_merge`，或 source 分支不存在）。
     不要猜、不要绕。

2. **在名额给的 worktree 里合并**：

   ```bash
   git -C <workdir> merge --no-ff <source> -m "merge <source> into <base> (kanban)"
   ```

   `granted` 输出里的 `merge <source> into <base> in that worktree` 一句就是这条命令的参数来源。
   **必须在 `<workdir>` 里执行**——它是 base 分支被检出的地方（base 就是 main 时即主检出）。

   - 干净合并：继续第 3 步。
   - 有冲突：就地解决（这是你被叫来的原因）。解决后 `git add` 冲突文件并
     `git -C <workdir> commit --no-edit` 收尾合并提交。
   - 解决不了：**不要** `git merge --abort` 后就完事——照第 3 步如实回执，
     让卡停在待处理，并在回复里说清卡在哪些文件。

3. **回执让插件复核**：

   ```bash
   superpowers-kanban merge-finish <card-id>
   ```

   - `done`：git 复核确认 `source` 已并入 `base`。告诉用户已合并。
   - `needs_you`：复核不通过（合并没真正落地）。**如实说**，并把卡的现状回报。
   - `cleared`：卡已不在合并中（多半已被别处推进）。不要自行改状态。

**绝不绕过名额直接 `git merge`。** 名额（`merge.lock`）保证同一项目同一时刻只有一个合并，
绕过去会让两条分支同时改主检出。

### 2c. 投递一张独立的合并卡（旧路径，保留）

需要为一个**没有实现卡**的分支（例如手工建的分支）单独立卡时：

```bash
superpowers-kanban add-merge <source> [--base <ref>] [--state-dir <dir>]
```

`--base` 缺省取仓库默认分支。命令**立刻**校验 refs 与 source 分支存在；
失败原样转述，不要猜分支名、不要重试。
```

- [ ] **Step 2: 更新「确认合并」一节的旧语义**

把 `### 5. 确认合并（更新看板进度）` 一节首句与第 2 步里的 `done` 用法保留（它仍是
「人工确认已合并」的兜底），但在节首加一句指针：

```markdown
**优先走 `### 2b`**（会话内申请名额并实际执行合并）。本节只用于「用户已在别处
手工合并完、只想让看板收尾」的情形。
```

- [ ] **Step 3: 校验（无自动化测试）**

Run: `grep -n "merge-request\|merge-finish\|绝不绕过名额" plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md`
Expected: 三条命中齐全。

- [ ] **Step 4: Commit**

```bash
git add plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md
git commit -m "docs(kanban): skill 增加会话内合并入口

把「合进主线」改成申请名额 → 在 granted worktree 里合并 → 回执复核
三步，并写明绝不绕过名额；旧 add-merge 降为独立合并卡的兜底路径。"
```

---

### Task 6: 宿主把 needs_you 的再说话接成重新申请，并处理 merging 兜底

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/card_scheduler.rs`
  （`TrackState`、`plan`、`CardAction`、`mod tests`）

**Interfaces:**
- Consumes: 既有 `BoardCard`（`state`/`thread_id`）、`TrackedThread`。
- Produces: `CardAction` 新增 `RequestMerge { card_id: String, thread_id: String }`；
  `plan` 在「tracked 卡进入空闲且卡处于 `merging`」时产出该动作（交由宿主接线执行
  `merge_finish`），并在「`needs_you` 的会话又忙起来」时产出 `Outcome::Running`（重新申请名额的前置）。

**说明：** 宿主**不**直接 `git merge`；它只在卡处于 `merging` 且会话已空闲时，代调
`board_query(..., "merge_finish", ...)` 收尾（幂等：`merge_finish` 以 git 复核为准）。

- [ ] **Step 1: 写失败测试**

在 `card_scheduler.rs` 的 `mod tests` 追加（沿用该文件既有的 `card(...)`/`tracked(...)` 助手风格；
若助手名不同，按现有测试对齐）：

```rust
    #[test]
    fn an_idle_merging_card_is_finished_by_the_host() {
        use std::collections::HashMap;
        let cards = vec![board_card("c1", "merging", Some("t1"))];
        let mut tracked = HashMap::new();
        tracked.insert(
            "c1".to_string(),
            TrackedThread {
                thread_id: "t1".into(),
                state: TrackState::Running,
                idle: true,
                failed: false,
                needs_you: false,
            },
        );
        let actions = plan(&cards, &tracked);
        assert!(matches!(
            actions.as_slice(),
            [CardAction::RequestMerge { card_id, .. }] if card_id == "c1"
        ));
    }

    #[test]
    fn a_merging_card_still_running_is_left_alone() {
        use std::collections::HashMap;
        let cards = vec![board_card("c1", "merging", Some("t1"))];
        let mut tracked = HashMap::new();
        tracked.insert(
            "c1".to_string(),
            TrackedThread {
                thread_id: "t1".into(),
                state: TrackState::Running,
                idle: false,
                failed: false,
                needs_you: false,
            },
        );
        assert!(plan(&cards, &tracked).is_empty());
    }
```

并在该 `mod tests` 内加一个构造 `BoardCard` 的助手（如不存在）：

```rust
    fn board_card(id: &str, state: &str, thread_id: Option<&str>) -> crate::server::BoardCard {
        crate::server::BoardCard {
            id: id.to_string(),
            state: state.to_string(),
            kind: "implementation".into(),
            source: None,
            base: None,
            thread_id: thread_id.map(str::to_string),
            workdir: None,
            spec_path: String::new(),
            plan_path: String::new(),
        }
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server an_idle_merging_card_is_finished_by_the_host`
Expected: FAIL —— `no variant named RequestMerge`。

- [ ] **Step 3: 写实现**

在 `enum CardAction` 中加：

```rust
    /// 卡处于 `merging` 且其会话已空闲：宿主代调 `merge_finish` 收尾。
    /// 复核以 git 为准，故这个动作幂等。
    RequestMerge { card_id: String, thread_id: String },
```

在 `plan` 内 `let t = tracked.get(&card.id)?;` 之后、现有 `match t.state` 之前插入：

```rust
            // 合并轮：卡在 merging 时，会话空闲即收纳尾（会话正在跑则等它）。
            if card.state == "merging" {
                return if t.idle {
                    Some(CardAction::RequestMerge {
                        card_id: card.id.clone(),
                        thread_id,
                    })
                } else {
                    None
                };
            }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server card_scheduler`
Expected: PASS（新用例 + 既有 `plan` 用例全过）。

- [ ] **Step 5: 接线到卡片调度 tick**

在 `server.rs` 的 `card_scheduler_tick`（约 5676 行）里，处理 `CardAction::Reconcile` 的
`match action`（约 5746 行附近）旁新增分支：

```rust
                CardAction::RequestMerge { card_id, .. } => {
                    let _ = board_query(
                        &board.project,
                        board_dir,
                        "merge_finish",
                        json!({ "card_id": card_id }),
                    );
                }
```

（若该 `match` 是 `for action in actions` 形式，按既有 `Reconcile` 分支的写法填入同一处。）

- [ ] **Step 6: 全量宿主测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server`
Expected: 全绿。

- [ ] **Step 7: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/card_scheduler.rs yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): merging 卡会话空闲时由宿主代调 merge_finish

合并轮的 git 动作发生在会话内；宿主只负责在会话空闲时触发复核收尾
（幂等，以 git 复核为准）。"
```

---

### Task 7: 桌面为 merging 补状态文案

**Files:**
- Modify: `desktop/src/lib/superpowersKanbanBoard.ts`（`columnForState` 已有 `merging → doing`，**无需改**）
- Test: `desktop/src/lib/superpowersKanbanBoard.test.ts`（追加一条锁住 `merging` 落 DOING）

**Interfaces:**
- Consumes: 既有 `columnForState`。
- Produces: 无新接口；补一条回归测试，防止日后有人把 `merging` 误挪出 `doing`。

- [ ] **Step 1: 写测试**

在 `desktop/src/lib/superpowersKanbanBoard.test.ts` 追加：

```ts
describe("columnForState merging", () => {
  it("合并中的卡落 DOING", () => {
    // 合并是一轮进行中的工作，放 needDecision 会让人以为要自己动手。
    expect(columnForState("merging")).toBe("doing");
  });
});
```

- [ ] **Step 2: 跑测试**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/lib/superpowersKanbanBoard.test.ts`
Expected: PASS（实现已存在；此步只是把契约钉成测试）。

- [ ] **Step 3: Commit**

```bash
git add desktop/src/lib/superpowersKanbanBoard.test.ts
git commit -m "test(desktop): 钉住 merging 落 DOING 的列映射"
```

---

## 验收（人可验证）

1. `cd plugins/superpowers-kanban && cargo test` 与 `cd yi-agent-rs && cargo test -p yi-agent-app-server` 全绿。
2. 在一条实现卡会话里说「合并」→ 卡进「合并中」（DOING 列）；同项目此时对另一张卡发起合并 → 返回 `busy`，卡不动。
3. 合并干净 → 卡 `done`；合并未落地/未解完 → 卡停「待处理」，回会话接着说可再次申请名额重试。
4. 崩溃后重启：停在 `merging` 的卡由 `migrate_legacy_running` 收成 `needs_you`（既有逻辑），不自动重试。

## Self-Review

- **Spec 覆盖**：§4.1 迁移表 → Task 1；§4.2 名额 + §4.3 流程 → Task 2；§5.1/5.2 CLI+dispatch →
  Task 3/4；§5.3 skill → Task 5；§5.4 宿主对账（`merging` 收尾）→ Task 6；§6.2 桌面列映射 →
  Task 7。**未覆盖**：§5.4 的 D10「`needs_you` 再说话 → 重新申请名额」在 `plan` 里**本来就有**
  （`TrackState::AwaitingMerge` + 非空闲 → `Outcome::Running`），故不设任务，仅在本计划 Task 6
  的 Interfaces 里点明它已满足。
- **修正**：§D5「专用合并 worktree，不碰主检出」不可实现（git 拒绝重复检出同一分支）——
  已在「关键事实 1」写明，实现沿用 `merge::prepare` 的复用语义。
- **占位符**：无。每个代码步骤都给出可直接粘贴的完整代码（Task 1 的迁移表断言已修正为合法写法）。
- **类型一致**：`MergeRequest`/`MergeFinish` 的变体在 Task 2 定义、Task 3 消费、Task 4 转成
  字符串，名称逐一对齐；`CardAction::RequestMerge` 在 Task 6 内自洽。
