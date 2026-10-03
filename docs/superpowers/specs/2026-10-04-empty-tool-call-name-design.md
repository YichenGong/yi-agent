# 空工具调用（空白工具框 / `tool not found:`）修复设计

日期：2026-10-04
状态：已设计

## 问题（已实测复现）

在 Desktop 的长会话里，UI 上出现**没有名字、没有摘要的空白工具调用框**，其
`result` 恒为 `error: tool not found: `（冒号后为空）。这些框一旦出现就**永久留在
历史里**，并随每个 turn 累积。

实测证据（真实线程文件）：

- `.worktrees/kanban/2026-10-03-resume-replay-lossless-*/​.yi-agent/threads/thread-2bc3434e-*.jsonl`：
  167 个 toolCall 中 **29 个 `name` 与 `call_id` 均为 `""`**，从 turn 3 起出现，
  canonical `messages` 里固定在第 116/128/132… 条，之后每 turn 都在。
- 主仓库 `.yi-agent/threads/thread-d9c3c943-*.jsonl` 里 `tool not found` 出现 233 次；
  `thread-245119e8-*.jsonl` 74 次。多个会话同病。

磁盘上的形态（一条空白框 = 一对）：

```json
{"role":"Assistant","content":[{"ToolUse":{"id":"","name":"","input":{"task_id":"b4a83d65-..."}}}]}
{"role":"Tool","content":[{"ToolResult":{"tool_use_id":"","content":[{"Text":"error: tool not found: "}],"is_error":true}}]}
```

而**紧随其后**往往是一次同工具、同目的的正常调用（如空 `inspect_agent(b4a83d65)`
后跟正常的 `inspect_agent(b4a83d65)`）。

## 根因

模型在流式响应里，对某次工具调用先发了一个**带 `id: ""`、且没有 `function.name`
字段**的 `tool_calls` 增量；OpenAI 兼容流解析器不做任何非空校验就把它升级为一次
工具调用：

```rust
// yi-agent-rs/crates/yi-agent-llm/src/openai/stream.rs:186-199（现状）
if let Some(id) = tc.get("id").and_then(Value::as_str) {      // "" 也是合法字符串
    let name = tc.get("function").and_then(|f| f.get("name"))
                 .and_then(Value::as_str).unwrap_or("")        // 缺 name → ""
                 .to_string();
    self.tool_calls.insert(index, (id.to_string(), name.clone()));
    events.push(ProviderEvent::ToolUseStart { id: id.to_string(), name }); // 发空名/id
}
```

连锁路径：

1. `openai/stream.rs` 发出 `ToolUseStart{id:"", name:""}`。
2. `yi-agent-core/src/provider.rs:169-188`（`accumulate_stream`）无校验地收进
   `ContentBlock::ToolUse { id:"", name:"", input }`。
3. `yi-agent-core/src/agent.rs:1144-1153` 取出 tool_uses → `agent.rs:1303-1306`
   `tools.get("")` 查不到 → `ToolResult::error("tool not found: ")`。
4. `yi-agent-app-server/src/translate.rs:236-259` 落成一个 `Item::ToolCall{ name:"" }`，
   写入 `.jsonl` 与 `messages`，并实时推给 Desktop。
5. Desktop `ToolCallCard.tsx:28` 只渲染 `{item.name}`（空），`toolSummary.ts:43-51`
   对未知名（含 `""`）返回 `null` → 头行「▸ + 空名 + 无摘要 + failed」= 空白栏。

放大效应：这些空调用已进 `messages`，之后**每轮请求都会重新序列化发给模型**
（`openai/types.rs` 按 `role/name/arguments` 序列化），既污染上下文，也可能诱导
模型继续产生同类空调用，故数量单调累积。

`anthropic/stream.rs:147-159` 有同源隐患（`unwrap_or("")` 取 id/name），本次一并
由下游兜底覆盖（见下）。

## 方案总览

三层纵深防御，**只做校验/兜底，不改变正常工具调用路径与线协议**：

1. **LLM 解析层（主修复）**：`openai/stream.rs` 按 `index` 累积，只有拿到
   **非空 `id` 且非空 `function.name`** 才发出 `ToolUseStart`；在 start 之前到达的
   arguments 先缓存，start 时按到达顺序补发为 `ToolUseDelta`。空 `id` 视为「未提供」。
   这样模型那次「丢名字的重放」要么被补全成真调用、要么被丢弃，**不再生成空名调用**。
2. **Core 累积层（协议无关兜底）**：`provider.rs` 的 `accumulate_stream` 里，
   `ToolUseStart` 的 `id` 或 `name` 为空时**忽略并 warn**，保证空名工具永不进入
   `ContentBlock`（同时覆盖 anthropic 流的同名隐患）。
3. **UI 兜底**：`ToolCallCard` 对空 `name` 显示占位 `(unknown tool)` 并展开 input，
   让**历史里已存在的旧空白框**在回放时也不再是全空白。

不变量：

- 正常工具调用的字节级行为不变（非空 name/id 的调用原样透传）。
- 线协议（`Item` / `.jsonl` / `.meta.json` / 通知方法）不变。
- 不清理历史脏数据（旧条目仍存在，但 UI 可读、且不再新增）。

## 详细设计

### 1. LLM 解析层：`openai/stream.rs`

把当前「先建 metadata、后按 id 查」的两段式改为「按 index 累积」：

```rust
struct PendingToolCall {
    id: String,        // 已见的 id（可能为空）
    name: String,      // 已见的 name（可能为空）
    args: String,      // 已累积的 arguments
    started: bool,     // 是否已发出 ToolUseStart
}
// OpenaiStream 字段：
// tool_calls: HashMap<usize, PendingToolCall>  // 取代 HashMap<usize,(String,String)>
```

处理某条 `tc`（index=i）：

1. `let id = tc.get("id").and_then(Value::as_str).unwrap_or("")` 写入 `pending.id`
   （若这次给了非空 id 覆盖之）。
2. `let name = tc.get("function").and_then(|f| f.get("name")).and_then(Value::as_str)
   .unwrap_or("")`；**若非空**则写入 `pending.name`（非空覆盖空）。
3. `let args = tc.get("function").and_then(|f| f.get("arguments")).and_then(Value::as_str)`；
   若非空则 `pending.args.push_str(args)`。
4. **若 `!pending.started` 且 `!pending.id.is_empty()` 且 `!pending.name.is_empty()`**：
   - 发 `ToolUseStart { id, name }`；
   - 若 `pending.args` 非空，立即补发一条 `ToolUseDelta { id, partial_json: args }` 并清空
     （保证「start 之后才有 delta」的事件顺序）；
   - `pending.started = true`。

`finish_reason == "tool_calls"` 时：对每个 index **按序**处理——

- 已 `started`：发 `ToolUseEnd { id }`；
- 未 `started`：说明 id/name 始终没凑齐。若 `pending.args` 非空，**降级为纯文本**：
  若 `current_text`……（此处无文本缓冲）→ 发 `TextDelta(args)`；并发一条
  `tracing::warn!(provider="openai", index, "discarding tool call with empty id/name")`。
  随后**不发** `ToolUseEnd`。
- `parse_frame` 在 `match finish` 之前已判 `self.stopped`，故 `tool_calls` 分支保持既有
  语义即可：逐 index 收尾 → 发 `Stop{EndTurn}`（无需新增守卫）。

删除旧的 `if let Some(id) = tc.get("id")` 直接发 start、以及 `self.tool_calls.get(&index)`
查 id 发 delta 的两段式逻辑（由 `PendingToolCall` 统一承载）。保持既有语义：
`id` 存在但为 `""` 时不再立即 start。

### 2. Core 累积层：`provider.rs::accumulate_stream`

```rust
ProviderEvent::ToolUseStart { id, name } => {
    if name.is_empty() || id.is_empty() {         // 新增
        tracing::warn!(?id, ?name, "accumulate_stream: ignoring tool use with empty id/name");
        // 尽量前插文本，保持既有行为；不注册空名工具
        if !current_text.is_empty() {
            content.push(ContentBlock::Text(std::mem::take(&mut current_text)));
        }
        continue;                                   // 不 insert、不发 on_event
    }
    if !current_text.is_empty() {
        content.push(ContentBlock::Text(std::mem::take(&mut current_text)));
    }
    tool_uses.insert(id, (name, String::new()));
}
```

要点：

- 一个空的 `ToolUseStart` 被丢弃后，其后续 `ToolUseDelta`（同空 id）因
  `tool_uses.get_mut("")` 查不到而被忽略（既有行为），`ToolUseEnd{id:""}` 也查不到 →
  不产生 `ContentBlock`。故**空名工具永不进入 content**。
- 该分支不把事件转发给 `on_event`（下游 translator 不会再见到空 `ToolCall`）。
- 非空 name/id 的路径逐字节不变。

### 3. UI 兜底：`desktop/src/components/ToolCallCard.tsx`

在头部把 `{item.name}` 改为：

```tsx
<span className="font-mono font-medium text-fg">
  {item.name || "(unknown tool)"}
</span>
```

这样历史里 `name:""` 的旧条目也显示为用户可理解的占位 + 其 input（已有展开区），
而不是全空白。

## 边界与错误处理

- **模型始终不发 name/id**：该调用降级为文本（把 arguments 当文本发出），并 warn；
  不产生空名 item。
- **id 后到**：`ToolUseStart` 推迟到 id/name 均非空时发出，事件顺序仍为
  `Start → Delta… → End`。
- **动画/顺序**：补发的 `ToolUseDelta` 紧跟 `ToolUseStart`，`finish_reason` 处再发
  `ToolUseEnd`，与既有测试期望（`events[0]=Start, events[1..]=Delta, events[n-2]=End,
  events[n-1]=Stop`）一致。
- **anthropic 流**：不在本次主修复范围内（其 name 通常可靠）；由第 2 层兜底覆盖。
- **历史脏数据**：不迁移、不清理；仅靠第 3 层让回放可读。

## 测试（TDD）

### `yi-agent-llm`（openai/stream.rs `mod tests`）

- **新增** `defers_tool_use_start_until_name_present`：首块 `{index:0, id:"call_x"}`
  无 name → 期望**无** `ToolUseStart`；随后 `{index:0, function:{name:"read"}}` →
  期望恰好一条 `ToolUseStart{id:"call_x", name:"read"}`。
- **新增** `buffers_arguments_arriving_before_name`：上面场景中，name 块之前先来
  `arguments:"{\"p\":"` → 期望 start 之后**紧跟**一条 `ToolUseDelta{...="{\"p\":"}`。
- **新增** `discards_tool_call_that_never_gets_a_name`：全程只有 `{index:0, id:"call_y",
  arguments:"{}"}`（无 name）→ 期望**无** `ToolUseStart`/`ToolUseEnd`；若把 arguments
  降级为文本，则期望一条 `TextDelta("{}")`。
- **保持绿**：`parses_tool_call_with_incremental_arguments`、
  `multiple_tool_calls_in_one_response`、`maps_finish_reasons_correctly`、
  `parses_done_marker_without_finish_reason`。

### `yi-agent-core`（provider.rs `mod tests`）

- **新增** `ignores_tool_use_with_empty_name`：事件 `ToolUseStart{id:"", name:""}` +
  `ToolUseDelta{id:"", "{}"}` + `ToolUseEnd{id:""}` + `Stop` → 期望 `content` **不含**
  任何 `ToolUse`（为空）。
- **新增** `ignores_tool_use_with_empty_id`：`ToolUseStart{id:"", name:"read"}` →
  content 不含 `ToolUse`。
- **保持绿**：`call_accumulates_text_and_tool_use`、
  `accumulate_stream_forwards_tool_use_delta_via_callback`、
  `accumulate_stream_multiple_tool_uses_in_order`。

### `desktop`（ToolCallCard.test.tsx）

- **新增** `shows a placeholder when the tool name is empty`：
  `render(<ToolCallCard item={bash({ command: "ls" }, { name: "" })} />)` →
  期望 `screen.getByText("(unknown tool)")`。
- **保持绿**：现有 5 个用例。

## 不做（YAGNI）

- 不改 `Item` / `.jsonl` / `.meta.json` / 通知方法。
- 不清理或迁移历史脏数据。
- 不做通用工具名白名单/校验（仅判「非空」）。
- 不在本 spec 内改 anthropic 流的解析（由第 2 层兜底）。
- 不把 arguments 的「降级为文本」做成可配置项（固定行为）。
