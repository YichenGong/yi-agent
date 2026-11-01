# CLI 级会话 resume 与 fork

日期：2026-11-01
状态：设计已确认，待实现

## 1. 问题

`yi-agent` 的会话持久化已经存在，但只在 app-server（桌面 / 手机）这条路上生效。终端交互式 TUI 是**纯内存会话**：进程退出，对话就没了，无法继续。

现状（已核实）：

| 能力 | 位置 | 状态 |
| --- | --- | --- |
| 会话落盘 `<cwd>/.yi-agent/threads/*.{jsonl,meta.json}` | `thread_store.rs:258` | 有 |
| 载入历史并恢复上下文 | `rpc/thread.rs:373` `thread/resume` | 有 |
| 只读补齐 items | `rpc/thread.rs:2208` `thread/readItems` | 有 |
| 截断尾部悬空 `tool_use` | `core/agent.rs:793` `fork_prefix_len` | 有 |
| CLI resume / fork 参数 | `config.rs:17` `Cli` | **无** |
| TUI 使用 `ThreadStore` | 全仓库 `crates/yi-agent/src` 零命中 | **无** |
| 跨进程写者互斥 | `ThreadStore.meta_lock` 仅进程内 `Mutex`（`thread_store.rs:261`） | **无** |

三个同名 `resume` 容易混淆，本设计**只**处理会话级：

- `yi-agent agent resume <task-id>` / TUI `/resume <task-id>`：恢复**子 agent 任务**，与本设计无关
- `thread/resume` RPC：本设计复用的会话恢复能力

## 2. 目标与非目标

### 目标

1. `yi-agent resume <thread-id>`：载入历史并继续对话
2. `yi-agent resume --last`：载入同目录最近一条
3. `yi-agent fork <thread-id>`：物理复制历史，开新会话继续
4. TUI 每轮对话落盘，使 resume 后的新内容可再次 resume
5. 跨进程写者互斥，冲突即拒绝
6. headless `yi-agent run --resume <id>`：端到端可测的 resume 载体

### 非目标

- 全屏 TUI picker（用 `yi-agent threads` 文本列表替代）
- 跨目录 `--all`（`ThreadStore` 按 cwd 组织，跨目录需 `listAll` 索引，另议）
- 会话归档 / 重命名 / 搜索 / 删除（app-server 已有，CLI 不重复实现）
- 会话级 fork 的 worktree 隔离（与子 agent 的 `worktree` 是两件事）
- **`/clear` 的物理回收**：本设计的 `/clear` 是追加边界（§6.4），不删数据。长期使用会让 `.jsonl` 只增不减。这是刻意取舍——物理回收需要 tarball/compaction 那套机制（app-server 的归档走的是另一条路），且删数据必须可撤销。若磁盘成为问题，后续单独做，不在本轮。

### 已知取舍

- fork 物理复制让长会话磁盘翻倍（§3.2 已说明）
- `/clear` 与 fork 都只追加不回收，`.jsonl` 会持续增长

## 3. 关键设计决策

### 3.1 不新增截断模块，复用 `forkable_messages`

resume 与 fork 都需要把落盘的 `messages` 变成 provider 可接受的转录：末尾若是带 `tool_use` 的 assistant 消息（崩溃或取消残留），必须截掉，否则 provider 拒绝请求。

**这条规则已经有唯一实现**——`fork_prefix_len`（`core/agent.rs:793`）。它当前被三条路径共用：fork 子 agent（`subagent/lib.rs:1573`）、取消回滚（`safe_cancel_truncate_len`）、以及 `forkable_messages`。

因此 resume 直接：

```rust
let session = Session::from_messages(loaded.messages);
let safe = forkable_messages(&session);
```

**不新建 `core/resume.rs`**。多一份实现就会多一次漂移机会，而这正是注释里已经警告过的事（"both paths share this single rule"）。

### 3.2 fork 用物理复制，`forked_from` 仅作展示

新会话建立自己的 `.jsonl`，写入源会话的全部 `items` + 一份 `messages` 快照。读取路径（`load`）**完全不知道 `forked_from` 存在**。

理由：零读取改动、两会话彻底独立、删除源会话不影响派生会话。代价是长会话磁盘翻倍，这是已知取舍。

被否掉的两条路：

- **引用前缀**（新会话只记 `{forked_from, prefix_len}`）：零复制，但 `load` 要回溯拼接，源会话被删会悬空，一致性规则复杂。
- **只复制 messages 不复制 items**：磁盘最省，但 resume 打开 fork 时屏幕空白，用户会以为历史丢了。

### 3.3 锁：按会话、`flock`、永不删锁文件

app-server（桌面）与 TUI 会写同一个 `<cwd>/.yi-agent/threads/`。没有跨进程锁，两端的 `append_turn` 会交错进同一个 `.jsonl`，`meta.json` 的读-改-写会互相覆盖。

**照抄项目已有的 `flock` 模式**（`device_store.rs:81` / `ipc.rs:1226`）：

- 锁文件：`<cwd>/.yi-agent/threads/<thread-id>.lock`
- **永不删除**。`device_store.rs:78` 的注释已记录原因：删除会让「已打开同一 inode 的竞争者」锁在 unlinked 文件上，第三个进程新建同名文件后两个进程都以为持锁。Codex 敢删是因为它另有 `.coordination.lock` 兜底（`rollout/src/writer_lock.rs:49`），本项目不引入协调锁，就绝不能删。
- **`LOCK_NB` 非阻塞**。`device_store` 用阻塞是因为临界区只有一次「读 + rename」，极短；这里要持有整个会话，阻塞会让用户干等。

**实现必须用 `libc::flock`，不能用 `std::fs::File::try_lock`。** 后者要到 Rust 1.89 才稳定，而本项目 MSRV 是 `1.85`（`yi-agent-rs/Cargo.toml:23`）。`ipc.rs:1226` 已把正确写法做出来，直接照抄：

```rust
let file = OpenOptions::new().create(true).write(true).truncate(false).open(path)?;
let outcome = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
if outcome != 0 {
    let error = std::io::Error::last_os_error();
    if matches!(error.raw_os_error(), Some(c) if c == libc::EWOULDBLOCK || c == libc::EAGAIN) {
        return Err(ThreadBusy { thread_id });   // 冲突
    }
    return Err(error);                          // 坏描述符 / 不支持的 fs，不得误报为冲突
}
Ok(ThreadWriterLock { _file: file })            // 仅持 File，无 Drop 实现
```

`EWOULDBLOCK` 与 `EAGAIN` 必须单独判，其余错误一律上抛——把真实故障误报成「会话被占用」会让用户永远找不到原因。

**必须做进程内去重表**。`ThreadStore` 有 127 处 `ThreadStore::new` 调用点，同一路径可能被构造出多个实例；而 `flock` 按**打开文件描述**加锁、不按路径，同一进程内两个描述符各拿一次不会互相阻塞，于是「本进程已持有」会被自己判成空闲。锁因此挂在**进程级静态表**上（`OnceLock<Mutex<HashMap<PathBuf, Arc<ThreadWriterLock>>>>`），与 `group_store.rs:120` 的进程级 `static LOCK` 同思路：

```rust
fn acquire_thread_lock(root: &Path, thread_id: &str) -> Result<Arc<ThreadWriterLock>, ThreadBusy>
```

语义：先查静态表，已持有则返回**同一个 `Arc`**（幂等，调用方可重复取）；表里没有才去动内核 `flock`。所有 `Arc` drop 后移除条目，内核锁随之释放。

**粒度：会话级持有**。进入 `resume` / `fork` 时取锁，进程退出随 `File` drop 释放；进程崩溃内核同样释放，不留死锁（`device_store.rs:76` 注释所述）。持有者是**该会话的 `Arc`**，所以 driver 要把它存进自有状态，避免「取完就 drop」导致锁形同虚设。

**这把锁必须两端都采用**。只加在 TUI 侧而 app-server 不拿锁，锁就是摆设。因此 `thread/resume` 与 driver 启动路径也要取同一把锁。这是本设计改动面积最大的一点。

**锁文件会污染 `list()` 与 `find_first_user_text`，必须显式过滤。** `list()`（`thread_store.rs:439`）只认 `.meta.json` 后缀，`<id>.lock` 天然被跳过，无需改。但任何「扫目录找最新会话」的新代码必须同样只认 `.meta.json`，不能按 `mtime` 取最近的任意文件——否则锁文件（每次 resume 都会创建）会被误判成最新会话。

### 3.4 `meta_lock` 不改，改为立不变量

`ThreadStore.meta_lock`（`thread_store.rs:261`）保持进程内 `Mutex` 不变。

理由：thread 级锁已经挡住跨进程对**同一** thread 的并发；不同 thread 是不同文件，互不相干。加两把锁只会让「哪把锁负责什么」变得含混。

替代方案是在 `ThreadStore` 上立一条不变量：

> **任何修改某个 thread 文件的路径，都必须先持有该 thread 的 writer lock。**

用一份锁同时覆盖 `.jsonl` 追加与 `meta.json` 读-改-写。

### 3.5 `role` 必须清空

fork 的 meta 继承规则里，`role` 是唯一必须**反向**处理的字段。

`find_lead`（`thread_store.rs:493`）按 `created_at` 升序取唯一组长。若 fork 继承 `role: "lead"`，一个项目会出现两个组长，破坏唯一性不变量，且会连带影响 `ask_lead` 的投递目标、`thread/listAll` 的懒创建门控、`lead_memory` 的判定。

## 4. CLI 表面

```
yi-agent                                   # 现状：新会话
yi-agent resume <thread-id>                # resume 指定会话
yi-agent resume <prefix>                   # 唯一前缀匹配
yi-agent resume --last                     # 同目录最近一条
yi-agent resume --last "继续修 bug"        # resume 后立即发这轮
yi-agent fork <thread-id>                  # 复制历史开新会话
yi-agent threads                           # 文本列表：id / 标题 / 更新时间
yi-agent run --resume <thread-id> "…"      # headless resume
```

### 4.1 `--last` 与 positional 的消歧

`resume --last "修复这个 bug"` 里的 positional 是**提示词**，不是会话 id；`resume <id>` 里的是会话 id。clap 无法表达这种条件语义，必须在 `From` 里手工消歧：

```rust
// 与 codex-rs/exec/src/cli.rs:256 同构
let (session_id, prompt) = if last && prompt.is_none() {
    (None, positional)
} else {
    (positional, prompt)
};
```

不做这层，提示词会被当成会话 id，报「找不到会话 修复这个 bug」。

### 4.2 `--last` 的选取规则

`ThreadStore::list()` → 按 `meta.updated_at` 降序 → 取第一条 `meta.cwd` 与当前 workdir canonical 后相等的。

不做 cwd 过滤会让 `--last` 跨目录串会话。不做 canonical 比较会让同一目录的不同写法（符号链接、相对路径）匹配不上。

## 5. 历史注入

三个入口（`resume`、`fork`、`run --resume`）共用一条路径：

```rust
let loaded = store.load(id)?.ok_or(NotFound)?;

// 1. 崩溃残留先升格进主日志（见下方第 2 点）
if loaded.pending_turn {
    store.promote_partial(id)?;   // 失败只记日志，不阻断
}

// 2. 截断尾部悬空 tool_use —— 与 fork 子 agent 共用同一条规则
let seeded = Session::from_messages(loaded.messages);
let safe = forkable_messages(&seeded);
let mut session = Session::from_messages(safe);

// 3. 恢复上次用量，让 auto-compact 在首轮即生效
session.set_last_input_tokens(loaded.usage.map(|u| u.input_tokens));

let agent = Agent::new(provider, tools, config).with_session_arc(Arc::new(Mutex::new(session)));
```

四点必须与 app-server 现行行为严格对齐，否则同一会话在 TUI 与桌面打开后状态不同：

1. **`set_last_input_tokens`**（`rpc/thread.rs:451`）：不设则 auto-compact 在 resume 首轮不触发，长会话可能直接超上下文。
2. **`pending_turn` 必须先 `promote_partial`**（`rpc/thread.rs:687-696`）：这是**容易漏掉且会丢数据**的一步。采纳的 partial 是那一轮唯一的持久副本；不 append 就返回，用户下一条消息的 turn-start checkpoint 会原子覆盖同一文件，该轮 items 永久丢失，而 session 上下文仍「记得」它们，造成 item 与上下文不一致。成功后再补一条 interrupted 提示，否则用户会困惑「最后一句为什么没有回复」。
3. **`forkable_messages` 接收 `&Session` 而非 `Vec<Message>`**（`core/agent.rs:804`）：所以要先 `Session::from_messages` 再截断，再重建一次 `Session`。多这一次构造是刻意的轻薄代价，换的是与 fork 路径共用同一实现。
4. **cwd 已删校验**（`rpc/thread.rs:392`）：`meta.cwd` 不是目录时明确报错，**不得**回退到当前 workdir。回退会让历史里的相对路径全部指向错误位置。

## 6. TUI 落盘

### 6.1 driver 改造

当前 driver 以 `Agent::new(provider, tools, config).with_permission(...)` 构造（`main.rs:2088`），**没有 session**；靠 `live.rebuild`（`main.rs:1855`）在控制命令时才换成 `with_session_arc`。

注入历史必须在首次构造时完成，因此 driver 改为启动即持 `Arc<Mutex<Session>>`：新会话种空 `Vec<Message>`，resume 种已载入的历史。

这与 `rebuild_driver_agent` 的既有约束相容——该函数注释强调 session Arc 必须跨重建存活，因为子 agent 的 `CallerContext` 绑在它上面。

### 6.2 每轮落盘规则

driver 收到本轮 `Done` 后构造 `TurnLine::Turn { items, usage, usage_total, messages }` 并 `append_turn`。规则照搬 `persist_and_finish_turn`（`server.rs:4770`）：

| 规则 | 依据 | 漏掉的后果 |
| --- | --- | --- |
| 补 `UserMessage` item | `server.rs:4795` 明确注释 | resume 后用户提问消失，只剩回答 |
| `preserve_thinking == false` 时剥离落盘副本的思考块 | `server.rs:4807` | 磁盘上的会话「复活」已放弃保留的凭证 |
| 成功后更新 `meta.updated_at` | 本设计新增要求 | `--last` 永远选中最旧会话 |
| 落盘失败不阻断对话 | 与 app-server 的 `eprintln!` 一致 | 磁盘问题导致对话中断 |

落盘只剥**副本**，不动 session 本体——session 是展示与请求的权威。

### 6.3 回放：`Item → HistoryCell` 映射（原设计遗漏，必须补）

**没有这一块，resume 等于白做**：上下文是对的，屏幕却是空的，用户会以为历史丢了。TUI 侧目前**完全没有** `Item` 渲染代码（`crates/yi-agent/src/tui/` 零命中 `Item::UserMessage`），而 `Item` 只定义在 `app-server/src/protocol.rs:825`，desktop 直接消费它、TUI 不能。

必须新增纯函数 `replay.rs::items_to_cells(items, width) -> Vec<HistoryCell>`，按 `Item` 五个变体逐一映射：

| `Item` 变体 | → `HistoryCell` | 备注 |
| --- | --- | --- |
| `UserMessage { text, .. }` | `UserMessage { text }` | 忽略 `attachments`/`images`：TUI 当前无附件渲染 |
| `AgentMessage { text, .. }` | `AssistantMessage { markdown }` | 走既有 markdown 渲染 |
| `ToolCall { name, input, status, result, .. }` | `ToolCall { .. }` + 紧随一个 `ToolResult { .. }` | `status == Failed` ⇒ `ToolResult.is_error = true`；`Running`（崩溃残留）⇒ 结果文本写「中断」 |
| `UserInterjection { text, .. }` | `UserMessage { text }` | TUI 无独立气泡样式，与桌面区分是纯展示差异 |
| `Thinking { .. }` | `Reasoning { text, expanded: false }` | **默认折叠**，与实时流行为一致（`cell.rs:43`） |

三条约束：

1. **必须复用 `HistoryState` 的既有上限**（`MAX_HISTORY_CELLS = 2000`，`history.rs:172`）。回放几千条 item 会撑爆内存，且 `enforce_history_cap` 已实现「外科式从头部 drop」并上报 `dropped_cells()`。直接批量 push 后由既有机制裁剪，不新写裁剪逻辑。
2. **回放不产生「新」的视觉分隔**：`Separator` 只在 turn 边界插入，与实时渲染同规则，否则重开一次会话就多出一堆横线。
3. **`pending_turn` 的 interrupted 提示**用 `Separator { label: Some("上一轮已中断") }`，与既有 `HistoryCell::Separator` 用法一致（`app.rs:2628` 的「对话已清空」即是此形）。

### 6.4 `/clear` 与 `/compact` 的持久化语义（原设计遗漏）

TUI 这两个命令目前只作用于内存（`app.rs:2626` / `:2677`，driver 侧 `main.rs:2108` / `:2124`）。会话一旦持久化，它们直接与磁盘冲突，必须各自定义语义，否则会出现「屏幕上清空了、resume 回来旧内容全在」这种最糟糕的错位。

**`/clear`：只清会话，不删数据。** 三件事一起做：

1. 内存里 `replace_messages(Vec::new())` + `set_last_input_tokens(None)`（沿用 `main.rs:2121` 现状）
2. 向 `.jsonl` 追加一条**空 turns 的边界记录**，让 resume 时按「最后一条边界之后」读取；**不截断、不删文件**
3. `content_epoch += 1`（与 app-server `thread/clear` 的语义一致，`thread_store.rs` 注释：这是 `content_epoch` 的唯一递增点）

第 2 点需要一个新变体：`TurnLine` 增加 `Clear { at: i64 }`，`read_log` 遇到它就把已累积的 items/messages/usage/totals 全部清空。这样 `/clear` 是**纯追加**，不会与并发的读者打架，也天然可回滚（想恢复就删文件）。**不能**用 `truncate` 就地改写——那会与 `flock` 之外的读者产生窗口期，且破坏「`.jsonl` 只追加」这条既有不变量。

实现上要注意：`read_log`（`thread_store.rs:314`）现在是 `match serde_json::from_str::<TurnLine>(line) { Ok(TurnLine::Turn {..}) => .., Err(e) => .. }` 的**穷尽匹配**，加变体后**编译会失败**，必须补 `Ok(TurnLine::Clear { .. }) => { items.clear(); messages.clear(); usage = None; totals = UsageTotals::default(); }` 分支。这不是「加个枚举值」那么轻——漏了它连编译都过不去（这反而是好事，编译器会强制实现者处理）。

**`/compact`：只换上下文，不写 items。** app-server 的 `thread/compact` 落盘时 items 为空、`content_epoch` 不递增（`thread_store.rs` 注释明确：「它落盘时 items 为空、历史 item 分毫未动」）。TUI 照此办理：把压缩后的 `messages` 作为一条 `items: []` 的 `TurnLine` 追加。回放时这条记录不产生任何 cell，但 `read_log` 会用它的 `messages` 覆盖上下文——正好是压缩该有的效果。

漏掉这两条的后果分别是：`/clear` 后 resume 旧内容复活；`/compact` 后 resume 拿到未压缩的超长上下文，可能直接超出模型窗口。

## 7. fork 语义

1. 源会话必须可 `load`，`messages` 经 `forkable_messages` 同口径截断
2. 新 `thread_id`：`thread-<uuid>`
3. 新 `.jsonl`：写入源会话全部 items + 一份 messages 快照（作为首条 `TurnLine`）
4. 新 meta：

| 字段 | 处理 | 理由 |
| --- | --- | --- |
| `cwd` `model` `model_ref` `reasoning_effort` `permission_mode` `preserve_thinking` `anchor_sha` | 继承 | 定义「这段对话是什么」，历史内容依赖它们 |
| `extra_roots` | 继承，过滤已删目录 | 历史提到的文件可能在这些根里；不继承则 fork 后工具访问不到 |
| `created_at` `updated_at` | 取当前时间 | 否则 `--last` 排序错乱 |
| `content_epoch` | 重置 `0` | 新会话的可见内容集合世代 |
| `pin_seq` `tag` | 清空 | 新会话不继承用户的整理意图 |
| `driver_project` `driver_ref` | 清空 | fork 是用户动作，不自动并入看板卡片 |
| `role` | **强制清空** | 见 §3.5 |

5. `forked_from = Some(源 id)`，**仅用于展示**，不参与任何读取路径

## 8. `yi-agent threads` 列表

纯文本输出（对齐 `yi-agent agents` 的既有风格），同目录会话按 `updated_at` 降序：

```
ID                                    标题                      更新时间
thread-0909c013-ed80-4858-822a-f52…   重构 subagent 超时处理      2026-11-01 14:22
thread-0ed0af09-a570-4bb5-aa60-ae3…   (untitled)                2026-10-31 09:15
```

两项必须处理，否则列表形同废纸：

1. **id 前缀匹配**：完整 id 没人愿意手抄。`resume` 接受唯一前缀（类 git short hash），歧义时列出候选并拒绝。
2. **标题兜底不用新写**：`first_user_text`（`thread_store.rs:1087`）已经实现「取第一条 user 文本」，`rebuild_meta` 正是用它填 `title`（`thread_store.rs:1118`）；截断长度也有 `title_from`（`thread_store.rs:1081`，30 字符）。TUI 落盘时直接调 `title_from(&first_user_text(&messages))` 即可，不引入第二套标题规则。

## 8.1 新会话的 meta 创建时机（原设计遗漏）

`resume --last` 依赖 `meta.updated_at`，但 TUI 新建会话时**根本还没有 meta**——`ThreadStore::create` 从未被 TUI 调用过。若不在启动时建 meta，`--last` 永远看不到终端里建的会话。

因此 TUI 启动（新会话路径）时必须做两步：

1. `thread_id = thread-<uuid>`，`store.create(&meta)` 写入初始 meta（`cwd`/`model`/`permission_mode`/`created_at`/`updated_at` 取当前值，其余字段用与 `rebuild_meta` 一致的默认）
2. 立刻取该 thread 的 writer lock 并持有到退出

这两步与 §6.1 的 session 重构是同一处改动，必须一起做——只建 meta 不持锁，等于给别的进程开了后门；只持锁不建 meta，`--last` 就找不到它。

**已存在的历史会话（本设计之前建的）没有 meta 也没关系**：`load` 在 meta 缺失时会走 `rebuild_meta` 从日志重建（`thread_store.rs:390`），但重建出的 `cwd` 是空串——所以 CLI 侧的 cwd 校验（§5 第 4 点）必须容忍空 `cwd`（视为「与当前 workdir 相同」），否则所有老会话都打不开。这条容易漏，测试要专门覆盖。

## 9. 错误处理

| 场景 | 行为 |
| --- | --- |
| thread 不存在 | `找不到会话 <id>`，提示 `yi-agent threads` |
| 前缀有歧义 | 列出候选 id，拒绝执行 |
| cwd 已删 | 明确报错，不回退（§5） |
| cwd 为空串（老会话重建的 meta） | 视为「当前 workdir」，放行（§8.1） |
| 锁被占 | `会话 <id> 正被另一个 yi-agent 实例使用`，提示关窗口或 `fork` |
| 锁文件 open 失败（权限/只读盘） | 报真实原因，**不得**报成「会话被占用」 |
| `.jsonl` 尾部损坏 | 沿用 `load_tolerates_truncated_final_line` 既有容错 |
| `meta.json` 损坏 | 沿用既有重建逻辑 |
| 日志含未知 `type` 行 | 跳过该行，其余照常（§11） |
| 落盘失败 | 日志 + 继续，不阻断对话 |
| `fork` 源会话不可 `load` | 报错退出，**不**建半个新会话（先 `load` 成功才写 meta） |

## 10. 测试策略

### 纯函数（无 I/O）

- `forkable_messages` 在 resume 路径上的四种边界：空会话 / 仅用户消息 / 完整轮次 / 尾部悬空 `tool_use`
- resume 与 fork 子 agent 的截断结果一致（防止规则漂移）
- `items_to_cells` 五个变体各一例，含 `ToolCall.status == Running`（崩溃残留）与 `Failed`
- 回放超过 `MAX_HISTORY_CELLS` 时，裁剪由既有 `enforce_history_cap` 生效且 `dropped_cells()` 上报非零

### 存储（app-server）

- `fork_thread` 复制完整性：items 数量、messages 内容、`forked_from`、meta 各字段取舍（含 `role` 清空、`extra_roots` 过滤）
- 锁：同一 `Arc` 二次获取幂等；跨进程持有失败（子进程验证）；锁文件在持有者退出后仍存在
- **锁的进程内去重**：同进程 `acquire_thread_lock` 两次拿到同一个 `Arc`（若退化成各自加锁，这条会红）
- **`Clear` 边界**：`read_log` 遇到 `Clear` 后只返回其后的 items/messages
- **未知 `type` 行不影响其余行**：混合日志里插入一条 `{"type":"future_thing"}`，其余照常解析
- **`/compact` 落盘**：`items: []` 的 TurnLine 不产生 cell，但覆盖 `messages`
- **meta 缺失的老会话**：`rebuild_meta` 的 `cwd` 为空串，CLI cwd 校验须放行（§8.1）

### CLI 解析

- `resume --last "prompt"` 的 positional 消歧
- `resume --last` 不带 prompt
- `resume <id> <prompt>`
- 前缀匹配：唯一命中 / 歧义 / 无命中

### 端到端（mock provider）

`run --resume` 是这条链路的唯一可自动化载体（TUI 全屏应用测不了）：

1. 建会话 → 一轮对话 → 进程退出
2. `run --resume <id>` → 断言历史被载入、能继续、新内容落盘
3. 再次 `run --resume <id>` → 断言第二轮内容也在
4. `/clear` 后退出 → `run --resume <id>` → 断言旧内容不可见（验证 §6.4 的边界语义）

## 11. 日志格式向后兼容

`TurnLine` 是 `#[serde(tag = "type", rename_all = "snake_case")]` 的带标签枚举（`thread_store.rs:211`），新增变体对**旧日志**是安全的：旧日志只有 `turn`，反序列化不会碰到未知变体。

但**新日志对旧版本不兼容**：新版本写入的 `clear` 行，旧版 `read_log` 反序列化会失败。`read_log` 现有的容错是「跳过损坏行」（`load_skips_corrupt_log_lines`），所以旧版本打开新日志不会崩，只是会**忽略 `/clear` 边界**，表现为「清空后的内容仍然可见」。

这是可接受的降级（单机 CLI，读写两端版本一致），但必须在实现时确认 `read_log` 对未知 `type` 的跳过路径确实生效，并写一条测试：**含未知变体的日志行不影响其余行解析**。

## 12. 落地顺序

按依赖倒序，每步可独立验证：

1. **锁**（`thread_store` 新增 `ThreadWriterLock` + 进程级去重表 + app-server 侧采用）—— 独立、可单测、无 UI
2. **`TurnLine::Clear` 变体 + `read_log` 边界处理** —— 为 `/clear` 铺路，纯存储层
3. **TUI 落盘**（session Arc 重构 + 新会话建 meta + 每轮 `append_turn` + `updated_at` + `/clear` `/compact` 语义）—— resume 的前提
4. **回放映射**（`items_to_cells`）—— 让历史真的看得见
5. **resume**（load → 注入 → 启动）—— 主干
6. **headless `run --resume`** —— 端到端测试载体
7. **fork**（物理复制 + meta 规则）—— 依赖 1、5
8. **`threads` 列表 + 前缀匹配** —— 纯展示

第 1-3 步不做，4-7 做出来是空转；第 4 步不做，resume 起来是空屏幕。
