# TUI 流式中输入的可视化与打断 实现计划

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 在 Ratatui TUI 中,流式响应期间用户输入的消息在输入框上方独立预览区显示(带 dim + italic 样式),被 driver 消费时才「转正」进 history;Esc / Ctrl+C 单击打断当前 agent,双击退出。

**Architecture:** TUI 侧新增 `queued: VecDeque<String>` 与 `input_tx` channel buffer 一一对应。提交时按 `is_running` 分流:空闲走老路(立即进 history),运行中走新路(进 `queued` 预览)。`AgentEvent::Done/Cancelled/Error` 时从 `queued` 转正。Esc / Ctrl+C 复用现有 `pending_quit` 机制,agent 运行时同时发 `interrupt_tx`,driver 侧已有 cancel 路径无需改动。

**Tech Stack:** Rust, ratatui 0.29, crossterm, tokio mpsc, std `VecDeque`

**Design doc:** `docs/plans/2026-07-25-tui-queued-input-design.md`

**Worktree:** `.worktrees/tui-queued-input` (branch `feature/tui-queued-input`)

---

## Task 1: 新增 queued 预览渲染函数

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/src/tui/queued.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/mod.rs:1-9`

这个 task 独立、纯函数,先做。渲染逻辑参考 codex `pending_input_preview.rs`,但我们只有一类消息(queued),更简单。

**Step 1: 写 `queued.rs` 的失败测试**

在 `yi-agent-rs/crates/yi-agent/src/tui/queued.rs` 写:

```rust
use std::collections::VecDeque;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

/// 渲染排队预览区。返回若干行,空队列返回空 Vec。
///
/// - 标题行:`⌛ 排队中 (N)`,dim,N = 总数
/// - 每条消息:`  ↳ ` 前缀,dim + italic
/// - 最多显示 3 行,超出显示 `… 还有 X 条` 计数行
pub fn render_queued_preview(queued: &VecDeque<String>, _width: u16) -> Vec<Line<'static>> {
    if queued.is_empty() {
        return Vec::new();
    }

    let dim = Style::new().add_modifier(Modifier::DIM);
    let dim_italic = Style::new().add_modifier(Modifier::DIM | Modifier::ITALIC);

    let mut lines = Vec::new();
    lines.push(Line::from(vec![Span::styled(
        format!("⌛ 排队中 ({})", queued.len()),
        dim,
    )]));

    let visible_count = 3;
    let total = queued.len();
    let show = total.min(visible_count);
    for text in queued.iter().take(show) {
        lines.push(Line::from(vec![
            Span::styled("  ↳ ", dim),
            Span::styled(text.clone(), dim_italic),
        ]));
    }
    if total > visible_count {
        let remaining = total - visible_count;
        lines.push(Line::from(vec![Span::styled(
            format!("  … 还有 {remaining} 条"),
            dim,
        )]));
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_queue_returns_no_lines() {
        let q = VecDeque::new();
        let lines = render_queued_preview(&q, 80);
        assert!(lines.is_empty());
    }

    #[test]
    fn single_message_has_header_and_one_row() {
        let mut q = VecDeque::new();
        q.push_back("hello".to_string());
        let lines = render_queued_preview(&q, 80);
        assert_eq!(lines.len(), 2);
        let title: String = lines[0].spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(title, "⌛ 排队中 (1)");
    }

    #[test]
    fn three_messages_shows_all_three() {
        let mut q = VecDeque::new();
        q.push_back("a".to_string());
        q.push_back("b".to_string());
        q.push_back("c".to_string());
        let lines = render_queued_preview(&q, 80);
        // 1 header + 3 messages
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn five_messages_truncates_with_count_line() {
        let mut q = VecDeque::new();
        for i in 0..5 {
            q.push_back(format!("msg{i}"));
        }
        let lines = render_queued_preview(&q, 80);
        // 1 header + 3 messages + 1 overflow count
        assert_eq!(lines.len(), 5);
        let last: String = lines[4].spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(last, "  … 还有 2 条");
    }

    #[test]
    fn header_shows_total_not_visible() {
        let mut q = VecDeque::new();
        for _ in 0..10 {
            q.push_back("x".to_string());
        }
        let lines = render_queued_preview(&q, 80);
        let title: String = lines[0].spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(title, "⌛ 排队中 (10)");
    }
}
```

**Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib tui::queued -- --nocapture`
Expected: 编译失败,因为 `queued.rs` 不存在 / `mod.rs` 没声明模块

**Step 3: 在 `mod.rs` 声明模块**

Edit `yi-agent-rs/crates/yi-agent/src/tui/mod.rs`,在 `pub mod markdown;` 之后加 `pub mod queued;`:

```rust
//! ratatui-based TUI with structured history cells.

pub mod app;
pub mod cell;
pub mod history;
pub mod input;
pub mod markdown;
pub mod queued;
pub mod slash;
```

**Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib tui::queued -- --nocapture`
Expected: 5 个测试 PASS

**Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/tui/queued.rs crates/yi-agent/src/tui/mod.rs
git commit -m "feat(tui): add queued preview render function"
```

---

## Task 2: 改造 `handle_key` 支持 Esc/Ctrl+C 打断

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:231-327` (handle_key 签名 + Esc/Ctrl+C 分支)
- Test: `yi-agent-rs/crates/yi-agent/src/tui/app.rs` (现有测试模块,行尾)

`handle_key` 目前签名缺 `is_running` 和 `queued`。先把签名加上,再改造 Esc/Ctrl+C 分支。

**Step 1: 写失败测试**

在 `app.rs` 末尾的 `#[cfg(test)] mod tests` 里加(如果没有 test module 就新建一个):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn make_key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn esc_when_running_sends_interrupt() {
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        let (agent_tx, mut agent_rx) = mpsc::channel::<yi_agent_core::AgentEvent>(16);
        let (input_tx, mut input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(1);
        let (decision_tx, _decision_rx) = mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(true)); // agent 在跑

        // 用一个空的 EventSource(不会产生事件),靠 poll 超时退出循环
        struct NoEvents;
        impl EventSource for NoEvents {
            fn poll(&self, _timeout: Duration) -> std::io::Result<Option<Event>> {
                Ok(None)
            }
        }

        let handle = std::thread::spawn(move || {
            let mut history = HistoryState::new();
            let mut input = InputLine::new();
            let mut queued: VecDeque<String> = VecDeque::new();
            run_loop(
                &mut terminal,
                &mut agent_rx,
                &mut history,
                &mut input,
                &input_tx,
                &interrupt_tx,
                &decision_tx,
                &is_running,
                &NoEvents,
            )
        });

        // 直接调用 handle_key 验证 interrupt 发送
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued: VecDeque<String> = VecDeque::new();
        let mut pending_quit = false;
        let mut popup = None;
        let result = handle_key(
            make_key(KeyCode::Esc, KeyModifiers::NONE),
            &mut input,
            &mut history,
            &input_tx,
            &interrupt_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
        );
        assert_eq!(result, KeyOutcome::None); // 第一次 Esc 不退出
        assert!(pending_quit); // pending_quit 置位
        // interrupt 已发
        let got = interrupt_rx.try_recv();
        assert!(got.is_ok(), "interrupt should be sent when agent running");

        // 清理 driver 循环
        drop(input_tx);
        drop(interrupt_tx);
        let _ = handle.join().unwrap();
        let _ = input_rx.try_recv();
    }

    #[test]
    fn esc_when_idle_does_not_send_interrupt() {
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(1);
        let (decision_tx, _decision_rx) = mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false)); // agent 空闲
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued: VecDeque<String> = VecDeque::new();
        let mut pending_quit = false;
        let mut popup = None;

        let result = handle_key(
            make_key(KeyCode::Esc, KeyModifiers::NONE),
            &mut input,
            &mut history,
            &input_tx,
            &interrupt_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
        );
        assert_eq!(result, KeyOutcome::None);
        assert!(pending_quit);
        // interrupt 不应发送
        assert!(interrupt_rx.try_recv().is_err(), "interrupt should NOT be sent when idle");
    }

    #[test]
    fn ctrl_c_when_running_sends_interrupt() {
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(1);
        let (decision_tx, _decision_rx) = mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(true));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued: VecDeque<String> = VecDeque::new();
        let mut pending_quit = false;
        let mut popup = None;

        let result = handle_key(
            make_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut input,
            &mut history,
            &input_tx,
            &interrupt_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
        );
        assert_eq!(result, KeyOutcome::None);
        assert!(pending_quit);
        assert!(interrupt_rx.try_recv().is_ok());
    }

    #[test]
    fn double_esc_quits() {
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (decision_tx, _decision_rx) = mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(true));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued: VecDeque<String> = VecDeque::new();
        let mut pending_quit = false;
        let mut popup = None;

        // 第一次 Esc
        let _ = handle_key(
            make_key(KeyCode::Esc, KeyModifiers::NONE),
            &mut input,
            &mut history,
            &input_tx,
            &interrupt_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
        );
        // 第二次 Esc
        let result = handle_key(
            make_key(KeyCode::Esc, KeyModifiers::NONE),
            &mut input,
            &mut history,
            &input_tx,
            &interrupt_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
        );
        assert_eq!(result, KeyOutcome::Quit);
    }
}
```

**Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib tui::app::tests -- --nocapture 2>&1 | head -30`
Expected: 编译失败,因为 `handle_key` 签名不匹配(缺 `is_running`、`queued` 参数)

**Step 3: 改造 `handle_key` 签名和 Esc/Ctrl+C 分支**

在 `app.rs`:

(a) `handle_key` 签名(行 231-240)加 `is_running` 和 `queued`:

```rust
#[allow(clippy::too_many_arguments)]
fn handle_key(
    key: KeyEvent,
    input: &mut InputLine,
    history: &mut HistoryState,
    input_tx: &tokio::sync::mpsc::Sender<String>,
    interrupt_tx: &tokio::sync::mpsc::Sender<()>,
    decision_tx: &tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
    is_running: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    queued: &mut std::collections::VecDeque<String>,
    pending_quit: &mut bool,
    popup: &mut Option<CommandPopup>,
) -> KeyOutcome {
```

(b) Esc 和 Ctrl+C 分支(行 279-297)合并改造:

把现有:
```rust
        KeyCode::Esc => {
            if *pending_quit {
                return KeyOutcome::Quit;
            }
            // If popup is active, Esc dismisses it (without setting pending_quit)
            if popup.is_some() {
                *popup = None;
                return KeyOutcome::None;
            }
            *pending_quit = true;
            return KeyOutcome::None;
        }
        KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
            if *pending_quit {
                return KeyOutcome::Quit;
            }
            *pending_quit = true;
            return KeyOutcome::None;
        }
```

改成:
```rust
        KeyCode::Esc => {
            if *pending_quit {
                return KeyOutcome::Quit;
            }
            // If popup is active, Esc dismisses it (without setting pending_quit)
            if popup.is_some() {
                *popup = None;
                return KeyOutcome::None;
            }
            *pending_quit = true;
            if is_running.load(std::sync::atomic::Ordering::SeqCst) {
                let _ = interrupt_tx.blocking_send(());
            }
            return KeyOutcome::None;
        }
        KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
            if *pending_quit {
                return KeyOutcome::Quit;
            }
            *pending_quit = true;
            if is_running.load(std::sync::atomic::Ordering::SeqCst) {
                let _ = interrupt_tx.blocking_send(());
            }
            return KeyOutcome::None;
        }
```

(c) 更新 `run_loop` 里 `handle_key` 的调用点(行 198-207),加上 `is_running` 和 `&mut queued` 参数。这一步只改调用点,具体 `queued` 逻辑在 Task 3。临时在 `run_loop` 顶部加 `let mut queued: std::collections::VecDeque<String> = std::collections::VecDeque::new();` 让编译通过。

**Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib tui::app::tests -- --nocapture`
Expected: 4 个测试 PASS(esc_when_running, esc_when_idle, ctrl_c_when_running, double_esc_quits)

**Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/tui/app.rs
git commit -m "feat(tui): wire Esc/Ctrl+C to interrupt_tx when agent running"
```

---

## Task 3: 排队分流 + 转正 + 渲染区接入 `run_loop`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:137-222` (run_loop)
- Test: `yi-agent-rs/crates/yi-agent/src/tui/app.rs` (tests 模块)

这是核心 task。三件事:(1) 提交按 `is_running` 分流 (2) Done/Cancelled/Error 时转正 (3) layout 加 queued preview 区。

**Step 1: 写失败测试**

在 `app.rs` tests 模块再加一个测试:

```rust
    #[test]
    fn submit_while_running_goes_to_queue_not_history() {
        use yi_agent_core::{AgentEvent, DoneReason};

        let (input_tx, mut input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (decision_tx, _decision_rx) = mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(true));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued: VecDeque<String> = VecDeque::new();
        let mut pending_quit = false;
        let mut popup = None;

        // 模拟输入框有内容
        input.buffer = "queued msg".to_string();
        input.cursor = input.buffer.len();

        let result = handle_key(
            make_key(KeyCode::Enter, KeyModifiers::NONE),
            &mut input,
            &mut history,
            &input_tx,
            &interrupt_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
        );
        // Enter 走 InputAction::Submit 分支,最终 handle_key 返回 KeyOutcome::Submit
        match result {
            KeyOutcome::Submit(text) => {
                assert_eq!(text, "queued msg");
                // 应该进 queued,不进 history
                assert_eq!(queued.len(), 1);
                assert_eq!(queued[0], "queued msg");
                assert!(history.cells.is_empty(), "history should be empty when queued");
                // 同时发到 input_tx
                let received = input_rx.try_recv();
                assert!(received.is_ok());
                assert_eq!(received.unwrap(), "queued msg");
            }
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn submit_while_idle_goes_to_history_not_queue() {
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (decision_tx, _decision_rx) = mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false)); // 空闲
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued: VecDeque<String> = VecDeque::new();
        let mut pending_quit = false;
        let mut popup = None;

        input.buffer = "idle msg".to_string();
        input.cursor = input.buffer.len();

        let result = handle_key(
            make_key(KeyCode::Enter, KeyModifiers::NONE),
            &mut input,
            &mut history,
            &input_tx,
            &interrupt_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
        );
        match result {
            KeyOutcome::Submit(text) => {
                assert_eq!(text, "idle msg");
                // 空闲时进 history,不进 queued
                assert!(queued.is_empty());
                assert_eq!(history.cells.len(), 1);
                match &history.cells[0] {
                    HistoryCell::UserMessage { text } => assert_eq!(text, "idle msg"),
                    _ => panic!("expected UserMessage"),
                }
            }
            _ => panic!("expected Submit"),
        }
    }
```

注意:这两个测试直接调 `handle_key`,但 `handle_key` 的 Submit 分支目前在 `run_loop` 里(`app.rs:209-212`),不在 `handle_key` 里。**这暴露了设计的细节**:Submit 的分流逻辑该放哪?

**设计调整**:把 Submit 分流逻辑从 `run_loop` 的 `KeyOutcome::Submit` 分支移到 `handle_key` 的 `InputAction::Submit` 分支末尾。`handle_key` 返回 `KeyOutcome::Submit(text)` 之前就决定好进 history 还是 queued。但 `handle_key` 没有 `history` 的可变引用?有 —— 签名里已经有 `history: &mut HistoryState`。`is_running`、`queued` 也都在签名里。所以分流逻辑可以放进 `handle_key`。`run_loop` 的 `KeyOutcome::Submit` 分支就只剩 `input_tx.blocking_send(text)`。

**Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib tui::app::tests::submit_while_running_goes_to_queue_not_history -- --nocapture`
Expected: FAIL,因为 Submit 分流逻辑还没移进 `handle_key`

**Step 3: 把 Submit 分流逻辑移进 `handle_key`**

(a) 在 `handle_key` 的 `InputAction::Submit` 分支(约 `app.rs:396-424`),`take_submitted()` 之后、`return KeyOutcome::Submit(text)` 之前加分流:

```rust
        InputAction::Submit => {
            let text = input.take_submitted();
            // ... 现有 slash command 处理不变 ...

            // 分流:agent 运行中进 queued,否则进 history
            if is_running.load(std::sync::atomic::Ordering::SeqCst) {
                queued.push_back(text.clone());
            } else {
                history.push(HistoryCell::UserMessage { text: text.clone() });
            }
            *popup = None;
            KeyOutcome::Submit(text)
        }
```

注意要处理 slash 命令路径 —— slash 命令走 `execute_slash_command` 不会返回 `Submit`,所以不经过分流逻辑,行为不变。

(b) `run_loop` 的 `KeyOutcome::Submit` 分支(行 209-212)简化为:

```rust
                KeyOutcome::Submit(text) => {
                    pending_quit = false;
                    let _ = input_tx.blocking_send(text);
                }
```

(去掉 `history.push(HistoryCell::UserMessage { text: text.clone() });`,因为已在 `handle_key` 里做)

**Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib tui::app::tests -- --nocapture`
Expected: 6 个测试 PASS(含新增 2 个)

**Step 5: 写转正逻辑测试**

再加测试验证转正(Done 时 queued 弹出进 history)。转正逻辑在 `run_loop` 的 `try_recv` 循环里,要测 `run_loop` 整体。用 TestBackend + 假 EventSource:

```rust
    #[test]
    fn done_event_promotes_queued_to_history() {
        use yi_agent_core::{AgentEvent, DoneReason};

        let (mut agent_tx, agent_rx) = mpsc::channel::<yi_agent_core::AgentEvent>(16);
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (decision_tx, _decision_rx) = mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(true));

        // 预先塞一条 Done 事件
        agent_tx
            .send(AgentEvent::Done {
                reason: DoneReason::EndTurn,
            })
            .await.unwrap();
        // 注意:这是同步上下文,不能 await。改用 blocking_send。

        // ... 这个测试需要异步,放 Tokio 测试里或重构 ...
    }
```

**问题**:`run_loop` 是同步函数,但 channel 操作在测试里跨异步/同步边界麻烦。**简化方案**:把转正逻辑抽成独立函数 `promote_on_turn_end(event, queued, history)`,在 `run_loop` 里调用,单独测它。

在 `app.rs` 加:

```rust
/// 回合结束时把排队第一条「转正」进 history。
fn promote_on_turn_end(event: &AgentEvent, queued: &mut VecDeque<String>, history: &mut HistoryState) {
    match event {
        AgentEvent::Done { .. } | AgentEvent::Cancelled | AgentEvent::Error(_) => {
            if let Some(text) = queued.pop_front() {
                history.push(HistoryCell::UserMessage { text });
            }
        }
        _ => {}
    }
}
```

测试:

```rust
    #[test]
    fn promote_on_done_pops_first_queued() {
        use yi_agent_core::DoneReason;
        let mut queued: VecDeque<String> = VecDeque::new();
        queued.push_back("first".to_string());
        queued.push_back("second".to_string());
        let mut history = HistoryState::new();
        let event = AgentEvent::Done { reason: DoneReason::EndTurn };
        promote_on_turn_end(&event, &mut queued, &mut history);
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0], "second");
        assert_eq!(history.cells.len(), 1);
        match &history.cells[0] {
            HistoryCell::UserMessage { text } => assert_eq!(text, "first"),
            _ => panic!("expected UserMessage"),
        }
    }

    #[test]
    fn promote_on_cancelled_pops_first_queued() {
        let mut queued: VecDeque<String> = VecDeque::new();
        queued.push_back("msg".to_string());
        let mut history = HistoryState::new();
        promote_on_turn_end(&AgentEvent::Cancelled, &mut queued, &mut history);
        assert!(queued.is_empty());
        assert_eq!(history.cells.len(), 1);
    }

    #[test]
    fn promote_on_error_pops_first_queued() {
        let mut queued: VecDeque<String> = VecDeque::new();
        queued.push_back("msg".to_string());
        let mut history = HistoryState::new();
        let event = AgentEvent::Error(anyhow::anyhow!("test"));
        promote_on_turn_end(&event, &mut queued, &mut history);
        assert!(queued.is_empty());
        assert_eq!(history.cells.len(), 1);
    }

    #[test]
    fn promote_on_assistant_text_does_nothing() {
        let mut queued: VecDeque<String> = VecDeque::new();
        queued.push_back("msg".to_string());
        let mut history = HistoryState::new();
        promote_on_turn_end(&AgentEvent::AssistantText("hi".to_string()), &mut queued, &mut history);
        assert_eq!(queued.len(), 1); // 不弹
        assert!(history.cells.is_empty()); // 不进 history
    }

    #[test]
    fn promote_with_empty_queue_does_nothing() {
        use yi_agent_core::DoneReason;
        let mut queued: VecDeque<String> = VecDeque::new();
        let mut history = HistoryState::new();
        let event = AgentEvent::Done { reason: DoneReason::EndTurn };
        promote_on_turn_end(&event, &mut queued, &mut history);
        assert!(queued.is_empty());
        assert!(history.cells.is_empty());
    }
```

**Step 6: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib tui::app::tests::promote -- --nocapture`
Expected: FAIL,`promote_on_turn_end` 还没加

**Step 7: 实现 `promote_on_turn_end`**

在 `app.rs` 加函数(见上方代码),然后在 `run_loop` 的 `try_recv` 循环里(`app.rs:155-157`)调用:

```rust
        while let Ok(event) = agent_rx.try_recv() {
            history.push_event(event, width);
            promote_on_turn_end(&event, &mut queued, history);
        }
```

**Step 8: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib tui::app::tests -- --nocapture`
Expected: 所有测试 PASS

**Step 9: 接入 queued preview 渲染区**

修改 `run_loop` 的 `terminal.draw` 闭包(行 159-194):

(a) 计算 `queued_height`:
```rust
        let width = terminal.size()?.width;
        let queued_lines = crate::tui::queued::render_queued_preview(&queued, width);
        let queued_height = queued_lines.len() as u16;
```

(放在 `terminal.draw` 之前,`let width = terminal.size()?.width;` 复用现有的 `terminal.size()?.width` 在行 154)

(b) Layout 从 4 块改 5 块:

```rust
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(3),                  // history
                    Constraint::Length(popup_height),     // popup
                    Constraint::Length(1),                // blank gap
                    Constraint::Length(queued_height),    // queued preview (新增)
                    Constraint::Length(input_height),      // input
                ])
                .split(area);
```

(c) 渲染 queued preview(在 popup 渲染之后、input 渲染之前):
```rust
            // Render queued messages preview
            if queued_height > 0 {
                let queued_area = chunks[3];
                for (row, line) in queued_lines.iter().enumerate() {
                    let y = queued_area.y + row as u16;
                    line.clone().render(
                        Rect {
                            x: queued_area.x,
                            y,
                            width: queued_area.width,
                            height: 1,
                        },
                        buf,
                    );
                }
            }
```

注意 `buf` 在 `terminal.draw(|f| { ... })` 闭包里是 `f.buffer_mut()`。需要用 `f.render_widget(Paragraph::new(queued_lines), chunks[3])` 更简洁。改用 `Paragraph`:

```rust
            if queued_height > 0 {
                let queued_widget = Paragraph::new(queued_lines.clone());
                f.render_widget(queued_widget, chunks[3]);
            }
```

(d) input 的 area 从 `chunks[3]` 改成 `chunks[4]`:
```rust
            let input_line = build_input_line(input, pending_quit, chunks[4].width);
            f.render_widget(input_line, chunks[4]);
```

**Step 10: 运行所有测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib -- --nocapture`
Expected: 所有测试 PASS

**Step 11: 手动验证(可选但推荐)**

构建二进制并在真实终端跑,观察:
- 启动 agent 后输入一条 → 应进 history(空闲)
- 在回复流式中再输入一条按 Enter → 应在输入框上方看到 `⌛ 排队中 (1)` + dim italic 消息行
- 等回复结束 → 预览区消息消失,作为正常 UserMessage 进 history
- 流式中按 Esc → 预期 agent 立即中断,显示 `Interrupted` 分隔行

Run: `cd yi-agent-rs && cargo build --bin yi-agent`
然后手动测试。

**Step 12: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/tui/app.rs
git commit -m "feat(tui): queue messages during streaming with preview area"
```

---

## Task 4: 清理 `let _ = is_running` 和无用代码

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:148`

Task 2/3 已经真正读取 `is_running`,但 `run_loop` 顶部 `let _ = is_running;` 还在。删掉它,让编译器确认参数被使用。

**Step 1: 删除 `let _ = is_running;`**

`app.rs:148` 行:
```rust
    let _ = is_running;
```
删掉。

**Step 2: 验证编译**

Run: `cd yi-agent-rs && cargo build -p yi-agent`
Expected: 编译通过(可能有 unused warning,因为参数现在被用了)

如果有 `unused variable` 警告反着来,说明签名没用上 —— 检查 Task 2/3 是否正确接入。

**Step 3: 运行所有测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib -- --nocapture`
Expected: 所有测试 PASS

**Step 4: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/tui/app.rs
git commit -m "refactor(tui): remove unused is_running suppression"
```

---

## Task 5: 最终验收 + clippy

**Files:** 无新改动,纯验证

**Step 1: cargo fmt 检查**

Run: `cd yi-agent-rs && cargo fmt --all -- --check`
Expected: 无输出(格式通过)

**Step 2: cargo clippy**

Run: `cd yi-agent-rs && cargo clippy -p yi-agent --all-targets -- -D warnings 2>&1 | tail -20`
Expected: 无 warning。如果有,按提示修。

**Step 3: 全量测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib -- --nocapture`
Expected: 所有测试 PASS

**Step 4: 手动集成测试**

构建并运行,验证三个场景:
1. 空闲提交 → 立即进 history
2. 流式中提交 → 进预览区 → Done 后转正进 history
3. 流式中按 Esc → 中断 + Cancelled → 预览区第一条转正 → driver 拾取下一条

Run: `cd yi-agent-rs && cargo build --bin yi-agent`

**Step 5: 合并准备**

不在这份计划里做合并,留给 `finishing-a-development-branch` skill。这步只确认状态:
- `git log --oneline feature/tui-queued-input ^main` 看提交历史
- `git status` 看工作区干净

---

## 不做的事(YAGNI 清单)

- 队列消息编辑/撤销:已砍,需要 id + driver 配合,过度设计
- steer(打断后合并重发):codex 有,我们没有 steer 概念,不做
- rejected steers 分类:同上
- `… 还有 N 条` 之外的更复杂截断(如逐条 `⌥+↑` 编辑):不做
- 队列容量提示(满 16 的边界处理):50ms poll 节奏下极少触发,不做

---

## 风险与缓解

| 风险 | 缓解 |
|---|---|
| `is_running` 时序与 driver 真实状态不同步 | 接受:TUI 读 atomic 是 best-effort,极端情况下空闲提交可能误进 queued,但下一轮 Done 时会转正;运行中提交可能误进 history(但 driver 仍会处理),视觉上略奇怪但功能正确 |
| `promote_on_turn_end` 与 driver 消费时序错位 | 见设计文档 Section 2,极端情况下 history 顺序略怪,不影响正确性 |
| `blocking_send` 在 channel 满(16 条)时阻塞 TUI | 50ms poll 节奏下用户极难打到 16 条排队;接受边界情况 |
| Esc 在 popup 激活时不打断 | 现有逻辑保留:popup 激活时 Esc 先关 popup,不打断。这是合理的 |
