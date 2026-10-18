# 设计：后端巨型模块拆分与测试脚手架收敛

日期：2026-10-17
状态：待评审
关系：来自 2026-10-17 冗余审计的 P1-3（测试脚手架）与 P1-4（巨型模块），
外加 P2-3（relay URL 解析重复）。

## 1. 背景与问题

### 1.1 `app-server/src/server.rs` 已是不可维护的单文件

| 指标 | 实测值 |
|---|---|
| 文件行数 | **22,368** |
| `serve()` 函数 | 行 2183 起，**3,454 行**（内层 `match method.as_str()` 从 2457 到 5587，约 2,130 行） |
| `serve()` 参数 | **15 个** |
| RPC 方法臂 | 该 match 内 **140** 条字符串臂，覆盖 16 个前缀域（`thread/`17、`agent/`7、`image/`5、`workspace/`3、`turn/`3、`process/`3、`plugin/`3、`onboarding/`3、`model/`3、`board/`3、`ui/`2、`pair/`2、`device/`2、`plugins/`1、`config/`1 …） |
| 顶层函数 | 101 个 |
| 单元测试 | 全部**内联**：`pub(crate) mod tests` 起于行 **9560**，占文件后 **57%**（12,808 / 22,368 行） |
| `tests/` 目录 | **不存在** |

该 crate 的兄弟模块已按域拆开（`attachments.rs`、`thread_store.rs`、`model_rpc.rs`、
`onboarding_rpc.rs`、`broadcast.rs`、`ws.rs` …），唯独**最大的 `server.rs` 没有**，
于是所有 RPC handler 都堆进 `serve()` 一个函数里。

后果：任何人改一条 RPC 都要在一个 3,454 行函数里定位；编译单元粒度退化（改一处
测试要重编整个 22k 行文件）；且新 handler 无自然的落位处，只会让函数继续膨胀。

### 1.2 测试脚手架复制粘贴

`yi-agent-store/tests/`（10 个文件，**11,816 行**）**没有 `common/` 模块**，于是：

| 重复物 | 实测 |
|---|---|
| `fn read_only_workspace` | 8 处（`runtime_ipc.rs` 2、`runtime_coordinator.rs` 3、`subagent_fork_ipc.rs`、`subagent_fork_upload.rs`、`subagent_runtime_e2e.rs` 各 1） |
| `struct StaticWorkspaceService` | 3 处（`runtime_ipc.rs`、`runtime_coordinator.rs`、`subagent_fork_ipc.rs`） |
| `impl AgentWorkerFactory` | 5 个测试文件里各持一份 |

对照：`yi-agent/tests/` **已有** `common/mod.rs`（353 行）被 4 个 e2e 文件复用，
但 `board_e2e.rs`（308 行）与 `board_watchman_e2e.rs`（846 行）**绕开它**，
各自重新定义了 `struct AppServer`（`board_e2e.rs:57`、`board_watchman_e2e.rs:137`），
两份只在注释措辞与少量辅助方法上不同。

### 1.3 relay URL 解析两份逐字相同

`yi-agent/src/main.rs:195` 的 `relay_parts` 与
`yi-agent-app-server/src/server.rs:1733` 的 `parse_relay_url`：**函数体逐字相同**
（含 `anyhow!` 报错文案），仅返回类型写法差 `Result` vs `anyhow::Result`。
server.rs 该处还留着注释「此处复制一份，app-server 不得反向依赖…」——但
**`yi-agent-relay` 是两个 crate 共同依赖的叶子 crate**，正是它的归属地。

### 1.4 god-signature（TUI）

`yi-agent/src/tui/app.rs`（10,683 行）：

| 函数 | 参数个数 |
|---|---|
| `handle_key` (行 1818) | **25** |
| `run_loop` (行 317) | **19** |
| `run_tui_with_backend_and_events` (行 241) | **11**，被调 **32** 次 |

`handle_key` 的第 11 个参数是 `control_tx: Sender<crate::ControlCommand>`，
这正是它签名臃肿的根源：每个"能力"都以独立 channel 传入。测试里的
`let (input_tx, _input_rx) = mpsc::channel::<String>(16);` 样板在同一文件出现 **74 次**
（因为每次构造调用点都要重建整串 channel）。

## 2. 目标与非目标

### 目标

1. 把 `server.rs` 的 `serve()` 按 RPC 前缀域拆成独立 handler 模块，`server.rs`
   只留 transport 循环与 dispatch 骨架；内联测试目录化（新建 `tests/`）。
2. `yi-agent-store/tests/` 建 `common/`，收拢 `read_only_workspace` /
   `StaticWorkspaceService` / `AgentWorkerFactory` 三个替身。
3. `relay_parts` / `parse_relay_url` 收成 `yi-agent-relay` 的单一函数。
4. `handle_key` / `run_loop` 的参数收进一个 `TuiChannels` 结构体（或等价 bundle），
   使 `handle_key` 参数降到个位数；测试样板顺带收敛。
5. `main.rs:2509` 的 `enum ControlCommand` 改名 `DriverControl`（连带 `tui/app.rs`
   的 101 处 `crate::ControlCommand`），消除与 `control_commands::ControlCommand`
   的同名——**该改名归本卡**（另一张卡刻意不碰 `main.rs`/`app.rs`，保证零文件重叠）。

### 非目标

- **不改任何 RPC 方法名与线上 JSON 形状**（纯搬移）。
- 不动前端（属前端架构卡）。
- 不重写 TUI 行为；`handle_key` 的 bundle 化是**纯力学重构**。
- 不追求把 `server.rs` 拆到某个行数目标；按域拆到"每条 RPC 有明确归属"即止。

## 3. 设计

### 3.1 `server.rs` 按域拆分

在 `yi-agent-app-server/src/` 下新增 handler 模块，每个对应一个 RPC 前缀域：

```
src/rpc/mod.rs           dispatch 表 + 共享上下文（AppState）
src/rpc/thread.rs        thread/*        (17 臂)
src/rpc/agent.rs         agent/*         (7)
src/rpc/image.rs         image/*         (5)
src/rpc/workspace.rs     workspace/* + workspace_index 相关 (3)
src/rpc/turn.rs          turn/*          (3)
src/rpc/process.rs       process/*       (3)
src/rpc/plugin.rs        plugin/* + plugins/* (4)
src/rpc/onboarding.rs    onboarding/*    (3)
src/rpc/model.rs         model/*         (3)
src/rpc/board.rs         board/*         (3)
src/rpc/ui.rs            ui/* + config/* (3)
src/rpc/pair.rs          pair/* + device/* (4)
```

`serve()` 的 `match method.as_str()` 退化为：

```rust
match rpc::domain_for(method.as_str()) {
    rpc::Domain::Thread => rpc::thread::handle(&mut state, method, params).await?,
    rpc::Domain::Agent => rpc::agent::handle(&mut state, method, params).await?,
    // …每个域一行
    rpc::Domain::Unknown => { /* 现有的 method-not-found 响应，逐字保留 */ }
}
```

每个 `handle` 内部保留原先的 `match method`，**分支体逐字搬移**，只把 `state`
（原 `serve()` 局部变量中 handler 需要的那些）提为 `rpc::AppState`。

> 拆分顺序按域逐个做、每个域一次提交，保持 review 面小。第一批建议
> `image/`、`model/`、`onboarding/`（三者已有独立模块，臂体短、依赖少）。

### 3.2 测试目录化

把 `server.rs:9560` 起的 `pub(crate) mod tests`（12,808 行）与新模块一起迁到
`yi-agent-app-server/tests/`（集成测试）或各模块的 `mod tests`（就近）。
判据：能被 `tests_support`（现 `server.rs:9481` 的 `pub(crate) mod tests_support`）
服务的是集成测试，否则就近留在模块内。

> `tests_support` 已因 `ws.rs` 的 E2E 需要而放在 `mod tests` 之外，拆分时它应
> 移到 `src/rpc/tests_support.rs` 或独立 `src/test_support.rs`，供 `tests/` 引用。

### 3.3 store 测试脚手架

新建 `yi-agent-store/tests/common/mod.rs`，导出：

```rust
/// 一个把父 workspace 路径原样继承的可读写替身。
pub fn read_only_workspace(
    repository_root: &Path,
    parent: Option<&WorkerWorkspace>,
    _task_id: &TaskId,
) -> Result<WorkerWorkspace, WorkerError>;

/// 一个不落盘、只回答固定值的 workspace provider。
pub struct StaticWorkspaceService { /* … */ }
```

各测试文件删掉本地副本，改 `mod common;` + 调用。`AgentWorkerFactory` 的实现
各家差异较大（不同测试要不同的 worker 行为），**只抽公共骨架**，差异部分留
一个 builder/回调。

`yi-agent/tests/board_e2e.rs` 与 `board_watchman_e2e.rs` 的 `AppServer` 合并进
`common/mod.rs`（两处的差异是注释与少量辅助方法——合并时取并集，行为以
`board_watchman_e2e.rs` 那份为准，因它更完整）。

### 3.4 relay URL 单一来源

在 `yi-agent-relay` 加：

```rust
/// 把中继端点拆成「去掉 `session` 的 URL」+「session id」。
pub fn parse_relay_parts(relay_url: &str) -> anyhow::Result<(String, String)>
```

`main.rs` 的 `relay_parts` 与 `server.rs` 的 `parse_relay_url` 改为一行委托。

### 3.5 TUI channel bundle

```rust
/// `run_loop` / `handle_key` 共用的出向 channel 集合。
///
/// 这些 channel 在整场 TUI 会话里成组存在、成组传递，故打包成一个结构体，
/// 避免每加一个能力就给 `handle_key` 再加一个参数。
pub(crate) struct TuiChannels {
    pub input: tokio::sync::mpsc::Sender<String>,
    pub interrupt: tokio::sync::mpsc::Sender<()>,
    pub kill: tokio::sync::mpsc::Sender<String>,
    /// 类型沿用当前 `main.rs` 里的 TUI→driver 指令枚举（本卡不改它的名字）。
    pub control: tokio::sync::mpsc::Sender<crate::ControlCommand>,
    pub decision: tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
}
```

> **跨卡注意**：另有一张卡会把 `main.rs` 的 `ControlCommand` 改名为 `DriverControl`。
> 本卡**不预设任何名字**：实现时以 `grep -n "enum .*Control" crates/yi-agent/src/main.rs`
> 的当前结果为准，写入 `TuiChannels.control` 字段类型。

`handle_key(key, input, history, max_scroll_offset, …, channels: &TuiChannels, …)`。
测试里的 74 处 `let (input_tx, …)` 样板收敛为一个
`common` helper（TUI 测试模块内的 `fn channels() -> TuiChannels`）。

## 4. 测试策略

- **纯搬移的验收**：拆分前后 `cargo test --workspace` 的**用例名集合**必须一致
  （只用 `-- --list` 比对，不看行数）。
- relay 单一来源：保留 `relay_parts` 的两条既有测试（它们现已驱动共享函数）。
- store 脚手架：三个替身的既有测试全绿；新增一条断言 `read_only_workspace`
  继承父路径。
- TUI bundle：`handle_key` 的既有测试全绿（数量不减）。

## 5. 验收标准

- `serve()` 行数从 3,454 降至 < 300（只剩 transport + dispatch）。
- `wc -l crates/yi-agent-app-server/src/server.rs` 显著下降；新增 `src/rpc/` 模块。
- `cargo test -p yi-agent-store --offline` 全绿，且
  `grep -rc "fn read_only_workspace" crates/yi-agent-store/tests/*.rs` 只剩 `common/mod.rs`。
- `grep -rn "fn relay_parts\|fn parse_relay_url" crates/` 只剩 `yi-agent-relay` 的定义。
- `handle_key` 参数个数 ≤ 12（去掉 5 个 channel 后）。
- `cargo clippy --workspace --all-targets -- -D warnings` 无警告。
