# 设计：LLM 生成的首轮标题在侧栏即时可见

日期：2026-10-17
状态：已评审
关联：`docs/superpowers/specs/2026-10-09-thread-title-llm-design.md` §5.1（本设计落实其「留待后续」项）

## 1. 背景与问题

`feat/thread-title-llm`（已合入 main）为 thread 首轮结束后用一次 LLM 调用生成
标题，写入 `meta.title`。但该分支的 spec §5 已登记一个已知限制：

> **标题不会随 `turn/completed` 自动刷新（已知限制）。** `turn/completed` 由
> translator 在 `persist_and_finish_turn` **之前**发出，而兜底标题与随后可能的
> LLM 标题都写在 `persist_and_finish_turn` **之后**。因此桌面端在本轮
> `turn/completed` 触发的 `refreshThreads()` 只能拿到兜底截断标题。

后果：LLM 标题虽已正确落盘，桌面端侧栏却仍显示首句截断标题，直到一次**无关的**
刷新（后续轮的 `thread/listAll`、resume、或重连）才更新。且协议中不存在任何标题
变更通知，没有第二条路径能把新标题推给 UI。

## 2. 目标与非目标

### 目标

1. LM 生成的首轮标题写入成功后，**所有已连接客户端**（桌面 + 手机）立即在会话
   列表里看到新标题，无需用户切换到别处或触发重连。
2. 复用既有的会话元数据同步模式，不新造机制。

### 非目标

- 不改标题生成时机、材料、清洗或超时逻辑（`title.rs` 不动）。
- 不改 `thread/rename` 的既有路径（手动改名后前端已自行重拉列表）。
- 不改 `thread/list(All)` 的返回格式。
- 不做「内容变了就重新生成标题」；仅在既有的首轮生成成功后补一条通知。
- 不引入「标题模型」配置项。

## 3. 设计

### 3.1 新增列表层通知 `thread/titleChanged`

与 `thread/modelChanged` / `thread/permissionModeChanged` /
`thread/reasoningEffortChanged` 同族：会话元数据变更，**列表层恒推**（按
`thread_key()` 路由，但不受订阅窗口过滤——窗口外会话的标题变更同样要送达，否则
手机端会「概率性不同步」）。

```rust
#[serde(rename = "thread/titleChanged")]
TitleChanged { thread_id: String, title: String },
```

- `thread_key()` → `Some(thread_id)`
- `delivery()` → `Delivery::List`
- 参数 snake_case（`thread_id` / `title`），与同族一致。

通知**直接携带权威标题字符串**（而非「该重拉了」的提示）：标题就是一个小字符串，
就地落笔比再扫一次全量 `thread/listAll` 便宜，也与 `modelChanged` 携带生效模型的
既有做法一致。（`process/updated` 那种「只提示重拉」的模式此处无必要。）

### 3.2 广播时机

在 `server.rs::persist_and_finish_turn` 中，`set_title_if_unchanged` **返回 `true`**
（确实发生了替换）时才 `write_notification(TitleChanged)`：

```rust
Ok(Ok(Some(title))) => {
    match store.set_title_if_unchanged(thread_id, &fallback, &title) {
        Ok(true) => {
            let _ = write_notification(
                hub, &Notification::TitleChanged { thread_id: ..., title: title.clone() },
            ).await;
        }
        Ok(false) => {}   // 被手动改名抢占 / 未知 id：不广播，与 CAS 语义一致
        Err(e) => { eprintln!(...); }
    }
}
```

为何只在 `Ok(true)` 时广播：`Ok(false)` 表示标题已被用户手动改名抢占（CAS 未替换）
或 thread 不存在，此时磁盘标题**不是**我们生成的这个值，广播它会让客户端显示一个
不存在的标题。与「手动改名优先」的设计保持一致。

失败/超时/空结果路径本就不写标题，天然不广播。

### 3.3 桌面端就地更新（方案 A）

前端收到 `thread/titleChanged` 后，**不重拉列表**，而是就地把本地 `groups` 与
`pinned` 中该 thread 的 `title` 改写为新值，再触发重渲染。理由：

- 与 `thread/modelChanged` / `thread/permissionModeChanged` 走的同族路径一致
  （通知带权威值，本地就地落笔；见 `threadStore.ts`）。
- 首轮结束本就频繁，避免每次都付一次全量 `thread/listAll`（多目录深扫）的成本。
- 无 RPC 竞态：不必等一个往返才可见。

实现上抽出一个**纯函数**（放在 `desktop/src/lib/`），输入 `(groups, pinned,
threadId, title)` 返回更新后的副本，便于单测覆盖「只改目标 thread、不动其它字段/
其它 thread、置顶与分组两份都更新」。`App.tsx` 的通知分支调用它并
`setGroups`/`setPinned`/`force`。

### 3.4 错误处理

- 广播失败（如序列化错误）：静默记 stderr，绝不影响 turn 收尾——与标题生成同款
  「锦上添花、绝不拖住收尾」的取向。
- 客户端收到未知 thread 的标题变更：就地更新是 no-op（该 thread 不在列表里就什么
  都不改），无需特判。

## 4. 影响面

- 协议新增一条通知；旧客户端不认识它，会走通知解码的默认分支（忽略），无破坏性。
- 前端仅新增一个通知分支与一个纯函数。
- 首轮结束多一帧广播（极小），无额外 RPC、无额外读盘。

## 5. 测试

- `protocol.rs` 单测：`TitleChanged` 的 wire 形状（method / params snake_case）、
  `delivery() == List`、`thread_key() == Some(thread_id)`。
- `server.rs` 集成：首轮 turn 后能读到 `thread/titleChanged`，且 `params.title`
  是 LLM 标题（非兜底截断值）；生成失败路径**不**发该通知。
- TS 纯函数单测：只改目标 thread 的 title；不动其它 thread 与其它字段；置顶与分组
  两份都更新；未知 thread 为 no-op。
- `App.test.tsx` 组件测：收到 `thread/titleChanged` 后侧栏该行标题变为新值。