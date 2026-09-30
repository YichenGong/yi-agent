# Mac 桌面端子 Agent 委派对齐 TUI 设计

**目标：** 让 Mac 桌面应用（Tauri GUI + `yi-agent app-server` sidecar）与 TUI 一样，
模型手里有 `spawn_agent` / `wait_agent` / `send_message` / `inspect_agent` /
`cancel_agent` / `review_agent` 这组委派工具，从而使"起 subagent"这件事在桌面端可用。

**状态：** 已设计，待实现。

**范围：** 只打通"启用"。不含启动确认弹窗、不含 GUI 开关、不含子 agent 任务树可视化
（后者是 `docs/project-management/desktop.md` 的 P3 路线图条目，独立工作）。

**相关文档：**
`docs/superpowers/specs/2026-08-09-subagent-architecture-design.md`（任务树与 daemon 架构）、
`docs/superpowers/specs/2026-09-29-agent-owned-worktree-design.md`（root 原地运行、workdir 显式传入）、
`docs/superpowers/specs/2026-09-30-tui-subagent-runtime-preference-design.md`（TUI 侧启动偏好）。

---

## 1. 问题

### 1.1 现象

Mac 桌面版 yi-agent app 上无法起 subagent：模型没有 `spawn_agent` 工具可调。

### 1.2 根因（已核实）

TUI 能起 subagent，是因为 `crates/yi-agent/src/main.rs` 里有一条完整的 bring-up 链路：

1. `attach_tui_runtime`（`crates/yi-agent/src/main.rs:857`）启动或复用项目内嵌 daemon
   （`Daemon::start_with_factory`），再对项目 workdir 发 `AttachApplicationRoot`
   （idempotency key = `tui:<pid>:<workdir>:<uuid>`）。
2. `build_tui_root_tools`（`crates/yi-agent/src/main.rs:740`）以
   `attached_root.workspace.path` 为根重建工具集，并调用
   `register_attached_root_tools`（`crates/yi-agent/src/tui/subagents.rs:75`
   → `register_application_subagent_tools`，`crates/yi-agent/src/subagent_runtime.rs:1042`）
   注册六个委派工具。
3. `build_daemon_worker_factory`（`crates/yi-agent/src/main.rs:517`）构造
   `DaemonAgentWorkerFactory`，即 daemon 侧真正跑子 worker 的工厂。
4. 每轮首个 prompt 前 `activate_tui_runtime_root`（`crates/yi-agent/src/main.rs:919`）
   激活 root；退出时 `detach_tui_runtime_root`（`:938`）。

桌面端这条链路**一步都没走**（不是坏了，是从未接过）：

- `desktop/src-tauri/src/bridge.rs:87` 只拉起 sidecar
  `yi-agent app-server --listen stdio://`（`:94` 把 cwd 设为 `$HOME`）；Rust 侧是纯帧
  桥接，不含 agent 逻辑（`desktop/README.md`）。
- `crates/yi-agent-app-server/src/server.rs:52` 的 `run()` 给每个 thread 建 agent 走
  `yi_agent_runtime::bootstrap::bootstrap_agent`；整个 crate **不依赖
  `yi-agent-store`**（`crates/yi-agent-app-server/Cargo.toml`），从不接触 daemon。
- `desktop/src/lib/protocol.ts` 与 `desktop/src/App.tsx` 无任何 subagent 接口。

`crates/yi-agent-app-server/src/translate.rs:375` 那段注释已经把这个事实写在代码里：
"App-server has no runtime bring-up of its own, so this event is unreachable from its
driver"。

### 1.3 三处与 TUI 不同的约束

1. **app-server 是长驻多 cwd 进程，TUI 是一进程一项目。** 所以 daemon 启动 +
   `AttachApplicationRoot` 不能是进程级一次性步骤，必须按 thread 的 `cwd` 懒初始化并缓存。
   `AttachApplicationRoot` 自带 `idempotency_key`（`crates/yi-agent-store/src/runtime.rs:692`
   `attach_application_root`），重复 attach 是幂等的，这让 per-cwd 缓存天然安全。
2. **TUI 的接线代码在 bin crate 里，共享不了。** `crates/yi-agent` 只有 `[[bin]]`
   （无 lib），且带 clap / crossterm / ratatui；app-server 不能依赖它。
3. **回调是同步的。** `run_with` 的 `build_agent` 闭包签名
   （`crates/yi-agent-app-server/src/server.rs:89`）返回 `anyhow::Result<BuiltAgent>`，
   非 async；而 daemon 启动与 attach 是同步阻塞调用（Unix socket + SQLite）。
   TUI 的 attach 同样在 driver 的同步路径上，性质一致。

---

## 2. 设计

### 2.1 新 crate `yi-agent-subagent`（纯平移）

新建 `yi-agent-rs/crates/yi-agent-subagent/`，依赖
`yi-agent-core` / `yi-agent-store` / `yi-agent-tools` / `yi-agent-runtime`。

**从 `crates/yi-agent/src/subagent_runtime.rs` 整体平移**（已确认该文件零 `crate::`
内部引用，可整块搬）：

- `DaemonAgentWorkerFactory`（`:35`）与 `DaemonWorkspaceService`（`:210`）
- git helper：`git_output` / `git_command_output` / `current_git_branch` /
  `git_writable_roots_for_worktree` / `git_dir_for_worktree` / `recovery_workspace_path` /
  `validate_recovery_context`
- 六个工具的 application 与 worker 两个变体、`DaemonSpawnAgentTool`、
  `spawn_mode` / `spawn_workdir` / `spawn_model`、`format_ipc_rejection`、
  `text_completion_report`、`provider_retry_failure`、`is_dirty_delivery_error`、
  `is_empty_delivery_error`、`retry_jitter_millis`
- `register_application_subagent_tools`（`:1042`）
- 该文件现有的 `#[cfg(test)] mod tests`（约 117 个 `fn`，含大量进程内测试）

**从 `crates/yi-agent/src/tui/subagents.rs` 平移**：

- `AttachedRoot`（`:34`）
- `register_attached_root_tools`（`:75`，签名不变——它只依赖 `ToolRegistry` +
  `AttachedRoot`，天然无 UI 依赖）
- `worker_workspace()` / `attached_root()` 两个测试 helper

**明确留在 TUI 侧**（TUI 专有交互，不进共享 crate）：
`RuntimeStartupChoice` / `RuntimeStartupIntent` / `RUNTIME_RESTART_NOTICE` /
`runtime_restart_notice()`。

**一条必须写死的边界：** `CURRENT_ATTACHED_ROOT`
（`crates/yi-agent/src/tui/subagents.rs:58` 的进程级 `OnceLock<Mutex<Option<AttachedRoot>>>`
单例）**只留在 TUI**。TUI 一进程一 root 可以这样表达；app-server 多 thread，若共用全局
单例会互相覆盖。app-server 用 per-cwd 映射（§2.3）。

`yi-agent` 侧改动仅为改 `use`：
`crates/yi-agent/src/tui/subagents.rs:9` 与
`crates/yi-agent/src/main.rs:535`。

### 2.2 共享 attach 客户端（`yi-agent-subagent::attach`）

把 `crates/yi-agent/src/main.rs` 的四个函数参数化后抽出（TUI 目前从 `Cli` 取配置，
改为接收 `&RuntimeConfig`）：

| 新 API | 来源 |
|---|---|
| `attach_project_runtime(cfg: &RuntimeConfig, socket_path: PathBuf) -> Result<AttachedProjectRuntime, AttachFailure>` | `attach_tui_runtime`（`main.rs:857`）+ `build_daemon_worker_factory`（`main.rs:517`）+ `runtime_database_path`（`main.rs:577`） |
| `activate_root(socket, &AttachedRoot, objective) -> Result<(), String>` | `activate_tui_runtime_root`（`main.rs:919`） |
| `detach_root(socket, &AttachedRoot)` | `detach_tui_runtime_root`（`main.rs:938`） |
| `project_socket_path(workdir) -> Result<PathBuf, IpcError>` | `runtime_socket_for`（`main.rs:552`） |

`AttachFailure { stage: &'static str, cause: String }`：`stage` 命名失败环节
（`daemon start` / `attach` / `activation`），`cause` 是原始诊断。两者都只进 trace，
不面向用户（桌面端无 UI 通道，见 §2.4）。

`AttachedProjectRuntime` 结构：

```rust
pub struct AttachedProjectRuntime {
    /// The Unix socket the runtime was attached through.
    pub socket_path: PathBuf,
    /// The Git checkout the dispatch tools resolve their workdir against:
    /// `attached_root.workspace.path`.
    pub workspace_root: PathBuf,
    /// The project directory the root was attached for.
    pub project_root: PathBuf,
    pub attached_root: AttachedRoot,
    /// Held only when this process started the daemon (released on drop).
    pub embedded_daemon: Option<Daemon>,
}
```

- `already running`（`IpcError::AlreadyRunning`）→ 复用既有 daemon，
  `embedded_daemon: None`，与 TUI 行为一致。
- `workspace_root` 取 `attached_root.workspace.path`。D-4（root 原地运行，见
  `2026-09-29-agent-owned-worktree-design.md` §3.2）之后它等于
  `DaemonWorkspaceService` 解析出的 git 顶层。
- `project_socket_path` 复用同一个环境变量覆盖
  （`YI_AGENT_RUNTIME_DIR`，`main.rs:558` `runtime_directory_for`），daemon 与客户端
  必须解析到同一处。
- `ignore_project_local_runtime_state`（`main.rs:596`）随 crate 平移；它必须在 daemon
  创建项目内状态**之前**调用，否则脏 checkout 会让第一次委派失败。

TUI 的 driver 改调这些函数，行为零变化。

### 2.3 app-server 接线

**依赖：** `crates/yi-agent-app-server/Cargo.toml` 加 `yi-agent-subagent`。

**per-cwd 缓存：** `server.rs` 的 `run_with` 新增

```rust
type ProjectRuntimes = Arc<Mutex<HashMap<PathBuf, Result<AttachedProjectRuntime, String>>>>;
```

按 canonical cwd 缓存；**失败也缓存**（存 `Err(原因)`），否则每个新 thread 都会重跑一遍
git 检查与 daemon 起停尝试。

**`yi-agent-runtime` 暴露装配部件：** app-server 需要「换掉 registry 的三个组成部分」
（工具集、skills catalog 句柄、system prompt）而**不重建 provider 与权限通道**——重建
provider 会重新读一次配置/密钥，重建权限通道会打断正在等待审批的 turn。
`AgentBootstrap`（`crates/yi-agent-runtime/src/bootstrap.rs:246`）因此增加：

- `tools: Arc<yi_agent_core::ToolRegistry>` —— 现在它不暴露 registry，app-server 拿到 agent
  后无法再往里加工具；
- `catalog: Option<SkillsCatalogHandle>` —— 移植自 `app-server/src/server.rs:391` 那条
  「attached 时 handle 传 `None`」的既有规避，共享 crate 让它可以直接被传递。

（其余字段 providers / permission / decision channels / yolo 已存在，不动。）
`bootstrap_agent`（`:277`）两个分支各填两行。

**工具集重建助手**（`yi-agent-subagent` 内，两个客户端共用同一逻辑）：

```rust
pub fn attach_root_tools(
    cfg: &RuntimeConfig,
    runtime: &AttachedProjectRuntime,
    catalog: Option<SkillsCatalogHandle>,
) -> Result<Arc<ToolRegistry>, String>
```

以 `runtime.workspace_root` 为造 `build_tool_setup_in` 的 `workspace`（skills 与 MCP 的项目根
仍取 `cfg.workdir`），再 `register_attached_root_tools` 叠加六个委派工具。TUI 现在的
`build_tui_root_tools`（`main.rs:740`）与 `load_permission_checker_for_workdir_async`
（`main.rs:965`，异步加载以免阻塞 driver）也改为调用它，从而消除第三份重复。

**每个 thread 建 agent 时**（`server.rs:391` 与 `:579` 两处 `build_agent` 调用）：

1. 查/建该 cwd 的 `AttachedProjectRuntime`（**释放 `build_agent` 的调用方锁**，见下）；
2. 成功 → `attach_root_tools(cfg, &runtime, built.catalog.clone())` 取得新 registry，权限检查器
   改用 `load_permission_checker_with_switch(&runtime.workspace_root, yolo_switch)`
   （`crates/yi-agent-runtime/src/bootstrap.rs:348`）以**与沙箱共用同一份 `YoloSwitch`**
   （否则运行期切 YOLO 时权限层与沙箱层会脱钩），最后用新 registry + 新 checker 重建 agent：
   provider 复用 `built` 里的 provider，decision channel 复用 `built.decision_tx` / `decision_rx`，
   session 用 `agent.session()` 取回；
3. 失败 → 记 trace，返回未改动的 agent（工具集与权限都保持现状）。

**锁边界（必须遵守）：** `build_agent` 的调用方持有 `threads` 的 `Mutex` 守卫与
`threads.get_mut(&thread_id)` 的可变借用（`server.rs:391` / `:579`）。attach 必须在该作用域
**之外**或**之前**完成（顶层 `AttachedProjectRuntime` 只经 `Mutex` 访问、clonable，不触碰
`threads`），否则会自我死锁。这也是 `ProjectRuntimes` 必须是 `Arc<Mutex<..>>` 而非借用
`threads` 的原因。

**`turn/start` 与激活：** 该 thread 首个 turn 前 `activate_root(objective = 该轮 prompt)`，
与 TUI"首次提交即激活"一致；失败只记 trace，不阻断本轮。**刻意不在 attach 时用占位
objective 激活**：
`activate_application_root` 会把 objective 写进 root 任务（`crates/yi-agent-store/src/runtime.rs:917`），
桌面端首次提交的 prompt 才是真正的 objective。

**激活并发（必须处理）：** 激活是同步 socket 调用（毫秒级，最坏受 30s 写超时约束，见
`docs/superpowers/specs/2026-09-28-ipc-response-frame-truncation-design.md`），而它发生在
**主循环**里（`turn/start` 分支）——多个 thread 同时首轮会让主循环被其中一个阻塞，卡住
所有 thread 的请求处理。因此把激活放进**该 thread 自己的 driver task**：`TurnPrompt` 上按
需携带"待激活 root"，driver 在 `agent.run()` 之前用 `tokio::task::spawn_blocking` 完成激活，
一个 thread 的激活不会阻塞请求循环。激活是幂等的（
`activate_application_root` 对 `running` root 直接返回 `Ok`），所以每个 driver 各自调用一次
是安全的。

**清理：** `thread/delete` 与显式关闭路径上 `detach_root`（与 TUI 退出时 detach 对齐）。

**已知缺口（写出来，实现时一并处理）：** app-server 当前**没有进程退出钩子**——sidecar 的
stdin 关闭时主循环只是 `break` 返回，主循环里也拿不到被主循环独占的 `threads` 之外的
AttachedRoot 清单，且退出时 `Daemon` 的 drop 会停止内嵌 daemon（TUI 依赖同一性质）。
因此本设计把退出路径的 detach 列为**实现阶段必须解决的缺口**，倾向的最小做法是：把
per-cwd 的 `AttachedProjectRuntime` 也存进一个顶层 `Arc<Mutex<..>>`，在主循环
`while` 退出后逐个 `detach_root`；若实现时发现代价过大，则明确接受"进程退出即断连、
root 由 daemon 侧按既有规则收敛"并把该取舍写进实现计划的偏差记录。

### 2.4 失败与降级语义

daemon 起不来、attach 被拒 → **一律静默降级**：`tracing::warn!` 记原因，agent 少几个工具，
其余一切照常。

**非 git 目录（默认 cwd 是 `$HOME`）不是 attach 失败，实测有三段不同的行为**（与 TUI
完全一致，因为两端口走同一份 `attach_project_runtime`）：

1. **attach 成功**：application root 本来就可以原地建立（`attach_application_root` 的既有
   测试 `non_git_application_root_can_be_reattached` 已经钉住这一点），非 git 项目没有
   checkout 可搬，root 就地运行，`workspace_root == project_root`。
2. **六个工具照常注册**：`register_attached_root_tools` 只看 attach 结果，不看项目是不是
   git 仓库。
3. **真正委派时才失败**：daemon 的 worker 准入要求一个可恢复的 `worktree:` lease
   （`repositories` 侧 `validate_recovery_context`），非 git 项目给不出，`spawn_agent` 返回
   `Error { code: Internal }`。

所以「非 git 目录下没有委派工具」这个前提是错的：工具在、调用会被拒。判定依据是
`cargo test -p yi-agent-app-server a_non_git_cwd_attaches_in_place_with_the_delegation_tools`
（断言 attach 成功、激活成功、工具齐全，且 spawn 被明确拒绝而不是半准入）。**结论不变**：
要在桌面端真正起子 Agent，仍需选一个 git 项目目录。

不新增协议通知、不加前端 banner、不加错误气泡：`spawn_agent` 就是普通工具，经
`crates/yi-agent-app-server/src/translate.rs` 变 `toolCall` item，桌面端已有卡片渲染
（`desktop/src/components/ToolCallCard.tsx`）。前端零改动。

**必须知晓的后果：** 默认 cwd（`$HOME`）不是 git 仓库，所以在桌面端若不显式选一个 git
项目目录，委派仍不可用（与 TUI 的同款降级：起不来就少工具，不阻断会话）。

### 2.5 与 TUI 共享 pref 偏好（刻意不做）

TUI 的 `.yi-agent/preferences.json` 三态（`crates/yi-agent/src/tui/runtime_prefs.rs`）决定
TUI 是否 `y` 询问/自动启动/禁用。本设计**不**让 app-server 读取它：桌面端没有询问入口，
`Ask` 在 TUI 是"弹窗"，在桌面端没有对应物；而读 `Never` 会让桌面端静默失去委派、无从解释。
两者的耦合留待 GUI 开关（本设计范围外）一并解决：那时需要一个显式的桌面端开关，pref 才
有意义。

---

## 3. 测试

**平移带走的测试**：`cargo test -p yi-agent-subagent`（原 `subagent_runtime.rs` 的单测 +
`tui/subagents.rs` 的 `attached_tui_root_exposes_subagent_tools_without_a_delegate_command`）。

**app-server 新增确定性测试**（临时 git repo 作 cwd、临时 Unix socket + SQLite、mock
provider，不调真实 LLM）：

1. `a_git_project_gets_the_delegation_tools` —— 该 cwd 为 git repo 时，thread 的 registry
   含六个工具（即 `crates/yi-agent/src/main.rs` 的
   `build_tui_root_tools_registers_subagent_tools_for_attached_runtime` 的 app-server 版）；
2. `a_non_git_cwd_attaches_in_place_with_the_delegation_tools` —— 非 git cwd 下 attach 成功、
   六个工具齐全、激活成功，但 `spawn_agent` 被 daemon 明确拒绝（见 §2.4 修正）；
3. `two_threads_in_one_cwd_share_one_attached_runtime` —— 同 cwd 两个 thread 只 attach 一次，
   复用同一个 `Arc`；
4. 端到端装配契约放在共享 crate 里（两个前端共用，故不属于 app-server）：
   `crates/yi-agent-subagent/tests/attach_delegation.rs` —— attach → 激活两次（验幂等）→
   六工具齐全 → `SpawnApplicationChild` 落在真实 git 项目上被接纳 → detach。

**回归**：`cargo test -p yi-agent-app-server`、`cargo test -p yi-agent --bin yi-agent`
（守平移）、`cargo test -p yi-agent-runtime`、`cd desktop && npx tsc --noEmit && npm test`。

---

## 4. 文档

- 新增 `docs/project-management/yi-agent-subagent.md` 并在
  `docs/project-management/README.md` 模块索引表登记一行（新 crate 规则）。
- 更新 `docs/project-management/desktop.md`（委派可用、附验证命令）、
  `yi-agent-app-server.md`（per-cwd runtime attach）、`subagent-runtime.md`（共享 crate）、
  `yi-agent-runtime.md`（`AgentBootstrap.tools`）。
- `docs/bug-list.md` 那条「Mac desktop 版本的 yi-agent app 上面，没法起 subagent」改 `[x]`，
  附根因、修复位置与验证命令。

---

## 5. 已知代价与风险

- **平移 diff 约 3k 行**：机械但大，审查成本主要在这一块；收益是 TUI 与桌面端此后共用同一份
  daemon 工厂，不会再漂移。
- **首次 attach 阻塞请求循环**：Unix socket + SQLite，每个 cwd 一次（激活已移入 driver task，
  见 §2.3）。
- **默认 cwd 非 git 仓库仍不可用**（§2.4），需用户在 GUI 里选一个 git 项目目录。
- **六个工具需要 git repo 才能工作**：非 git 项目下工具在、但 daemon 的 worker 准入给不出
  `worktree:` lease，`spawn_agent` 返回 `Error { code: Internal }`。这是运行时既有行为
  （TUI 同样如此），本设计不改；差异只在于文档此前把它误记成「工具不注册」。

---

## 6. 完成判据

1. 在 **git 项目目录**下新建 thread，发一句要求委派的 prompt（如"用 spawn_agent 起一个只读
   子任务调研 X"），桌面端对话里出现 `toolCall` 卡片 `spawn_agent` 并返回 `task_id`。
2. 该 thread 的 tool registry 含六个委派工具：`cargo test -p yi-agent-app-server
   daemon_attached_thread_`。
3. 非 git 目录（如 `$HOME`）下新建 thread 一切照常（有工具，但 `spawn_agent` 会被 daemon
   拒绝）：`cargo test -p yi-agent-app-server a_non_git_cwd_attaches_in_place`。
4. TUI 行为零回归：`cargo test -p yi-agent --bin yi-agent`、`cargo test -p yi-agent-subagent`。
5. 前端零改动成立：`cd desktop && npx tsc --noEmit && npm test`。
