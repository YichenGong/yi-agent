# 看板状态流转改由模型驱动 — 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把看板的卡片状态从「daemon 用 idle/HEAD 猜测」改为「模型显式调用 CLI 声明」，daemon 只写它亲手做过的事。

**Architecture:** 三层分工。插件 daemon 只写「排队 / 启动成功 / 启动失败」三种它亲手执行的动作；模型经新增的 `superpowers-kanban state` 动词写其余状态；`observations.json` 作为只读旁路承载「会话是否还活着」，`list`/桌面只读它派生 `stale`，永不写回卡片。并发改为按宿主上报的活会话数计，与卡片状态解耦。

**Tech Stack:** Rust（插件 workspace + 宿主 app-server）、TypeScript（desktop）、Superpowers 插件 CLI/IPC/skill 三件套。

## Global Constraints

- 状态集固定 8 个：`PreQueued` / `Queued` / `Running` / `NeedsYou` / `AwaitingMerge` / `Done` / `Failed` / `Cancelled`。删除 `Launching`、`Merging`、`Paused`。
- 迁移校验只有一条：终态（`Done`/`Failed`/`Cancelled`）不可复活，其余任意设置。
- daemon **不得**读取 git 判断卡片状态（不调 `git merge-base --is-ancestor`、不调 `rev-parse` 做状态决策、不跑 `git merge --abort`）。
- `observations.json` 是只读旁路：只有 daemon 写，且**永不**写回 `board.json` 的卡片状态。
- 合并名额：全项目同一时刻只有一个进程能合并在执行。
- 所有新增/修改的 Rust 代码 `cargo fmt --all` 必过；commit message 用 conventional commits，不写 Co-Authored-By。
- 插件对主程序保持零依赖（不得 `use yi_agent_*`）。

---

## 文件结构

**插件 core（`plugins/superpowers-kanban/crates/superpowers-kanban-core/src/`）**
- `card.rs` — 状态枚举、迁移表、`Card` 字段（删 `base_commit`）
- `board.rs` — 队列、认领、归档；删除 `start_due`/`running_count`/`free_slots` 的状态耦合
- `converge.rs` — **删除**

**插件 runner（`plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/`）**
- `service.rs` — `driver_step` 收缩为「回报 → 启动」；`state` 与合并名额的新实现
- `reconcile.rs` — **删除**
- `merge_lock.rs` — 加 holder 记录（`{card_id,pid,since}`）与残留回收
- `observation.rs` — **新建**：`observations.json` 的读写与 `stale` 判定
- `runner.rs` — 删除 `state_for_task_state`、`plan_launches`
- `dispatch.rs` — 新增 `card.state` 方法；`driver/next` 解析 `alive`
- `tick.rs` — objective 文案追加职责说明
- `main.rs` — `state` 动词、`--self`、`list` 输出 stale
- `lib.rs` — 去掉 `pub mod reconcile;`，加 `pub mod observation;`

**宿主（`yi-agent-rs/crates/yi-agent-app-server/src/`）**
- `session_driver.rs` — `DriverThread` 加 `alive`、payload 带上
- `server.rs` — idle 快照同时算 `alive`；装配期注入 `SUPERPOWERS_KANBAN_CARD`

**契约**
- `plugins/superpowers-kanban/crates/superpowers-kanban-runner/contract/driver-protocol.json`
- `plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md`

**桌面**
- `desktop/src/lib/superpowersKanbanState.ts` — `BoardCard` 加 `live`/`lastActiveAt`/`stale`
- `desktop/src/lib/superpowersKanbanSwitch.ts` — `fetchBoard` 返回里带观测
- `desktop/src/components/SuperpowersKanbanCard.tsx` — 角标 + 最后活动时间
- `desktop/src/lib/superpowersKanbanBoard.ts` — `columnForState` 删除已删状态

---

### Task 1: 状态枚举收缩为 8 个

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/board.rs`
- Test: 同文件 `#[cfg(test)]`

**Interfaces:**
- Produces: `CardState` 只有 8 个变体；`CardState::is_terminal(self) -> bool` 语义不变；`CardState::can_transition_to(self, next: CardState) -> bool` 新语义 = `!self.is_terminal() && self != next`。
- 删除：`CardState::Launching`、`CardState::Merging`、`CardState::Paused`、`CardState::occupies_slot`。

- [ ] **Step 1: 写失败测试**

在 `card.rs` 的 `tests` 模块里替换既有迁移测试为：

```rust
#[test]
fn any_non_terminal_state_may_move_to_any_other() {
    use CardState::*;
    let live = [PreQueued, Queued, Running, NeedsYou, AwaitingMerge];
    for from in live {
        for to in live {
            if from == to {
                assert!(!from.can_transition_to(to), "{from:?} -> {to:?} must be rejected");
            } else {
                assert!(from.can_transition_to(to), "{from:?} -> {to:?} must be allowed");
            }
        }
        assert!(from.can_transition_to(Done), "{from:?} -> Done");
        assert!(from.can_transition_to(Failed), "{from:?} -> Failed");
        assert!(from.can_transition_to(Cancelled), "{from:?} -> Cancelled");
    }
}

#[test]
fn a_terminal_state_never_revives() {
    use CardState::*;
    for from in [Done, Failed, Cancelled] {
        for to in [PreQueued, Queued, Running, NeedsYou, AwaitingMerge, Done, Failed, Cancelled] {
            assert!(!from.can_transition_to(to), "{from:?} -> {to:?} must be rejected");
        }
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core --lib card::`
Expected: 编译失败，`Launching`/`Merging`/`Paused` 仍在（旧测试引用它们）

- [ ] **Step 3: 改枚举与迁移表**

`card.rs` 里把三个变体及其全部迁移边删除，`can_transition_to` 收缩为：

```rust
pub fn can_transition_to(self, next: CardState) -> bool {
    !self.is_terminal() && self != next
}
```

删除 `occupies_slot`。同时删除 `Card.base_commit` 字段（及 `board.rs` 的 `set_base_commit`），因为产出判断不再由 daemon 做。

- [ ] **Step 4: 修 board.rs 的连带**

- 删除 `Board::start_due`、`Board::running_count`、`Board::free_slots`
- `claim_next_launch` 改为不再判名额（名额判据在 service 的租约里）：

```rust
pub fn claim_next_launch(&mut self) -> Option<CardId> {
    let next = self.next_startable()?;
    if let Some(card) = self.cards.iter_mut().find(|c| c.id == next) {
        card.state = CardState::Running;
    }
    Some(next)
}
```

- `archive` 拒绝活跃卡的集合改为 `Queued | Running`
- 删除所有引用 `Launching`/`Merging`/`Paused` 的测试，按新语义重写

- [ ] **Step 5: 跑 core 测试**

Run: `cargo test -p superpowers-kanban-core --lib`
Expected: PASS（runner crate 此时会编译失败，先不管）

- [ ] **Step 6: Commit**

```bash
cd plugins/superpowers-kanban && cargo fmt --all && git add crates/superpowers-kanban-core && git commit -m "refactor(kanban): shrink card states to eight and loosen transitions"
```

---

### Task 2: 删除收敛与 git 判断（daemon 不再猜状态）

**Files:**
- Delete: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/converge.rs`
- Delete: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/reconcile.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/lib.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/lib.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs`（删 `reconcile_merged` 调用）

- [ ] **Step 1: 写失败测试**

在 `service.rs` 的 tests 里加：

```rust
/// daemon **不得**再按 git 事实改卡片状态：一张 awaiting_merge 的卡，即使
/// 分支已并入 base，也必须原样停着等模型/人发话。
#[test]
fn a_merged_branch_never_flips_a_card_to_done_by_itself() {
    let dir = tempfile::tempdir().unwrap();
    let project = project_with_worktree(dir.path());
    let (service, branch) = service_with_awaiting_card(&project);
    git_run(&project, &["merge", "--no-ff", "--no-edit", &branch]);
    // 给 daemon 一拍机会（曾经的 reconcile_merged 入口已不存在，这里断言它是
    // 编译期消失的：本测试的存在即是回归锁——API 一旦回来就会被误用）。
    assert_eq!(state_of(&service, "card-1"), "AwaitingMerge");
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --lib a_merged_branch_never`
Expected: 编译失败（`ClaimNextLaunch` 签名已变、`service_with_awaiting_card` 用到已删状态）

- [ ] **Step 3: 删模块与调用点**

- 删 `core/src/converge.rs`，从 `core/src/lib.rs` 去掉 `pub mod converge;`
- 删 `runner/src/reconcile.rs`，从 `runner/src/lib.rs` 去掉 `pub mod reconcile;`
- `main.rs::run_daemon` 里删除 `for report in service.reconcile_merged()` 整段
- `service.rs` 删除 `reconcile_merged`、`notified_missing_branch` 字段

- [ ] **Step 4: 跑测试**

Run: `cargo test -p superpowers-kanban-core --lib && cargo test -p superpowers-kanban-runner --lib a_merged_branch_never`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && git add -A crates && git commit -m "refactor(kanban): drop git-driven state convergence"
```

---

### Task 3: `driver_step` 只保留「回报 → 启动」

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs:292-500`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/runner.rs`

**Interfaces:**
- Consumes: Task 1 的 `claim_next_launch()` 新签名
- Produces: `pub fn driver_step(&self, threads: &[(ThreadFacts)], results: &[DriverResult]) -> Result<Option<DriverAction>, String>`，其中 `ThreadFacts { thread_id: String, idle: bool, alive: bool }`。`DriverAction` 只剩 `Launch`。

- [ ] **Step 1: 写失败测试**

在 `service.rs` tests 里加：

```rust
/// 会话空闲**不再**等于「跑完了」：一张 running 卡在会话 idle 时状态必须原样不动。
/// 这是误报的核心回归锁（旧实现会在这里翻成 awaiting_merge/needs_you）。
#[test]
fn an_idle_session_never_changes_a_running_card() {
    let dir = tempfile::tempdir().unwrap();
    let project = project_with_worktree(dir.path());
    let service = service_with_card(&project);
    service.mark_running("card-1", "thread-1").unwrap();
    let threads = vec![facts("thread-1", true, true)];
    assert!(service.driver_step(&threads, &[]).unwrap().is_none());
    assert_eq!(state_of(&service, "card-1"), "Running");
}

/// 宿主的会话表里没有这张卡（重启后）也**不得**降级：卡停在 running，靠观测标 stale。
#[test]
fn a_session_the_host_forgot_never_changes_the_card() {
    let dir = tempfile::tempdir().unwrap();
    let project = project_with_worktree(dir.path());
    let service = service_with_card(&project);
    service.mark_running("card-1", "thread-1").unwrap();
    assert!(service.driver_step(&[], &[]).unwrap().is_none());
    assert_eq!(state_of(&service, "card-1"), "Running");
}
```

同文件 tests 顶部加辅助：

```rust
fn facts(thread_id: &str, idle: bool, alive: bool) -> crate::service::ThreadFacts {
    crate::service::ThreadFacts {
        thread_id: thread_id.to_string(),
        idle,
        alive,
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p superpowers-kanban-runner --lib an_idle_session_never`
Expected: 编译失败（`ThreadFacts` 不存在）

- [ ] **Step 3: 收缩 driver_step**

在 `service.rs` 里：

```rust
/// 宿主报的一条会话事实。`alive` 由宿主按「这一拍还看得见这个会话」给出，
/// 与 `idle` 是两件事：idle 只说明「此刻没有 turn 在跑」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadFacts {
    pub thread_id: String,
    pub idle: bool,
    pub alive: bool,
}
```

`driver_step` 函数体删掉步骤 1.5 / 2 / 3 / 4（`recover_cards_the_host_forgot`、合并轮收尾、
对账、合并轮开局），只留：

```rust
pub fn driver_step(
    &self,
    threads: &[ThreadFacts],
    results: &[DriverResult],
) -> Result<Option<DriverAction>, String> {
    if self.lock().leases_dir.is_none() {
        return Err("no lease directory".into());
    }
    self.consume_results(results);
    self.write_observations(threads);
    let cards = self.locked_cards();
    let limit = superpowers_kanban_core::calendar::ConcurrencyCalendar::load_preferring_new(
        &self.state_dir,
    )
    .limit_at(chrono::Local::now());
    match self.next_launch(limit, chrono::Local::now()) {
        Ok(Some(claim)) => { /* 原样，见既有代码 */ }
        Ok(None) => Ok(None),
        Err(error) => {
            eprintln!("superpowers-kanban: launch failed: {error}");
            Ok(None)
        }
    }
}
```

删除：`recover_cards_the_host_forgot`、`mark_needs_you`、`merge_retries` 字段、
`MAX_MERGE_RETRIES`、`has_commits`、`effective_head`、`idle_of`、`PendingKind::Append`、
`DriverAction::Append`、`apply_result` 里的 `Append` 分支。

- [ ] **Step 4: 删 runner.rs 的死代码**

删除 `state_for_task_state`、`plan_launches` 及其测试（daemon 不再从任务状态推导卡片状态）。

- [ ] **Step 5: 跑测试**

Run: `cargo test -p superpowers-kanban-runner --lib -- an_idle_session a_session_the_host_forgot`
Expected: PASS

- [ ] **Step 6: 清理被删测试**

跑 `cargo test -p superpowers-kanban-runner --lib`，把引用了已删行为的测试整条删掉（这些测试守护的是被明确移除的行为，保留它们只会保护错误）：
`driver_step_marks_a_running_card_awaiting_merge_when_its_session_goes_idle`、
`driver_step_marks_a_running_card_needs_you_when_its_session_ended_without_commits`、
`driver_step_revives_an_awaiting_merge_card_whose_session_is_busy_again`、
`driver_step_recovers_cards_whose_session_the_host_no_longer_reports`、
`driver_step_recovers_an_untracked_merging_card_instead_of_stranding_it`、
`driver_step_retries_a_failed_merge_round_but_only_a_bounded_number_of_times`、
`driver_step_settles_a_git_negative_merge_round_as_needs_you_and_retries_it`、
`driver_step_settles_a_merging_card_with_the_git_verified_merge_finish`、
`driver_step_does_not_append_twice_while_a_merge_is_running`、
`driver_step_does_not_settle_a_merge_turn_it_just_dispatched`、
`driver_step_emits_an_append_for_a_card_awaiting_merge_once_the_gate_is_held`、
`driver_step_never_drags_a_session_ended_needs_you_card_into_a_merge_round`、
`driver_step_skips_a_merge_candidate_whose_source_branch_is_missing`、
`a_merging_card_does_not_consume_a_session_slot`、
`a_merging_card_left_by_a_crash_becomes_needs_you_on_startup`、
`a_launching_card_left_by_a_crash_becomes_failed_on_startup`

- [ ] **Step 7: Commit**

```bash
cargo fmt --all && git add -A crates && git commit -m "refactor(kanban): daemon only launches; no idle or git inference"
```

---

### Task 4: 新增 `state` 动词（模型写状态的唯一入口）

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`（`set_state`）
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/dispatch.rs`（`card.state`）
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs`（CLI 解析与命令）

**Interfaces:**
- Produces:
  - `BoardService::set_state(&self, card_id: &str, state: CardState, detail: Option<&str>) -> Result<(), String>`
  - IPC `card.state`，参数 `{"card_id": String, "state": String, "detail": Option<String>}`
  - CLI `superpowers-kanban state <card-id> <state> [--detail <text>]` 与 `state --self <state> [--detail <text>]`

- [ ] **Step 1: 写失败测试**

`service.rs` tests：

```rust
#[test]
fn the_model_may_set_any_live_state_but_not_revive_a_terminal_one() {
    let dir = tempfile::tempdir().unwrap();
    let project = project_with_worktree(dir.path());
    let service = service_with_card(&project);
    service.set_state("card-1", CardState::AwaitingMerge, Some("plan done")).unwrap();
    assert_eq!(state_of(&service, "card-1"), "AwaitingMerge");
    assert_eq!(detail_of(&service, "card-1").as_deref(), Some("plan done"));
    service.set_state("card-1", CardState::Done, None).unwrap();
    let error = service.set_state("card-1", CardState::Running, None).unwrap_err();
    assert!(error.contains("terminal"), "{error}");
}

#[test]
fn setting_a_terminal_state_clears_the_stale_detail() {
    let dir = tempfile::tempdir().unwrap();
    let project = project_with_worktree(dir.path());
    let service = service_with_card(&project);
    service.set_state("card-1", CardState::Running, None).unwrap();
    service.set_state("card-1", CardState::Done, None).unwrap();
    assert_eq!(detail_of(&service, "card-1"), None);
}
```

`main.rs` tests：

```rust
#[test]
fn the_state_verb_parses_a_card_and_a_state() {
    let command = parse_subcommand(
        ["state", "card-1", "needs_you", "--detail", "blocked on credentials"]
            .map(str::to_string),
    )
    .unwrap();
    match command {
        Subcommand::State { card_id, state, detail, .. } => {
            assert_eq!(card_id.as_deref(), Some("card-1"));
            assert_eq!(state, "needs_you");
            assert_eq!(detail.as_deref(), Some("blocked on credentials"));
        }
        other => panic!("expected State, got {other:?}"),
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p superpowers-kanban-runner --lib the_state_verb_parses`
Expected: 编译失败（`Subcommand::State` 不存在）

- [ ] **Step 3: 实现 set_state**

```rust
/// 模型声明这张卡现在处于什么状态。这是除 launch 之外**唯一**写状态的入口。
///
/// 不做迁移校验（只有一条：终态不可复活，由 `CardState::can_transition_to` 兜住）。
/// 模型有上下文，整张迁移表只会在正当场景上挡住它。
pub fn set_state(
    &self,
    card_id: &str,
    state: CardState,
    detail: Option<&str>,
) -> Result<(), String> {
    let id = CardId::new(card_id);
    persist::with_board(&self.state_dir, |board| {
        let Some(card) = board.get(&id) else {
            return Err(format!("unknown card: {card_id}"));
        };
        if card.state.is_terminal() && card.state != state {
            return Err(format!(
                "card {card_id} is {:?}; a terminal state cannot be revived",
                card.state
            ));
        }
        board.transition(&id, state).map_err(|e| e.to_string())?;
        // 终态不该挂着一句为「还没做完」写的判词；其余状态用调用方给的判词
        // （`None` 表示无判词，与原语义一致）。
        let detail = if state.is_terminal() { None } else { detail.map(str::to_string) };
        board.set_detail(&id, detail).map_err(|e| e.to_string())
    })?;
    self.lock().leases.remove(&id);
    Ok(())
}
```

- [ ] **Step 4: 接 IPC 与 CLI**

`dispatch.rs` 加方法：

```rust
"card.state" => {
    let card_id = params.get("card_id").and_then(Value::as_str)
        .ok_or_else(|| "card.state needs a `card_id`".to_string())?;
    let state = params.get("state").and_then(Value::as_str)
        .ok_or_else(|| "card.state needs a `state`".to_string())?;
    let parsed = parse_state(state)?;   // 见下
    let detail = params.get("detail").and_then(Value::as_str);
    service.set_state(card_id, parsed, detail)?;
    Ok(json!({"ok": true}))
}
```

`parse_state`（放 dispatch.rs，大小写不敏感，接受下划线形式）：

```rust
fn parse_state(value: &str) -> Result<CardState, String> {
    match value.to_ascii_lowercase().as_str() {
        "pre_queued" => Ok(CardState::PreQueued),
        "queued" => Ok(CardState::Queued),
        "running" => Ok(CardState::Running),
        "needs_you" => Ok(CardState::NeedsYou),
        "awaiting_merge" => Ok(CardState::AwaitingMerge),
        "done" => Ok(CardState::Done),
        "failed" => Ok(CardState::Failed),
        "cancelled" => Ok(CardState::Cancelled),
        other => Err(format!(
            "unknown state: {other} (expected pre_queued|queued|running|needs_you|\
             awaiting_merge|done|failed|cancelled)"
        )),
    }
}
```

`main.rs`：`Subcommand::State { state_dir, card_id: Option<String>, state: String, detail: Option<String> }`；
解析 `--self` 时把 `card_id` 设为 `None`；`command_state` 里：

```rust
let card_id = match card_id {
    Some(id) => id,
    None => std::env::var("SUPERPOWERS_KANBAN_CARD").map_err(|_| {
        "state --self needs SUPERPOWERS_KANBAN_CARD: this shell is not a kanban card session"
            .to_string()
    })?,
};
superpowers_kanban_runner::dispatch::dispatch(
    &state_dir, "card.state",
    &serde_json::json!({"card_id": card_id, "state": state, "detail": detail}),
)?;
Ok(format!("{card_id} -> {state}"))
```

`--detail` 解析：`--detail <text>`，出现在 `--self` 前后都可。

- [ ] **Step 5: 更新 USAGE**

USAGE 里加一行：

```
  state         <card-id> <state> [--detail <t>] | --self <state> [--detail <t>] [--state-dir <d>]
```

- [ ] **Step 6: 跑测试**

Run: `cargo test -p superpowers-kanban-runner --lib -- the_state_verb_parses the_model_may_set_any`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
cargo fmt --all && git add -A crates && git commit -m "feat(kanban): add state verb so the model owns card states"
```

---

### Task 5: 合并名额改为 holder 记录 + 残留回收

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/merge_lock.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`（`merge_request_locked`、`merge_finish`）
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs`（`command_merge_finish` 文案）

**Interfaces:**
- Produces:
  - `merge_lock::acquire_with_holder(state_dir: &Path, card_id: &str) -> MergeAttempt`
  - `enum MergeAttempt { Granted(MergeLock), Busy { card_id: String, pid: u32 }, Reclaimed(MergeLock) }`
  - `MergeFinish` 只剩 `Released` / `Cleared`
- 删除：`MergeFinish::Done` / `MergeFinish::NeedsYou`、`merge_finish` 里的 git 复核、卡片上的 `Merging` 迁移。

- [ ] **Step 1: 写失败测试**

`merge_lock.rs` tests：

```rust
#[test]
fn a_dead_holder_is_reclaimed_instead_of_blocking_forever() {
    let dir = tempfile::tempdir().unwrap();
    // 伪造一个 pid 绝不存在的 holder（pid 0 不会被当作存活进程）。
    std::fs::write(
        dir.path().join("merge.lock.holder"),
        r#"{"card_id":"old","pid":0,"since":"2026-10-01T00:00:00+08:00"}"#,
    )
    .unwrap();
    match acquire_with_holder(dir.path(), "new") {
        MergeAttempt::Reclaimed(_) | MergeAttempt::Granted(_) => {}
        other => panic!("a dead holder must not block: {other:?}"),
    }
}

#[test]
fn a_live_holder_reports_busy_with_its_card() {
    let dir = tempfile::tempdir().unwrap();
    let held = match acquire_with_holder(dir.path(), "card-a") {
        MergeAttempt::Granted(lock) => lock,
        other => panic!("expected the first grant, got {other:?}"),
    };
    match acquire_with_holder(dir.path(), "card-b") {
        MergeAttempt::Busy { card_id, .. } => assert_eq!(card_id, "card-a"),
        other => panic!("expected busy, got {other:?}"),
    }
    drop(held);
}
```

`service.rs` tests：

```rust
#[test]
fn merge_finish_only_releases_the_gate_and_never_touches_git_or_state() {
    let dir = tempfile::tempdir().unwrap();
    let project = project_with_worktree(dir.path());
    let service = service_with_awaiting_card(&project);
    assert!(matches!(service.merge_request("card-1").unwrap(), MergeRequest::Granted { .. }));
    assert_eq!(service.merge_finish("card-1").unwrap(), MergeFinish::Released);
    // 状态一动不动：合没合上由模型自己判断并 `state ... done`。
    assert_eq!(state_of(&service, "card-1"), "AwaitingMerge");
    // 名额已放掉，第二个申请者能拿到。
    assert!(matches!(service.merge_request("card-1").unwrap(), MergeRequest::Granted { .. }));
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p superpowers-kanban-runner --lib a_dead_holder_is_reclaimed merge_finish_only_releases`
Expected: 编译失败（`acquire_with_holder` / `MergeFinish::Released` 不存在）

- [ ] **Step 3: 实现 holder 记录**

`merge_lock.rs`：

```rust
/// holder 记录与锁文件分开：`flock` 的 fd 不能跨进程读内容，故另落一个小文件。
fn holder_path(state_dir: &Path) -> PathBuf { state_dir.join("merge.lock.holder") }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder { pub card_id: String, pub pid: u32, pub since: String }

fn read_holder(state_dir: &Path) -> Option<Holder> {
    let text = std::fs::read_to_string(holder_path(state_dir)).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_holder(state_dir: &Path, holder: &Holder) -> std::io::Result<()> {
    let text = serde_json::to_string(holder).map_err(std::io::Error::other)?;
    let tmp = holder_path(state_dir).with_extension("holder.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, holder_path(state_dir))
}

/// `pid` 是否还活着。`ESRCH` 才算消失；`EPERM` 视为存在（别人的进程也算活着）。
fn pid_alive(pid: u32) -> bool {
    if pid == 0 { return false; }
    let outcome = unsafe { libc::kill(pid as i32, 0) };
    if outcome == 0 { return true; }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[derive(Debug)]
pub enum MergeAttempt {
    Granted(MergeLock),
    Busy { card_id: String, pid: u32 },
    Reclaimed(MergeLock),
}

pub fn acquire_with_holder(state_dir: &Path, card_id: &str) -> MergeAttempt {
    let previous = read_holder(state_dir);
    let lock = match acquire(state_dir) {
        Some(lock) => lock,
        None => {
            let holder = previous.unwrap_or(Holder {
                card_id: "unknown".into(), pid: 0, since: String::new(),
            });
            return MergeAttempt::Busy { card_id: holder.card_id, pid: holder.pid };
        }
    };
    let reclaimed = match &previous {
        Some(holder) if !pid_alive(holder.pid) => true,
        _ => false,
    };
    let _ = write_holder(state_dir, &Holder {
        card_id: card_id.to_string(),
        pid: std::process::id(),
        since: chrono::Local::now().to_rfc3339(),
    });
    if reclaimed { MergeAttempt::Reclaimed(lock) } else { MergeAttempt::Granted(lock) }
}
```

注意：锁的保持由 `MergeLock` 的 fd 决定，但合并跨会话数分钟，进程内若把 `MergeLock`
随函数返回就 drop 了。因此 `service` 必须把拿到的 `MergeLock` **存进 `Inner`**，直到
`merge_finish` 释放：

```rust
// Inner 新增
merge_gate: Option<crate::merge_lock::MergeLock>,
```

`merge_request_locked` 末尾：

```rust
let gate = match crate::merge_lock::acquire_with_holder(&self.state_dir, card_id) {
    MergeAttempt::Granted(lock) | MergeAttempt::Reclaimed(lock) => lock,
    MergeAttempt::Busy { card_id, .. } => return Ok(MergeRequest::Busy(card_id)),
};
self.lock().merge_gate = Some(gate);
```

`MergeRequest::Busy` 改为带 `card_id: String`（CLI 文案要说出「谁在合」）。

- [ ] **Step 4: 改写 merge_finish**

```rust
/// 释放合并名额。**不碰 git、不改卡片状态**——合并成没成由模型自己判断并
/// `state <card> done`（或 `needs_you`）。这里只回答「名额还在不在我手上」。
pub fn merge_finish(&self, card_id: &str) -> Result<MergeFinish, String> {
    let mut inner = self.lock();
    let Some(gate) = inner.merge_gate.take() else {
        return Ok(MergeFinish::Cleared);
    };
    // 记录属实时才清 holder 文件；不是自己拿的就不动别人的账。
    if crate::merge_lock::read_holder(&self.state_dir)
        .is_some_and(|holder| holder.card_id == card_id)
    {
        crate::merge_lock::clear_holder(&self.state_dir);
    }
    drop(gate);
    Ok(MergeFinish::Released)
}
```

`clear_holder` 用 `std::fs::remove_file`（缺文件算成功）。同时删掉 `merge_finish` 里所有
git 调用、`merge_retries` 相关代码。

- [ ] **Step 5: 改 dispatch.rs 的两种应答**

`merge_request` 的 `Busy` 要带上 holder 的卡号：

```rust
crate::service::MergeRequest::Busy { card_id } => {
    Ok(json!({ "status": "busy", "card_id": card_id }))
}
```

`merge_finish` 的三种结局收缩为两种：

```rust
let status = match service.merge_finish(card_id)? {
    crate::service::MergeFinish::Released => "released",
    crate::service::MergeFinish::Cleared => "cleared",
};
```

- [ ] **Step 6: 改 main.rs 的 merge-finish 文案**

```rust
Some("released") => Ok(format!(
    "released\nmerge gate is free again. If the merge actually landed, \
     run `superpowers-kanban state {card_id} done`"
)),
```

`merge-request` 的 `busy` 文案带上 holder：

```rust
Some("busy") => Ok(format!(
    "busy: {}\nwait for it to finish",
    value.get("card_id").and_then(serde_json::Value::as_str)
        .map(|id| format!("card {id} is merging"))
        .unwrap_or_else(|| "another merge is running in this project".to_string())
)),
```

- [ ] **Step 7: 跑测试**

Run: `cargo test -p superpowers-kanban-runner --lib -- merge_lock merge_request merge_finish a_dead_holder`
Expected: PASS

- [ ] **Step 8: Commit**

```bash
cargo fmt --all && git add -A crates && git commit -m "feat(kanban): merge gate keyed by holder record, no git verification"
```

---

### Task 6: 观测旁路 `observations.json` 与并发按活会话计

**Files:**
- Create: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/observation.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/lib.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`（`write_observations`、`adopt_running_leases` 重写、`next_launch` 名额判据）
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/lease.rs`（新增 `acquire_for`，按 thread 而非卡占槽）

**Interfaces:**
- Produces:
  - `observation::Observation { thread_id: String, alive: bool, last_seen_alive_at: Option<DateTime<Local>> }`
  - `observation::ObservationFile { threads: BTreeMap<String, Observation> }`
  - `observation::load(state_dir: &Path) -> ObservationFile`
  - `observation::save(state_dir: &Path, file: &ObservationFile) -> io::Result<()>`
  - `observation::stale(card: &Card, file: &ObservationFile) -> Option<&Observation>`（`None` = 不 stale）
  - `BoardService::write_observations(&self, threads: &[ThreadFacts])`
  - `lease::acquire_for(dir: &Path, limit: usize, key: &str) -> Option<Lease>`（同 key 幂等复用）

- [ ] **Step 1: 写失败测试**

`observation.rs` tests：

```rust
#[test]
fn a_thread_seen_alive_gets_its_timestamp_refreshed() {
    let dir = tempfile::tempdir().unwrap();
    let mut file = ObservationFile::default();
    file.observe("thread-1", true, at(1, 0));
    assert_eq!(file.threads["thread-1"].last_seen_alive_at, Some(at(1, 0)));
    file.observe("thread-1", false, at(1, 1));
    assert!(!file.threads["thread-1"].alive);
    assert_eq!(
        file.threads["thread-1"].last_seen_alive_at,
        Some(at(1, 0)),
        "a dead sighting must not overwrite the last time it was alive"
    );
}

#[test]
fn a_running_card_whose_session_is_dead_is_stale() {
    let mut board = Board::new();
    board.enqueue(CardId::new("a"), "a.spec.md".into(), "a.plan.md".into(), at(1, 0));
    board.transition(&CardId::new("a"), CardState::Running).unwrap();
    board.set_thread_id(&CardId::new("a"), "thread-1".into()).unwrap();
    let mut file = ObservationFile::default();
    file.observe("thread-1", false, at(1, 1));
    let card = board.get(&CardId::new("a")).unwrap();
    assert!(observation::stale(card, &file).is_some());
    // 非 running 卡永不 stale：done 的卡没有会话是正常的。
    board.transition(&CardId::new("a"), CardState::Done).unwrap();
    let card = board.get(&CardId::new("a")).unwrap();
    assert!(observation::stale(card, &file).is_none());
}
```

`service.rs` tests：

```rust
/// 并发按**活会话**计：一张卡标了 done 但会话还活着，仍占名额。
#[test]
fn a_live_session_keeps_its_slot_even_after_the_card_is_done() {
    let dir = tempfile::tempdir().unwrap();
    let project = project_with_worktree(dir.path());
    let service = service_with_cards(&project, &["card-1", "card-2"]);
    let launch = service.driver_step(&[], &[]).unwrap().expect("first launch");
    let thread = match &launch {
        DriverAction::Launch { .. } => "thread-1",
        _ => unreachable!(),
    };
    service.mark_running("card-1", thread).unwrap();
    // 模型提前标 done，但会话仍活着。
    service.set_state("card-1", CardState::Done, None).unwrap();
    let threads = vec![facts(thread, false, true)];
    assert!(
        service.driver_step(&threads, &[]).unwrap().is_none(),
        "a live session must still hold the only slot"
    );
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p superpowers-kanban-runner --lib -- observation a_live_session_keeps`
Expected: 编译失败（模块与函数不存在）

- [ ] **Step 3: 实现 observation.rs**

```rust
//! 会话存活观测：只读旁路，**永不**写回卡片状态。
//!
//! 为什么另立一个文件而不是往 `board.json` 加字段：`board.json` 是控制面渲染的
//! 事实源，而「会话此刻还在不在」是宿主内存里的一时之见。把它写进卡片就等于让
//! 观测反向污染状态——这正是本次改造要根除的误报来源。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use superpowers_kanban_core::card::{Card, CardState};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub alive: bool,
    #[serde(default)]
    pub last_seen_alive_at: Option<DateTime<Local>>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ObservationFile {
    #[serde(default)]
    pub threads: BTreeMap<String, Observation>,
}

impl ObservationFile {
    /// 记一次观测。只有「看到活着」才刷新时间戳：会话没了之后，人要看的是
    /// 「它最后一次还活着是什么时候」。
    pub fn observe(&mut self, thread_id: &str, alive: bool, now: DateTime<Local>) {
        let entry = self.threads.entry(thread_id.to_string()).or_insert(Observation {
            alive,
            last_seen_alive_at: None,
        });
        entry.alive = alive;
        if alive {
            entry.last_seen_alive_at = Some(now);
        }
    }
}

pub fn path(state_dir: &Path) -> PathBuf { state_dir.join("observations.json") }

pub fn load(state_dir: &Path) -> ObservationFile {
    std::fs::read_to_string(path(state_dir))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save(state_dir: &Path, file: &ObservationFile) -> std::io::Result<()> {
    std::fs::create_dir_all(state_dir)?;
    let text = serde_json::to_string_pretty(file).map_err(std::io::Error::other)?;
    let tmp = path(state_dir).with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, path(state_dir))
}

/// 一张卡是否「写着 running 但会话已经没了」。只有 `running` 卡会 stale：
/// 其余状态没有活会话是正常的，标 stale 只会制造噪声。
pub fn stale<'a>(card: &Card, file: &'a ObservationFile) -> Option<&'a Observation> {
    if card.state != CardState::Running {
        return None;
    }
    let thread_id = card.thread_id.as_deref().filter(|id| !id.is_empty())?;
    let observation = file.threads.get(thread_id)?;
    (!observation.alive).then_some(observation)
}
```

- [ ] **Step 4: 服务侧写入与并发口径**

`service.rs`：

```rust
/// 把本拍的会话事实写进观测旁路。**只写 observations.json**，绝不改 board.json。
fn write_observations(&self, threads: &[ThreadFacts]) {
    let now = chrono::Local::now();
    let mut file = crate::observation::load(&self.state_dir);
    let seen: std::collections::HashSet<&str> =
        threads.iter().map(|t| t.thread_id.as_str()).collect();
    for thread in threads {
        file.observe(&thread.thread_id, thread.alive, now);
    }
    // 这一拍完全没被提到的会话：默认按「已经没了」记，但**保留**最后一次活着的
    // 时间戳（`observe(false)` 正是这个语义）。
    let missing: Vec<String> = file
        .threads
        .keys()
        .filter(|id| !seen.contains(id.as_str()))
        .cloned()
        .collect();
    for id in missing {
        file.observe(&id, false, now);
    }
    if let Err(error) = crate::observation::save(&self.state_dir, &file) {
        eprintln!("superpowers-kanban: could not write observations: {error}");
    }
}
```

`next_launch` 的名额判据改为「按本进程持有的活会话租约数」：

```rust
pub fn next_launch(&self, limit: u16, now: DateTime<Local>) -> Result<Option<LaunchClaim>, String> {
    let mut inner = self.lock();
    let Some(dir) = inner.leases_dir.clone() else {
        return Err("no lease directory".into());
    };
    // 先补账：已在跑且会话仍活着的卡必须占着租约，否则重启后名额被别的项目抢走。
    self.adopt_running_leases_locked(&mut inner, limit);
    let Some(lease) = lease::acquire_in(&dir, limit as usize) else {
        return Ok(None);
    };
    // …（其余与原实现一致：with_board 认领 → 建 worktree → 置 Running）
}
```

`adopt_running_leases` 改名 `adopt_running_leases_locked`，判据从「`state == Running`」改为
「`state == Running` 且 `observations.json` 里该 thread `alive`」。

- [ ] **Step 5: 跑测试**

Run: `cargo test -p superpowers-kanban-runner --lib -- observation a_live_session_keeps`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
cargo fmt --all && git add -A crates && git commit -m "feat(kanban): observation sidecar and liveness-based concurrency"
```

---

### Task 7: 宿主上报 `alive`（契约同步）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/session_driver.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:5676-5690`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/contract/driver-protocol.json`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/dispatch.rs`（解析 `alive`）
- Test: 两侧契约测试

**Interfaces:**
- `DriverThread { thread_id: String, alive: bool }`；`threads_payload` 带 `"alive"`。
- `driver/next` 的 `threads[]` 条目：`{"thread_id": String, "idle": bool, "alive": bool}`。
- `alive` 缺省 `true`（前向兼容：旧宿主不发时，插件不会把所有卡判 stale）。

- [ ] **Step 1: 写失败测试**

`session_driver.rs` tests：

```rust
#[test]
fn the_payload_reports_which_sessions_are_still_alive() {
    let threads = vec![
        DriverThread { thread_id: "t1".into(), alive: true },
        DriverThread { thread_id: "t2".into(), alive: false },
    ];
    let payload = threads_payload(&threads, &|_| true);
    assert_eq!(payload[0]["alive"], true);
    assert_eq!(payload[1]["alive"], false);
    assert_eq!(payload[0]["idle"], true, "idle 语义不变");
}
```

`service.rs` 契约测试（同 `DRIVER_CONTRACT` 那组）：

```rust
#[test]
fn the_driver_contract_threads_carry_liveness() {
    let contract: Value = serde_json::from_str(DRIVER_CONTRACT).unwrap();
    let threads = contract["request"]["threads"].as_array().unwrap();
    assert!(threads.iter().all(|t| t.get("alive").is_some()), "{threads:?}");
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib the_payload_reports_which`
Expected: 编译失败（`alive` 字段不存在）

- [ ] **Step 3: 改宿主**

`session_driver.rs`：

```rust
pub(crate) struct DriverThread {
    pub thread_id: String,
    /// 宿主这一拍还看得见这个会话吗。与 `idle` 无关：idle 只说明此刻没有 turn。
    pub alive: bool,
}
```

`threads_payload` 多一个键：

```rust
serde_json::json!({
    "thread_id": thread.thread_id,
    "idle": idle(&thread.thread_id),
    "alive": thread.alive,
})
```

`server.rs` 快照处（约 5676-5690）在遍历 `threads` 时把 `alive` 一并算进闭包：

```rust
let snapshot: HashMap<String, (bool, bool)> = threads
    .iter()
    .map(|(thread_id, session)| {
        let idle = matches!(
            *session.status.lock().unwrap_or_else(|p| p.into_inner()),
            ThreadStatus::Idle
        ) && session.active_turn_id.is_none();
        (thread_id.clone(), (idle, true))
    })
    .collect();
let facts = move |thread_id: &str| {
    snapshot.get(thread_id).copied().unwrap_or((false, false))
};
```

`driver_tick` 里对应把 `idle` 与 `alive` 分别传给 `driver_step`；`threads: &mut Vec<DriverThread>`
在 `Launch` 成功时 push `DriverThread { thread_id, alive: true }`。

- [ ] **Step 4: 更新契约文件**

`driver-protocol.json` 的 `request.threads` 条目加 `"alive": true`，并把 `_comment` 补一句：
`alive` 只表示宿主这一拍还看得见该会话。

- [ ] **Step 5: 改插件侧解析**

`dispatch.rs` 的 `driver/next` 分支：

```rust
let alive = thread.get("alive").and_then(Value::as_bool).unwrap_or(true);
snapshot.push(crate::service::ThreadFacts {
    thread_id: thread_id.to_string(),
    idle,
    alive,
});
```

`forbidden` 语义注释要改：旧实现「缺 idle 当作不闲」，新实现「缺 alive 当作还活着」——
后者保守（宁可不判 stale，也不误报废一张卡）。

- [ ] **Step 6: 跑两侧测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib -- session_driver`
Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --lib -- the_driver_contract`
Expected: PASS

- [ ] **Step 7: Commit（两个仓库各一次）**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add yi-agent-rs/crates/yi-agent-app-server plugins/superpowers-kanban && git commit -m "feat(driver): report session liveness alongside idleness"
```

- [ ] **Step 8: 同步已安装的插件二进制**

```bash
cd plugins/superpowers-kanban && cargo build --release
pkill -f "superpowers-kanban run"   # 监督器会拉起新进程（旧进程持有旧 inode）
```

---

### Task 8: `list` 与桌面显示 stale

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`（`list`）
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs`（`command_list`）
- Modify: `desktop/src/lib/superpowersKanbanState.ts`
- Modify: `desktop/src/lib/superpowersKanbanBoard.ts`
- Modify: `desktop/src/components/SuperpowersKanbanCard.tsx`

**Interfaces:**
- `list` 每张卡多两个键：`"stale": bool`、`"last_alive_at": Option<String>`。
- `BoardCard` 增 `stale: boolean`、`lastAliveAt?: string`。
- `columnForState` 删除 `launching` / `paused` / `merging` 分支。

- [ ] **Step 1: 写失败测试**

`service.rs` tests：

```rust
#[test]
fn list_marks_a_running_card_stale_when_its_session_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let project = project_with_worktree(dir.path());
    let service = service_with_card(&project);
    service.mark_running("card-1", "thread-1").unwrap();
    let threads = vec![facts("thread-1", false, true)];
    service.driver_step(&threads, &[]).unwrap();
    assert_eq!(service.list()["cards"][0]["stale"], false);
    let threads = vec![facts("thread-1", false, false)];
    service.driver_step(&threads, &[]).unwrap();
    let card = &service.list()["cards"][0];
    assert_eq!(card["stale"], true);
    assert!(card["last_alive_at"].is_string(), "要带上最后活着的时刻: {card}");
}
```

`desktop/src/lib/superpowersKanbanState.test.ts`：

```ts
it("carries the stale flag and the last live timestamp", () => {
  const [card] = parseBoard(JSON.stringify({ cards: [{
    id: "a", state: "running", spec_path: "a.spec.md",
    stale: true, last_alive_at: "2026-10-22T10:00:00+08:00",
  }]}));
  expect(card.stale).toBe(true);
  expect(card.lastAliveAt).toBe("2026-10-22T10:00:00+08:00");
});
```

`desktop/src/components/SuperpowersKanbanCard.test.tsx`：

```tsx
it("shows a stale badge with the last live time", () => {
  render(<SuperpowersKanbanCard card={{
    id: "a", state: "running", progress: null, detail: "",
    threadId: "t1", originThreadId: null, stale: true,
    lastAliveAt: "2026-10-22T10:00:00+08:00",
  }} />);
  expect(screen.getByLabelText("会话已不在")).toBeInTheDocument();
  expect(screen.getByText(/10:00/)).toBeInTheDocument();
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p superpowers-kanban-runner --lib list_marks_a_running_card_stale`
Run: `cd desktop && npx vitest run src/lib/superpowersKanbanState.test.ts`
Expected: 两侧都失败

- [ ] **Step 3: 插件侧**

`service.rs::list`：

```rust
let observations = crate::observation::load(&self.state_dir);
// … 每张卡的 json! 里追加：
"stale": crate::observation::stale(card, &observations).is_some(),
"last_alive_at": card
    .thread_id
    .as_deref()
    .and_then(|thread| observations.threads.get(thread))
    .and_then(|obs| obs.last_seen_alive_at)
    .map(|at| at.to_rfc3339()),
```

`main.rs::command_list` 非 queued 行的行尾追加 `" [stale]"`（当 `card` 对应 JSON 的 stale 为真）。
实现方式：把 `command_list` 改成先调 `service.list()`（与桌面同口径），而不是自己读 board.json，
这样 stale 只算一处。

- [ ] **Step 4: 桌面侧**

`superpowersKanbanState.ts`：`BoardCard` 加两个字段，`normalizeCard` 里：

```ts
stale: record.stale === true,
lastAliveAt: optionalString(record.last_alive_at),
```

`superpowersKanbanBoard.ts::columnForState`：删掉 `case "launching":`、`case "paused":`、
`case "merging":` 三个分支（枚举已不存在，留着会让一条永不到达的分支骗过读者）。

`SuperpowersKanbanCard.tsx`：

```tsx
{card.stale ? (
  <span aria-label="会话已不在" className="shrink-0 text-amber-400" title="卡片写着 running，但会话已经不在了">
    ⚠
  </span>
) : null}
...
{card.stale && card.lastAliveAt ? (
  <div className="mt-1 text-xs text-fg-subtle">
    最后活动 {new Date(card.lastAliveAt).toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit" })}
  </div>
) : null}
```

- [ ] **Step 5: 跑测试**

Run: `cargo test -p superpowers-kanban-runner --lib list_marks_a_running_card_stale`
Run: `cd desktop && npx vitest run src/lib/superpowersKanbanState.test.ts src/components/SuperpowersKanbanCard.test.tsx && npx tsc --noEmit`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
cargo fmt --all && cd ../.. && git add -A plugins/superpowers-kanban desktop && git commit -m "feat(kanban): surface stale cards with last live time"
```

---

### Task 9: 让模型知道自己的职责（objective + 环境变量 + skill）

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/tick.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（装配期注入 `SUPERPOWERS_KANBAN_CARD`）
- Modify: `plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md`
- Copy: `skills/superpowers-kanban/SKILL.md` → `~/.yi-agent/skills/superpowers-kanban/SKILL.md`

**Interfaces:**
- Consumes: Task 4 的 `state` 动词与 `--self`
- objective 末尾追加固定句子（见 Step 3），含 `state` 动词名

- [ ] **Step 1: 写失败测试**

`tick.rs` tests：

```rust
#[test]
fn the_objective_tells_the_session_it_owns_the_card_state() {
    let board = board_with(&["a"]);
    let card = board.get(&CardId::new("a")).unwrap().clone();
    let objective = engagement_objective(&card);
    assert!(objective.contains("superpowers-kanban state"), "{objective}");
    assert!(objective.contains("--self"), "{objective}");
    assert!(objective.contains("SUPERPOWERS_KANBAN_CARD"), "{objective}");
    // 旧的硬约束必须还在。
    assert!(objective.contains("Never merge yourself"), "{objective}");
}
```

`main.rs`（插件）的 skill 契约测试会自动覆盖新动词（`the_skill_documents_every_cli_verb`
从 USAGE 解析），因此只需 SKILL.md 里补上 `superpowers-kanban state`。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p superpowers-kanban-runner --lib the_objective_tells_the_session`
Expected: FAIL（objective 里没有 `state`）

- [ ] **Step 3: 改 objective**

`tick.rs::engagement_objective_for_paths`：

```rust
pub fn engagement_objective_for_paths(plan_path: &str, spec_path: &str) -> String {
    format!(
        "Implement the plan at {plan_path} following its spec at {spec_path}. \
         Work only in this worktree. Use superpowers:subagent-driven-development \
         (or superpowers:executing-plans) to execute it, then \
         superpowers:finishing-a-development-branch to present the integration \
         options to the user. Never merge yourself. If you are blocked, report BLOCKED. \
         \n\nYou own this card's kanban state: before you end a turn, when you need a \
         decision from the user, and when the work is finished, run \
         `superpowers-kanban state --self <state>` (the plugin injects \
         SUPERPOWERS_KANBAN_CARD into your shell). The board never guesses your state."
    )
}
```

同时更新 `the_engagement_objective_is_verbatim_the_old_wording` 这条「逐字契约」测试的
期望字符串（文案变了，测试必须跟着变——它是契约，不是装饰）。

- [ ] **Step 4: 注入环境变量**

`server.rs::launch_inner` 里 `start_thread_core` 之前，把卡片句柄与 thread id 一起交给装配：

```rust
// 驱动会话的 bash 子进程要知道自己跑的是哪张卡（`state --self` 用它）。
// `driver_ref` 就是插件给的卡片 id；普通会话为 None，不注入。
let card_env: Vec<(String, String)> = driver_ref
    .map(|card| vec![("SUPERPOWERS_KANBAN_CARD".to_string(), card.to_string())])
    .unwrap_or_default();
```

把 `card_env` 与既有 `YI_AGENT_THREAD_ID` 的注入点合并（同一处 `build_runtime_tooling`
的 `session_env` 参数），并加测试：

```rust
#[tokio::test]
async fn a_driven_session_gets_its_card_id_in_the_shell_env() {
    // 起一个 launch 动作（driver_ref = "card-1"），在会话里跑 `printenv SUPERPOWERS_KANBAN_CARD`，
    // 断言输出 "card-1"。普通会话跑同一条命令必须为空。
}
```

- [ ] **Step 5: 写 SKILL.md 的新一节**

在「怎么做」之后插入：

```markdown
### 维护卡片状态（模型负责）

看板**不会**替你判断状态。结束回合前、需要用户决策时、完成时，你必须自己声明：

```bash
superpowers-kanban state --self <state> [--detail "<原因>"]
```

| 情境 | `<state>` |
|---|---|
| 计划跑完、有产出等验收 | `awaiting_merge` |
| 需要用户决策才能继续 | `needs_you` |
| 已合并、用户确认完成 | `done` |
| 环境阻塞（缺依赖、无权限） | `failed` + `--detail` |
| 用户明确放弃 | `cancelled` |

规则：

- **不要**为了「让看板好看」提前写 `done`。卡片状态是给用户的承诺。
- 一轮里问了用户一句话就写 `needs_you`：会话空闲不等于活儿干完了，但「我在等人」
  正是 `needs_you` 的语义。
- 会话被杀了、你没能写状态时卡会停在 `running`，看板显示 `stale` + 最后活动时间。
  这不是插件的错，是这里没写。
- 终态不可复活。要重做，让用户新建一张卡。
```

同时把正文里所有 `launching` / `merging` / `paused` 的说明删掉，并更新 §2b 的合并流程：
`merge-finish` 现在只释放名额，**不再**自动判 done，需要自己 `state <card> done`。

- [ ] **Step 6: 跑测试**

Run: `cargo test -p superpowers-kanban-runner --lib -- tick:: the_skill`
Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib -- a_driven_session_gets_its_card`
Expected: PASS

- [ ] **Step 7: 同步 skill 副本（新会话才生效）**

```bash
cp plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md \
   ~/.yi-agent/skills/superpowers-kanban/SKILL.md
diff plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md \
     ~/.yi-agent/skills/superpowers-kanban/SKILL.md && echo in-sync
```

- [ ] **Step 8: Commit**

```bash
cargo fmt --all && git add -A plugins/superpowers-kanban yi-agent-rs/crates/yi-agent-app-server && \
  git commit -m "feat(kanban): tell the session it owns its card state"
```

---

### Task 10: 端到端验收与文档登记

**Files:**
- Modify: `docs/project-management/superpowers-kanban.md`
- Modify: `docs/project-management/yi-agent-app-server.md`
- Test: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`（集成级测试）

- [ ] **Step 1: 写端到端测试**

在 `service.rs` tests 里加一条串起全流程的：

```rust
/// 一张卡从排队到 done 的**全新**通路：daemon 只负责启动，状态全部由「模型」
/// （这里由测试代劳调 `set_state`）声明。中间夹一次「会话 idle 但没写状态」，
/// 断言看板**不**自作主张。
#[test]
fn a_card_moves_only_where_the_model_puts_it() {
    let dir = tempfile::tempdir().unwrap();
    let project = project_with_worktree(dir.path());
    let service = service_with_card(&project);

    let launch = service.driver_step(&[], &[]).unwrap().expect("launch");
    let DriverAction::Launch { card_id, .. } = launch else { panic!("expected launch") };
    service.mark_running(&card_id, "thread-1").unwrap();

    // 会话空闲、模型什么都没写 → 状态不动，只标 stale=false（会话还活着）。
    let threads = vec![facts("thread-1", true, true)];
    assert!(service.driver_step(&threads, &[]).unwrap().is_none());
    assert_eq!(state_of(&service, &card_id), "Running");

    // 模型问用户问题 → needs_you。
    service.set_state(&card_id, CardState::NeedsYou, Some("which DB?")).unwrap();
    assert_eq!(state_of(&service, &card_id), "NeedsYou");

    // 用户回话、模型继续干；干完 → awaiting_merge；合并后 → done。
    service.set_state(&card_id, CardState::Running, None).unwrap();
    service.set_state(&card_id, CardState::AwaitingMerge, None).unwrap();
    assert!(matches!(service.merge_request(&card_id).unwrap(), MergeRequest::Granted { .. }));
    assert_eq!(service.merge_finish(&card_id).unwrap(), MergeFinish::Released);
    service.set_state(&card_id, CardState::Done, None).unwrap();
    assert_eq!(state_of(&service, &card_id), "Done");
}
```

- [ ] **Step 2: 跑全量插件测试**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core && cargo test -p superpowers-kanban-runner --lib`
Expected: PASS

- [ ] **Step 3: 跑宿主侧相关测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib -- session_driver driver_tick`
Expected: PASS

- [ ] **Step 4: 跑桌面测试**

Run: `cd desktop && npx vitest run src/lib/superpowersKanban* src/components/SuperpowersKanban* && npx tsc --noEmit`
Expected: PASS

- [ ] **Step 5: 登记文档**

`docs/project-management/superpowers-kanban.md` 的 Features 里加：

```markdown
- [x] 状态流转改由模型驱动 — daemon 只写「排队 / 启动成功 / 启动失败」，其余状态经
  `superpowers-kanban state` 由会话自己声明；daemon 不再读 git、不再用 idle 推断、
  不再自动合并。并发按宿主上报的活会话计。判据：`cargo test -p superpowers-kanban-runner --lib`
  的 `a_card_moves_only_where_the_model_puts_it`、`an_idle_session_never_changes_a_running_card`、
  `a_live_session_keeps_its_slot_even_after_the_card_is_done`
- [x] 不再自动收敛的分支 — `core/converge.rs`、`runner/reconcile.rs` 已删除；一张
  `awaiting_merge` 卡分支即使已并入 base 也停着等人/模型。判据：`a_merged_branch_never_flips_a_card_to_done_by_itself`
- [x] 观测旁路 `observations.json` — 只读，`list`/桌面据此标 `stale` + 最后活着时刻，
  永不写回卡片状态。判据：`list_marks_a_running_card_stale_when_its_session_is_gone`
```

`docs/project-management/yi-agent-app-server.md` 里加一行（`driver/next` 上报 `alive`）。

- [ ] **Step 6: Commit**

```bash
cargo fmt --all && git add -A docs plugins/superpowers-kanban && \
  git commit -m "docs: record model-driven kanban state transitions"
```

---

## 自检

**Spec 覆盖**

| Spec 章节 | 任务 |
|---|---|
| §2.1 状态集 8 个 | Task 1 |
| §2.2 迁移表 (b) | Task 1、4 |
| §2.3 并发 = 活会话 | Task 6、7 |
| §2.4 合并名额项目级锁 | Task 5 |
| §2.5 daemon 不碰 git | Task 2、3 |
| §2.6 objective + 环境变量 | Task 9 |
| §2.7 观测与 stale | Task 6、8 |
| §3.1 `state` 动词 | Task 4 |
| §3.3 要删的代码面 | Task 1、2、3、5 |
| §3.4 SKILL.md | Task 9 |
| §4 验收 | Task 10 |

**占位符扫描**：无 TBD / TODO；每步含可执行命令或完整代码。

**类型一致性**：`ThreadFacts`（Task 3 定义、Task 6/7 消费）、`ObservationFile`（Task 6 定义、
Task 8 消费）、`MergeFinish::Released`（Task 5 定义、Task 10 消费）三处命名已对齐。
