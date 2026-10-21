# 设计：用户首次输入时即生成会话标题

日期：2026-10-22
状态：已确认（方案 A：并发、不阻塞 turn）
关联：`docs/superpowers/specs/2026-10-09-thread-title-llm-design.md`、
`docs/superpowers/specs/2026-10-17-thread-title-live-update-design.md`（本设计取代其触发时机）

## 1. 背景与问题

现状：标题在 session **首轮结束后**由 `persist_and_finish_turn` 生成
（`server.rs:4556`），材料是「用户提问 + 首轮助手回复」。用户发出消息后要等整轮
跑完（可能数十秒）才在侧栏看到有意义的标题；在此之前侧栏显示的是首句截断值。

需求：用户输入完、把消息发出去的那一刻，就基于**用户输入本身**生成标题，不再等
session 这一轮结束。

## 2. 目标与非目标

### 目标

1. 用户首次输入被 driver 接收后，**立即**发起一次标题生成，不等待本轮结束。
2. 标题生成不得拖慢 turn 的启动与首字输出——用户不该为标题等待。
3. 失败、超时、空结果时静默保留 `title_from` 兜底标题，绝不打断 turn。
4. 保持既有不变量：仅首次；手动改名优先（CAS）；组长/驱动会话不受影响。

### 非目标

- 不改标题模型的输入口径之外的任何参数（模型、`max_tokens=64`、5 秒超时、清洗规则）。
- 不改 `thread/rename` RPC、`thread/titleChanged` 通知形状、桌面端就地更新逻辑。
- 不做「后续轮次内容变了就重新生成标题」。
- 不在 `turn/start` RPC 层做（那样只覆盖 RPC 路径，漏掉插件驱动与组长投递）。

## 3. 设计

### 3.1 触发点

放在 `run_thread_driver` 收到 `TurnPrompt` 之后、`agent.run_blocks(blocks)` 之前
（`server.rs:4955` 附近）。这一处同时覆盖三条投递路径：`turn/start` RPC、
插件 `prepare_and_deliver`、组长 `deliver_to_lead`。

伪代码：

```rust
// 已拿到 user_prompt（= display_text，用户原话，不含注入清单）
if !user_prompt.trim().is_empty() {
    // touch 仍是「是否首次」的唯一判据，同时写入兜底标题。
    if let Ok(true) = store.touch(&thread_id, Some(&user_prompt)) {
        let fallback = crate::thread_store::title_from(&user_prompt);
        let provider = Arc::clone(&provider);
        let config = config.clone();
        let store = Arc::clone(&store);
        let hub = Arc::clone(&hub);
        let tid = thread_id.clone();
        let prompt = user_prompt.clone();
        tokio::spawn(async move {
            // 与旧实现完全相同的「生成 → 清洗 → CAS → 广播」序列
            ...
        });
    }
}
```

- `touch` 保留作为首次判据：它返回 `true` 当且仅当 meta 的 title 由 `None` 变为
  兜底值。组长会话与插件驱动会话创建时标题已非空，返回 `false`，天然不触发。
- `tokio::spawn` 而非 `await`：driver 是单任务串行驱动，await 会把用户首字延迟
  最多 `TITLE_TIMEOUT`（5 秒）。spawn 后 driver 立刻进入 `run_blocks`。

### 3.2 素材：只用用户输入

`generate_title(&provider, &config, &prompt, None)`。

- `build_title_material(prompt, None)` 只渲染「用户提问：…」一段，材料上限仍是
  每侧 2000 字符。
- 直接取舍：首句若是「帮我改一下」这类指代性短语，标题质量弱于原先「带助手回复」
  的材料。这是「不等本轮结束」的必然代价，用户已确认接受。

### 3.3 落盘与广播

与现状逐字相同，只在 spawn 的任务里执行：

1. `generate_title` 外包 `tokio::time::timeout(TITLE_TIMEOUT, ...)`。
2. `Ok(Ok(Some(title)))` → `store.set_title_if_unchanged(&tid, &fallback, &title)`。
3. 仅当返回 `Ok(true)` 才 `write_notification(Notification::TitleChanged)`。
4. `Ok(Ok(None))` / `Ok(Err(_))` / `Err(_)`：静默保留兜底标题（失败记
   `tracing::info!`）。

CAS 的 `expected` 仍是 `title_from(prompt)`，与 `touch` 写入的字节完全一致，
因此手动 `rename` 依然优先。

### 3.4 删除旧触发点

`persist_and_finish_turn` 中 `first_turn` 变量与整个「首轮结束生成标题」分支删除
（`server.rs:4509`、`4528`、`4556`–`4600` 一带）。`touch` 调用保留（仍负责
`updated_at` 与兜底标题），但返回值不再被使用。

删除后 `persist_and_finish_turn` 不再需要 `provider` 参数；签名收敛，四个调用点
同步去掉该实参。

### 3.5 顺序与可见性的变化

- 标题写入（兜底）发生在 turn 开始前，而非结束后：侧栏在用户回车后立刻有名字。
- LLM 标题通常在数秒内完成，与 turn 并行；`thread/titleChanged` 到达时桌面端就地
  更新，无需重拉列表。
- 兜底标题不广播：它发生在 `turn/start` 的响应之前/同时，客户端随后必然通过
  `refreshThreads()`（`turn/completed` 触发）读到它；且首轮兜底标题与截断首句
  在客户端本地预览一致，不发通知不产生可见偏差。

## 4. 错误处理

| 情况 | 处理 |
|---|---|
| `touch` 返回 `false`（已有标题 / 组长 / 驱动会话） | 不生成 |
| `touch` 返回 `Err` | 记 stderr；不生成（标题是展示层，不影响 turn） |
| `user_prompt` 全空白（图片-only 消息） | 不生成（没有可用的标题材料） |
| provider 报错 / 超时 / 清洗为空 | 保留兜底标题，记 `tracing::info!` |
| CAS 返回 `Ok(false)`（被手动改名抢占） | 静默跳过，不广播 |
| 广播失败 | 记 stderr |

spawn 的任务不 panic 传播：标题失败不得影响 driver。

## 5. 影响面

- `server.rs::run_thread_driver`：新增触发块。
- `server.rs::persist_and_finish_turn`：删除标题分支与 `first_turn`，去掉
  `provider` 参数。
- 新增一个小的内部函数（如 `spawn_title_generation`）承载 spawn 与 CAS/广播序列，
  便于单测与阅读。
- `yi-agent-core/src/title.rs` 不动。
- 协议、桌面端、TUI 均不动。

## 6. 测试

- `rpc_tests`：首轮 `turn/start` 后能读到 `thread/titleChanged`，且
  `params.title` 是 LLM 标题——现在应**早于** `turn/completed` 到达（用
  `read_until_method` 断言标题通知先于完成通知）。
- `rpc_tests`：标题 provider 报错时最终标题等于兜底值，且只调用过一次。
- `rpc_tests`：第二轮不再次调用标题模型（`title_calls == 1`）。
- `rpc_tests`：组长会话与驱动会话不产生标题调用。
- 单测：`user_prompt` 全空白时不触发。
