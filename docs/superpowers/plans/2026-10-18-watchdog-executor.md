# 看门狗执行者 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `evaluate_watchdogs` 真正被 daemon 每分钟调用一次，使越限的 attempt 被终止并释放资源租约。

**Architecture:** 四步，全部走 TDD（先写会失败的测试）。先补 `evaluate_watchdogs` 的**逐条失败隔离**（否则一个未 hydrate 的会话会毒死整轮扫描），再把它接进 daemon 的分钟 tick，最后用**驱动路径测试**证明接线生效。测试落点选 `ipc.rs` 的 crate 内单元测试，因为要触私有项 `Daemon::coordinator` 与 `run_minute_maintenance`。

**Tech Stack:** Rust、rusqlite、tokio（current-thread runtime）、既有 `Daemon::start_with_factory` 脚手架。

**Spec:** `docs/superpowers/specs/2026-10-18-watchdog-executor-design.md`

## Global Constraints

- 环境：`export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`；`export TMPDIR=/Users/gongyichen/.yi-agent-tmp`；cargo 一律加 `--offline`。
- **只按 crate 跑测试**：`cargo test -p yi-agent-store`。**禁止** `--workspace`（按 CLAUDE.md，会 OOM/exit 137）。跑之前先 `ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent"` 确认没有残留进程；若有，先 kill。
- 提交前在 `yi-agent-rs/` 下跑 `cargo fmt --all`。
- 只显式 `git add <path>`，不要 `git add -A`。
- 本卡**不碰**看门狗的判定语义（`evaluate_watchdog` / `WatchdogLimits` 的默认值）与其幂等事务。
- 本卡**不碰** `docs/bug-list.md` / `docs/relay-deploy.md` / `justfile`（那是 `feat/secret-hygiene` 的范围）。

## 前置检查（动手前做一次）

- [ ] 确认 tick 循环的现状没被在途分支重排。Run:
      `grep -n "last_schedule_minute" yi-agent-rs/crates/yi-agent-store/src/ipc.rs`
      Expected: 命中 `904`/`943`/`963` 附近（若 `2026-10-18-stranded-daemon-self-stop` 已落地并重排了循环，按实际行号对齐插入点，语义不变）。
- [ ] 确认 `evaluate_watchdogs` 仍无生产调用者。Run:
      `grep -rn "evaluate_watchdogs" --include=*.rs yi-agent-rs/crates/*/src/`
      Expected: 只命中 `runtime.rs` 的定义行（若已有人在别处接线，先停下来对齐，别重复接）。

---

### Task 1: 让 `evaluate_watchdogs` 逐条隔离失败（TDD）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs:2250-2292`（`evaluate_watchdogs`）
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`（新增一个测试）

**Interfaces:**
- Consumes: `RuntimeRepository::active_attempt_watchdogs()`（`repository.rs:1315`）、`record_watchdog_terminal`（`runtime.rs:2187`）。
- Produces: `evaluate_watchdogs` 的新契约——**单条快照失败不中止整轮**；只有"取快照列表"失败才返回 `Err`。返回值仍是"本轮提交终态的条数"。

- [ ] **Step 1: 写会失败的测试**

在 `runtime_coordinator.rs` 末尾追加（复用文件里既有的 `TempDir` / `RuntimeRepository::open` / `create_task_with_attempt` / `save_attempt_watchdog_snapshot` 用法，见该文件 `2647` 起的既有测试）：

```rust
/// 一个未 hydrate 的会话（其 supervisor 不在 coordinator 的 map 里）不应
/// 阻断整轮判定。这回归的是「库全局读 × 进程内部分视图」的结构性错配：
/// `active_attempt_watchdogs()` 返回库里的全部快照，而
/// `record_watchdog_terminal` 只认本进程 hydrate 过的会话，未 hydrate 的
/// 返回 `Err(SessionNotFound)`——它的 `?` 曾让 `evaluate_watchdogs` 整轮返回 Err。
#[tokio::test]
async fn an_unhydrated_session_does_not_fail_the_whole_watchdog_sweep() {
    use yi_agent_store::schedule::{WatchdogLimits, WatchdogUsage};

    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");

    // 造 2 条越限快照，分属 2 个会话，且都未 hydrate（与真实库里那 50 个
    // 越限 queued 任务同形）。
    let started = DateTime::parse_from_rfc3339("2026-08-09T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let limits = WatchdogLimits {
        max_turns: None,
        max_wall_time_secs: None,
        max_idle_time_secs: Some(1), // started 远在过去 → 必然 Stalled
        ..WatchdogLimits::default()
    };
    let usage = WatchdogUsage::default();
    {
        let mut repository = RuntimeRepository::open(&database).unwrap();
        for _ in 0..2 {
            let session = RootSessionId::new();
            let task = yi_agent_core::TaskId::new();
            let attempt = yi_agent_core::AttemptId::new();
            repository
                .create_task_with_attempt(&task, &session, &attempt, 1, "queued")
                .unwrap();
            repository
                .save_attempt_watchdog_snapshot(
                    &task, &attempt, &limits, &usage, None, started, None,
                )
                .unwrap();
        }
    }

    // 不 hydrate 任何会话：supervisors map 为空。
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();

    let now = started + ChronoDuration::seconds(3600);
    let result = coordinator.evaluate_watchdogs(now).await;

    // 修复前：整轮返回 Err(SessionNotFound)。
    // 修复后：Ok，且 0 条被提交（两条的会话都未 hydrate，各自被隔离跳过）。
    assert!(
        result.is_ok(),
        "an unhydrated session must not fail the whole sweep: {result:?}"
    );
    assert_eq!(
        result.unwrap(),
        0,
        "no verdict can be committed for unhydrated sessions, but the sweep must complete"
    );
    // 库里也不应出现任何 watchdog 终态（两条都被隔离跳过，而非被误判）。
    let leftover = RuntimeRepository::open(&database)
        .unwrap()
        .event_records_after(0)
        .unwrap();
    assert!(
        !leftover.iter().any(|event| matches!(
            event.event,
            RuntimeEvent::TaskStalled
                | RuntimeEvent::TaskTimedOut
                | RuntimeEvent::TaskBudgetExhausted
        )),
        "no watchdog terminal may be written for an unhydrated session"
    );
}
```

再补一条**混合**用例，证明扫描不会在第 1 条就停下——一个会话被 hydrate（可终止），
另一个未 hydrate：

```rust
/// 混合：一个会话 hydrate 过、另一个没有。修复前，若未 hydrate 的那条先被
/// 处理（顺序由 SQL 决定，不可控），已 hydrate 的那条就永远不会被判定。
/// 断言"已 hydrate 的必然被终止"覆盖了两种顺序。
#[tokio::test]
async fn a_hydrated_attempt_is_terminated_even_beside_an_unhydrated_one() {
    use yi_agent_store::schedule::{WatchdogLimits, WatchdogUsage};

    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();

    // 会话 A：走 create_session，因此其 supervisor 已在 map 里。
    let hydrated = coordinator.create_session().unwrap();
    let hydrated_root = coordinator.root_task_id(&hydrated).unwrap();

    // 会话 B：手工造一条未 hydrate 的 queued 越限快照。
    let started = DateTime::parse_from_rfc3339("2026-08-09T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let limits = WatchdogLimits {
        max_turns: None,
        max_wall_time_secs: None,
        max_idle_time_secs: Some(1),
        ..WatchdogLimits::default()
    };
    {
        let mut repository = RuntimeRepository::open(&database).unwrap();
        let session = RootSessionId::new();
        let task = yi_agent_core::TaskId::new();
        let attempt = yi_agent_core::AttemptId::new();
        repository
            .create_task_with_attempt(&task, &session, &attempt, 1, "queued")
            .unwrap();
        repository
            .save_attempt_watchdog_snapshot(
                &task, &attempt, &limits, &WatchdogUsage::default(), None, started, None,
            )
            .unwrap();
        // 让已 hydrate 的那个也越限。
        let hydrated_attempt: yi_agent_core::AttemptId = Connection::open(&database)
            .unwrap()
            .query_row(
                "SELECT active_attempt_id FROM tasks WHERE id = ?1",
                [hydrated_root.to_string()],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
            .parse()
            .unwrap();
        repository
            .save_attempt_watchdog_snapshot(
                &hydrated_root, &hydrated_attempt, &limits, &WatchdogUsage::default(),
                None, started, None,
            )
            .unwrap();
    }

    let now = started + ChronoDuration::seconds(3600);
    let terminated = coordinator.evaluate_watchdogs(now).await.unwrap();
    assert!(
        terminated >= 1,
        "the hydrated attempt must still be terminated beside an unhydrated sibling"
    );
    assert!(
        matches!(
            coordinator.task_state(&hydrated_root).unwrap().as_str(),
            "stalled" | "timed_out" | "budget_exhausted"
        ),
        "hydrated attempt must reach a watchdog terminal"
    );
}
```

注意 `hydrated_root` 的 attempt 初始状态是 `queued`（`create_session_with_objective_and_mode`
用 `"queued"` 建根任务），`record_watchdog_terminal` 的 SQL 白名单含 `'queued'`，
所以它可被判定——若你发现它落不进白名单，先把该任务 activate 再测。


> Step 2 会把 `coordinator` 的具体构造补上。**先写测试、让它因为 `coordinator` 未定义而编译失败**，确认测试代码本身进入了编译单元。

- [ ] **Step 2: 补上 coordinator 的构造，确认测试失败**

查 `RuntimeCoordinator` 的构造入口：它的签名是
`open(database_path: impl AsRef<Path>, factory: Arc<dyn AgentWorkerFactory>)`
（`runtime.rs:392`）。把 Step 1 的占位换成：

```rust
    let factory = Arc::new(RecordingFactory::new());
    let coordinator = RuntimeCoordinator::open(&database, factory).unwrap();
```

工厂用 `runtime_coordinator.rs` 里**既有的**那个——该文件已定义了
`RecordingFactory`（:32）、`ProviderGateRecordingFactory`（:49）、
`FailingFactory`（:72）等，并用
`RuntimeCoordinator::open(&database, factory)`（:666/:694/:747）构造协调器。
挑一个最省事且不启真 worker 的；**不要**自己新造工厂类型。

Run: `cargo test -p yi-agent-store --test runtime_coordinator one_unhydrated_session_does_not_abort_the_watchdog_sweep`
Expected: **失败**，且失败原因是 `evaluate_watchdogs` 返回了 `Err`（`SessionNotFound`），或断言卡在错误的位置——不是编译失败。

- [ ] **Step 3: 实现逐条隔离**

把 `runtime.rs` 的 `evaluate_watchdogs` 改成"每条快照独立处理、失败只 warn"。核心改动：把 per-watchdog 的 `record_watchdog_terminal(...).await?` 换成 match，Err 时记录并继续：

```rust
    pub async fn evaluate_watchdogs(
        &self,
        now: DateTime<Utc>,
    ) -> Result<usize, RuntimeCoordinatorError> {
        // 只有「取快照列表」失败才算整轮失败；单条快照处理失败必须隔离，
        // 否则一个未 hydrate 的会话会让它后面所有 attempt 都逃过判定。
        let watchdogs = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .active_attempt_watchdogs()?;
        let mut terminal_count = 0;

        for watchdog in watchdogs {
            let Some(outcome) = evaluate_watchdog(
                &watchdog.snapshot.limits,
                &watchdog.snapshot.observation,
                now,
            ) else {
                continue;
            };
            let terminal = match outcome {
                WatchdogOutcome::Stalled => WatchdogTerminal::Stalled,
                WatchdogOutcome::TimedOut(kind) => WatchdogTerminal::TimedOut(kind),
                WatchdogOutcome::BudgetExhausted(kind) => WatchdogTerminal::BudgetExhausted(kind),
            };
            let elapsed_secs = now
                .signed_duration_since(watchdog.snapshot.observation.last_meaningful_at)
                .num_seconds()
                .max(0) as u64;
            let evidence = WatchdogEvidence {
                last_meaningful_event_id: watchdog.snapshot.last_meaningful_event_id,
                last_meaningful_at: watchdog.snapshot.observation.last_meaningful_at,
                elapsed_secs,
                current_wait: watchdog.snapshot.current_wait.clone(),
            };
            match self
                .record_watchdog_terminal(
                    &watchdog.session_id,
                    &watchdog.task_id,
                    terminal,
                    evidence,
                )
                .await
            {
                Ok(true) => terminal_count += 1,
                Ok(false) => {}
                Err(error) => tracing::warn!(
                    session = %watchdog.session_id,
                    task = %watchdog.task_id,
                    error = %error,
                    "watchdog verdict could not be applied; other attempts continue"
                ),
            }
        }
        Ok(terminal_count)
    }
```

注意 `current_wait: watchdog.snapshot.current_wait.clone()`——原来的代码在这里把字段 move 出去了，改成 clone 以便 `watchdog.session_id` / `task_id` 在同一分支里仍可用（编译器会告诉你需不需要）。

- [ ] **Step 4: 跑测试，确认通过**

Run: `cargo test -p yi-agent-store --test runtime_coordinator`
Expected: 全绿，含新测试 `one_unhydrated_session_does_not_abort_the_watchdog_sweep`。

- [ ] **Step 5: 回归既有 watchdog 测试（确认没改坏语义）**

Run: `cargo test -p yi-agent-store`
Expected: 全绿（尤其 `runtime_coordinator.rs` 里既有的看门狗用例与 `scheduler.rs`）。

- [ ] **Step 6: fmt + Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-store/src/runtime.rs yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs
git commit -m "fix(store): 看门狗判定逐条隔离失败,一个未 hydrate 会话不再毒死整轮扫描"
```

---

### Task 2: 把分钟分支抽成可直调的方法（纯搬移，先不接线）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs:904-963`（listener 闭包）与 `impl Daemon`（`ipc.rs:823`）

**Interfaces:**
- Consumes: `RuntimeCoordinator::evaluate_schedules` / `prune_terminal_traces` / `prune_expired_fork_uploads`。
- Produces: `Daemon::run_minute_maintenance(runtime: &tokio::runtime::Runtime, coordinator: &RuntimeCoordinator, now: DateTime<Local>)`——**私有**（crate 内可见），供 Task 3 接线、Task 4 测试直调。

- [ ] **Step 1: 确认当前分支体的确切内容**

Run: `sed -n '941,965p' yi-agent-rs/crates/yi-agent-store/src/ipc.rs`
Expected: `if minute != last_schedule_minute { … evaluate_schedules … prune_terminal_traces … prune_expired_fork_uploads … last_schedule_minute = minute; }`。

- [ ] **Step 2: 抽出方法（行为完全不变）**

在 `impl Daemon` 里（`socket_path()` 附近）新增：

```rust
    /// 每分钟的周期维护。抽成方法是为了让测试能驱动**线程每分会调用的同一条
    /// 路径**，而不是等墙上时钟走到下一个整分（否则测试只能绕过驱动路径
    /// 直调各方法，那正好测不到「接线」本身）。
    fn run_minute_maintenance(
        runtime: &tokio::runtime::Runtime,
        coordinator: &RuntimeCoordinator,
        now: DateTime<Local>,
    ) {
        let _ = coordinator.evaluate_schedules(now);
        // Trace retention is cleanup, not work: a failure here must never wedge
        // the accept loop, but a wedged database must be visible rather than silent.
        match coordinator.prune_terminal_traces(Utc::now()) {
            Ok(pruned) => tracing::debug!(pruned, "pruned terminal task traces"),
            Err(error) => tracing::warn!(
                error = %error,
                "terminal trace pruning failed; retrying next minute"
            ),
        }
        // Abandoned fork uploads are the other per-minute leak: a client that
        // dies mid-upload leaves state a later client may never claim.
        let expired_uploads = coordinator.prune_expired_fork_uploads(Instant::now());
        if expired_uploads > 0 {
            tracing::debug!(expired_uploads, "pruned expired fork uploads");
        }
    }
```

然后把闭包体里的那段替换成：

```rust
                if minute != last_schedule_minute {
                    Daemon::run_minute_maintenance(&runtime, &coordinator, now);
                    last_schedule_minute = minute;
                }
```

保留原来的整段注释（"Trace retention is cleanup…"等）**挪进**方法体，别丢。

- [ ] **Step 3: 编译并跑相关测试，证明纯搬移无行为变化**

Run: `cargo test -p yi-agent-store --lib`
Expected: 全绿。

Run: `cargo test -p yi-agent-store --test runtime_ipc`
Expected: 全绿（daemon 端到端路径未变）。

- [ ] **Step 4: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-store/src/ipc.rs
git commit -m "refactor(store): 把 daemon 的分钟维护抽成 Daemon::run_minute_maintenance"
```

---

### Task 3: 接线——在分钟维护里调用 `evaluate_watchdogs`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`（`run_minute_maintenance` 内）

**Interfaces:**
- Consumes: Task 1 的 `evaluate_watchdogs`（现在逐条隔离）、Task 2 的 `run_minute_maintenance`。
- Produces: daemon 每分钟执行一次看门狗判定（生产调用者出现）。

- [ ] **Step 1: 确认接线前的调用者清单（这是"修复前"的证据）**

Run: `grep -rn "evaluate_watchdogs" --include=*.rs yi-agent-rs/crates/*/src/`
Expected: 只有 `runtime.rs` 的定义行。**把这条输出记下来**，它会在 Step 3 变成两条。

- [ ] **Step 2: 在 `run_minute_maintenance` 里加调用**

在 `let _ = coordinator.evaluate_schedules(now);` **之后**插入：

```rust
        // 看门狗决定任务生死（终止越限 attempt 并释放其租约），所以它的失败
        // 不能像 prune 那样静默：一个失败的 tick 意味着越限的任务又活了一分钟。
        // `evaluate_watchdogs` 是 async（要拿 supervisor 的 async 锁），用
        // current-thread runtime 的 block_on 驱动——与 `reconcile_worker_events`
        // 在 accept 循环里的调用方式一致。
        match runtime.block_on(coordinator.evaluate_watchdogs(Utc::now())) {
            Ok(terminated) if terminated > 0 => {
                tracing::info!(terminated, "watchdog terminated over-budget attempts")
            }
            Ok(_) => {}
            Err(error) => tracing::error!(
                error = %error,
                "watchdog evaluation failed; over-budget attempts survive until the next tick"
            ),
        }
```

- [ ] **Step 3: 确认生产调用者已出现**

Run: `grep -rn "evaluate_watchdogs" --include=*.rs yi-agent-rs/crates/*/src/`
Expected: 两条——`runtime.rs`（定义）与 `ipc.rs`（调用）。这直接对应 spec §4 的第一条验收判据。

- [ ] **Step 4: 编译 + 跑 store 全量**

Run: `cargo test -p yi-agent-store`
Expected: 全绿。

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-store/src/ipc.rs
git commit -m "fix(store): daemon 每分钟执行看门狗判定,越限 attempt 不再逃过终止"
```

---

### Task 4: 驱动路径测试——证明接线真的会终止越限 attempt（TDD）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`（crate 内 `#[cfg(test)] mod`）

**Interfaces:**
- Consumes: `Daemon::run_minute_maintenance`（Task 2）、`evaluate_watchdogs`（Task 3）。
- Produces: 无新接口；这是 spec §3.3 要求的"从驱动路径出发"的验证。

- [ ] **Step 1: 写会失败的测试**

在 `ipc.rs` 的 crate 内测试里新增（选一个既有的 `#[cfg(test)] mod`，或新建一个
`mod watchdog_tick_tests`）。它必须**驱动 `run_minute_maintenance`**，而不是直调
`evaluate_watchdogs`。

该 mod 需要补 import（`ipc.rs` 的生产代码今天不引这些）：
`use yi_agent_store::schedule::{WatchdogLimits, WatchdogUsage};`、
`use yi_agent_store::repository::RuntimeRepository;`、
`use chrono::DateTime;`（`Local`/`Utc`/`Instant` 已在文件顶部 import）。
`tempfile` 是 dev-dependency，可直接用。

```rust
    /// 回归「看门狗从不运行」：测试走的是 daemon 线程每分会调的那条路径。
    /// 修复前（未接线）本测试失败——越限 attempt 仍是 running。
    #[test]
    fn a_minute_maintenance_tick_terminates_an_over_budget_attempt() {
        let directory = tempfile::TempDir::new().unwrap();
        let database = directory.path().join("runtime.sqlite");

        // 1) 起真实 daemon（复用既有脚手架）。
        let daemon = Daemon::start_with_factory(
            directory.path().join("runtime"),
            &database,
            Arc::new(UnavailableWorkerFactory),
        )
        .unwrap();

        // 2) 造一个越限 attempt（该会话未 hydrate，符合真实形状）。
        let session = RootSessionId::new();
        let task = yi_agent_core::TaskId::new();
        let attempt = yi_agent_core::AttemptId::new();
        let started = DateTime::parse_from_rfc3339("2026-08-09T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        {
            let mut repository = RuntimeRepository::open(&database).unwrap();
            repository
                .create_task_with_attempt(&task, &session, &attempt, 1, "queued")
                .unwrap();
            repository
                .save_attempt_watchdog_snapshot(
                    &task,
                    &attempt,
                    &WatchdogLimits {
                        max_turns: None,
                        max_wall_time_secs: None,
                        max_idle_time_secs: Some(1), // 让"很久没有有效事件"触发 Stalled
                        ..WatchdogLimits::default()
                    },
                    &WatchdogUsage { turns: 9, ..WatchdogUsage::default() },
                    None,
                    started,
                    None,
                )
                .unwrap();
        }

        // 3) 驱动一次分钟维护——与线程每分调用的完全同一条路径。
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        Daemon::run_minute_maintenance(
            &runtime,
            &daemon.coordinator,
            (started + chrono::Duration::seconds(10)).with_timezone(&Local),
        );

        // 4) 断言三条后果。
        let repository = RuntimeRepository::open(&database).unwrap();
        let state = repository.task_state(&task).unwrap();
        assert!(
            matches!(state.as_str(), "stalled" | "timed_out" | "budget_exhausted"),
            "over-budget attempt must be terminated by the tick, got {state}"
        );
        assert!(
            repository
                .event_records_after(0)
                .unwrap()
                .iter()
                .any(|event| matches!(
                    event.event,
                    RuntimeEvent::TaskStalled
                        | RuntimeEvent::TaskTimedOut
                        | RuntimeEvent::TaskBudgetExhausted
                )),
            "a watchdog terminal event must be recorded"
        );
        assert!(
            !repository.has_active_lease_prefix(&task, "").unwrap(),
            "the attempt's leases must be released"
        );
    }
```

> 上面用到的 API 已核对真实存在：`RuntimeRepository::task_state`（`repository.rs:2498`）、
> `has_active_lease_prefix`（`repository.rs:3092`，传空前缀即"该任务还有没有活跃租约"）、
> `event_records_after`（`repository.rs:3625`）、`create_task_with_attempt`（`repository.rs:922`）、
> `save_attempt_watchdog_snapshot`（`repository.rs:1111`）、`UnavailableWorkerFactory`（`ipc.rs:4052`）。
> `RuntimeEvent::TaskStalled` / `TaskTimedOut` / `TaskBudgetExhausted`（`repository.rs:127-129`）。
> `schedule` 是 `pub mod`（`lib.rs:11`），故 `WatchdogLimits` / `WatchdogUsage` 可从外部路径引。
> 但这些类型的**实际 use 路径**以 `runtime_coordinator.rs` 顶部的 import 为准，先看一眼再写，别猜。

- [ ] **Step 2: 跑测试，确认它在"未接线"时失败**

若 Task 3 已完成，临时把 `run_minute_maintenance` 里的 `evaluate_watchdogs` 调用注释掉，Run:
`cargo test -p yi-agent-store --lib a_minute_maintenance_tick_terminates_an_over_budget_attempt`
Expected: **失败**，断在 `must be terminated`（state 仍是 `running`）。然后恢复调用。

> 这一步是 TDD 的"红"：它证明测试真的能发现"看门狗没被调用"这个缺陷——
> 今天那两处只调 `evaluate_watchdogs` 的测试**做不到**这件事。

- [ ] **Step 3: 跑测试，确认接线后通过**

Run: `cargo test -p yi-agent-store --lib a_minute_maintenance_tick_terminates_an_over_budget_attempt`
Expected: 通过。

- [ ] **Step 4: 补隔离性测试（对应 Task 1 的契约，但走驱动路径）**

在同一个 `mod` 里新增：造两个越限快照（分属两个未 hydrate 的会话），驱动一次 tick，断言**两条**都被终止（事件表里有两条 watchdog 终态事件）：

```rust
    #[test]
    fn one_unhydrated_session_does_not_stop_the_tick_from_terminating_another() {
        // …与上一条同构，但造 2 个会话的越限 attempt，
        // 断言 tick 之后事件表里有 2 条 watchdog 终态事件。
    }
```

Run: `cargo test -p yi-agent-store --lib`
Expected: 全绿（含这两条新测试）。

- [ ] **Step 5: 跑 store 全量 + clippy**

Run: `cargo test -p yi-agent-store`
Expected: 全绿。

Run: `cd yi-agent-rs && cargo clippy -p yi-agent-store --all-targets --all-features -- -D warnings`
Expected: 无警告。

- [ ] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-store/src/ipc.rs
git commit -m "test(store): 从 daemon 驱动路径证明越限 attempt 会被看门狗终止"
```

---

### Task 5: 手工验证隔离性（用真实库的副本，最接近生产）

**Files:** 无代码改动。产出是 PR 描述里的一段验证记录。

**为什么这条最有价值**：真实库里此刻就有 **50 个**越限的 `queued` 快照
（分属 50 个会话、全部越过 idle 上限）。未做 §3.2 隔离时，首轮扫描会在第 1 条
中止；做了隔离后，同一批 tick 应当覆盖**全部 50 条**。这把"隔离生效"从一个
合成用例变成了一次可复现的真实数据验证。

- [ ] **Step 1: 复制真实库，确认 50 条越限快照**

```bash
work=$(mktemp -d)
cp ~/.yi-agent/runtime/runtime.sqlite "$work/runtime.sqlite"
sqlite3 "$work/runtime.sqlite" "
select count(*) from attempt_watchdogs w join tasks t on t.id = w.task_id
 where t.state_json = 'queued'
   and (julianday('now') - julianday(w.last_meaningful_at)) * 86400
       >= json_extract(w.limits_json, '$.max_idle_time_secs');"
```

Expected: `50`（若你的库不同，记下实际数 N，下面按 N 断言）。**注意**：`max_turns`
默认 200、`queued` 的 `usage.turns=0`，所以这批是靠 **idle 超时**（`Stalled`）
触发的，不是 turns。

- [ ] **Step 2: 在副本上直接跑隔离性单元测试的等价断言**

比起起完整 daemon 等一分钟，更稳的做法是复用 Task 4 的测试形态，把库路径指向
这个副本：

```bash
cp "$work/runtime.sqlite" /tmp/watchdog-isolation.sqlite
```

然后写一个临时的、只读断言（不要提交）：打开该库，跑一次
`Daemon::run_minute_maintenance`（或直接 `evaluate_watchdogs`），再查
`select count(*) from tasks where state_json in ('stalled','timed_out','budget_exhausted')`
——应新增 **N** 条。

> 若你更想走真 daemon，见 Step 3；两条路径择一即可，但**必须**给出 N 条的实测数字。

- [ ] **Step 3（可选）: 起真 daemon 观察日志**

Run（在 `yi-agent-rs` 下，参数据实际 CLI 为准——先
`grep -rn "runtime_dir\|runtime-dir" yi-agent-rs/crates/yi-agent/src/main.rs | head`）：
```bash
RUST_LOG=yi_agent_store=info cargo run --offline -p yi-agent -- run --help 2>&1 | head -5
```
Expected: 一分钟内日志出现 `watchdog terminated over-budget attempts terminated=N`（N>=1）。

- [ ] **Step 4: 清理**

Run: `rm -rf "$work" /tmp/watchdog-isolation.sqlite`（**不要**动
`~/.yi-agent/runtime/runtime.sqlite` 本体）。

---

## 收尾：合并前的验证清单

- [ ] `grep -rn "evaluate_watchdogs" --include=*.rs yi-agent-rs/crates/*/src/` 命中两条（`runtime.rs` + `ipc.rs`）
- [ ] `cargo test -p yi-agent-store` 全绿
- [ ] `cargo test -p yi-agent-store --lib` 含 `a_minute_maintenance_tick_terminates_an_over_budget_attempt`
- [ ] 该测试在注释掉接线时**失败**（红已验过，见 Task 4 Step 2）
- [ ] 隔离性测试通过（2 条越限都终止）
- [ ] Task 5 给出真实库副本上「N 条越限全部被判定」的实测数字
- [ ] `cargo clippy -p yi-agent-store --all-targets --all-features -- -D warnings` 无警告
- [ ] `cargo fmt --all`（在 `yi-agent-rs/` 下）
- [ ] 按仓库规范更新 `/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/docs/project-management/` 对应模块文件（本卡改的是 `yi-agent-store`）
