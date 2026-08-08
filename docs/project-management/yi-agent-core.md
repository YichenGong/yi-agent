# yi-agent-core

## 模块说明

yi-agent 的核心库，定义消息模型、工具系统、Provider 抽象和 Agent 主循环，不包含任何具体 provider 实现、工具实现或持久化实现。

## 范围边界

**做什么：**
- 定义核心 trait 和数据结构（Message、Tool、Provider、Agent）
- 实现 Agent 循环和工具调度
- 抽象 LLM Provider 接口（流式优先）

**不做什么：**
- 不绑定具体 LLM 厂商的 SDK（由 yi-agent-llm 负责）
- 不提供 CLI 入口（由 yi-agent CLI 负责）
- 不做持久化（由 yi-agent-store 负责）

## Features

- [x] 消息模型（Role, Message, ContentBlock）— `crates/yi-agent-core/src/message.rs` 定义全部类型 — [设计](../plans/2026-07-18-yi-agent-core-design.md)
- [x] Tool trait 与 ToolRegistry — `crates/yi-agent-core/src/tool.rs` 提供 `Tool` trait + `ToolRegistry` — [设计](../plans/2026-07-18-yi-agent-core-design.md)
- [x] Provider trait 与 ProviderEvent — `crates/yi-agent-core/src/provider.rs` 定义 `Provider` trait + `ProviderEvent` 流式事件 — [设计](../plans/2026-07-18-yi-agent-core-design.md)
- [x] Agent loop、Session、AgentEvent（并行工具执行）— `crates/yi-agent-core/src/agent.rs` 实现 think-act-observe 循环 — [实现](../plans/2026-07-18-yi-agent-core-impl.md)
- [x] ProviderRequest / AgentConfig 加 model 字段 — `provider.rs::ProviderRequest.model` + `agent.rs::AgentConfig.model` 可在请求级覆盖
- [x] 流式输出与中断处理 — `agent.rs` 用 `CancellationToken`，`run()` 后捕获 token 可取消 — [设计](../plans/2026-07-24-yi-agent-core-streaming-cancel-token-design.md)
- [x] Token 计数 — `AgentEvent::Usage` + `ProviderEvent::Usage` 携带 `TokenUsage` — [设计](../plans/2026-07-24-yi-agent-core-streaming-cancel-token-design.md)
- [x] 权限管理集成 — `agent.rs::request_permission()` 发送 `AgentEvent::PermissionRequest`/`PermissionResolved` — [设计](../plans/2026-07-25-permission-management-design.md) · [gaps 修复](../plans/2026-07-25-permission-gaps-impl.md)
- [x] 批量工具调用引导 — `agent.rs::default_system_prompt()` 内嵌"并行调用 / 串行 && "指引 — [设计](../plans/2026-07-25-batch-tool-call-prompt-design.md)
- [x] LLM 消息 tracing — `--debug` 时 `agent.rs` 打印 `think: request delta` / `think: response` debug 日志 — [设计](../plans/2026-07-25-trace-llm-content-design.md)
- [x] auto-compact — `agent.rs::run_loop` 每轮 THINK 前用上次 `Usage.input_tokens` 检测,超 `compact_threshold` 调 `compact::compact_session` 压缩 session 并发 `AgentEvent::AutoCompacting` — [设计](../plans/2026-07-26-auto-compact-design.md) · 测试 `auto_compact_triggers_when_threshold_exceeded` 等 7 个
- [x] Agent 完成语义与变更审计 — `agent.rs::DoneReason` 区分正常完成、截断和异常中断；`run_loop` 在变更型工具后要求一次 read/diff/build/test 审计；验证：`cargo test -p yi-agent-core --lib agent_`
- [ ] 图片工具（`ContentBlock::Image` 已留类型，无对应 Tool 实现）
- [ ] 插件系统（`ToolSource::Plugin` 枚举已留，无加载机制）
