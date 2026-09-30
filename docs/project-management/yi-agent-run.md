# yi-agent run（headless 模式）

## 模块说明

`yi-agent run` 是 yi-agent CLI 的非交互子命令，用于脚本化 / 端到端测试场景。将 `AgentEvent` 流 drain 到 stdout/stderr，支持 `--json` 切换为 JSONL 供程序化断言。`--naked` 可运行裸模型（无工具、无 skills、无系统提示词补丁）。同时承载 CLI 配置层级合并（全局 `~/.yi-agent/.env` + 本地 `.yi-agent/.env`）。

## 范围边界

**做什么：**
- `yi-agent run <prompt>` 非交互执行，drain AgentEvent 到终端
- `--json` 输出 JSONL（`AgentEvent` 实现 `Serialize`）
- `--stdin` 从 stdin 读取追加输入
- `--naked` 裸模型模式（跳过工具注册、skills 加载、系统提示词）
- 配置层级合并（本地 `.yi-agent/.env` 覆盖全局 `~/.yi-agent/.env`）
- `--workdir` 显式指定时不加载全局配置
- 真实 LLM 端到端测试基于此子命令（`tests/e2e_real.rs`）

**不做什么：**
- 不做交互式 TUI（默认无子命令时进入 TUI，由 yi-agent-tui 负责）
- 不做跳平台信号语义统一（非 unix 只处理 Ctrl+C；`SIGTERM` 的整组回收是 unix 专属）
- 不做流式 SSE 对接（由 provider 层负责）
- 不做会话持久化（YAGNI）

## Features

- [x] `yi-agent run` 子命令 — `crates/yi-agent/src/config.rs::Command::Run` 定义 + `main.rs` dispatch
- [x] Headless 事件 drain — `main.rs::run_headless()` 拼接 AssistantText 到 stdout/stderr
- [x] `--json` JSONL 输出 — `AgentEvent` 实现 `Serialize`，`--json` 切换输出格式
- [x] `--stdin` 追加输入 — `config.rs::Run.stdin` flag 读取 stdin
- [x] `--naked` 裸模型模式 — `main.rs` naked 分支跳过工具/skills/系统提示词 — [设计](../plans/2026-07-26-run-naked-flag-design.md)
- [x] 配置层级合并 — 由 `yi-agent-runtime/src/config.rs`（`RuntimeConfig::load`）实现本地 `.yi-agent/.env` 覆盖全局 `~/.yi-agent/.env` — [设计](../plans/2026-07-25-config-layering-design.md)
- [x] headless 工具集与 TUI 对齐 — `yi-agent-runtime/src/bootstrap.rs::build_tool_setup_in` 注册内置工具 + 进程工具 + `SkillTool`（`--naked` 除外，`--subagents` 时改用 `build_headless_root_tools`）；验证：`cargo test -p yi-agent --bin yi-agent build_headless_setup_`
- [x] 真实 LLM 端到端测试 — `crates/yi-agent/tests/e2e_real.rs` 用 `#[ignore]` gate — [设计](../plans/2026-07-26-real-llm-testing-design.md)
- [x] 复杂 one-shot 任务测试(Tier 3)— `crates/yi-agent/tests/e2e_complex.rs` 4 个场景 — [设计](../plans/2026-07-26-graded-test-system-design.md)
- [x] 信号中断会回收 bash 进程组 — `SIGINT` / `SIGTERM` 不再走默认处置（那会让 CLI 在工具 future 被丢弃前就退出，`bash` 的 `ProcessGroupGuard::drop` / `kill_on_drop` 都不执行，整组被 init 收养成孤儿且永不回收）。改为经 `shutdown_signal` 转入与 TUI / app-server 同一条取消路径：`agent.cancel()` → 继续 drain 到 `Cancelled` → 工具 future 被丢弃 → 整组被 SIGKILL，退出码 130（`--json` 与人读格式一致）。`main.rs::drain_with_signals` / `shutdown_signal` / `event_exit_code`；第二次信号仍走默认处置，用于「连按两次 Ctrl+C 立即终止」。验证：`cargo test -p yi-agent --test headless_signal_reap`（`SIGINT` / `SIGTERM` 各一，移除修复即两者同时失败）、`cargo test -p yi-agent --bin yi-agent drain_stream`
