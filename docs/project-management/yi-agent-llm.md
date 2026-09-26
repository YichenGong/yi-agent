# yi-agent-llm

## 模块说明

yi-agent 的 LLM provider 实现层。基于 `yi-agent-core` 的 `Provider` trait,实现 Anthropic Claude 和 OpenAI provider,架构上预留多 provider 扩展能力。

## 范围边界

**做什么:**
- Anthropic Messages API (streaming SSE) 接入
- OpenAI Chat Completions API (streaming SSE + tool calling) 接入
- Provider 配置(base_url / api_key / api_version / timeout 多来源优先级)
- SSE 流解析 + ProviderEvent 映射
- HTTP 错误码到 ProviderError 的映射

**不做什么:**
- 不做重试逻辑(YAGNI)
- 不做流断连重连(YAGNI)
- 不做本地模型 / Ollama provider(后续)
- 不做 Bedrock / Vertex AI 适配(后续)
- 不做 tracing 日志(YAGNI)

## Features

- [x] AnthropicProvider 设计 — `crates/yi-agent-llm/src/anthropic/` 目录存在 — [设计](../plans/2026-07-19-yi-agent-llm-design.md)
- [x] AnthropicProvider 实现 — `anthropic/{types,stream,client}.rs` + wiremock 测试通过 — [实现](../plans/2026-07-19-yi-agent-llm-impl.md)
- [x] OpenAI provider — `crates/yi-agent-llm/src/openai/` 目录存在 + wiremock 测试通过 — [实现](../plans/2026-07-24-openai-provider-impl.md)
- [x] 流式请求默认总超时 5 分钟 — `anthropic/client.rs` 与 `openai/client.rs` 的 `DEFAULT_TIMEOUT_SECS = 300`，并有单元测试验证 — `cargo test -p yi-agent-llm --lib default_stream_timeout_is_five_minutes`
- [x] 多模态图片序列化 — `anthropic/types.rs` 的 `ContentBlock::Image` → `AnthropicContentBlock::Image`（含 tool_result 内嵌图片，`detail` 被忽略）；`openai/types.rs` 新增 `OpenaiContentPart::{Text,ImageUrl}`，user 消息含图时输出 content 数组，tool 结果里的图片拆成 `role:tool`（文本 + `[image attached in the following message]` 占位）+ 紧随的 `role:user`（`image_url` parts，因为 Chat Completions 的 tool 消息不能带图）；验证：`cargo test -p yi-agent-llm --lib openai::types anthropic::types`、`cargo test -p yi-agent-llm --test openai_integration` — [设计](../superpowers/specs/2026-09-26-view-image-tool-design.md)
- [x] 流式传输错误类型化分类 — `anthropic/stream.rs` 与 `openai/stream.rs` 在仍持有 `reqwest::Error` 时用 `is_timeout()` 分类（其 Display 为 `error decoding response body`，无 `timed out` 字样）：超时 → `ProviderError::Network`（可重试），其余传输中断 → `ProviderError::Stream`（终结）；两个 client 的 `map` 改为 `ProviderEvent::StreamError(e)`，`scan` 终态守卫同时 latch `Stop` 与 `StreamError`（否则失败流会一直 yield 到服务端关闭）；验证：`cargo test -p yi-agent-llm --test integration request_timeout_is_a_total_deadline_not_an_idle_timeout` — [设计](../superpowers/specs/2026-09-26-think-idle-stall-retry-design.md) §9
- [ ] 本地模型 (Ollama) provider — 无 `crates/yi-agent-llm/src/ollama/`
- [ ] Bedrock / Vertex AI 适配 — 无对应模块
- [ ] 流断连重连 — llm crate 内仍无重连逻辑（超时重试已由 core 的 attempt 循环承担，见 [yi-agent-core](./yi-agent-core.md)；连接重置按设计不重试）
