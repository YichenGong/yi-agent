# 桌面端 /fork：在会话级做分支尝试

日期：2026-11-01

## 问题

后端早已有 `ThreadStore::fork_thread`（`yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs:315`），
它会物理复制一份会话，并清掉那些「继承过来就会坏掉」的字段。但它只被 CLI 的
`yi-agent fork`（`yi-agent-rs/crates/yi-agent/src/main.rs:1415`）调用。

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

- **消息级 fork**（从某条消息截断出新会话）。它需要在 `fork_thread` 上加截断点，
  并处理 trailing `tool_use` 的配对问题（`yi-agent-core/src/agent.rs:793` 的
  `fork_prefix_len` 是已有规则，但截断点落在半轮中间时 `items` 与 `messages`
  两条流的截断口径会分叉）。留待后续单独立项。
- **fork 正在运行的会话**。明确拒绝，见「运行中即拒绝」。
- **跨目录 fork**。新会话沿用源的 cwd，不接受指定别的目录。

## 设计

### 1. 后端：新增 `thread/fork` RPC

参数 `{ threadId }`，返回 `{ thread_id, title }`。

分支放在 `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs`，与
`thread/rename` / `thread/delete` 相邻。桌面端只能走 JSON-RPC，所以必须新开这个方法。

权限档位 `Scope::Control`，与 `thread/rename` / `thread/delete` 同档
（判定写法见 `rpc/thread.rs:1605` 的 `state.client_scope < Scope::Control`）。
Control 本就能 `thread/start` + `turn/start`，单开此门不扩大能力面。

流程：

1. `require_thread_id` 解析参数（与其它 thread 方法同一入口）。
2. `store_lookup(state.threads, state.workspaces, state.cfg, &thread_id)` 定位权威
   store。**冷会话必须能 fork**——「复制一个旧会话去分支」是本功能的主要用法，
   所以不能只查 `state.threads`。
3. 存在性沿用 `thread/delete` 的 `in_memory || on_disk`
   （`rpc/thread.rs:1623`），都不在则 `-32011 unknown_thread`。
4. **运行中即拒绝。** 判据与 `power_busy::local_busy`（`power_busy.rs:65`）同一形状：
   `active_turn_id.is_some()` 或 `status != ThreadStatus::Idle`。
   命中就回 `-32012 turn_in_progress`，**在任何磁盘动作之前**返回。
5. **cwd 必须仍然存在。** 用 `thread_cwd(...)` 得到工作区，`!is_dir()` 则回
   `-32602`，文案与 `thread/resume` 的同款检查（`rpc/thread.rs:385`）一致。
   否则副本一建出来就是个打不开的会话。
6. `new_id = format!("thread-{}", uuid::Uuid::new_v4())`（`valid_id` 只允许
   alnum/`-`/`_`，UUID 带连字符，合法），调 `store.fork_thread(&thread_id, &new_id)`。
7. 返回新 id 与已落盘的标题。

**不在此分支里起 session。** 复制完只落盘；桌面端随后走既有的 `thread/resume`
把 agent 建起来，避免在 fork 分支里重写 `start_thread_core` 那一大段装配。

**锁的语义（已核实）。** `fork_thread` 先 `acquire_thread_lock(from)`。本进程已持有
该锁时（会话正开着，`ThreadSession.writer_lock` 是 `Some`），
`process_locks()` 表会返回同一个 `Arc`（`thread_store.rs:1364`），所以「fork 自己
开着的会话」不会卡死；另一个 yi-agent 进程持有时返回 `ThreadBusy`，被
`fork_thread` 包成 `io::ErrorKind::WouldBlock`（`thread_store.rs:322`）。

### 2. 后端：三种失败各用哪个错误码

| 情况 | 返回 | 说明 |
|---|---|---|
| 会话不存在 | `-32011 unknown_thread` | 与其它 thread 方法一致 |
| 本进程内该会话在跑 | `-32012 turn_in_progress` | 第 4 步判定 |
| 另一个进程持有写锁 | `-32603 internal`，message 用 `ThreadBusy` 的 `Display` | 不能复用 `-32012`：那是「本会话有 turn 在跑」，而这里是「别的实例占着」，说法不同，混用会让人去等一个根本不会结束的回合 |
| 复制 IO 失败 | `-32603 internal` | 直接 `e.to_string()` |

`ThreadBusy` 的 `Display`（`thread_store.rs:1275`）已经是中文
「会话 X 正被另一个 yi-agent 实例使用」，桌面端可以原样展示，不必翻译。

调用点不需要区分 `WouldBlock` 与其它 IO 错误：`fork_thread` 已经把 `ThreadBusy`
的中文塞进 `io::Error` 的 message，直接 `RpcError::internal(e.to_string())` 即可，
两条路径落到同一个 `-32603`。要区分只在客户端需要不同引导时才值得引入新错误码，
本轮不需要。

### 3. 后端：fork 时把附件目录一并搬过去

`fork_thread` 现在只复制 jsonl 与 meta。但历史里的图片与文档都指向源会话的附件目录：
`ContentBlock::Image.path`、`Attachment.path`、`ImageRef.path`，形如
`.yi-agent/attachments/<源threadId>/<hash>-<name>`（`attachments.rs:58` 的
`stored_relative_path`）。

而 `thread/delete` 会调 `remove_attachments`（`attachments.rs:132`）把
`<cwd>/.yi-agent/attachments/<threadId>` 整个删掉。

不处理的话：**fork 完把源会话删掉，副本里的图片全部失效**。这恰落在本功能的主要
用法上（「试完分支，旧那条不要了」），属于静默坏掉，不能接受。

在 `fork_thread` 里、`append_turn` 之前：

1. **cwd 从 store 反推**，与 `thread/delete` 同一写法
   （`rpc/thread.rs:1773`）：`self.root().parent().and_then(|p| p.parent())`。
   **不能用 `loaded.meta.cwd`**——meta 缺失时会走 `rebuild_meta`，`cwd` 是空串。
2. 源目录 `<cwd>/.yi-agent/attachments/<from>`，目标目录换 `<new_id>`。
   源目录不存在则整步跳过，不建空目录。
3. 复制时**跳过名为 `.tmp` 的子目录**——那是分片上传的暂存区
   （`image_upload.rs:58`），内容未 commit，属于半成品。只复制 `is_file()`
   的条目（与 `image/read` 拒绝 FIFO 同一条理由：不能因为一个怪异条目把 serve
   循环挂住）。复制失败按 `io::Error` 上报，不静默放过。
4. 重写路径。要覆盖的字段是全集，漏一个就会留下指向源目录的悬空引用：

   | 位置 | 字段 |
   |---|---|
   | `Item::UserMessage` | `attachments[].path`、`images[].path` |
   | `Item::UserInterjection` | `attachments[].path`、`images[].path` |
   | `Item::ToolCall` | `images[].path` |
   | `Message.content[]` | `ContentBlock::Image.path`，以及 `ContentBlock::ToolResult.content[]` 里递归出现的 `Image.path` |

   匹配方式：`path.strip_prefix(".yi-agent/attachments/<from>/")` 成功才替换，
   不能用 `starts_with` 直接拼——`<from>` 可能是另一个 id 的前缀。
   实现上分两个函数：一个走 `Item`，一个递归走 `ContentBlock`。
5. 源目录一概不动。

**实现约束（已核实）**

- 现有 `fork_thread` 里 `let loaded = self.load(from)?`（`thread_store.rs:328`）是
  不可变绑定，重写路径要改成 `let mut loaded`；`Item` 有 `Clone`
  （`protocol.rs:822` 的 derive），但直接原地改更省一次深拷贝。
- `first_user_text` 在同文件且是私有 `fn`（`thread_store.rs:1204`），
  `fork_thread` 与它同模块，可直接调用；而 `title_from` 是 `pub(crate)`，
  本模块内用也无障碍。
- 复制附件用 `std::fs::create_dir_all` + `fs::copy`，不引入 `walkdir` 之类新依赖：
  目录只有一层（`<tid>/<hash>-<name>`）加一个要跳过的 `.tmp`，手写一层遍历即可。

### 4. 后端：`(fork)` 后缀不能被截断吃掉

`fork_thread` 现在这么取标题（`thread_store.rs:334`）：

```rust
let title = loaded.meta.title.as_ref().map(|t| title_from(&format!("{t} (fork)")));
```

`title_from`（`thread_store.rs:1198`）先把空白折叠再 `chars().take(30)`。而源标题
本身通常已经是截过的 30 字——拼上 ` (fork)` 正好超长，**后缀被切掉**，副本题和原题
在侧栏里一模一样。

这不只是难看：`fork_clears_fields_that_would_break_the_desktop` 断言的
「标题要标出 fork 来源，否则列表里两条一模一样」正是这个诉求，但那条测试用的是短标题，
没覆盖到截断。

修法：先按 24 字截断源标题，再拼 ` (fork)`，总长仍在 30 以内；源无语义标题
（`title` 为 `None`，例如老会话）时退化为 `first_user_text` 再截断，不能让副本和
原件一起显示 `(untitled)`：

```rust
let base = loaded
    .meta
    .title
    .clone()
    .or_else(|| first_user_text(&loaded.messages));
let title = base.map(|t| {
    let normalized = t.split_whitespace().collect::<Vec<_>>().join(" ");
    let head: String = normalized.chars().take(24).collect();
    format!("{head} (fork)")
});
```

`first_user_text` 就在同文件（`thread_store.rs:1204`）。

### 5. 桌面端：侧栏「创建副本」

`ThreadSidebar` 的会话右键/长按菜单（`role="menu"` 的 Popover）在标签分组之后加一条
`role="menuitem"` 的「创建副本」，回调 `onFork(t.thread_id)`。新增 prop 走 `App.tsx:2501`
那一处已有的一长串 `onRename=` / `onDelete=` 旁。

`App.tsx` 里实现：

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

**不切换会话。** 从侧栏复制一份旧会话，用户的注意力不该被抢走；新条目带 `(fork)`
标题出现在列表顶部（`fork_thread` 把 `updated_at` 置为当前时刻，侧栏按它倒序），
这本身就是反馈。

这里修正先前对话里的说法：原计划给一条「已创建副本」轻提示，但桌面端**没有 toast
基础设施**（`desktop/src` 下搜不到任何 toast 组件）。为一条提示引入一套浮层体系
不成比例。侧栏条目 + 标题后缀已经足够定位，失败走既有的 `setCurrentError` 横幅。

### 6. 桌面端：`/fork` 斜杠命令

`desktop/src/lib/slash.ts`：

```ts
{ name: "fork", description: "从当前会话分叉一个新副本（需会话空闲）", usage: "[message]", needsArg: false }
```

并加进 `HELP_GROUPS` 的「会话」组。那张一致性测试要求分组拍平后与
`SLASH_COMMANDS` 不重、不漏、同序（`slash.ts:29` 的注释），漏改直接红。

`needsArg: false` 与 `/help` 一致：不强制带参数，带了就当草稿用。

**输入框的已知行为（必须在 UI 文案里兜住）**：`MessageInput` 在 turn 进行中把
Enter 交给「中断」，`/` 开头的文本因此不会执行任何命令
（`MessageInput.tsx:305` 那段的 `if (turnActive) { onInterrupt(); return; }`）。
用户在会话正在跑时敲 `/fork` 并回车，结果是**中断一轮回答**，而不是 fork。
这正是「运行中拒绝 fork」那条规则的另一半：与其让提示语出现，不如先让它不静默发生。

处理办法：`/fork` 的描述写成「从当前会话分叉一个新副本（需会话空闲）」，
并在 `/help` 卡片里与之一致。不引入新提示——那条分支是既有语义，`/clear` /
`/compact` 同样受它约束。

`App.tsx` 的 `onSlashCommand` 加 `case "fork"`：

1. 调 `thread/fork`。
2. 失败：`session.notice(...)`。`-32012` 翻译成「这个会话正在运行，跑完再 fork」；
   `-32011` 与 `-32603` 用 `formatError(e)`（后者若是「正被另一个实例使用」，
   服务端给的就是中文，原样展示即可）。
3. 成功：
   - 先 `await sess.refreshThreads()`。**`thread/fork` 不发任何通知**（不同于
     `thread/start` 会广播 `ThreadStarted`），侧栏行只能来自 `thread/listAll`。
     不先刷新就切过去，用户会短暂看到一个「侧栏里不存在」的会话。
   - 再 `await selectThread(newId)`。

     注意：`selectThread` 对新 id 是**安全的**，不必照抄 `newThread` 那条手工路径。
     新 id 不在 `syncPlan` 里、也没有缓存，`resolveSyncPlan`
     （`lib/syncPlan.ts:57`）在 `summary` 为 undefined 时判成 `full` 而非 `drop`
     ——`drop` 只在 plan 显式裁决时出现。冷 id 也没有 `warm` 标记，
     所以最终会落到 `thread/resume` 全量回放，`thread/started` 帧会带回
     cwd/model，`view.info` 由此填上。
4. 草稿规则（Q5 = B）：

   **只有一条**：`/fork` 后面的文字成为**新会话**的第一条草稿；不带文字则不写草稿。

   ```ts
   // 在第一个 await 之前就读：args 来自这次调用的参数，不受后续清空影响
   const seed = args ?? null;
   ...
   if (seed) store.setDraft(newId, seed);   // 写在 selectThread 之前
   await selectThread(newId);
   ```

   **时序是这里唯一会出错的地方。** `runSlash`（`MessageInput.tsx:163`）调用
   `onSlashCommand(name, args)` 时**不 await**，紧接着同步执行
   `onDraftChange("")`，而 `changeDraft` 写的是 `store.currentId`
   （`App.tsx:2274`）。

   于是有两条必须同时成立：

   1. `onSlashCommand` 的 `fork` 分支必须在第一次 `await` 之前就把 `args` 取到
      局部变量——`await` 期间那次同步清空随时可能发生。
   2. `store.setDraft(newId, ...)` 必须发生在 `await` **之后**（自然如此，因为它
      要用到 fork 返回的新 id）。此时 `onDraftChange("")` 已经执行完毕，清的是源
      会话；若谁把它挪到 `await` 之前，清空的就会是新会话刚写好的草稿。

   这条约束靠测试钉住：`/fork 换个思路` 之后断言新会话草稿是「换个思路」。

   用 `store.setDraft(newId, text)` 而不是 `changeDraft`：后者写的是
   `store.currentId`（此刻仍是源会话）。写在 `selectThread` **之前**，是因为输入框的
   value 直接读 `view(id).draft`（`App.tsx:2701`），预先写好才能在新会话首帧就显示。
   `store.setDraft` 会 `view(id)` 建视图，早写不会丢。

   **不搬源会话的草稿。** 这里修正了先前对话里的说法：`/fork` 是斜杠命令，源会话
   此刻的草稿**就是**「/fork 换个思路」这串命令本身，外加 `runSlash`
   （`MessageInput.tsx:163`）在命令返回后立刻 `onDraftChange("")` 把它清空。所以
   「不带参数时把源草稿搬到新会话」既没有内容可搬（那是命令文本），也与既有清理
   时序冲突。搬过去只会让新会话输入框里出现一条 `/fork`。

   也**不搬待发附件**（`pending[id]`）：附件在服务端已按源会话 id 落盘，跨会话搬运
   要再走一次落盘与重写，代价与收益不成比例。

切到新会话后，输入框里已经是要说的话，用户自己按发送。**不自动发**——连一次模型
调用的成本都不该在用户还没看清新上下文时替人决定。

### 7. 数据流

```
侧栏菜单 ┐
         ├─→ client.request("thread/fork", {threadId})
/fork ───┘            │
                      ▼  (rpc/thread.rs)
        require_thread_id → 存在性 → 在跑? → cwd 存在? → fork_thread
                      │        （复制 jsonl + meta + attachments，重写路径）
                      ▼  {thread_id, title}
              refreshThreads() → thread/listAll（新条目出现在侧栏顶）
                      │
                      ├─ 侧栏入口：到此为止
                      └─ /fork：setDraft → selectThread → thread/resume → 输入框带草稿
```

### 8. 已知限制

- **其它设备不会即时看到副本。** `thread/fork` 不发通知，手机端要等下一次
  `thread/listAll`（重连、切前台、或发消息后）才会看到这一行。要让它即时出现，
  得先解决「广播了 `ThreadStarted` 但侧栏仍不会自动加行」这半个问题，不在本轮范围。
- **卡片 worktree 会话的副本不带 worktree 归属。** `fork_thread` 把
  `driver_ref` / `driver_project` 置 `None`，所以副本删除时不会触发 worktree 回收
  ——这正是想要的（副本不该替源会话决定那个目录的去留），但它也不在卡片里。
- **组长会话的副本是普通会话。** `role` 被清空（唯一组长不变量），副本与该目录的
  组长身份无关。

## 测试

**Rust（`rpc_tests.rs`）**

- `thread/fork` 出现在「方法 → 域」映射表里，路由到 `Domain::Thread`。
  （表在 `rpc_tests.rs:3663` 一带，`domain_for` 只按前缀切分，新方法名不会自己出现。）
- 对一个已落盘的空闲会话 fork：返回新 id；`thread/load` 或 `read_meta` 能读出它，
  且 `forked_from` 指向源。
  **注意：`forked_from` 目前不在 wire 上**——`thread_summary_json_with_status`
  （`server.rs:4583`）没有这个字段，它只存在于 meta 文件里供展示。因此这条断言必须
  在服务端侧读 meta，**不能**写成「`thread/listAll` 返回里有 `forked_from`」。
- 源会话 jsonl 与 meta 字节不变（`fork` 是只读源）。
- 运行中的会话 fork 返回 `-32012`，且磁盘上**没有**多出任何 thread 目录。
- 不存在的 id 返回 `-32011`。
- 源 cwd 被删后 fork 返回 `-32602`。

**Rust（`thread_store.rs` 单测，标题）**

- 源标题 30 个汉字时，fork 标题以 ` (fork)` 结尾且总长 ≤ 30。
- 源 `title` 为 `None` 但有 user 消息时，fork 标题非空且以 ` (fork)` 结尾。

**Rust（`thread_store.rs` 单测，附件）**

- 源有附件目录时，fork 后新目录里有同名文件。
- `Item::UserMessage.attachments[].path`、`Item::UserMessage.images[].path`、
  以及 `messages` 里 `ContentBlock::Image.path` 都指向新前缀；源目录内容不变。
- `ContentBlock::ToolResult` 里嵌套的 `Image.path` 也被重写（证明递归走到底）。
- 形如 `<from>2` 的目录名不被误伤（`strip_prefix` 而非 `starts_with`）。
- `.tmp` 子目录不被复制。
- 源没有附件目录时不创建空目录，不报错。
- 附件目录的 cwd 由 `store.root()` 上溯两级得到，meta 里 `cwd` 为空（损坏重建）
  时仍能正确复制。

**TypeScript**

- `ThreadSidebar`：菜单点「创建副本」发出 `thread/fork`，且**没有**发出
  `thread/resume`（侧栏入口不切会话）。
- `App`：`/fork 换个思路` → 新会话草稿是「换个思路」，源会话草稿为空
  （`runSlash` 清掉的是命令文本本身，这是既有行为，不是这次引入的）。
- `App`：`/fork` 无参数 → 新会话草稿为空（不把 `/fork` 这串命令文本带过去），
  源会话草稿被 `runSlash` 清空。
- `App`：`thread/fork` 回 `-32012` → 会话里出现中文提示，且**没有**发
  `thread/resume`。
- `slash.test.ts`：`fork` 在目录里且落在「会话」分组。

## 文档

- `docs/project-management/yi-agent-app-server.md`：加 `thread/fork` 一条，判据是
  上面的 RPC 测试名。
- `docs/project-management/desktop.md`：加侧栏「创建副本」与 `/fork` 两条，判据是
  TS 测试名。
