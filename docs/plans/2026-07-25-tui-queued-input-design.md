# TUI 流式中输入的可视化与打断设计

**日期**: 2026-07-25
**状态**: 已确认,待实现
**范围**: `yi-agent-rs/crates/yi-agent/src/tui/`(主要 `app.rs`)

---

## 背景

当前 Ratatui TUI 在模型流式返回过程中,用户输入的处理存在两个问题:

1. **排队不可见**:用户按 Enter 提交的消息通过 `input_tx.blocking_send` 进入 channel buffer(`app.rs:212`),等当前流结束后才被 driver 消费。但这期间消息已经作为正常 `UserMessage` 推到 history(`app.rs:211`),与「正在被处理」的消息视觉上无法区分。用户不知道自己这条是「正在跑」还是「排队等着」。

2. **无法打断**:`interrupt_tx` channel 已接线到 driver 的 `tokio::select!`(`main.rs:237-245`),`agent.cancel()` 机制也完整,但 TUI 的 `handle_key` 从不往 `interrupt_tx` 发信号。Ctrl+C 只做 `pending_quit`(双击退出,`app.rs:291-297`),Esc 同理(`app.rs:279-290`)。流式中想中途停止只能整个退出程序。

参考 codex(`codex-rs/tui/src/bottom_pane/pending_input_preview.rs`)的 `PendingInputPreview` 设计,引入「排队预览区」+ 打断接线。

---

## 设计决策

| 决策点 | 选择 | 理由 |
|---|---|---|
| 可视化位置 | 输入框上方独立预览区 | 与「即将被处理的动作」语义匹配,codex 同位 |
| 转化时机 | 被 driver 消费时才进 history | 保证「history = 已处理/正在处理」语义干净 |
| 打断键位 | Esc / Ctrl+C 单击都打断,双击任一键退出 | 与 codex 一致,两键等效 |
| 队列操作 | 只读,不支持编辑/撤销 | YAGNI,撤销需要 id + driver 配合,当前不引入 |
| 队列容量 | 跟随 `input_tx` 的 16(`main.rs:194`) | 不引入新限制 |

---

## 架构总览

在 `run_loop` 单线程 TUI 侧新增 `queued: VecDeque<String>` 状态,与 `input_tx` channel buffer 一一对应(消息同时进两处)。

**提交分两条路**:
- Agent 空闲(`is_running == false`)→ 立刻 push `UserMessage` 到 history(现有行为),发 `input_tx`
- Agent 运行中(`is_running == true`)→ 推到 `queued` 预览,发 `input_tx`,**不** push history

**转化时机**:TUI 收到 `AgentEvent::Done` / `Cancelled` / `Error` 时,从 `queued.front()` 弹出一条进 history。driver 侧同时从 `input_rx.recv().await` 拿到同一条消息开始新轮。

**打断**:Esc / Ctrl+C 单击 → `interrupt_tx.send(())`(agent 运行时)+ 设 `pending_quit`;双击 → 退出程序。driver 已有 `interrupt_rx` 分支处理 cancel(`main.rs:237-245`),无需改动。

---

## 数据流

### 判断 agent 是否在运行

复用 `main.rs:196` 的 `is_running: Arc<AtomicBool>`。当前 `tui/app.rs:148` 用 `let _ = is_running;` 抑制未使用警告,本设计要真正读取:
- driver 在 `main.rs:220` 设 true,`main.rs:253` 设 false
- TUI 提交时读 `is_running.load(SeqCst)` 决定走哪条路

### 提交路径(替换 `app.rs:209-212`)

```rust
KeyOutcome::Submit(text) => {
    pending_quit = false;
    if is_running.load(SeqCst) {
        queued.push_back(text.clone());
    } else {
        history.push(HistoryCell::UserMessage { text: text.clone() });
    }
    let _ = input_tx.blocking_send(text);
}
```

### 转化时机(在 `app.rs:155-157` 的 `try_recv` 循环里)

```rust
while let Ok(event) = agent_rx.try_recv() {
    history.push_event(event, width);
    // 回合结束:把排队第一条「转正」进 history
    match &event {
        AgentEvent::Done { .. } | AgentEvent::Cancelled | AgentEvent::Error(_) => {
            if let Some(text) = queued.pop_front() {
                history.push(HistoryCell::UserMessage { text });
            }
        }
        _ => {}
    }
}
```

### 时序说明

driver 从 `input_rx.recv().await` 拿到消息(`main.rs:212`)≈ TUI 这边处理 Done 事件,两者时序接近。极端情况下:
- TUI 稍早(pop 了一条还没被 driver 拿走):预览区先空,history 稍后补
- TUI 稍晚(driver 已在跑新轮但 history 里还没显示):新轮的 AssistantText 先到,UserMessage 后到

两者都不影响正确性。

---

## 布局与渲染

### Layout 改动(`app.rs:159-194`)

现有 4 块:history / popup / blank gap / input。改成 5 块,在 blank gap 和 input 之间插入 queued preview:

```rust
let queued_height = queued.len().min(3) as u16;

let chunks = Layout::default()
    .direction(Direction::Vertical)
    .constraints([
        Constraint::Min(3),                  // history
        Constraint::Length(popup_height),     // popup
        Constraint::Length(1),                // blank gap
        Constraint::Length(queued_height),    // queued preview (新增)
        Constraint::Length(input_height),     // input
    ])
    .split(area);
```

### 渲染样式

```
  ⌛ 排队中 (2)                       ← dim, 标题行,有消息时显示
    ↳ 第一条消息内容…                 ← dim + italic
    ↳ 第二条消息内容…                 ← dim + italic
```

- 标题行:`⌛ 排队中 (N)`,dim 样式,N = 消息总数(不是显示数)
- 每条消息:`  ↳ ` 前缀,dim + italic
- 最多显示 3 行,超出用「… 还有 N 条」计数行
- 无消息时 `queued_height = 0`,不占空间

### 位置选择理由

放在 input 上方(而非 history 下方):输入框是用户的「动作区」,排队消息是「即将被处理的动作」,视觉上靠近输入框更符合「这些是你要发的消息」的直觉。codex 也是这个位置。

---

## 打断逻辑

### 键位语义(Esc / Ctrl+C 合并改造)

两个键统一语义:**单击打断,双击退出**。复用现有 `pending_quit` 机制(`app.rs:149`),但语义调整:

```rust
// app.rs handle_key 里 Esc 和 Ctrl+C 分支合并改造
KeyCode::Esc | KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
    if *pending_quit {
        return KeyOutcome::Quit;  // 第二次:退出
    }
    *pending_quit = true;
    // 如果 agent 在跑,发 interrupt
    if is_running.load(SeqCst) {
        let _ = interrupt_tx.blocking_send(());
    }
    return KeyOutcome::None;
}
```

**关键变化**:
- 之前 Esc 单独管 pending_quit,Ctrl+C 也管 pending_quit,但两者**都不发 interrupt**
- 现在两者**都发 interrupt**(agent 运行时),driver 走已有的 cancel 路径(`main.rs:237-245`)

### driver 侧已有逻辑无需改动

- `main.rs:237-245` 的 `interrupt_rx.recv()` 分支已经调用 `agent.cancel()` 并 drain stream
- `main.rs:216-217` 在每轮开始前 `interrupt_rx.try_recv()` 清掉陈旧信号
- cancel 后 driver 跳回 `input_rx.recv().await`(`main.rs:212`),队列里有消息就自动拾取

### 打断后的 history 显示

`agent.cancel()` 让 stream 产出 `AgentEvent::Cancelled`,走转化逻辑:弹出排队第一条进 history,然后 driver 拾取下一条开始新一轮。时序:Cancelled → 第一条排队消息进 history → 新轮 AssistantText 开始。

### 打断与队列的交互

**路径 A:打断后队列里还有消息(常见)**

1. Esc 按下 → `interrupt_tx.send(())`
2. driver `main.rs:237` 收到 → `agent.cancel()` → drain stream(产出 `Cancelled` 事件)
3. TUI 收到 `Cancelled` → 转化逻辑弹出 `queued[0]` 进 history
4. driver 跳回 `input_rx.recv().await` → 拿到 `input_tx` 里下一条 → 新一轮开始
5. 剩余 `queued[1..]` 继续在预览区,等下一轮 `Done` 时逐条弹出

**路径 B:用户在 Esc 之后、agent 真正 Cancelled 之前又按 Esc**

`pending_quit` 已经是 true,第二次 Esc 走 `KeyOutcome::Quit` 直接退出程序。与现有语义一致。

---

## 改造清单

### 1. `tui/app.rs` — `run_loop`

- `let _ = is_running;` 改成真正读取(`app.rs:148`)
- 新增 `let mut queued: VecDeque<String> = VecDeque::new();`
- 提交分支(`app.rs:209-212`)按 `is_running` 分两条路
- `try_recv` 循环里(`app.rs:155-157`)处理完 `push_event` 后,检测回合结束事件弹出 `queued.front()` 进 history
- layout 从 4 块改 5 块,新增 queued preview 区
- 新增渲染 queued preview 的代码

### 2. `tui/app.rs` — `handle_key`

- Esc 和 Ctrl+C 分支合并改造(`app.rs:279-297`):单击发 `interrupt_tx.blocking_send(())`(agent 运行时)+ 设 `pending_quit`;双击退出
- 去掉撤销相关(已砍)

### 3. `handle_key` 签名

需要传 `is_running`、`queued`、`interrupt_tx` 进去。`interrupt_tx` 已在参数里(`app.rs:203`)。`is_running` 要加。`queued` 要加 `&mut`。

### 4. `tui/app.rs` — 测试辅助函数

`run_tui_with_backend` 和 `run_tui_with_backend_and_events`(`app.rs:87-134`)已经传了 `is_running`,只是没用,去掉 `let _ =`。

### 5. 新增渲染函数

`render_queued_preview(queued: &VecDeque<String>, width: u16) -> Vec<Line<'static>>` —— 放 `app.rs` 或新文件 `tui/queued.rs`。

### 6. 不动的地方

- `main.rs` driver 逻辑(`interrupt_rx` select、`agent.cancel()`、drain stream)—— 已有,直接复用
- `agent.rs` cancel 机制 —— 已有
- `cell.rs` `HistoryCell` —— 不新增 variant(`UserMessage` 复用)
- `history.rs` `push_event` —— 不改,转化逻辑在 `run_loop` 里做

### 7. 测试

- 单元测试:`run_loop` 注入 fake 事件序列(Start → AssistantText → Done),中途提交一条,验证 queued 出现 + Done 时转化进 history
- `handle_key` 测试:agent 运行时 Esc 发 interrupt_tx;空闲时 Esc 不发
- 渲染测试:queued 为空 / 1 条 / 3 条 / 5 条的渲染输出
