# yi-agent TUI 简化补全设计

**日期**: 2026-07-24
**状态**: 已确认，待实现
**范围**: `yi-agent-rs/crates/yi-agent` 的 app.rs 和 render/inline.rs

---

## 1. 目标

补全 TUI 起步版遗留的两个简化项：

1. **ESC 中断**：当前 `run_esc_listener` 已存在但未与 App 主循环正确集成
2. **CancellationToken 接线**：`yi-agent-core` 的 `Agent` 已实现 `cancel()` / `cancel_token()`，但 TUI 层未使用——中断仅靠 drop stream

## 2. 现状

### yi-agent-core（已实现）

- `Agent::cancel(&self)` — 触发 `CancellationToken`
- `Agent::cancel_token(&self)` — 获取 token clone
- `AgentEvent::Cancelled` — 中断时发出的事件
- `run_loop` 在三个检查点响应中断：循环起始、THINK 阶段（select!）、ACT 阶段（select!）

### yi-agent TUI（未接线）

- `app.rs` 中断时仅 `current_stream = None`（drop stream），未调用 `agent.cancel()`
- `render/inline.rs` 未处理 `AgentEvent::Cancelled` 事件
- `run_esc_listener` 逻辑基本可用，但需确认与 reedline 不冲突

## 3. 改动

### app.rs

**中断逻辑**（Ctrl+C 和 ESC 两个 `select!` 分支）：

```rust
// 改前
current_stream = None;
self.renderer.render_system("已中断");

// 改后
self.agent.cancel();
// 不再手动 render_system("已中断")——由 AgentEvent::Cancelled 事件驱动渲染
```

中断后 `AgentEvent::Cancelled` 会通过 stream 自然流出，被 `select!` 的 agent 事件分支捕获并交给 renderer 渲染。stream 随后结束（`None`），`current_stream` 置 `None`。

**`select!` agent 事件分支**：新增 `Cancelled` 处理（与 `Done` 相同——置 `current_stream = None`，由 renderer 渲染消息）。

### render/inline.rs

新增 `AgentEvent::Cancelled` 处理：

```rust
AgentEvent::Cancelled => {
    self.finish_streaming_line();
    println!("{COLOR_DIM}· 已中断{COLOR_RESET}");
}
```

### ESC listener

当前实现已可用，无需改动。`crossterm::event::poll` 的 100ms 轮询 + 即时检查模式正确。

## 4. 不改的部分

- `yi-agent-core` — CancellationToken 已完整实现，无需改动
- `/clear` — 已正确实现（重建 Agent with `Session::new()`）
- 配置、输入、main.rs — 无需改动
