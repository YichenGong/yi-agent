# TUI 待发请求队列重构实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 TUI 的排队输入收敛为单一 `PendingQueue`，同时至多 1 条在途，消除队列满导致的 UI 冻结（P0）与双 FIFO 时钟错位（P1）。

**Architecture:** 新增 `PendingQueue`（`items: Vec<String>` + `in_flight: bool`）作为队列的唯一所有者。提交时按 `Sent`/`Queued`/`Rejected` 三态处理；回合结束时 `on_turn_end` 弹出下一条并**立即发送**，使"转正"与"发送"成为同一个动作。通道因此永不积压，发送改用 `try_send` 不可能阻塞。

**Tech Stack:** Rust、tokio（`mpsc`）、ratatui、cargo test。

## Global Constraints

- 队列上限：`PendingQueue::CAPACITY = 16`（与 `main.rs:1166` 的通道容量一致）。
- 通道容量保持 16 不变（同时至多 1 条在途，16 仅作安全余量）。
- 发送一律用 `try_send`，**不得**使用 `blocking_send`（P0 根因）。
- 不新增 `HistoryCell` 变体；提示复用 `HistoryCell::Separator { label: Some(..) }`。
- 不改动 app-server / agent core / headless（见 spec §7 Non-goals）。
- 工作目录：`/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix/tui-pending-queue`（本计划所有命令均在此目录下执行）。
- 所有测试命令形如：`cargo test -p yi-agent --bin yi-agent -- <filter>`（在 `yi-agent-rs/` 下执行）。

---

### Task 1: `PendingQueue` 类型

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/queued.rs`
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: 无（纯新类型）
- Produces:
  - `pub enum SubmitOutcome { Sent, Queued, Rejected }`（`Debug + Clone + Copy + PartialEq + Eq`）
  - `pub struct PendingQueue`
  - `PendingQueue::CAPACITY: usize = 16`
  - `PendingQueue::new() -> Self`
  - `PendingQueue::submit(&mut self, text: String) -> SubmitOutcome`
  - `PendingQueue::on_turn_end(&mut self) -> Option<String>`
  - `PendingQueue::len(&self) -> usize`
  - `PendingQueue::is_empty(&self) -> bool`
  - `PendingQueue::items(&self) -> &[String]`
  - `PendingQueue::clear(&mut self) -> usize`（返回丢弃条数）

- [ ] **Step 1: 写失败的测试**

在 `queued.rs` 的 `mod tests` 中追加（保留现有 5 个渲染测试不动）：

```rust
    use super::{PendingQueue, SubmitOutcome};

    #[test]
    fn idle_submit_sends_immediately_and_leaves_queue_empty() {
        let mut q = PendingQueue::new();
        assert_eq!(q.submit("a".into()), SubmitOutcome::Sent);
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn busy_submit_queues_without_sending() {
        let mut q = PendingQueue::new();
        assert_eq!(q.submit("a".into()), SubmitOutcome::Sent);
        assert_eq!(q.submit("b".into()), SubmitOutcome::Queued);
        assert_eq!(q.len(), 1);
        assert_eq!(q.items(), ["b".to_string()]);
    }

    #[test]
    fn full_queue_rejects_and_keeps_existing_items() {
        let mut q = PendingQueue::new();
        assert_eq!(q.submit("first".into()), SubmitOutcome::Sent);
        // 16 slots: the first turn is in flight, so 16 more fill the queue.
        for i in 0..PendingQueue::CAPACITY {
            assert_eq!(q.submit(format!("m{i}")), SubmitOutcome::Queued);
        }
        assert_eq!(q.len(), PendingQueue::CAPACITY);
        assert_eq!(q.submit("overflow".into()), SubmitOutcome::Rejected);
        assert_eq!(q.len(), PendingQueue::CAPACITY);
        assert!(
            !q.items().iter().any(|t| t == "overflow"),
            "a rejected message must not enter the queue"
        );
    }

    #[test]
    fn turn_end_pops_one_and_keeps_in_flight() {
        let mut q = PendingQueue::new();
        assert_eq!(q.submit("a".into()), SubmitOutcome::Sent);
        assert_eq!(q.submit("b".into()), SubmitOutcome::Queued);
        assert_eq!(q.submit("c".into()), SubmitOutcome::Queued);

        assert_eq!(q.on_turn_end(), Some("b".to_string()));
        assert_eq!(q.len(), 1);
        // The promoted message became the new in-flight turn, so a further
        // submit must still queue rather than send.
        assert_eq!(q.submit("d".into()), SubmitOutcome::Queued);
    }

    #[test]
    fn turn_end_on_empty_queue_resets_in_flight() {
        let mut q = PendingQueue::new();
        assert_eq!(q.submit("a".into()), SubmitOutcome::Sent);
        assert_eq!(q.on_turn_end(), None);
        // in_flight reset: the next submit sends immediately again.
        assert_eq!(q.submit("b".into()), SubmitOutcome::Sent);
    }

    #[test]
    fn turn_end_emits_one_item_per_call() {
        let mut q = PendingQueue::new();
        assert_eq!(q.submit("a".into()), SubmitOutcome::Sent);
        assert_eq!(q.submit("b".into()), SubmitOutcome::Queued);
        assert_eq!(q.submit("c".into()), SubmitOutcome::Queued);

        assert_eq!(q.on_turn_end(), Some("b".to_string()));
        assert_eq!(q.len(), 1, "only one message is promoted per turn end");
        assert_eq!(q.on_turn_end(), Some("c".to_string()));
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn clear_drops_waiting_messages_and_resets_in_flight() {
        let mut q = PendingQueue::new();
        assert_eq!(q.submit("a".into()), SubmitOutcome::Sent);
        assert_eq!(q.submit("b".into()), SubmitOutcome::Queued);
        assert_eq!(q.submit("c".into()), SubmitOutcome::Queued);

        assert_eq!(q.clear(), 2, "clear reports how many messages were dropped");
        assert!(q.is_empty());
        assert_eq!(q.submit("d".into()), SubmitOutcome::Sent);
    }
```

- [ ] **Step 2: 运行测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent -- tui::queued::tests 2>&1 | tail -20
```

预期：编译失败，`cannot find type PendingQueue` / `SubmitOutcome`。

- [ ] **Step 3: 实现**

在 `queued.rs` 顶部（`render_queued_preview` 之前）加入：

```rust
/// 提交结果：调用方必须处理三态，避免"拒收"被静默忽略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// Agent 空闲：消息已立即发送。
    Sent,
    /// 有轮次在途：消息已入队等待。
    Queued,
    /// 有轮次在途且队列已满：消息未被接受。
    Rejected,
}

/// 待发请求队列：队列的唯一所有者。
///
/// 不变量：`in_flight` 为真时队列是**唯一**的待发缓冲区，通道中至多 1 条消息。
/// 因此底层通道永不满，`try_send` 不会阻塞调用线程。
pub struct PendingQueue {
    items: Vec<String>,
    in_flight: bool,
}

impl PendingQueue {
    /// 与 `main.rs` 的输入通道容量一致。
    pub const CAPACITY: usize = 16;

    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            in_flight: false,
        }
    }

    /// 空闲则立即发送，忙则入队，满则拒收。
    pub fn submit(&mut self, text: String) -> SubmitOutcome {
        if !self.in_flight {
            self.in_flight = true;
            return SubmitOutcome::Sent;
        }
        if self.items.len() >= Self::CAPACITY {
            return SubmitOutcome::Rejected;
        }
        self.items.push(text);
        SubmitOutcome::Queued
    }

    /// 回合结束：弹出下一条待发消息。
    ///
    /// 弹出时 `in_flight` 保持为真——被弹出的消息成为新的在途轮次。
    /// 队列为空时才复位，表示真正回到空闲。
    pub fn on_turn_end(&mut self) -> Option<String> {
        match self.items.is_empty() {
            false => Some(self.items.remove(0)),
            true => {
                self.in_flight = false;
                None
            }
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 供预览渲染使用的只读视图。
    pub fn items(&self) -> &[String] {
        &self.items
    }

    /// 丢弃全部待发消息并复位 `in_flight`，返回丢弃条数。
    ///
    /// 用于 `/clear`：driver 会重建 agent，TUI 必须同时复位在途标记，
    /// 否则会认为仍有轮次在跑而永不再发送。
    pub fn clear(&mut self) -> usize {
        let dropped = self.items.len();
        self.items.clear();
        self.in_flight = false;
        dropped
    }
}

impl Default for PendingQueue {
    fn default() -> Self {
        Self::new()
    }
}
```

由于 Task 3 才会接线，先临时允许未使用（沿用本项目既有做法，参见 commit `421a337`）：

```rust
#[allow(dead_code)]
impl PendingQueue {
```

（即把上面 `impl PendingQueue {` 改为该形式。`const CAPACITY` 与 `Default` 实现同样被覆盖。）

- [ ] **Step 4: 运行测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent -- tui::queued::tests 2>&1 | tail -20
```

预期：`12 passed`（原 5 个渲染测试 + 新增 7 个）。

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/queued.rs
git commit -m "feat(tui): add PendingQueue owning submitted-input state

A single owner for the pending queue plus an in_flight flag. submit()
reports Sent/Queued/Rejected so a rejection cannot be silently dropped,
and on_turn_end() pops one message while keeping in_flight set, because
the promoted message becomes the next in-flight turn."
```

---

### Task 2: `render_queued_preview` 接收切片

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/queued.rs`
- Test: 同文件 `mod tests`（现有 5 个渲染测试）

**Interfaces:**
- Consumes: Task 1 的 `PendingQueue::items() -> &[String]`
- Produces: `render_queued_preview(items: &[String], width: u16) -> Vec<Line<'static>>`

**理由:** 改为切片后，Task 3 的布局预计算可直接用 `&queue.items()[n..]` 渲染"弹出后"的状态，无需克隆整个队列（当前实现每帧 `queued.clone()`）。

- [ ] **Step 1: 改签名并适配现有测试**

把 `render_queued_preview` 的签名与实现开头改为：

```rust
pub fn render_queued_preview(items: &[String], _width: u16) -> Vec<Line<'static>> {
    if items.is_empty() {
        return Vec::new();
    }
```

函数体内所有 `queued` 引用改为 `items`（`queued.len()` → `items.len()`，`queued.iter().take(show)` → `items.iter().take(show)`）。

移除文件顶部的 `use std::collections::VecDeque;`（若无其他使用者）。

现有 5 个测试的构造改为 `Vec<String>`：

```rust
    #[test]
    fn empty_queue_returns_no_lines() {
        let q: Vec<String> = Vec::new();
        let lines = render_queued_preview(&q, 80);
        assert!(lines.is_empty());
    }

    #[test]
    fn single_message_has_header_and_one_row() {
        let q = vec!["hello".to_string()];
        let lines = render_queued_preview(&q, 80);
        assert_eq!(lines.len(), 2);
        let title: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(title, "⌛ 排队中 (1)");
    }

    #[test]
    fn three_messages_shows_all_three() {
        let q = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let lines = render_queued_preview(&q, 80);
        // 1 header + 3 messages
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn five_messages_truncates_with_count_line() {
        let q: Vec<String> = (0..5).map(|i| format!("msg{i}")).collect();
        let lines = render_queued_preview(&q, 80);
        // 1 header + 3 messages + 1 overflow count
        assert_eq!(lines.len(), 5);
        let last: String = lines[4]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(last, "  … 还有 2 条");
    }

    #[test]
    fn header_shows_total_not_visible() {
        let q: Vec<String> = (0..10).map(|_| "x".to_string()).collect();
        let lines = render_queued_preview(&q, 80);
        let title: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(title, "⌛ 排队中 (10)");
    }
```

- [ ] **Step 2: 运行测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent -- tui::queued::tests 2>&1 | tail -20
```

预期：`12 passed`。此时 `app.rs` 仍传 `&VecDeque<String>`，会编译失败——这是预期的，Task 3 修复。

- [ ] **Step 3: 提交**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/queued.rs
git commit -m "refactor(tui): take a slice in render_queued_preview

Lets the layout pre-computation render the post-promotion queue state via
&items[n..] instead of cloning the whole queue every frame."
```

---

### Task 3: 接线提交与回合结束路径（修复 P0 + P1）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`
- Test: `app.rs` 的 `mod tests`

**Interfaces:**
- Consumes: `PendingQueue`、`SubmitOutcome`、`render_queued_preview(&[String], u16)`
- Produces: `handle_key` 与 `run_loop` 内部改用 `&mut PendingQueue`；`execute_slash_command` 暂不变（Task 4 处理）

**关键背景:** driver 在 `main.rs:1440` 清除 `is_running`，该语句位于排空循环**之后**，而 `Done` 在循环**内**发出。故 TUI 见 `Done` 时 `is_running` 仍为 `true`。**不得**用 `is_running` 判断"是否该发下一条"，否则 TUI 与 driver 互等死锁。判据一律用 `PendingQueue::in_flight`（经 `submit`/`on_turn_end` 间接体现）。

- [ ] **Step 1: 改 `run_loop` 的状态初始化**

`app.rs:221` 改为：

```rust
    let mut queued = crate::tui::queued::PendingQueue::new();
```

- [ ] **Step 2: 改布局预计算**

把 `app.rs:263-286` 的 `promotion_count` / `final_queue` 段替换为：

```rust
        // A completed turn promotes one queued item into history and sends it.
        // Determine the resulting layout before applying those history mutations.
        let promotion_count = pending_events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    AgentEvent::Done { .. } | AgentEvent::Cancelled | AgentEvent::Error(_)
                )
            })
            .count()
            .min(queued.len());
        let final_queued_lines =
            crate::tui::queued::render_queued_preview(&queued.items()[promotion_count..], width);
```

- [ ] **Step 3: 改回合结束的转正+发送**

把 `app.rs:289-307` 的事件循环替换为：

```rust
        for event in pending_events {
            let is_turn_end = matches!(
                event,
                AgentEvent::Done { .. } | AgentEvent::Cancelled | AgentEvent::Error(_)
            );
            route_event(
                &mut task_registry,
                &mut statusbar_state,
                &mut cost_tracker,
                &event,
            );
            history.push_event(event, final_history_area.width);
            // 回合结束:弹出下一条待发消息,立即发送并「转正」进 history。
            // 发送与转正是同一个动作,不再依赖 driver 是否取走。
            if is_turn_end {
                if let Some(text) = queued.on_turn_end() {
                    let _ = input_tx.try_send(text.clone());
                    history.push(HistoryCell::UserMessage { text }, final_history_area.width);
                }
            }
        }
```

- [ ] **Step 4: 改渲染调用**

`app.rs:320` 改为：

```rust
        let queued_lines = crate::tui::queued::render_queued_preview(queued.items(), width);
```

- [ ] **Step 5: 改 `handle_key` 签名与提交分支**

签名（`app.rs:830`）改为：

```rust
    queued: &mut crate::tui::queued::PendingQueue,
```

提交分支（`app.rs:1085-1095`）替换为：

```rust
            *popup = None;
            use crate::tui::queued::SubmitOutcome;
            match queued.submit(text.clone()) {
                SubmitOutcome::Sent => {
                    history.push(
                        HistoryCell::UserMessage { text: text.clone() },
                        history_width,
                    );
                    let _ = input_tx.try_send(text.clone());
                }
                SubmitOutcome::Queued => {
                    // 只在预览区显示,发送推迟到本回合结束。
                }
                SubmitOutcome::Rejected => {
                    // 文本已被 take_submitted 取走,必须退回,否则静默丢失。
                    // 此处 `text` 是 String(非对 input 的借用),故可变借 input 合法。
                    input.insert_str(&text);
                    history.push(
                        HistoryCell::Separator {
                            label: Some(format!(
                                "排队已满 ({})，本条未发送，已退回输入框",
                                crate::tui::queued::PendingQueue::CAPACITY
                            )),
                        },
                        history_width,
                    );
                }
            }
            KeyOutcome::Submit(text)
```

- [ ] **Step 6: 适配测试构造（15 处调用点）**

`handle_key` 的调用点需把 `&mut queued` 的构造从 `VecDeque::new()` 改为 `PendingQueue::new()`。逐个替换：

```bash
cd yi-agent-rs && sed -i '' 's/let mut queued = VecDeque::new();/let mut queued = crate::tui::queued::PendingQueue::new();/g; s/let mut queued: VecDeque<String> = VecDeque::new();/let mut queued = crate::tui::queued::PendingQueue::new();/g' crates/yi-agent/src/tui/app.rs
```

再检查是否残留 `VecDeque` 引用：

```bash
cd yi-agent-rs && grep -n 'VecDeque' crates/yi-agent/src/tui/app.rs
```

预期：仅剩测试模块的导入行 `use std::collections::VecDeque;`（约 `app.rs:2320`）。删除该行：

```bash
cd yi-agent-rs && sed -i '' '/^    use std::collections::VecDeque;$/d' crates/yi-agent/src/tui/app.rs
```

再次确认无残留（预期无输出）：

```bash
cd yi-agent-rs && grep -n 'VecDeque' crates/yi-agent/src/tui/app.rs
```

注：`handle_key` 的 `queued` 形参类型决定各测试点 `let mut queued = PendingQueue::new();` 的类型推断，故无需显式标注。`app.rs` 内共 15 处调用点（生产 1 处 + 测试 14 处），`sed` 的两种模式已覆盖全部 `VecDeque::new()` 写法。

- [ ] **Step 7: 更新既有测试的通道断言**

`submit_while_running_goes_to_queue_not_history`（`app.rs` 内）原先断言 `input_rx.try_recv()` 能收到消息。**该断言必须删除**——忙时提交现在**不**发通道（这正是 P0 的修复）。改为：

```rust
            KeyOutcome::Submit(text) => {
                assert_eq!(text, "queued msg");
                assert_eq!(queued.len(), 1);
                assert_eq!(queued.items(), ["queued msg".to_string()]);
                assert!(
                    history.cells.is_empty(),
                    "history should be empty when agent running"
                );
                assert!(
                    input_rx.try_recv().is_err(),
                    "a queued message must NOT be sent while a turn is in flight"
                );
            }
```

同时把该测试中 `input_rx` 的绑定由 `mut input_rx` 保留（`try_recv` 需要 `&mut`）。

- [ ] **Step 8: 新增 P0 回归测试**

在 `app.rs` 的 `mod tests` 中新增：

```rust
    #[test]
    fn full_queue_rejects_and_restores_input_without_blocking() {
        let (input_tx, mut input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(true));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::PendingQueue::new();
        let mut pending_quit = false;
        let mut popup = None;

        // Fill the queue to capacity.
        for i in 0..crate::tui::queued::PendingQueue::CAPACITY {
            input.buffer = format!("msg{i}");
            input.cursor = input.buffer.len();
            let _ = handle_key(
                make_key(KeyCode::Enter, KeyModifiers::NONE),
                &mut input,
                &mut history,
                1000,
                80,
                24,
                &CostTracker::default(),
                &input_tx,
                &interrupt_tx,
                &control_tx,
                &decision_tx,
                &is_running,
                &mut queued,
                &mut pending_quit,
                &mut popup,
            );
        }
        assert_eq!(queued.len(), crate::tui::queued::PendingQueue::CAPACITY);
        let history_len_before = history.cells.len();

        // The overflow submit must not block and must not be accepted.
        input.buffer = "overflow".to_string();
        input.cursor = input.buffer.len();
        let _ = handle_key(
            make_key(KeyCode::Enter, KeyModifiers::NONE),
            &mut input,
            &mut history,
            1000,
            80,
            24,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
        );

        assert_eq!(
            queued.len(),
            crate::tui::queued::PendingQueue::CAPACITY,
            "the queue must stay at capacity"
        );
        assert_eq!(
            input.buffer, "overflow",
            "the rejected text must be restored to the input line"
        );
        assert!(
            history.cells.len() > history_len_before,
            "the rejection must be visible in history"
        );
        assert!(
            input_rx.try_recv().is_err(),
            "nothing may reach the channel while a turn is in flight"
        );
    }
```

- [ ] **Step 9: 新增 P1 死锁回归测试**

该测试直接覆盖 spec §1.3：即使 `is_running` 仍为 `true`，回合结束后也必须能发送下一条。

```rust
    #[test]
    fn turn_end_sends_next_queued_message_even_while_is_running_is_true() {
        let (input_tx, mut input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::PendingQueue::new();
        let mut pending_quit = false;
        let mut popup = None;

        // First submit: idle -> sent. Second: queued.
        for text in ["first", "second"] {
            input.buffer = text.to_string();
            input.cursor = input.buffer.len();
            let _ = handle_key(
                make_key(KeyCode::Enter, KeyModifiers::NONE),
                &mut input,
                &mut history,
                1000,
                80,
                24,
                &CostTracker::default(),
                &input_tx,
                &interrupt_tx,
                &control_tx,
                &decision_tx,
                // The driver has NOT yet cleared this flag: this mirrors the
                // real ordering, where Done is emitted before is_running=false.
                &Arc::new(AtomicBool::new(true)),
                &mut queued,
                &mut pending_quit,
                &mut popup,
            );
        }
        assert_eq!(input_rx.try_recv().unwrap(), "first");
        assert!(input_rx.try_recv().is_err(), "second must still be queued");
        assert_eq!(queued.len(), 1);

        // Turn ends: the queued message is sent despite is_running being true.
        assert_eq!(queued.on_turn_end(), Some("second".to_string()));
        let _ = input_tx.try_send("second".to_string());
        assert_eq!(input_rx.try_recv().unwrap(), "second");
    }
```

- [ ] **Step 10: 运行测试**

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent -- tui:: 2>&1 | tail -25
```

预期：全部通过（含新增 2 个 + 适配后的既有测试）。

- [ ] **Step 11: 运行完整测试套件**

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent 2>&1 | tail -15
```

预期：全部通过，无失败。

- [ ] **Step 12: 提交**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/app.rs
git commit -m "fix(tui): single-owner queue with at most one message in flight

P0: the 17th queued message used blocking_send on a cap-16 channel the
driver only drains between turns, freezing the TUI's spawn_blocking thread
until the turn ended. The queue now holds waiting messages and at most one
is ever in the channel, so try_send cannot block.

P1: promotion to history and sending are now one action keyed on the
queue's own in_flight flag, not on is_running. is_running is still true at
Done time (the driver clears it only after draining the stream), so keying
sends off it would deadlock the TUI against the driver.

A rejected message is restored to the input line: take_submitted already
cleared the buffer, so dropping it would silently lose the user's text."
```

---

### Task 4: `/clear` 清空待发队列

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`
- Test: `app.rs` 的 `mod tests`

**Interfaces:**
- Consumes: `PendingQueue::clear() -> usize`
- Produces: `execute_slash_command(..., queued: &mut PendingQueue, ...)`（新增参数）

- [ ] **Step 1: 写失败的测试**

在 `app.rs` 的 `mod tests` 中新增：

```rust
    #[test]
    fn clear_command_drops_pending_queue_and_resets_in_flight() {
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, mut control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let mut history = HistoryState::new();
        let mut queued = crate::tui::queued::PendingQueue::new();

        // One in flight plus two waiting.
        use crate::tui::queued::SubmitOutcome;
        assert_eq!(queued.submit("a".into()), SubmitOutcome::Sent);
        assert_eq!(queued.submit("b".into()), SubmitOutcome::Queued);
        assert_eq!(queued.submit("c".into()), SubmitOutcome::Queued);

        let outcome = execute_slash_command(
            SlashCommand::Clear,
            None,
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &mut queued,
        );

        assert_eq!(outcome, KeyOutcome::None);
        assert!(queued.is_empty(), "/clear must drop waiting messages");
        // in_flight reset: a later submit sends immediately again.
        assert_eq!(queued.submit("d".into()), SubmitOutcome::Sent);
        assert!(matches!(
            control_rx.try_recv(),
            Ok(crate::ControlCommand::Clear)
        ));
    }
```

- [ ] **Step 2: 运行测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent -- tui::app::tests::clear_command_drops 2>&1 | tail -20
```

预期：编译失败，`execute_slash_command` 参数数量不匹配。

- [ ] **Step 3: 改 `execute_slash_command` 签名与 Clear 分支**

签名（`app.rs:1144-1151`）在 `control_tx` 之后加入：

```rust
    queued: &mut crate::tui::queued::PendingQueue,
```

`SlashCommand::Clear` 分支改为：

```rust
        SlashCommand::Clear => {
            // 本地清空 history 显示,TUI 不等 driver 确认。
            // 通过 control channel 通知 driver 重建 agent(空 session)。
            let dropped = queued.clear();
            history.clear();
            history.push(
                HistoryCell::Separator {
                    label: Some("对话已清空".to_string()),
                },
                width,
            );
            if dropped > 0 {
                // 清空必须可见:否则用户以为排队消息还在。
                history.push(
                    HistoryCell::Separator {
                        label: Some(format!("已丢弃 {dropped} 条排队消息")),
                    },
                    width,
                );
            }
            let _ = control_tx.blocking_send(crate::ControlCommand::Clear);
            KeyOutcome::None
        }
```

- [ ] **Step 4: 更新两处调用点**

`app.rs:989` 与 `app.rs:1063` 的 `execute_slash_command(...)` 调用，在 `&control_tx,` 之后各加一行 `queued,`。

- [ ] **Step 5: 更新测试调用点**

`app.rs` 内两个测试调用 `execute_slash_command`（约 `5964`、`5998`）同样补 `&mut queued` 参数；若测试未构造 `queued`，加：

```rust
        let mut queued = crate::tui::queued::PendingQueue::new();
```

- [ ] **Step 6: 运行测试**

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent -- tui:: 2>&1 | tail -20
```

预期：全部通过。

- [ ] **Step 7: 提交**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/app.rs
git commit -m "feat(tui): /clear drops pending messages and resets in_flight

/clear means start over, so keeping messages queued under the previous
context contradicts it. Resetting in_flight is required too: the driver
rebuilds the agent, so a stale in-flight flag would make the TUI believe a
turn is still running and never send again. The drop count is shown so the
loss is visible."
```

---

### Task 5: 文档

**Files:**
- Modify: `docs/bug-list.md`
- Modify: `docs/project-management/yi-agent-tui.md`

**Interfaces:**
- Consumes: Task 1-4 的实现
- Produces: 无代码接口

- [ ] **Step 1: 关闭 bug-list 条目**

`docs/bug-list.md` 中该行：

```
- [ ] 排队user request加入对话的逻辑不是很清晰。
```

改为：

```
- [x] 排队user request加入对话的逻辑不是很清晰。（修复：队列收敛为单一 `PendingQueue`（`tui/queued.rs`），同时至多 1 条在途，发送改用 `try_send`，消除队列满时 `blocking_send` 冻结 UI；转正与发送合并为同一动作，不再依赖 driver 是否取走；满队列拒收并把文本退回输入框。见 [设计](../superpowers/specs/2026-09-27-tui-pending-queue-design.md)、[计划](../superpowers/plans/2026-09-27-tui-pending-queue.md)。验证：`cargo test -p yi-agent --bin yi-agent -- tui::queued::tests`、`cargo test -p yi-agent --bin yi-agent -- tui::app::tests`）
```

- [ ] **Step 2: 更新项目管理状态**

`docs/project-management/yi-agent-tui.md:37` 的条目补充实现要点：

```
- [x] 输入排队 — 单一 `PendingQueue`（`tui/queued.rs`）持有待发队列与 `in_flight`，同时至多 1 条在途，`try_send` 不阻塞；满队列拒收并退回输入框 — [设计](../superpowers/specs/2026-09-27-tui-pending-queue-design.md)
```

- [ ] **Step 3: 提交**

```bash
git add docs/bug-list.md docs/project-management/yi-agent-tui.md
git commit -m "docs: record the TUI pending-queue fix and close the bug-list entry"
```

---

## 验收标准

全部满足方可视为完成：

1. `cargo test -p yi-agent --bin yi-agent` 全绿。
2. 全仓无 `blocking_send` 用于 `input_tx`（`grep -rn 'input_tx.blocking_send' yi-agent-rs/crates/yi-agent/src/` 应无输出）。
3. 全仓无 `VecDeque` 用于队列状态（`grep -n 'VecDeque' yi-agent-rs/crates/yi-agent/src/tui/app.rs` 应无输出）。
4. `PendingQueue` 上不再有 `#[allow(dead_code)]`（Task 3 接线后应移除）。

## Self-Review 记录

- **Spec 覆盖：** §1.1 P0 → Task 3 Step 5/8；§1.2 P1 → Task 3 Step 3/9；§1.3 `in_flight` 必要性 → Task 3 Step 9；§2 架构 → Task 1；§3.1 提交三态 → Task 3 Step 5；§3.2 回合结束 → Task 3 Step 3；§3.3 `try_send` → Task 3 Step 5；§4.1 `/clear` → Task 4；§4.2 附带修正 → 未改未知 slash 行为（仅保留注释），退出丢消息提示未实现（spec 列为范围外）；§5 影响面 → Task 3 Step 6；§6 测试计划 → Task 1 Step 1、Task 3 Step 8/9、Task 4 Step 1。
- **类型一致性：** `submit`/`on_turn_end`/`len`/`is_empty`/`items`/`clear` 的签名在 Task 1 定义，Task 3/4 使用处一致；`render_queued_preview` 在 Task 2 改为 `&[String]`，Task 3 两处调用均传切片。
- **已知缺口：** spec §4.2 提到的"退出前打印丢弃条数"未纳入任务。属 Non-goals 边缘项，若需要应另开任务。
