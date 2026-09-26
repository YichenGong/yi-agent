# TUI 待发请求队列重构设计

**目标:** 修复 TUI 排队输入的两个缺陷——队列满时冻结整个 UI（P0），以及
队列与通道双 FIFO 结构导致的转正/发送时钟错位（P1）——并把队列行为收敛为
一个有名字、可单测的类型，让"排队 user request 如何进入对话"这件事**逻辑清晰**。

**状态:** 设计已确认，待转实现计划。

**修订关系:** 本设计修订 `../plans/2026-07-25-tui-queued-input-design.md` 中的两条决策：

1. 「队列容量跟随 `input_tx` 的 16」——原设计只对齐了容量数字，未考虑
   `blocking_send` 在通道满时**阻塞调用线程**的语义。本设计改为队列上限 16
   且**同时至多 1 条在途**，使通道永不积压。
2. 「`queued` 与 `input_tx` channel buffer 一一对应（消息同时进两处）」——
   双 FIFO 结构是 P1 的根因，本设计取消它：只保留一个队列，通道退化为
   单条传递通道。

---

## 1. 问题

### 1.1 P0：队列满时冻结整个 UI

TUI 提交消息时调用 `app.rs:1094`：

```rust
let _ = input_tx.blocking_send(text.clone());
```

通道为 `main.rs:1166` 的 `mpsc::channel::<String>(16)`，容量 16。driver 仅在
**轮次之间** poll 该通道（`main.rs:1212` 的 `select!` 分支），`agent.run(text).await`
（`main.rs:1408`）执行期间不消费任何输入。

因此第 17 条排队消息会命中 `tokio::sync::mpsc` 的 `blocking_send` 阻塞语义，
而 TUI 跑在 `main.rs:1454` 的 `spawn_blocking` 线程上——该线程被卡死后
**UI 完全无响应**，直到本轮结束。

注意这不是旧 bug 的重复：`app.rs:3874` 的 `blocking_send_does_not_panic_on_runtime_thread`
修的是"在 runtime 线程上调用 `blocking_send` 会 panic"，修法是改用
`spawn_blocking`。那修的是**在错误的线程上**阻塞；本设计修的是**通道满时**阻塞。
正是前一个修复让该问题从 panic 变成了静默冻结。

### 1.2 P1：双 FIFO 的时钟错位

消息同时进入两个独立队列：`queued: VecDeque<String>`（用于预览）与通道
buffer（用于投递）。二者仅靠"都按同序 push"维持一致，但弹出侧的时钟不同：

- `queued` 由 TUI 在观察到终止事件时弹出（`app.rs:303`）
- 通道由 driver 调用 `input_rx.recv()` 时弹出（`main.rs:1212`）

由此派生四个问题：

| 问题 | 位置 | 说明 |
|---|---|---|
| 双队列靠约定同步 | `app.rs:1087` + `:1094` | 无强制不变量，靠同序 push |
| 转正与真开跑无绑定 | `app.rs:290-306` | TUI 见 `Done` 即标为"正在处理"，driver 是否取走无关联 |
| 1:1 不变量未强制 | `app.rs:265-274` | 用 `.min(queued.len())` 兜底，属隐式约定 |
| Esc 语义反直觉 | `app.rs:898-911` | 打断 → 弹一条 → driver 立刻起新一轮，等于"跳下一条" |

### 1.3 承重发现：`is_running` 不能用于"是否该发下一条"

driver 在 `main.rs:1440` 清除 `is_running`，该语句位于**排空循环之后**；
而 `AgentEvent::Done` 是在循环**内部**发出的。所以 TUI 观察到 `Done` 的那一刻，
`is_running` **仍为 `true`**。

若用 `is_running` 决定"现在该不该发下一条"，TUI 会认为仍有在途轮次而拒绝发送，
driver 则已回到 `recv()` 等待输入——**双方互等，死锁**。

故必须引入 TUI 本地的 `in_flight` 标志，由 TUI 自行维护。`is_running` 保留原有
职责：仅用于 Esc 打断判断（避免对未运行的 agent 发打断信号）。

---

## 2. 架构

在 `tui/queued.rs` 新增 `PendingQueue`，取代 `run_loop` 中裸的 `VecDeque<String>`，
把队列不变量收进类型内部：

```rust
pub struct PendingQueue {
    items: VecDeque<String>,   // 等待发送的消息(仅用于预览)
    in_flight: bool,           // 是否有一轮已发出但尚未结束
}

pub enum SubmitOutcome {
    Sent,        // 空闲:已立即发送
    Queued,      // 忙:已入队
    Rejected,    // 忙且已满:未入队、未发送
}

impl PendingQueue {
    pub const CAPACITY: usize = 16;

    /// 空闲则立即发送,忙则入队,满则拒收。
    pub fn submit(&mut self, text: String) -> SubmitOutcome;

    /// 回合结束:弹出下一条待发消息;队列空则复位 in_flight。
    pub fn on_turn_end(&mut self) -> Option<String>;

    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    /// 供预览渲染,不暴露内部结构。
    pub fn iter(&self) -> impl Iterator<Item = &String>;
}
```

**设计理由:** 直接回应 bug 记录的"逻辑不是很清晰"。队列行为变成三个有名字、
有明确返回值的操作，而非散在 `run_loop` 里的 `push_back`/`pop_front`/`.min()` 拼凑。
`SubmitOutcome` 让"拒收"成为显式分支，调用方必须处理。

`render_queued_preview` 改为接收 `&PendingQueue`（签名调整），渲染逻辑与现有
5 个测试保持不变。

---

## 3. 数据流

### 3.1 提交（`app.rs:1085-1095`，全仓唯一发送点）

| 状态 | 行为 |
|---|---|
| `in_flight == false` | `submit` 返回 `Sent`：立即发送、`in_flight = true`、进 history |
| `in_flight == true` 且未满 | `Queued`：入队、**仅**进预览、**不发通道** |
| `in_flight == true` 且已满 | `Rejected`：**文本退回输入框** + history 加 `Separator` 提示「排队已满 (16)，请稍后再发」 |

**拒收必须退回文本**：`InputLine::take_submitted`（`input.rs:182-191`）用
`std::mem::take(&mut self.buffer)` 清空输入框，文本已被取走。若不退回即为静默数据
丢失。退回后用户看到字仍在框内，自然理解未发出。

`Separator` 复用现有提示样式（`app.rs:1076-1081` 的「未知命令」同款），不新增
`HistoryCell` 变体。

### 3.2 回合结束（TUI 观察到 `Done`/`Cancelled`/`Error`，`app.rs:290-306`）

| 状态 | 行为 |
|---|---|
| 队列非空 | `on_turn_end` 弹出 → **立即发送** → `in_flight = true` → 进 history |
| 队列空 | `in_flight = false` |

**关键变化：转正与发送合并为同一个动作。** 不再依赖"driver 是否取走"的推测，
消除了 P1 的时钟错位。

### 3.3 发送方式

发送改用 `try_send`。由于同时至多 1 条在途，通道永不满，`try_send` 正常路径必然成功；
即便意外返回 `Full`/`Closed`，也走拒收路径（退回输入框 + 提示）而非阻塞线程。
这是 P0 的构造性消除，不是打补丁。

---

## 4. 缺陷如何被消解

| 缺陷 | 消解方式 |
|---|---|
| P0 队满冻结 | 同时至多 1 条在途 → 通道永不积压 → `try_send` 不可能阻塞 |
| P1 双队列同步 | 只剩一个队列，通道退化为单条传递通道 |
| P1 转正/开跑无绑定 | 转正与发送是同一个动作 |
| P1 1:1 不变量靠约定 | 不变量成为 `PendingQueue` 的内部性质 |
| P1 Esc 语义反直觉 | 行为保持，但变为**显式且可见**：转正即发送，预览区本就展示待发内容 |

### 4.1 附带修正

- **未知 slash 命令绕过队列**（`app.rs:1073-1083`）：该分支在队列逻辑之前
  `return`，与其他输入路径不一致。本设计不改其行为（它本就不该进队列，因为
  它不是发给 agent 的 prompt），但会补一条注释说明为何不经过 `PendingQueue`。
- **退出丢消息**：队列非空时双击 Ctrl+C 退出会静默丢弃待发消息。本设计**不在
  范围内**（见 Non-goals），仅在退出前打印一条提示行说明丢弃条数。

---

## 5. 影响面

| 文件 | 改动 |
|---|---|
| `tui/queued.rs` | 新增 `PendingQueue`、`SubmitOutcome`；`render_queued_preview` 签名调整；新增单测 |
| `tui/app.rs` | `handle_key` 与 `run_loop` 的队列段；`queued: &mut VecDeque<String>` → `&mut PendingQueue` |
| `main.rs` | 通道容量 16 保留（功能上容量 1 已充分，保留 16 作安全余量，避免不必要耦合） |

**调用点改动：** `handle_key` 共 15 个真实调用点（生产 1 处 `app.rs:514` +
测试 14 处），其中 14 处传 `&mut queued`，需机械替换类型。

---

## 6. 测试计划

先写测试再实现（TDD）。`PendingQueue` 单元测试：

1. 空闲 `submit` → 返回 `Sent`，`in_flight` 为真，队列空
2. 忙时 `submit` → 返回 `Queued`，入队，`in_flight` 保持真
3. 忙且满（16 条）→ 返回 `Rejected`，队列仍 16 条
4. `on_turn_end` 队列非空 → 弹出首条，`in_flight` 保持真
5. `on_turn_end` 队列空 → 返回 `None`，`in_flight` 复位为假
6. 连续 `submit` 3 条后 `on_turn_end` → 只弹出 1 条，剩 2 条仍在队列

`handle_key` 集成测试（沿用现有 `submit_while_*` 命名与构造方式）：

7. 忙时提交 → 入队且通道**无**消息
8. 满时提交 → 文本回到输入框、history 出现提示、通道无消息
9. 回归：`in_flight` 语义——模拟"看到 `Done` 时 `is_running` 仍为 true"，
   验证仍能正确发送下一条（此测试直接覆盖 §1.3 的死锁场景）

现有测试更新：`submit_while_running_goes_to_queue_not_history`、
`submit_while_idle_goes_to_history_not_queue` 语义不变，仅适配构造方式。

---

## 7. Non-goals

- **不改 app-server**：desktop 侧「忙时拒收 `-32012`」是刻意的不同语义
  （`server.rs:570-573`），本设计不统一两侧行为。
- **不改 agent 内核**：core 无队列，一轮一跑，此设计不动。
- **不支持队列编辑/撤销**：延续原设计的 YAGNI 决策。
- **不处理退出丢消息**：仅加提示行，不做落盘或恢复。
- **不改 headless**：单 prompt 模式无排队需求。
