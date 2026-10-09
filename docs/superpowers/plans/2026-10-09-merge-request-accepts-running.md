# 合并名额接受 `running` 卡 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让停在 `awaiting_merge` 的实现在用户发话触发合并轮、被宿主对账翻回 `running` 之后，仍然能申请到合并名额。

**Architecture:** 只动插件两处纯逻辑：`core/card.rs` 的状态机放行 `running → merging`；`runner/service.rs` 的 `merge_request` 准入放宽到接受 `running`。宿主对账、桌面、IPC 协议、CLI 契约一律不变——名额的持久载体仍是卡片状态 `Merging`，`merge.lock` 仍只管「此刻是否已有合并」。

**Tech Stack:** Rust（edition 2024，`rust-version = 1.85`）、`serde_json`、flock（`libc`）；插件 crates `superpowers-kanban-core`（无 I/O）与 `superpowers-kanban-runner`（做 I/O）。

**依据 spec:** `docs/superpowers/specs/2026-10-09-merge-request-accepts-running-design.md`（§3 决策 D1–D5、§4 状态机与名额、§6 测试与验收）。

## Global Constraints

- **插件不新增第三方依赖。** 只用现有 crate 与 `std`。
- `superpowers-kanban-core` **必须保持无 I/O**：状态机、纯判断留在 core；`git`/文件/socket 一律留在 runner。
- 插件命令：`cd plugins/superpowers-kanban && cargo fmt --all && cargo test`（**不要并发**跑多个 `cargo test`）。
- **不要改宿主代码**（`yi-agent-rs/`）：本设计不动 `card_scheduler.rs` 的 `plan` 翻转语义。
- 锁一律 flock，**不 unlink**。
- 注释中文、讲「为什么」；测试名中文、说清锁住的契约。
- commit 用 conventional commits，正文中文，**不写 `Co-Authored-By`**。
- 不改全局并发日历：`occupies_slot()` 仍只认 `Running`。

---

### Task 1: 状态机放行 `running → merging`

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card.rs`
  （`can_transition_to` 的 `match (self, next)`，现 87-90 行的合并阶段块；文件底部 `mod tests`）

**Interfaces:**
- Consumes: 无。
- Produces: `CardState::can_transition_to` 新增一条合法迁移 `(Running, Merging)`。
  **保持** `(Merging, Running)` 为假（合并阶段不回流会话通路），`(AwaitingMerge, Running)` 仍为真（`23846506` 显示语义零回归）。

- [ ] **Step 1: 写失败测试**

在 `card.rs` 底部的 `mod tests` 里追加（放在既有 `a_card_may_enter_and_leave_the_merge_stage` 之后）：

```rust
    #[test]
    fn a_running_card_may_enter_the_merge_stage_when_the_user_asks() {
        use CardState::*;
        // 用户在一张正在跑的实现卡上发话要求合并：宿主对账可能已先把
        // awaiting_merge 的卡翻回 running，此时仍必须能进 merging，
        // 否则合并名额永远申请不到（本 bug 的核心）。
        assert!(Running.can_transition_to(Merging));
        // 合并阶段绝不回流会话通路。
        assert!(!Merging.can_transition_to(Running));
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core a_running_card_may_enter_the_merge_stage_when_the_user_asks`
Expected: FAIL —— `assertion failed: Running.can_transition_to(Merging)`（当前返回 `false`）。

- [ ] **Step 3: 改实现**

在 `card.rs` 的 `match (self, next)` 中，`(NeedsYou, Merging) => true,` 之**后**插入：

```rust
            // 用户在一张**正在跑**的实现卡上发话要求合并：宿主对账可能已先
            // 把 awaiting_merge 的卡翻回 running（见 2026-10-03 看板状态误判），
            // 不放行这条边则合并名额永远申请不到。反向的 merging → running
            // 仍不合法（见上方 `_ => false`）：合并阶段不回流会话通路。
            (Running, Merging) => true,
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core`
Expected: PASS（该 crate 全部用例通过，含既有迁移表用例与 `a_reviewed_card_can_resume_running_so_follow_up_turns_stay_visible`）。

- [ ] **Step 5: Commit**

```bash
git add plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card.rs
git commit -m "feat(kanban): 状态机放行 running → merging

合并已改为在原会话里发话触发，而用户发话会让会话变忙、宿主对账
把 awaiting_merge 的卡翻回 running。不放行这条边，合并名额就永远
申请不到。merging → running 仍拒绝，合并阶段不回流会话通路。"
```

---

### Task 2: `merge_request` 准入放宽到 `running`

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`
  （`merge_request_locked` 的准入判断，现 484-489 行；文件底部 `mod tests`）

**Interfaces:**
- Consumes: Task 1 的 `(Running, Merging)` 迁移（否则 `transition(id, Merging)` 会失败）。
- Produces: `BoardService::merge_request` 对 `CardState::Running` 的卡也返回
  `MergeRequest::Granted`（而非 `Denied`），并把拒绝文案改为
  `"card {card_id} is {:?}, not awaiting_merge/needs_you/running"`。
  返回类型与变体不变：`MergeRequest::{Granted { workdir, source, base }, Busy, Denied(String)}`。

- [ ] **Step 1: 写失败测试**

在 `service.rs` 的 `mod tests` 里追加（放在既有 `merge_request_grants_and_marks_the_card_merging` 之后）：

```rust
    /// 一张正在跑的实现卡（宿主对账已把它翻回 running）：用户发话要求合并
    /// 仍应拿到名额。这是本 bug 的回归锁。
    #[test]
    fn merge_request_grants_for_a_running_card_whose_session_is_live() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        // 走生产路径：启动 → awaiting_merge → 会话又忙（对账翻回 running）。
        service.mark_running("card-1", "thread-1").unwrap();
        service
            .mark_terminal("card-1", "awaiting_merge", None)
            .unwrap();
        service.mark_running("card-1", "thread-1").unwrap();
        assert_eq!(state_of(&service, "card-1"), "running");

        let slug = crate::worktree::slugify(&CardId::new("card-1"));
        let source = format!("kanban/{slug}");
        git_run(&project, &["branch", &source]);

        match service.merge_request("card-1").unwrap() {
            MergeRequest::Granted {
                source: s, base, ..
            } => {
                assert_eq!(s, source);
                assert_eq!(base, "main");
            }
            other => panic!("expected Granted, got {other:?}"),
        }
        assert_eq!(state_of(&service, "card-1"), "merging");
        // 已在 merging：第二次申请被拒（名额的持久载体是卡片状态）。
        match service.merge_request("card-1").unwrap() {
            MergeRequest::Denied(reason) => {
                assert!(reason.contains("not awaiting_merge"), "{reason}")
            }
            other => panic!("expected Denied on second request, got {other:?}"),
        }
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner merge_request_grants_for_a_running_card_whose_session_is_live`
Expected: FAIL —— `expected Granted, got Denied("card card-1 is Running, not awaiting_merge/needs_you")`。

- [ ] **Step 3: 写实现**

把 `merge_request_locked` 里的准入块

```rust
        if !matches!(card.state, CardState::AwaitingMerge | CardState::NeedsYou) {
            return Ok(MergeRequest::Denied(format!(
                "card {card_id} is {:?}, not awaiting_merge/needs_you",
                card.state
            )));
        }
```

替换为：

```rust
        // 准入：等待验收、需要用户决定、以及**正在跑**的卡都可以申请名额。
        // 正在跑也要接受，是因为合并已改为「原会话里发话触发」——用户发话本身
        // 会让会话变忙，宿主对账随即把卡翻回 running；若这里仍拒绝 running，
        // 合并就永远申请不到名额（本 bug）。真正的闸是「用户显式发话」
        // （只有 CLI/skill 会调 merge_request，宿主从不自动调）、source 分支存在、
        // 以及每项目一把 merge.lock。
        if !matches!(
            card.state,
            CardState::AwaitingMerge | CardState::NeedsYou | CardState::Running
        ) {
            return Ok(MergeRequest::Denied(format!(
                "card {card_id} is {:?}, not awaiting_merge/needs_you/running",
                card.state
            )));
        }
```

其余逻辑（source/base 推导、`source_branch_exists` 校验、`merge::prepare`、`transition(id, Merging)`）**一字不改**。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner merge_`
Expected: PASS（新增用例 + 既有 `merge_request_denies_a_card_that_is_not_waiting_to_merge`、
`merge_request_grants_and_marks_the_card_merging`、`merge_request_is_busy_while_the_gate_is_held`、
`merge_finish_*` 全过；既有拒绝用例断言的是 `contains("not awaiting_merge")`，新文案仍含该子串，无需改）。

- [ ] **Step 5: 全量插件测试 + fmt**

Run: `cd plugins/superpowers-kanban && cargo fmt --all && cargo test`
Expected: 全绿。

- [ ] **Step 6: Commit**

```bash
git add plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs
git commit -m "fix(kanban): merge_request 接受 running 卡

卡停在 awaiting_merge 时用户在会话里说合并，会话变忙会让宿主对账
把卡翻回 running，旧准入只看 awaiting_merge/needs_you 因而拒绝，
卡进不了合并队列。准入放宽到 running；真正的闸仍是用户发话 +
source 分支存在 + 每项目 merge.lock，卡一旦 granted 进 merging，
第二次申请依旧被拒。"
```

---

### Task 3: 同步项目进度文档

**Files:**
- Modify: `docs/project-management/yi-agent-app-server.md`（文件末尾追加一条 bullet）
- Modify: `docs/project-management/README.md:27`（`yi-agent-app-server` 计数 `32 / 32` → `33 / 33`）

**Interfaces:**
- Consumes: Task 1/2 的最终代码状态。
- Produces: 无代码接口；只更新人可读的进度台账。

- [ ] **Step 1: 追加台账条目**

在 `docs/project-management/yi-agent-app-server.md` 末尾追加一行：

```markdown
- [x] 看板合并名额接受 `running` 卡（修复「等待合并的卡在会话里发话后进不了合并队列」）— 卡停在 `awaiting_merge` 时用户在原会话说「合并」，该轮会话活动让宿主对账把它翻回 `running`（`src/card_scheduler.rs:110` 的 `TrackState::AwaitingMerge` + 非空闲 → `Outcome::Running`），而 `merge_request` 旧准入只认 `awaiting_merge`/`needs_you`，导致 `denied`、卡进不了合并队列。修复：状态机放行 `(Running, Merging)`（`plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card.rs`），`merge_request` 准入放宽为 `awaiting_merge`/`needs_you`/`running`（`plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs` 的 `merge_request_locked`）；真正的闸仍是用户发话 + source 分支存在 + 每项目 `merge.lock`，`granted` 后卡进 `merging`，第二次申请仍被拒。宿主与桌面零改动，`merging → running` 仍拒绝。验证：`cargo test -p superpowers-kanban-core a_running_card_may_enter_the_merge_stage_when_the_user_asks`、`cargo test -p superpowers-kanban-runner merge_request_grants_for_a_running_card_whose_session_is_live` — [设计](../superpowers/specs/2026-10-09-merge-request-accepts-running-design.md)
```

- [ ] **Step 2: 同步模块索引计数**

把 `docs/project-management/README.md` 第 27 行

```markdown
| yi-agent-app-server | 32 / 32 | [详情](./yi-agent-app-server.md) |
```

改为

```markdown
| yi-agent-app-server | 33 / 33 | [详情](./yi-agent-app-server.md) |
```

- [ ] **Step 3: 校验**

Run: `grep -n "33 / 33" docs/project-management/README.md && grep -n "合并名额接受" docs/project-management/yi-agent-app-server.md`
Expected: 两条命中齐全。

- [ ] **Step 4: Commit**

```bash
git add docs/project-management/yi-agent-app-server.md docs/project-management/README.md
git commit -m "docs: 记录看板合并名额接受 running 卡

台账新增一条并同步模块索引计数 32/32 → 33/33。"
```

---

## 验收（人可验证）

1. `cd plugins/superpowers-kanban && cargo fmt --all && cargo test` 全绿。
2. `git -C yi-agent-rs status` 无改动——本次**不碰宿主**。
3. 原生复现路径（可选，需真实看板）：拿一张 `awaiting_merge` 实现卡，在其会话里说「合并」→
   卡进「合并中」（DOING 列），不再 `denied`；合并成功 → `done`，冲突/未落地 → `needs_you`
   且回会话接着说可再次申请名额重试。

## Self-Review

- **Spec 覆盖**：§3 D1（准入放宽）→ Task 2；D2（放行 `(Running,Merging)`、保持 `(Merging,Running)` 拒绝）
  → Task 1；D3（名额持久载体仍是 `Merging`）→ Task 2 Step 1 第二段断言；D4（拒绝文案更新）→ Task 2 Step 3；
  D5（不新增失败边）→ 无代码改动（Task 2 明确「其余逻辑一字不改」，`(Merging, Failed)` 未触碰）。
  §6 测试：core 三条 → Task 1；runner 两条 → Task 2；宿主回归「零实现改动」→ 无任务（Global Constraints 明确不改宿主）。
  §7 涉及文件 → Task 1/2/3 逐一对应。
- **占位符**：无。每个代码步骤给出可直接粘贴的完整代码与精确行号锚点。
- **类型一致**：`MergeRequest::Granted { workdir, source, base }` 与 `MergeFinish` 未改；
  测试助手 `service_with_card`/`git_run`/`state_of`/`project_with_worktree`、`crate::worktree::slugify`
  均为该文件既有符号（`service.rs:710/742/1084/1098`），名称逐一对齐。
- **风险**：放宽准入后仍要求 source 分支存在，且 `merge_request` 只由 CLI/skill 触发（宿主只调 `merge_finish`），
  不存在自动路径滥用；`23846506` 显示语义由既有 `a_card_awaiting_review_is_revived_when_its_thread_runs_again`
  与 core 的 `a_reviewed_card_can_resume_running_so_follow_up_turns_stay_visible` 双向锁住。
