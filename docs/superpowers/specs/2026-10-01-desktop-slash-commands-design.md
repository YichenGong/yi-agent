# 桌面端 Slash 命令设计

**目标：** 让桌面 App 的输入框支持 `/` 触发的 slash 命令弹窗与执行，功能对齐 TUI
的对应子集。

**状态：** 设计已确认，待实现。

**范围：** 命令集为 6 个（见 §2）。**做**：前端弹窗 + 分发，加**两个** app-server
RPC（`thread/clear` / `thread/compact`）。**不做**：TUI 的 21 个 daemon 控制命令
（`/agents` `/agent` `/events` `/diff` `/mailbox` `/message` `/pause` `/resume`
`/cancel` `/retry` `/priority` `/approve` `/deny` `/review` `/accept` `/rework`
`/reject` `/budget` `/daemon` `/runtime` `/mcp`），理由见 §7。

---

## 1. 背景

TUI 有一套 28 个命令的 slash 体系（`yi-agent-rs/crates/yi-agent/src/tui/slash.rs`，
由 `tui/app.rs::execute_slash_command` 执行）。桌面端当前完全没有 slash 支持：
`desktop/src/components/MessageInput.tsx` 把输入原样交给 `App.send()`，
以 `/` 开头的内容会被当作普通 prompt 发给 agent。

两侧的架构差异决定了"对齐"的边界：

- TUI 的 21 个 daemon 命令走**本地 Unix socket**（`crate::runtime_socket_for(workdir)`
  → daemon client），不经 app-server。
- 桌面前端只依赖 app-server 的 **JSON-RPC** 线协议（`desktop/src/lib/protocol.ts`），
  且不依赖任何 Rust 类型。要在 GUI 里跑那 21 个命令，必须先在 app-server 里新增
  一整套 daemon 代理 RPC——本次不做。

因此本次对齐的是"本地类"命令：`/help` `/cost` `/model` `/config` `/clear`
`/compact`。其中 TUI 的 `/model` 与 `/config` 本身是"暂未实现"占位
（`tui/app.rs` 直接 push「暂未实现」），桌面端改为展示真实数据，属合理增强。

## 2. 命令集

| 命令 | 参数 | 行为 | 依赖 |
|---|---|---|---|
| `/help` | `[command]` | 无参数列出全部命令；带参数显示该命令用法与描述；未知命令显示「未知命令」 | 无（纯前端，复用同一目录） |
| `/cost` | 无 | 显示当前 thread 的 token 用量与估算成本 | `Session.usage` + `lib/pricing.ts` |
| `/model` | 无 | 显示当前 thread 使用的模型 | thread info（`thread/started` / resume 回读） |
| `/config` | 无 | 调 `config/read` 展示关键配置（模型、workdir 等） | 已有 RPC |
| `/clear` | 无 | 清空该 thread 的 agent 上下文 + 截断持久化对话记录 + 清空界面 | **新 RPC** `thread/clear` |
| `/compact` | 无 | 压缩该 thread 的对话历史 | **新 RPC** `thread/compact` |

**不提供 `/quit`。** GUI 的"退出"是关窗口的系统行为，列进弹窗反而误导。

命令目录的单一事实来源是前端 `desktop/src/lib/slash.ts`：`name` / `description`
（中文，与 TUI 一致）/ `usage` / `needsArg`。`/help` 的输出由同一目录渲染，
不允许另写一份文案。

## 3. 后端（`yi-agent-app-server`）

### 3.1 为什么必须走 driver

`yi_agent_core::Agent::session()` 返回的是 `Session` 的 **clone**
（`yi-agent-core/src/agent.rs:517`），`Agent` 本身不暴露 session 的可变引用。
session 由 driver task 独占持有，所以 `/clear`、`/compact` 对 session 的修改
**只能在 driver 内完成**——这与 TUI 把 `ControlCommand::Clear|Compact` 发进
driver 的既有模式同构。

### 3.2 传输

`ThreadSession`（`yi-agent-app-server/src/session.rs:36`）新增：

```rust
pub(crate) session_tx: mpsc::Sender<SessionCommand>,
```

`SessionCommand` 形如：

```rust
enum SessionCommand {
    Clear { reply: oneshot::Sender<Result<(), String>> },
    Compact { reply: oneshot::Sender<CompactOutcome> },
}

enum CompactOutcome {
    Compacted,
    /// 无可压缩的历史（`compact_session` 返回 `None`）。
    NotReduced,
    Failed(String),
}
```

`run_thread_driver` 新增 `session_rx` 分支：`tokio::select!` 只用于选事件源，
新增一个 `session_rx.recv() => { ... }` arm 即可——与 `interrupt_rx` /
`interject_rx` 同构（两者都是 `mpsc::Receiver`）。两个 spawn 点（`thread/start`
与 `thread/resume`）都要建通道并传入。

driver 还需要 `provider`（`Arc<dyn Provider>`）与 `config`（`AgentConfig`）：
compact 要调 `yi_agent_core::compact_session(provider, config, &session)`。
两者已在 `BuiltAgent` 里（`server.rs:46`，`provider` / `config` 字段），
`run_thread_driver` 加两个参数传入即可。

### 3.3 `thread/clear`

请求：`{"method":"thread/clear","params":{"threadId":"<id>"}}`

处理（主循环）：

1. 未知 thread → `-32011`（`RpcError::unknown_thread`）。
2. `active_turn_id.is_some()` → `-32012`（`RpcError::turn_in_progress`，语义即
   "a turn is busy"，与 clear 冲突定义完全吻合，**不新增错误码**）。
3. 否则向 driver 发 `SessionCommand::Clear`，等 oneshot。

driver 侧：

1. `agent = agent.with_session(yi_agent_core::Session::new())`。
2. `ThreadStore::truncate(&thread_id)`（新增方法）：删除 `<id>.jsonl`，
   **保留** `<id>.meta.json`。thread 身份（id / 标题 / cwd / model /
   permission_mode）不变，侧栏条目仍在。
3. 回 `Ok(())`。

**为什么必须截断日志：** app-server 的 thread 就是持久化记录
（`thread_store.rs`：只追加 `.jsonl` + 可变 `.meta.json`），`thread/resume` 会
`session.replace_messages(loaded.messages)`（`server.rs:780`）。若只清内存而
不截断日志，resume 会把旧消息回放回来，AI 又"想起来"——用户以为清空了实则没有。
截断日志才能让"忘记"成立。

响应：`{"result":{}}`。

### 3.4 `thread/compact`

请求：`{"method":"thread/compact","params":{"threadId":"<id>"}}`

主循环前置校验同 §3.3 的 1–2（未知 thread `-32011`，turn 进行中 `-32012`）。

driver 侧：

```rust
let session = agent.session();
match yi_agent_core::compact_session(&provider, &config, &session).await {
    Ok(Some(s)) => { agent = agent.with_session(s); Compacted }
    Ok(None)     => { NotReduced }
    Err(e)       => { Failed(e.to_string()) }
}
```

`Ok(None)` 表示 `plan_compaction` 判定无安全缩减（历史太短），**不是错误**。
in-place 替换 session 即可，无需重建 agent（provider / tools / 权限都沿用原实例，
与 TUI 重建后语义等价，但少一次装配）。

响应：

```json
{"result": {"status": "compacted"}}   // 或 "not_reduced" / "failed"
```

`failed` 时附 `"error": "<message>"`。响应区分三态，前端据此渲染不同提示；
RPC 本身不因"无需压缩"或"压缩失败"返回 JSON-RPC error——请求被正常处理了。

### 3.5 协议文档同步

`desktop/src/lib/protocol.ts` 是 types-only 的线协议镜像。两个新方法的
请求/响应形状加进去（注释标注为文档用途），保持前端只依赖线协议。

## 4. 前端

### 4.1 新模块 `desktop/src/lib/slash.ts`

- `SlashCommandSpec`：`name` / `description` / `usage` / `needsArg`。
- `SLASH_COMMANDS: SlashCommandSpec[]`：§2 的 6 条，顺序即弹窗顺序。
- `filterCommands(prefix: string): SlashCommandSpec[]`：按 `name.startsWith(prefix)`
  过滤（与 TUI `CommandPopup::filter` 一致，前缀匹配而非模糊匹配）。
- `parseSlashInput(text): { kind: "command"; name; args } | { kind: "path" } | { kind: "none" }`：
  对齐 TUI 的路径规则——首个 token 含 **≥2 个 `/`** 时视为绝对路径输入，原样透传给
  agent（`tui/app.rs` 的既有测试 `submit_` 覆盖该行为）；否则以 `/` 开头即命令。
- `renderHelp(target?: string): string`：`/help` 输出，复用 `SLASH_COMMANDS`。

### 4.2 新组件 `desktop/src/components/SlashPopup.tsx`

输入框上方浮层，列出 `filterCommands` 结果，高亮当前选中项，展示
`/name usage` + 中文描述；空结果时不渲染。

### 4.3 `desktop/src/components/MessageInput.tsx`

弹窗状态机（对齐 TUI 的按键优先级）：

- 激活条件：文本以 `/` 开头，且光标位于首个空格之前（命令名区间）。
- `↑` / `↓`：移动选中项（循环），**不**触发输入框的历史/光标行为。
- `Tab`：把选中命令补全为 `/name `（尾部带空格），关闭弹窗。
- `Enter`：弹窗有选中项时**执行命令**，不发送消息；无匹配项时输出
  「未知命令: /x」notice，同样不发送。**例外**：文本恰为裸 `/`（尚未输入任何命令名
  字符）时，Enter 不执行**默认高亮**（目录首项即破坏性的 `/clear`），弹窗保持打开；
  但**显式**的 `↑`/`↓` 选择后 Enter 照常执行，`Tab` 始终补全。
- `Esc`：关闭弹窗，保留已输入文本。
- 输入空格：关闭弹窗（进入参数输入模式）。
- 文本不再以 `/` 开头：关闭弹窗。

IME 守卫（`lib/imeEnter.ts`）保持不变：中文输入法确认候选词的回车仍然只上屏。

### 4.4 `desktop/src/lib/session.ts`

新增纯前端的 `notice` 条目：

```ts
{ type: "notice"; id: string; text: string }
```

`Session.notice(text)` 追加一条。命令输出用 notice，**不**伪装成 agent 消息
（对齐 TUI 用 `HistoryCell::Separator` 渲染命令输出）。`Item` 是 protocol 类型，
故 notice 定义在 `session.ts` 内并让 `Session.items` 的类型为 `Item | NoticeItem`。

### 4.5 `desktop/src/components/ChatView.tsx`

`notice` 渲染为居中的弱化单行（灰字、等宽可选），与 agent/用户气泡区分。
`default` 分支的"unsupported item type"防御逻辑保持不变。

### 4.6 `desktop/src/App.tsx`

新增 `onSlashCommand(name: string, args: string | null)`：

- `help` → `session.notice(renderHelp(args))`
- `cost` → `session.notice(...)`，用 `pricing.ts` 的 `estimateCost` /
  `formatCost` 渲染 `session.usage`
- `model` → `session.notice(当前模型)`
- `config` → `config/read` → notice 渲染
- `clear` → `thread/clear`；成功则 `session.reset()` + notice「对话已清空」；
  失败则 notice 错误文本（**不**清界面，避免"看着清了、服务端没清"）
- `compact` → `thread/compact`；按 `compacted` / `not_reduced` / `failed`
  分别 notice「对话已压缩」/「无需压缩」/「压缩失败: …」

命令分发不经过 `send()`，因此不产生 user 气泡、不触发 `turn/interact` 逻辑。

## 5. 错误处理

| 场景 | 表现 |
|---|---|
| 未知命令 + Enter | notice「未知命令: /x」，不发 agent |
| `/clear`、`/compact` 时 turn 正在跑 | RPC 返回 `-32012` → notice 提示稍后再试 |
| 未知 thread | RPC 返回 `-32011` → notice 提示 |
| app-server 未初始化 | 沿用既有 `-32010` 处理路径 |
| `/compact` 压缩失败 | notice「压缩失败: …」，界面不变，连接不断 |

## 6. 测试策略

**Rust（`yi-agent-app-server`，mock provider）：**

- `thread/clear`：清空后 `agent.session().messages()` 为空；`.jsonl` 已删、
  `.meta.json` 仍在且 thread 仍出现在 `thread/list`；**关键回归**：clear 之后
  `thread/resume` 回放的消息为空（证明旧上下文不会复活）。
- `thread/compact`：三态各一例（compacted / not_reduced / failed），并断言
  compacted 后 session 消息数下降。
- 未知 thread → `-32011`；active turn 中调用 → `-32012`（两个方法各覆盖）。
- 验证：`cd yi-agent-rs && cargo test -p yi-agent-app-server`

**前端（Vitest）：**

- `src/lib/slash.test.ts`：目录完整性、前缀过滤、`parseSlashInput`（命令 / 路径
  两斜杠规则 / 普通文本）。
- `src/components/SlashPopup.test.tsx`：渲染、高亮、空结果不渲染。
- `src/components/MessageInput.test.tsx`：`/` 激活、↑↓ 选择、Tab 补全、Enter 执行
  不发送、Esc 关闭、空格关闭、未知命令提示。
- `src/lib/session.test.ts`：`notice` 追加与 `reset` 清理。
- `src/App.test.tsx`：`/clear` 发 `thread/clear` 并在成功后清空 items；`/compact`
  三态 notice 渲染；命令不触发 `turn/start`。
- 验证：`cd desktop && npx vitest run && npx tsc --noEmit && npm run build`

## 7. 非目标（明确不做）

- TUI 的 21 个 daemon 控制命令——需要 app-server 新增整套 daemon 代理 RPC，
  且桌面端当前无子 agent 任务树的消费界面，独立立项。
- `/quit`、`/runtime`、`/mcp`（MCP 在桌面路线图是 P3）。
- 命令历史（↑ 翻历史）、命令参数补全、`/config` 的写操作（只读展示）。
- 改 TUI 一侧的任何行为。

## 8. 风险

- **`thread/clear` 截断日志后 `updated_at` / 会话列表排序**：截断只删 `.jsonl`，
  不重写 `.meta.json`；thread 保留原 `updated_at`，排序位置不变。这是有意选择
  （clear 不改 thread 身份），需在实现中保持。
- **driver 忙碌判定与竞态**：`active_turn_id` 由主循环维护，判定与投递之间
  若正好有一个 turn 结束，clear/compact 会在 turn 结束后的干净 session 上执行，
  结果依然正确（只是不再被拒）。不需要额外加锁。
- **compact 是长请求**（要调一次 provider 生成摘要），期间该 thread 不能开新 turn。
  实现时对 `thread/compact` 的等待不设短超时；前端在等待期间禁用输入框发送。
