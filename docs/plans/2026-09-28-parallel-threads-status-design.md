# 并行多 thread 执行 + 每 thread 状态指示 设计与实现方案

- 日期：2026-09-28
- 状态：设计定稿，待实现
- 模块：`desktop`（Tauri GUI）+ `yi-agent-app-server`（协议 / 状态）
- 关联：`docs/project-management/desktop.md`、`docs/project-management/yi-agent-app-server.md`、
  `docs/superpowers/specs/2026-09-26-desktop-gui-p1-persistence-design.md`

## 1. 背景与现状

诉求两条：

1. **并行多 thread**：在一个 thread 起任务后，可以切到另一个 thread 继续干活 / 查看进度，
   多个 thread 同时跑，提升效率。
2. **状态指示**：某个 thread 跑完或在等用户确认时，侧栏冒出一个点，让用户知道何时去确认。

现状（关键结论：**后端已经并行，瓶颈全在前端与协议**）：

- **后端已并行**。每个 thread 一个独立 driver task（`server.rs:447` 新建、`server.rs:599`
  resume），turn 只在**同一 thread 内**串行；不同 thread 的 turn 本就并发。全流程无全局
  执行锁。输出层是单个 `Arc<MessageWriter>`（内部 mutex 串行），并发通知按行交错但每行是
  完整 JSONL 且带 `thread_id`。
- **前端被写死成单 thread**：
  - 全 app 只有一个 `Session`（`App.tsx:45`），且 `Session.apply()` **不按 `thread_id`
    过滤**（`session.ts:42`），`RpcClient.onMessage` 也无差别扇出（`rpc.ts:101`）。
  - 全局 `busy = session.turnActive`（`App.tsx:58`）把切换 / 改名 / 删除全禁用
    （`App.tsx:88,120,166,219`；`ThreadSidebar.tsx:140,148,180,216,277,288,324,336`）。
  - 只有一个审批槽 `approval`（`App.tsx:52`）+ 全局 `inert`（`App.tsx:313`）；
    并行后**多个 thread 可能同时待审批**，一个槽装不下。
- **协议无 thread 级状态**。`thread/list` / `thread/listAll`（`server.rs:296`、`server.rs:327`）
  只返回 id/cwd/model/title/时间戳；"在跑 / 待审批"只存在于内存
  （`session.rs:19` 的 `active_turn_id`、`server.rs:138` 的 `pending` map），线上不可见。
- **`thread/resume` 会打断正在跑的 turn**：`server.rs:517` 先
  `interrupt_and_wait_for_persist`，随后 `server.rs:582` 直接 `threads.insert` 覆盖
  `ThreadSession`，**孤立旧 driver**。这是本特性的最大暗礁（见 §3.1 约束 1）。
- 路线图：`docs/project-management/desktop.md:71` 的"多 thread 标签页"仍是 `[ ]`；
  `docs/superpowers/specs/2026-09-26-desktop-gui-p1-persistence-design.md:209` 明确写着
  "切换 thread 不并发（一次一个）"——本设计取代该约束。

## 2. 需求澄清（已确认的决策）

| # | 决策点 | 结论 |
|---|--------|------|
| 1 | 状态呈现 | **持续状态徽标 + 注意力点**：侧栏每 thread 常显当前状态；未被查看的 thread 完成 / 待确认时额外亮一个点，打开后清除 |
| 2 | 跨线程审批提醒 | **侧栏徽标 + 顶部可关闭全局横幅**（带「跳过去」）；**超时行为不变**（后台 300s 自动拒绝） |
| 3 | 侧栏排序 | **不改排序**（仍按 `updated_at` 倒序、按 workspace 分组），只加徽标 / 点 |
| 4 | 状态归属 | **服务端权威**：协议新增 `ThreadStatus`，服务端在跃迁点推送，`thread/listAll` 带快照；未读点纯前端 |

## 3. 设计

### 3.1 总体架构与硬约束

范围：只动 `yi-agent-app-server`（协议 + 状态）与 `desktop/`（前端）。TUI / CLI 不在范围内。

三条硬约束：

1. **"切换视图"与"resume"必须解耦**。切到 **warm**（本次 app 启动后已打开过）的 thread
   时**绝不调用 `thread/resume`**，只切视图、复用其内存 `Session`。只有 **cold** thread
   才 resume，而 cold thread 不可能有活跃 turn，故安全。这样绕开 §1 的 resume 打断陷阱。
2. **前端状态按 thread 拆分**：`Map<thread_id, Session>`，通知按 `params.thread_id` 路由。
   后台 thread 的通知照常累积进它自己的 Session，切回去即最新。
3. **审批按 thread 存储**，全局 `inert` 去掉，侧栏保持可点——否则用户无法切到"待确认"
   的 thread 去响应。

### 3.2 服务端 / 协议

**新增 `ThreadStatus`（线上三态）**：`idle | running | awaiting_approval`。刻意**不含
`failed`**——失败是事件不是状态（失败后 thread 立即回 `idle`）；"失败未读"属前端注意力
概念，前端从 `turn/completed.params.status`（`completed|interrupted|failed`）自行上色。

**状态存储**：`ThreadSession`（`session.rs:15`）新增 `status: Arc<Mutex<ThreadStatus>>`
（默认 `Idle`），driver spawn 时 clone 一份传入。**选共享句柄而非走 `TurnEvent`**，是为了
不碰 `interrupt_and_wait_for_persist` 的脆弱逻辑：它那个 `while let Some(TurnEvent::Finished{..})`
（`server.rs:1003`）一旦收到新变体会**静默吞掉并跳出**，会连带破坏 resume / delete 的
"等落盘"保证。状态走共享句柄后，该函数与 `turn_tx` 保持原样。

**状态跃迁**（唯一实时写入者是 driver；主循环在 turn/start 受理时置 `running` 兜底，
保证 driver 尚未取走 prompt 的窗口内状态也正确）：

| 时机 | 位置 | 目标状态 |
|---|---|---|
| turn/start 受理 | `server.rs:830` 附近 | `running` |
| driver 收到 `PermissionRequest` | `server.rs:1095` | `awaiting_approval` |
| 决定返回 / 超时 / 被中断 | `server.rs:1126` 决定循环退出后 | `running` |
| turn 结束（落盘前） | `server.rs:1216` 之前 | `idle` |

**线上新增**：

- 通知 `thread/status/updated { thread_id, status }`，由 driver 直接写（它已持有
  `Arc<MessageWriter>`），插在既有通知序列中。
- `thread/list`（`server.rs:304`）与 `thread/listAll`（`server.rs:342`）每条 thread 增加
  `status` 字段：内存中的 thread 取共享句柄，cold thread（不在 `threads` map，`server.rs:145`）
  一律 `idle`。**使前端重载 / 重连后状态依然正确**。
- `desktop/src/lib/protocol.ts` 同步：新增 `ThreadStatus` 类型、`ThreadSummary.status`
  （`protocol.ts:102`）、`thread/status/updated` 通知变体（`protocol.ts:43`）。
- Tauri bridge（`desktop/src-tauri/src/bridge.rs`）**无需改动**：通知 / 反向请求已按其万能
  分类转发；`ApprovalRequest` 已带 `params.thread_id`（`protocol.ts:85`，服务端
  `server.rs:1107` 发送）。

### 3.3 前端状态重构

`App.tsx` 状态模型（核心是"每 thread 独立"）：

- `sessions: Map<thread_id, Session>`（ref 持有）
- `warm: Set<thread_id>`——本次 app 启动后已打开过
- `liveStatus: Map<thread_id, ThreadStatus>`——来自 `thread/status/updated` 流
- `approvals: Map<thread_id, ApprovalRequest>`——每 thread 至多一个待处理审批
- `unread: Set<thread_id>`——注意力点
- `threadInfos: Map<thread_id, {cwd, model}>`——每 thread 各自的 cwd/model
- `currentThreadId`

**状态读取合流**：`status(id) = liveStatus.get(id) ?? groups 里的 status ?? "idle"`。
启动快照（`thread/listAll` 带 status）+ 实时流两路都对，重载后也正确。

**通知路由**：`App` 订阅 `RpcClient.onNotification`，按 `params.thread_id` 分发到对应
`Session`。无 `thread_id` 的 `error` 归当前 thread。`thread/started` 同时写入 `threadInfos`。

**选择 thread（关键）**：

```
selectThread(id):
  切 currentThreadId; 清 unread[id]
  若 warm 已含 id → 只切视图，不发任何 RPC
  否则 → resume 一次，标记 warm，把回放通知灌进新建的 Session
```

**全局闸门下沉到 per-thread**：

- 选择 thread：永远允许
- rename：永远允许（服务端有 `meta_lock`，`thread_store.rs:78`）
- delete：允许；服务端已有 `interrupt_and_wait_for_persist` 兜住活跃 turn
  （`server.rs:780`）。删除后清理该 id 的
  `liveStatus / approvals / unread / sessions / threadInfos`，若正在看则切空态
- new / browse：永远允许
- 只保留 `inFlightResume: Set<thread_id>` 防止同一 cold thread 被重复 resume

**审批**：

- `onApproval(r)`（`App.tsx:260`）→ 写入 `approvals[r.params.thread_id]`
- 当前 thread 有待审批 → 显示模态框（`ApprovalDialog`，`App.tsx:348`）
- 其它 thread 有待审批 → 全局横幅 `"Thread X 需要确认 [跳过去]"`（可并列多条）
- **去掉全局 `inert`**（`App.tsx:313`），改为只把模态罩在对话区，侧栏保持可点
- 决定回传后从 `approvals` 删除

**注意力点**：`turn/completed` 且 thread ≠ `currentThreadId` → 加入 `unread`，
颜色由 `params.status` 决定（`completed` 蓝 / `failed` 红 / `interrupted` 灰）。

### 3.4 UI 呈现

**侧栏每行**（`ThreadSidebar.tsx:134` `renderThread`）两处标记，语义正交：

- **状态徽标**（贴标题前，`ThreadSidebar.tsx:164` 区）：`running` → 旋转 spinner；
  `awaiting_approval` → 琥珀色实心点；`idle` → 不显示。数据来自服务端 `status`。
- **注意力点**（贴行尾、`relativeTime` 区，`ThreadSidebar.tsx:176`）：仅当 ∈ `unread`。
  打开该 thread 即清除。

"运行中且未被查看"会同时出现 spinner 与点——两者都成立。当前查看的 thread 不产生点。

**全局横幅**（App 顶部，可关闭）：列出所有 `awaiting_approval` 的 thread + `[跳过去]`
（= `selectThread(id)`）。只提示、不抢焦点；关闭后保持隐藏，直到**新的**审批到达再弹。

**状态栏 / 输入区**：`StatusBar` 显示当前 thread 的 cwd/model/usage；`MessageInput`
的 Send↔Stop 仍由当前 thread 的 `session.turnActive` 决定——天然 per-thread，无需改逻辑。

**排序**：不动。

### 3.5 边界与风险

- **输出层已安全**：所有 driver 写同一 `MessageWriter`（内部 mutex），并发通知按行交错但
  每行完整且带 `thread_id`，前端按行解复用即可。无需改输出层。
- **审批超时黑洞（必须处理）**：审批超时后服务端自动 `Deny`（`server.rs:1124`），但服务端
  **不发"审批已解决"通知**。若不管，前端 `approvals` 里那条会永远留着、模态框还开着，用户
  一点就报"无待处理审批"。解法：**当某 thread 状态离开 `awaiting_approval`（回 `running`
  或 `idle`）时，前端清掉该 thread 的审批**。超时 / 中断 / 正常决定三条路径全覆盖。
- **每 thread 至多一个待审批**：driver 同一时刻只 await 一个决定（`server.rs:1126`），
  故按 thread 存单条正确；跨 thread 的多个由横幅并列。
- **删除运行中的 thread**：服务端已安全；前端须清理其全部状态条目（见 §3.3）。
- **前端 HMR 重载（dev-only 限制）**：webview 热重载会丢 `warm` 集合；若此时后台有 turn
  在跑，用户点它会被当 cold thread 去 resume → 服务端 `thread/resume`（`server.rs:517`）
  会打断它。生产环境重载 = 整个 app 重启 = sidecar 重建（无活跃 turn），不受影响。
  记录为已知限制，不为此改服务端 resume 语义。
- **`interrupt_and_wait_for_persist` 不碰**：状态走共享句柄、不走 `turn_tx`，`server.rs:1003`
  的 `while let` 保持原样。
- **列表刷新频率**：任意 thread 的 `turn/completed` 都会 `refreshThreads`（`App.tsx:258`）；
  并行下变频繁。仍是本地文件列举，可接受，必要时去抖。

## 4. 非目标（YAGNI）

- TUI / CLI 的并行与状态展示。
- 未读状态的跨重启持久化（`unread` 仅活在内存）。
- 侧栏改排序 / "需处理"独立区（决策 3 明确不改）。
- 延长或取消审批超时（决策 2 明确保持 300s）。
- 服务端 `thread/resume` 语义改造（如 warm thread 幂等 resume）——用前端 warm 集合规避。
- 后台 thread 完成时弹系统通知 / 抢焦点。

## 5. 测试

**Rust（`yi-agent-app-server` 内联 `mod tests`，复用现有 harness）**

现有 harness 已具备注入点：`run_with(reader, writer, cfg, permission_timeout, workspaces,
build_agent)`（`server.rs:89`）——可注入 mock provider 与**很短的 `permission_timeout`**，
测试经真实 JSON-RPC 管道驱动。

协议单测（`protocol.rs`）：`ThreadStatus` 序列化成 `idle|running|awaiting_approval`；
`thread/status/updated` 信封形状。

服务端集成测试（多 thread 走同一根管道）：

1. **并行证明**：thread A 用 `SlowProvider`（`server.rs:1379`，永不结束）起 turn，紧接着
   thread B 也起一个 → 两边都收到 `turn/started`，且此刻 `thread/listAll` 里两者 status
   **同为 `running`**（证明无跨 thread 串行 / `-32012`）。
2. **状态序列**：mock turn 期间收到 `thread/status/updated`：`running` →（完成）→ `idle`。
3. **审批闭环**：provider 触发 `PermissionRequest` → status = `awaiting_approval` →
   客户端回决定后回 `running` → turn 结束 `idle`。
4. **超时路径**：`run_with` 传极短 `permission_timeout` → 断言 status **离开**
   `awaiting_approval`（正是 §3.5 前端清审批的依据）。
5. **中断**：`turn/interrupt` 后 status 回 `idle`。
6. **cold thread**：未被 resume 的持久化 thread 在 `listAll` 里 status = `idle`。
7. **回归**：现有 `interrupt_and_wait_for_persist` / driver 相关测试保持绿。

**前端（vitest + jsdom + testing-library）**

- 新增 per-thread 状态模块单测：通知按 `thread_id` 隔离（写 B 不动 A 的 items）；
  `turn/completed` 在非当前 thread → 进 `unread`、在当前 thread → 不进；`selectThread`
  清 `unread`；`status/updated` 更新徽标；status 离开 `awaiting_approval` → 清该 thread
  的审批。
- 扩 `desktop/src/components/ThreadSidebar.test.tsx`：`running` 出 spinner、
  `awaiting_approval` 出琥珀点、`unread` 出点；**busy 时不再禁用选择**。
- 扩 `desktop/src/App.test.tsx`：并行下切换 warm thread **不触发 `thread/resume`**；
  无全局 `inert`；后台审批出现横幅、点"跳过去"能切视图。现有把"单 session / busy 禁用"
  当前提的断言需一并改。

Mock 足够覆盖，不涉及真实 LLM 分层测试。

## 6. 文档同步（CLAUDE.md 要求）

- `docs/project-management/desktop.md`：把 `:71` 的"多 thread 标签页"从 `[ ]` 转 `[x]`
  （带 `file:line` 判据）；更新前端单测计数；补一条状态徽标 / 注意力点的 Feature。
- `docs/project-management/yi-agent-app-server.md`：登记 `ThreadStatus` /
  `thread/status/updated` / `listAll` status 字段。
- `docs/superpowers/specs/2026-09-26-desktop-gui-p1-persistence-design.md:209`：修订
  "切换 thread 不并发"的旧约束，指向本设计。
- `README.md`：模块索引计数若变化则同步。

## 7. 实现顺序建议

1. 协议 + 服务端状态（`protocol.rs` / `session.rs` / `server.rs`）+ Rust 测试。
2. 前端 per-thread 状态模块（`session.ts` 拆分 / 新增路由）+ 单测。
3. `App.tsx` 接线（路由、`warm`、`selectThread`、审批表、去 `inert`）。
4. `ThreadSidebar` / 横幅 UI + 组件测试。
5. 文档同步。
