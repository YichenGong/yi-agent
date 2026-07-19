# yi-agent-llm

## 模块说明

yi-agent-llm 是 yi-agent 的 LLM provider 实现层。基于 `yi-agent-core` 的 `Provider` trait,初期实现 Anthropic Claude provider(Messages API + 流式 SSE),架构上预留多 provider 扩展能力。

## 范围边界

**做什么:**
- Anthropic Messages API (streaming SSE) 接入
- Provider 配置(base_url / api_key / api_version / timeout 多来源优先级)
- SSE 流解析 + ProviderEvent 映射
- HTTP 错误码到 ProviderError 的映射

**不做什么:**
- 不做重试逻辑(YAGNI)
- 不做流断连重连(YAGNI)
- 不做 OpenAI / Ollama provider(后续)
- 不做 Bedrock / Vertex AI 适配(后续)
- 不做 tracing 日志(YAGNI)

## Features

- [x] AnthropicProvider 设计 — [设计](../plans/2026-07-19-yi-agent-llm-design.md)
- [x] AnthropicProvider 实现(core `model` 字段 + types + stream + client + 测试)— [实现](../plans/2026-07-19-yi-agent-llm-impl.md)
- [ ] OpenAI provider
- [ ] 本地模型 (Ollama) provider
- [ ] Bedrock / Vertex AI 适配
- [ ] 重试与流断连重连
