# yi-agent-core

## 模块说明

yi-agent-core 是 yi-agent 的核心库，定义消息模型、工具系统、Provider 抽象和 Agent 主循环，不包含任何具体 provider 实现、工具实现或持久化实现。

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

- [x] 消息模型 (Role, Message, ContentBlock) — [设计](../plans/2026-07-18-yi-agent-core-design.md)
- [x] Tool trait 与 ToolRegistry — [设计](../plans/2026-07-18-yi-agent-core-design.md)
- [x] Provider trait 与 ProviderEvent — [设计](../plans/2026-07-18-yi-agent-core-design.md)
- [x] Agent loop、Session、AgentEvent（并行工具执行）— [实现](../plans/2026-07-18-yi-agent-core-impl.md)
- [ ] 流式输出与中断处理
- [ ] Token 计数（扩展 AgentEvent::Done）
- [ ] 图片工具（ContentBlock::Image 已留类型）
- [ ] 插件系统（基于 ToolSource::Plugin）
