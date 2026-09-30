# yi-agent-subagent（共享子 Agent 运行时装配）

## 模块说明

`yi-agent-subagent` 是「应用侧」子 Agent runtime 的共享 crate：它把项目 daemon 的
worker 工厂、daemon 客户端（attach / activate / detach）与六个委派工具
（`spawn_agent` / `send_message` / `wait_agent` / `inspect_agent` / `cancel_agent` /
`review_agent`）实现一次，供 CLI（`yi-agent`）与桌面 app-server
（`yi-agent-app-server`）共用。

它从 `crates/yi-agent/src/subagent_runtime.rs` 整体平移而来（该文件已删除）。抽取的
动机是**停止漂移**：此前 app-server 完全不知道 daemon 的存在，桌面端因此根本没有
`spawn_agent`，而 TUI 有——同一份能力在两个前端里存在与否，取决于各自的装配代码。

## 范围边界

**做什么：**
- daemon 侧 worker 工厂 `DaemonAgentWorkerFactory`（provider、skills-only registry、
  sandbox、项目 workspace、per-child model）
- daemon 客户端：`attach_project_runtime` / `activate_root` / `detach_root`
  （`AttachFailure` 带失败阶段，供降级路径记录）
- 六个委派工具的注册：`register_attached_root_tools`（application root 版）与
  `register_application_subagent_tools`
- `AttachedRoot`（session id / root task id / capability / workspace）
- 项目 runtime 目录解析 `project_runtime_directory`（读 `YI_AGENT_RUNTIME_DIR`，
  否则 `<workdir>/.yi-agent/runtime`）与 `ignore_project_local_runtime_state`

**不做什么：**
- 不做 daemon 本身（`yi-agent-store` 负责持久化、IPC、调度）
- 不做「当前 root」的单例：TUI 用进程级单例（一进程一 root），app-server 用 per-cwd
  缓存（一进程多项目），二者刻意不同，故由各应用自己持有
- 不做 attach 的决策（是否启用、何时询问用户）：TUI 由
  `tui/runtime_prefs.rs` 的三态偏好决定，app-server 直接尝试并降级

## Features

- [x] crate 骨架 + workspace 注册 — `yi-agent-rs/crates/yi-agent-subagent/`；
  `yi-agent-rs/Cargo.toml` 的 members 与 `workspace.dependencies` 均含本 crate
- [x] daemon 工厂与六个委派工具整体平移 — `src/lib.rs`（`DaemonAgentWorkerFactory`
  `src/lib.rs:42`、`DaemonWorkspaceService` `src/lib.rs:217`、`register_attached_root_tools`
  `src/lib.rs:1062`）；平移前该文件在 `crates/yi-agent/src/subagent_runtime.rs`，无
  `crate::` 内部引用，故可整块搬移；验证：`cargo test -p yi-agent-subagent`
- [x] 共享 attach 客户端 — `src/attach.rs`：`attach_project_runtime(cfg, runtime_dir)`
  `src/attach.rs:115`（收 **runtime 目录**而非 socket 路径，测试因此能隔离单个项目的
  runtime 而不动进程级环境变量）、`activate_root` `src/attach.rs:193`（幂等）、
  `detach_root` `src/attach.rs:216`（best-effort，失败只记 trace）；`AttachFailure`
  `src/attach.rs:18` 的 `stage`（`runtime directory` / `worker factory` / `daemon start`
  / `attach`）让降级路径能说清是哪一步断了；验证：`cargo test -p yi-agent-subagent --test attach_delegation`
- [x] 端到端装配契约 — `tests/attach_delegation.rs`：干净 git 项目 attach → 激活两次
  （证明幂等）→ 注册的委派工具存在 → `SpawnApplicationChild` 被 daemon 接纳并返回
  child task id → detach；这是 TUI 与 app-server 共用的那条链路的可执行说明

**边界（两端口一致，故记在这里）：** attach 与工具注册**不看**项目是不是 git 仓库（非
git 目录原地成 root，`workspace_root == project_root`），真正需要仓库的是后续 `spawn_agent`
——daemon 的 worker 准入要求可恢复的 `worktree:` lease。因此非 git 项目下「工具在、调用被拒」。
app-server 侧的钉法是 `cargo test -p yi-agent-app-server
a_non_git_cwd_attaches_in_place_with_the_delegation_tools`。

**验证命令：** `cargo test -p yi-agent-subagent`（28 个单元测试 + 1 个集成测试）
