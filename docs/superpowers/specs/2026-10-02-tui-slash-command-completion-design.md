# TUI 未完成 slash 命令补齐设计

**目标：** 把 TUI 里仍未接通的 slash 命令补齐到可闭环使用，并把确实没有后端
支撑的命令从弹窗目录隐藏（保留识别与明确理由），同时修复 `/compact` 完成状态
在 TUI 路径上的回归。

**状态：** 设计已确认，待实现。

**范围：** 只改 TUI（`crates/yi-agent`）与必要的 `yi-agent-core` 事件定义。
**做**：`/config`（真配置）、`/model <name>`（运行期切换）、`/daemon [status|stop]`、
隐藏 `/approve` `/deny` `/budget` `/priority`（保留识别与理由）、恢复 `/compact`
完成事件。**不做**：为 `/approve` `/deny` `/budget` `/priority` 新增任何后端
能力（core / store / IPC 的权限桥、budget/priority 写 API），这些另立专项。

---

## 1. 背景

`crates/yi-agent/src/tui/app.rs::execute_slash_command`（`app.rs:2168`）当前有
三类占位输出：

| 命令 | 现状 | 位置 |
|---|---|---|
| `/config` | 输出「当前配置: (暂未实现)」 | `app.rs:2207-2215` |
| `/model <name>` | 输出「切换模型到: X (暂未实现)」 | `app.rs:2228-2245` |
| `/approve` `/deny` `/budget` `/priority` | 输出「…将由本地 daemon runtime 执行 (控制客户端接入中)」 | `app.rs:2271-2286` |

后端可行性分三档（已逐一核实）：

- **A 纯 TUI 可闭环**：`/config`（运行期已有 `RuntimeConfig` / `AgentConfig` /
  `McpManager` / runtime 偏好可读）、`/model`（driver 已有重建 agent 的模式）。
- **B 走既有 IPC 可闭环**：`/daemon status` / `/daemon stop`（`IpcRequest::Status`
  / `Stop` 已存在并已被 CLI 使用）。
- **C 需新增后端，本期不做**：`/approve` `/deny` 需要 worker permission channel →
  supervisor 持久化 → worker factory 桥接 `AgentEvent::PermissionRequest` →
  `IpcPermissionDecision` 全链路（生产路径当前**没有任何** worker 发过 permission
  request）；`/budget` `/priority` 在 core / store / IPC **完全没有写 API**。
  设计上 `approve` 也刻意不提供（`docs/project-management/subagent-runtime.md:68`、
  `docs/superpowers/specs/2026-10-01-subagent-observation-design.md:206`：验收只由父
  agent 真实 merge 后 daemon 观察 Git ancestry 完成）。

### 附带的既有回归：`/compact` 完成状态

`/compact` 看似已实现，但 TUI 路径上 pending 行「正在压缩对话...」**永不闭环**：

- `tui/history.rs:778 replace_pending_compaction` 只在收到 `ManualCompacted` /
  `ManualCompactFailed` 时替换该行（`history.rs:966-977`）。
- 但 TUI driver（`main.rs:1500` 的 `ControlCommand::Compact`）成功时只
  `tracing::info!`，不发任何事件；`Ok(None)` 也只 `tracing::info!`；
  `Err` 发的是 `AgentEvent::Error`（不是 `ManualCompactFailed`）。
- 只有 app-server 的 `translate.rs` 会发 `ManualCompacted`（`translate.rs:390`）。

git 证据：`8d5b23e`（"fix(tui): resolve compact status from driver events"）曾加入
`manual_compaction_outcome_event` 并在 driver 里发事件；`cf9a926` 仍保留它；
merge `19ba3cb`（"merge: integrate main compact history changes"）在解冲突时把它
丢掉了——该 merge 的两个 parent 之一 `48be050` 有该函数，merge 结果没有，且此后
再未恢复。而 `docs/project-management/yi-agent-tui.md` 把「压缩状态闭环」标为 `[x]`。

本设计把它一并修复，因为它就是「没开发完的 slash command」，且文档已有错误结论。

---

## 2. 命令语义

### 2.1 `/config`（只读展示）

无参数。渲染当前会话真实配置，逐行 `Separator` 输出。字段：

- `provider`
- `model`（当前值；`/model` 切换后跟随新值）
- `workdir`
- `sandbox`（`read-only` / `workspace-write` / `danger-full-access`）
- `yolo`（on / off）
- `max_turns`
- `compact_threshold`（token 数）
- `mcp` master（on / off）
- runtime 偏好（`ask` / `always` / `never`，来源文件路径）

**安全：绝不渲染 `api_key`。**

带多余参数时给用法提示 `/config`（无参数）。

### 2.2 `/model <name>`（运行期切换，本会话生效）

- 无参数：用法提示 `/model <model-name>`。
- 有参数：经新 `ControlCommand::SetModel(String)` 让 driver 用新 model 重建
  agent，**保留 session**（与 `Clear`/`Compact` 同构：重建 `Agent`、复用
  `provider` / `current_tools` / `PermissionChecker` / `decision_rx`）。
- 成功后 driver 发新事件 `AgentEvent::ModelChanged { model }`；TUI 收到后：
  - 更新本地 `model`（状态栏显示新模型）；
  - 在转录写一行确认（`已切换模型: <model>`）。
- 失败：写错误行，本地 `model` 不变。
- 切换**不落盘**，重启回到启动配置。

### 2.3 `/daemon [status|stop]`

- 无参数或 `status`：走 `IpcRequest::Status`，输出与 CLI
  （`main.rs:582 control_daemon_client`）同义的文案（含 event high-water mark）。
  失败（daemon 未运行）给可读原因。
- `stop`：走 `IpcRequest::Stop`，输出「daemon stopping」。
- `start` 不支持（TUI 已内嵌 daemon，语义冲突）：给用法提示
  `/daemon [status|stop]`。
- 其他参数：用法提示。

### 2.4 隐藏 `/approve` `/deny` `/budget` `/priority`

采用策略 (a)：**从弹窗与 `/help` 全量目录移除，但保留识别**。

- 新增 `SlashCommand::completable()`：等于 `all()` 去掉隐藏项，供弹窗
  `CommandPopup` 过滤与 `/help`（无参数）的全部列表使用。
- `SlashCommand::all()` 与 `from_name` **不变**，隐藏项仍可解析、仍参与
  `control_commands_tests.rs` 的目录断言与 CLI grammar（留锚点，未来接回不返工）。
- 显式敲出隐藏项时，给**明确理由**而不是「未知命令」：
  - `/approve`、`/deny` → 「`/approve` 暂不支持：子 agent 验收由父 agent 真实合并后
    daemon 自动观察，无交互式权限审批。」
  - `/budget`、`/priority` → 「`/budget` 暂不支持：daemon 尚未提供预算/优先级写接口。」

理由文案集中在一处（如 `SlashCommand::unavailable_reason() -> Option<&'static str>`），
便于未来接入时一次性删除。

### 2.5 `/compact` 完成闭环（回归修复）

driver 的 `ControlCommand::Compact` 分支恢复回报：

- `Ok(Some(new_session))` → `AgentEvent::ManualCompacted { old_msg_count, new_msg_count }`
- `Ok(None)` → `AgentEvent::ManualCompactFailed { message: "没有可压缩的历史" }`
- `Err(e)` → `AgentEvent::ManualCompactFailed { message: e.to_string() }`

即恢复 `8d5b23e` 的 `manual_compaction_outcome_event(old, result)` 辅助函数与其测试，
把 `Err` 分支从直接发 `AgentEvent::Error` 改为该 helper（`Ok(None)` 一并转成
`ManualCompactFailed`，这样 pending 行在三种结果下都闭环）。

---

## 3. 架构与数据流

### 3.1 配置快照进 TUI（`/config`）

`run_tui` 现在只收到 `model: String`（`app.rs:75-91`）。新增一个 TUI 自有的纯数据
结构 `TuiConfigSnapshot`（放在 `tui/slash.rs` 或新的 `tui/config_view.rs`）：

```rust
pub struct TuiConfigSnapshot {
    pub provider: String,
    pub model: String,
    pub workdir: PathBuf,
    pub sandbox: String,        // 已渲染的可读名
    pub yolo: bool,
    pub max_turns: u32,
    pub compact_threshold: u32,
    pub mcp_master: bool,
    pub runtime_preference: String, // ask/always/never
    pub runtime_preference_path: PathBuf,
}
```

`main.rs` 在调用 `run_tui` 前从 `RuntimeConfig`、`AgentConfig`、`mcp.master()`、
`runtime_prefs::load(&workdir)` 组装。`run_tui` / `run_loop` 增加该参数并向下传；
`execute_slash_command` 增加对应参数（现在是 `#[allow(clippy::too_many_arguments)]`，
新增一个参数沿用该 allow）。

`/model` 切换后，TUI 本地保存的 `model` 更新，`/config` 渲染时用**当前** model
（即 snapshot 的 model 字段也要在本地可变，或渲染时用独立的 `current_model`）。

**决定：** `run_loop` 已有一个 `model: &str` 参数（`app.rs:292`）。改为持有可变
`current_model: String`，`/config` 与状态栏都读它；`ModelChanged` 事件更新它。
`TuiConfigSnapshot` 里不再单独存 model，避免两处真值。

### 3.2 driver 侧 `/model` 与 `/daemon`

- `ControlCommand`（`main.rs:1868`）新增 `SetModel(String)`。driver 的
  `DriverInput::Control` 匹配新增分支：用 `rebuild_config` 的副本改 `model` 字段
  重建 agent + 保留 `agent.session()`，成功发 `ModelChanged`，失败发 `Error`。
- `/daemon` 不进 driver：`execute_slash_command` 直接同步调用既有
  `crate::runtime_socket_for(workdir)` + `yi_agent_store::ipc::send_request`（与
  `/agents` 等既有 daemon 命令同一模式），渲染到 history。

### 3.3 新事件 `AgentEvent::ModelChanged`

`crates/yi-agent-core/src/agent.rs` 的 `AgentEvent` 新增：

```rust
/// 运行期 `/model` 切换成功后广播；TUI 据此更新状态栏并写确认行。
ModelChanged { model: String },
```

需同步的匹配点（已核实各处的穷尽性）：

- **`crates/yi-agent-app-server/src/translate.rs:386-394`（唯一真正穷尽）**：该处是
  对所有 `AgentEvent` 的显式枚举 + 一个 `{}` 忽略组；新增 `ModelChanged { .. }`
  必须加进忽略组，否则编译失败。桌面端本期不消费。
- **`crates/yi-agent/src/main.rs` 的 headless drain（`record_human`，`:1152`）**：
  该 match 以 `_ => {}` 收尾，不会编译失败；`yi-agent run` 非交互式，无需渲染，
  可不改。
- **`crates/yi-agent-subagent/src/lib.rs`（worker 事件循环，`:725-906`）**：以
  `Some(_) => {}` 收尾，是通配，无需改。
- **`crates/yi-agent/src/tui/history.rs::push_event`（`:790`）**：以 `_ => {}` 收尾；
  `ModelChanged` 无需在此渲染（确认行由 `run_loop` 写，或直接在此加一个显式分支，
  见下）。
- **`crates/yi-agent/src/tui/app.rs::route_event`（`:1643`）**：以 `_ => {}` 收尾。
  它只拿 `&mut RunningTaskRegistry / &mut StatusBarState / &mut CostTracker`，没有
  model 持有者，因此 `current_model` 的更新放在 `run_loop` 的事件处理里，不塞进
  `route_event`。

**确认行的落点：** `ModelChanged` 的转录确认行有两种等价落法——(1) 在
`run_loop` 的事件分支里 `history.push(Separator)`；(2) 在 `history.rs::push_event`
加显式分支。二选一，实现时取与 `/compact` 的 `ManualCompacted` 处理一致的落点
（后者在 `push_event` 里处理），即优先 (2)，保持 `push_event` 是「事件 → 转录」的
唯一入口。

### 3.4 弹窗/help 的目录来源

- `CommandPopup`（`slash.rs:355`）的 `filter` 改用 `SlashCommand::completable()`。
- `help_text(None)`（`slash.rs:255`）改用 `completable()`；`help_text(Some(name))`
  保持对 `all()` 的解析，因此 `/help approve` 仍给出该命令信息（含 unavailable
  reason）。
- `slash.rs:510` 的测试 `filter_empty_shows_all` 与 `slash.rs:754` 的唯一性测试
  需相应调整：前者改用 `completable().len()`，后者保持覆盖 `all()`。

---

## 4. 错误处理

| 场景 | 表现 |
|---|---|
| `/config` 带参数 | 用法提示 `/config` |
| `/model` 无参数 | 用法提示 `/model <model-name>` |
| `/model` 空字符串 | 用法提示 |
| `/model` 切换失败（driver 报错） | 转录错误行，`current_model` 不变 |
| `/daemon` 参数非法 | 用法提示 `/daemon [status\|stop]` |
| `/daemon status/stop` daemon 未运行 | 可读原因（沿用既有「无法读取本地 daemon runtime: …」风格） |
| 敲 `/approve` 等隐藏项 | 明确 reason 行，非「未知命令」 |
| `/compact` 无可压缩 | pending 行替换为「压缩失败：没有可压缩的历史」 |

---

## 5. 测试策略

全部为 mock / 无网络测试，按 crate 跑：

**`yi-agent-core`**
- `ModelChanged` 变体存在且可构造（编译期即覆盖穷尽匹配）。

**`yi-agent`（`--bin yi-agent`）**
- `tui/slash.rs`：
  - `completable()` 不含 4 个隐藏项，`all()` 仍含；
  - 弹窗 `filter("")` 得到 `completable()`；
  - `help_text(None)` 不含隐藏项，`help_text(Some("approve"))` 含 reason；
  - `from_name("approve")` 仍解析。
- `tui/app.rs`：
  - `/config` 渲染出 provider/model/workdir 等字段且**不含 api_key**；
  - `/model X` 发 `ControlCommand::SetModel("X")`（断言 control channel 收到）；
  - `ModelChanged{X}` 事件后状态栏/`current_model` 更新；
  - `/daemon bad` 用法提示；`/daemon status` 走 IPC（可注入 fake socket 或断言错误路径）。
  - 隐藏项 `/approve` 输出 reason 文案。
- `main.rs`：
  - 恢复 `manual_compaction_outcome_event` 三态测试；
  - `SetModel` 重建后 `agent.session()` 非空（保留历史）；
  - driver 在 Compact 三态下分别发 `ManualCompacted` / `ManualCompactFailed`。

**验证命令**
```
cargo test -p yi-agent-core --lib
cargo test -p yi-agent --bin yi-agent tui::slash
cargo test -p yi-agent --bin yi-agent tui::app
cargo test -p yi-agent --bin yi-agent -- --exact manual_compaction_outcome_events_preserve_counts_and_errors
cargo test -p yi-agent-app-server
cargo fmt --all && just fmt-check
```

---

## 6. 文档更新

- `docs/project-management/yi-agent-tui.md`：
  - slash 目录条目：记录 4 个命令隐藏 + reason，`/config`、`/model`、`/daemon` 落地。
  - 「压缩状态闭环」条目：补上 driver 侧事件回报的说明与验证命令（此前为 `[x]` 但
    实现缺失，本设计恢复后为真）。
- `docs/project-management/subagent-runtime.md`：slash 条目同步隐藏说明。

---

## 7. 非目标（明确不做）

1. 不新增 `/approve` `/deny` `/budget` `/priority` 的后端能力（C 类，另立专项）。
2. 不做 `/model` 的持久化（不改 `.env` / preferences；仅本会话）。
3. 不做 `/daemon start`（TUI 内嵌 daemon）。
4. 不做桌面端 `ModelChanged` 消费（仅保证 app-server 翻译不因新变体编译失败）。
5. 不改 CLI grammar / `ControlCommand` 目录项（策略 a：留锚点）。
6. 不做 `/config` 的写操作（只读展示）。

## 8. 风险

- **`AgentEvent` 新增变体是跨 crate 破坏性改动**：所有穷尽匹配必须同步，
  编译期可发现；`translate.rs` 与 headless drain 是最容易漏的两处。
- **`execute_slash_command` 参数继续膨胀**：已 `allow(too_many_arguments)`；本期
  新增 1 个参数，不重构（避免把改动面铺开）。
- **`current_model` 真值位置**：必须单一（`run_loop` 局部变量），否则 `/config` 与
  状态栏会漂移。
- **`/daemon` 同步 IPC 阻塞 UI 线程**：与既有 `/agents` `/review` 等一致（它们就是
  同步 `send_request`），沿用该惯例；daemon 不可达会快速返回错误。
