# 桌面端 /fork：在会话级做分支尝试

日期：2026-11-01

## 问题

后端早已有 `ThreadStore::fork_thread`（`yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs:315`），
它会物理复制一份会话，并清掉那些「继承过来就会坏掉」的字段。但它只被 CLI 的
`yi-agent fork`（`yi-agent-rs/crates/yi-agent/src/main.rs:1406`）调用。

**桌面端没有任何入口能触达它。** 侧栏的会话菜单只有标签、分组、重命名、删除
（`desktop/src/components/ThreadSidebar.tsx:1192` 一带）；斜杠命令目录只有
`clear` / `compact` / `config` / `usage` / `help`（`desktop/src/lib/slash.ts:18`）。

于是「想在某一步换条路再试试」在桌面端只能靠新建会话再手动重述上下文，或者干脆
clone 掉原会话冒险改——两条路都要么丢上下文，要么毁掉已有路径。

## 目标

- 在桌面端能对**任意**会话（含不在前台的旧会话）做一次会话级复制，源会话不动。
- 在当前会话里敲 `/fork`，立刻分叉出去一个新的工作区继续试。
- 复制是纯读源、纯写新：源会话的 jsonl 与 meta 必须逐字节不变。

## 非目标

- **消息级 fork**（从某条消息截断出新会话）。本轮明确不做：它需要在
  `fork_thread` 上加截断点，并处理 trailing `tool_use` 的配对问题
  （`yi-agent-core/src/agent.rs:793` 的 `fork_prefix_len` 是已有规则，但截断点
  落在半轮中间时，`items` 与 `messages` 的截断口径会分叉）。留待后续单独立项。
- **fork 正在运行的会话**。明确拒绝，见下文「Q4 = A」。
- **跨目录 fork**。新会话沿用源的 cwd，不接受指定别的目录。

## 设计

### 1. 后端：新增 `thread/fork` RPC

参数 `{ threadId }`，返回 `{ thread_id, title }`。

分支放在 `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs`，与
`thread/rename` / `thread/delete` 相邻。之所以要新开这个 RPC，而不是让桌面端调
CLI：桌面端只能走 JSON-RPC。

```
"thread/fork" => {
    // Scope::Control,与 thread/rename/delete 同档。
    // Control 本就能 thread/start + turn/start,单开此门不扩大能力面。
```

流程：

1. `store_lookup(state.threads, state.workspaces, state.cfg, &thread_id)` 定位权威
   store。冷会话（不在内存里）同样能 fork——这正是「复制一个旧会话去分支」的
   主要用法，所以**不能**只查 `state.threads`。
2. 存在性判定沿用 `thread/delete` 的 `in_memory || on_disk`，都不在则
   `-32011 unknown_thread`。
3. **运行中判定（Q4 = A）**：若 `state.threads.get(&thread_id)` 的
   `active_turn_id.is_some()`，或 `status != Idle`，直接回
   `-32012 turn_in_progress`，**在创建任何文件之前**返回。
4. `new_id = format!("thread-{}", uuid::Uuid::new_v4())`，调
   `store.fork_thread(&thread_id, &new_id)`。
   - 它内部会取源的写锁。本进程已持有该锁时 flock 可重入，所以「fork 本进程正开着
     的会话」不会卡死；另一个进程持有时返回 `WouldBlock`，转成 `-32012`。
5. 返回新 id 与由 `fork_thread` 落盘的标题。

**不在此分支里起 session。** 复制完只落盘；桌面端随后 `selectThread`，走既有的
`thread/resume` 把 agent 建起来。这样「建会话」与「开会话」两条路不重复装配
`start_thread_core` 那一大段。

`thread/fork` 必须登记进 `rpc_tests.rs:3663` 那张「方法 → 域」的表。漏了就静默
变成 `method_not_found`——`domain_for` 只按前缀切分，新方法名不会自己出现。

### 2. 后端：fork 时把附件目录一并搬过去

`fork_thread` 目前只复制 jsonl 与 meta。但历史里的图片引用是
`ContentBlock::Image.path = Some(".yi-agent/attachments/<源threadId>/<hash>-<name>")`
（`server.rs:7626`），文档引用是 `Attachment.path`，同一种前缀
（`attachments.rs:58` 的 `stored_relative_path`）。

而 `remove_attachments`（`attachments.rs:132`）删的就是
`<cwd>/.yi-agent/attachments/<threadId>` 整个目录。

于是不处理的话：**fork 完把源会话删掉，副本里的图片全部失效**。这恰好落在本功能的
主要用法上（「试完分支，旧那条不要了」），属于静默坏掉，不能接受。

处理方式，在 `fork_thread` 里、`append_turn` 之前：

1. 附件源目录 `src_dir = <cwd>/.yi-agent/attachments/<from>`，目标
   `dst_dir = <cwd>/.yi-agent/attachments/<new_id>`。
2. 源目录存在则递归复制（只复制普通文件，`is_file()` 不成立的条目跳过——与
   `image/read` 拒绝 FIFO 同一条理由：不能因为一个怪异条目把整条 serve 循环挂住）。
   复制失败按 `io::Error` 上报，不静默放过：一个没有附件的新会话看似正常，等用户
   翻到那张图才发现是空白，比直接报错糟得多。
3. 把 `items` 里 `Item::UserMessage.attachments[*].path` 与
   `ContentBlock::Image { path: Some(p) }`、以及 `messages` 里
   `ContentBlock::Image` 的路径，凡前缀是 `.yi-agent/attachments/<from>/` 的，
   替换成新前缀。前缀比较用「`strip_prefix(src_prefix)` 成功才替换」，避免
   `<from>` 是 `<from>2` 这类子串误伤。
4. 源目录一概不动。

附件目录为空/不存在时整个步骤跳过，不建空目录。

### 3. 后端：`(fork)` 后缀不能被截断吃掉

`fork_thread` 现在这么取标题（`thread_store.rs:334`）：

```rust
let title = loaded.meta.title.as_ref().map(|t| title_from(&format!("{t} (fork)")));
```

`title_from`（`thread_store.rs:1198`）先把空白折叠再 `chars().take(30)`。而源标题
本身通常已经是 `title_from` 截过的 30 字——拼上 ` (fork)` 正好超长，**后缀被切掉**，
副本题和原题在侧栏里一模一样。

这不只是难看：`fork_clears_fields_that_would_break_the_desktop` 那条测试断言的
「标题要标出 fork 来源，否则列表里两条一模一样」正是这个诉求，而现有测试用的是短标题，
没覆盖到截断。

修法：先按 24 字截断源标题，再拼 ` (fork)`，总长仍在 30 以内。

```rust
let title = loaded.meta.title.as_ref().map(|t| {
    let base: String = t.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(24).collect();
    format!("{base} (fork)")
});
```

新增单测：源标题是 30 个汉字时，fork 标题以 ` (fork)` 结尾、总长 ≤ 30。

### 4. 桌面端：侧栏「创建副本」

`ThreadSidebar` 的会话右键/长按菜单（`role="menu"` 的那个 Popover）在标签分组之后
加一条 `role="menuitem"` 的「创建副本」，回调 `onFork(t.thread_id)`。

`App.tsx` 里实现 `forkThread`：

```ts
const forkThread = async (id: string) => {
  const c = clientRef.current;
  if (!c) return;
  try {
    await c.request("thread/fork", { threadId: id });
  } catch (e) {
    setCurrentError(formatError(e));
    return;
  }
  await sess.refreshThreads();
};
```

**不切换会话。** 从侧栏复制一份旧会话，用户的注意力还在会话说不上被抢走；新条目
带 `(fork)` 标题出现在列表里，就是反馈本身。

注意这里我修正了先前对话里的说法：原计划给一条「已创建副本」轻提示，但桌面端
**没有 toast 基础设施**（`desktop/src` 下搜不到任何 toast 组件）。为一条提示引入
一套浮层体系不成比例。侧栏条目 + 标题后缀已经足够定位，失败则走既有的
`setCurrentError` 横幅。

### 5. 桌面端：`/fork` 斜杠命令

`desktop/src/lib/slash.ts`：

```ts
{ name: "fork", description: "从当前会话分叉一个新副本", usage: "[message]", needsArg: false }
```

并加进 `HELP_GROUPS` 的「会话」组。那张一致性测试要求分组拍平后与
`SLASH_COMMANDS` 不重、不漏、同序，漏改直接红。

`App.tsx` 的 `onSlashCommand` 加 `case "fork"`：

1. 调 `thread/fork`。
2. 失败：`session.notice(...)`。`-32012` 翻译成「这个会话正在运行，跑完再 fork」。
   其余用 `formatError(e)`。
3. 成功：
   - 先 `await sess.refreshThreads()`。**顺序不能反**：`selectThread` 在远端客户端
     路径上要按服务端列表反查 summary，新会话不在列表里会被判成 `drop`。
   - 再 `await selectThread(newId)`，与 Q3 的 C 一致：`/fork` 是明确想换场子。
4. 草稿规则（Q5 = B）：
   - 带参数（`/fork 换个思路`）：参数写进**新会话**的草稿，源会话草稿**原样保留**。
     用户正在打的东西不能因为一次分叉就丢掉。
   - 不带参数：把源会话的草稿**搬**到新会话，源会话置空。搬而不是复制，是为了
     避免同一段文字在两个会话里各留一份、先后误发两遍。

切到新会话后，输入框里已经是要说的话，用户自己按发送。**不自动发**——连一次模型
调用的成本都不该在用户还没看清新上下文时替人决定。

### 6. 数据流

```
侧栏菜单 / ⬇
            /fork 斜杠命令
                 │
                 ▼
        client.request("thread/fork", {threadId})
                 │
                 ▼  (rpc/thread.rs)
   store_lookup → 运行中? → fork_thread（复制 jsonl + meta + attachments，重写路径）
                 │
                 ▼  {thread_id, title}
     refreshThreads()  ──→  thread/listAll（新条目出现在侧栏）
                 │
                 ├─ 侧栏入口：到此为止，不切会话
                 └─ /fork：selectThread(newId) → thread/resume → 输入框带草稿
```

## 错误处理

| 情况 | 服务端 | 桌面端呈现 |
|---|---|---|
| 会话不存在 | `-32011 unknown_thread` | `formatError` 原文 |
| 会话运行中 | `-32012 turn_in_progress` | 「这个会话正在运行，跑完再 fork」 |
| 另一进程持锁 | `thread_store` 的 `WouldBlock` → 转 `-32012` | 同上 |
| 复制 IO 失败 | `-32603 internal` | `formatError` 原文 |
| 客户端未连上 | 本地 reject | `formatError` 已有的连接类中文 |

## 测试

**Rust（`rpc_tests.rs`）**

- `thread/fork` 出现在域映射表里，路由到 `Domain::Thread`。
- 对一个已落盘的空闲会话 fork：返回新 id；`thread/listAll` 里出现该 id，且
  `forked_from` 指向源。
- 源会话 jsonl 与 meta 字节不变（`fork` 是只读源）。
- 运行中的会话 fork 返回 `-32012`，且磁盘上**没有**多出任何 thread 目录。
- 不存在的 id 返回 `-32011`。

**Rust（`thread_store.rs` 单测，标题）**

- 源标题 30 个汉字时，fork 标题以 ` (fork)` 结尾且总长 ≤ 30。

**Rust（`thread_store.rs` 单测，附件）**

- 源有附件目录时，fork 后新目录里有同名文件。
- 历史里的 `Attachment.path` 与 `ContentBlock::Image.path` 都指向新前缀。
- 源目录内容不变。
- 源没有附件目录时不创建空目录，不报错。

**TypeScript**

- `ThreadSidebar`：菜单点「创建副本」发出 `thread/fork`，且**没有**发出
  `thread/resume`。
- `App`：`/fork 换个思路` → 新会话草稿是「换个思路」，源会话草稿仍在。
- `App`：`/fork` 无参数 → 新会话草稿是源会话原来的草稿，源会话草稿为空。
- `App`：`thread/fork` 回 `-32012` → 显示中文提示，不切会话。
- `slash.test.ts`：`fork` 在目录里且落在「会话」分组。

## 文档

- `docs/project-management/yi-agent-app-server.md`：加 `thread/fork` 一条，判据是
  上面的 RPC 测试名。
- `docs/project-management/desktop.md`：加侧栏「创建副本」与 `/fork` 两条，判据是
  TS 测试名。
