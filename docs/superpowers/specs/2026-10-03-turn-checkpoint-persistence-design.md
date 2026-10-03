# 进行中 turn 的 checkpoint 落盘（抗震杀 / 崩溃 / 断电）

日期：2026-10-03
状态：设计已定，待实现

## 问题

每个 thread 的会话内容只在**一轮 turn 收尾**时一次性落盘：driver 把本轮
`ItemCompleted` 累积进 `completed_items`，流结束后由 `persist_and_finish_turn`
（`yi-agent-rs/crates/yi-agent-app-server/src/server.rs:4304`）append 进
`<cwd>/.yi-agent/threads/<id>.jsonl`。全仓 `append_turn` 只有一个生产调用点
（`server.rs:4332`），没有任何增量写。

后果：

- **点 Stop** 不丢：`turn/interrupt` → driver `cancel_token.cancel()` → core 发
  `AgentEvent::Cancelled` → translator `finish_turn` → driver 常规收尾落盘
  （`server.rs:4765-4772`、`translate.rs:309`、`server.rs:4837`）。
- **直接关 App / SIGKILL / 崩溃 / 断电会丢整轮**：桌面退出没有任何收尾逻辑
  （`desktop/src-tauri/src/lib.rs` 无 `CloseRequested`/`RunEvent` 钩子），
  app-server 的 stdio EOF 只让主循环 `break`（`server.rs:1907`），**不会**中断或等待
  各 thread 的 driver（driver 是独立 spawn 的 task，随进程消失）。此时
  `completed_items` 可能还是空，整轮零落盘。

本设计只解决第二条：让**已经完成的部分**在任意时刻崩溃后仍能恢复。

## 语义（已与用户确认：方案 B）

落盘单元 = **已经 finalize 的 item**：

- 每轮的用户提问：turn 一开始就 `emit_item` 成 `ItemStarted` + `ItemCompleted`
  （`server.rs:5415-5437`、`opening_user_item` `server.rs:5405`），天然属于已 finalize，
  在 turn 开头即被抢救。
- 助手消息块：`finalize_agent_msg` 产出的 `Item::AgentMessage`
  （`translate.rs:126-143`）。
- 已完成的工具调用：`Item::ToolCall`（`translate.rs:180-191`）。

**不落盘**：正在流式输出、尚未 finalize 的助手文本（只存在于 translator 的
`active_agent_msg`），以及仍在 running 的工具调用。

崩溃恢复后，进行中的这一轮缺失"半截话"，但已产出的工作留存。这与"未崩溃时"
的边界是清楚的：**已完成的才算数**。

## 存储格式

新文件（每 thread 一个活跃 turn）：

```
<cwd>/.yi-agent/threads/<thread_id>.partial.json
```

内容：

```json
{
  "turn_id": "turn-<uuid>",
  "items": [ /* 本轮已 finalize 的 Item，含开头的用户提问 */ ],
  "messages": [ /* agent.session().messages() 的一致性前缀 */ ],
  "usage": { /* 可选，最近一次用量快照 */ }
}
```

- 主 `TurnLine::Turn` / `<id>.jsonl` / `<id>.meta.json` 格式**完全不变**。
- partial 是"可变"文件，整体原子重写（复用 `thread_store::write_atomic`）。

## 写时机

driver 侧：

1. **turn 开始**：写一次 partial，items 只含用户提问。保证"即使立刻崩溃，提问也在"。
2. **每次 item finalize**：重写 partial，items 追加该 item。
3. **去抖**：对 partial 的重写加 ~500ms 去抖——每次 item finalize 只把"脏"标记置位，
   由一个 500ms 定时或收尾路径触发实际重写；窗口内多次 finalize 合并为一次写。turn 收尾
   时无条件写一次最终态、随后删除。首次（turn 开始）写用户提问不走去抖，立即落盘。
4. **turn 收尾**（`persist_and_finish_turn` 成功 append 主 jsonl 后）：**删除** partial。

崩溃时 partial 自然留在盘上，成为恢复来源。

## 读时机（恢复）

`ThreadStore::load`：

1. 先按现有逻辑读 jsonl，得到 `items` / `messages` / `usage`。
2. 判断该 partial 是否"已被收尾"：取 partial 的**首个 item id**（即本轮开头的用户提问
   `user-<turn_id>`，全局唯一），若它已存在于 jsonl 的 items 中，则视为该轮已成功 append
   （只是 delete partial 失败）→ **忽略 partial**，返回第 1 步结果，不做任何改动。
   这条判据不需要给 `TurnLine::Turn` 增加字段，主 jsonl 格式保持不变。
   - 判据未命中 = 崩溃时该轮尚未 append → 进入第 3 步。
3. 采纳 partial：
   - 把 partial 的 `items` 接在已加载 items 之后（按 item id 去重，防御性——正常不重叠）；
   - 采纳 partial 的 `messages` 作为续聊上下文、`usage` 作为用量；
   - `LoadedThread` 增加一个标志（如 `pending_turn: bool`）告知调用方"最后一段是崩溃残留"。

**升格（promotion）**：`thread/resume` 在 `pending_turn` 为真、回放这段残轮之后，**必须**
调 `ThreadStore::promote_partial` 把它升格为权威日志的一轮：以 `TurnLine::Turn { items,
usage, messages }` append 进主 jsonl（复用 `append_turn`，`.jsonl` 格式与 `.meta.json`
均不变），随后 `clear_partial` 删除 partial。判据（`partial_is_committed`）与第 2 步
**共用**：首个 item id 已在 jsonl ⇒ 视为已收尾，只清 partial、不重复 append（因此重复
调用幂等）；partial 缺失/为空/损坏 ⇒ no-op。升格失败只记 stderr，**不阻断 resume**。

不升格的后果：采纳的 partial 是这一轮**唯一**的持久副本，用户下一条消息的 turn-start
checkpoint 会**整体原子覆盖**同一文件（`write_partial` → `write_atomic`），该轮 items
永久丢失——而 session 上下文仍"记得"它们，item 与上下文就此不一致。升格把这一轮在
partial 被覆盖之前钉进权威日志，重复 load 也不会重复计数。

**中断标记的落地机制**：`resume` 在回放完 items 之后，若 `pending_turn` 为真，额外发一条
`turn/completed{status: Interrupted}`（复用现有 `Notification`，客户端 `Session.apply` 已在
`threadStore`/`session.ts` 处理该通知，会把它显示为"这一轮被中断"）。不用新造 Item 类型，
也不改回放帧的既有顺序。

## 生命周期清理

partial 必须在以下路径一并处理，否则会残留/复活：

- `delete`：删除 `<id>.partial.json`。
- `truncate`（`/clear`，`server.rs` 的 `apply_session_command`）：删除 partial。
- `compact`：turn 收尾后覆盖，无需特殊处理（收尾即删 partial）。

说明：`exists` / `list` 不需要感知 partial——真实 thread 在 `thread/start` 时已写好
`.meta.json`，`list` 与 `find_thread_dir` 都能照常找到它；partial 只影响 `load` 的返回内容。

## 边界与错误处理

- 写 partial 失败：记 stderr，**不阻断 turn**（持久化是尽力而为，与现有
  `failed to persist turn` 同一策略）。
- partial 解析失败/损坏：忽略它（记 stderr），退回"只有 jsonl"的现状，绝不因此让
  resume 报错。
- partial 与 jsonl 的 messages 不一致：以 jsonl 为准（jsonl 是收尾后的权威）。

## 测试（TDD）

`thread_store` 单元测试：

- 写入 partial 后 load：items = jsonl items + partial items，上下文取 partial messages。
- turn 收尾后 partial 被删：load 只回 jsonl，语义与今天逐字节一致。
- partial 的 turn 已落盘（收尾成功、delete 失败）：load 忽略 partial，不重复 items。
- `promote_partial`：未收尾的 partial 被 append 成主 jsonl 的一轮并清掉；已收尾的只清
  partial、不重复 append（幂等）；缺失/为空 no-op。
- 损坏的 partial：load 退回 jsonl 内容，不报错。
- `delete` / `truncate` 对 partial 的处理。

app-server 集成测试（沿用 `Harness`）：

- 起一个慢 turn（`build_slow_agent`，`server.rs:7155` 一带），等首个 finalize item 到达后
  **直接 drop 连接/结束 serve**（模拟崩溃），再用同一 `<cwd>/.yi-agent/threads` 起新 server，
  `thread/resume`：断言提问 + 已完成的助手块按序回放，且末条是 interrupted 标记。
- 崩在"只有提问、还没产出"时：resume 至少回放出提问。
- 正常 turn 收尾后 resume：无重复、无标记，与现状一致（回归）。

## 不做（YAGNI）

- 不落盘流式中间态、不落盘半截助手文本（方案 B 的明确取舍）。
- 不改主 jsonl 的"每行一个完整 turn"语义。
- 不做跨文件的事务性保证：partial 与 jsonl 之间允许"收尾成功但 delete 失败"，由 load 的
  去重规则兜住。
