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
- [x] 文件发现提示词护栏 — `crates/yi-agent-core/src/agent.rs::default_system_prompt()` 提醒避免 `glob({"path":".","pattern":"**/*"})`，优先 `rg --files` / 限定目录；验证：`cargo test -p yi-agent-core --lib agent::tests::default_system_prompt_discourages_unbounded_glob -- --exact`
- [x] 工具调用进度叙述提示词 — `crates/yi-agent-core/src/agent.rs::default_system_prompt()` 的 `Progress narration:` 段：每条带工具调用的响应先说 1-2 句在做什么，至少每约 10 次工具调用汇报一次，明确叙述不意味着拆分调用；验证：`cd yi-agent-rs && cargo test -p yi-agent-core --lib agent::tests::default_system_prompt_requires_progress_narration -- --exact`；见 [设计](../superpowers/specs/2026-09-30-tool-call-narration-and-card-summary-design.md)
- [x] LLM 消息 tracing — `--debug` 时 `agent.rs` 打印 `think: request delta` / `think: response` debug 日志 — [设计](../plans/2026-07-25-trace-llm-content-design.md)
- [x] Codex 式 auto-compact — `compact.rs::plan_compaction` 保留最多 20,000 token 的真实 User 输入、12,000 token 的完整工具后缀并生成 handoff 摘要；`agent.rs::Session` 在 pre-turn/mid-turn 基于持久化 `Usage.input_tokens` 触发；验证：`cd yi-agent-rs && cargo test -p yi-agent-core --lib compact::tests && cargo test -p yi-agent-core --lib 'agent::tests::auto_compact_'` — [设计](../superpowers/specs/2026-08-15-compact-history-redesign-design.md)
- [x] Agent 完成语义与变更审计 — `agent.rs::DoneReason` 区分正常完成、截断和异常中断；`run_loop` 在变更型工具后要求一次 read/diff/build/test 审计；验证：`cargo test -p yi-agent-core --lib agent_`
- [x] 显式可重试工具失败 — `tool.rs::Tool::retryable()` 默认关闭；`agent.rs` 仅对明确 opt-in 的工具最多重试两次并发出 `ToolRetry`；验证：`cargo test -p yi-agent-core --lib agent_retries_only_an_explicitly_retryable_tool_failure`
- [x] 图片工具类型与工具实现（`ContentBlock::Image` + `ImageDetail`，工具在 `yi-agent-tools::ViewImageTool`）— `crates/yi-agent-core/src/message.rs` 的 `ImageDetail { High, Original }`（`detail` 带 `#[serde(default)]`，缺省 High，旧会话可反序列化）；工具实现见 [yi-agent-tools](./yi-agent-tools.md)；验证：`cargo test -p yi-agent-core --lib message::` — [设计](../superpowers/specs/2026-09-26-view-image-tool-design.md)
- [x] THINK 阶段空闲停滞退避重试 — `provider.rs::StopReason::Stalled` + `agent.rs` attempt 循环（默认 3 次、2s/4s/8s，封顶 30s）；`AgentConfig.think_stall_retry_limit` / `think_stall_backoff_base` 可调；停滞 partial 不写入 session；`AgentEvent::ProviderRetry` 使重试对用户可见；验证：`cargo test -p yi-agent-core --lib agent::tests::agent_exhausts_stall_retries_then_reports_interruption` — [设计](../superpowers/specs/2026-09-26-think-idle-stall-retry-design.md)
- [x] 请求超时退避重试与类型化流错误 — `ProviderEvent::StreamError(ProviderError)` 把中途传输失败的类型化分类保留到 core；`provider.rs::StreamEnd::{Stopped,Failed}` 让超时路径也保留 partial（不写入 session）；`RetryCause::{IdleStall,RequestTimeout}` 区分两类瞬时失败并**共享**同一重试预算（默认 3 次、2s/4s/8s）；非法 SSE 载荷与连接重置仍终结不重试；验证：`cargo test -p yi-agent-core --lib agent::tests::agent_retries_a_request_timeout_and_completes` — [设计](../superpowers/specs/2026-09-26-think-idle-stall-retry-design.md) §9
- [x] 运行期可翻转的 `YoloSwitch` — `crates/yi-agent-core/src/autonomy.rs:9`（`YoloSwitch(Arc<AtomicBool>)`，`Clone` 让多个持有者共享同一原子位，`SeqCst`）；`new` `:12` / `get` `:17` / `set` `:22`；验证：`cargo test -p yi-agent-core --lib autonomy::` — [设计](../plans/2026-09-27-desktop-yolo-mode-design.md)
- [x] 轮次中途追加用户输入（`Inbox` + `Agent::interject` + `AgentEvent::InterjectionAccepted` / `InterjectionsReturned`）— `agent.rs` 新增 `Interjection { seq, text, tag }`、`InterjectError::{Full,NotRunning}`、`Inbox`（FIFO，`CAPACITY = 16`，`push` 单调分配 `seq`）与 `InboxHandle`（`Arc<Mutex<Inbox>>`，跨线程投递）；`Agent::interject(text, tag)` 在运行中受理、未运行返回 `NotRunning`；`run_loop` 在两个位置 drain：每次 provider 请求前（`turn += 1` 之后）与 `EndTurn` 判定前（审计关卡之前，消除结束竞态），注入文本带 `INTERJECTION_PREFIX` 折进当前轮次；每个终止出口（5 处 `Cancelled` + 6 处 `Done`）先发 `InterjectionsReturned` 把未消费文本还给调用方，顺序保证在 `Cancelled`/`Done` 之前；验证：`cargo test -p yi-agent-core --lib agent::tests::interjection_` 与 `cargo test -p yi-agent-core --lib agent::tests::cancel_returns_unconsumed_interjections_before_cancelled` — [设计](../superpowers/specs/2026-09-30-mid-turn-user-interjection-design.md)
- [x] 会话句柄与 fork 裁剪原语 — `Agent::session_handle`（`agent.rs:505`）返回活的 `Arc<Mutex<Session>>`，让工具在**调用时**读取调用者「此刻」的对话而非装配时快照；`Agent::with_session_arc(self, session)`（`agent.rs:510`）用调用者的同一 Arc 重建 Agent（`Arc::ptr_eq` 不变，持有句柄不失效），`Agent::set_session_messages`（`agent.rs:516`）就地把新消息写回同一会话（供 compact 使用，Arc 不变）。`Session::from_messages`（`agent.rs:71`）用完整转录预置一个会话（fork 子 worker 的起点）。`fork_prefix_len`（`agent.rs:703`）给出 provider 有效前缀：末条若为带 `tool_use` 的 assistant（无配对 `tool_result`）则丢弃它，否则全量；`forkable_messages`（`agent.rs:714`）返回该前缀，既作 `safe_cancel_truncate_len` 的唯一实现（cancel 语义不变），又是 fork 的唯一裁剪（只做协议有效性裁剪，**不按大小截断历史**）。验证：`cargo test -p yi-agent-core --lib 'agent::tests::fork_prefix_'`、`cargo test -p yi-agent-core --lib 'agent::tests::session_handle_stays_the_same_across_with_session_arc'`
- [ ] 插件系统（`ToolSource::Plugin` 枚举已留，无加载机制）
