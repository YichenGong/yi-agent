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
- 不做流式 SSE 对接（由 provider 层负责）
- 不做会话持久化（YAGNI）

## Features

- [x] `yi-agent run` 子命令定义与 dispatch — [实现](../plans/2026-07-26-real-llm-testing-impl.md)
- [x] Headless 事件 drain（stdout/stderr，流式拼接 AssistantText）
- [x] `--json` JSONL 输出（`AgentEvent: Serialize`）
- [x] `--stdin` 追加输入
- [x] `--naked` 裸模型模式 — [设计](../plans/2026-07-26-run-naked-flag-design.md)
- [x] 配置层级合并（全局 + 本地）— [设计](../plans/2026-07-25-config-layering-design.md)
- [x] 真实 LLM 端到端测试（`#[ignore]` gate，`just test-real-e2e`）— [设计](../plans/2026-07-26-real-llm-testing-design.md)
