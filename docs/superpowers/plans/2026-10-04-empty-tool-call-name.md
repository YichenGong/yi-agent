# 空工具调用（空白工具框 / `tool not found:`）修复 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 消除流式响应里「没有名字/没有 id」的工具调用，使空名工具永远不会被解析成工具调用、不会进入会话、不会在 UI 上呈现为空白框。

**Architecture:** 三层纵深防御。主修复在 OpenAI 流解析器（`yi-agent-llm`）——按 index 累积、id+name 均非空才发 `ToolUseStart`；第二层在核心累积器（`yi-agent-core`）——丢弃空 id/name 的 `ToolUseStart`（协议无关兜底）；第三层在 Desktop 卡片（`desktop`）——对空 `name` 显示占位，让已有历史脏数据也可读。

**Tech Stack:** Rust（`yi-agent-llm` / `yi-agent-core`，tokio + futures + serde_json + tracing）、TypeScript/React（`desktop`，vitest + @testing-library/react）。

## Global Constraints

- 不修改线协议：`Item` 结构、`.jsonl` / `.meta.json` 格式、通知方法名均不变。
- 正常工具调用（id 与 name 均非空）的事件序列与字节级行为不变。
- 不迁移、不清理历史脏数据（旧条目仍在磁盘上）。
- 只判「非空」：不做通用工具名白名单/正则校验。
- 所有 Rust 命令在 `yi-agent-rs/` 下运行；所有 desktop 命令在 `desktop/` 下运行。
- 一次只改本任务列出的文件。

## File Structure

- `yi-agent-rs/crates/yi-agent-llm/src/openai/stream.rs` — 新增 `PendingToolCall` 累积结构；`OpenaiStream.tool_calls` 字段替换为 `pending_tool_calls`；新增 `absorb_tool_call_delta` / `finish_tool_calls`；改写 `parse_frame` 的 tool_calls 与 finish_reason 分支。单测就近在文件底部 `mod tests`。
- `yi-agent-rs/crates/yi-agent-core/src/provider.rs` — `accumulate_stream` 的 `ToolUseStart` 分支增加空 id/name 丢弃。单测就近在 `mod tests`。
- `desktop/src/components/ToolCallCard.tsx` — 头部空 `name` 占位。单测在 `desktop/src/components/ToolCallCard.test.tsx`。

---

### Task 1: OpenAI 流解析器不再产生空名工具调用

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/stream.rs`（结构体字段 `tool_calls` ~80；`new()` ~95；`parse_frame` 的 tool_calls 分支 ~180-216 与 finish_reason 的 `"tool_calls"` 分支 ~237-249；新增结构体与方法）
- Test: `yi-agent-rs/crates/yi-agent-llm/src/openai/stream.rs` 的 `mod tests`

**Interfaces:**
- Consumes: 既有 `ProviderEvent::{ToolUseStart, ToolUseDelta, ToolUseEnd, TextDelta, Stop}`（`yi_agent_core`）、`serde_json::Value`、`tracing`。
- Produces:
  - `struct PendingToolCall { id: String, name: String, args: String, started: bool }`（`Default`）
  - `fn absorb_tool_call_delta(&mut self, index: usize, tc: &Value, events: &mut Vec<ProviderEvent>)`
  - `fn finish_tool_calls(&mut self, events: &mut Vec<ProviderEvent>)`

- [ ] **Step 1: 写失败测试**

在 `yi-agent-rs/crates/yi-agent-llm/src/openai/stream.rs` 的 `mod tests` 内（`surfaces_invalid_json_as_error` 之后）追加：

```rust
    #[tokio::test]
    async fn defers_tool_use_start_until_name_present() {
        // id arrives before name: no ToolUseStart until the name shows up.
        let body = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_x\"}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"read\",\"arguments\":\"\"}}]}}]}\n\n\
             data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\n\
             data: [DONE]\n\n";
        let bytes = body.to_string().into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;
        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        assert_eq!(events.len(), 3, "events: {:?}", events);
        assert!(
            matches!(&events[0], ProviderEvent::ToolUseStart { id, name } if id == "call_x" && name == "read")
        );
        assert!(matches!(&events[1], ProviderEvent::ToolUseEnd { id } if id == "call_x"));
    }

    #[tokio::test]
    async fn buffers_arguments_arriving_before_name() {
        // Arguments that arrive before the name must not be lost; they are
        // re-emitted as a ToolUseDelta right after the deferred ToolUseStart.
        let body = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_x\",\"function\":{\"arguments\":\"{\\\"p\\\":\"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"read\"}}]}}]}\n\n\
             data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\n\
             data: [DONE]\n\n";
        let bytes = body.to_string().into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;
        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        assert_eq!(events.len(), 4, "events: {:?}", events);
        assert!(
            matches!(&events[0], ProviderEvent::ToolUseStart { id, name } if id == "call_x" && name == "read")
        );
        assert!(
            matches!(&events[1], ProviderEvent::ToolUseDelta { id, partial_json } if id == "call_x" && partial_json == "{\"p\":")
        );
        assert!(matches!(&events[2], ProviderEvent::ToolUseEnd { id } if id == "call_x"));
    }

    #[tokio::test]
    async fn discards_tool_call_that_never_gets_a_name() {
        // An id with no name must never become a tool call. Its arguments are
        // downgraded to text so the model's intent is not silently lost.
        let body = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_y\",\"function\":{\"arguments\":\"{}\"}}]}}]}\n\n\
             data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\n\
             data: [DONE]\n\n";
        let bytes = body.to_string().into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;
        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, ProviderEvent::ToolUseStart { .. })),
            "no ToolUseStart may be emitted: {:?}",
            events
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ProviderEvent::TextDelta(t) if t == "{}")),
            "arguments must be downgraded to text: {:?}",
            events
        );
        assert!(matches!(events.last(), Some(ProviderEvent::Stop { .. })));
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm --lib openai::stream::tests`
Expected: 三个新用例 FAIL（例如 `defers_tool_use_start_until_name_present` 期望 3 条事件、实际收到 4 条：当前实现会在第一块就发 `ToolUseStart`）。

- [ ] **Step 3: 新增 `PendingToolCall` 与累积方法**

在 `stream.rs` 中 `pub struct OpenaiStream<S>` 定义之前插入：

```rust
/// A tool call being assembled from streamed deltas.
///
/// The provider may split a single tool call across chunks and may send its
/// `id`/`name` in a later chunk than its `arguments` (or never send them at
/// all). We buffer everything by `index` and only emit `ToolUseStart` once both
/// a non-empty `id` and a non-empty `name` are known, so an unnamed/empty-id
/// fragment can never become a tool call.
#[derive(Default)]
struct PendingToolCall {
    id: String,
    name: String,
    args: String,
    started: bool,
}
```

把字段 `tool_calls: HashMap<usize, (String, String)>` 改为：

```rust
    pending_tool_calls: HashMap<usize, PendingToolCall>,
```

并把 `new()` 里的 `tool_calls: HashMap::new(),` 改为 `pending_tool_calls: HashMap::new(),`。

在 `impl<S> OpenaiStream<S> where S: Stream<...> + Unpin` 内、`fn parse_frame` 之前新增：

```rust
    /// Absorb one streamed `tool_calls` delta for `index`.
    ///
    /// Emits `ToolUseStart` only once the call has both a non-empty id and a
    /// non-empty name. Arguments seen before the start are buffered and flushed
    /// (as a `ToolUseDelta`) immediately after the start; arguments seen after
    /// the start are forwarded straight through.
    fn absorb_tool_call_delta(
        &mut self,
        index: usize,
        tc: &Value,
        events: &mut Vec<ProviderEvent>,
    ) {
        let pending = self.pending_tool_calls.entry(index).or_default();

        if let Some(id) = tc.get("id").and_then(Value::as_str) {
            if !id.is_empty() {
                pending.id = id.to_string();
            }
        }
        if let Some(name) = tc
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
        {
            if !name.is_empty() {
                pending.name = name.to_string();
            }
        }
        if let Some(args) = tc
            .get("function")
            .and_then(|f| f.get("arguments"))
            .and_then(Value::as_str)
        {
            if !args.is_empty() {
                if pending.started {
                    events.push(ProviderEvent::ToolUseDelta {
                        id: pending.id.clone(),
                        partial_json: args.to_string(),
                    });
                } else {
                    pending.args.push_str(args);
                }
            }
        }

        if !pending.started && !pending.id.is_empty() && !pending.name.is_empty() {
            pending.started = true;
            events.push(ProviderEvent::ToolUseStart {
                id: pending.id.clone(),
                name: pending.name.clone(),
            });
            if !pending.args.is_empty() {
                let args = std::mem::take(&mut pending.args);
                events.push(ProviderEvent::ToolUseDelta {
                    id: pending.id.clone(),
                    partial_json: args,
                });
            }
        }
    }

    /// Close out every pending tool call at `finish_reason == "tool_calls"`.
    ///
    /// Started calls emit `ToolUseEnd`. Fragments that never received both an id
    /// and a name are dropped as tool calls; any arguments they carried are
    /// downgraded to text so nothing is silently lost.
    fn finish_tool_calls(&mut self, events: &mut Vec<ProviderEvent>) {
        let mut indices: Vec<usize> = self.pending_tool_calls.keys().copied().collect();
        indices.sort();
        for idx in indices {
            let Some(pending) = self.pending_tool_calls.remove(&idx) else {
                continue;
            };
            if pending.started {
                events.push(ProviderEvent::ToolUseEnd { id: pending.id });
            } else {
                tracing::warn!(
                    provider = "openai",
                    index = idx,
                    "discarding tool call with empty id/name"
                );
                if !pending.args.is_empty() {
                    events.push(ProviderEvent::TextDelta(pending.args));
                }
            }
        }
    }
```

- [ ] **Step 4: 改写 `parse_frame` 的 tool_calls 分支**

把 `if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) { ... }` 整块替换为：

```rust
                        if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array())
                        {
                            for tc in tool_calls {
                                let index =
                                    tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                                self.absorb_tool_call_delta(index, tc, &mut events);
                            }
                        }
```

- [ ] **Step 5: 改写 finish_reason 的 `"tool_calls"` 分支**

把：

```rust
                        "tool_calls" => {
                            self.stopped = true;
                            let mut indices: Vec<usize> = self.tool_calls.keys().copied().collect();
                            indices.sort();
                            for idx in indices {
                                if let Some((id, _)) = self.tool_calls.remove(&idx) {
                                    events.push(ProviderEvent::ToolUseEnd { id });
                                }
                            }
                            events.push(ProviderEvent::Stop {
                                reason: StopReason::EndTurn,
                            });
                        }
```

替换为：

```rust
                        "tool_calls" => {
                            self.stopped = true;
                            self.finish_tool_calls(&mut events);
                            events.push(ProviderEvent::Stop {
                                reason: StopReason::EndTurn,
                            });
                        }
```

- [ ] **Step 6: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm --lib openai::stream::tests`
Expected: 全部 PASS，含新增三个用例与既有 `parses_tool_call_with_incremental_arguments`、`multiple_tool_calls_in_one_response`、`maps_finish_reasons_correctly`、`parses_done_marker_without_finish_reason`。

- [ ] **Step 7: 提交**

```bash
cd yi-agent-rs
git add crates/yi-agent-llm/src/openai/stream.rs
git commit -m "fix(llm/openai): never emit a tool call with an empty id or name"
```

---

### Task 2: Core 累积器丢弃空名工具调用（协议无关兜底）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/provider.rs:169-174`（`accumulate_stream` 的 `ToolUseStart` 分支）
- Test: `yi-agent-rs/crates/yi-agent-core/src/provider.rs` 的 `mod tests`

**Interfaces:**
- Consumes: 既有 `ProviderEvent::ToolUseStart { id, name }`、`ContentBlock`。
- Produces: 无新公开 API；仅行为收紧（空 id/name 的 `ToolUseStart` 被丢弃，不进入 `content`、不转发 `on_event`）。

- [ ] **Step 1: 写失败测试**

在 `yi-agent-rs/crates/yi-agent-core/src/provider.rs` 的 `mod tests` 内追加：

```rust
    #[tokio::test]
    async fn ignores_tool_use_with_empty_name() {
        let events = vec![
            ProviderEvent::ToolUseStart {
                id: String::new(),
                name: String::new(),
            },
            ProviderEvent::ToolUseDelta {
                id: String::new(),
                partial_json: "{}".into(),
            },
            ProviderEvent::ToolUseEnd { id: String::new() },
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ];
        let stream = futures::stream::iter(events).boxed();
        let mut seen: Vec<ProviderEvent> = Vec::new();
        let (content, _end, _usage) =
            accumulate_stream(stream, |ev| seen.push(ev), None).await.unwrap();
        assert!(
            content.iter().all(|b| !matches!(b, ContentBlock::ToolUse { .. })),
            "empty-name tool use must not enter content: {content:?}"
        );
        assert!(
            !seen
                .iter()
                .any(|e| matches!(e, ProviderEvent::ToolUseStart { .. })),
            "empty-name tool use must not be forwarded: {seen:?}"
        );
    }

    #[tokio::test]
    async fn ignores_tool_use_with_empty_id() {
        let events = vec![
            ProviderEvent::ToolUseStart {
                id: String::new(),
                name: "read".into(),
            },
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ];
        let stream = futures::stream::iter(events).boxed();
        let mut seen: Vec<ProviderEvent> = Vec::new();
        let (content, _end, _usage) =
            accumulate_stream(stream, |ev| seen.push(ev), None).await.unwrap();
        assert!(
            content.iter().all(|b| !matches!(b, ContentBlock::ToolUse { .. })),
            "empty-id tool use must not enter content: {content:?}"
        );
        assert!(
            !seen
                .iter()
                .any(|e| matches!(e, ProviderEvent::ToolUseStart { .. })),
            "empty-id tool use must not be forwarded: {seen:?}"
        );
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib provider::tests::ignores_tool_use`
Expected: 两个用例 FAIL（当前实现会把空名工具收进 `content`）。

- [ ] **Step 3: 实现丢弃逻辑**

把 `accumulate_stream` 内：

```rust
            ProviderEvent::ToolUseStart { id, name } => {
                if !current_text.is_empty() {
                    content.push(ContentBlock::Text(std::mem::take(&mut current_text)));
                }
                tool_uses.insert(id, (name, String::new()));
            }
```

替换为：

```rust
            ProviderEvent::ToolUseStart { id, name } => {
                // Flush any buffered text either way, preserving prior behavior.
                if !current_text.is_empty() {
                    content.push(ContentBlock::Text(std::mem::take(&mut current_text)));
                }
                // A provider may stream an id-less / name-less tool-call
                // fragment (and never complete it). Such a fragment must never
                // become a tool call: an empty name resolves to no tool and
                // would surface as `tool not found: ` with a blank card.
                if id.is_empty() || name.is_empty() {
                    tracing::warn!(
                        ?id,
                        ?name,
                        "accumulate_stream: ignoring tool use with empty id or name"
                    );
                    continue;
                }
                tool_uses.insert(id, (name, String::new()));
            }
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib provider::tests`
Expected: 全部 PASS，含新增两用例与既有 `call_accumulates_text_and_tool_use`、`accumulate_stream_forwards_tool_use_delta_via_callback`、`accumulate_stream_multiple_tool_uses_in_order`。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs
git add crates/yi-agent-core/src/provider.rs
git commit -m "fix(core): drop tool use fragments with an empty id or name"
```

---

### Task 3: Desktop 卡片对空工具名显示占位

**Files:**
- Modify: `desktop/src/components/ToolCallCard.tsx:28`
- Test: `desktop/src/components/ToolCallCard.test.tsx`

**Interfaces:**
- Consumes: 既有 `ToolCallItem`（`{ name: string; ... }`）。
- Produces: 无新 API；`name` 为空时头部渲染 `(unknown tool)`。

- [ ] **Step 1: 写失败测试**

在 `desktop/src/components/ToolCallCard.test.tsx` 的 `describe` 内追加：

```tsx
  it("shows a placeholder when the tool name is empty", () => {
    render(<ToolCallCard item={bash({ command: "ls -la" }, { name: "" })} />);
    expect(screen.getByText("(unknown tool)")).toBeTruthy();
  });
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd desktop && npx vitest run src/components/ToolCallCard.test.tsx`
Expected: 新用例 FAIL（`getByText("(unknown tool)")` 抛 "Unable to find an element"）。

- [ ] **Step 3: 实现占位**

把 `desktop/src/components/ToolCallCard.tsx:28` 的：

```tsx
        <span className="font-mono font-medium text-fg">{item.name}</span>
```

替换为：

```tsx
        <span className="font-mono font-medium text-fg">
          {item.name || "(unknown tool)"}
        </span>
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd desktop && npx vitest run src/components/ToolCallCard.test.tsx`
Expected: 6 个用例全部 PASS。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/ToolCallCard.tsx desktop/src/components/ToolCallCard.test.tsx
git commit -m "fix(desktop): show a placeholder for a tool call with an empty name"
```

---

## Self-Review

**1. Spec coverage:**

- 第 1 层（LLM 解析层）→ Task 1（含 id/name 延迟、arguments 缓冲、无 name 丢弃并降级为文本）。
- 第 2 层（Core 兜底）→ Task 2。
- 第 3 层（UI 兜底）→ Task 3。
- 不变量/不做（线协议不变、不清理历史、只判非空、不动 anthropic）→ 无任务，正是「不做」符合预期。

**2. Placeholder scan:** 无 TBD/TODO；每个代码步骤均给出可直接粘贴的完整代码与命令。

**3. Type consistency:** `PendingToolCall{id,name,args,started}`、`pending_tool_calls`、`absorb_tool_call_delta`、`finish_tool_calls` 在 Task 1 内被一致引用；Task 2 仅用既有 `ProviderEvent` 变体；Task 3 仅用既有 `ToolCallItem.name`。
