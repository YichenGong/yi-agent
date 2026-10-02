# 按会话订阅过滤（S1）设计

**状态：** 已设计，待实现。
**前置：** Tier 1 + Tier 1.5 已合入 main（`0081130`）。
**定位：** mobile-remote-access 的 Tier 1.1 之一（spec §383「按 thread 订阅过滤」）。

## 1. 背景与问题

Tier 1 把 app-server 改成了多客户端扇出（`Broadcaster`），但**所有通知广播给所有
客户端**。手机在外网时，用户只关心自己正开着的 1-2 个会话，却会收到电脑上**全部**
会话的实时流，代价是：

- **流量/电量**：会话越多，每台设备被灌得越狠；外网尤其明显。
- **噪音/隐私**：手机不该实时看到它不关心的会话。
- **规模化**：无法随会话数增长。

这是 IM 类应用早已解决的问题。本设计采用 IM 的**简化版**：

1. **内容层按需订阅**（"打开哪个窗口收哪个"）；
2. **列表层不常开**（要看列表时主动拉，不靠实时推；后台提醒留给 Tier 1.1 的 APNs）；
3. **高频逐字流合并降频**（对应 IM 的"正在输入"合并）。

## 2. 目标与非目标

**目标**

- **G1 内容订阅（opt-in-narrowing）**：新增 `thread/subscribe { threadIds }`，**整体替换**
  该客户端的订阅集合。**一旦某客户端调用过它**，此后只收"订阅集合内 thread"的
  **内容通知**，以及**永远放行**的全局帧。**从未调用过**的客户端（= 今天的桌面）
  **行为完全不变**（全收）。集合上限 16，`[]` 表示"不收任何内容通知"。
- **G2 逐字流合并**：`item/delta` 不逐字广播，按 thread 攒批（约 100ms 或 4KB）再发，
  减少外网帧数与头开销。合并仅针对 delta，其余通知不动、顺序不变。
- **G3 列表层不常开**：客户端要看会话列表时主动 `thread/listAll`；服务端不为"列出所有
  会话"新增推送。

**非目标（后续）**

- APNs 后台推送（Tier 1.1 单独项）。
- 中继端到端加密（S3）、二维码扫描（S4）。
- 客户端侧 UI（本轮只做**服务端机制 + 协议 + 必要的共享前端接线**；手机上"打开会话即
  订阅"的界面与 S4 的配对界面一起在后续小步补，或本轮末端补一个最小接线）。

## 3. 设计

### 3.1 客户端订阅状态（G1）

`Broadcaster` 里每个客户端增加一份**订阅状态**：

```rust
enum Feed {
    /// 尚未订阅：收全部内容通知（今天的桌面行为）。
    All,
    /// 已订阅：只收这些 thread 的内容通知（可为空集）。
    Only(HashSet<String>),
}
```

- 默认 `Feed::All`。客户端调 `thread/subscribe` 后置为 `Feed::Only(ids)`（**整体替换**）。
- `broadcast` 由 `broadcast(frame)` 变为 `broadcast_for(thread_key, frame)`：

  | 帧 | `thread_key` | 语义 |
  |---|---|---|
  | 带 `thread_id` 的**内容**通知（started/delta/completed/status/tokenUsage/agent trace/process/…） | `Some(thread_id)` | 按订阅过滤；`All` 放行，`Only` 命中放行 |
  | **全局**帧：`ui/settings/updated`、`error`、`item/toolCall/approvalResolved` | `None` | **永远放行** |
  | **审批请求** `item/toolCall/requestApproval`（广播） | `Some(params.thread_id)` | 命中才推（见 §3.3） |

- **关键不变量**：`Feed::All` 且**当前没有已订阅客户端连接**时，客户端收到的字节流与今天
  **逐字节相同**。桌面走 stdio 实例（永无订阅者），故桌面**始终**不受影响；仅在同时存在
  订阅者的 ws 实例上，delta 才会对所有客户端合并（见 §3.2）。这是零回归的判据。

### 3.2 逐字流合并（G2）

`item/delta` 在 `write_notification` 之前**不进广播**，而是进入一个**按 thread 的合并
缓冲**。**只有当存在已订阅客户端（某个 `Feed::Only`）时才合并**；无一订阅者时（桌面/stdio）
**不合并、逐字节不变**（保住 §3.1 的零回归不变量）。注意：合并是实例级的——同时存在订阅者
时，delta 对**所有**客户端（含 `Feed::All`）一起合并，以保证同一实例内帧序列一致。

- 缓冲键：`thread_id`（同一 thread 的连续 delta 合并成一条）；
- 触发条件（先到者）：**每 100ms 一个 tick**，或缓冲累计 ≥ **4KB**；
- 合并规则：同一 `item_id` 的相邻 delta **拼接** `delta` 字段，保留最新 `item_id`；
  跨 `item_id` 的 delta **不合并**（顺序不能乱）；
- **屏障**：任何**非 delta** 通知（`item/started`、`item/completed`、`turn/completed`、
  `thread/status/updated`、审批请求…）到达时，**先 flush 该 thread 的 delta 缓冲**，
  保证客户端看到的顺序与今天一致（"先吐字，再标记这项完成"）。

**范围**：只合并 `item/delta`。其余通知逐条即时，顺序不变。无一订阅者时（如桌面 stdio 实例）`Feed::All` 客户端逐字节不变。

### 3.3 审批的过滤（G1 的安全子条款）

审批请求现在**广播**给所有客户端、**先到先得**。加了订阅过滤后**不是**简单按 thread 过滤就完事，否则会出现"用户被拦住了，手机却没收到弹窗"：

- **规则**：审批请求只推给"**订阅了该 thread** 的客户端" ∪ "**全收的客户端（桌面）**"。
  手机上没打开这个会话，就不该被这个会话的审批打断（它在列表里能看到红点，点进去再拉）。
- **收紧**：若某 thread 的审批**没有任何客户端命中**，且它一直没人答，则沿用既有
  `PERMISSION_TIMEOUT` → 保守 **Deny**（与今天"设备都断开"的 fail-safe 一致）。
- `approvalResolved` 仍**全局广播**（无 thread 过滤）——否则其它端弹窗关不掉。

### 3.4 协议（新增，纯增量）

- `thread/subscribe` 请求：`{ "threadIds": ["t1","t2"] }`（数组；上限 16）。
  响应：`{ "subscribed": ["t1","t2"] }`。
- 超上限 → `invalid_params`（-32602，提示上限）。
- 该方法**不在 admin 门禁内**（任何已初始化客户端可订阅自己的 feed）。
- 不新增通知类型。

## 4. 测试策略

**Tier 0（单元）**
- `Feed` 过滤：`All` 恒真；`Only` 命中/未命中；`None` 键（全局）恒真。
- 合并器：同 `item_id` 相邻 delta 拼接；跨 `item_id` 不合并；屏障通知触发 flush 且顺序
  正确；字节上限触发 flush；tick 触发 flush。

**Tier 0（集成，mock provider）**
- 双客户端：A `subscribe(["t1"])`、B 不订阅；驱动 t1、t2 两个 thread 产生通知 →
  **A 只收 t1、B 收全部**；A 收不到 t2 的任何内容通知。
- 全局帧（`ui/settings/updated`、`approvalResolved`）在 A 订阅后仍收到。
- 审批：t1 的审批请求 → 订阅 t1 的 A 收到、未订阅 t2 的 A 收不到；桌面 B 收到。
- **零回归**：既有 272 个 app-server 测试全绿（`Feed::All` 逐字节不变）。

**前端**
- 桌面端 439 个测试全绿（桌面不订阅，行为不变）。
- 若本轮补接线：`wsTransport` 支持发送 `thread/subscribe`。

**验证命令**
```
cargo test -p yi-agent-app-server
cd desktop && npx vitest run && npx tsc --noEmit
```

## 5. 兼容与迁移

- **纯增量**：不调 `thread/subscribe` 的客户端行为不变（桌面零改动）。
- `PROTOCOL_VERSION` 与 capabilities 暴露新方法（沿用既有约定）。
- 合并对客户端语义透明：它仍是"收到 `item/delta`"，内容与顺序不变，只是帧更粗。注意
  实例级耦合：一旦实例内存在订阅者，`Feed::All` 客户端的 delta 也会一起变粗（见 §3.2）。

## 6. 已知限制（如实记录）

- 合并引入**最多 100ms** 的显示延迟，且**只对已订阅的客户端**生效（桌面 stdio 实例
  永无订阅者，故逐字节不变、不受影响；同一 ws 实例内若有订阅者，则所有客户端一起变粗）。
- 订阅是**每连接**的：重连后客户端需**重新订阅**（与 `initialize` 后状态重建一致）。
- 列表层不推送：客户端需主动 `thread/listAll`（本设计有意为之）。
