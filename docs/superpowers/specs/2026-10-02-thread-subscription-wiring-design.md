# 按会话订阅接线（S2：IM 式列表实时 + 内容按需）设计

**状态：** 已设计，待实现。
**前置：** 按会话订阅过滤（S1）已合入 `main`（`13347b4`）。
**定位：** mobile-remote-access 的 Tier 1.1 之一「按 thread 订阅过滤」的**客户端接线 + 列表层细化**。
S1 交付了服务端机制（`thread/subscribe`、按 thread 路由、delta 合并），但**手机端至今不订阅**，
也**收不到列表层的实时状态**。本设计把 S1 补成"手机上真的省流量、列表又准"的 IM 行为。

## 1. 背景与问题

S1 落地后有两个缺口：

1. **手机不订阅**：前端从不调用 `thread/subscribe`，于是手机仍被灌电脑上**全部**会话的
   实时内容流——正是 S1 要解决的问题没被真正用上。
2. **列表层不实时**：S1 的 `thread_key` 把 `thread/status/updated` 等也一并按订阅过滤了。
   若手机只订阅当前会话，**后台会话的"正在运行/待审批"在侧边栏会滞后**到下次主动拉取。

IM 类应用的做法是**分层**：内容层按需订阅（打开哪个窗口收哪个），列表层用**轻量摘要**持续
刷新（谁在跑、谁有红点、时间戳），后台层用推送（APNs，属 Tier 1.1 另一项，不在本设计）。
本设计补上**内容层接线**与**列表层细化**两层。

## 2. 目标与非目标

**目标**

- **G1 列表层实时（服务端）**：`thread/started`、`thread/status/updated` 归为**列表层**，
  对**所有客户端恒推**（无论是否订阅）。手机侧边栏实时准确。
- **G2 内容层按需（服务端语义不变 + 前端接线）**：其余内容通知只在订阅集合内推（既有
  `Feed` 规则不变）。前端（**仅远程客户端**）维护一个 warm 窗口并调 `thread/subscribe`。
- **G3 warm 窗口**：订阅集合 = 当前会话 + 最近用过的至多 `K=8` 个会话（LRU）；`K ≤ 16` 是
  编译期常量约束，天然落在 S1 的上限内。切回窗口内会话**即时无空档**（通知一直在累积）。
- **G4 只读补齐**：新增只读 RPC `thread/readItems`，供"窗口外的会话"返回时补上错过的内容，
  **不 resume、不打断任何正在跑的回合**。
- **G5 零回归**：桌面（Tauri/stdio）**一行不改**，永不订阅，`Feed::All` 行为与今天逐字节相同。

**非目标（后续）**

- APNs 后台推送；中继端到端加密（S3）；二维码扫描（S4）；Android。
- 列表层不做"未读数/最后活动"摘要流——侧边栏仍靠既有 `thread/listAll` 主动刷新。

## 3. 设计

### 3.1 服务端：把投递规则分成三层（G1）

`Notification` 增加一个分类（与 S1 的 `thread_key` 并列），`broadcast` 据此选择投递规则：

```rust
/// 该通知的投递层级。
pub(crate) enum Delivery {
    /// 全局帧：主题/错误/审批已处理。恒推。
    Global,
    /// 列表层：会话存在与状态。**恒推**（列表要实时，与订阅无关）。
    List,
    /// 内容层：会话的正文流。按 `Feed` 过滤（`All` 全收，`Only` 命中才推）。
    /// 过滤键仍取自 `thread_key()`。
    Content,
}
```

| 层 | 帧 | 投递规则 |
|---|---|---|
| Global | `ui/settings/updated`、`error`、`item/toolCall/approvalResolved` | 恒推（与 S1 相同） |
| **List（新）** | `thread/started`、`thread/status/updated` | **恒推**（忽略 `Feed::Only`） |
| Content | `item/started`、`item/delta`、`item/completed`、`turn/started`、`turn/completed`、`turn/retry`、`interjectionsReturned`、`tokenUsage`、`agent/trace`、`agent/children`、`process/updated` | `Feed::All` 全收；`Feed::Only` 只推集合内 thread |

- `Broadcaster` 的过滤判据由"`feed.accepts(key)`"细化为"**List 层恒真** ∪ `feed.accepts(key)`"，
  即 `Feed::accepts` 对 List 层直接放行（最简：在 `Notifications` 分类结果里，List 层走
  `broadcast_for(None, frame)`，Global 层亦然——它们本就是"无 thread 键即恒放行"）。
- **审批请求**沿用 S1 规则（订阅了该 thread ∪ 全收客户端）。
- 桌面零影响：桌面从不订阅 → `Feed::All` → 全部照收（List/Content 都收）。

> **实现备注（供实现计划采用）**：最小改法是给 `Notification` 加
> `fn delivery(&self) -> Delivery`，`write_notification` 对 `Delivery::List | Global` 用
> `hub.broadcast_for(None, frame)`，对 `Delivery::Content` 用 `hub.broadcast_for(thread_key, frame)`。
> 这样**不新增** `Feed` 的枚举值，也不改 `subscribe` 语义。

### 3.2 服务端：只读补齐 `thread/readItems`（G4）

**请求**：`{ "threadId": "t1", "afterItemId": "<id>"? }`（`afterItemId` 可省＝从头）。
**响应**：`{ "items": [Item, ...] }`（已落盘的 items，按序；`afterItemId` 只返回其后的部分）。

- **只读**：读 `store_lookup(...).load(threadId)`，**不 resume、不重建 agent、不中断 turn**。
  这是相对 `thread/resume` 的关键差异（resume 会先中断在跑的回合）。
- 会话不存在/不可读 → `unknown_thread`（-32011），与既有约定一致。
- 不在 admin 门禁内（任何已初始化客户端可读自己看得到的会话正文；与 `thread/resume` 同级）。
- `afterItemId` 找不到时（客户端记的 id 已被 compact 等丢弃）→ 退化为返回全部 items，
  由客户端按 item id 去重（见 §3.4）。

> **实现备注**：`Item` 已有稳定 `id`（`Item::UserMessage{id,..}` 等），足以做前后端去重与
> "从某 item 之后"的切片；`LoadedThread.items` 即所需数据。

### 3.3 前端：warm 窗口 + 订阅（G2/G3，仅远程客户端）

- 新增 `isRemoteClient()` 判定（已存在，见 `lib/platform.ts`）：仅远程（ws）客户端参与订阅。
- 维护 `warmWindow: string[]`（LRU，上限 `K=8`；`K ≤ 16` 是编译期常量约束，无需再夹）。
- `selectThread(id)`：
  - 把 `id` 移到窗口头；
  - 若**在窗口内**：无网络动作（通知一直在累积，切回即最新）；
  - 若**不在窗口内**（冷会话）：先 `thread/readItems(id, after 上次看到的 item id)` 补齐内容，
    再 `thread/subscribe(window)`；**不调 resume**。
- 需要对该会话发消息/打断时，若它在服务端内存中不存在（如服务端重启过），才回退到既有
  `thread/resume`（保持今天的行为）。
- 桌面分支（Tauri/stdio）：**不发任何 subscribe/readItems**，一行不改。

### 3.4 错误处理与并发

- `readItems` 返回的 items 与随后到达的实时帧**按 item id 去重合并**，避免重复气泡。
- `subscribe` 失败（超 16、缺参）→ 记错误、**不清空**已显示内容；窗口 `K=8` 天然不超限。- `readItems` 失败（-32011 等）→ 保留已有内容并提示，不清空视图。
- **补齐与实时并发**：先 `readItems` 再 `subscribe`；两段之间若有实时帧漏掉，由 `subscribe`
  之后的下一帧或下次补齐覆盖；以 item id 去重保证不重不漏（允许极小的顺序抖动，不阻塞 UI）。

### 3.5 协议（新增，纯增量）

- `thread/readItems` 请求/响应见 §3.2。不新增通知类型，`PROTOCOL_VERSION` 不变。

## 4. 测试策略

**服务端（Tier 0，单元/集成）**
- `delivery()` 分类：`thread/started`、`thread/status/updated` → List；正文类 → Content；
  主题/错误/approvalResolved → Global。
- **列表层恒推**：订阅 t1 的客户端**能**收到 t2 的 `thread/status/updated`（与 S1 的
  "内容按 thread 过滤"不冲突），但**收不到** t2 的 `item/delta`。
- `Feed::All` 客户端（桌面）列表层/内容层全收（零回归）。
- `thread/readItems`：对**正在跑**的会话调用后**回合不中断**、返回正确 items；`afterItemId`
  切片正确；未知 thread → -32011。

**前端（vitest）**
- 远程客户端：`selectThread` 触发 `thread/subscribe`（集合含当前 + warm）；桌面**从不**发。
- 窗口内切换：不触发 `readItems`；窗口外切换：触发 `readItems` 补齐。
- `readItems` 与实时帧按 id 去重（无重复气泡）。
- 桌面 439+ 测试全绿、`tsc --noEmit` 干净。

**验证命令**
```
cargo test -p yi-agent-app-server
cd desktop && npx vitest run && npx tsc --noEmit
```

## 5. 兼容与迁移

- **纯增量**：不调 `thread/subscribe`/`thread/readItems` 的客户端行为不变（桌面零改动）。
- 列表层恒推意味着"已订阅客户端也会收到**所有**会话的 `thread/status/updated`"——这是有意的
  （列表要准）；这些帧很小，流量代价可忽略。
- 桌面（stdio）实例永无订阅者，S1 的"零回归字节不变"不变量继续成立。

## 6. 已知限制（如实记录）

- 手机侧边栏的**未读数/最后活动**仍靠主动 `thread/listAll`，本设计不新增摘要流。
- warm 窗口超出后返回，依赖 `thread/readItems` 补齐：若服务端尚未落盘（例如极短时间内的
  在途内容），返回时可能有**极小空档**，由随后的实时帧与再次补齐覆盖。
- 订阅是**每连接**的：重连后需重新 `thread/subscribe`（与 `initialize` 后状态重建一致）。
- 内容层与列表层是**实例级**的：同一 ws 实例内若有订阅者，delta 合并对所有客户端生效
  （S1 §3.2 已知限制，本设计不改）。
