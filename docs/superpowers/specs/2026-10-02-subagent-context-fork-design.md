# Subagent fork 调用者上下文（`spawn_agent` 的 `fork` 选项）设计

**目标：** 让 `spawn_agent` 支持一个**显式、按需**的 `fork` 选项。开启时，子
agent 的会话以**调用者此刻的完整对话**为前缀初始化，再追加自己的 objective；
子 agent 因此无需重新获取上下文。默认不开启，行为与今天逐字节一致。

**状态：** 设计已确认，待转实现计划。

**相关文档：**
`docs/superpowers/specs/2026-08-09-subagent-architecture-design.md`（root → child →
leaf、窄 objective 的委派模型）、
`docs/superpowers/specs/2026-08-09-runtime-daemon-design.md`（daemon 生命周期与
IPC 协议）、
`docs/superpowers/specs/2026-09-28-ipc-response-frame-truncation-design.md`
（1 MiB 帧上限的来源与保留理由）、
`docs/superpowers/specs/2026-10-01-per-session-subagent-root-design.md`（一对话一
root 的 `ThreadRoot` / `RuntimeBinding` 防过期惯例）、
`docs/superpowers/specs/2026-09-29-subagent-orchestration-capabilities-design.md`
（六个委派工具）。

---

## 1. 背景与现状（已核实）

### 1.1 今天的子 agent 拿不到任何调用者上下文

- 委派工具只把 **objective 字符串**发给 daemon。两个 spawn 实现——root 用的
  `DaemonApplicationSpawnAgentTool`（`yi-agent-subagent/src/lib.rs:1457` schema，
  `:1520` 发 `SpawnApplicationChild`）与子 agent 用的 `DaemonSpawnAgentTool`
  （`yi-agent-subagent/src/lib.rs:1820`，`:1881` 发 `SpawnChild`）——都只持有
  `root: Arc<ThreadRoot>`，**够不到调用者的对话**。
- 子 worker 的首个 prompt 就是那个 objective：`yi-agent-subagent/src/lib.rs:665`
  `let mut prompt = objective;`，worker 用全新的 `Session` 起步
  （`lib.rs:646` `Agent::new(...)`，未 `with_session`）。

### 1.2 调用者的对话在两个不同的地方，daemon 都看不见

- **root 调用时**：root 的会话在 TUI/app-server 进程内的 `Agent.session()`
  （`yi-agent-core/src/agent.rs:527`，`Session` 定义在 `agent.rs:39`）。daemon
  只调度 root 的 children，自己不跑 root 的 LLM 循环——`activate_application_root`
  仅 set objective 并同步状态（`yi-agent-store/src/runtime.rs:1003`）。
- **子 agent 调用时**：子 agent 的会话在 daemon 内那个 worker 线程的 `Agent` 里
  （`yi-agent-subagent/src/lib.rs:646`）。

两种情况下，`spawn_agent` 工具都读不到「调用者此刻的 messages」。

### 1.3 子 agent 没有专属 system prompt

worker 工厂在 `yi-agent-subagent/src/lib.rs:568-574` 组装子 worker 的 config，
system prompt 直接取自**同一个 skills catalog 句柄**，与 root 逐字相同：

```rust
let mut config = self.config.clone();
config.model = self.worker_config_model(&request.model);
if let Some(catalog) = &self.catalog {
    if let Some(prompt) = catalog.current_system_prompt() {
        config.system_prompt = Some(prompt);
    }
}
```

该 prompt 由 `yi-agent-runtime/src/bootstrap.rs:420-428` 的 `resolve_system_prompt`
（`AgentConfig::default_system_prompt()` + 用户指令 + 当天日期）与
`SkillsCatalogHandle::current_system_prompt`（追加 skills catalog）拼成。全局搜
`You are a sub` / `child agent` / `delegated agent` 无专属提示词命中。

**结论：父子提示词字面相同。** 子 agent 与 root 的差异只有两点：首个 prompt 是
窄 objective；工具集不同（子 agent 拿六个委派工具，read-only 子 agent 还缺
`write`/`edit`，见 `lib.rs:614-636` 与 `worker_tool_registry`）。

### 1.4 协议与 transcript 的两条硬约束

- **IPC 单帧上限 1 MiB**：`yi-agent-store/src/ipc.rs:38`
  `const MAX_FRAME_BYTES: usize = 1024 * 1024;`。长会话历史极易超限，故不能把
  历史塞进单个 spawn 请求。
- **transcript 不允许悬空 `tool_use`**：`yi-agent-core/src/agent.rs:668-690` 的
  `safe_cancel_truncate_len` 明确——含 `tool_use` 的 assistant 消息后面必须有配对
  的 `tool_result`，否则下一次 provider 请求会被拒。而 `spawn_agent` 正是**在父
  turn 内**被调用的：此刻父 session 末尾是 `assistant(tool_use: spawn_agent)`，
  其 `tool_result` 尚未产生。原样 fork 会天然携带悬空 `tool_use`。

---

## 2. 已确认的设计决策

| # | 议题 | 决策 |
|---|---|---|
| 1 | fork 语义 | **完整复制**调用者对话（非摘要、非片段） |
| 2 | 触发 | **显式可选**，只在需要时用；默认为今天的窄 objective |
| 3 | fork 谁的对话 | **直接调用者**（root 调用则 fork 用户会话；子 agent 调用则 fork 它自己，天然累积成继承链） |
| 4 | 载荷大小 | **全量、不按大小截断**；分块/流式传输，突破 1 MiB 单帧上限 |
| 5 | 触发方式 | `spawn_agent` 新增可选布尔参数 `fork`，默认 `false` |
| 6 | 传输通道 | 在**现有 socket** 上做分块上传（不落盘，不新开文件信任边界，不放开大帧） |
| 7 | 接线 | **活上下文句柄**（`CallerContext`），每次调用解析「当前」对话，沿用 `ThreadRoot`/`RuntimeBinding` 防过期惯例 |
| 8 | 落点 | `[父历史] + [objective]` 追加在末尾，子 agent 在同一视角继续 |
| 9 | 末端裁剪 | 丢弃末尾含自身 `tool_use` 的 assistant 消息，fork 起点落在最后一个「已配对」检查点（**协议有效性裁剪，非按大小截断**） |

**内存保护（补充确认）：** daemon 组装缓冲区设**可配置上限**（默认 32 MiB）。
超限时**明确报错拒绝 spawn**，绝不静默截断——即「要么完整 fork，要么明确失败」。

---

## 3. 总体设计

### 3.1 三条新接线

本次改动的实质是补三条缺失的接线：

| 接线 | 位置 | 作用 |
|---|---|---|
| `Agent::session_handle()` / `with_session_arc()` / `Session::from_messages()` | `yi-agent-core/src/agent.rs` | 暴露**活的** `Arc<Mutex<Session>>`；重建 Agent 时复用同一个 Arc（防过期）；允许用既有消息预置会话 |
| `CallerContext` 活句柄 | `yi-agent-subagent` | 工具持稳定句柄，spawn 时读取「当前」对话；沿用 `ThreadRoot`/`RuntimeBinding` 惯例 |
| 分块上传协议 | `yi-agent-store/src/ipc.rs` + `runtime.rs` | 把任意大小的父历史经现有 socket 送到 daemon |

### 3.2 数据流（`fork: true`）

```text
调用者 turn 内调用 spawn_agent{fork:true}
  -> 工具经 CallerContext 解析「调用者当前 Session」（已做 checkpoint 裁剪）
  -> daemon: BeginForkUpload(total_bytes) -> fork_token
  -> daemon: AppendForkChunk(fork_token, seq, bytes) x N
  -> daemon: SpawnApplicationChild{fork_token, objective, ...}
       daemon 组装 Vec<Message>，校验完整性
  -> WorkerStart{fork_messages: Some(msgs), objective}
  -> worker: Session::from_messages(msgs) 预置，再 run(objective)
```

`fork: false` 完全走原路径，不产生任何上传请求。

---

## 4. 详细设计

### 4.1 core：会话句柄与预置

`yi-agent-core/src/agent.rs`：

- 新增 `pub fn session_handle(&self) -> Arc<Mutex<Session>>` —— 返回 `session`
  字段的克隆（`Arc`），供工具在**调用时**读取当前对话。
- 新增 `pub fn with_session_arc(self, session: Arc<Mutex<Session>>) -> Self` ——
  与既有 `with_session(Session)`（`agent.rs:481`）并列，但**复用传入的 Arc**
  而不是新建。重建 Agent 时必须用它，才能让 `CallerContext` 指向的句柄继续有效。
  现有 `with_session` 的签名与语义**保持不变**，避免波及已有调用点。
- `Session` 新增 `pub fn from_messages(messages: Vec<Message>) -> Self`（复用
  `agent.rs:65` 的 `replace_messages` 语义）。
- 裁剪：新增一个可复用函数（从 `safe_cancel_truncate_len` 提炼，`agent.rs:678`），
  输入 `&Session` 输出「可安全前缀长度」，供 fork 与 cancel 两处共用。

**不变量：** 同一个 thread 的 `Arc<Mutex<Session>>` 在**所有重建路径**上保持稳定。
app-server 在 compact、换工具集时会重建 `Agent`
（`server.rs:777-809`、`3242`、`3583-3625`），TUI 同理；这些路径只要经由
`with_session_arc(agent.session_handle())` 重建，句柄就永不过期。

### 4.2 subagent：`CallerContext` 活句柄

`yi-agent-subagent` 新增一个轻量类型：

```rust
pub struct CallerContext { /* 内部：Arc<Mutex<Option<Arc<Mutex<Session>>>>> */ }
impl CallerContext {
    pub fn bind(&self, session: Arc<Mutex<Session>>);
    pub fn snapshot(&self) -> Option<Vec<Message>>; // 读取当前并做 checkpoint 裁剪
}
```

- 工具（`DaemonApplicationSpawnAgentTool`、`DaemonSpawnAgentTool`）新增
  `caller: CallerContext` 字段。
- 装配点（三处）在**注册委派工具之后**绑定调用者的 session 句柄：
  - TUI：`yi-agent/src/tui/subagents.rs:150`、`:159` 附近的注册路径；
  - app-server：`yi-agent-app-server/src/server.rs:706` 的
    `register_attached_root_tools_in_thread`；
  - worker 工厂：`yi-agent-subagent/src/lib.rs:614-636` 注册子 worker 工具处
    （子 agent fork 自己时用）。
- `spawn_agent` 收到 `fork: true` 时调用 `caller.snapshot()`；返回 `None`（句柄
  未绑定）则**显式报错**，不回退到静默的无 fork 行为。

### 4.3 工具签名

`spawn_agent` 的 schema 增加：

```json
"fork": {
  "type": "boolean",
  "default": false,
  "description": "Fork the caller's current conversation into the child so it inherits context. Off by default: pass true only when the child genuinely needs the history."
}
```

`required` 仍为 `["task"]`，`additionalProperties: false` 不变。两个 schema 副本
（`lib.rs:1457`、`lib.rs:1820`）都必须同步，并各自加一个「schema 含 fork 且默认
false」的断言测试。

### 4.4 IPC：分块上传协议

`yi-agent-store/src/ipc.rs` 的 `IpcRequest` 新增（全部带 capability 鉴权）：

```rust
BeginForkUpload {
    session_id: String,
    caller_task_id: String,
    capability: String,
    total_bytes: u64,
},                                          // -> ForkUploadStarted { fork_token }
AppendForkChunk {
    fork_token: String,
    seq: u64,
    bytes: Vec<u8>,
},                                          // -> ForkChunkAccepted { received: u64 }
AbortForkUpload { fork_token: String },     // -> ForkUploadAborted
SpawnApplicationChild { /* 既有字段 */, fork_token: Option<String> }  // 增量
SpawnChild            { /* 既有字段 */, fork_token: Option<String> }  // 增量
```

- 新增字段一律 `#[serde(default)]`，老客户端省略即 `None`，向后兼容
  （与 `thread_id` 的既有做法一致，`ipc.rs:218-222`）。
- **chunk 载荷编码（已定）：** `AppendForkChunk.bytes` 为 `String`，内容是父历史
  UTF-8 字节的 **base64** 分片。整个 payload 先把 `Vec<Message>` 序列化为一个
  JSON 字符串，再取其 UTF-8 字节流按固定大小（**256 KiB**）切分。不直接用
  `Vec<u8>`，因为 serde 会把字节数组编成 JSON 数字数组，体积膨胀约 4 倍。
- 组装完成后按同一格式反序列化回 `Vec<Message>`（`Message` 已实现
  `Serialize`/`Deserialize`，`yi-agent-core/src/message.rs:15`）。
- **每个 chunk 远小于 1 MiB**，故 `MAX_FRAME_BYTES` 与既有帧截断修复
  （`2026-09-28-ipc-response-frame-truncation-design.md`）完全不动。

### 4.5 daemon 侧：上传状态与组装

`yi-agent-store/src/runtime.rs` 的 `RuntimeCoordinator` 维护：

```text
fork_uploads: Mutex<HashMap<String /*fork_token*/, ForkUpload>>
struct ForkUpload { session_id, caller_task_id, expected_bytes, received: Vec<u8>, deadline }
```

- `BeginForkUpload`：校验 capability 属于 `caller_task_id`，`total_bytes` 不超过
  **可配置上限**（默认 32 MiB），分配 token 并记录 deadline。
- `AppendForkChunk`：校验 token 存在、`seq` 从 0 起**严格递增且恰好等于当前
  已收 chunk 数**（既防重复投递也防乱序），累计不超过 `expected_bytes`。
- **上限的配置位置（已定）：** 该上限是 daemon 侧的进程配置，经
  `RuntimeConfig`（`yi-agent-runtime/src/config.rs`）读取环境变量
  `YI_AGENT_FORK_MAX_BYTES`，缺省 32 MiB；与 `YI_AGENT_RUNTIME_DIR` 等同属
  「项目 runtime 的启动配置」，不下沉到 IPC 请求里。
- `SpawnApplicationChild` / `SpawnChild` 收到 `fork_token`：要求上传**完整**
  （`received.len() == expected_bytes`）且属于同一 `session_id`/`caller_task_id`；
  反序列化为 `Vec<Message>`；在提交 spawn 后**消费并移除**该 token（一次性）。
  不完整 / 未知 / 过期 token 一律拒绝，不回退。
- 清理：`AbortForkUpload` 立即移除；deadline 到期由现有后台巡检回收；
  spawn 成功或失败后移除，避免泄漏。

### 4.6 worker 侧装配与顺序

- `WorkerStart`（`yi-agent-core/src/subagent/worker.rs:68`）新增
  `fork_messages: Option<Vec<Message>>`（+ `with_fork_messages` builder）。
- worker 工厂 `start_with_provider_turn_gate`（`lib.rs:543`）把
  `fork_messages` 传入线程闭包；`Agent::new` 之后用 `with_session`/`from_messages`
  预置会话。
- **顺序（明确）：** fork 前缀 → `initial_user_messages`（`worker.rs:86`）→
  objective。objective 始终是最后一条 user 消息。

### 4.7 末端裁剪（协议有效性）

fork 快照前，对调用者 session 施加与 `safe_cancel_truncate_len`
（`agent.rs:678`）相同的规则：若末尾是含 `tool_use` 的 assistant 消息，则丢弃它，
使前缀以「已配对」的检查点结尾。随后追加 objective。

**这不是对决策 4「不按大小截断」的违背**：历史内容一条不少，只移除那条**尚未
完成、且会令 provider 拒绝整个请求**的工具调用尾巴。

---

## 5. 错误处理（全部显式，绝不静默）

| 情形 | 行为 |
|---|---|
| `fork:true` 但 `CallerContext` 未绑定 | 工具返回错误「fork requested but no caller context available」 |
| 上传不完整 / 超时 / token 过期 | spawn 被拒，返回明确错误码 |
| `total_bytes` 超可配置上限 | `BeginForkUpload` 即拒绝，不进入传输 |
| capability 不匹配 / 非调用者 | 拒绝（沿用既有 capability 校验） |
| 组装后反序列化失败 | 拒绝 spawn，移除 token |

---

## 6. 测试策略

- **core**（`yi-agent-core`）：
  - `session_handle` 与 `with_session_arc` 复用同一 Arc（重建后句柄仍指向活会话）；
  - fork 用 checkpoint 裁剪函数：末尾悬空 `tool_use` 被丢弃，已配对的历史保留。
- **store**（`yi-agent-store`）：
  - `BeginForkUpload` → 多次 `AppendForkChunk` → `SpawnApplicationChild{fork_token}`
    全链路成功；
  - 不完整上传被拒；未知/越权 token 被拒；`AbortForkUpload` 后 token 失效；
  - 超过 32 MiB 上限时 `BeginForkUpload` 拒绝。
- **subagent**（`yi-agent-subagent`）：
  - `fork:true` 时断言 worker 的**首个 provider 请求**包含父历史 + objective；
  - `fork:false` 时首个请求仍**只有** objective（回归不变）；
  - 未绑定 `CallerContext` 时 `fork:true` 报错。
- **app-server**：compact/重建 Agent 后，`fork:true` 仍读到**当前**会话（防过期
  回归）。

---

## 7. 明确不做（YAGNI）

- 只 fork **直接调用者**，不支持 fork 任意祖先或跨层指定。
- 不做摘要式上下文模式（决策 3 选了完整复制；`"summary"` 之类枚举留待将来）。
- fork 结果**不跨 daemon 重启持久化**（一次性、内存态）。
- 不改动 `MAX_FRAME_BYTES`，不放开大帧。
- 不为子 agent 引入专属 system prompt（现状父子相同，非本次范围）。

---

## 8. 风险与缓解

| 风险 | 缓解 |
|---|---|
| 超大历史打爆 daemon 内存 | 可配置上限（默认 32 MiB），超限明确拒绝 |
| 重建 Agent 后 fork 到过期会话 | `CallerContext` 持活句柄 + `with_session_arc` 保 Arc 稳定的不变量 |
| 上传中断留下半截 token | deadline 巡检 + `AbortForkUpload` + spawn 后一次性消费 |
| 悬空 `tool_use` 被 provider 拒 | fork 快照前做 checkpoint 裁剪 |
| fork 的 token 成本失控（并行多子 agent） | `fork` 默认 false，显式按需开启 |
