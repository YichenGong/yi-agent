# 桌面 GUI P1：会话持久化 + 历史侧栏 设计

> 承接 `docs/superpowers/plans/2026-09-26-desktop-gui-design.md` §11 路线图 P1 的前半部分
> （会话持久化 `thread/list` / `resume` / `delete`、历史侧栏）。
> 路线图 P1 的后半部分（markdown 富渲染 / 代码高亮 / reasoning / 用量面板）不在本设计内。

## 1. 背景与目标

基线（P0）的 app-server 把 thread 完全放在内存里：`thread/start` 分配一个进程内递增的
`thread-{n}`，`ThreadSession` 只持有驱动任务的 channel，重启即丢失，且没有任何会话历史落盘。
CLI/TUI 同样不持久化对话（TUI 的 history 是纯渲染状态；`yi-agent-store` 是 subagent runtime
的 task 域 SQLite，与聊天历史无关）。

**目标**：让 thread 跨 app-server 重启存活；GUI 提供历史侧栏，可以列出、恢复（并**继续对话**）、
重命名、删除历史 thread。

**成功判据**：
- 关闭并重启 app，侧栏仍列出之前的 thread；点击可恢复完整历史。
- 恢复后继续发消息，agent 带着前文上下文回答（不是失忆的新会话）。
- 重命名后重启仍是新标题；删除后该 thread 从列表与磁盘消失。
- 未知 thread id 返回 `-32011`，UI 显示错误而非崩溃。
- `cargo test -p yi-agent-app-server` 与 `desktop` 前端单测全绿；现有 CLI e2e 不回归。

## 2. 范围边界

**做什么：**
- 会话历史落盘（每 thread 一日志 + 一元数据文件）
- 持久化 thread id（跨重启稳定）
- app-server 新方法：`thread/list`、`thread/resume`、`thread/rename`、`thread/delete`；
  `thread/start` 改为分配持久 id 并写元数据
- resume 时恢复 agent 上下文（核心 `Message` 快照 → `Agent::with_session`）并回放 UI
- 前端历史侧栏：列表 / 恢复 / 内联重命名 / 删除 / New thread / 当前高亮

**不做什么（YAGNI / 延后）：**
- 不做搜索 / 过滤
- 不做 thread fork / 分支
- 不做 markdown 富渲染、代码高亮、reasoning、用量成本面板（P1 后半，单独排期）
- 不做跨 workdir 的全局历史（本设计按 workdir 隔离）
- 不做历史分页 / 懒加载（列表一次性返回）
- 不做多窗口共享（P3）
- 不迁移已有 `yi-agent-store` 的 SQLite schema

## 3. 关键决策

| 决策 | 选择 | 理由 |
|---|---|---|
| resume 语义 | **能继续对话** | 需持久化核心 `Message` 并用 `with_session` 恢复上下文 |
| 存储位置 | **按 workdir** `<workdir>/.yi-agent/threads/` | 与既有 `<workdir>/.yi-agent/runtime/` 同级；GUI 下 workdir=home，等价全局 |
| 文件形状 | **每 thread 两个文件**（`.jsonl` 日志 + `.meta.json` 元数据） | 单一职责、单写者；重命名只重写 tiny meta；无全局索引的失同步/原子写问题 |
| 持久化粒度 | **每 turn 一条记录**（最终 Item，不存 delta） | 日志紧凑；回放时前端按 item upsert 重建状态 |
| 回放方式 | **逐条 `item/completed`** | 复用前端既有 upsert 逻辑，零新前端代码路径 |
| 落点 | 新模块 `yi-agent-app-server/src/thread_store.rs` | 仅 app-server 使用；刻意不复用 `yi-agent-store` 的 task schema |
| thread id | `thread-<uuid>` | 跨重启稳定、无碰撞 |
| 删除活跃 thread | 先中断当前 turn → 从内存移除 → 删两个文件 | 最少惊讶；前端删当前 thread 后回到"无 thread"态 |

## 4. 存储设计

### 4.1 目录与文件

```
<workdir>/.yi-agent/threads/
  <thread_id>.jsonl       # 只追加：每 turn 一条记录
  <thread_id>.meta.json   # 可变：thread 元数据
```

`<thread_id>` 形如 `thread-<uuid>`。目录按需创建（首次写入时 `create_dir_all`）。

### 4.2 `.meta.json`（可变，整体重写）

```json
{
  "thread_id": "thread-0192...",
  "cwd": "/Users/x",
  "model": "claude-...",
  "created_at": 1758880000000,
  "updated_at": 1758880100000,
  "title": "帮我重构 config 模块"
}
```

- `thread/start` 时写入，`title` 暂为 `null`。
- 每 turn 完成时更新 `updated_at`；若 `title` 仍为 `null`，用本轮首条 userMessage 前 N 字填充。
- `thread/rename` 只重写 `title` + `updated_at`。
- 写入用 **temp 文件 + rename** 原子替换，避免半写状态。

### 4.3 `.jsonl`（只追加）

每 turn 完成追加一行：

```json
{"type":"turn","items":[ ...本轮最终 Item... ],"usage":{"model":"...","input_tokens":123,"output_tokens":45},"messages":[ ...核心 Message 快照... ]}
```

- `items`：本轮**最终** Item（不记录 `item/delta`、不记录 `item/started` 中间态）。
  同一 thread 内 item id 唯一，按序拼接即为完整历史。
- `messages`：本轮结束时的核心 `Message` 列表（`Agent::session().messages()`），
  供 resume 时 `Agent::with_session` 恢复上下文。取**最后一条**记录即可，无需拼接。
- `usage`：本轮 `AgentEvent::Usage`（恢复时回放最近一次用量）。

### 4.4 容错

- 行 JSON 解析失败（崩溃可能截断最后一行）→ **跳过该行** + stderr 记日志，继续加载。
- `.meta.json` 缺失或损坏 → 从 `.jsonl` 重建最小 meta（`title` 回退、`created_at` 用文件 mtime），
  不报错。
- `threads/` 不存在或不可读 → 视为空列表。
- `thread/list` 忽略"有 `.jsonl` 但无 `.meta.json`"的孤立文件（记录日志）。

## 5. 协议设计

### 5.1 方法清单（新增/改动）

| 方法 | 参数 | 结果 | 错误 |
|---|---|---|---|
| `thread/list` | `{}` | `{threads:[ThreadSummary]}` | — |
| `thread/resume` | `{threadId}` | `{thread_id,cwd,model}` | `-32011` 未知 |
| `thread/rename` | `{threadId,title}` | `{}` | `-32011` 未知 / `-32602` title 空 |
| `thread/delete` | `{threadId}` | `{}` | `-32011` 未知 |
| `thread/start`（改） | `{}` | `{thread_id,cwd,model}` | 不变 |

`ThreadSummary = {thread_id, cwd, model, created_at, updated_at, title}`，
按 `updated_at` 降序排列。

错误码沿用现有约定（`protocol.rs`）：`-32010` 未初始化、`-32011` 未知 thread、
`-32012` turn 进行中、`-32602` 参数非法。

### 5.2 resume 回放序列

`thread/resume` 的 server 行为（依次 emit 通知，最后返回结果）：

1. `thread/started`（`{thread_id,cwd,model}`）
2. 存储的每个 item 一条 `item/completed`（按历史顺序）
3. 最近一次 `thread/tokenUsage/updated`
4. 返回 `{thread_id,cwd,model}`

同时用载入的 `messages` 构建 agent（`Agent::with_session`），使恢复的会话可继续对话。

> **实现时须先核实**：基线 server 是否 emit `userMessage` item（App 端当前是乐观插入用户气泡的）。
> 回放只忠实重放"持久化下来的 item"，因此只要写入端与前端所见一致，就不会出现用户消息双份；
> 但实现第一步必须读 `translate.rs` / `server.rs` 确认这一点，并在测试中锁定。

### 5.3 delete 语义

- 目标 thread 未在内存：直接删两个文件。
- 目标 thread 活跃：先中断其当前 turn（复用现有 interrupt 通道），从 `threads` map 移除，
  再删文件。
- 文件不存在但内存有：仍从内存移除（幂等）。

## 6. app-server 内部设计

### 6.1 新模块 `thread_store.rs`

职责：纯存储层，不依赖协议/agent 类型之外的业务逻辑。

```
pub struct ThreadStore { root: PathBuf }              // root = <workdir>/.yi-agent/threads
impl ThreadStore {
    pub fn new(workdir: &Path) -> Self
    pub fn create(&self, meta: &ThreadMeta) -> io::Result<()>          // 写 meta
    pub fn append_turn(&self, id: &str, turn: &TurnRecord) -> io::Result<()>
    pub fn load(&self, id: &str) -> io::Result<Option<LoadedThread>>   // meta + items + messages + usage
    pub fn list(&self) -> io::Result<Vec<ThreadMeta>>                  // 读所有 *.meta.json，按 updated_at 降序
    pub fn rename(&self, id: &str, title: &str) -> io::Result<bool>    // 只重写 meta
    pub fn delete(&self, id: &str) -> io::Result<bool>                 // 删两个文件
}
```

`ThreadMeta` / `TurnRecord` / `LoadedThread` 为 serde 类型；`LoadedThread` 里的
`messages` 用 `yi_agent_core::message::Message`，`items` 用协议 `Item`。

### 6.2 `server.rs` 改动

- `thread/start`：生成 `thread-<uuid>`，`ThreadStore::create` 写 meta，其余流程不变。
- 新增三个 handler（`thread/list` / `thread/resume` / `thread/rename` / `thread/delete`），
  在现有 `match method.as_str()` 分派里注册。
- turn 完成时（driver 收到 `AgentEvent::Done` / `Cancelled` 后）：
  1. 收集本轮 Item 与 `Agent::session().messages()`
  2. `ThreadStore::append_turn`
  3. 更新 meta 的 `updated_at`（必要时填 `title`）
- 每 thread 的驱动任务需要持有 `ThreadStore`（或共享 `Arc<ThreadStore>`）。

### 6.3 agent 恢复

`thread/resume` 构造 agent 时走 `bootstrap_agent` 的变体，接受一个可选 `Session`：

```
build_agent(workdir, model, session: Option<Session>)
  → 若 Some(s) 则 Agent::with_session(s)，否则新建
```

（基线 `bootstrap_agent` 在 `server.rs` 中为纯工厂；改造点在此。）

## 7. 前端设计

### 7.1 组件

- **`ThreadSidebar.tsx`**（新）：props `{threads, currentId, onSelect, onRename, onDelete, onNew, busy}`。
  列表项显示 `title ?? 首条消息截断` + 相对时间；当前项高亮；双击进入内联重命名输入；
  每项一个删除按钮；顶部一个 "New thread"。
- **`App.tsx`**：新增 `threads` 状态；挂载时 `thread/list` 填充；`resumeThread(id)` 切换；
  turn 完成后刷新列表（标题/时间随之更新）；重命名/删除/新建后刷新。
- **`session.ts`**：新增 `reset()`，清空 `items` / `turnActive` / `lastStatus` / `lastError` / `usage`。
- **布局**：`App` 外层改为 `flex-row`，左侧 `ThreadSidebar`，右侧保持现有 `StatusBar + ChatView + MessageInput` 主列。

### 7.2 竞态与并发

- **回放通知可能先于 `thread/resume` 响应到达**。因此必须**在发出 resume 请求之前同步调用
  `session.reset()`**，否则回放先应用、再被 reset 清掉（或留下旧 thread 残留）。
- 若 resume 失败（`-32011`）：会话已清空 —— 接受，并显示错误横幅。
- **`turnActive` 期间禁用侧栏切换/删除**、**切换 thread 不并发（一次一个）** —— **已废弃**：
  此两条约束属于本设计（P1 单会话）的权宜之计；已由后续「并行多 thread + 每 thread 状态」特性
  取代——每 thread 各自持有 session / status，通知按 `thread_id` 路由到对应
  `ThreadView`（`desktop/src/lib/threadStore.ts`），切换 thread 不再打断后台 turn、允许并行
  turn。详见设计 `docs/plans/2026-09-28-parallel-threads-status-design.md`、模块文档
  `docs/project-management/desktop.md`（「多 thread 标签页」「每 thread 状态徽标」）
  与 `docs/project-management/yi-agent-app-server.md`（「每 thread 实时状态」）。

### 7.3 交互细节

- New thread：`session.reset()` + `thread/start` + 刷新列表。
- 删除当前 thread：删除成功后 `session.reset()` 并回到"无 thread"态（提示新建）。
- 重命名：乐观更新本地列表，失败则回滚 + 错误横幅。

## 8. 错误处理

| 场景 | 行为 |
|---|---|
| 未知 thread id | server `-32011`；前端错误横幅 |
| title 为空 | server `-32602`；前端不提交 |
| JSONL 行损坏 | 跳过 + stderr 日志，继续加载 |
| `.meta.json` 损坏/缺失 | 从 `.jsonl` 重建最小 meta |
| `threads/` 不存在 | 空列表 |
| 磁盘写失败 | server 返回错误；turn 本身不因此失败（持久化是尽力而为，记 stderr） |

## 9. 测试策略

- **Rust 单测**（`thread_store.rs`）：读写往返、`list` 按 `updated_at` 降序、`rename` 只重写 meta、
  `delete` 删两文件、损坏行容错、meta 缺失重建、title 回退。
- **Rust app-server 层测**：`thread/list` 空/非空、`thread/resume` 回放序列与 `-32011`、
  `thread/rename` 校验、`thread/delete` 幂等、turn 完成后落盘、resume 后 agent 带上下文
  （沿用现有 69 测试的 harness）。
- **前端单测**：`session.reset()`；`ThreadSidebar` 无 jsdom → 沿用基线策略（`tsc` + `vite build`）。
- **文档**：更新 `docs/project-management/yi-agent-app-server.md`（新方法）、
  `docs/project-management/desktop.md`（侧栏）、`README.md` 计数 —— 按 CLAUDE.md 同一 commit 提交。

## 10. 风险与缓解

| 风险 | 缓解 |
|---|---|
| 回放与乐观用户气泡重复 | 实现第一步核实 server 是否 emit userMessage item，并加针对性测试 |
| resume 的 reset 时序竞态 | 请求前同步 reset；测试覆盖"回放先到"的顺序 |
| 追加日志在崩溃时截断最后一行 | 逐行解析 + 跳过损坏行 |
| 每 turn 追加 `messages` 使文件变大 | 可接受；必要时后续做压缩（YAGNI，暂不做） |
| 活跃 thread 被删除后驱动任务悬挂 | 删除前显式中断并 drop 任务句柄 |
