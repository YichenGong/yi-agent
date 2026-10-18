# 设计：看门狗需要执行者 —— 让 `evaluate_watchdogs` 真正被周期性调用

日期：2026-10-18
状态：待评审
分支：`feat/watchdog-executor`
关系：本篇是 2026-10-17 全模块审查（`.superpowers/review-2026-10-17/`）P0-1 的落地设计。
与 `2026-08-09-runtime-scheduling-design.md`（定义了看门狗判定与「每个 lease 恰好释放一次」
的语义）、`2026-10-02-subagent-cascade-on-terminal-design.md`（定义终态级联）互补：
那两篇定义**判定与级联语义**，本篇补的是**唯一缺失的调用者**。

## 1. 背景与问题

### 1.1 现象：看门狗是一台装好了却从没接电的机器

`RuntimeCoordinator::evaluate_watchdogs` 是完整的看门狗执行入口：它读持久化的
attempt 快照、用 `evaluate_watchdog` 判定、把终态写进库、终止 worker、释放租约、
级联父终态。但它在**生产代码里没有任何调用者**：

```
$ grep -rn "evaluate_watchdogs" --include=*.rs yi-agent-rs/crates/*/src/
yi-agent-rs/crates/yi-agent-store/src/runtime.rs:2250:    pub async fn evaluate_watchdogs(
$ grep -rn "evaluate_watchdogs" --include=*.rs yi-agent-rs/crates/ | grep -v "/src/"
yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs:2765: …evaluate_watchdogs(now)…
yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs:2767: …evaluate_watchdogs(now)…
```

即：**唯一的两处引用都在测试里**。它读的
`RuntimeRepository::active_attempt_watchdogs`（`repository.rs:1315`）同样只被它自己调用，
所以整条链在生产中从未被触发。

### 1.2 运行时证据：上限被穿越了，而且每次都靠人工兜底

看门狗要执行的三条上限（`WatchdogLimits::default()`，`schedule.rs:157-165`）是
`max_turns=200`、`max_wall_time_secs=2700`、`max_idle_time_secs=300`。查真实运行库
（`<项目>/.yi-agent/runtime/runtime.sqlite`：1407 个任务 / 1431 个 attempt 的账本）：

```
-- 越过 turns 预算的 attempt：18 个，最严重者用了 458 轮（上限 200，超 258 轮）
select count(*) from attempt_watchdogs w
 where json_extract(w.usage_json,'$.turns') >= json_extract(w.limits_json,'$.max_turns');
→ 18

-- 这 18 个最终是怎么结束的（看门狗本该是唯一正常的出口）
select a.state, count(*) from attempt_watchdogs w join attempts a on a.id=w.attempt_id
 where json_extract(w.usage_json,'$.turns') >= json_extract(w.limits_json,'$.max_turns')
 group by 1;
cancelled            13     ← 人手动取消
rework_requested      3
failed                1
completed_no_changes  1     ← 侥幸跑完

-- 越过 wall-time 上限（2700s）的 attempt
→ 362

-- 看门狗终态是否曾经发生过（事件表全量）
select distinct kind from events where kind in
  ('task_stalled','task_timed_out','task_budget_exhausted');
→ (空)
```

最后一条是决定性的：**看门狗的三个终态在整本账里一条都没有出现过**。越过预算的
attempt 全部落进 `cancelled`（13/18）——也就是说，**今天"看门狗"的实际实现是"人发现
它不停，手动取消"**。362 个 attempt 跑过 2700 秒墙钟也没被停。

### 1.3 为什么这是 P0

1. **预算不是软建议，是准入决策的依据。** 资源租约（`llm`/`worktree`）按 attempt
   的存在与否被占用。一个越过 200 轮的 attempt 不会被释放，它持续占着名额，
   后续任务被 `waiting_for_resource` 挡住——这正是「一个跑飞的任务让整个队列饿死」
   的形状。
2. **数据管道是活的，所以这不是"用不上的死代码"。** 快照在生产中被持续写入：
   `reconcile_worker_events`（daemon 每 tick 调用）会把 worker 上报的
   `ProviderUsage`/`ProviderRetry`/`ToolRetry`/`MeaningfulProgress` 落到
   `attempt_watchdogs.usage_json`（`runtime.rs:2944-2968`）。所以值一直在涨、
   一直在被正确记录，**只是没人在旁边看**。上面 turns=458 的行就是活证据。
3. **它使一批"已实现"的功能处于假活状态。** `stall_task`/`timeout_task`/
   `exhaust_task_budget`、`WatchdogTerminal`、`record_watchdog_terminal` 的幂等事务、
   父终态级联、租约释放——全都只有测试覆盖。**测试全绿而生产中从不执行**，
   是审查报告「模式 B：测试绿但生产从不走这条路」的教科书案例。

## 2. 目标与非目标

**目标**

- G1：把 `evaluate_watchdogs` 接进 daemon 既有的周期性驱动，使它按确定的节奏运行。
- G2：确保接入后**不会**在某个会话尚未 hydrate 时让整轮扫描中止（见 §3.2 的
  `SessionNotFound` 陷阱）。
- G3：补一个**跨层**的测试：从 daemon 的驱动路径出发，断言越限 attempt 真的被终止——
  不允许只测 `evaluate_watchdogs` 自己（那正是今天"全绿但从不跑"的原因）。

**非目标**

- 不改看门狗的判定语义（`evaluate_watchdog` 的优先级顺序、各上限的默认值）。
  那是 `2026-08-09-runtime-scheduling-design.md` 的范围。
- 不改终态级联与租约释放的幂等实现（`record_watchdog_terminal` 已正确）。
- 不引入新的后台线程/定时器框架。daemon 已有一个分钟 tick（`ipc.rs:904-963`），
  看门狗挂进去即可。
- 不动 `docs/bug-list.md` / `docs/relay-deploy.md`（那是 `feat/secret-hygiene` 的范围）。

## 3. 设计

### 3.1 挂载点：daemon 的既有分钟 tick

daemon 的 listener 线程里已有一个"每分钟一次"的分支（`ipc.rs:943-963`），今天做三件事：

```rust
if minute != last_schedule_minute {
    let _ = coordinator.evaluate_schedules(now);
    match coordinator.prune_terminal_traces(Utc::now()) { … }
    let expired_uploads = coordinator.prune_expired_fork_uploads(Instant::now());
    last_schedule_minute = minute;
}
```

看门狗挂在这里，**理由**：

- 它需要"时间在走"才能给出 `TimedOut`/`Stalled`，而这里是 daemon 唯一稳定的
  时间刻度（`DAEMON_ACCEPT_POLL = 10ms`，靠 `minute != last_schedule_minute` 折叠出分钟事件）。
- `evaluate_schedules` 已经在同一位置，二者是同类的"周期判定"，相邻放置让"哪些
  东西需要周期扫描"这个问题只有一个答案。
- 失败姿态沿用邻居的约定：`evaluate_schedules` 用 `let _ =`（排程失败不能让
  accept 循环停摆），trace pruning 用 `match` + `warn`。看门狗采用后者——
  **它决定任务生死，失败必须可见**（与 `reconcile_worker_events` 的
  `tracing::error!` 一致）。

```rust
if minute != last_schedule_minute {
    let _ = coordinator.evaluate_schedules(now);
    // 看门狗决定任务生死，所以它的失败不能像 prune 那样静默：
    // 一个失败的 tick 意味着越限的任务又活了一分钟。
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
    match coordinator.prune_terminal_traces(Utc::now()) { … }
    …
}
```

注意 `runtime.block_on(...)`——`evaluate_watchdogs` 是 `async fn`（它要拿
supervisor 的 async 锁），而这段循环所在的 `runtime` 是 current-thread tokio
运行时，`reconcile_worker_events` 就是这么调的（`ipc.rs:933`），沿用同一手法。

**同时必须做的重构：把"分钟分支"抽成一个可直调的方法。** 今天这三件事内联在
`thread::spawn` 的闭包体里，测试无法在 60 秒之外触发它——要么等一分钟，要么
只能绕过驱动路径直调 `evaluate_watchdogs`（正是 §1.3 批评的假活测试）。
所以抽出：

```rust
impl Daemon {
    /// 每分钟的周期维护。抽出来是为了让测试能驱动同一条路径，
    /// 而不是等墙上时钟走到下一个整分。
    fn run_minute_maintenance(
        runtime: &tokio::runtime::Runtime,
        coordinator: &RuntimeCoordinator,
        now: DateTime<Local>,
    ) { … 上面那四件事 … }
}
```

线程里改为 `if minute != last_schedule_minute { Daemon::run_minute_maintenance(&runtime, &coordinator, now); last_schedule_minute = minute; }`。
这是纯搬移，行为不变；但它把"接线本身"变成了可测对象。

### 3.2 必须处理的陷阱：库全局读 × 进程内部分视图

`evaluate_watchdogs` 读的是**库全局**的事实，而它判定时要查的是**进程内**的表：

- 读：`active_attempt_watchdogs()`（`repository.rs:1315`）返回**库里**所有活跃
  attempt 的看门狗快照——不限会话、不限本进程。
- 查：`record_watchdog_terminal` → `self.supervisor(session)?`（`runtime.rs:3238-3247`），
  而 `supervisors` 只装**本进程已 hydrate 的会话**：

```rust
fn supervisor(&self, session: &RootSessionId) -> Result<…> {
    self.supervisors.lock()…get(session).cloned()
        .ok_or_else(|| RuntimeCoordinatorError::SessionNotFound(session.clone()))
}
```

`supervisors` 的填充点只有三处：启动 hydrate `recovery_required`/`recovery_gated`/
`recovery_attested`（`runtime.rs:513`）、`create_session*`（`runtime.rs:641`）、
`attach_application_root` / `ensure_application_root_supervisor`（`runtime.rs:760` 附近）。
**一个 `queued` 任务三处都不在**：

- `recover_inflight_tasks`（`repository.rs:2113`）只 park `running` / `waiting_*`；
- `recovered_tasks()`（`repository.rs:3175`）只 hydrate `recovery_required` /
  `recovery_gated` / `recovery_attested`；
- `create_session*` 只覆盖"本进程新建"的会话；`attach_application_root` 要等客户端
  真的来 attach。

于是**结构性错配**：库里有一条越限快照，而它的会话在本进程里没有 supervisor。
此时 `record_watchdog_terminal` 返回 `Err(SessionNotFound)`，而
`evaluate_watchdogs` 的 `?` 在 for 循环**内部**，于是整轮扫描在第一条这样的快照处
中止，**后面所有 attempt 都不被判定**（且顺序由 SQL 决定，不是"最该处理的那个优先"）。

**这不是假想的边界情况。** 本机真实库此刻就有 **50 个**越限的 `queued` 快照
（分属 50 个会话、全部越过 `max_idle_time_secs=300`，其 session 的 attachment
状态都是 `attached` 但需客户端重连才 hydrate）：

```
select count(*) from attempt_watchdogs w join tasks t on t.id = w.task_id
 where t.state_json = 'queued'
   and (julianday('now') - julianday(w.last_meaningful_at)) * 86400
       >= json_extract(w.limits_json, '$.max_idle_time_secs');
→ 50
```

接上 tick 后，**这个进程在客户端 attach 之前**的那段时间里就有 50 条会让扫描返回
`Err`——看门狗于是"每分钟失败一次"，与 §1.1 的"从不运行"在效果上等同，只多了日志。

**设计决定**：不修改 `record_watchdog_terminal`（它的 `?` 在单条语义下是对的），
而是让 `evaluate_watchdogs` **逐条隔离失败**——单条快照的 `Err` 降级为
`tracing::warn!` 并继续；只有"取快照列表"失败才向上返回 `Err`。这与它
"重复扫描幂等"的既有性质一致（已终止的任务不再出现在 `active_attempt_watchdogs()`）。

> 这是本设计**最重要的发现**：少了它，接线只是把"从不运行"换成"每分钟报一次错"。

### 3.3 测试：必须从驱动路径出发

今天的测试直接调 `coordinator.evaluate_watchdogs(now)`，所以它**无法**发现
"没人调用"这个缺陷。本设计要求新增一个测试，覆盖**接入本身**：

落点：**`ipc.rs` 内既有的 crate 内单元测试**（该文件已有 10 个
`#[cfg(test)] mod`、49 个用例），而不是 `tests/` 下的集成测试。理由：
`run_minute_maintenance` 是私有项、`Daemon.coordinator` 是私有字段，集成测试
只能触 pub API（唯一能触到协调器的口子是给 `Daemon` 加 pub 访问器，
那会把内部结构泄进公开面）。单元测试直接调
`Daemon::run_minute_maintenance(&runtime, &daemon.coordinator, Local::now())`，
**驱动的就是线程每分会调的那段代码**——测试与生产的路径是同一条。

```
ipc.rs :: #[cfg(test)] mod（新增用例）
fn a_minute_tick_terminates_an_over_budget_attempt()
- Daemon::start_with_factory(...) 起真实 daemon（复用既有脚手架）
- 用 RuntimeRepository 打开同一个库，造一个 attempt 并把它的
  attempt_watchdogs.limits_json 调成 max_turns 已被 usage 越过
  （该会话不 hydrate，符合真实形状——这正是 §3.2 的陷阱条件）
- 调 Daemon::run_minute_maintenance(&runtime, &daemon.coordinator, Local::now())
- 断言：
    (a) tasks.state_json 变成 stalled / timed_out / budget_exhausted 之一；
    (b) events 里出现 task_stalled / task_timed_out / task_budget_exhausted；
    (c) resource_leases 里该 task 的行 state='released'。
```

同时补一个**隔离性**测试（对应 §3.2）：

```
- 造两个越限快照：一个属于未 hydrate 的会话，一个属于已 hydrate 的会话
- 断言已 hydrate 的那个仍被终止（即坏会话没有毒死整轮扫描）
```

### 3.4 是否需要"降级"而非"终止"

原审查报告 T1 的建议是终止（`evaluate_watchdogs` 现有实现即终止）。本设计沿用终止，
理由：`max_turns`/`max_wall_time_secs` 是**用户在策略里显式设的预算**
（`schedule.rs:141-165`），越过它就是"这次尝试不再有资格继续占用资源"。若要
"暂停并等用户决定"，那是一种新的产品语义，应另开设计（见 §7），不应把
`evaluate_watchdogs` 的语义改暗。

## 4. 验收判据

- [ ] `grep -rn "evaluate_watchdogs" --include=*.rs yi-agent-rs/crates/*/src/` 至少命中
      `ipc.rs` 一处（证明有生产调用者）。
- [ ] `ipc.rs` 的 crate 内单元测试 `a_minute_tick_terminates_an_over_budget_attempt`
      在**修复前失败**、修复后通过（它驱动的是线程每分调用的同一条路径）。
- [ ] 隔离性测试通过（一个坏会话不阻断其它 attempt 的终止）。
- [ ] 手工验证：在测试库里留一个越限快照，起 daemon，一个 tick 内它被终止并释放租约。
- [ ] 在**真实库的副本**上验证隔离生效：50 个越限 `queued` 任务在首轮 sweep 后
      全部被判定（同一批 tick 覆盖到全部 50 个，而非只处理第 1 个），见 Task 5。
- [ ] `cargo test -p yi-agent-store` 全绿。
- [ ] `cargo clippy -p yi-agent-store --all-targets --all-features -- -D warnings` 通过。

## 5. 取舍与风险

### 5.1 为什么挂在 daemon 的 tick，而不是 app-server 或 worker 自己

- **worker 不能给自己判死刑**：判死需要完整快照（墙钟起点、最后有效事件、资源等待），
  这些在库里，worker 只上报增量。
- **app-server 不是唯一入口**：`--relay` 与 CLI 都会起 daemon，逻辑放 app-server
  会漏掉它们。
- **daemon 已经是"周期性事实"的唯一 owner**：它今天就在跑 `evaluate_schedules`
  与两类 pruning。把看门狗放别处会制造第二个"周期"概念。

### 5.2 分钟粒度够不够

够。三条上限里最紧的是 `max_idle_time_secs=300`（5 分钟）与
`max_wall_time_secs=2700`（45 分钟），分钟粒度相对它们有 5 倍以上余量。
`max_turns=200` 是计数上限，与时间无关，分钟粒度仅决定"最多多跑一轮"。

### 5.3 与在途工作的关系

- `feat/secret-hygiene`（本会话另一张卡）改的是 `docs/` 与 `justfile`，与本卡无交集。
- `2026-10-18-stranded-daemon-self-stop-design.md` 处理的是 daemon 自身不退出
  （幽灵 worktree），与本卡的"daemon 内要做周期判定"相邻但不同面。若那篇落地时
  重排了 tick 循环，本卡的插入点需要对齐——plan 里把它列为前置检查。

### 5.4 风险：终止会不会误伤"正在做长任务"的 attempt

会，且是**有意的**。`max_wall_time_secs=2700` 是策略默认值，用户可以用
`policy.runtime.max_wall_time_secs` 放宽（`repository.rs:681` 把它写进快照）。
把"我确实需要跑 3 小时"表达为策略，比把它表达为"别停我"更可审计。

## 6. 影响面

| 文件 | 改动 |
|---|---|
| `yi-agent-rs/crates/yi-agent-store/src/ipc.rs` | 分钟分支抽成 `Daemon::run_minute_maintenance`；其中新增 `evaluate_watchdogs` 调用；crate 内单元测试新增驱动路径用例与隔离性用例 |
| `yi-agent-rs/crates/yi-agent-store/src/runtime.rs` | `evaluate_watchdogs` 逐条隔离失败（§3.2） |

## 7. 后续（不在本篇范围）

- **看门狗的可观测面**：今天越限只能从库或日志看出来。可另开设计把"当前有多少
  attempt 逼近上限"暴露给 UI（`thread/diff/read` 那类 RPC 的邻居）。
- **"降级暂停"语义**：若产品上希望越限时先暂停等用户决定，应作为新的
  `WatchdogOutcome` 变体另开设计，而不是改本篇的终止决定。
- **idle 判定的精度**：`last_meaningful_at` 依赖 worker 上报 `MeaningfulProgress`。
  若某 provider 长时间只输出被判定为"非有效"的内容，会触发 `Stalled` 终止——
  这是 `2026-09-26-think-idle-stall-retry-design.md` 的范围。