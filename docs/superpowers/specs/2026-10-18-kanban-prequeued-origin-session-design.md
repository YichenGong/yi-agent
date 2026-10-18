# 看板 pre-queued 卡关联讨论会话 Design

日期：2026-10-18
状态：设计已定稿，待用户 review 后进入 writing-plans。

## 1. 背景与问题

看板的 `PRE-QUEUED` 列承载「已决定要做、spec/plan 还没写」的需求（见
`docs/superpowers/specs/2026-10-18-kanban-pre-queued-design.md`）。这类卡通常诞生在
一次**讨论**里：用户在某个会话里聊出一个需求，说「先记着」，于是投一张预排队卡。

问题是这张卡记下之后**与那次讨论脱了钩**：

- 卡片上没有任何字段指向「它是在哪个会话里提出来的」；
- 390 天后回头看这张卡，没人知道当初为什么提出它、上下文在哪；
- 桌面看板上那张卡只有一个标题，点不进任何会话。

同时，卡片现有的 `thread_id` 字段**不能**拿来存这件事：它的既有语义是
「**跑这张卡的执行会话**」，由宿主调度器在启动卡片会话后回填
（`yi-agent-app-server/src/server.rs` 的 `board.mark_running { card_id, thread_id }`
→ `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs::mark_running`），
且 `mark_running` **每次都会覆写**它。用它存讨论会话，会在这张卡被提升、开跑时被
执行会话覆盖，恰好丢掉要保留的那条线。

## 2. 目标与非目标

**目标**

- 预排队卡记录**提出它的讨论会话**，在桌面看板上可点击跳回那个会话。
- 该关联**不与执行会话（`thread_id`）混淆**：提升、开跑都不覆盖它。
- 覆盖两条真实投递路径：
  1. 会话内的 agent 用 skill/CLI 投递（`superpowers-kanban add-prequeued <title>`）——
     这是主路径；
  2. 桌面看板面板的「预排队」输入框。
- 拿不到会话身份时**不阻断**投递（会话外手动记卡是合法操作）。

**非目标**

- 不做「承接上下文」：不把讨论会话作为实现会话的起点，不让实现会话读讨论上下文。
  实现会话仍由调度器另起（`next_launch` → `LaunchRequest`），语义不变。
- 不做反向可见（在讨论会话里列出「我提过哪几张卡」）。
- 不做 TUI 的会话关联：TUI 是本地单会话进程，没有 daemon thread id 可填
  （见 §4.3）。
- 不改看板并发模型、不改卡片状态机、不动 `thread_id` 的任何既有行为。
- 不为旧卡回填来源会话（历史数据无从考证）。

## 3. 数据模型

### 3.1 `Card.origin_thread_id`

`plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card.rs`：`Card` 新增

```rust
/// 提出这张卡的**讨论会话**。与 `thread_id`（跑这张卡的执行会话）语义不同，
/// 故不复用后者：`thread_id` 会被调度器的 `mark_running` 覆写成执行会话，
/// 复用它会让溯源线在提升/开跑时断掉。
/// 旧 board.json 缺此字段 → `None`。
#[serde(default)]
pub origin_thread_id: Option<String>,
```

- 落盘键名 `origin_thread_id`，与既有 `thread_id` 同风格（snake_case，插件零
  `yi-agent-*` 依赖，协议手写）。
- `#[serde(default)]`：旧 `board.json` 无此键 → `None`，**不得**整份解析失败
  （与 `title`、`base_commit` 等字段同款前向兼容要求）。

### 3.2 构造点

三个构造点都要补这个字段（否则编译不过，正好当一次全面排查）：

- `Board::enqueue`（实现卡）→ `origin_thread_id: None`
- `Board::enqueue_merge`（合并卡）→ `origin_thread_id: None`
- `Board::enqueue_prequeued` → 新增参数 `origin_thread_id: Option<String>`

### 3.3 `promote` 不改它

`Board::promote_prequeued(id, spec, plan, title)` **不触碰** `origin_thread_id`：
id 不重算、来源会话随卡走。这是「溯源不断线」与「id 不重算」同一条原则的延续。

## 4. 会话身份从哪来

### 4.1 桌面端面板

看板面板打开时，App 仍持有当前会话 id（`desktop/src/App.tsx` 的 `sess.currentId`；
打开看板只改 `selectedBoard`，不动 `currentId`，见 `useBoardState` 顶部注释）。

- `desktop/src/lib/superpowersKanbanSwitch.ts::enqueuePrequeuedCard` 增加
  `originThreadId: string | null` 参数，非空时放进 `plugin/query` 的 params：
  `{ title, origin_thread_id }`。
- 投递点（`App.tsx` 的 `prequeue={(title) => ...}`）传 `sess.currentId`。
  **注意**：`sess.currentId` 可能为 `null`（尚未开任何会话），此时传 `null`，
  由 §4.4 的「静默留空」兜底。

### 4.2 会话内 CLI（主路径）

`superpowers-kanban add-prequeued` 是独立进程，拿不到会话上下文。宿主必须在**装配期**
把当前 thread id 注入**工具子进程的环境变量**：

- 环境变量名：`YI_AGENT_THREAD_ID`。
- 注入点：`yi-agent-app-server/src/server.rs::build_runtime_tooling` 已经拿到
  `thread_id`（同款先例：`ShowGitDiffTool::new(git_diff, Some(thread_id))`、
  `AskLeadTool::new(lead_inbox, project_key, thread_id.to_string())`，该函数注释也
  明写「模型看不到自己的 thread id，只能由装配期注入」）。把它继续往下传，
  让 `BashTool` 在 spawn 子进程时 `.env("YI_AGENT_THREAD_ID", thread_id)`。
- **透传接缝（刻意收窄，避免波及既有调用点）**：`register_builtin_tools_with_sandbox`
  有 7 个调用点（subagent、headless、TUI 子 agent 运行时），逐层加参数会全部波及。
  而 `ToolsContext` 在生产代码里**只有一处构造点**：
  `yi-agent-tools/src/lib.rs:98`（`register_builtin_tools_with_controller` 内部）。
  故走这条最小面：
  1. `ToolsContext` 新增 `session_env: Vec<(String, String)>`（经新构造器
     `with_roots_and_env` 或 builder 设置，既有 `new`/`with_roots` 保持默认空集）；
  2. `BashTool` spawn 时 `.envs(self.ctx.session_env())`（空集时行为与今天完全一致）；
  3. `yi-agent-tools` 新增注册入口
     `register_builtin_tools_with_controller_and_env(..., session_env)`，
     既有 `register_builtin_tools_with_controller` 委托给它并传空集——**既有 7 个
     调用点一行不改**；
  4. `yi_agent_runtime::bootstrap::build_tool_setup_with_controller` 新增同款
     `session_env` 参数（它只有 2 个调用点：bootstrap 内部一处、app-server 一处），
     由 `build_runtime_tooling` 传 `[("YI_AGENT_THREAD_ID", thread_id)]`。
  为不走 env 的路径（TUI 本地会话、headless）传空集：它们本就拿不到 thread id。
- CLI 侧：`command_prequeued` 读 `std::env::var("YI_AGENT_THREAD_ID")`，`Ok(v)` 且
  非空时作为 `origin_thread_id`；否则 `None`。

**为什么走环境变量而不是 CLI 参数**：skill 与 README 记录的调用形式是
`superpowers-kanban add-prequeued "<title>"`，模型不该被迫拼一个自己看不见的
thread id；环境变量由装配期注入，模型无感，且不会打错。

**注入范围**：只注入 `BashTool` 起的 shell 子进程，不改 daemon 自身进程环境——
daemon 可能有多个 thread，进程级环境变量无法表达「哪个 thread」。

### 4.3 TUI

TUI（`yi-agent/src/main.rs::run_tui_agent`）是本地单会话进程，自己 `Agent::builder`
起 agent，**没有 daemon thread id**；只有 attach 到 runtime 后才有
`crate::tui::subagents::current_attached_root().session_id`，而那是 daemon 的
`session_id`（子 agent 运行时），与「app-server 的 thread id」不是一回事，拿它填
`origin_thread_id` 会让桌面端跳去一个不存在的对话。故 TUI 一律留空（`None`）。

### 4.4 拿不到时静默留空

`origin_thread_id` 是**尽力而为的附加信息**：会话外手动投递、`sess.currentId` 为
`null`、TUI、以及任何未注入 env 的宿主路径，都照常投递、字段为 `None`，桌面端不
画跳转按钮。关联缺失**不**报错、**不**阻断（`add-prequeued` 在任何环境下都是合法
操作）。

## 5. 写入路径：仍然全部走 inbox

架构不变量不变：`board.json` 的唯一写者是 runner 推进循环，外部一律通过 `inbox/`
投递（见 `docs/superpowers/specs/2026-10-17-kanban-single-source-of-truth-design.md`）。

`core/src/inbox.rs::deliver_prequeued_card` 增加 `origin_thread_id: Option<&str>` 参数，
落盘形状：

```json
{ "kind": "prequeued", "id": "...", "title": "...", "origin_thread_id": "thread-..." }
```

`origin_thread_id` 为 `None` 时**不带该键**（与 `deliver_promote_card` 对 `title`
的处理同款）。`temp + rename` 原子替换不变。

消费侧 `runner/src/inbox.rs`：

- `RawDelivery` 加 `#[serde(default)] origin_thread_id: Option<String>`。
- prequeued 分支把 `delivery.origin_thread_id.clone()` 透传给
  `board.enqueue_prequeued(...)`。
- `promote` 分支**不读**它（提升不改溯源，见 §3.3）。

## 6. 展示与跳转

### 6.1 插件 `list` 输出

`runner/src/service.rs::BoardService::list` 的每张卡 JSON 加 `"origin_thread_id"`。
`dispatch.rs` 的 `list` 相关测试同步断言字段存在（缺省可为 `null`）。

### 6.2 桌面数据层

`desktop/src/lib/superpowersKanbanState.ts`：

- `BoardCard` 加 `originThreadId: string | null`。
- `normalizeCard` 读 `record.origin_thread_id`，非空字符串才取值，否则 `null`
  （与 `threadId` 同款归一化）。
- `BoardCardDto`（`superpowersKanbanSwitch.ts`）同步加可选字段。

### 6.3 卡片渲染

`desktop/src/components/SuperpowersKanbanCard.tsx` 的会话按钮改为取
**`card.threadId ?? card.originThreadId`**：

- pre-queued / queued 阶段：`thread_id` 为空 → 显示**讨论会话**；
- 卡片跑起来后：`thread_id` 有值 → 自动切到**执行会话**；
- 两者都空 → 不渲染按钮（现有 `card.threadId ? ... : null` 的写法扩展成这个回退）。

一张卡同一时刻只显示一个会话链接，语义随生命周期自然切换，无需额外状态。

## 7. 测试

- **core**（`superpowers-kanban-core`）：
  - 旧 `board.json`（无 `origin_thread_id`）反序列化 → `None`；
  - `enqueue_prequeued` 存下 `origin_thread_id`，`None` 时不落该键；
  - `promote_prequeued` 后 `origin_thread_id` 原样保留；
  - `enqueue` / `enqueue_merge` 的 `origin_thread_id` 为 `None`。
- **runner**：
  - `consume` 把投递里的 `origin_thread_id` 落到卡上；
  - `deliver_prequeued_card` 的落盘形状（带/不带该键两种）；
  - CLI `add-prequeued` 在有/无 `YI_AGENT_THREAD_ID` 时的行为（有则带上，无则留空、
    仍成功）；
  - dispatch `enqueue_prequeued` 的 params 可选性（不传也成功）。
- **tools**（`yi-agent-tools`）：`BashTool` 注入的 env 确实能被 shell 子进程读到
  （起一个 `sh -c 'echo $YI_AGENT_THREAD_ID'` 断言）；未设置时子进程看不到该变量。
- **app-server**：装配期把 `thread_id` 一路传到 `BashTool`（接线测试；沿用现有
  `build_tool_setup_with_controller` 测试的注入范式）。
- **desktop**（vitest）：
  - `normalizeCard` 读 `origin_thread_id`；
  - `enqueuePrequeuedCard` 带/不带 `originThreadId` 时 params 形状；
  - 卡片按钮回退：只有 `originThreadId` 时点它跳讨论会话，`threadId` 有值时优先它。
- 手工路径（可选）：在会话里 `superpowers-kanban add-prequeued "示例"` → 桌面
  PRE-QUEUED 卡上出现讨论会话链接 → 点击跳回。

## 8. 文档与 skill 同步

- 插件 `README.md`：预排队一节补「关联讨论会话」：会话内投递经
  `YI_AGENT_THREAD_ID` 自动关联，桌面面板自动带上当前会话，缺失时不阻断。
- `plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md`：同步一句，让
  agent 知道在会话里 `add-prequeued` 会自动挂上当前会话（无需额外参数）。
- `docs/project-management/`：同步 `desktop.md`（卡片字段与按钮）、
  `yi-agent-app-server.md`（装配期 env 注入）、`yi-agent-tools.md`（BashTool env）、
  以及 `plugins` 相关模块状态；并按需更新 `README.md` 计数。

## 9. 验收判据（可执行）

- `cargo test -p superpowers-kanban-core -p superpowers-kanban-runner`
  （在 `plugins/superpowers-kanban/` 下）通过。
- `cargo test -p yi-agent-tools -p yi-agent-runtime -p yi-agent-app-server` 通过。
- `cd desktop && npm test` 通过。
- 手工路径：会话内 `superpowers-kanban add-prequeued "示例需求"` → 看板出现
  PRE-QUEUED 卡且带讨论会话链接 → `promote` 后链接仍在（卡变 QUEUED）→ 卡开跑后
  链接切换为执行会话。
- 回归：旧 `board.json` 仍能反序列化；`add-prequeued` 在会话外仍成功（字段为空）。
