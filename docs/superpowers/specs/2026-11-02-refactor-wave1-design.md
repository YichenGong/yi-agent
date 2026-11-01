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
> 两个门禁命令**已实测可跑**并写死基线：`ipc` 测试 **56** 个、`agent` 的 `cancel` 测试 **11** 个。

> **第二轮自查（八轮交叉核对，修正 11 处，其中 3 处会让实施直接失败）**
>
> 这一轮把每个数字和每条门禁都拿去实跑验证，发现的问题按严重度排列：
>
> **A. 会让实施直接失败的（3 处）**
> 1. **A3 的测试名 diff 门禁必然失败**。`mod socket_path_tests` 从 `ipc.rs` 搬进 `ipc/server.rs` 后，
>    全名从 `ipc::socket_path_tests::*` 变成 `ipc::server::socket_path_tests::*`——模块路径多一层。
>    我用最小工程实测确认（`sub::alpha_tests::the_test`）。门禁已改为**只比对末段函数名**（`sed 's/.*:://'`）。
> 2. **`ipc` 拆模块后漏了 re-export**。全仓有 **436 处** `yi_agent_store::ipc::Xxx` 引用、跨 8 个 crate，
>    还有插件侧独立复刻的协议。拆成子模块后必须在 `ipc/mod.rs` 逐项 `pub use`，否则大面积编译失败。
>    已把实测的 **25 个必须 re-export 的名字**列进 spec。
> 3. **`repository.rs` 的 7 个顶层自由函数**（`append_event`/`insert_attempt`/`trim_trace_rows`/`migrate` 等）
>    被 impl 方法调用（`trim_trace_rows` 3 次、`migrate` 2 次）。搬进子模块后必须变 `pub(super)`，
>    初稿完全没提，会直接编译失败。
>
> **B. 目标本身不可达的（3 处，全是方向性错误）**
> 4. **`repository.rs` ≤ 700 行做不到**。搬走 3,343 行 impl 后壳文件仍有 **966 行**，其中 696 行是内联测试；
>    测试外移是 C1 的活。改为 **≤ 1,000**（= 理论下限）。
> 5. **`App.tsx` ≤ 2,000 行做不到**。抽走 632 行 effect 后仍有 **2,877 行**，剩下两个巨石
>    `setThreadMode`(654 行) 与 `selectThread`(320 行) 本块明确不抽。改为 **≤ 2,950**。
> 6. **`agent.rs` 「降到 ≤ 1,600 行」在数学上做不到**。阶段抽取是**重新分布**不是删除：
>    THINK(361)+ACT(213)+OBSERVE(30) 搬进新函数还会多出签名开销，总行数只增不减。
>    D1a 消除 6 处重复只净减 **24 行**（1,876→1,852）。判据已改为**形状指标**
>    （`run_loop` ≤ 200 行 + 重复序列从 6 处降到 1 处 + 总行数 ≤ 1,900）。
>
> **C. 数字错误的（5 处）**
> 7. `RuntimeRepository` 方法数是 **101**（100 `pub` + 1 私有），不是 102。
> 8. `impl RuntimeCoordinator` 是 3,069 行（不是 3,066），69 个方法 = 54 `pub` + 15 私有。
> 9. `app.rs` 生产 **3,830 行** / 测试 **8,230 行**（初稿写 3,829 / 8,232）。
> 10. **分组表是猜的，合计只有 87，缺口 14 个方法**。已改为逐名枚举的实测结果（100/100 全部归组）。
>     并且发现 `task` 组会吸收 **26 个方法**——直接把 3,343 行的 impl 换成 800+ 行的 `task.rs`，
>     问题原样搬家。已在 spec 里把它定成**实施硬要求**（再分 create/transition/field 三片）。
> 11. `repository.rs` 测试行是 696（不是 695），壳文件 966（不是 967）；`runtime.rs` 生产 3,910。
>
> **D. 表述不准的（1 处）**
> 12. `_for_test` 测试接缝是 **4 个**，但初稿列错了：`daemon_agent_detail_at` 不是接缝（它是普通生产函数，
>     名字里没有 `_for_test`）。正确 4 个：`snapshot_for_tests`、`catalog_for_tests`、
>     `switch_runtime_tab_for_test`、`handle_runtime_popup_key_for_test`。
>     另补一项实测的爆炸半径：测试模块里 `run_loop` 被调用 **13 次**、`handle_key` **20 次**。
>
> **关于测量方法的一次自我纠正**：这一轮中我一度得到「`runtime.rs` 生产 3,832 行 / 壳文件 763 行」，
> 随后又得出「841 行」。原因是 `runtime.rs` 的测试模块**不在文件末尾**（L3833–4344 之后还有 77 行生产代码），
> 用「首个测试模块之前」估算就会错。最终统一用「逐行标记测试覆盖集合」重算，并对每个结果做
> `wc -l` 与算术自洽双重校验（966 = 270+696 = 4,309−3,343；841 = 330+511 = 3,910−3,069）。
>
> **基线复核**：完整单次跑 `cargo test --workspace --all-features` 得到
> `3954 passed / 0 failed / 27 ignored`（78 个 test binary），与 spec 一致。
> 期间曾出现一次 `3063 passed / 1 failed`，原因是**并发跑了两个 cargo 争构建锁导致的截断**，
> 不是真失败——这正是 CLAUDE.md 里记录过的那类误判。

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
- `yi_agent_store::repository::RuntimeRepository` 的 **100 个 `pub fn`** 全表（外加 1 个 `pub(crate) connection_for_test`，测试用）
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
| `repository.rs` | 4,309 | `impl RuntimeRepository` 单块 **3,343 行 / 101 个方法**（100 `pub` + 1 私有），覆盖约 20 张表 |
| `runtime.rs` | 3,910 | `impl RuntimeCoordinator` 单块 **3,069 行 / 69 个方法**（54 `pub` + 15 私有） |
| `ipc.rs` | 3,745 | 线协议 + socket 服务端 + 客户端 + 队列 + worker 工厂，一个文件 |

关键事实：`RuntimeRepository` 只持有一个字段 `connection: Connection`，且 Rust 允许**子模块写 `impl` 并访问父模块的私有字段**。这意味着切分不需要引入 trait、不需要 newtype、不需要 `Deref`。

`repository.rs` 的 100 个 `pub fn` 按表与语义归组如下。**这是逐名枚举的实测结果**
（100/100 全部归组，无遗漏），不是抽样估算：

| 组 | 代表方法 | 方法数 |
|---|---|---|
| `shell`（留在 `repository.rs`） | `open` / `schema_version` / `has_table` | 3 |
| `schedule` | `create_schedule` / `claim_schedule_occurrence` / `skip_schedule_occurrence` … | 11 |
| `watchdog` | `save_attempt_watchdog_snapshot` / `recover_inflight_tasks` / `reclaim_orphaned_tasks` … | 15 |
| `mailbox` | `record_user_override_message` / `mailbox_messages_for_task` / `request_permission` … | 12 |
| `trace` | `append_trace` / `trace_after` / `trim_trace` … | 6 |
| `application_root` | `record_application_root_attachment` / `reattach_application_root` … | 8 |
| `admission` | `save_provider_turn_grant` / `release_provider_turn_lease` … | 7 |
| `event` | `append_event` / `events_after` / `advance_event_replay_floor_through` … | 6 |
| `read` | `task_snapshots` / `task_detail` / `subscription_snapshot` / `cancel_scope` … | 6 |
| `task` | `create_task*` / `transition_task*` / `task_state` / `set_task_extra_roots` … | **26** |

⚠️ **`task` 组 26 个方法仍嫌大**，实施时应再分两片：
- `task/create.rs`：`create_task*`、`create_child_task*`、`create_attempt`（约 7 个）
- `task/transition.rs`：`transition_task*` 六个变体（状态机，约 6 个）
- `task/field.rs`：`set_task_*`、`task_*` 读写单列字段（约 9 个）
- 其余（`append_event` 之外的事件/查询）归入上面已有的 `event`、`read` 组

这一条是**实施时的硬要求**，不是建议：把 26 个方法整块搬进一个 `task.rs`，只是把一个 3,343 行的
`impl` 换成一个 800+ 行的 `task.rs`，问题原样搬家。

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

A3 的额外验收（防止测试隐身）：拆完后测试**数量**必须与拆前一致，但**名字会变**——`mod socket_path_tests` 搬到 `ipc/server.rs` 后，全名从 `ipc::socket_path_tests::*` 变成 `ipc::server::socket_path_tests::*`。这一点已用最小工程实测确认（`src/sub/mod.rs` 里的 `mod alpha_tests` 报为 `sub::alpha_tests::the_test`）。

⚠️ 因此**直接 diff 全名会必然失败**。正确门禁是**只比对末段测试函数名**（去掉模块路径）：

```bash
cargo test -p yi-agent-store --lib -- --list \
  | grep '^ipc::' | sed 's/.*:://' | sort > /tmp/ipc-before.txt
# ... 执行 A3 ...
cargo test -p yi-agent-store --lib -- --list \
  | grep '^ipc::' | sed 's/.*:://' | sort > /tmp/ipc-after.txt
diff /tmp/ipc-before.txt /tmp/ipc-after.txt
wc -l < /tmp/ipc-after.txt    # 期望 56（实测基线，含 15 个测试模块）
```

注意 `grep '^ipc::'` 依赖模块仍挂在 `ipc` 之下——A3 的拆法是 `ipc.rs → ipc/mod.rs`，所以前缀保留、只多一层 `server::` / `client::` 等。若将来拆出 `ipc` 之外，这条 grep 也要改。

### 2.4 A 块的完成判据（已按「只搬不改 ⇒ 总量守恒」验算过）

⚠️ **先纠正一个初稿里的不可达目标**：初稿写 `repository.rs` ≤ 700 行。这不可能——
把 3,343 行的 impl 整个搬走后，壳文件仍有 `4,309 - 3,343 = 966` 行，其中 **695 行是内联测试**。
测试外移是 C1 的活，不在本波范围。**任何不含测试外移的拆分都无法把这个文件压到 700 行。**

按实际构成重新定判据：

| 文件 | 拆前 | 拆后壳文件 | 判据 | 依据 |
|---|---|---|---|---|
| `repository.rs` | 4,309 生产 | 966 行（270 生产 + 696 测试） | **≤ 1,000** | 3,343 行 impl 全部搬走后的理论下限 |
| `runtime.rs` | 3,910 生产 | 841 行（330 生产 + 511 测试） | **≤ 900** | 3,910 − 3,069(impl) = 841，余量留给子模块声明 |
| `ipc/mod.rs` | 3,745 生产 | 约 1,050（协议类型） | **≤ 1,300** | 协议类型 + 25 个 re-export + 15 个测试模块 |

**这三个数字是「搬完就该长这样」，不是「努力目标」**——若实测偏离，说明搬得不干净或有东西没搬走，应当排查而不是调数字。

外加两条硬门禁：

1. **store 的 344 个测试全绿**（`cargo test -p yi-agent-store`）
2. **公共 API 零变化**：`git diff` 里 `pub fn` 签名行数为 0
3. **`ipc` 的 25 个 `pub` 项必须逐一 re-export**：全仓有 **436 处** `yi_agent_store::ipc::Xxx` 引用（跨 8 个 crate，含插件侧独立复刻的协议）。
   拆成 `ipc/mod.rs` + 子模块后，`mod.rs` 里必须有：
   ```rust
   pub use client::{send_request, send_request_with_version, send_request_with_version_timeout,
                    subscribe, subscribe_with_filters, subscribe_trace, read_task_trace};
   pub use server::{Daemon, TraceSubscription};
   pub use queue::SubscriptionFilters;
   // 协议类型、IpcError、IpcErrorCode、PROTOCOL_VERSION、socket_path_for 等留在 mod.rs
   ```
   实测被外部引用的 25 个名字：
   `Daemon` `IpcError` `IpcErrorCode` `IpcEvent` `IpcPermissionDecision` `IpcRequest` `IpcResponse`
   `IpcTaskDetail` `IpcTaskSummary` `IpcTaskUsage` `IpcTraceRow` `MAX_SOCKET_PATH_BYTES`
   `PROTOCOL_VERSION` `SubscriptionFilters` `TraceSubscription` `clear_plugin_sockets`
   `plugin_socket_for` `read_task_trace` `register_plugin_sockets` `send_request`
   `send_request_with_version_timeout` `socket_path_for` `subscribe` `subscribe_trace`
   `subscribe_with_filters`
4. **`repository.rs` 的 7 个顶层自由函数**（`append_event` / `append_event_with_payload` /
   `append_event_with_actor_payload` / `insert_attempt` / `validate_recovery_context` /
   `trim_trace_rows` / `migrate`）被 impl 方法调用（`trim_trace_rows` 3 次、`migrate` 2 次）。
   搬到子模块后它们必须变 `pub(super)` 或 `pub(crate)`——**这是 R1 允许的可见性收紧，不算设计违反**，但要一并做，否则编译失败。

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

### 3.4 完成判据（已验算）

⚠️ **纠正初稿的不可达目标**：初稿写 `App.tsx` ≤ 2,000 行。抽走 631 行 effect 后仍有 **2,879 行**
（总 3,509 − 631，其中 JSX 占 593 行）。剩下的两个巨石是 `setThreadMode`（**654 行**）与
`selectThread`（**320 行**），它们**本块明确不抽**（见 3.2「明确不做」），所以 2,000 行不可达。

按实际构成重定判据：

| 指标 | 拆前 | 判据 | 依据 |
|---|---|---|---|
| 最大的单个 `useEffect` | **632 行** | **≤ 150** | 四件事拆成三个 hook 后，单块职责单一 |
| `App.tsx` 总行数 | 3,509 | **≤ 2,950** | 3,509 − 631(effect) + 约 70(hook 调用与依赖) ≈ 2,948 |
| `vitest` 用例 | 1,932 全绿 | **≥ 1,932 且全绿** | 只许增不许减 |
| 新增 hook | — | 各带 `.test.ts` | 三个新文件 |

**选 ≤ 2,950 而非更激进数字的理由**：`setThreadMode`（654 行）与 `selectThread`（320 行）是
跨关注点的编排逻辑（会话切换要同时碰 session、workspace、订阅窗口、看板），抽出去只会制造隐式依赖。
它们的体量属于「值得单独讨论」的下一块，不该被本块的数字倒逼着抽。

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
| D1a | 提取 `abort_run`，替换 **6** 处 | `cargo test -p yi-agent-core` 309 全绿 |
| D1b | 抽 `think_phase` | 同上 |
| D1c | 抽 `act_phase` + `observe_phase` | 同上 |

### 4.4 完成判据（已验算）

⚠️ **纠正初稿一个方向性错误**：初稿要求「`agent.rs` 生产代码从 1,876 降到 ≤ 1,600 行」。
**这在数学上做不到，也不该是这块的目标。**

- 阶段抽取是**重新分布**，不是删除。把 THINK(361) + ACT(213) + OBSERVE(30) 抽成三个函数后，
  这些行搬进新函数，还会多出约 15–30 行的签名与返回值传递开销。**总行数只增不减。**
- 本块唯一真正删除重复的是 D1a：6 处 × (5 行 → 1 行) = **净减 24 行**，即 1,876 → **1,852**。

因此判据改为**形状指标**，不再看总行数：

| 指标 | 拆前 | 判据 |
|---|---|---|
| `run_loop` 行数 | 718 | **≤ 200**（抽走三段后理论值约 114，留余量） |
| `run_loop` 里「取消→回滚→冲刷→发事件」序列 | 6 处逐字重复 | **1 处**（只在 `abort_run` 内） |
| `agent.rs` 生产行数 | 1,876 | **≤ 1,900**（允许因签名开销略增，但不得超） |

**取消语义零变化**——`agent::tests` 里所有含 `cancel` 的测试名必须全部保留且通过。该命令已实测可用，基线 **11 个**（`agent_cancel_during_act_emits_cancelled`、`agent_cancel_during_act_rolls_back_session`、`agent_cancel_during_think_emits_cancelled`、`agent_cancel_keeps_completed_round_trips_and_drops_only_the_dangling_tool_use`、`agent_cancel_then_run_works`、`agent_cancel_token_is_cancellable`、`agent_cancel_while_auto_compact_is_in_flight`、`agent_cancel_while_provider_call_is_blocking` 等）：

```bash
cargo test -p yi-agent-core --lib -- --list | grep -i cancel | sort > /tmp/cancel-before.txt
# ... 执行 D1a/D1b/D1c ...
cargo test -p yi-agent-core --lib -- --list | grep -i cancel | sort > /tmp/cancel-after.txt
diff /tmp/cancel-before.txt /tmp/cancel-after.txt && wc -l < /tmp/cancel-after.txt   # 期望 11
```

---

## 5. D2 块：TUI 事件循环（`yi-agent/src/tui/app.rs`）

### 5.1 现状（实测）

生产代码 **4,061 行**（另有 **8,000 行**内联测试）。两个函数是问题源：

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

### 5.4 完成判据（已按实测清单核对）

| 指标 | 拆前 | 判据 |
|---|---|---|
| `handle_key` 参数个数 | **20** | **≤ 3**（`key` + `&mut KeyState` + `&KeyContext`） |
| `run_loop` 行数 | 568 | **≤ 250** |
| `run_loop` 局部 `let mut` | 20 | **≤ 6**（三组状态各一个 + 少量真正的循环局部量） |

**生产代码里的测试接缝现存 4 个，一个都不能少**（`app.rs` 内实测）：
`snapshot_for_tests`(L382)、`catalog_for_tests`(L408)、`switch_runtime_tab_for_test`(L1204)、
`handle_runtime_popup_key_for_test`(L1465)。

> 初稿把 `daemon_agent_detail_at` 也算作接缝，**这是错的**——它在 L3244，是普通生产函数，
> 名字里没有 `_for_test`，只是恰好被测试调用。上面的清单是 `grep 'fn .*_for_test'` 的实测结果。

⚠️ 改签名的爆炸半径（实测）：`app.rs` 的测试模块里 **`run_loop` 被调用 13 次、`handle_key` 被调用 20 次**。
这 33 处调用点必须随签名同步更新——**只改调用点，不改断言**。

⚠️ D2 的最大风险：`app.rs` 有 8,000 行内联测试直接构造这些函数（33 处调用点）。**D2 不搬测试**（那是 C1 的活）。因此改签名会让测试编译失败——这是**预期的**，处理方式是同步更新测试的调用点，**不改断言**。

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
2. 测试总量**只增不减**：Rust `cargo test --workspace --all-features` 通过数 ≥ **3954**（实测基线，单次完整运行为 `3954 passed / 0 failed / 27 ignored`）；桌面 `npx vitest run` ≥ **1932**（146 文件）
3. 结构指标全部达成：

| 块 | 指标 | 拆前 | 目标 |
|---|---|---|---|
| A | `repository.rs` | 4,309 生产行 | ≤ 1,000（理论下限 966） |
| A | `runtime.rs` | 3,910 生产行 | ≤ 900 |
| A | `ipc/mod.rs` | 3,746 生产行 | ≤ 1,300 |
| B | `App.tsx` | 3,509 行 | ≤ 2,950 |
| B | 最大单个 `useEffect` | 632 行 | ≤ 150 |
| D1 | `run_loop`(agent) | 718 行 | ≤ 200 |
| D2 | `run_loop`(tui) | 568 行 | ≤ 250 |
| D2 | `handle_key` 参数 | 20 | ≤ 3 |
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
