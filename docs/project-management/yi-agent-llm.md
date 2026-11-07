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

- [x] 超时语义改为「无输出才计时」 — 两个 provider 原先用 `reqwest::Client::builder().timeout(300s)`，那是**总期限**（从开始连接一直到响应体读完），一条正常输出的长回答到点必被砍断（现象：长回答/长思考在 300 秒处变成 stream error）。现改为 `connect_timeout` + `read_timeout`：每次读到数据就重置，真静默了才判超时；「无输出」的判定继续由 agent 层的 `think_idle_timeout`(60s) 负责。验证：`cargo test -p yi-agent-llm --test integration a_stream_that_keeps_producing_output_is_not_cut_off_by_a_total_deadline a_silent_stream_still_times_out`（两条互为约束：前者保证忙流不被砍，后者保证静默不挂死）
- [x] AnthropicProvider 设计 — `crates/yi-agent-llm/src/anthropic/` 目录存在 — [设计](../plans/2026-07-19-yi-agent-llm-design.md)
- [x] AnthropicProvider 实现 — `anthropic/{types,stream,client}.rs` + wiremock 测试通过 — [实现](../plans/2026-07-19-yi-agent-llm-impl.md)
- [x] OpenAI provider — `crates/yi-agent-llm/src/openai/` 目录存在 + wiremock 测试通过 — [实现](../plans/2026-07-24-openai-provider-impl.md)
- [x] 流式请求默认总超时 5 分钟 — `anthropic/client.rs` 与 `openai/client.rs` 的 `DEFAULT_TIMEOUT_SECS = 300`，并有单元测试验证 — `cargo test -p yi-agent-llm --lib default_stream_timeout_is_five_minutes`
- [x] 多模态图片序列化 — `anthropic/types.rs` 的 `ContentBlock::Image` → `AnthropicContentBlock::Image`（含 tool_result 内嵌图片，`detail` 被忽略）；`openai/types.rs` 新增 `OpenaiContentPart::{Text,ImageUrl}`，user 消息含图时输出 content 数组，tool 结果里的图片拆成 `role:tool`（文本 + `[image attached in the following message]` 占位）+ 紧随的 `role:user`（`image_url` parts，因为 Chat Completions 的 tool 消息不能带图）；验证：`cargo test -p yi-agent-llm --lib openai::types anthropic::types`、`cargo test -p yi-agent-llm --test openai_integration` — [设计](../superpowers/specs/2026-09-26-view-image-tool-design.md)
- [x] 流式传输错误类型化分类 — `anthropic/stream.rs` 与 `openai/stream.rs` 在仍持有 `reqwest::Error` 时用 `is_timeout()` 分类（其 Display 为 `error decoding response body`，无 `timed out` 字样）：超时 → `ProviderError::Network`（可重试），其余传输中断 → `ProviderError::Stream`（终结）；两个 client 的 `map` 改为 `ProviderEvent::StreamError(e)`，`scan` 终态守卫同时 latch `Stop` 与 `StreamError`（否则失败流会一直 yield 到服务端关闭）；验证：`cargo test -p yi-agent-llm --test integration request_timeout_is_a_total_deadline_not_an_idle_timeout` — [设计](../superpowers/specs/2026-09-26-think-idle-stall-retry-design.md) §9
- [x] reasoning effort 能力探测（裸 HTTP，绕开「被拒剥离重投」）— `probe.rs` 直接发一条**带 dummy tool**的最小请求：`ProbeChannel`（`:15`，`chat` / `responses` / `anthropic`）、`ProbeVerdict`（`:22`，`Supported`/`Unsupported`/`Indeterminate`/`Fatal`）、`EffortProbeTarget`（`:33`）、`probe_tools`（`:42`，`noop` 工具——真实 agent 请求永远带 tools，不带 tool 探出来的结论没有意义）、`probe_effort`（`:65`，2xx → `Supported`；400/422 且 body 点名 effort → `Unsupported`；其它 400/422 → `Indeterminate`；401/429/5xx/网络/超时 → `Fatal`）、`detect_openai_channel`（`:148`，`auto` 通道先判网关走 responses 还是 chat：404/405 且 body 空 → `Chat`，2xx → `Responses`）。**为什么必须绕过 provider**：provider 层遇到 effort 相关 400 会剥离字段重投（`openai/error.rs:72`、`anthropic/error.rs:38` 的 `is_effort_rejection`，现为大小写不敏感），重投成功后调用方只看到 2xx，拒绝被完全掩盖；判别性测试 `the_provider_path_would_hide_an_effort_rejection_but_the_probe_does_not`（`probe.rs:330`）在同一 wiremock 上同时跑两条路径证明这一点。验证：`cargo test -p yi-agent-llm --lib probe::tests`（11 例）— [设计](../superpowers/specs/2026-10-19-reasoning-effort-capability-probe-design.md) §5

- [x] Anthropic `stop_reason: tool_use` 归一为 `EndTurn` — `anthropic/stream.rs` 的 `message_delta` 分支（原映射为 `Other("tool_use")`，而 agent 循环对 `Other` 直接中断，导致 Anthropic 的工具调用**从不执行**；OpenAI 侧 `finish_reason=tool_calls` 一直正确，属两条路径的行为漂移）。验证：`cargo test -p yi-agent-llm --lib a_full_tool_use_response_ends_with_end_turn`
- [x] usage 分片字段级合并 — `yi-agent-core/src/provider.rs` 的 `TokenUsage::merge_snapshot` + `accumulate_stream` 的 `Usage` 分支；Anthropic 的 `message_delta` 只带 `output_tokens`，此前「整体覆盖」会把 `message_start` 的 `input_tokens` 抹成 0，而自动压缩正是靠它判定（`agent.rs:1747`），后果是 Anthropic 会话**永不触发自动压缩**。验证：`cargo test -p yi-agent-core --lib merge_snapshot`
- [x] 空参数工具按 `{}` 处理 — `accumulate_stream` 的 `ToolUseEnd` 分支；Anthropic 对无参数工具可以完全不发 `input_json_delta`（只有 `content_block_start` + `content_block_stop`），此前空串被 `serde_json::from_str` 判为 `malformed tool use JSON` 直接失败。非空但被截断的 JSON 仍然报错。验证：`cargo test -p yi-agent-core --lib a_zero_arg_tool_call_without_arguments_parses_as_empty_object`
- [x] 跨 provider 能力契约测试 — `yi-agent-llm/tests/provider_contract.rs`，同一套语义用例（纯文本 / 单工具 / 多工具 / 文本+工具混排 / 无参数工具 / usage 合并 / 错误码）对 Anthropic 与 OpenAI 各断言一遍；三个修复点均已做变异验证（把修复回退即变红）。验证：`cargo test -p yi-agent-llm --test provider_contract`（7 例）— [设计](../superpowers/specs/2026-10-29-anthropic-tool-use-and-default-provider-design.md)
- [x] 进程级共享 HTTP 连接池 — 每个 provider 原先各建一个 `reqwest::Client`，于是**每个新会话的首条消息都要重付一次 TCP+TLS 握手**（本机到网关实测 TCP 0.26s + TLS 0.82s ≈ 1.08s，即用户感知的「建连要 1 秒」；复用同连接后 TTFB 从 2.28s 降到 0.25s）。新增 `crates/yi-agent-llm/src/http.rs`：`shared_client(base_url, timeout)` 用 `OnceLock<Mutex<HashMap<(String, Duration), reqwest::Client>>>` 缓存，`OpenaiProvider::new` 与 `AnthropicProvider::new` 改为从该池取。**`timeout` 在键里**——它被烤进 client，探测用 15s、正式对话用 300s，混用会让 15s 缩短读超时；**`api_key` 不在键里**——它是每请求 header，与连接复用无关。构建在锁外、`entry().or_insert()` 双检，避免并发首次调用各持一个池而永久失去复用。同时把 `ChannelCache` 从「每 provider 一份」（注释称进程级、实现却不是）提升为真正的进程级单表（`openai/channel.rs` 的 `CHANNELS: OnceLock<...>`），使 `api_style=auto` 网关的 `/responses` 探测只发生一次，不再每个新会话多付一次 404 往返。范围**只含** openai + anthropic 两个 provider；`probe.rs` 与 `yi-agent-tools` 的 web client 刻意不动。验证：`cargo test -p yi-agent-llm --test shared_client`（6 例：同键复用 1 条连接 / 跨 provider 复 1 条 / 不同 timeout 分池 / 不同 base_url 隔离 / `shared_client` 不做归一化 / 通道判定跨 provider 共享）；真实判据：连续开两个新会话，第二个首条消息的 `sending request → response received` 应显著下降，trace 中不再出现第二次 TLS 握手 — [设计](../superpowers/specs/2026-11-07-process-wide-shared-http-client-design.md)、[计划](../superpowers/plans/2026-11-07-shared-http-client.md)
- [ ] 本地模型 (Ollama) provider — 无 `crates/yi-agent-llm/src/ollama/`
- [ ] Bedrock / Vertex AI 适配 — 无对应模块
- [ ] 流断连重连 — llm crate 内仍无重连逻辑（超时重试已由 core 的 attempt 循环承担，见 [yi-agent-core](./yi-agent-core.md)；连接重置按设计不重试）
