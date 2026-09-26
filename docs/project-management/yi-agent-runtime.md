# yi-agent-runtime（共享运行时）

## 模块说明

`yi-agent-runtime` 是配置加载与 Agent 装配的共享 crate，供 CLI（`yi-agent`）与
桌面 app-server（`yi-agent-app-server`）复用，避免两处装配逻辑漂移。它从
`crates/yi-agent` 的 `config.rs` / `main.rs` 抽取而来，但**不依赖 clap**——CLI 负责
把 clap 解析结果转成 `ConfigOverrides` 再交给本 crate，因此 GUI 侧无需引入 CLI
参数解析。

## 范围边界

**做什么：**
- 配置加载：显式 override > 真实 env > `workdir/.yi-agent/.env` > `~/.yi-agent/.env` > 默认值
- provider 构造（anthropic / openai）
- 工具集装配：内置工具 + 进程工具（`ProcessManager`）+ skills + 系统提示词（含当前日期与 skills catalog）
- Agent 装配：provider + 工具 + 权限检查器 + 权限决定通道
- 面向 GUI 的脱敏配置视图

**不做什么：**
- 不做 clap 参数解析（生产依赖中不含 clap，仅 dev-dependency 用于编译器守卫）
- 不做 TUI（由 yi-agent-tui 负责）
- 不做 app-server 协议与传输（由 yi-agent-app-server 负责）
- 不做会话持久化（YAGNI，路线图 P1）

## Features

- [x] crate 骨架 + workspace 注册 — `yi-agent-rs/crates/yi-agent-runtime/`；`Cargo.toml` members 含 `crates/yi-agent-runtime`
- [x] `RuntimeConfig` + `ConfigOverrides` + `load()`（env/.env 层级合并，不依赖 clap）— `src/config.rs:13` / `src/config.rs:34` / `src/config.rs:169`
- [x] sandbox 手写解析 + 变体漂移编译器守卫 — `src/config.rs:64`，测试 `src/config.rs:486`
- [x] `redacted_view()`（api_key 脱敏，供 GUI `config/read`）— `src/config.rs:326`
- [x] `build_provider()`（anthropic / openai）— `src/bootstrap.rs:11`
- [x] `build_tool_setup()` / `build_tools()` / `build_system_prompt()`（内置工具 + 进程工具 + skills + 当前日期；`naked` 返回空 registry + `None` prompt）— `src/bootstrap.rs:68`
- [x] `bootstrap_agent()` + `PermissionMode` + 权限决定通道（`Interactive` 保留双向通道，`AutoAllow` 关闭通道使黑名单命令解析为 Deny）— `src/bootstrap.rs:182`
- [x] 装配辅助 API：`build_prompt_setup()`（skills + system prompt）、`build_tool_setup_in()`（指定 workspace 根）、`build_agent_config()`、公开 `load_permission_checker()` — `src/bootstrap.rs:41` / `src/bootstrap.rs:97` / `src/bootstrap.rs:166` / `src/bootstrap.rs:231`
- [x] CLI 装配迁移到本 crate — `crates/yi-agent/src/main.rs` 的 provider（4 处：`control_schedule` / daemon worker / TUI / headless）、skills + system prompt、工具集、AgentConfig 装配全部委托本 crate，消除重复 — 验证：`cargo test -p yi-agent --bin yi-agent`（350 个测试）

**验证命令：** `cargo test -p yi-agent-runtime`（49 个测试）
