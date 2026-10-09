# yi-agent-cli（CLI 二进制）

## 模块说明

`yi-agent-cli` 模块对应 `yi-agent-rs/crates/yi-agent/` 这个**二进制 crate**（产物名
`yi-agent`）。本文件只记录「CLI 自身」的跨领域关注点：顶层子命令语法、共享参数、
以及 shell 补全等面向终端用户的命令行体验。各子命令的**业务语义**仍归各自的领域
文档负责（如 `yi-agent run` → [yi-agent-run](./yi-agent-run.md)，app-server 子命令 →
[yi-agent-app-server](./yi-agent-app-server.md)，`agents`/`agent`/`daemon`/`schedule` →
[subagent-runtime](./subagent-runtime.md)）。

## 范围边界

**做什么：**
- clap 参数语法定义（`crates/yi-agent/src/config.rs` 的 `Cli` / `Command`）
- 顶层 dispatch（`crates/yi-agent/src/main.rs::main`）
- 面向用户命令行的辅助体验：shell 补全脚本生成

**不做什么：**
- 不做配置加载语义（由 `yi-agent-runtime` 拥有，见 [yi-agent-runtime](./yi-agent-runtime.md)）
- 不做各子命令的业务实现（归各自领域文档）
- 不做动态补全（补全运行时的模型名 / 会话 id / task id —— 当前未实现）

## Features

- [x] Shell 补全脚本生成 — `yi-agent completions <shell>` 子命令（`crates/yi-agent/src/config.rs::Command::Completions`，dispatch 在 `main.rs::print_completion`），基于 `clap_complete` 从 CLI 语法派生静态补全脚本，支持 `bash` / `elvish` / `fish` / `powershell` / `zsh`（由 `clap_complete::Shell` 提供）。补全内容覆盖顶层 flag（`--provider` / `--workdir` / `--sandbox` 等）、子命令及 `value_enum` 候选值（如 `--sandbox` 的 `read-only` / `workspace-write` / `danger-full-access`）。为保持脚本纯净,该子命令**跳过 tracing 初始化**（`main.rs::main` 中 `matches!(cli.command, Some(Command::Completions { .. }))` 时 `_trace_guard` 为 `None`），确保 stderr 无任何日志行。安装方式(以 zsh 为例)：`yi-agent completions zsh > "${fpath[1]}/_yi-agent"`；bash：`yi-agent completions bash > /usr/local/etc/bash_completion.d/yi-agent`。验证：`cargo test -p yi-agent --test cli_completions`

- [x] TUI 首启问答 + `yi-agent run` 未配置指引 — 新增 `crates/yi-agent/src/onboarding_prompt.rs`（`should_offer` / `provider_defaults` / `run_onboarding_prompt`：交互式收集 provider / api_key / model / api_url、可选连接测试、把字段写进全局 `~/.yi-agent/.env`），`run_agent` 首启自动弹问答；`yi-agent run`（headless）未配置时经 `unconfigured_guidance` 打印中文指引并以非零码退出（判定基于 `run` 实际加载出的配置，含全局 `.env` / 进程环境变量 / CLI 覆盖，不会误拦已配置机器）。验证 `cargo test -p yi-agent --bin yi-agent onboarding_prompt` 与 `cargo test -p yi-agent --bin yi-agent guidance` — [设计](../superpowers/specs/2026-10-09-first-run-onboarding-design.md)

**验证命令：** `cargo test -p yi-agent --test cli_completions`（4 个测试）
