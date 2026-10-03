# 会话回放不丢帧 + 超长会话冷开优化（resume replay）

日期：2026-10-03
状态：已实现

## 问题（已实测复现）

用户在 Desktop 上重启 App 后，**长会话的最近一段内容不显示**；但继续对话时大模型
「记得」那段历史。即：**数据没丢，回放丢了**。

根因已用真实二进制（`/Applications/yi-agent.app/Contents/MacOS/yi-agent`）实测确认：
起 app-server、走 stdio、发 `thread/resume`，统计客户端实际收到的 `item/completed`
条数，与该会话 `.jsonl` 里的 item 总数对比：

| 会话 | `.jsonl` item 数 | resume 回放数 | 丢失 |
|---|---|---|---|
| thread-1e0ef9db | 41 | 41 | 0 |
| thread-dbc93336 | 112 | 112 | 0 |
| thread-2daebf29 | 172 | 172 | 0 |
| thread-36ebf486 | 539 | 278 | 261 |
| thread-a414ddb8 | 1463 | 320 | 1143 |

规律：**item 数 ≤ 256 的会话回放 100% 完整；超过 256 就开始丢，丢的全是尾部
（最近的内容）。** 磁盘上的 `.jsonl` 与 `messages` 始终完整——这正是「大模型知道、
Mac App 不显示」的来源。

## 根因

`thread/resume` 的历史回放是**逐条**调用 `write_notification`
（`server.rs:2776` 的 `for item in loaded.items`）。每次调用最终落到
`Broadcaster::broadcast_for`：

```rust
// broadcast.rs 的 broadcast_for
if client.reliable {
    let _ = client.tx.try_send(frame.clone());   // 静默忽略失败
    true
} else {
    client.tx.try_send(frame.clone()).is_ok()
}
```

- 出站队列深度固定 `CLIENT_QUEUE = 256`（`broadcast.rs:14`）。
- 用的是 `try_send`（非阻塞），**返回值被丢弃**：队列满时该帧**被静默丢弃**。
  代码注释已写明该设计：「可靠客户端队列写满时保留登记、丢弃当前帧」。
- `pump_stdout` 是**独立任务**，从队列取帧写 stdout；主循环灌帧的速度在回放期
  远快于 pump 写完，队列随即写满，溢出帧被丢。

因此回放突然超过 256 帧时，尾部内容被静默截断。**这是既有缺陷**，早于
「进行中 turn 的 checkpoint 落盘」那次改动；checkpoint 恢复出的内容同样经此
回放路径，故一并受害。

附带成本：回放是**同步**循环，写 N 帧期间阻塞请求循环——长会话 resume 时整个
app-server 会卡顿。

## 方案总览

只改**回放路径**，实时路径逐字节不变。

1. 服务端新增一个**回放专用批量通知** `items/completed`，params 携带
   `{ thread_id, items: Item[] }`。
2. `thread/resume` 的回放循环把 `loaded.items` 按**字节预算 + 条数上限**切成若干块，
   每块打成一条 `items/completed` 下发；块间用**带背压的 await 入队**，队列满即等
   pump 消费，**绝不丢帧**。
3. 客户端 `session.apply` 增加 `items/completed` 分支 → 直接调用**已有的**
   `upsertItems(items)`（按 id 去重、就地替换，天然幂等）。
4. 实时流（`item/started` / `item/completed` / `item/delta`）与 `thread/readItems`
   完全不动。

不变量：
- **回放期绝不丢帧**（本地 reliable 客户端）。
- **实时期行为逐字节不变**（仍是逐条 `try_send`）。
- 单帧**远小于** `MAX_FRAME_BYTES = 1 MiB`（读侧硬上限，见
  `ws.rs:461` / `transport.rs:41`）。

## 协议

新增 `Notification` 变体：

```rust
/// 回放期批量下发历史 item：一次一帧，替代逐条 `item/completed`。
#[serde(rename = "items/completed")]
ItemsCompleted { thread_id: String, items: Vec<Item> },
```

- **仅在回放期发送**；实时流不带此方法。
- `delivery()` 归 `Content`（与 `item/completed` 同族，按 `thread_id` 过滤），
  故 `thread_key()` 返回该 thread。
- `Item` 结构、`.jsonl` / `.meta.json` 格式均不变。
- 老客户端兼容：现有 `session.apply` 的 `default` 分支忽略未知 method，故不会
  报错（降级为「回放内容不显示」= 今天的行为，不更差）。

## 服务端

### 分块器

遍历 `loaded.items`，累计序列化后近似字节数 `bytes += serde_json::to_vec(item).len()`：

- 软预算 `CHUNK_BYTES = 256 * 1024`（256 KiB）。
- 条数上限 `CHUNK_MAX_ITEMS = 200`。
- **谁先到就 flush 当前块**（`bytes > CHUNK_BYTES` 或 `count >= CHUNK_MAX_ITEMS`），
  再开新块。因此单帧满足 `预算 + 最后一条 item 的大小`，正常远小于 1 MiB。
- 单条 item 自身就超预算的（超大工具结果）**单独成帧**尽力发送——与现状同构，
  不在本次范围内再切分（若单条超 1 MiB，读侧仍会拒绝，属既有边界）。
- 顺序严格保持：块内顺序 + 块间顺序 = jsonl 顺序。

分块器写成纯函数（输入 items，输出 `Vec<Vec<Item>>`），便于单测。

### 带背压的扇出

新增 `Broadcaster::broadcast_await(key, frame)`，语义与 `broadcast_for` 相同，区别：

- **reliable 客户端**（stdio 的 `local`）：`tx.send(frame).await`（背压），队列满即等，
  **不丢帧**。
- **非 reliable 客户端**（ws）：维持 `try_send`，满则丢帧/摘除——保持 ws 既有契约
  不变（ws 已有 `thread/readItems` 兜底）。

实现注意：**先克隆出目标 tx、释放锁、再 await**，避免跨 `.await` 持锁。
`send` 返回 `Err`（接收端已丢弃 = 客户端真死）时，回放**记 stderr 并中止本轮回放**，
不向已死的队列继续灌帧；**不让 resume 报错**，已发出的帧不受影响。

### 回放循环

把现有 `for item in loaded.items { write_notification(ItemCompleted) }` 替换为：

1. 用分块器切块；
2. 逐块构造 `items/completed` 帧，`broadcast_await`（顺序 await，保证不丢）；
3. 其余帧（`thread/started`、usage、`turn/completed{Interrupted}`、响应）顺序**不变**。

`loaded.items` 为空时不产生任何 `items/completed` 帧，行为与今天一致。

## 客户端

- `desktop/src/lib/protocol.ts`：`Notification` 增加 `items/completed` 形状。
- `desktop/src/lib/session.ts`：`apply()` 增加分支：
  ```ts
  case "items/completed":
    this.upsertItems(notification.params.items);
    break;
  ```
  复用既有 `upsertItems`（按 id 就地替换 / 去重 / 保持顺序 / 推进
  `lastServerItemId`），因此与实时帧混用也幂等、不重复。

## 边界与错误处理

- **背压下客户端真死**：`pump_stdout` 写失败时既有的 `hub.unregister` 摘除该客户端，
  回放的 `send` 随即 `Err` → 中止本轮回放、记 stderr、resume 仍成功返回。
- **空历史 / 单条**：正常降级为 0 或 1 帧，与现状一致。
- **单条超 1 MiB**：读侧拒绝（既有边界），本次不处理。
- **中途断开后重连**：客户端重新 `thread/resume` 即重新回放，因为磁盘数据完整。

## 测试（TDD）

服务端单元：

- 分块器：给定 items，按 256 KiB / 200 条切分；**顺序保持**；每帧序列化后 < 1 MiB。
- `broadcast_await`：对 reliable 客户端在**队列满**时仍**不丢帧**（灌 > CLIENT_QUEUE
  帧后全部收到）；对 ws（非 reliable）保持 `try_send` 语义（满则丢/摘除，既有测试）。

app-server 集成（沿用 `Harness`）：

- **> 256 items 的 thread**：`thread/resume` 后**断言客户端收到的 item 集合 ==
  `.jsonl` 的 item 集合**（无缺失、顺序一致）。**此测试在旧实现下必失败**——正是
  实测的 bug 复现。
- 回归：现有 `thread_resume_replays_history_and_restores_context` 等（< 256 items）
  仍全绿；崩溃 partial 的 resume 测试（回放 + interrupted）仍绿。

desktop 单元：

- `session.apply("items/completed")` 把一批 items 并入 `items`，顺序与逐条
  `item/completed` 等价；与本地乐观回声、与已存在 item 均去重。

## 不做（YAGNI）

- 不改实时流（`item/started` / `item/completed` / `item/delta` 保持逐条）。
- 不改 `thread/readItems`（它本就一帧返回整段，无此缺陷）。
- 不给非 reliable（ws）客户端做背压（保持其「满则摘除」契约）。
- 不重构 `broadcast_for` / `reply` 的既有语义。
- 不处理单条 item 超过 `MAX_FRAME_BYTES` 的极端（既有边界）。
- 不改 `.jsonl` / `.meta.json` 格式。
