# 项目组长 Agent P3-A：视野工具（`lead_list_threads` / `lead_read_thread`）设计

状态：待实现

上级设计：[2026-10-17-project-lead-agent-design.md](./2026-10-17-project-lead-agent-design.md)（§7.1、§7.2、§7.5、§8）
前序分期：[P1](./2026-10-17-project-lead-agent-p1.md) 已实现、[P2](./2026-10-17-project-lead-agent-p2.md) 已实现。

## 1. 本设计覆盖的范围

上级 spec §11 把 P3 定为三块：①按需拉取工具（`lead_list_threads` / `lead_read_thread` 的完整形态）；
②权限留痕/撤销；③移动端远程。本设计**只做 ①**（下称 P3-A）。

**②③ 明确不在本设计内**，理由记在 §9。

同时，本设计顺带修掉 P2 遗留的两处缺陷（§7），它们与本块同属「组长视野/记忆」的语义面，
且都在同一个 crate、同一批文件里，分开走会重复一套评审流程。

## 2. 要解决的问题

组长（lead）今天只有两个视野工具：`lead_remember` / `lead_recall`（读写自己的账本）。
它**看不到名下有谁、谁卡住了、别人交付了什么**——P2 的后台 digest 只在「有变化」时推一条
极简文本（`lead_loop::render_digest`，只展开被唤醒的 thread），组长无法主动查看全貌。

结果：组长能收留言、能记账，但不能回答「我手下这几个 thread 现在什么情况」「A 那轮到底交付了
什么」。上级 spec §7.2 要求补齐这两个按需拉取工具。

## 3. 核心架构决定：纯磁盘派（不注入实时状态）

**决定：两个工具只读磁盘真相，不读实时状态。**

数据来源仅两处，都是组长装配期已有、且与它同根的：

| 来源 | 路径 | 提供 |
|---|---|---|
| thread meta | `<project_root>/.yi-agent/threads/*.meta.json` | thread_id / 标题 / cwd / 创建与更新时间 / 角色 / 模型 / 看板卡 |
| thread 转录 | `<project_root>/.yi-agent/threads/<id>.jsonl` | 逐轮的 item（用于取「最后一条结论」） |

### 3.1 为什么不带实时状态

实时状态（`Running` / `AwaitingApproval` / `Idle`）**只存在于主循环独占的内存**
（`server.rs` 的 `threads: HashMap<String, ThreadSession>`，状态是 `ThreadSession.status`）；
持久化的 `update_status` 只广播通知、**不落盘**。而 `lead_*` 工具是在 driver 侧、装配期只拿到
`project_root` 构造的，天然看不到那份内存。

要带真实状态，只能把状态复制成第二份共享内存（`Arc<Mutex<HashMap<thread_id, ThreadStatus>>>`）
并穿过 6 处装配点注入。**这是刻意不做的**：

- 上级 spec §6.4 的核心原则是「可再生信息每轮现算、不进历史」；状态是最易变的一类。
- 复制一份易变状态并要求时刻同步，正是 spec §10 反复警告的模式，且它并不能消除
  「工具读到时已经过期」——只是把过期窗口挪到了另一处。
- 分工本来就更清楚：**推 = 状态变化**（后台 digest，P2 已有：`AwaitingApproval` → `ThreadStuck`），
  **拉 = 目录 + 交付结论 + 转录**（本设计）。digest 推的是「变了什么」，列表给的是「有谁」。

### 3.2 与上级 spec §7.1 的偏差（显式记录）

§7.1 字面要求索引含「标题 / **状态** / 最后一条结论 / **是否有待裁决事项**」。
纯磁盘派拿不到后两项（都要实时状态）。**本设计有意偏离，并把这四项重新分工**：

| §7.1 的字面字段 | 本设计里由谁提供 |
|---|---|
| 标题 | `lead_list_threads`（磁盘 meta） |
| 最后一条结论 | `lead_list_threads`（读 jsonl 尾，见 §4.2） |
| 状态 | 后台 digest 推（P2 已有）；列表**不**报 |
| 是否有待裁决事项 | 同上 |

这条偏差是 P3-A 的**已知限制**，不是遗漏：若将来实测发现组长必须在不被唤醒时也知道谁在等审批，
再考虑注入状态投影（届时改的是装配链，不是本设计的工具形状）。

### 3.3 卡片会话（看板 worktree）不在视野内

一条看板卡会话的 cwd 是卡片 worktree（`.worktrees/<branch>/`），其 store 与 meta 落在
**worktree 的** `.yi-agent/threads` 下，`project_key` 因此是 worktree 而非项目根——它在
P2 的 tick 分组里本就被当成「另一个（没有组长的）项目」跳过。

本设计沿用这一口径：`lead_list_threads` 只列**同一个 store**（项目根下）的 thread。
卡片会话不进组长的视野列表，**这是既有语义的一致延续，不是新引入的缺口**。

## 4. 工具一：`lead_list_threads`

### 4.1 形状

```
lead_list_threads({}) -> text
```

无必填参数。可选 `limit`（默认 50，上限 200）。输出是一份**逐行**清单，每个人一条：

```
本目录（<project_root>）共有 4 个会话：
- thread-a 「实现登录」 更新 2026-10-18 10:03（3 条）最后：已跑完 fmt 与单测，等你验收
- thread-b 「数据清洗脚本」 更新 2026-10-18 09:41（12 条）最后：卡在权限审批，需要你放行
- <lead 自己>（跳过）
```

规则：

- 读 `ThreadStore::new(project_root).list()`（按 `updated_at` 降序，既有实现）。
- **跳过组长自己**（`role == LEAD_ROLE`）——它对「我手下有谁」没有信息量，且会让列表自指。
- 每条附「最后一条结论」：取该 thread 转录里**最后一条非空 `AgentMessage.text`**（§4.2）。
  取不到（新会话、纯工具轮）就不写「最后：」那一段。
- 末尾附一行总计与被截断提示（`limit` 生效时）。

### 4.2 「最后一条结论」怎么取（有界）

复用既有落盘格式：`.jsonl` 每行是一条 `TurnLine::Turn { items, .. }`。
为不把整个转录读进内存（§6.4 的预算意识），实现**从文件尾部反向扫描**：
按行从后往前找第一条含非空 `AgentMessage` 的行，取该 item 的 `text`，截断到
`LEAD_LIST_LAST_TEXT_MAX = 200` 字符（超长加 `…`）。

- 正常情况最后一行就命中（大多数会话的最后一条 item 就是助手回复）。
- 读失败 / 文件不存在：该条不加「最后：」段，**不让整个列表失败**（与 `ThreadStore::list`
  跳损坏 meta 的既有宽容口径一致）。
- 组长自己与普通会话走同一段代码，只是被 skip。

## 5. 工具二：`lead_read_thread`

### 5.1 形状

```
lead_read_thread({ thread_id: string, limit?: number }) -> text
```

- `thread_id` 必填。**必须**属于本项目 store（`store.load(thread_id)` 返回 `Some`）；
  不属于则报错，绝不读项目外的路径。
- `limit` = 最多返回多少条 item，默认 `LEAD_READ_DEFAULT_ITEMS = 20`，上限
  `LEAD_READ_MAX_ITEMS = 50`。
- 输出 = **尾部**（最近的 limit 条 item），按时间正序渲染；并明确披露
  「共 N 条，显示最近 M 条」（截断时必须说，否则组长以为看到了全部）。

### 5.2 为什么取尾部而不是头部

组长问「A 交付了什么」「B 卡在哪」——答案在**最近**的来往里，不在会话开头。
这与上级 spec §7.2 的「增量/截断」一致，也复用既有 `thread/readItems` 的
`afterItemId` 增量思路（本工具不做增量指针，只做尾部窗口：组长的用法是「看一眼现状」，
不是「跟着流读」，多一个游标状态没有收益）。

### 5.3 渲染口径

复用既有 item 的展示语义（`Item` 的五个变体），每条一行：

- `UserMessage` / `UserInterjection` → `用户：<text>`
- `AgentMessage` → `组长/助手：<text>`（用中性前缀「助手：」，因为读的是别的 thread）
- `ToolCall` → `工具 <name>（<status>）`
- `Thinking` → `（思考块，略）`

单条文本截断到 `LEAD_READ_ITEM_MAX = 500` 字符；整体输出再加
`LEAD_READ_MAX_BYTES = 32 * 1024` 字节的硬上限，超了就从**最旧**的一端丢并在头部标注。
这两个上限是 §6.4「注入预算」在拉取侧的直接落实。

## 6. 注册与边界（§7.5）

两个工具与 `lead_remember` / `lead_recall` **同一处**注册、**同一条件**门控：
`build_runtime_tooling` 里 `if crate::lead_memory::is_lead_thread(&store, thread_id)` 的分支内
（`server.rs` 现约 `:999`）。

- 普通 thread 与子 worker 一律看不到（§7.5）。
- 两个都 `read_only: true`（`ToolMetadata`）——它们是纯读，不触发审批、不改任何状态。
- `project_root` 与账本工具同源：`cfg.workdir`（= thread cwd = 项目根），
  理由与 §7.3 的三条一致（meta、账本、本工具读的 store 必须同根）。

## 7. 顺带修的两处 P2 缺陷

两处都在 main 上、都有明确改法，本设计一并纳入（计划里为 Task 1、2）。

### 7.1 `lead_recall` 返回最旧条目且不披露截断

现状（`lead_memory.rs`）：

```rust
hits.sort_by_key(|e| e.timestamp_ms);   // 升序 = 最旧在前
hits.truncate(limit);                    // 留下 = 最旧的 limit 条
```

账本装的是「决策与理由」，同一 thread 先后两次决策时**最新那条才是权威**；现状恰好丢掉最新几条、
返回已被推翻的旧理由——这比返回空更难排查，与 §7.3「可审计」的目的直接冲突。

**改**：取**最近** `limit` 条（丢掉最旧的），并在工具输出披露「匹配 N 条，显示最近 M 条」。
`search` 的返回**顺序**仍按时间升序（便于阅读），只是窗口从头部换成尾部。

### 7.2 后台 tick 在 `AwaitingApproval` 期间每 30s 重复注入同一 digest

现状：`lead_loop::decide_wake` 对每个 `AwaitingApproval` 无条件产出 `ThreadStuck`；
`LEAD_TICK = 30s`、`PERMISSION_TIMEOUT = 300s`，故一次审批窗最多让组长被同一条消息唤醒
**10 次**，每次都是带着组长全部上下文的完整 LLM turn（纯噪声 + 真金白银）。
（另注：tick 恒以 `pending_asks = 0` 调 `decide_wake`，`AskLead` 分支在 tick 里是死的。）

**改**：主循环为每个项目记住**上次投递的 digest 文本**，文本相同就**不重复投递**；
内容一变（状态变了 / 新 thread 卡住）就恢复投递。约 4 行 + 一个 `HashMap<String, String>`，
纯内存、不落盘、自愈。

比按 `(thread_id, reason)` 加闩锁更好：不用理解状态机，天然覆盖所有 wake reason，
且不违背 §6.4「态势每轮现算」（digest 仍每轮现算，只是不重复**投递**一模一样的）。

## 8. 测试与验证

- 新增模块 `lead_index.rs`，判定/渲染全为纯函数（可单测，零 LLM）：
  - `list_summaries(project_root, limit) -> io::Result<Vec<ThreadSummary>>`
  - `read_tail(project_root, thread_id, limit) -> io::Result<Option<ThreadTail>>`
  - `render_summaries(&[ThreadSummary], total: usize) -> String`
  - `render_tail(&ThreadTail) -> String`
- 工具层两个 `#[tokio::test]`（`lead_list_threads` / `lead_read_thread` 的 `call()`）。
- 门控测试：非组长装配后注册表里**没有**这两个工具（沿用既有
  `only_the_lead_sees_the_memory_tools` 的形状）。
- 两处修复各自的回归测试（§7.1 取最近 + 披露；§7.2 相同 digest 不重复投递）。

验证命令：

```
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead_index
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc -- \
  two_conversation_roots_do_not_share_children_or_reach_across \
  ending_one_conversation_reclaims_only_its_own_root
cd yi-agent-rs && cargo fmt --all -- --check
```

## 9. 明确不做（范围外）

- **不做实时状态投影**（§3.1）：列表不报 `Running` / `AwaitingApproval`。
- **不做权限留痕/撤销**（上级 §8）：组长仍**零写工具**（除账本）。上级 §8 引用的
  「既有 `review_agent` 强制 feedback/reason、`accept` 只由 Git ancestry 观察完成」这个前提
  **在当前代码里已不成立**——`yi-agent-subagent/src/lib.rs` 有测试强制 `review_agent`
  **不得注册**（交付即完成，验收是父自己的 git 动作）。因此「取消/打回/改 objective + 留痕 + 可撤销」
  需要先重新定义「在没有 review 工具的世界里『打回』是什么」，另开一轮设计。
- **不做移动端专用 UI**：iOS 打包的是同一份前端产物（`tauri.conf.json` 的
  `frontendDist: ../dist`），且侧栏组长行**不**被 `isMobile` 门控，上级 §9.2 的
  「移动端天然可见」今天已成立。真正的缺口是组长行带 `data-thread-row` 会被手机长按弹
  「重命名/置顶/删除」菜单（后两项对组长无意义），属独立小修，不在本设计内。
- 不为组长另建一套索引存储（§7.1 要求「不新增存储」）。
- 不把两个新工具暴露给普通 thread 或子 worker（§7.5）。

## 10. 危险点与对策

| 危险点 | 对策 |
|---|---|
| 列表把整个项目全读进上下文 | 每 thread 只取「最后一条结论」一行、截断 200 字符；`limit` 默认 50 |
| 读转录撑爆组长窗口 | 尾部窗口（默认 20 条）+ 单条 500 字符 + 整体 32 KiB 硬上限 |
| 组长读到项目外的 thread | `store.load(thread_id)` 必须在**本项目 store** 命中；否则报错 |
| 工具给错对象（普通 thread 也能读全项目） | 与账本工具同门控（`is_lead_thread`），并有门控测试 |
| 列表随项目变大而变慢 | `ThreadStore::list()` 只读 meta（已有）；「最后一条结论」反向扫描，命中即停 |
| 组长把「没看到状态」当成「没事」 | 输出首行写明本列表**不含实时状态**，状态变化由后台唤醒推送 |