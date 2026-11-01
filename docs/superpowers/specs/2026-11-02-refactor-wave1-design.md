# 重构第一波：store 内聚、桌面接线、agent 主循环、TUI 事件循环

**状态：** 设计（待评审）
**日期：** 2026-11-02
**范围：** 四块互相解耦的纯重构，**不改任何对外行为**

> 本设计对应「重构机会分析」里的第一波，**不含 C1**（`server.rs` 测试外移）。
> 四块的共同前提是同一条：**只搬不改语义，公共 API 与线上协议一律不变**。

> **修订记录（写完后自查，全部回源实测）**
>
> 初稿有 4 处数字错误，已逐条修正：
> 1. 取消序列写成「重复 7 次」——`safe_cancel_truncate_len` 的调用点实为 **6 处**（971/1134/1163/1190/1248/1615）。第 7 个 grep 命中是函数定义本身。另补一条关键区分：`flush_unconsumed` 结尾但不回滚的取消点有 8 处，语义不同，**不并入** `abort_run`。
> 2. `agent.rs` 生产代码写成 1,877 行——实为 **1,876**（测试模块起于 L1877）。
> 3. `App.tsx` 的 `useState` 写成 9 个——实为 **10 个**（首轮 grep 用 `const [x, setY]` 模式漏掉了 `const [, force]`）。补记为 29 个 `useRef` / 11 个 `useCallback`。
> 4. 源码注释里的「`handle_key` 已长到 25 个」是 **`TuiChannels` 引入前的历史峰值**，当前实测 **20 个**。已在正文说明这次补救只削减了 channel 一组。
>
> 两个门禁命令**已实测可跑**并写死基线：`ipc` 测试 **56** 个、`agent` 的 `cancel` 测试 **11** 个。目录里所有行号与行数均来自 `main` @ `f4a2b433` 的实跑，非估算。

---

## 0. 为什么是这四块

体检基线（`main` @ `f4a2b433`，2026-11-02 实测）：

| 项 | 数字 | 来源 |
|---|---|---|
| Rust 15 crate / 206,396 行（内联测试 78,223，37.9%） | — | 逐文件统计 |
| 桌面端 TS/TSX | 56,630 行 | — |
| `cargo clippy --workspace --all-targets --all-features` | **0 告警** | 实跑 |
| crate 依赖图 | **无环**，`yi-agent-core` 零内部依赖 | `Cargo.toml` 逐份 |
| `cargo test -p yi-agent-store` | 344 passed / 0 failed | 实跑 |
| `cargo test -p yi-agent-core` | 309 passed / 0 failed | 实跑 |
| `npx vitest run`（desktop） | 146 文件 / 1932 用例全绿 | 实跑 |

**结论：问题不在 lint，也不在 crate 分层，而在文件与函数的粒度过粗。**
因此这四块都不改架构、不换技术栈、不动协议，只做「把一大块按已被验证的边界切开」。

四块的可解耦性来自**文件集零重叠**：

| 块 | 触及的文件 |
|---|---|
| A | `yi-agent-store/src/{repository,runtime,ipc}.rs` |
| B | `desktop/src/App.tsx` + 新增 `hooks/`、`lib/` |
| D1 | `yi-agent-core/src/agent.rs` |
| D2 | `yi-agent/src/tui/app.rs` |

`Cargo.toml` 不改，crate 不增不减，所以四块可以并行，谁先合都不阻塞别人。

---

## 1. 全波次共用规则

这些规则对四个块无差别生效，后面各块不再重复。

**R1 只搬不改。** 允许的机械改动只有：函数/类型搬家、`use` 路径、可见性（`fn` → `pub(crate) fn` 或子模块内 `pub(super)`）、测试模块路径。**禁止**在本次顺手改逻辑、改文案、改变量名、改错误消息。

**R2 公共 API 冻结。** 下列签名逐字不变，任何变动都是设计被违反：
- `yi_agent_store::repository::RuntimeRepository` 的 102 个 `pub fn` 全表
- `yi_agent_store::runtime::RuntimeCoordinator` 的 69 个方法全表
- `yi_agent_store::ipc::*` 全部 `pub` 项（协议层是跨进程契约）
- `yi_agent_core::Agent` 全部 `pub` 方法、`AgentEvent` 全部变体
- `tui::app` 导出的 `pub fn run_tui*` 系列

**R3 分步提交。** 每块按下面各节的步骤顺序提交，每步都必须是「全绿可停」的状态——不允许存在「第三步做完才绿」的中间态。

**R4 每步验收固定三条**（各块只在这三条上加东西，不替换）：

```bash
cd yi-agent-rs
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
# 该块自己的测试范围，见各节
```

**R5 禁止移动测试模块的判定内容。** 测试断言与注释随测试一起搬，**一个字都不改**。搬完若某个测试因为可见性变了而编译不过，改的是可见性，不是测试。

---

## 2. A 块：store 内聚（`yi-agent-store`）

### 2.1 现状（实测）

| 文件 | 生产行 | 问题 |
|---|---|---|
| `repository.rs` | 3,911 | `impl RuntimeRepository` 单块 **3,343 行 / 102 个方法**，覆盖约 20 张表 |
| `runtime.rs` | 3,911 | `impl RuntimeCoordinator` 单块 **3,066 行 / 69 个方法** |
| `ipc.rs` | 3,744 | 线协议 + socket 服务端 + 客户端 + 队列 + worker 工厂，一个文件 |

关键事实：`RuntimeRepository` 只持有一个字段 `connection: Connection`，且 Rust 允许**子模块写 `impl` 并访问父模块的私有字段**。这意味着切分不需要引入 trait、不需要 newtype、不需要 `Deref`。

`repository.rs` 的方法按 SQL 里的表名可清晰分成 8 组（实测表出现频次）：

| 组 | 表 | 方法数（约） |
|---|---|---|
| `schedule` | `schedules`、`schedule_occurrences` | 11 |
| `admission` | `resource_admission_cursors`、`resource_leases` | 6 |
| `task` | `tasks`、`attempts` | 22 |
| `watchdog` | `attempt_watchdogs` | 8 |
| `mailbox` | `mailbox_messages`、`deliveries`、`permission_requests` | 12 |
| `trace` | `task_trace_events` | 7 |
| `application_root` | `application_root_attachments` | 9 |
| `read` | 跨表只读快照（`task_detail`、`subscription_snapshot` 等） | 12 |

### 2.2 目标结构

`repository.rs` 变成「结构体 + `open` + 迁移 + 分组声明」的薄壳，其余按组落到子模块：

```
src/repository.rs          # RuntimeRepository 定义、open()、migrate()、错误类型、类型定义
src/repository/schedule.rs # impl RuntimeRepository { ... }  仅 impl，无新类型
src/repository/task.rs
src/repository/watchdog.rs
src/repository/mailbox.rs
src/repository/trace.rs
src/repository/application_root.rs
src/repository/admission.rs
src/repository/read.rs
```

`runtime.rs` 同理，按方法前缀与注释里的域切：

```
src/runtime.rs               # RuntimeCoordinator 定义、open/with_fork_max_bytes、错误类型
src/runtime/fork.rs          # begin_fork_upload / append_fork_chunk / abort / take_fork_messages / prune_expired_fork_uploads
src/runtime/application_root.rs  # attach / activate / detach / authorise / ensure_supervisor / hydration
src/runtime/spawn.rs         # spawn_child* / spawn_application_child / start_worker / resolve_parent_workdir / prepare_task_workspace
src/runtime/lifecycle.rs     # cancel / pause / resume / retry / graceful_stop / draining / interrupt_*
src/runtime/watchdog.rs      # evaluate_watchdogs / record_watchdog_terminal / persist_worker_trace_facts
src/runtime/messaging.rs     # send_* / mailbox_snapshot / delivery_diff / direct_children / wait_for_children*
src/runtime/schedule.rs      # evaluate_schedules / resident cursor / capacity
src/runtime/admission.rs     # worker 事件对账 reconcile_worker_events / 认领与租约
```

`ipc.rs` 切成四个文件（按现有 `impl` 块的边界，**这一层很干净**）：

```
src/ipc/mod.rs         # 协议类型（IpcRequest 67 变体、IpcResponse、IpcError、IpcEvent…）、PROTOCOL_VERSION、socket_path_for
src/ipc/server.rs      # Daemon、InstanceLock、handle_client、respond_to_subscription、stream_subscription、write_* / encode_*
src/ipc/client.rs      # send_request*、subscribe*、into_result、write_request、next_request_id
src/ipc/queue.rs       # PendingSubscriptionFrames、PendingFrameWrite、ConfirmationStore、CancelScope、Subscription
```

⚠️ **`ipc.rs` 有两处必须当心**，两者都在现有测试里有约束：

1. 文件内共 **16 个 `#[cfg(test)] mod`**，穿插在正文中。重新划分文件时，**每个测试模块必须跟着它测试的类型一起走**。例如 `socket_path_tests` 跟 `socket_path_for` 走，`watchdog_tick_tests` 跟 `RequestContext` 走。这不是可选的整理，漏搬会直接丢测试覆盖。
2. `plugin_protocol_tests` 读的是**外部插件源码**（`plugins/superpowers-kanban/crates/.../wire.rs`），不是自身源码，切开不受影响，但它断言 `PROTOCOL_VERSION == 2`。**这条断言必须原样保留在新的 `ipc/mod.rs` 里**——它是宿主与插件协议同步的唯一防线。

### 2.3 步骤与验收

| 步 | 动作 | 验收 |
|---|---|---|
| A1 | `repository.rs` 按 8 组拆子模块（纯 impl 搬移） | `cargo test -p yi-agent-store` 344 全绿 |
| A2 | `runtime.rs` 按 9 组拆子模块 | 同上 |
| A3 | `ipc.rs` 拆 4 文件，16 个测试模块随类型迁移 | 同上 + `cargo test -p yi-agent-store --test subagent_ipc_protocol` |
| A4 | 更新 `docs/project-management/yi-agent-store.md` 的代码位置引用 | 引用行号可点开即中 |

A3 的额外验收（防止测试隐身）：拆完后 `ipc` 相关测试名集合必须与拆前**完全一致**。该命令已实测可用，拆前基线是 **56 个**：
```bash
# 拆前 / 拆后各跑一次，两次输出应 diff 为空
cargo test -p yi-agent-store --lib -- --list | grep '^ipc::' | sort > /tmp/ipc-before.txt
# ... 执行 A3 ...
cargo test -p yi-agent-store --lib -- --list | grep '^ipc::' | sort > /tmp/ipc-after.txt
diff /tmp/ipc-before.txt /tmp/ipc-after.txt && wc -l < /tmp/ipc-after.txt   # 期望 56
```

**A 块完成判据**：`repository.rs` ≤ 700 行、`runtime.rs` ≤ 900 行、`ipc/mod.rs` ≤ 1,200 行；store 的 344 个测试名集合不变；公共 API 零变化（`git diff` 里 `pub fn` 的签名行数为 0）。

---

## 3. B 块：桌面端接线收敛（`desktop/src/App.tsx`）

### 3.1 现状（实测，与直觉不同）

`App.tsx` 是 3,500 行，但**不是**「59 个 useState 堆在一起」。实际情况：

- 只有 **10 个 `useState`**（其中 4 个是设置/搜索类布尔或字符串，per-thread 内容一个都不在里面）。per-thread 状态放在**可变**的 `ThreadStore` 里，靠 `force()` 强制重渲染（文件开头的注释明确写了这个设计）。另有 29 个 `useRef` 与 11 个 `useCallback`。
- 真正的巨石是**一个 631 行的 `useEffect`**（L1635–2266），它同时承担四件互不相关的事：
  1. **连接生命周期**：`connect` / `connectSlot` / `syncActiveClient` / `reconnectNow` / `scheduleReconnect` / `reconnectSlot` / 退避计时器
  2. **前台唤醒**：`onVisibilityChange`（iOS 回前台 socket 已被系统掐断）
  3. **通知分派**：十几个 `if (n.method === "...")` 分支（`agent/children/updated`、`ui/settings/updated`、`model/capabilitiesChanged`、`ui/gitDiff/focus`、`agent/trace/event`、`process/updated`、`thread/titleChanged`、`thread/tagChanged`、`thread/groupsChanged`、`ui/tagNames/updated`、`turn/completed`）
  4. **握手后的恢复编排**：重放会话、补订阅、清空 warm 缓存、拉 boards、重新握手

已有的正确模式（B 块要沿用，不是发明）：
- `hooks/useSessionState.ts`、`useSettingsState.ts`、`useBoardState.ts`、`useDiffState.ts`、`useProcessState.ts` 五个领域 hook
- `lib/` 下 **130+ 个带单测的小模块**
- 文件内已有 11 处 `useCallback` 作为接缝

### 3.2 目标结构

只做**抽取**，不重写：

```
hooks/useConnectionLifecycle.ts   # 连接/重连/退避/前台唤醒（第 1、2 项）
hooks/useServerNotifications.ts   # 通知分派表（第 3 项），把 if 链变成 method→handler 映射
hooks/useHandshakeRecovery.ts     # 握手后恢复编排（第 4 项）
```

三个 hook 都接收现有的 `{ rpc, force, setCurrentError, ... }` 依赖，**行为与现在逐字等价**。`App.tsx` 保留 JSX 与业务回调（`selectThread`、`sendMessage` 等编排留在原处——它们跨关注点，抽出去反而制造隐式依赖）。

**明确不做**：不动 `ThreadStore` 的可变 + `force()` 架构。那是一个有意的性能取舍，注释里写明了理由；改它属于架构重构，不在这块范围。

### 3.3 步骤与验收

| 步 | 动作 | 验收 |
|---|---|---|
| B1 | 抽 `useConnectionLifecycle`（第 1、2 项） | `npx vitest run` 1932 全绿 |
| B2 | 抽 `useServerNotifications`，`if` 链改为映射表 | 同上；且每个 method 分支至少一条测试 |
| B3 | 抽 `useHandshakeRecovery` | 同上 |

**B 块完成判据**：`App.tsx` ≤ 2,000 行；**最大的单个 `useEffect` ≤ 150 行**；`vitest` 用例数 **≥ 1932 且全绿**（只许增不许减）；新增三个 hook 各自带 `.test.ts`。

风险提示：`App.test.tsx` 是 6,358 行的集成测试，它对 hook 内部细节不敏感，对**时序**敏感。B1 是最大风险步——抽取后如果 connect 的调用次数或顺序变了，`App.test.tsx` 会准确报出来，这正是我们想要的护栏。

---

## 4. D1 块：agent 主循环（`yi-agent-core/src/agent.rs`）

### 4.1 现状（实测）

`run_loop()`（`agent.rs:933`）单函数 **718 行**，是一个 `loop { }` 内含四个标号阶段：

```
Check 1 (THINK 前)  → 自动压缩 → max_turns 检查 → 注入 interjection
1. THINK            → 租约 → provider.call_stream → 重试退避 → accumulate → 重试退避
2. Termination check
3. ACT              → 权限检查（Allow/Deny/NeedConfirm）→ join_all 并行执行
Check 3 (ACT 中)    → cancel
4. OBSERVE          → 按 tool_use_id 顺序回填 tool_result
```

已提取的辅助函数（说明抽取是这个文件**既有**的做法，不是新引入的）：`maybe_auto_compact`、`inject_pending`、`flush_unconsumed`、`safe_cancel_truncate_len`、`accumulate_provider_stream`、`handle_confirmation`、`wait_for_decision`、`stall_backoff_delay`、`retryable_provider_error_cause`。

**核心发现**：「取消 → 回滚 session → 冲刷 interjection → 发 `Cancelled` → return」这段序列在 `run_loop` 里**逐字重复了 6 次**（`safe_cancel_truncate_len` 的调用点：971 / 1134 / 1163 / 1190 / 1248 / 1615）。这是这块最大、也最安全的收益。

（第 7 个以 `flush_unconsumed` 结尾、不带回滚的取消点分布在 959 / 1000 / 1069 / 1301 / 1317 / 1337 / 1348 / 1379，它们发生在「尚无 assistant 消息」的阶段，语义不同，**不并入**同一个 `abort_run`。D1a 只统一那 6 处。）

### 4.2 目标结构

两步，顺序不可换：

**D1a：消除 7 处重复**（先做，因为它让后续抽取的规模直接减半）

```rust
// 语义与现在逐字相同，只是把 5 行序列变成 1 次调用
async fn abort_run(
    tx: &EventTx,
    inbox: &Option<InboxHandle>,
    session: &Arc<Mutex<Session>>,
    turn: u32,
    reason: &'static str,
) { ... }
```

7 处调用点的**日志文本各不相同**（`"agent loop cancelled before think"`、`"cancelled during auto-compact"`、`"cancelled during provider call"` …），所以 `reason` 必须作为参数传入，不能统一。

**D1b：阶段抽取**

```rust
async fn think_phase(...)  -> ThinkOutcome    // 租约 + 调 provider + 重试 + accumulate
async fn act_phase(...)    -> Vec<ContentBlock> // 权限检查 + 并行执行 + 拒绝结果合并
fn        observe_phase(...)                   // 回填顺序 + push 到 session
```

关键约束：**`stall_retries`、`provider_turn_lease`、`last_logged` 必须显式在签名里传递**，不许塞进一个 `&mut LoopState`。理由：这三个值的作用域决定了正确性（租约必须在退避前 drop、`last_logged` 必须在压缩后归零），打包成结构体会让「谁在什么时候改它」变得不可见，而那正是现在这段代码最容易出错的地方。

### 4.3 步骤与验收

| 步 | 动作 | 验收 |
|---|---|---|
| D1a | 提取 `abort_run`，替换 7 处 | `cargo test -p yi-agent-core` 309 全绿 |
| D1b | 抽 `think_phase` | 同上 |
| D1c | 抽 `act_phase` + `observe_phase` | 同上 |

**D1 完成判据**：`run_loop` ≤ 200 行；`agent.rs` 生产代码（剔除测试）从 **1,876** 行降到 ≤ 1,600 行；**取消语义零变化**——`agent::tests` 里所有含 `cancel` 的测试名必须全部保留且通过。该命令已实测可用，基线 **11 个**（`agent_cancel_during_act_emits_cancelled`、`agent_cancel_during_act_rolls_back_session`、`agent_cancel_during_think_emits_cancelled`、`agent_cancel_keeps_completed_round_trips_and_drops_only_the_dangling_tool_use`、`agent_cancel_then_run_works`、`agent_cancel_token_is_cancellable`、`agent_cancel_while_auto_compact_is_in_flight`、`agent_cancel_while_provider_call_is_blocking` 等）：

```bash
cargo test -p yi-agent-core --lib -- --list | grep -i cancel | sort > /tmp/cancel-before.txt
# ... 执行 D1a/D1b/D1c ...
cargo test -p yi-agent-core --lib -- --list | grep -i cancel | sort > /tmp/cancel-after.txt
diff /tmp/cancel-before.txt /tmp/cancel-after.txt && wc -l < /tmp/cancel-after.txt   # 期望 11
```

---

## 5. D2 块：TUI 事件循环（`yi-agent/src/tui/app.rs`）

### 5.1 现状（实测）

生产代码 3,829 行（另有 8,232 行内联测试）。两个函数是问题源：

| 函数 | 行数 | 参数数 | 局部状态 |
|---|---|---|---|
| `run_loop`（L583） | **568** | 19 | **20 个 `let mut`** |
| `handle_key`（L2164） | 356 | **20** | — |
| `execute_slash_command`（L2816） | 423 | — | — |

`TuiChannels` 的文档注释已经自承这件事：「避免每加一个能力就给 `handle_key` 再加一个参数（**它已因此长到 25 个**）」。注意：25 是**引入 `TuiChannels` 之前**的历史峰值，当前实测签名是 **20 个**——说明那次补救削减了参数，但只削减了 channel 那一组，其余仍然裸露。

`run_loop` 里的 20 个局部变量可以分成三类：
- **会话状态**：`current_model`、`current_model_ref`、`effort_override`、`cost_tracker`、`statusbar_state`
- **弹窗状态**：`popup`、`runtime_popup`、`trace_streams`、`runtime_notice_popup`、`subagent_children`
- **循环记账**：`pending_quit`、`queued`、`task_registry`、`previous_viewport`、`notifications`、`process_*`

### 5.2 目标结构

**D2a：把「参数爆炸」变成「上下文对象」**

```rust
/// handle_key / run_loop 共享的只读环境。
pub(crate) struct KeyContext<'a> {
    pub workdir: &'a Path,
    pub mcp: &'a Arc<McpManager>,
    pub config: &'a TuiConfigSnapshot,
    pub catalog: &'a ModelCatalog,
    pub model: &'a str,
    ...
}
```
`handle_key` 从 20 个参数降到 3 个：`(key, &mut KeyState, &KeyContext)`。

**D2b：`run_loop` 的三个可变状态分组**

```rust
struct SessionUi { model, model_ref, effort_override, cost_tracker, statusbar }
struct PopupStack { command, runtime, trace_streams, notice, children }
struct LoopBookkeeping { pending_quit, queued, task_registry, viewport, notifications, process_* }
```
这三个结构体**只承载状态**，不承载行为（行为仍在 `app.rs` 的函数里）。与 D1 相反：D1 禁止打包状态，D2 要求打包状态——差别在于 D1 的状态有严格的作用域时序要求，D2 的状态是「活到下一帧」的 UI 状态。

**D2c：`run_loop` 的事件分派抽成 `dispatch_event`**

现有 `match event { Some(Event::Key(..)) => ..., Some(Event::Paste(..)) => ..., Some(Event::Mouse(..)) => ... }` 抽成独立函数，`run_loop` 只剩「画一帧 → poll 事件 → 分派 → 处理按键结果」。

### 5.3 步骤与验收

| 步 | 动作 | 验收 |
|---|---|---|
| D2a | 引入 `KeyContext`，`handle_key` 参数 20 → 3 | `cargo test -p yi-agent` 全绿 |
| D2b | `run_loop` 状态分三组 | 同上 |
| D2c | 抽 `dispatch_event` | 同上 |

**D2 完成判据**：`handle_key` ≤ 3 个参数；`run_loop` ≤ 250 行且局部 `let mut` ≤ 6 个；`app.rs` 里以 `_for_test` 结尾的测试接缝（现有 4 个：`switch_runtime_tab_for_test`、`handle_runtime_popup_key_for_test`、`daemon_agent_detail_at`、`snapshot_for_tests`）**一个都不能少**。

⚠️ D2 的最大风险：`app.rs` 有 8,232 行内联测试直接构造这些函数。**D2 不搬测试**（那是 C1 的活）。因此改签名会让测试编译失败——这是**预期的**，处理方式是同步更新测试的调用点，**不改断言**。

---

## 6. 四块的并行与合并

```
并行：A ─┐
      B ─┼─→ 各自独立合并，无顺序要求
    D1 ─┤
    D2 ─┘
```

唯一共享风险：A 与 D2 都改 `yi-agent-store` 的调用面——**不成立**，A 不改任何公共签名（R2），所以 D2 看到的 `ipc` API 与今天一致。

合并方式（按项目规范）：每块一个 worktree + 分支，`just ci` 全绿后在 `main` 上 `git merge --no-ff`。

**每块合并前必须额外确认**（这是「只搬不改」的自证）：
```bash
# 公共签名零变化
git diff main -- yi-agent-rs/crates/*/src | grep -E '^[-+]\s*pub (async )?fn' | wc -l   # 应为 0
# 线上协议零变化
git diff main -- yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs | wc -l            # 应为 0
git diff main -- desktop/src/lib/generated/ | wc -l                                        # 应为 0
```

---

## 7. 完成定义（整波）

1. 四块各自合并，`just ci` 在 `main` 上全绿
2. 测试总量**只增不减**：Rust `cargo test --workspace` 通过数 ≥ 基线的 3954；桌面 vitest ≥ 1932
3. 结构指标全部达成：`repository.rs` ≤ 700、`runtime.rs` ≤ 900、`App.tsx` ≤ 2,000、`run_loop`(agent) ≤ 200、`run_loop`(tui) ≤ 250、`handle_key` 参数 ≤ 3
4. `docs/project-management/` 对应模块文件的代码位置引用已同步更新（A 块 → `yi-agent-store.md`；D1 → `yi-agent-core.md`；D2 → `yi-agent-tui.md`；B → `desktop.md`）

---

## 8. 已知风险与明确不做的事

| 风险 | 处置 |
|---|---|
| `App.test.tsx` 631 行 effect 抽取后时序变化 | B1 单独成一步，失败即回退该步，不影响 B2/B3 |
| `ipc.rs` 16 个测试模块搬漏 | A3 用 `--list` 前后 diff 作为门禁 |
| `app.rs` 测试直接构造函数，改签名即编译失败 | 预期内，同步改调用点、不改断言；改不动就说明该步设计有问题 |
| `agent.rs` 的取消语义被改变 | D1 用 `--list | grep cancel` diff 作门禁 |
| 大文件拆分引入的可见性噪音 | 只允许 `pub(crate)` 或 `pub(super)`，不允许出现新的 `pub` 项 |

**明确不做**（这些属于后续波次，本 spec 不碰）：

- `server.rs`（7,559 行）的任何改动 —— 那是 C 线
- `rpc/thread.rs` 的 2,347 行 `handle()` —— 那是 C3
- `rpc_tests.rs`（21,435 行）的拆分 —— 那是 C1
- 工具装配的统一（5 层 `build_tool_setup` 套娃 + 三个入口各拼一遍）—— 那是 C4
- `main.rs`（3,197 行）的拆分 —— 那是 D3，依赖 C4
- `ThreadStore` 可变 + `force()` 架构 —— 有意的设计，不动
- 任何性能优化、任何行为变更
