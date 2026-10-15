# CoT 规范回传 + Anthropic adaptive thinking Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 按行业规范补齐三个 provider 通道的推理状态凭证（signature / encrypted_content）回传，并为 Anthropic 接入 adaptive thinking。

**Architecture:** 四层推进。(1) 数据模型：`ContentBlock` 新增 `Thinking` 变体，用 `ThinkingToken` 枚举区分四种凭证形态；(2) 传输：新增块级 `ProviderEvent::ThinkingBlock`（与展示用的 `ReasoningDelta` 严格分离），由 `accumulate_stream` 唯一构造 `ContentBlock`；(3) provider 适配：Anthropic 新增 thinking 请求字段 + 流解析（含分段 signature 拼接），DeepSeek chat 补 `reasoning_content`，Responses 补 reasoning item；(4) 生命周期：跨轮默认保留 + compaction 前缀校验兼容 + transcript 持久化 + protocol Item。

**Tech Stack:** Rust 2024（`yi-agent-core` / `yi-agent-llm` / `yi-agent-app-server` / `yi-agent-runtime` / `yi-agent`）、wiremock（provider 集成测试）、tokio-test。

设计依据：`docs/superpowers/specs/2026-10-15-cot-round-trip-and-anthropic-thinking-design.md`。

## Global Constraints

- **不在 `main` 直接改动**。实现走 worktree，分支建议 `feat/cot-round-trip`。
- 提交前在 `yi-agent-rs/` 跑 `cargo fmt --all`；commit message 用 conventional commits，首行 ≤72 字符，**不写** `Co-Authored-By`。
- 测试按 crate 跑（`cargo test -p <crate>`），避免 `--workspace` 全量。
- 后端改动不得让既有测试变红。每个任务结束时，受影响的 crate 测试须全绿。
- **零回归硬要求**：`reasoning_effort` 未配置时，各 provider 请求体中该字段不得出现。Anthropic 请求除 `thinking` / `output_config` / `max_tokens` 三个新增字段外，其余形状与今天一致（见 spec §5.1，thinking 无条件发送）。
- **凭证保真铁律**（spec §1.3）：凭证必须逐字回传，不重编码、不过滤；`display:"omitted"` 场景明文为空串但凭证必须完整；明文可丢，凭证不可丢。
- **明文不进摘要**：thinking 明文不得进入 compaction 摘要输入（spec §4.2）。
- worktree 内所有 `cargo` 命令用 worktree 自己的目录（`<worktree>/yi-agent-rs`）。

---

## File Structure

**修改（核心数据模型）**
- `yi-agent-rs/crates/yi-agent-core/src/message.rs` — 新增 `ContentBlock::Thinking` 与 `ThinkingToken`
- `yi-agent-rs/crates/yi-agent-core/src/provider.rs` — 新增 `ProviderEvent::ThinkingBlock` + `accumulate_stream` 处理
- `yi-agent-rs/crates/yi-agent-core/src/compact.rs` — token 估算、摘要渲染、前缀兼容
- `yi-agent-rs/crates/yi-agent-core/src/agent.rs` — prefill 估算

**修改（Anthropic）**
- `yi-agent-rs/crates/yi-agent-llm/src/anthropic/types.rs` — 请求字段 + 回传映射 + `max_tokens` 默认
- `yi-agent-rs/crates/yi-agent-llm/src/anthropic/stream.rs` — thinking 块流解析

**修改（OpenAI）**
- `yi-agent-rs/crates/yi-agent-llm/src/openai/stream.rs` — `reasoning_content` 解析
- `yi-agent-rs/crates/yi-agent-llm/src/openai/types.rs` — `OpenaiMessage.reasoning_content` 回传
- `yi-agent-rs/crates/yi-agent-llm/src/openai/responses/stream.rs` — reasoning item 解析
- `yi-agent-rs/crates/yi-agent-llm/src/openai/responses/types.rs` — reasoning item 回传

**修改（服务端与持久化）**
- `yi-agent-rs/crates/yi-agent-app-server/src/translate.rs` — 落盘 thinking、渲染占位
- `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs` — 新增 `Item::Thinking`
- `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs` — `ThreadMeta.preserve_thinking`
- `yi-agent-rs/crates/yi-agent-app-server/src/session.rs` / `server.rs` — 开关命令

**新增（辅助）**
- `yi-agent-rs/crates/yi-agent-core/src/thinking.rs` — 凭证剥离与保真辅助函数（集中一处，避免各 provider 漂移）

---

## Task 1: `ContentBlock::Thinking` 与 `ThinkingToken`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/message.rs:20-46`（`ContentBlock` 枚举）
- Modify: `yi-agent-rs/crates/yi-agent-core/src/message.rs`（新增 `ThinkingToken` 定义 + 测试）
- Modify: `yi-agent-rs/crates/yi-agent-core/src/compact.rs:86-96`（`estimate_block_tokens`）、`:384-415`（`format_messages_for_summary`）
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs:1671-1674`（prefill 估算）
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/types.rs:109-129`（`From<ContentBlock>`）
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/types.rs`（`extract_text` / `image_part`）
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/responses/types.rs`（`user_item` 等）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/translate.rs:481-491`（`render_content`）
- Modify: `yi-agent-rs/crates/yi-agent-mcp/src/manager.rs:536`、`yi-agent-rs/crates/yi-agent-tools/src/fs/view_image.rs:113`（补分支）

**Interfaces:**
- Consumes: 无（本任务是最底层）
- Produces:
  - `ContentBlock::Thinking { text: String, token: ThinkingToken }`
  - `ThinkingToken::{Signature(String), Redacted(String), Encrypted(String), None}`（在 `yi_agent_core` 导出）

- [ ] **Step 1: 写失败测试（凭证无损往返 + 变体可构造）**

在 `message.rs` 的 `mod tests` 内追加：

```rust
#[test]
fn thinking_block_roundtrips_all_token_kinds() {
    let cases = vec![
        ThinkingToken::Signature("sig-abc".into()),
        ThinkingToken::Redacted("red-xyz".into()),
        ThinkingToken::Encrypted("enc-123".into()),
        ThinkingToken::None,
    ];
    for token in cases {
        let block = ContentBlock::Thinking {
            text: "summary".into(),
            token: token.clone(),
        };
        let json = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(block, back, "thinking block must round-trip losslessly");
    }
}

#[test]
fn thinking_block_preserves_empty_text_with_token() {
    // display:"omitted" 场景：明文为空串，凭证必须仍在。
    let block = ContentBlock::Thinking {
        text: String::new(),
        token: ThinkingToken::Signature("sig-full".into()),
    };
    let json = serde_json::to_string(&block).unwrap();
    let back: ContentBlock = serde_json::from_str(&json).unwrap();
    match back {
        ContentBlock::Thinking { text, token } => {
            assert_eq!(text, "");
            assert_eq!(token, ThinkingToken::Signature("sig-full".into()));
        }
        other => panic!("expected Thinking, got {other:?}"),
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p yi-agent-core thinking_block -- --nocapture`
Expected: 编译失败，报 `cannot find variant Thinking` / `cannot find type ThinkingToken`

- [ ] **Step 3: 实现变体与枚举**

在 `message.rs` 的 `ContentBlock` 枚举内、`Image` 变体之后追加：

```rust
    Thinking {
        /// 可读摘要。`display:"summarized"` 时有值；`omitted` 或 redacted 时为空串。
        text: String,
        /// 不透明推理状态凭证。回传时必须逐字不变。
        token: ThinkingToken,
    },
```

在同文件 `ContentBlock` 定义之后追加：

```rust
/// 推理状态凭证的形态。
///
/// 注意：`Signature` 与 `Redacted` 都是 Anthropic 的凭证载荷，但序列化成**不同**的
/// `type` 值（`thinking` vs `redacted_thinking`），故必须区分为两个变体——单靠
/// 「有没有凭证」无法决定回传时的 `type`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThinkingToken {
    /// Anthropic 明文 thinking 块（text 可有内容，display 决定）。
    Signature(String),
    /// Anthropic redacted_thinking 块（text 恒为空，只有 data）。
    Redacted(String),
    /// OpenAI Responses 的 encrypted_content。
    Encrypted(String),
    /// 无凭证。DeepSeek 的 reasoning_content 按规范不需回传，明文仅供展示。
    None,
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p yi-agent-core thinking_block -- --nocapture`
Expected: PASS（2 个测试）

- [ ] **Step 5: 补齐所有穷尽 match（编译器驱动）**

Run: `cargo build --all-targets 2>&1 | grep -E "^error|--> " | head -40`

按报错逐点补齐。各点处理方式**必须**遵循下表（与 spec §3.3 一致）：

| 文件 | 位置 | 处理 |
|---|---|---|
| `compact.rs` | `estimate_block_tokens` | `ContentBlock::Thinking { text, .. } => estimate_text_tokens(text)` |
| `compact.rs` | `format_messages_for_summary` | `ContentBlock::Thinking { .. } => "[思考]".to_string()` |
| `agent.rs` | `estimate_prefill_tokens` | `ContentBlock::Thinking { text, .. } => total += estimate_tokens(text)` |
| `anthropic/types.rs` | `From<ContentBlock>` | 见 Task 4（本步先 `ContentBlock::Thinking { .. } => AnthropicContentBlock::Text { text: String::new() }` 占位会破坏保真，**改用在 Task 4 一次性实现**；本步暂时 `unimplemented!()` 会导致测试红——故本步先跳过该文件，Task 4 处理） |
| `openai/types.rs` | `extract_text` / `image_part` | 非 `Text`/`Image` 分支已用 `_ =>`，无需改；**确认** `extract_text` 是 `filter_map` 形式（只挑 `Text`），若是穷尽 match 则补 `Thinking => None` |
| `openai/responses/types.rs` | `user_item` 等 | 同上，确认为 `filter_map` 形式 |
| `translate.rs` | `render_content` | `ContentBlock::Thinking { .. } => rendered.push_str("[thinking]")` |
| `mcp/manager.rs` | `536` | MCP 无此概念，补 `_ => {}` 或显式忽略分支 |
| `view_image.rs` | `113` | 补 `_ => {}` |

**注意**：`compact.rs` 与 `agent.rs` 的 token 口径必须一致（都用 `estimate_text_tokens` /
`estimate_tokens` 的等价实现），否则状态栏估算漂移。

Run: `cargo build --all-targets 2>&1 | grep -cE "^error"`
Expected: `0`

- [ ] **Step 6: 跑核心 crate 测试确认无回归**

Run: `cargo test -p yi-agent-core && cargo test -p yi-agent-llm`
Expected: 全绿（含新增 2 个测试）

- [ ] **Step 7: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-core/src/message.rs crates/yi-agent-core/src/compact.rs \
        crates/yi-agent-core/src/agent.rs crates/yi-agent-app-server/src/translate.rs \
        crates/yi-agent-mcp/src/manager.rs crates/yi-agent-tools/src/fs/view_image.rs
git commit -m "feat(core): 新增 ContentBlock::Thinking 与 ThinkingToken"
```

---

## Task 2: `ProviderEvent::ThinkingBlock` 与 `accumulate_stream`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/provider.rs:104-136`（`ProviderEvent`）
- Modify: `yi-agent-rs/crates/yi-agent-core/src/provider.rs:234-302`（`accumulate_stream` match）
- Modify: `yi-agent-rs/crates/yi-agent-core/src/provider.rs`（测试）

**Interfaces:**
- Consumes: `ContentBlock::Thinking`、`ThinkingToken`（Task 1）
- Produces:
  - `ProviderEvent::ThinkingBlock { text: String, token: ThinkingToken }`
  - `accumulate_stream` 语义：收到该事件即 `content.push(ContentBlock::Thinking{..})`，并转发给 `on_event`

- [ ] **Step 1: 写失败测试（块进入 content 且被转发）**

在 `provider.rs` 的 `mod tests` 内追加：

```rust
#[tokio::test]
async fn thinking_block_enters_content_and_is_forwarded() {
    use crate::message::{ContentBlock, ThinkingToken};
    let stream = futures::stream::iter(vec![
        ProviderEvent::ThinkingBlock {
            text: "summary".into(),
            token: ThinkingToken::Signature("sig".into()),
        },
        ProviderEvent::TextDelta("answer".into()),
        ProviderEvent::Stop { reason: StopReason::EndTurn },
    ])
    .boxed();

    let mut forwarded = Vec::new();
    let (content, _end, _) =
        accumulate_stream(stream, |e| forwarded.push(e.clone()), None)
            .await
            .unwrap();

    assert_eq!(
        content,
        vec![
            ContentBlock::Thinking {
                text: "summary".into(),
                token: ThinkingToken::Signature("sig".into())
            },
            ContentBlock::Text("answer".into()),
        ],
        "the thinking block must be part of assistant content, in order"
    );
    assert!(
        forwarded.iter().any(|e| matches!(
            e,
            ProviderEvent::ThinkingBlock { text, .. } if text == "summary"
        )),
        "the thinking block must also be forwarded to on_event"
    );
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p yi-agent-core thinking_block_enters -- --nocapture`
Expected: 编译失败，报 `no variant named ThinkingBlock`

- [ ] **Step 3: 新增事件变体**

在 `provider.rs` 的 `ProviderEvent` 内、`ReasoningDelta` 之后追加：

```rust
    /// 一个完整的、携带凭证的思考块。块级（非流式）。
    ///
    /// 与 [`ProviderEvent::ReasoningDelta`]（展示通道）严格分离：本事件是**协议数据**，
    /// 用于构造 [`crate::message::ContentBlock::Thinking`] 并参与上下文回传。
    /// 两者互不替代——`display:"omitted"` 时只有本事件、没有 `ReasoningDelta`。
    ThinkingBlock {
        /// 明文摘要（可为空串）。
        text: String,
        /// 凭证形态。
        token: crate::message::ThinkingToken,
    },
```

- [ ] **Step 4: 在 `accumulate_stream` 中处理**

在 `provider.rs` 的 `accumulate_stream` match 内、`ReasoningDelta` 分支之后追加：

```rust
            ProviderEvent::ThinkingBlock { text, token } => {
                // 块级协议数据：原子地进入 assistant content，同时转发。
                // 与 ReasoningDelta 不同，本事件**参与**上下文回传。
                content.push(crate::message::ContentBlock::Thinking {
                    text: text.clone(),
                    token: token.clone(),
                });
                on_event(ProviderEvent::ThinkingBlock { text, token });
            }
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test -p yi-agent-core thinking_block -- --nocapture`
Expected: PASS

- [ ] **Step 6: 确认既有事件转发未受影响**

Run: `cargo test -p yi-agent-core provider_reasoning_and_notice_events_reach_the_agent_stream`
Expected: PASS。若失败，说明 `AgentEvent` 侧需要补分支（见 Task 10 的翻译层）——本任务**不**新增 AgentEvent，故该测试应仍绿。

- [ ] **Step 7: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-core/src/provider.rs
git commit -m "feat(core): 新增块级 ProviderEvent::ThinkingBlock 并接入 accumulate_stream"
```

---

## Task 3: 凭证辅助函数（剥离与保真）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-core/src/thinking.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/lib.rs`（模块声明与导出）

**Interfaces:**
- Consumes: `Message`、`ContentBlock`、`ThinkingToken`（Task 1）
- Produces:
  - `pub fn strip_thinking(messages: &[Message]) -> Vec<Message>` — 剥离所有 thinking 块（供关闭开关与压缩使用）
  - `pub fn strip_thinking_except_last_assistant(messages: &[Message]) -> Vec<Message>` — 保留最后一条 assistant 消息的 thinking（轮内保留）

**为什么单独一个模块**：剥离逻辑被 §4.1（开关）、§4.2（压缩）两处复用，集中一处避免漂移。

⚠️ **命名冲突警告**：`agent.rs:759` 已有一个**私有** `fn has_tool_use(message: &Message) -> bool`，
而 DeepSeek 的回传条件需要同一判据。**不要把 `has_tool_use` 加进本模块**——会导致
同一 crate 内两个同名函数（一个是 `agent.rs` 私有、一个是 `thinking` 模块 pub）。
正确做法：把 `agent.rs:759` 的私有函数**改为引用**本模块的实现，或在
`thinking.rs` 里叫别的名字（如 `message_has_tool_use`）。本计划采用**后者**，改动最小：
`thinking.rs` 导出 `pub fn message_has_tool_use(msg: &Message) -> bool`，
`agent.rs:759` 的私有函数保持不变（其内部可改为调用新函数以消除重复，可选）。

- [ ] **Step 1: 写失败测试**

创建 `thinking.rs`，先只写测试骨架说明预期：

```rust
//! 思考块的剥离与保真辅助。
//!
//! 集中一处：`preserve_thinking` 开关与 compaction 前缀兼容都用同一判据，
//! 避免各 provider 各自实现导致漂移。

use crate::message::{ContentBlock, Message, Role};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ThinkingToken;

    fn thinking(text: &str) -> ContentBlock {
        ContentBlock::Thinking {
            text: text.into(),
            token: ThinkingToken::Signature(format!("sig-{text}")),
        }
    }

    #[test]
    fn strip_thinking_removes_all_blocks_and_keeps_others() {
        let msgs = vec![
            Message::user("hi"),
            Message::assistant(vec![thinking("a"), ContentBlock::Text("t1".into())]),
            Message::assistant(vec![thinking("b")]),
        ];
        let out = strip_thinking(&msgs);
        assert_eq!(out[1].content.len(), 1, "thinking must be gone");
        assert!(matches!(out[1].content[0], ContentBlock::Text(_)));
        assert!(out[2].content.is_empty(), "block-less message stays, emptied");
    }

    #[test]
    fn strip_except_last_assistant_keeps_only_that_one() {
        let msgs = vec![
            Message::assistant(vec![thinking("old")]),
            Message::user("hi"),
            Message::assistant(vec![thinking("current"), ContentBlock::Text("t".into())]),
        ];
        let out = strip_thinking_except_last_assistant(&msgs);
        assert!(
            matches!(out[0].content[0], ContentBlock::Text(_)),
            "earlier thinking stripped"
        );
        assert!(
            matches!(out[2].content[0], ContentBlock::Thinking { .. }),
            "last assistant thinking preserved"
        );
    }

    #[test]
    fn message_has_tool_use_detects_tool_use_blocks() {
        let with = Message::assistant(vec![ContentBlock::ToolUse {
            id: "t1".into(),
            name: "read".into(),
            input: serde_json::json!({}),
        }]);
        let without = Message::assistant(vec![ContentBlock::Text("hi".into())]);
        assert!(message_has_tool_use(&with));
        assert!(!message_has_tool_use(&without));
    }
}
```

在 `lib.rs` 追加模块声明（放在既有模块声明旁）：

```rust
pub mod thinking;
```

并确保 `pub use thinking::{message_has_tool_use, strip_thinking, strip_thinking_except_last_assistant};`
或按该文件既有导出风格处理。

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p yi-agent-core strip_thinking -- --nocapture`
Expected: 编译失败，报 `cannot find function strip_thinking`

- [ ] **Step 3: 实现**

在 `thinking.rs` 顶部（`use` 之后、`mod tests` 之前）追加：

```rust
/// 剥离所有 thinking 块。
///
/// 用途：`preserve_thinking=false` 时清历史；compaction 时清被压缩区间。
/// 空壳消息**保留**（不删消息），避免破坏 user/assistant 交替与 tool_use 配对——
/// 那会让 provider 请求非法。
pub fn strip_thinking(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .map(|m| Message {
            role: m.role,
            content: m
                .content
                .iter()
                .filter(|b| !matches!(b, ContentBlock::Thinking { .. }))
                .cloned()
                .collect(),
        })
        .collect()
}

/// 剥离所有 thinking 块，但保留**最后一条 assistant 消息**中的。
///
/// 用途：轮内硬保证——当前轮工具循环的凭证必须回传，历史轮的可释放。
pub fn strip_thinking_except_last_assistant(messages: &[Message]) -> Vec<Message> {
    let last_assistant = messages
        .iter()
        .rposition(|m| m.role == Role::Assistant);

    messages
        .iter()
        .enumerate()
        .map(|(i, m)| {
            if Some(i) == last_assistant {
                m.clone()
            } else {
                Message {
                    role: m.role,
                    content: m
                        .content
                        .iter()
                        .filter(|b| !matches!(b, ContentBlock::Thinking { .. }))
                        .cloned()
                        .collect(),
                }
            }
        })
        .collect()
}

/// 该消息是否含 tool_use 块。DeepSeek 的 `reasoning_content` 仅在有工具调用时回传。
///
/// 命名带 `message_` 前缀以避免与 `agent.rs` 的私有 `has_tool_use` 撞名。
pub fn message_has_tool_use(msg: &Message) -> bool {
    msg.content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
}
```

**注意**：`Message` 的字段是 `role` / `content`（见 `message.rs:13-16`），构造时需两字段齐全。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p yi-agent-core strip_thinking -- --nocapture`
Expected: PASS（3 个测试）

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-core/src/thinking.rs crates/yi-agent-core/src/lib.rs
git commit -m "feat(core): 新增思考块剥离与保真辅助函数"
```

---

## Task 4: Anthropic 请求侧（thinking 字段 + 逐字回传 + max_tokens）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/types.rs:7-18`（`AnthropicRequest`）
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/types.rs:29-48`（`AnthropicContentBlock`）
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/types.rs:109-129`（`From<ContentBlock>`）
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/types.rs:145-153`（`From<GenParams>`）
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/types.rs:171-224`（`From<ProviderRequest>`）

**Interfaces:**
- Consumes: `ContentBlock::Thinking`、`ThinkingToken`（Task 1）
- Produces:
  - `AnthropicContentBlock::Thinking { thinking: String, signature: String }`
  - `AnthropicContentBlock::RedactedThinking { data: String }`
  - `AnthropicRequest.thinking: Option<ThinkingConfig>`、`.output_config: Option<OutputConfig>`
  - `AnthropicGenParams.max_tokens` 有默认值（常量 `DEFAULT_ANTHROPIC_MAX_TOKENS`）

- [ ] **Step 1: 写失败测试（凭证逐字 + 块顺序 + max_tokens + 不回传 Encrypted/None）**

在 `anthropic/types.rs` 的 `mod tests` 内追加：

```rust
#[test]
fn thinking_block_serializes_before_tool_use_with_verbatim_signature() {
    use yi_agent_core::{ContentBlock, ThinkingToken};
    let req = ProviderRequest {
        model: "claude-opus-5-5".into(),
        system: None,
        messages: vec![Message::assistant(vec![
            ContentBlock::Thinking {
                text: "".into(), // display:"omitted"
                token: ThinkingToken::Signature("sig-part-1.sig-part-2".into()),
            },
            ContentBlock::ToolUse {
                id: "toolu_1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "a.rs"}),
            },
        ])],
        tools: vec![],
        params: GenParams::default(),
    };
    let json = serde_json::to_value(AnthropicRequest::from(req)).unwrap();
    let blocks = json["messages"][0]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "thinking", "thinking must be first");
    assert_eq!(blocks[0]["signature"], "sig-part-1.sig-part-2", "verbatim");
    assert_eq!(blocks[0]["thinking"], "");
    assert_eq!(blocks[1]["type"], "tool_use", "tool_use must follow");
}

#[test]
fn redacted_thinking_serializes_as_its_own_type() {
    use yi_agent_core::{ContentBlock, ThinkingToken};
    let req = ProviderRequest {
        model: "claude-opus-5-5".into(),
        system: None,
        messages: vec![Message::assistant(vec![ContentBlock::Thinking {
            text: "".into(),
            token: ThinkingToken::Redacted("enc-blob".into()),
        }])],
        tools: vec![],
        params: GenParams::default(),
    };
    let json = serde_json::to_value(AnthropicRequest::from(req)).unwrap();
    let block = &json["messages"][0]["content"][0];
    assert_eq!(block["type"], "redacted_thinking");
    assert_eq!(block["data"], "enc-blob");
    assert!(block.get("signature").is_none());
}

#[test]
fn deepseek_and_responses_tokens_are_not_sent_to_anthropic() {
    use yi_agent_core::{ContentBlock, ThinkingToken};
    for token in [ThinkingToken::None, ThinkingToken::Encrypted("x".into())] {
        let req = ProviderRequest {
            model: "claude-opus-5-5".into(),
            system: None,
            messages: vec![Message::assistant(vec![
                ContentBlock::Thinking { text: "t".into(), token },
                ContentBlock::Text("visible".into()),
            ])],
            tools: vec![],
            params: GenParams::default(),
        };
        let json = serde_json::to_value(AnthropicRequest::from(req)).unwrap();
        let blocks = json["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1, "non-Anthropic token must be dropped");
        assert_eq!(blocks[0]["type"], "text");
    }
}

#[test]
fn thinking_and_output_config_are_emitted_when_effort_set() {
    use yi_agent_core::GenParams;
    let req = ProviderRequest {
        model: "claude-opus-5-5".into(),
        system: None,
        messages: vec![Message::user("hi")],
        tools: vec![],
        params: GenParams {
            reasoning_effort: Some("high".into()),
            ..Default::default()
        },
    };
    let json = serde_json::to_value(AnthropicRequest::from(req)).unwrap();
    assert_eq!(json["thinking"]["type"], "adaptive");
    assert_eq!(json["thinking"]["display"], "summarized");
    assert!(json["thinking"].get("budget_tokens").is_none(), "no manual budget");
    assert_eq!(json["output_config"]["effort"], "high");
}

#[test]
fn max_tokens_always_present_even_when_unset() {
    let req = ProviderRequest {
        model: "claude-opus-5-5".into(),
        system: None,
        messages: vec![Message::user("hi")],
        tools: vec![],
        params: GenParams::default(),
    };
    let json = serde_json::to_value(AnthropicRequest::from(req)).unwrap();
    assert!(
        json["max_tokens"].as_u64().is_some(),
        "max_tokens is required by the Anthropic API; must never be absent"
    );
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p yi-agent-llm thinking_block_serializes -- --nocapture`
Expected: 编译失败，报 `cannot find variant Thinking`（`AnthropicContentBlock` 侧）

- [ ] **Step 3: 实现请求字段与内容块**

在 `types.rs` 的 `AnthropicContentBlock` 内追加两个变体：

```rust
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
```

在 `AnthropicRequest` 内追加：

```rust
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
```

在同文件追加类型定义：

```rust
/// adaptive thinking 配置。新代模型拒绝 `budget_tokens`，故不带该字段。
#[derive(Serialize, Debug)]
pub struct ThinkingConfig {
    pub r#type: String,   // 恒为 "adaptive"
    pub display: String,  // 恒为 "summarized"
}

#[derive(Serialize, Debug)]
pub struct OutputConfig {
    pub effort: String,
}
```

在 `From<ContentBlock> for AnthropicContentBlock`（`types.rs:109`）内，`Image` 分支后追加：

```rust
            ContentBlock::Thinking { text, token } => match token {
                yi_agent_core::ThinkingToken::Signature(signature) => {
                    AnthropicContentBlock::Thinking { thinking: text, signature }
                }
                yi_agent_core::ThinkingToken::Redacted(data) => {
                    AnthropicContentBlock::RedactedThinking { data }
                }
                // 非 Anthropic 形态**不会**到达这里：`From<ProviderRequest>` 已先过滤。
                // 保守起见用 unreachable 而非编造凭证——静默造出非法 wire 形状更糟。
                yi_agent_core::ThinkingToken::Encrypted(_)
                | yi_agent_core::ThinkingToken::None => {
                    unreachable!("non-Anthropic thinking tokens are filtered before conversion")
                }
            },
```

**为什么过滤放在 `From<ProviderRequest>` 而不是这里**：`From<ContentBlock>` 无上下文，
无法决定「要不要整条消息丢弃」；过滤需要看整条消息。故该分支只作防御性断言，
真正的过滤在 Step 4。

- [ ] **Step 4: 实现请求构造（过滤 + 字段填充 + max_tokens 默认）**

在 `types.rs` 顶部追加常量：

```rust
/// Anthropic Messages API 的 `max_tokens` 是**必填**字段（缺失即 400）。
///
/// 取值需同时容纳 thinking token 与响应文本——thinking 计入 `max_tokens`。
/// 该值对当前新代模型（输出上限 ≥64k）留有充足余量。
pub const DEFAULT_ANTHROPIC_MAX_TOKENS: u32 = 32_000;
```

在 `From<yi_agent_core::GenParams> for AnthropicGenParams`（`types.rs:145`）内改为：

```rust
        Self {
            temperature: p.temperature,
            // 必填字段：未指定时用默认，绝不序列化缺席。
            max_tokens: Some(p.max_tokens.unwrap_or(DEFAULT_ANTHROPIC_MAX_TOKENS)),
            top_p: p.top_p,
            stop_sequences: p.stop_sequences,
        }
```

`AnthropicGenParams.max_tokens` 的 serde 属性**保持不变**（`skip_serializing_if`），
因为经此转换后它恒为 `Some`。

在 `From<ProviderRequest> for AnthropicRequest`（`types.rs:171`）内：

1. assistant 分支的内容转换改为先过滤：

```rust
                _ => {
                    let role = role_label(m.role).to_string();
                    let content: Vec<AnthropicContentBlock> = m
                        .content
                        .into_iter()
                        // Anthropic 只认自己的两种凭证形态；其他 provider 的
                        // 思考块在转换前丢弃，避免编造出非法 wire 形状。
                        .filter(|b| {
                            !matches!(
                                b,
                                ContentBlock::Thinking {
                                    token: yi_agent_core::ThinkingToken::Encrypted(_)
                                        | yi_agent_core::ThinkingToken::None,
                                    ..
                                }
                            )
                        })
                        .map(Into::into)
                        .collect();
                    // 全空的 assistant 消息（只有被过滤的块）不发送。
                    if !content.is_empty() {
                        messages.push(AnthropicMessage { role, content });
                    }
                }
```

2. 构造返回值时填充新字段：

```rust
        let thinking = Some(ThinkingConfig {
            r#type: "adaptive".into(),
            display: "summarized".into(),
        });
        let output_config = req
            .params
            .reasoning_effort
            .clone()
            .map(|effort| OutputConfig { effort });

        Self {
            model: req.model,
            messages,
            system,
            tools: req.tools.into_iter().map(Into::into).collect(),
            params: req.params.into(),
            thinking,
            output_config,
            stream: true,
        }
```

**注意**：`params: req.params.into()` 必须在 `req.params.reasoning_effort` 被 move 之前取值，或用 `clone()`。上面用了 `req.params.clone()` 语义——实现时若 `GenParams: Clone`（它 derive 了 `Clone`），直接用 `req.params.clone()` 再取 effort 最清晰。

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test -p yi-agent-llm --lib anthropic`
Expected: 全绿（含新增 5 个测试 + 既有测试）

- [ ] **Step 6: 跑全 crate 测试确认无回归**

Run: `cargo test -p yi-agent-llm`
Expected: 全绿。若有既有测试断言 `max_tokens` 缺席，需按新契约更新该断言（并确认这是预期变更）。

- [ ] **Step 7: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-llm/src/anthropic/types.rs
git commit -m "feat(anthropic): thinking 请求字段、凭证逐字回传与 max_tokens 默认"
```

---

## Task 5: Anthropic 流解析（分段 signature 拼接）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/stream.rs:100-107`（`AnthropicStream` 状态）
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/stream.rs:140-205`（`parse_frame`）
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/stream.rs`（测试）

**Interfaces:**
- Consumes: `ProviderEvent::ThinkingBlock`（Task 2）
- Produces: 流解析对 thinking 块的产出——`content_block_stop` 时 emit `ThinkingBlock{text, token}`

**本任务的最高风险点**：signature 通过 `signature_delta` 分段到达，少拼一段即回传失败。

- [ ] **Step 1: 写失败测试（分段 signature + redacted + 顺序）**

在 `anthropic/stream.rs` 的 `mod tests` 内追加：

```rust
#[tokio::test]
async fn thinking_block_concatenates_fragmented_signature() {
    let body = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n\
         event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"let me \"}}\n\n\
         event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"think\"}}\n\n\
         event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"AAA\"}}\n\n\
         event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"BBB\"}}\n\n\
         event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"CCC\"}}\n\n\
         event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
         event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n";
    let bytes = body.to_string().into_bytes();
    let events: Vec<ProviderEvent> = collect_events(vec![bytes.as_slice()])
        .await
        .into_iter()
        .filter_map(|r| r.ok())
        .collect();

    let block = events
        .iter()
        .find_map(|e| match e {
            ProviderEvent::ThinkingBlock { text, token } => Some((text.clone(), token.clone())),
            _ => None,
        })
        .expect("a ThinkingBlock must be emitted at content_block_stop");

    assert_eq!(block.0, "let me think", "明文按序拼接");
    assert_eq!(
        block.1,
        yi_agent_core::ThinkingToken::Signature("AAABBBCCC".into()),
        "分段 signature 必须完整拼接——少一段即回传失败"
    );

    // 展示通道照常收到增量。
    let deltas: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::ReasoningDelta { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, vec!["let me ".to_string(), "think".to_string()]);
}

#[tokio::test]
async fn redacted_thinking_is_emitted_as_its_own_token() {
    let body = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"enc-blob\"}}\n\n\
         event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
         event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n";
    let bytes = body.to_string().into_bytes();
    let events: Vec<ProviderEvent> = collect_events(vec![bytes.as_slice()])
        .await
        .into_iter()
        .filter_map(|r| r.ok())
        .collect();

    let found = events.iter().any(|e| matches!(
        e,
        ProviderEvent::ThinkingBlock { text, token }
            if text.is_empty()
                && *token == yi_agent_core::ThinkingToken::Redacted("enc-blob".into())
    ));
    assert!(found, "redacted_thinking must map to ThinkingToken::Redacted");
}

#[tokio::test]
async fn thinking_emitted_before_tool_use_in_stream_order() {
    let body = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n\
         event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"SIG\"}}\n\n\
         event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
         event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read\",\"input\":{}}}\n\n\
         event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n\
         event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n";
    let bytes = body.to_string().into_bytes();
    let events: Vec<ProviderEvent> = collect_events(vec![bytes.as_slice()])
        .await
        .into_iter()
        .filter_map(|r| r.ok())
        .collect();

    let thinking_at = events
        .iter()
        .position(|e| matches!(e, ProviderEvent::ThinkingBlock { .. }))
        .expect("thinking emitted");
    let tool_at = events
        .iter()
        .position(|e| matches!(e, ProviderEvent::ToolUseStart { .. }))
        .expect("tool use emitted");
    assert!(thinking_at < tool_at, "thinking must precede tool_use");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p yi-agent-llm --lib thinking_block_concatenates -- --nocapture`
Expected: FAIL，`a ThinkingBlock must be emitted`（当前 parser 不认 thinking）

- [ ] **Step 3: 实现状态与解析**

在 `AnthropicStream` 结构体内追加缓冲字段：

```rust
    /// 进行中的 thinking / redacted_thinking 块缓冲：index → 已累积内容。
    /// signature 分段到达，故必须缓冲到 content_block_stop 才能产出完整块。
    thinking_buffers: HashMap<usize, ThinkingBuffer>,
```

在同文件追加：

```rust
/// 进行中的 thinking 块累积状态。
#[derive(Default)]
struct ThinkingBuffer {
    /// 明文摘要（thinking_delta 累积）。
    text: String,
    /// 凭证载荷（signature_delta 累积，或 redacted 的一次性 data）。
    token: Option<yi_agent_core::ThinkingToken>,
}
```

在 `new()` 内初始化 `thinking_buffers: HashMap::new(),`。

在 `parse_frame` 的 `content_block_start` 分支内，`if block_type == "tool_use"` **之前**追加：

```rust
                if block_type == "thinking" {
                    self.thinking_buffers.insert(index, ThinkingBuffer::default());
                    return Ok(None);
                }
                if block_type == "redacted_thinking" {
                    let data = block
                        .get("data")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    self.thinking_buffers.insert(
                        index,
                        ThinkingBuffer {
                            text: String::new(),
                            token: Some(yi_agent_core::ThinkingToken::Redacted(data)),
                        },
                    );
                    return Ok(None);
                }
```

在 `content_block_delta` 的 `match delta_type` 内追加两个分支：

```rust
                    "thinking_delta" => {
                        let text = delta
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if let Some(buf) = self.thinking_buffers.get_mut(&index) {
                            buf.text.push_str(text);
                        }
                        if text.is_empty() {
                            return Ok(None);
                        }
                        Ok(Some(ProviderEvent::ReasoningDelta {
                            text: text.to_string(),
                            kind: ReasoningKind::Summary,
                        }))
                    }
                    "signature_delta" => {
                        let sig = delta
                            .get("signature")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if let Some(buf) = self.thinking_buffers.get_mut(&index) {
                            // 分段拼接：少一段即回传失败。
                            let buf_token = buf
                                .token
                                .get_or_insert_with(|| {
                                    yi_agent_core::ThinkingToken::Signature(String::new())
                                });
                            if let yi_agent_core::ThinkingToken::Signature(s) = buf_token {
                                s.push_str(sig);
                            }
                        }
                        Ok(None)
                    }
```

在 `content_block_stop` 分支内，`if let Some(id) = self.block_ids.remove(&index)`
**之前**追加：

```rust
                if let Some(buf) = self.thinking_buffers.remove(&index) {
                    let token = buf
                        .token
                        .unwrap_or_else(|| yi_agent_core::ThinkingToken::Signature(String::new()));
                    // 无凭证且无明文：空块无意义，不产出。
                    let meaningful = !buf.text.is_empty()
                        || !matches!(
                            token,
                            yi_agent_core::ThinkingToken::Signature(ref s) if s.is_empty()
                        );
                    if meaningful {
                        return Ok(Some(ProviderEvent::ThinkingBlock {
                            text: buf.text,
                            token,
                        }));
                    }
                    return Ok(None);
                }
```

在文件顶部的 `use` 中确认 `ReasoningKind` 已导入（`openai/responses/stream.rs:16` 是同样用法）：
`use yi_agent_core::{..., ReasoningKind, ...};`

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p yi-agent-llm --lib -- thinking_block_concatenates redacted_thinking_is_emitted thinking_emitted_before_tool_use`
Expected: PASS（3 个测试）

- [ ] **Step 5: 跑全 crate 测试确认无回归**

Run: `cargo test -p yi-agent-llm`
Expected: 全绿。既有 `parses_text_delta_sequence` 等测试的事件计数不应改变。

- [ ] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-llm/src/anthropic/stream.rs
git commit -m "feat(anthropic): 解析 thinking 块并完整拼接分段 signature"
```

---

## Task 6: DeepSeek chat 的 `reasoning_content`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/stream.rs:263-295`（delta 处理）
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/stream.rs`（测试）
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/types.rs:29-39`（`OpenaiMessage`）
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/types.rs`（`From<ProviderRequest>` assistant 分支）
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/types.rs`（测试）

**Interfaces:**
- Consumes: `ThinkingToken::None`（Task 1）、`ProviderEvent::ThinkingBlock`（Task 2）、`message_has_tool_use`（Task 3）
- Produces: `OpenaiMessage.reasoning_content: Option<String>`；流解析 emit `ThinkingBlock{text, token:None}`

**回传条件**：仅当 assistant 消息含 tool_calls 时才序列化 `reasoning_content`。

- [ ] **Step 1: 写失败测试**

在 `openai/stream.rs` 的 `mod tests` 内追加：

```rust
#[tokio::test]
async fn reasoning_content_is_parsed_as_thinking_block() {
    let body = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"step \"}}]}\n\n\
         data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"one\"}}]}\n\n\
         data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\n\
         data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
         data: [DONE]\n\n";
    let events: Vec<ProviderEvent> = collect_events(vec![bytes.as_slice()])
        .await
        .into_iter()
        .filter_map(|r| r.ok())
        .collect();

    let deltas: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::ReasoningDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, vec!["step ", "one"], "展示通道按增量产出");

    let block = events.iter().find_map(|e| match e {
        ProviderEvent::ThinkingBlock { text, token } => Some((text.clone(), token.clone())),
        _ => None,
    });
    let (text, token) = block.expect("stream end must emit one ThinkingBlock");
    assert_eq!(text, "step one", "整条消息的 reasoning_content 汇总为一个块");
    assert_eq!(token, yi_agent_core::ThinkingToken::None, "DeepSeek 无凭证");
}
```

**helper 说明**：该文件既有 `collect_events(chunks: Vec<&[u8]>)` helper（见
`openai/stream.rs:402`），直接复用它——测试体需自建 `bytes`：
`let bytes = body.to_string().into_bytes();` 再传 `vec![bytes.as_slice()]`。
**不要新造 helper。**

在 `openai/types.rs` 的 `mod tests` 内追加：

```rust
#[test]
fn reasoning_content_emitted_only_when_tool_calls_present() {
    use yi_agent_core::{ContentBlock, ThinkingToken};
    let base = |blocks: Vec<ContentBlock>| ProviderRequest {
        model: "deepseek-v4".into(),
        system: None,
        messages: vec![Message::assistant(blocks)],
        tools: vec![],
        params: GenParams::default(),
    };

    // 有 tool_calls：reasoning_content 必须回传。
    let with_tool = base(vec![
        ContentBlock::Thinking { text: "cot".into(), token: ThinkingToken::None },
        ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "read".into(),
            input: serde_json::json!({}),
        },
    ]);
    let json = serde_json::to_value(OpenaiRequest::from(with_tool)).unwrap();
    assert_eq!(json["messages"][0]["reasoning_content"], "cot");

    // 无 tool_calls：用后即弃，不得回传。
    let without_tool = base(vec![
        ContentBlock::Thinking { text: "cot".into(), token: ThinkingToken::None },
        ContentBlock::Text("answer".into()),
    ]);
    let json = serde_json::to_value(OpenaiRequest::from(without_tool)).unwrap();
    assert!(
        json["messages"][0].get("reasoning_content").is_none(),
        "无工具调用时 reasoning_content 不得回传"
    );
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p yi-agent-llm --lib reasoning_content -- --nocapture`
Expected: FAIL（既无解析，也无回传字段）

- [ ] **Step 3: 实现流解析**

在 `openai/stream.rs` 的 `/chat/completions` 解析器结构体内追加缓冲：

```rust
    /// 累积本条消息的 `reasoning_content`（DeepSeek 属整条消息，非块）。
    reasoning_buffer: String,
```

在 `new()` 内初始化 `reasoning_buffer: String::new(),`。

在 delta 处理（`if let Some(delta) = choice.get("delta")` 块内）、`content` 处理**之前**追加：

```rust
                        if let Some(reasoning) =
                            delta.get("reasoning_content").and_then(Value::as_str)
                        {
                            if !reasoning.is_empty() {
                                self.reasoning_buffer.push_str(reasoning);
                                events.push(ProviderEvent::ReasoningDelta {
                                    text: reasoning.to_string(),
                                    kind: ReasoningKind::Content,
                                });
                            }
                        }
```

在流结束处（`finish_reason` 已产出 Stop、或 `[DONE]` 处理处）追加**一次**产出：

```rust
        // 流结束：整条消息的 reasoning 汇总为一个块（DeepSeek 的粒度是消息级）。
        if !self.reasoning_buffer.is_empty() {
            events.push(ProviderEvent::ThinkingBlock {
                text: std::mem::take(&mut self.reasoning_buffer),
                token: yi_agent_core::ThinkingToken::None,
            });
        }
```

**实现提示**：必须找到该解析器"产出 Stop 后返回 events"的确切位置插入（既有代码在
`finish_reason` 命中时 push `Stop`）。**只产出一次**——用一个 `emitted_reasoning: bool`
守卫，避免多 chunk 重复产出。

- [ ] **Step 4: 实现回传字段**

在 `openai/types.rs` 的 `OpenaiMessage` 内追加：

```rust
    /// DeepSeek thinking mode 的思维链。**仅在有 tool_calls 时回传**——
    /// 无工具调用时官方要求用后即弃。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
```

在 `From<ProviderRequest> for OpenaiRequest` 的 assistant 分支内：
先算出该消息的 thinking 明文与是否含 tool_use，再据此填 `reasoning_content`。

```rust
            Role::Assistant => {
                let reasoning: String = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Thinking { text, token }
                            if matches!(token, yi_agent_core::ThinkingToken::None) =>
                        {
                            Some(text.as_str())
                        }
                        _ => None,
                    })
                    .collect();
                let has_tools = m
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolUse { .. }));
                let tool_calls = /* 既有 tool_calls 构造逻辑 */;
                messages.push(OpenaiMessage {
                    role: "assistant".to_string(),
                    name: None,
                    content: /* 既有 content 构造（thinking 块不进 content） */,
                    tool_calls,
                    tool_call_id: None,
                    // 仅在有工具调用时回传——无工具调用时官方要求用后即弃。
                    reasoning_content: (has_tools && !reasoning.is_empty()).then_some(reasoning),
                });
            }
```

**注意**：`OpenaiMessage` 的所有构造点都需补 `reasoning_content` 字段（编译器会指出）。
非 assistant 的消息一律填 `None`。`content` 构造不得包含 thinking 块（`extract_text`
只挑 `Text`，已天然满足——**确认**这一点）。

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test -p yi-agent-llm --lib reasoning_content`
Expected: PASS（2 个测试）

- [ ] **Step 6: 跑全 crate 测试确认无回归**

Run: `cargo test -p yi-agent-llm`
Expected: 全绿

- [ ] **Step 7: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-llm/src/openai/stream.rs crates/yi-agent-llm/src/openai/types.rs
git commit -m "feat(openai-chat): 解析并按规范回传 DeepSeek reasoning_content"
```

---

## Task 7: OpenAI Responses 的 reasoning item

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/responses/stream.rs:145-235`（item 处理）
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/responses/types.rs:34-90`（`responses_request_from`）
- Modify: 两文件的测试

**Interfaces:**
- Consumes: `ThinkingToken::Encrypted`（Task 1）、`ProviderEvent::ThinkingBlock`（Task 2）
- Produces: reasoning item 的解析与回传（`{type:"reasoning", encrypted_content}` 在 `function_call` 之前）

**诚实标注**：本通道官方措辞是 "highly recommend"，不回传**不报错**。这是效果优化，
**不是 bug 修复**。测试只锁定请求形状，不声称性能收益。

- [ ] **Step 1: 写失败测试**

在 `responses/stream.rs` 的 `mod tests` 内追加：

```rust
#[tokio::test]
async fn reasoning_item_done_emits_thinking_block_with_encrypted_content() {
    let body = "event: response.output_item.done\n\
         data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"enc-blob\",\"summary\":[]}}\n\n\
         event: response.completed\n\
         data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
    let bytes = body.to_string().into_bytes();
    let events: Vec<ProviderEvent> = collect_events(vec![bytes.as_slice()])
        .await
        .into_iter()
        .filter_map(|r| r.ok())
        .collect();

    let found = events.iter().any(|e| matches!(
        e,
        ProviderEvent::ThinkingBlock { token, .. }
            if *token == yi_agent_core::ThinkingToken::Encrypted("enc-blob".into())
    ));
    assert!(found, "reasoning item must surface its encrypted_content");
}
```

在 `responses/types.rs` 的 `mod tests` 内追加：

```rust
#[test]
fn reasoning_item_is_replayed_before_function_call() {
    use yi_agent_core::{ContentBlock, ThinkingToken};
    let req = ProviderRequest {
        model: "gpt-6-sol".into(),
        system: None,
        messages: vec![Message::assistant(vec![
            ContentBlock::Thinking {
                text: "".into(),
                token: ThinkingToken::Encrypted("enc-blob".into()),
            },
            ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "read".into(),
                input: serde_json::json!({}),
            },
        ])],
        tools: vec![],
        params: GenParams::default(),
    };
    let body = serde_json::to_value(responses_request_from(req)).unwrap();
    let input = body["input"].as_array().unwrap();
    let reasoning_at = input
        .iter()
        .position(|i| i["type"] == "reasoning")
        .expect("reasoning item must be present");
    let call_at = input
        .iter()
        .position(|i| i["type"] == "function_call")
        .expect("function_call must be present");
    assert!(reasoning_at < call_at, "reasoning must precede function_call");
    assert_eq!(input[reasoning_at]["encrypted_content"], "enc-blob");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p yi-agent-llm --lib reasoning_item -- --nocapture`
Expected: FAIL（无解析、无回传）

- [ ] **Step 3: 实现流解析**

在 `responses/stream.rs` 的 `"response.output_item.done"` 分支内，`function_call`
判断**之前**追加：

```rust
                    if item.get("type").and_then(Value::as_str) == Some("reasoning") {
                        if let Some(enc) = item
                            .get("encrypted_content")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                        {
                            events.push(ProviderEvent::ThinkingBlock {
                                text: String::new(),
                                token: yi_agent_core::ThinkingToken::Encrypted(enc.to_string()),
                            });
                        }
                    }
```

- [ ] **Step 4: 实现回传**

在 `responses/types.rs` 的 `responses_request_from` 的 `Role::Assistant` 分支内，
产出 assistant 文本**之前**追加 reasoning item：

```rust
            Role::Assistant => {
                // reasoning item 必须在 function_call 之前——与服务端返回顺序一致。
                for b in &m.content {
                    if let ContentBlock::Thinking {
                        token: yi_agent_core::ThinkingToken::Encrypted(enc),
                        ..
                    } = b
                    {
                        input.push(serde_json::json!({
                            "type": "reasoning",
                            "encrypted_content": enc
                        }));
                    }
                }
                // 既有：assistant 文本与 function_call 构造
                let text: String = /* 既有逻辑 */;
                // ...
            }
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test -p yi-agent-llm --lib reasoning_item`
Expected: PASS（2 个测试）

- [ ] **Step 6: 跑全 crate 测试确认无回归**

Run: `cargo test -p yi-agent-llm && cargo test -p yi-agent-llm --test openai_responses`
Expected: 全绿

- [ ] **Step 7: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-llm/src/openai/responses/
git commit -m "feat(responses): 解析并回传 reasoning item 的 encrypted_content"
```

---

## Task 8: 轮内回传的端到端验证

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs`（测试）
- Modify: `yi-agent-rs/crates/yi-agent-llm/tests/openai_integration.rs` 或新建测试

**Interfaces:**
- Consumes: Task 1-7 的全部产物
- Produces: 端到端证据——工具循环的第二轮请求含第一轮的思考块

**为什么单独一个任务**：前三层的单测各自绿，但「工具循环里凭证真的材回传了」只有集成
测试能证明。这是 spec §7.2 的落点。

- [ ] **Step 1: 写失败测试（core 层：块进入 session 并随下一轮请求发出）**

在 `agent.rs` 的 `mod tests` 内追加。该文件已有 `ScriptedProvider` 可复用（它记录收到的请求）：

```rust
#[tokio::test(flavor = "multi_thread")]
async fn tool_loop_round_two_carries_the_thinking_block() {
    use crate::message::{ContentBlock, ThinkingToken};

    // 第一轮：thinking + tool_use；第二轮：纯文本收尾。
    //
    // 注意：工具循环由「content 里是否有 ToolUse 块」决定（见 agent.rs:1192-1203），
    // **不是**靠 StopReason——`StopReason` 没有 ToolUse 变体。故脚本的第一轮
    // 必须真的产出 ToolUseStart/ToolUseEnd 以构造出 ToolUse 块。
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            ProviderEvent::ThinkingBlock {
                text: "".into(),
                token: ThinkingToken::Signature("SIG-1".into()),
            },
            ProviderEvent::ToolUseStart {
                id: "t1".into(),
                name: "echo".into(),
            },
            ProviderEvent::ToolUseDelta {
                id: "t1".into(),
                partial_json: "{}".into(),
            },
            ProviderEvent::ToolUseEnd { id: "t1".into() },
            ProviderEvent::Stop { reason: StopReason::EndTurn },
        ],
        vec![
            ProviderEvent::TextDelta("done".into()),
            ProviderEvent::Stop { reason: StopReason::EndTurn },
        ],
    ]));
    let tools = Arc::new(ToolRegistry::new()); // echo 未注册 → 得 tool-not-found 结果，循环仍会推进
    let mut agent = Agent::new(provider.clone(), tools, AgentConfig::default());

    let stream = agent.run("hi".into()).await.unwrap();
    let _ = collect_events(stream);

    // 第二轮请求必须带着第一轮的 thinking 块。
    let requests = provider.recorded_requests();
    assert!(requests.len() >= 2, "tool loop must issue a second request");
    let has_sig = requests[1].messages.iter().any(|m| {
        m.content.iter().any(|b| {
            matches!(
                b,
                ContentBlock::Thinking { token: ThinkingToken::Signature(s), .. } if s == "SIG-1"
            )
        })
    });
    assert!(has_sig, "the second request must carry the first round's signature");
}
```

**实现提示**：`ScriptedProvider`（`agent.rs:1760`）的 `call_stream` 当前用 `_req`
丢弃请求，需改为记录（见 Step 3）。`ScriptedProvider::new` 返回 `Self`（非 `Arc`），
故测试里显式 `Arc::new(...)`。

⚠️ **务必用 `arc.clone()` 或 `provider.clone()` 传入 Agent 以保留测试侧引用**
（`Agent::new` 接受 `Arc<dyn Provider>`）。

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p yi-agent-core tool_loop_round_two -- --nocapture`
Expected: FAIL（`recorded_requests` 不存在）或断言失败

- [ ] **Step 3: 补测试脚手架并实现断言通过**

给 `ScriptedProvider` 加请求记录：

```rust
    requests: std::sync::Mutex<Vec<crate::provider::ProviderRequest>>,
```

在 `call_stream` 开头记录：

```rust
        self.requests.lock().unwrap().push(req.clone());
```

并提供：

```rust
    fn recorded_requests(&self) -> Vec<crate::provider::ProviderRequest> {
        self.requests.lock().unwrap().clone()
    }
```

（`ProviderRequest` 需 `Clone`——已 derive。）

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p yi-agent-core tool_loop_round_two -- --nocapture`
Expected: PASS

- [ ] **Step 5: 加 provider 层的请求体断言（wiremock）**

在 `yi-agent-llm/tests/openai_responses.rs` 追加一个测试，验证**实际 HTTP 请求体**
含 reasoning item（而非只验构造函数）：

```rust
// 伪码骨架：复用该文件既有的 wiremock 模式。
// 1. 起 mock server，首个响应返回 reasoning item + function_call；
// 2. 让 provider 跑完一轮工具循环（或多轮 collect）；
// 3. 断言第二个收到的请求 body 里 input[] 含 {"type":"reasoning"}。
```

**实现提示**：**必须**复用该文件既有的 `MockServer` 装配与 `provider_with_style`
helper（见该文件顶部 `simple_request` / `provider_with_style`）。不要新造装配。

- [ ] **Step 6: 运行确认通过**

Run: `cargo test -p yi-agent-llm --test openai_responses`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-core/src/agent.rs crates/yi-agent-llm/tests/
git commit -m "test: 端到端锁定轮内思考块回传"
```

---

## Task 9: compaction 前缀兼容

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/compact.rs:279-303`（`build_compacted_messages`）
- Modify: `yi-agent-rs/crates/yi-agent-core/src/compact.rs:259-277`（`plan_compaction`）
- Modify: `yi-agent-rs/crates/yi-agent-core/src/compact.rs`（测试）

**Interfaces:**
- Consumes: `strip_thinking`（Task 3）
- Produces: `build_compacted_messages` 不再把摘要插入既有 assistant 消息内部；被压缩区间的 thinking 被剥离

**这是全计划风险最高的任务**：`build_compacted_messages` 被所有 provider 共用。

- [ ] **Step 1: 写失败测试**

在 `compact.rs` 的 `mod tests` 内追加：

```rust
#[test]
fn summary_is_not_inserted_into_an_assistant_message() {
    // 保留后缀以 assistant(tool_use) 开头时，摘要必须是独立的消息/块，
    // 不得插进该 assistant 消息的 content 内部——那会改写其块序列，
    // 破坏 Anthropic 的 thinking 前缀校验。
    let messages = vec![
        Message::user("old request"),
        Message::assistant(vec![ContentBlock::Text("old work".into())]),
        Message::tool_results(vec![ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: vec![ContentBlock::Text("old result".into())],
            is_error: false,
        }]),
        Message::user("current request"),
        Message::assistant(vec![ContentBlock::ToolUse {
            id: "t2".into(),
            name: "read".into(),
            input: serde_json::json!({}),
        }]),
    ];
    let plan = plan_compaction(&messages, 20_000, 12_000).expect("must plan");
    let built = build_compacted_messages(&plan, "summary text").unwrap();

    // 保留的 assistant（含 tool_use）消息内不得混入摘要文本。
    for m in &built {
        if m.content.iter().any(|b| matches!(b, ContentBlock::ToolUse { .. })) {
            let has_summary = m.content.iter().any(|b| match b {
                ContentBlock::Text(t) => t.contains("[对话摘要]"),
                _ => false,
            });
            assert!(
                !has_summary,
                "summary must not be injected into an assistant tool_use message"
            );
        }
    }
}

#[test]
fn compaction_strips_thinking_from_dropped_region() {
    use crate::message::ThinkingToken;
    let messages = vec![
        Message::user("old"),
        Message::assistant(vec![
            ContentBlock::Thinking { text: "old cot".into(), token: ThinkingToken::Signature("OLD".into()) },
            ContentBlock::Text("old work".into()),
        ]),
        Message::user("current"),
        Message::assistant(vec![ContentBlock::Text("recent".into())]),
    ];
    let plan = plan_compaction(&messages, 20_000, 12_000).expect("must plan");
    let built = build_compacted_messages(&plan, "s").unwrap();
    let any_thinking = built.iter().any(|m| {
        m.content.iter().any(|b| matches!(b, ContentBlock::Thinking { .. }))
    });
    assert!(
        !any_thinking,
        "被压缩区间的 thinking 必须剥离（凭证已失去对应上下文）"
    );
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p yi-agent-core summary_is_not_inserted -- --nocapture`
Expected: FAIL（现实现正是 `first.content.insert(0, summary)`）

- [ ] **Step 3: 实现改造**

把 `build_compacted_messages`（`compact.rs:279`）的后缀处理改为**摘要独立成块**，
不再往既有 assistant 消息内部插：

```rust
pub fn build_compacted_messages(
    plan: &CompactionPlan,
    summary: &str,
) -> Result<Vec<Message>, CompactError> {
    let summary_block = ContentBlock::Text(format!("[对话摘要]\n{summary}"));
    let mut messages = vec![plan.retained_user.clone()];

    if plan.retained_tool_suffix.is_empty() {
        messages.push(Message::assistant(vec![summary_block]));
    } else {
        let suffix = plan.retained_tool_suffix.clone();
        let Some(first) = suffix.first() else {
            return Err(CompactError::NoReduction);
        };
        if first.role != Role::Assistant || tool_use_ids(first).is_empty() {
            return Err(CompactError::InvalidHistory(
                "tool suffix must start with assistant tool use".into(),
            ));
        }
        // 摘要作为**独立的 assistant 消息**置于保留后缀之前。
        //
        // 不能像此前那样 `first.content.insert(0, summary)`：那会改写该
        // assistant 消息的块序列，而 Anthropic 的 thinking 前缀校验要求
        // 思考块之前的内容 append-only，块序列一变即凭证失效。
        messages.push(Message::assistant(vec![summary_block]));
        messages.extend(suffix);
    }
    validate_compacted_messages(&messages)?;
    Ok(messages)
}
```

**注意**：这会产生两条相邻的 assistant 消息，可能违反 `validate_compacted_messages`
的「adjacent assistant messages」检查。实现时需确认并在该验证器上放宽此特例
（或把摘要并入保留后缀的**第一个消息之前**作为独立的 `Role::User`？——**不要**，
那会破坏 role 交替）。**正确做法**：把摘要块作为保留后缀首消息的**独立**
assistant 消息时，验证器需允许「assistant(summary-only) → assistant(tool_use)」
这一对。实现时在 `validate_compacted_messages` 内加特例：前一条 assistant 仅含
`Text`（摘要）且后一条含 tool_use 时放行。

在 `plan_compaction` 内，对被保留的工具单元做剥离：

```rust
    // 被压缩区间的凭证已失去对应上下文，保留只会触发前缀失效——统一剥离。
    let retained_tool_suffix = crate::thinking::strip_thinking(&retained_tool_suffix_raw);
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p yi-agent-core compact -- --nocapture`
Expected: 新增 2 个测试 PASS，既有 compact 测试 PASS

- [ ] **Step 5: 跨 provider 回归（关键）**

Run: `cargo test -p yi-agent-core && cargo test -p yi-agent-app-server`
Expected: 全绿。**若 app-server 的 compaction 测试变红，说明改动影响了非 Anthropic
路径——必须修到全绿，不得放宽断言。**

- [ ] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-core/src/compact.rs
git commit -m "fix(compact): 摘要不再插入 assistant 消息内部,并剥离被压缩区间的思考块"
```

---

## Task 10: `max_tokens` 契约锁定与 catalog 可覆盖

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/models.rs`（条目级 max_tokens，若采用）
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs:295-315`
- Modify: 相关测试

**Interfaces:**
- Consumes: `DEFAULT_ANTHROPIC_MAX_TOKENS`（Task 4）
- Produces: 可配置的 `max_tokens`（可选）

**说明**：Task 4 已给出默认值。本任务确保它不被误改回 `None`，并保留按模型覆盖的能力。
若用户认为条目级覆盖属过度设计，**可跳过本任务**（默认值已满足正确性）。

- [ ] **Step 1: 写失败测试（默认值不缺席）**

在 `bootstrap.rs` 测试内追加：

```rust
#[test]
fn anthropic_gen_params_always_carry_max_tokens() {
    let params = yi_agent_core::GenParams::default();
    let anthropic: yi_agent_llm::anthropic::AnthropicGenParams = params.into();
    assert!(
        anthropic.max_tokens.is_some(),
        "max_tokens 是 Anthropic 必填字段，转换后绝不为 None"
    );
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p yi-agent-runtime anthropic_gen_params_always -- --nocapture`
Expected: FAIL（Task 4 若已实现则直接 PASS；若 PASS 则本步改为**回归锁定**，直接进 Step 4）

- [ ] **Step 3: 实现（如需要）**

确保 `AnthropicGenParams` 从 `GenParams` 转换时用 `DEFAULT_ANTHROPIC_MAX_TOKENS` 兜底
（Task 4 Step 4 已实现）。若需条目级覆盖，在 `models.rs` 的条目加
`max_tokens: Option<u32>` 并经 `resolve_effective` 传入 `GenParams`。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p yi-agent-runtime`
Expected: 全绿

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-runtime/
git commit -m "test(runtime): 锁定 Anthropic max_tokens 必填契约"
```

---

## Task 11: transcript 持久化（推翻「不进 transcript」）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/translate.rs:306-315`（`ReasoningDelta` 处理）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/translate.rs`（测试）

**Interfaces:**
- Consumes: `ContentBlock::Thinking`（Task 1）
- Produces: translate 层把 thinking 块并入当前 assistant 消息

**这是对既有有意设计的推翻**（spec §6.1），必须显式记录理由。

- [ ] **Step 1: 写失败测试**

在 `translate.rs` 的 `mod tests` 内追加：

```rust
#[test]
fn thinking_block_is_merged_into_the_open_agent_message() {
    // 既有 helper：`translator()`（见 translate.rs:505），已设好 thread/turn。
    let mut t = translator();
    // 先开一条 agent 消息（现有事件）。
    t.on_event(AgentEvent::AssistantText("answer".into()));
    // 再收到一个完整思考块。
    t.on_event(AgentEvent::ThinkingBlock {
        text: "cot summary".into(),
        token: ThinkingToken::Signature("SIG".into()),
    });
    // 断言：通知照发（展示不受影响），且 transcript 里的 assistant 消息含 thinking 块。
    let msg = t.take_completed_message(); // 若既有 API 名称不同，用该文件既有的取消息方式
    assert!(
        msg.content.iter().any(|b| matches!(
            b,
            ContentBlock::Thinking { token: ThinkingToken::Signature(s), .. } if s == "SIG"
        )),
        "凭证必须进入 transcript（推翻原『不进 transcript』设计）"
    );
}
```

**实现提示**：`translator()` helper 已存在（`translate.rs:505`）。取「当前 assistant
消息」的 API 需按该文件既有方式（`open_message` 字段或既有 getter）——**先读该文件
的 `Translator` 定义，复用既有访问方式，不要新造 API**。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p yi-agent-app-server thinking_block_is_merged -- --nocapture`
Expected: 编译失败（`AgentEvent::ThinkingBlock` 不存在）

- [ ] **Step 3: 在 core 层加 `AgentEvent::ThinkingBlock` 并转发**

在 `agent.rs` 的 `AgentEvent` 内追加变体，并在 `accumulate_provider_stream` 的
`ProviderEvent::ReasoningDelta` 分支旁加转发（`agent.rs:1620` 附近）：

```rust
            ProviderEvent::ThinkingBlock { text, token } => {
                let _ = tx.try_send(AgentEvent::ThinkingBlock { text, token });
            }
```

在 `translate.rs` 处理该事件：发通知（供实时展示）+ 并入当前 assistant 消息。

- [ ] **Step 4: 改写 `translate.rs` 的处理与注释**

把 `ReasoningDelta` 分支的注释改为反映新契约，并新增 `ThinkingBlock` 分支：

```rust
            AgentEvent::ReasoningDelta { text, kind } => {
                // 展示用增量：推给前端实时渲染。
                out.push(Notification::ReasoningDelta {
                    thread_id: self.thread_id.clone(),
                    text,
                    kind,
                });
            }
            AgentEvent::ThinkingBlock { text, token } => {
                // 协议数据：**进入 transcript**。
                //
                // 这推翻了此前的有意设计（原注释：reasoning never enters the
                // transcript）。理由：凭证必须跨进程、跨重启存活——否则重开
                // 会话时回传的是「块在但 signature 丢了」的残缺历史，反而触发
                // 400。持久化不是展示需求，是正确性需求。
                self.open_message.push_thinking(text, token);
            }
```

- [ ] **Step 5: 运行确认通过**

Run: `cargo test -p yi-agent-app-server`
Expected: 全绿

- [ ] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/translate.rs crates/yi-agent-core/src/agent.rs
git commit -m "feat(app-server): thinking 块进入 transcript 以保凭证跨重启存活"
```

---

## Task 12: `preserve_thinking` 开关

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs:40-58`（`ThreadMeta`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/session.rs:80-90`（`SessionCommand`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（命令处理）
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs`（请求前剥离）
- Modify: 各文件测试

**Interfaces:**
- Consumes: `strip_thinking_except_last_assistant`（Task 3）
- Produces: `ThreadMeta.preserve_thinking: bool`（默认 `true`）；`AgentConfig.preserve_thinking: bool`

- [ ] **Step 1: 写失败测试（默认 true + 旧 meta 零迁移）**

在 `thread_store.rs` 测试内追加：

```rust
#[test]
fn preserve_thinking_defaults_true_and_legacy_meta_migrates() {
    // 旧 meta 无该字段 → true（零迁移）。
    let legacy: ThreadMeta = serde_json::from_str(r#"{"id":"t"}"#).unwrap();
    assert!(legacy.preserve_thinking, "旧 meta 缺字段必须回 true");
}
```

**实现提示**：`bool` 的 `#[serde(default)]` 默认是 `false`。要默认 `true`，必须写
一个显式默认函数：

```rust
fn default_true() -> bool { true }

#[serde(default = "default_true")]
pub preserve_thinking: bool,
```

在 `agent.rs` 测试内追加：

```rust
#[tokio::test(flavor = "multi_thread")]
async fn preserve_thinking_false_strips_history_but_keeps_current_round() {
    // 见 Step 3 的实现：请求前剥离历史轮，保留最后一条 assistant。
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p yi-agent-app-server preserve_thinking_defaults && cargo test -p yi-agent-core preserve_thinking_false`
Expected: 编译失败（字段不存在）

- [ ] **Step 3: 实现**

`ThreadMeta` 加字段（用上面的 `default_true`）、`ThreadMeta::new()` 与各构造点补
`preserve_thinking: true`。

`AgentConfig` 加 `pub preserve_thinking: bool`（Default 里 `true`）。

在 `run_loop` 构造请求前（`agent.rs:928` 的 `ProviderRequest` 之前）插入：

```rust
        // 关闭跨轮保留时，剥离历史轮的思考块；保留最后一条 assistant 的
        // （轮内硬保证：当前工具循环的凭证必须回传）。
        let request_messages = if config.preserve_thinking {
            messages.clone()
        } else {
            crate::thinking::strip_thinking_except_last_assistant(&messages)
        };
```

并把 `messages: messages.clone()` 改为 `messages: request_messages`。

**注意**：`messages`（历史）本身**不**改剥离结果——剥离只作用于**请求副本**。
但按 spec §4.1 的裁定，关闭开关时**落盘的 transcript 也必须剥离**，否则重开会话后
被剥离的凭证会「复活」。落地方式：切换开关的命令处理里，对 session 的消息执行
`strip_thinking` 并把结果写回 session（与 meta 更新同一事务）。因此：

- **请求路径**：`preserve_thinking=false` → 每次请求前对副本执行
  `strip_thinking_except_last_assistant`（保留当前轮）。
- **落盘路径**：开关切到 `false` 时 → 对 session 执行 `strip_thinking`（全剥），
  写回并随下一次 `append_turn` 落盘。

**两条路径的粒度不同是有意的**：请求需要当前轮的凭证（轮内硬保证），而落盘是
"该会话已放弃跨轮保留"的持久状态，不留历史凭证。

`SessionCommand` 加变体（与 `SetReasoningEffort` 同构）：

```rust
    /// 切换该会话的跨轮 CoT 保留。`true` = 保留（默认）。
    SetPreserveThinking {
        preserve: bool,
        reply: oneshot::Sender<Result<(), String>>,
    },
```

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p yi-agent-app-server && cargo test -p yi-agent-core`
Expected: 全绿

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/ crates/yi-agent-core/src/agent.rs
git commit -m "feat: 新增跨轮 CoT 保留开关(默认开启)"
```

---

## Task 13: protocol `Item::Thinking` 与历史回放

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs:536-570`（`Item`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/translate.rs`（产出 item）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`（落盘/回放）
- Modify: 桌面端（`desktop/`）与 iOS 的渲染

**Interfaces:**
- Consumes: Task 11 的 translate 改动
- Produces: `Item::Thinking { id, text, redacted }`（**不**携带凭证本体）

- [ ] **Step 1: 写失败测试**

在 `protocol.rs` 测试内追加：

```rust
#[test]
fn thinking_item_serializes_without_credentials() {
    let item = Item::Thinking {
        id: "th_1".into(),
        text: "summary".into(),
        redacted: false,
    };
    let json = serde_json::to_value(&item).unwrap();
    assert_eq!(json["type"], "thinking");
    assert_eq!(json["text"], "summary");
    assert!(json.get("signature").is_none(), "凭证不得下发前端");
    assert!(json.get("encrypted_content").is_none());
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p yi-agent-app-server thinking_item_serializes -- --nocapture`
Expected: 编译失败

- [ ] **Step 3: 实现 Item 与产出/回放**

在 `Item` 内追加：

```rust
    /// 一次推理的展示条目。**不携带凭证**——signature / encrypted_content 是
    /// 服务端回传 API 用的协议数据，前端渲染不需要，也不应持有（避免经客户端
    /// 往返被意外改写）。
    Thinking {
        id: String,
        /// 展示用明文（`display:"omitted"` 时为空串）。
        text: String,
        /// redacted 占位（前端渲染"已加密"）。
        redacted: bool,
    },
```

在 `translate.rs` 的 `ThinkingBlock` 处理中额外产出该 item；在 `thread_store` 按既有
item 机制落盘与回放。桌面端与 iOS 用既有的折叠卡片组件渲染该 item（**实现时先找
桌面端既有的 reasoning 卡片组件并复用，不要新建组件**）。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p yi-agent-app-server`
Expected: 全绿

- [ ] **Step 5: 前端验证**

Run: `cd desktop && npx vitest run && npx tsc --noEmit`
Expected: 全绿

- [ ] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/
git commit -m "feat(app-server): 新增 thinking Item 以支持历史回放"
cd .. && git add desktop && git commit -m "feat(desktop): 渲染历史 thinking 条目"
```

---

## Task 14: 压缩可见性 Notice

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs`（`maybe_auto_compact` 附近）
- Modify: 测试

**Interfaces:**
- Consumes: `AgentEvent::Notice`（既有）、`preserve_thinking`（Task 12）
- Produces: 压缩发生时发 Notice（仅当 `preserve_thinking=true`）

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test(flavor = "multi_thread")]
async fn compaction_emits_notice_about_released_credentials() {
    // preserve_thinking = true 时压缩 → 事件流含 Notice，文案说明凭证已释放。
    // preserve_thinking = false 时 → 不发该 Notice。
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p yi-agent-core compaction_emits_notice -- --nocapture`
Expected: FAIL

- [ ] **Step 3: 实现**

在压缩成功、session 被替换的位置（`agent.rs` 的 `maybe_auto_compact` 返回
`Some(new_messages)` 的分支，`run_loop` 内 `agent.rs:895` 附近）追加：

```rust
        if let Some(new_messages) = maybe_auto_compact(&tx, &provider, &config, &session).await {
            messages = new_messages;
            last_logged = 0;
            // 压缩常态触发时会释放历史推理凭证——不得静默。
            if config.preserve_thinking {
                let _ = tx
                    .send(AgentEvent::Notice {
                        level: NoticeLevel::Info,
                        text: "已压缩历史，推理凭证已释放".into(),
                    })
                    .await;
            }
        }
```

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p yi-agent-core`
Expected: 全绿

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-core/src/agent.rs
git commit -m "feat: 压缩时告知推理凭证已释放"
```

---

## Task 15: 端到端手工验证与文档

**Files:**
- Modify: `docs/bug-list.md` 或新建验证记录
- Modify: spec 的「未决问题」（把实测结论回填）

**说明**：这是唯一**不能**用单测证明的部分——需要真实端点。

- [ ] **Step 1: 实测 DeepSeek 张力点（spec §8 未决 4）**

用真实端点验证：DeepSeek 在 thinking mode + tools 下是否**真的**要求回传
`reasoning_content`（即是否真会 400）。

- 若会 400 → Task 6 是**修 bug**，在文档中如实记录。
- 若不会 → Task 6 是**防御性实现**，如实记录，不得声称修了 bug。

- [ ] **Step 2: 实测 Responses 回传收益（spec §5.3）**

对比「回传 reasoning item」与「不回传」的 `reasoning_tokens` 消耗。

- **只有拿到数据才能声称收益**；拿不到就如实写"未观察到显著差异"。

- [ ] **Step 3: 实测 `prefix_mismatch_behavior` 透传（spec §4.3）**

确认你们的 Anthropic 网关是否透传该字段，决定后续是否启用。

- [ ] **Step 4: 回填 spec 未决问题**

把 Step 1-3 的结论写进 spec §8，并将相应条目从"未决"改为"已实测"。

- [ ] **Step 5: 端到端人工走查**

跑一次真实的工具调用循环（Anthropic 通道），确认：
- 请求体含 `thinking` 与 `output_config`
- 第二轮请求含第一轮的思考块与 signature
- 流式过程中 UI 显示思考内容
- 压缩后无非法请求

- [ ] **Step 6: Commit**

```bash
git add docs/
git commit -m "docs: 回填 CoT 回传的实测结论与未决项"
```

---

## 附录：任务依赖与提交顺序

```
Task 1 (数据模型) ──┬─→ Task 2 (事件通道) ──┬─→ Task 5 (Anthropic 流) ──┐
                    │                       ├─→ Task 6 (DeepSeek)     ├─→ Task 8 (端到端)
                    │                       └─→ Task 7 (Responses)    ┘
                    ├─→ Task 3 (辅助函数) ──┬─→ Task 9 (compaction)
                    │                       └─→ Task 12 (开关)
                    └─→ Task 4 (Anthropic 请求) ──→ Task 10 (max_tokens 契约)
Task 11 (持久化) ← Task 1
Task 13 (Item) ← Task 11
Task 14 (Notice) ← Task 12
Task 15 (实测) ← 全部
```

**建议的提交粒度即任务粒度**：每个任务一个 commit，使二分定位容易。Task 9 风险最高，
建议单独提交并在其前后各跑一次受影响 crate 的全量测试。