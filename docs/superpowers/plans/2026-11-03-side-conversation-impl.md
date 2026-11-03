# 三端临时旁问 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 按[设计规格](../specs/2026-11-03-side-conversation-design.md)实现桌面、手机和原生 TUI 的 `/side` 与 `/btw` 临时只读多轮旁问，不打断、污染或持久化进父任务。

**Architecture:** core 提供安全历史快照与可终止的受管任务树；tools 提供基于文件句柄的动态只读访问；runtime 负责独立运行、预算、授权、资源所有权和有界转录。app-server 适配父生命周期及 RPC/分页，桌面与手机共享临时 store 和对账控制器；TUI 直接调用共享 runtime，不改成 RPC 客户端。三个交付面共用同一安全和状态语义，最后联合验收，不能仅完成桌面即宣布交付。

**Status:** implementation plan 草案经书面核查；用户已要求进入计划阶段，尚未实施。书面核查不代替代码评审或实际测试。

**Tech Stack:** Rust workspace（edition 2024、rust-version 1.89）、Tokio、tokio-util、futures、serde、现有 libc；React 19、TypeScript 6、Vitest、ratatui；mock provider、tempfile、wiremock。不新增 crate，不调用真实 LLM 作为默认验收。

## Global Constraints

- 设计基线：`docs/superpowers/specs/2026-11-03-side-conversation-design.md`，初始调研代码 `1b97509c`，文档合入前与 main `810d789c` 复核；新增的严格恢复入口仍会提升 partial，Task10 必须保持独立无副作用读取。本文所有新接口、测试均为**待实施**，不是已存在/已通过声明。
- 文档编写 worktree：`.worktrees/side-conversation-spec`，分支 `docs/side-conversation-spec`。本文编写阶段仅产出文档，不自动开始功能实施；文档提交/合并及看板入队按用户后续明确指令执行。正式实施由用户执行指令或正式看板任务授权，须另建功能 worktree（手工执行用 `feat/side-conversation`；看板执行沿用其分配的 `kanban/*` worktree），从包含已评审文档的 main 起步，不复用或删除其他会话的 worktree。
- 每父每宿主最多一个旁支；每宿主最多 **8** 个资源槽，包含 Creating/Closing/隔离清理。桌面/手机同宿主共享，TUI 宿主独立。
- Creating **30 秒**；Idle/Failed **30 分钟**；turn **10 分钟**；停止 **5 秒**后转实际 core abort/join，不承诺所有 OS blocking IO 同时结束。
- 文件副本与内嵌 decoded binary 合计 **64 MiB/side**；core 文本和可见 transcript 分别 **16 MiB/side**；预留 **4 KiB** 控制空间。不能自动截历史/compact。
- 受管事件 **256 项/1 MiB**，单事件 **128 KiB**；可见 item **128 KiB**；完整 RPC 信封 **512 KiB**；通知合并最多 **50ms**，终态前 flush。
- blocking 工作每宿主 **8**、每 side **2**，含已停止未结束者；本文把等待队列也明确限定为每宿主 **8**、每 side **2**，满即 resource_limit，不增加无限后台队列。
- readItems token 滑动 **60 秒**、绝对 **5 分钟**、每连接/side **1**、宿主 **8**，保留版本合计 **128 MiB**；客户端通知缓存 **256 帧/2 MiB**；前台水位轮询 **5 秒**，不续空闲 TTL。
- clientRequestId 规范 UUID、**36 ASCII 字符**；问题非空且原 UTF-8 字节最多 **32 KiB**；每 side **256** 个受理 id；tombstone **5 分钟/1024 份**且不含正文。
- 只允许 builtin `read/read_document/view_image/glob/grep/web_fetch/web_search` 的实际启用子集，`Builtin && read_only` 双校验；YOLO 不扩大能力，无审批、shell、MCP、plugin、skill、process、agent 或写工具。
- `/btw` 是隐藏别名；`/back` 保留运行，`/side-close` 丢弃；空旁支不请求模型，拒嵌套/新附件/旁支插话。键入旁问且有待发附件整次拒绝，独立按钮不消费父草稿。
- 手机普通运行中输入仍 Stop-only，只有精确旁问命令/独立按钮例外，Enter 始终换行。TUI 父审批态 `/` 进入独立命令编辑，Enter 不批准。
- 新字段 camelCase 请求、snake_case 响应；scope 沿用 Observe < Control < Admin，不虚构工作区 ACL。每条连接独立握手保存 capability 与 sideHostInstanceId。
- 普通 run/run_blocks、thread/fork、父持久化与默认工具行为保持兼容；只为本功能抽必要公共原语。不增加新持久 side thread、正文日志或普通缓存。
- 每次实施提交前执行 `cargo fmt --all`、`just ci` 全绿；合并结果再跑 `just ci`。前端另跑 tsc/Vitest/build。门禁以实际 justfile 为准，不写死数量。

---

## 0. 阅读地图、依赖与执行纪律

### 0.1 文件与职责

路径均相对仓库根；下文 `core/`、`tools/`、`runtime/`、`server/`、`cli/` 分别简写 `yi-agent-rs/crates/yi-agent-{core,tools,runtime,app-server,}/`，其中 `cli/` 是 `yi-agent-rs/crates/yi-agent/`，不是新目录。Files 块一律用完整实际路径。

| 单元 | 新文件 | 现有接入点 | 职责 |
| --- | --- | --- | --- |
| core | `src/side_context.rs`, `src/managed_run.rs`, `src/run_budget.rs` | `agent.rs`, `lib.rs` | 安全前缀、边界、真实 task tree、有界生产 |
| tools | `src/side_read/{mod,unix,blocking,file_tools}.rs` | `lib.rs`, `fs/read_document.rs`, `image_prep.rs` | 动态授权、目录句柄、bounded blocking、侧用五种文件工具 |
| runtime | `src/side_conversation.rs` 与 `src/side_conversation/{source,policy,ledger,resources,runner,transcript,test_support}.rs` | `bootstrap.rs`, `lib.rs`, `Cargo.toml` | 生命周期类型、有效来源、只读装配、目录 lease、共享 runner/转录 |
| app-server | `src/side_conversation.rs`, `src/side_conversation/{source,registry,replay}.rs`, `src/rpc/side.rs`, `src/rpc/side_methods_baseline.txt`, `src/server/tests/side.rs` | `session.rs`, `thread_store.rs`, `protocol.rs`, `broadcast.rs`, `rpc/mod.rs`, `server/transport/mod.rs`, `server/assembly.rs`, `server/turn/command.rs`, `server/runtime_binding.rs`, `server.rs`, `lib.rs` | 活跃/冷父来源、注册表、协议/订阅/分页 |
| 前端 | `lib/sideConversation.ts`, `lib/sideSync.ts`, `lib/sideCommands.ts` 及测试，`hooks/useSideConversation.ts`，`components/SideConversationHeader.tsx` 及测试 | `App.tsx`, `lib/slash.ts`, `hooks/useNotificationRouter.ts`, `hooks/useHandshakeRecovery.ts`, `hooks/connectionLifecycle.ts`, `lib/protocol.ts`, `components/MessageInput.tsx` | 连接归属、独立 reducer、对账、定向输入、两端 UI |
| TUI | `src/tui/side.rs`, `src/side_driver.rs` | `main.rs`, `tui/{mod,app,slash,replay,title}.rs`, `tui/notify/mod.rs`, `tests/e2e_resume.rs` | 独立调度、双视图、审批模式、按键与日志隔离 |

新增文件先与最近的 `mod.rs/lib.rs` 一起登记；不创建不存在的 `runtime/src/tool_setup.rs` 或 `desktop/src/lib/slashCommands.ts`。不为了减少路径数量把所有逻辑堆进 App/main/agent。

### 0.2 可评审交付与依赖

```text
01 snapshot ──02 managed tree──03 production budgets
04 safe handles──05 blocking/parsers/tools
01+03+05──06 effective source/policy──07 resources──08 runner──09 bounded transcript
06──10 parent source/cold read
08+09+10──11 registry/barriers──12 wire/domain/scope──13 replay/broadcast
12──14 client host/store──15 reconciliation──16 command policy──17 desktop/mobile integration
08+09+10(仅来源契约，不依赖app实现)──18 TUI driver──19 TUI views/approval──20 TUI keys/persistence
13+17+20──21 joint acceptance/documentation/gates
```

01–09 为共享底座；10–13 为服务端；14–17 为桌面/手机；18–20 为 TUI，可在共享底座稳定后独立推进。若分会话执行，可把这四组作为独立执行批次，仍由本文作为唯一依赖和验收入口；不能提前把 capability 暴露给未完成的服务端。

### 0.3 测试与提交约定

- 以下 Rust 命令默认 `cd <实施worktree>/yi-agent-rs`；前端默认 `cd <实施worktree>/desktop`。准备 PATH：`export PATH="/opt/homebrew/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:$PATH"`。
- 各 Task 的 red 是先写测试再运行；尚无符号时编译失败只是第一步，补最小可编译骨架后还须看到**目标断言失败**，不能把路径错误/0 tests 当作有效 red。green 输出必须实际包含命名测试且数量非零。
- 时间测试在涉及的 core/tools/runtime/app-server/cli 的 `[dev-dependencies] tokio` 追加 `features = ["test-util"]`（保留 workspace 原 features），使用 `#[tokio::test(start_paused = true)]`、`advance` 与 Notify/Barrier；不靠真实长 sleep。
- shared fixture 是本计划明确要创建的代码，不是假设已有测试框架。新的 fixture 只能使用临时目录、mock provider 与注入时钟/工厂；不读真实 key，不启动 launchd/MCP，不修改进程级 YI_AGENT_HOME 来隔离并行测试。
- **提交门 G：** Task 完成先审 diff；生成的 bindings 先生成、审核，再与本任务源码一起 `git add <明确文件>`（使 bindings-check 比较索引时不因预期生成变更误红）；`cargo fmt --all` 后重新 stage 格式变化，`just ci` 必须全绿，涉及前端再跑 `npx tsc --noEmit && npx vitest run && npm run build`，然后 `git diff --cached --check && git commit -m '<该Task消息>'`。任何门禁失败不提交。每个 Task 的最后一步都调用 G；若为了避免多次全量门禁合并提交，只能在同一评审批次末调用 G，并把尚未提交的任务列清楚，绝不能省门禁。
- 不并发启动多个共享 target 的 Cargo 编译；不清 target 来“解锁”；若卡住先区分编译锁与测试运行，并按 AGENTS.md 定位。

---

## Phase A：共享安全底座

### Task 01：独立上下文与不可覆盖的历史边界

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-core/src/side_context.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/lib.rs`
- Test: 新文件内 `tests`；复用 `agent.rs::forkable_messages`，不改 fork 语义。

**Interfaces:**
- Consumes: `forkable_messages(&Session) -> Vec<Message>`、`Session::from_messages(Vec<Message>) -> Session`。
- Produces: `SideContextSnapshot { pub messages: Vec<Message> }`、`SideContextError::EmptyHistory`；`snapshot_for_side(session: &Session) -> Result<SideContextSnapshot, SideContextError>`；`side_system_prompt(parent: Option<&str>) -> String`。隐藏边界显式构造 `Message { role: Role::User, content: vec![ContentBlock::Text(SIDE_BOUNDARY.to_owned())] }`，仅在安全前缀非空后追加，UI 不读取继承 messages。

- [ ] 写最小失败测试，另加未配对 ToolUse/完整配对/仅压缩摘要/ThinkingToken 三种凭证逐字相等用例。

```rust
#[test]
fn side_snapshot_is_independent_and_marks_history_reference_only() {
    let parent = vec![Message { role: Role::User,
        content: vec![ContentBlock::Text("完成主任务".into())] }];
    let mut side = snapshot_for_side(&Session::from_messages(parent.clone())).unwrap();
    assert_eq!(side.messages[0], parent[0]);
    assert!(matches!(&side.messages.last().unwrap().content[0],
        ContentBlock::Text(s) if s.contains("仅是参考")));
    side.messages[0].content.clear();
    assert!(!parent[0].content.is_empty());
}
```

- [ ] Red：`cargo test -p yi-agent-core --lib side_snapshot_ -- --nocapture`，确认缺边界/独立性断言失败。
- [ ] 实现唯一历史裁剪入口，保留 Role/ThinkingToken/ImageSource；边界原文采用 spec §5.2，系统策略追加在父 system 末尾，不覆盖项目规则。

```rust
let mut messages = forkable_messages(session);
if messages.is_empty() { return Err(SideContextError::EmptyHistory); }
messages.push(Message { role: Role::User,
    content: vec![ContentBlock::Text(SIDE_BOUNDARY.to_owned())] });
Ok(SideContextSnapshot { messages })
```

- [ ] Green：上述命令与 `cargo test -p yi-agent-core --lib fork_prefix_`；记录父序列化字节不变、新 `Arc<Mutex<Session>>` 与父 ptr 不同（Arc 在 Task 06 装配测试再验证）。
- [ ] G；提交 `feat: add independent side context snapshots`。

### Task 02：受管运行取得真实 core task tree

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-core/src/managed_run.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/{agent.rs,lib.rs}`
- Modify: `yi-agent-rs/crates/yi-agent-core/Cargo.toml`
- Test: `agent.rs` 现有 tests（可直接用 private ScriptedProvider）、`managed_run.rs` tests。

**Interfaces:**
- Consumes: Task 01 的新 Session；现 `Agent::run_blocks`。
- Produces: `Agent::run_managed_blocks(&mut self, blocks: Vec<ContentBlock>, limits: ManagedRunLimits) -> Result<ManagedAgentRun, AgentError>`；`ManagedAgentRun { pub events: BoxStream<'static, BudgetedAgentEvent>, pub control: ManagedRunControl }`。`BudgetedAgentEvent::event(&self) -> &AgentEvent`。`ManagedRunControl` 持实际 root JoinHandle、child task tracker、本轮 cancel token；提供 `cancel(&self)`、`async fn stop_and_join(&mut self, grace: Duration) -> Result<(), JoinError>`、`is_finished(&self) -> bool`。JoinError 使用 tokio 类型。Task 03 补完整预算 sender；本任务在managed_run.rs定义 `ManagedRunLimits { pub event_count: usize, pub event_bytes: usize, pub max_event_bytes: usize, pub text_bytes: usize, pub binary_bytes: usize, pub control_reserve: usize }` 与 `side_defaults()`（取全局约束），先落实事件项数/字节有界。另产出 `Agent::run_tracked_blocks(&mut self, blocks: Vec<ContentBlock>) -> Result<TrackedAgentRun, AgentError>`，`TrackedAgentRun { pub events: BoxStream<'static,AgentEvent>, pub control: ManagedRunControl }`：仅返回相同任务树所有权，保留普通运行的事件/正文策略，供Task20主TUI退出时join；不把side配额套主会话，普通run/run_blocks本身不变。

- [ ] 在 agent 原 tests 写失败测试：ScriptedProvider 第一个流用 `stream::pending()`，用 Notify 确认 call 已进入；保存 run 前 token，启动 managed 后 cancel 旧 token 不结束新轮，再 stop_and_join；第二个 mock 工具运行期发 ToolEvent，记录 forwarder Drop。给该 fixture 添加 `entered: Arc<Notify>`、`dropped: Arc<AtomicBool>`，Drop guard 放在**provider stream/forwarder内部**，不是消费者。

```rust
// 在 agent.rs::tests，使用该模块已有 scripted-provider/Agent 构造辅助。
// 新辅助的精确接口：async fn managed_pending_agent()
//   -> (Agent, Arc<Notify>, Arc<AtomicBool>);
#[tokio::test(start_paused = true)]
async fn side_managed_close_joins_actual_core_task() {
    let (mut agent, entered, dropped) = managed_pending_agent().await;
    let mut run = agent.run_managed_blocks(
        vec![ContentBlock::Text("旁问".into())], ManagedRunLimits::side_defaults())
        .await.unwrap();
    entered.notified().await;
    run.control.stop_and_join(Duration::from_secs(5)).await.unwrap();
    assert!(run.control.is_finished());
    assert!(dropped.load(Ordering::SeqCst));
}
```

- [ ] Red：`cargo test -p yi-agent-core --lib side_managed_ -- --nocapture`；用“只 abort 消费者”的临时最小实现验证测试确会失败。
- [ ] 将 start_run 构造拆成内部共用函数；managed 保存 run_loop JoinHandle，token 在 run 重置后返回；ToolEvent forwarder 注册进 tracker，root 取消/abort 后逐个 abort/join。实际工具 future 仍由原批次聚合，不虚构每工具 spawn。Control Drop 同步 cancel+abort 所有 tracked handles，显式 stop_and_join 才给完成证据；不在 Drop 中 detach 清理异步任务。

```rust
self.cancel.cancel();
if tokio::time::timeout(grace, &mut self.root).await.is_err() {
    self.root.abort();
    let _ = (&mut self.root).await;
}
self.children.abort_all();
while self.children.join_next().await.is_some() {}
```

- [ ] Green：managed close/旧token/forwarder/pending provider tests；`cargo test -p yi-agent-core --lib` 确认普通 run stream Drop 的兼容行为未被顺带改变。
- [ ] G；提交 `feat: track managed agent task lifetimes`。

### Task 03：生产端预算、共享 payload 与取消可达性

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-core/src/run_budget.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/{agent.rs,provider.rs,managed_run.rs,lib.rs}`
- Test: `run_budget.rs` tests、`agent.rs` tests。

**Interfaces:**
- Consumes: Task 02 `ManagedAgentRun/ManagedRunControl`。
- Produces: 消费Task02定义的 `ManagedRunLimits`，不重复定义；`RunBudget::new(limits: ManagedRunLimits) -> Self`、`reserve_text(&self, bytes: usize) -> Result<BudgetLease, BudgetExceeded>`、`reserve_binary(&self, bytes: usize) -> Result<BudgetLease, BudgetExceeded>`；`BudgetLease` Drop 释放，`BudgetExceeded` 实现 Error；Task 02 的 `BudgetedAgentEvent` 持共享 `Arc<AgentEvent>` 和资源 lease，克隆不重复收正文预算。`BudgetedEventSender::try_send(&self, event: AgentEvent) -> Result<(), BudgetExceeded>` 超限主动 cancel，不依赖调用点处理错误。`measure_side_history(session:&Session, limits:&ManagedRunLimits)->Result<SideHistorySize,BudgetExceeded>`，`SideHistorySize { pub text_bytes:usize, pub binary_bytes:usize }`：仅安全前缀，serde counting writer计算正文序列化开销，不分配整份JSON；inline source独立解码长度验证/计数，不把base64再次算正文。先做编码长度有界预检再校验解码，拒非法来源；边界/后续输入同样计入预算。

- [ ] 写字节/项数/控制保留空间测试，测试原串 Unicode 转义的序列化开销；provider mock 连续 ToolInput delta、ThinkingToken、大图片，消费者不读，核心内存不得无限增长。加入正常消费者/consumer Drop/backpressure取消用例。

```rust
#[test]
fn side_text_budget_rejects_before_accumulation() {
    let mut limits = ManagedRunLimits::side_defaults();
    limits.text_bytes = 8; limits.control_reserve = 0;
    let budget = RunBudget::new(limits);
    let held = budget.reserve_text(8).unwrap();
    assert!(budget.reserve_text(1).is_err());
    drop(held);
    assert!(budget.reserve_text(8).is_ok());
}
```

- [ ] Red：`cargo test -p yi-agent-core --lib side_ -- --nocapture`，确认爆量/消费者退出测试失败而非卡死。
- [ ] 把 EventTx 抽为普通/managed 两种策略；普通行为不变，managed 统一走预算 sender。逐个审计 run_loop、think_phase、工具 forwarder 的 send/try_send；在 provider 累积字符串/参数 append **之前** reserve，在工具结果入 Session **之前**计数。不可拆凭证/图片通过 Arc payload+lease，队列仅计句柄开销；正文预算在 payload 最后 owner 释放前保留。delta 按 UTF-8 拆，128KiB不是截断凭证的理由。

```rust
let lease = budget.reserve_text(next_fragment.len())?;
partial.push_str(next_fragment); // 先reserve，后append
partial_leases.push(lease);      // 随partial/session共同持有
// sender拒绝任何权威事件时执行 cancellation_token.cancel()
```

- [ ] Green：队列256/1MiB、单事件128KiB、生产累积16MiB、decoded binary64MiB、base64不重复算文本、控制空间4KiB、真实 task结束；再跑 core 全 lib。对 provider 自身现有网络帧限制记录边界，不能宣称 core 限制消除了所有上游临时分配。
- [ ] G；提交 `feat: bound managed agent production and event queues`。

### Task 04：动态 roots 与安全文件句柄

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-tools/src/side_read/{mod.rs,unix.rs}`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/lib.rs`
- Test: `side_read/mod.rs` tests（平台 cfg）。

**Interfaces:**
- Consumes: `std::fs::File`、`std::path::{Path,PathBuf}`、现有libc0.2；不消费runtime/PermissionChecker，也不假设ToolsContext的静态roots会自动撤权。
- Produces: `SideRoots: Send + Sync { fn current_roots(&self) -> Vec<PathBuf>; }`；`SideReadAccess::new(primary: PathBuf, created_roots: Vec<PathBuf>, current: Arc<dyn SideRoots>) -> io::Result<Self>`；`open_regular(&self, path: &Path) -> io::Result<File>`；`open_directory(&self, path: &Path) -> io::Result<File>`；`set_attachment_aliases(&mut self, aliases: Vec<SideFileAlias>) -> io::Result<()>`，`SideFileAlias { pub original: PathBuf, pub copy: PathBuf }`。别名只精确命中登记文件，并校验 original 当前仍有授权；copy 从受持有目录句柄定位，不给通用root。`SideReadAccess` Clone 共享授权源。

- [ ] 新建测试 Roots 使用 `Arc<RwLock<Vec<PathBuf>>>` 实现 trait；写嵌套范围交集、相对路径primary固定、父新增不继承、撤销、`..`、路径前缀同名、目录换symlink、FIFO/socket/设备拒绝、alias撤权。race 用屏障/注入 open hook，不靠概率循环。

```rust
#[test]
fn side_read_access_rechecks_roots_and_blocks_path_races() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), b"ok").unwrap();
    let roots = Arc::new(TestRoots(RwLock::new(vec![dir.path().to_owned()])));
    let access = SideReadAccess::new(dir.path().into(),
        vec![dir.path().into()], roots.clone()).unwrap();
    assert!(access.open_regular(Path::new("a.txt")).is_ok());
    roots.0.write().unwrap().clear();
    assert!(access.open_regular(Path::new("a.txt")).is_err());
}
```

- [ ] Red：`cargo test -p yi-agent-tools --lib side_read_ -- --nocapture`；用当前“check path再open”的行为确认race测试能抓出漏洞。
- [ ] macOS/Linux 用 libc `openat` 逐组件 `O_NOFOLLOW|O_CLOEXEC`，中间组件加 `O_DIRECTORY`，最终 `O_RDONLY|O_NONBLOCK` 后 fstat 仅普通文件；安全遍历目录同样持 fd。保守拒绝所有 symlink 是首版允许策略。canonicalize只用于初始可信root定位，不能在检查后再次用拼接绝对路径打开。root源在每次open/scan/candidate打开重新读取，按组件包含关系求交。

```rust
// A祖先B => intersection取B，B祖先A => 取A；不是集合元素相等。
let intersection = if a.starts_with(b) { Some(a) }
    else if b.starts_with(a) { Some(b) } else { None };
// unix::open_beneath(root_fd, relative) 内每一组件openat并检查类型。
```

- [ ] Green：所有上述测试；unsupported target 文件能力明确 Unsupported，不能回退普通工具；普通 ToolsContext roots 原测试保持通过。
- [ ] G；提交 `feat: add revocable handle-based side read access`。

### Task 05：有界 blocking 与侧用文件工具

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-tools/src/side_read/{blocking.rs,file_tools.rs}`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/{side_read/mod.rs,fs/read_document.rs,image_prep.rs,lib.rs}`
- Modify: `yi-agent-rs/crates/yi-agent-tools/Cargo.toml`
- Test: 新模块 tests；原文档/image prep tests。

**Interfaces:**
- Consumes: Task 04 `SideReadAccess`；Task 03预算由runtime在Tool结果边界再核算，tools不依赖runtime。
- Produces: `SideBlockingPool::new(host_limit: usize, per_side_limit: usize, host_queue: usize, per_side_queue: usize) -> Self`；`async fn run<T: Send + 'static>(&self, side_id: &str, cancel: CancellationToken, owner: Arc<dyn Send + Sync>, job: impl FnOnce() -> io::Result<T> + Send + 'static) -> io::Result<T>`；owner由工作闭包持有到真实结束。`register_side_file_tools(registry: &mut ToolRegistry, access: Arc<SideReadAccess>, pool: SideBlockingPool, side_id: String, cancel: CancellationToken, owner: Arc<dyn Send + Sync>)`。parser新增 `read_document_bytes(bytes: &[u8], display_name: &str, extension: &str, pages: Option<&str>, max_chars: usize) -> Result<String, ToolsError>`，负责同一pages验证/CJK修复/扫描件说明/有界输出；原extract(path,pages)保持签名和文案，通过共用私有bytes解析核心委托，不丢pages/文件展示名。图片直接用已有prepare_image_bytes。

- [ ] 写 parser已打开文件内容测试（打开后替换路径，仍读原fd），逐个read/read_document/view_image/glob/grep测试 revoked candidate；写 host8/side2、bounded queue、等待可取消、阻塞中close不提前释放owner计数测试。

```rust
#[tokio::test]
async fn side_blocking_pool_does_not_run_cancelled_waiter() {
    let pool = SideBlockingPool::new(8, 2, 8, 2);
    let cancel = CancellationToken::new(); cancel.cancel();
    let result = pool.run("s", cancel, Arc::new(()), || -> io::Result<()> {
        panic!("cancelled work must not start")
    }).await;
    assert!(result.is_err());
}
```

- [ ] Red：`cargo test -p yi-agent-tools --lib side_ -- --nocapture`。tools若无直接 tokio-util依赖，增加 `tokio-util = { version = "0.7", features = ["rt"] }`；核对 workspace 是否已有可复用声明。
- [ ] queue先有界reserve，再等 host/per-side许可，取消等待直接撤销；blocking执行器使用宿主最多8个**专用std::thread工作线程**与有界job队列，线程数不随side增加。启动工作后许可/owner移动进闭包，异步等待取消只丢接收不释放真实工作占用。不要用共享Tokio spawn_blocking承载不可终止IO：其runtime默认Drop会等未完成工作，可能违背宿主有界退出。shutdown停止准入/取消队列，限时等待已开始工作，残留线程/owner仍记在宿主隔离表直到结束或进程退出，不谎称杀掉OS IO。读文件采用 `take(limit+1)` 检测实际越限，parser只接受有限bytes。glob/grep用fd递归，限制候选数/深度/输出/截止时间，每候选重新授权，不使用WalkDir随后File::open路径。为工具元数据显式设Builtin/read_only。

```rust
let permit = permits.acquire(cancel.clone()).await?;
let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
let worker_cancel = cancel.clone();
self.enqueue(Box::new(move || {
    let _held = (permit, owner); // 到job真实退出才drop
    let result = if worker_cancel.is_cancelled() {
        Err(io::Error::from(io::ErrorKind::Interrupted))
    } else { job() };
    let _ = reply_tx.send(result);
}))?;
tokio::select! { result = reply_rx => map_reply(result),
    _ = cancel.cancelled() => Err(io::Error::from(io::ErrorKind::Interrupted)) }
```

这里 `permits.acquire(CancellationToken) -> io::Result<BlockingPermits>`、`map_reply(Result<io::Result<T>, oneshot::error::RecvError>) -> io::Result<T>` 是本模块私有辅助，前者包含队列/host/side三种RAII reservation，后者将worker关闭转io错误；`enqueue(&self, job:Box<dyn FnOnce()+Send>)->io::Result<()>`是有界入队。工作线程用catch_unwind包job，panic不杀整个池且completion失败可见；pool登记工作完成状态和线程句柄供资源owner查询，不能把Drop句柄当终止证据。

- [ ] Green：新增tests和 `cargo test -p yi-agent-tools --lib`；确认解析库不启动外部进程，现path API兼容；block closure未结束时槽与owner仍在。
- [ ] G；提交 `feat: bound side file parsing and scanning work`。

### Task 06：有效来源发布、只读装配与共享 provider 准入

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-runtime/src/side_conversation.rs`
- Create: `yi-agent-rs/crates/yi-agent-runtime/src/side_conversation/{source.rs,policy.rs,ledger.rs,test_support.rs}`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/{lib.rs,bootstrap.rs}`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/Cargo.toml`
- Test: source/policy tests。

**Interfaces:**
- Consumes: Tasks 01–05。新runtime直接依赖 futures0.3、tokio-util0.7(rt)、uuid1(v4)、libc0.2、base64 0.22、sha2 0.10（已有workspace声明用workspace，否则与现有crate版本对齐）；只新增实际import的依赖，不靠传递依赖。
- Produces: `EffectiveSideSource { session: Arc<Mutex<Session>>, provider: Arc<dyn Provider>, config: AgentConfig, primary: PathBuf, roots: Vec<PathBuf>, current_roots: Arc<dyn SideRoots>, attachments: SideAttachmentCatalog, revision: u64, generation: u64, provider_gate: Option<Arc<dyn yi_agent_core::ProviderTurnGate>>, web_policy: SideWebPolicy }`，字段pub；`SideAttachmentCatalog(pub Vec<SideAttachment>)`、`SideAttachment { pub original: PathBuf, pub manifest_marker: String }`，marker来自受理时生成清单，非猜路径。`SideWebPolicy { fetch_enabled: bool, search_config: Option<serde_json::Value> }` 仅宿主内部，不derive Serialize/Debug泄密。
- `SideSourcePublisher::new(source: EffectiveSideSource) -> Self`、`async fn snapshot(&self) -> Result<EffectiveSideSource, SideError>`、`begin_update(&self) -> Result<SourceUpdate, SideError>`；`SourceUpdate::commit(self, source: EffectiveSideSource)` 同步发布配置、revision、授权源；Drop回滚updating标记。snapshot最多重试3次，仍不稳定source_changed，不持锁跨await IO；这里的Mutex一律std::sync::Mutex，与core session_handle类型一致。source捕获/clone前将revision与source本体在同一发布锁内读出，禁止先拿旧source再读取新revision造成假一致。
- `SideToolPolicy` 为固定白名单；`build_side_tool_setup(source: &EffectiveSideSource, access: Arc<SideReadAccess>, pool: SideBlockingPool, side_id: String, cancel: CancellationToken, owner: Arc<dyn Send + Sync>) -> Result<ToolRegistry, SideError>`。`SideToolSetupFactory::new(source: EffectiveSideSource, access: Arc<SideReadAccess>, pool: SideBlockingPool, side_id: String, owner: Arc<dyn Send + Sync>) -> Self`、`build_for_turn(&self, cancel: CancellationToken) -> Result<ToolRegistry, SideError>`只用固定source重建侧用registry，将**本轮IO token**传入每个工具；不能把整side token误作每轮取消，也不做热刷新。
- 复用core已有 `ProviderTurnGate::acquire(&self) -> BoxFuture<'static, Result<Box<dyn ProviderTurnLease>, String>>` 与 `Agent::with_provider_turn_gate(Arc<dyn ProviderTurnGate>)`，不新建同名类型或在provider外双重加gate。EffectiveSideSource携带父实际gate的Arc，side独立acquire、不复用lease；core think_phase现有cancel select保持生效。宿主没有统一gate时保留None，不凭空引入新的全局限流策略；mock gate测试容量1/2。
- `CloseReason::{UserClosed,Expired,ParentCleared,ParentDeleted,HostShutdown,ResourceLost}` 在本Task定义于runtime（serde snake_case），后续宿主共享。
- `SideError { pub reason: SideErrorReason, pub message: String }` 实现 Error；reason包含 spec §8.3全部稳定reason及 `InvalidParams/TurnInProgress/InsufficientScope/UnknownThread/Cancelled`；`SideError::new(reason, message)`。公共API统一 `Result<T, SideError>`，内部io/anyhow显式映射，不暴露密钥。

- 共享 `ledger.rs` 提供 `AcceptedRequest { pub request_id:String, pub turn_id:String, pub digest:[u8;32] }`、`SideRequestLedger::new()->Self`、`lookup(&self,id:&str,text:&str)->Result<Option<String>,SideError>`、`accept(&mut self,id:String,text:&str,turn_id:String)->Result<(),SideError>`、`accepted_requests(&self)->BTreeMap<String,String>`。规范UUID、非空32KiB、原字节SHA-256、256上限集中在此；lookup先于busy，上限只挡新id。`SideIdleDeadline::new(now:tokio::time::Instant)->Self`、`touch(&mut self,now:Instant)`、`expires_at(&self)->Instant`集中定义30min规则。ledger保留有界BTreeMap<String,AcceptedRequest>，deadline持last_activity:Instant；不依赖app协议。两宿主直接消费，不等TUI阶段再倒置依赖。

- [ ] 写纯policy/ledger/TTL测试与交错source更新测试，捕获provider request核对model/system/effort/ThinkingToken；通过空旁支验证provider calls=0。

```rust
#[test]
fn side_registry_rejects_mutation_even_with_yolo() {
    let policy = SideToolPolicy::default();
    assert!(policy.allows("read", ToolSource::Builtin, true));
    assert!(!policy.allows("read", ToolSource::Plugin { name: "fake".into() }, true));
    assert!(!policy.allows("bash", ToolSource::Builtin, true));
    assert!(!policy.allows("read", ToolSource::Builtin, false));
}
```

`allows(&self, name: &str, source: ToolSource, read_only: bool) -> bool` 是本Task新增方法；额外测试所有危险名字与enabled=false。

- [ ] Red：`cargo test -p yi-agent-runtime --lib side_ -- --nocapture`。
- [ ] bootstrap抽无副作用 `resolve_side_config(base: &RuntimeConfig, cwd: &Path, model_override: Option<&str>, effort_override: Option<&str>, preserve_thinking: bool) -> anyhow::Result<RuntimeConfig>`，普通装配与冷旁支都调用；另抽 `resolve_side_prompt(cfg:&RuntimeConfig)->anyhow::Result<Option<String>>`：复用resolve_system_prompt的base/日期/root AGENTS规则，skills目录只用 `SkillsService::new(roots).snapshot()/render_catalog(budget)` 做一次只读解析和catalog渲染（这两个方法不安装文件），不调用build_skills_service中的install_system_skills，不挂热刷新handle、不交互询问、不执行skill。roots与catalog预算取普通父相同的配置解析纯函数，不从真实home重新猜实例路径；抽取原skills读取/渲染纯函数共用，不调用完整build_prompt_setup/build_tool_setup、MCP或写入式迁移。旁支独立registry从已审核builtin构造，不接父PermissionChecker/decision channel/inbox/YoloSwitch；registry能力边界加工具内部SideReadAccess双重拒绝，未知工具走core受控缺失错误，文件越界不触发审批。系统末尾固定策略，不再绑定prompt refresh。用状态revision前后检查包住短session clone，新建独立Session Arc。side agent用source.provider_gate调用已有with_provider_turn_gate；不复用父租约，不更改现有准入/释放时机，等gate仍可取消。

```rust
let (source, before) = publisher.capture_source(); // 原子读source+revision；updating返回None
let messages = {
    let guard = source.session.lock().unwrap_or_else(|p| p.into_inner());
    measure_side_history(&guard, &ManagedRunLimits::side_defaults())
        .map_err(map_budget_error)?;
    forkable_messages(&guard) // 仅克隆安全前缀；不跨await持std::sync::Mutex
};
if before.is_none() || before != publisher.revision() { continue; }
let mut captured = source.clone();
captured.session = Arc::new(Mutex::new(Session::from_messages(messages)));
return Ok(captured); // 此处仅一致克隆；runner创建时添加一次隐藏边界
```

`capture_source(&self)->(EffectiveSideSource,Option<u64>)`、`revision(&self) -> Option<u64>`、`map_budget_error(BudgetExceeded)->SideError` 在source.rs定义；EffectiveSideSource实现Clone（Arc共享provider/授权源、config值克隆），snapshot返回一致的原始历史新Arc，Task08再执行一次snapshot_for_side加边界，不双重追加。试次上限固定3。主配置变更必须先begin_update再重建发布，Task10/18接入，不允许只依赖读者检查计数。

- [ ] Green：model/effort/compact/prompt boundary、registry伪造/YOLO、gate容量1/2等待取消、父side反向cancel；所有新依赖 `cargo metadata` 核对rust-version，维护workspace1.89下限或明确升级评审，不能偷偷抬高。
- [ ] G；提交 `feat: assemble isolated side sources and read-only policies`。

### Task 07：附件、目录 lease 与资源槽真实所有权

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-runtime/src/side_conversation/resources.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/side_conversation/{source.rs,test_support.rs}`
- Test: resources tests。

**Interfaces:**
- Consumes: catalog/source、SideReadAccess/BlockingPool、RunBudget。
- Produces: `SideHostResources::new(instance_home: PathBuf, pool: SideBlockingPool) -> Result<Self, SideError>`（创建host UUID、持host目录排他锁）；`host_instance_id(&self) -> &str`；`reserve_slot(&self, side_id: &str) -> Result<Arc<SideResourceLease>, SideError>`；`async fn prepare(&self, lease: Arc<SideResourceLease>, source: &EffectiveSideSource, cancel: CancellationToken) -> Result<PreparedSideResources, SideError>`。`PreparedSideResources { pub access: Arc<SideReadAccess>, pub lease: Arc<SideResourceLease>, pub binary_bytes: usize }`；`SideResourceLease`最后owner Drop将清理作业提交给**预留容量的有界宿主清理队列**，不在Drop/async线程同步remove_dir_all；作业持槽与host锁直至真实删除完成，删除失败记无正文诊断后按需留待启动回收。队列容量8（每个资源槽预留一个作业位置），清理也使用同宿主blocking配额；host锁共享持到所有残留work/清理完成。`slots_in_use(&self) -> usize`用于测试/资源状态；`#[cfg(test)] wait_for_cleanup_blocking(&self, timeout:Duration)->io::Result<()>`等待真实清理ack，生产异步清理由tracked completion通知。

- [ ] 编写catalog只取安全历史marker、内嵌图path已丢失、文档缺失/越界/修改后副本、decoded预算、父撤权后alias不可读、旧host清理拿不到锁跳过测试。

```rust
#[test]
fn side_resource_slot_is_held_until_last_owner() {
    let root = tempfile::tempdir().unwrap();
    let pool = SideBlockingPool::new(8, 2, 8, 2);
    let host = SideHostResources::new(root.path().into(), pool).unwrap();
    let lease = host.reserve_slot("00000000-0000-4000-8000-000000000001").unwrap();
    let worker_owner = lease.clone(); drop(lease);
    assert_eq!(host.slots_in_use(), 1);
    drop(worker_owner);
    // 清理完成后才归还slot；同步测试用host.wait_for_cleanup_blocking()，
    // 仅测试提供且有短超时，不让生产Drop等待。
    host.wait_for_cleanup_blocking(Duration::from_secs(2)).unwrap();
    assert_eq!(host.slots_in_use(), 0);
}
```

- [ ] Red：`cargo test -p yi-agent-runtime --lib side_resource -- --nocapture`，另运行 `side_inherits_inline_images_and_copies_catalog_files` 与 `side_blocking_cleanup_keeps_lease_and_resource_slot`（同模块新增，不是已有测试）。
- [ ] 专属目录用安全创建/权限，host锁用现libc文件锁，启动只清排他锁可取得的旧host；不凭PID/时间。复制通过授权fd有界读取，字节/inline decoded合算64MiB，base64经实际解码核验但不下载URL。catalog只匹配安全历史中的已生成manifest marker，不遍历自然语言。alias映射仅本side，不把实例根并入roots。每项IO由pool带lease执行。

```rust
let mut input = access.open_regular(&attachment.original)?;
let copied = io::copy(&mut input.by_ref().take(remaining + 1), &mut output)?;
if copied > remaining { return Err(resource_limit()); }
// resource_limit()是resources私有构造：SideErrorReason::ResourceLimit。
// output仅宿主初始化可写；工具registry没有此写能力。
```

- [ ] Green：8槽+Creating/Closing/worker残留、64MiB边界、关闭/撤销/启动回收；明确测试host/resources随owner共享，drop宿主对象不能提前释放文件锁。
- [ ] G；提交 `feat: own side attachment snapshots and cleanup leases`。

### Task 08：共享多轮 runner、停止收敛与 TTL

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-runtime/src/side_conversation/runner.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/side_conversation.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/side_conversation/test_support.rs`
- Test: runner tests。

**Interfaces:**
- Consumes: Tasks01–07。
- Produces: `SideStatus { Creating, Idle, Running, Failed, Closing, Closed }`（serde snake_case）、`SideRunnerEvent { pub side_id: String, pub generation: u64, pub turn_id: String, pub event: BudgetedAgentEvent }`；`SideUsage { pub total: SideTokenTotals, pub complete: bool }`，`SideTokenTotals`的input/output/cache_creation/cache_read/reasoning计数均u64，跨请求checked_add，不能用u32 TokenUsage反复累计溢出；单次provider usage仍复用TokenUsage。
- `SideConversationRunner::new(side_id: String, source: EffectiveSideSource, resources: PreparedSideResources, tools: SideToolSetupFactory) -> Result<Self, SideError>`；`async fn start_turn(&mut self, turn_id: String, text: String) -> Result<BoxStream<'static, SideRunnerEvent>, SideError>` **只完成受理启动，不等整轮**；`async fn interrupt(&mut self, turn_id: &str) -> Result<(), SideError>`、`async fn close(&mut self) -> Result<(), SideError>`、`status(&self) -> SideStatus`、`usage(&self) -> SideUsage`。run结束由内部task completion收敛状态；`async fn wait_quiescent(&mut self) -> Result<(), SideError>`供宿主worker在锁外等待。主注册表负责幂等/TTL，runner负责单轮10min/停止5sec；TUI通过同一runtime宿主状态辅助执行TTL，见Task18。
- test_support新增 `async fn pending_runner() -> (SideConversationRunner, Arc<Notify>, Arc<AtomicBool>)` 与 `async fn echo_runner() -> SideConversationRunner`，使用独立TempDir/resources/mock Provider，fixture持TempDir到runner drop，不能依赖用户工作区。

- [ ] 写取消不串父、反向取消、空旁支不请求、失败后追问、stopping期间拒发、旧turn completion/工具结果不能写新轮、5sec实际abort、10min超时测试。

```rust
#[tokio::test(start_paused = true)]
async fn side_interrupt_waits_for_actual_quiescence() {
    let (mut runner, entered, dropped) = pending_runner().await;
    let events = runner.start_turn("turn-a".into(), "问题".into()).await.unwrap();
    entered.notified().await;
    runner.interrupt("turn-a").await.unwrap();
    runner.wait_quiescent().await.unwrap();
    assert!(dropped.load(Ordering::SeqCst));
    assert_ne!(runner.status(), SideStatus::Running);
    drop(events);
}
```

- [ ] Red：`cargo test -p yi-agent-runtime --lib side_ -- --nocapture`。
- [ ] runner拥有agent、managed control、owner、provider流和bounded event pump；创建时对一致原始source的Session做一次snapshot_for_side并固定config，保存独立Session。每轮新建IO CancellationToken、用factory.build_for_turn构造固定策略registry并重建agent复用该side Session Arc；run_managed_blocks产生的实际core token另由control管理。interrupt先cancel本轮IO token和core token（不撤销side生命周期token），旧工具future/排队任务不能进入下一轮；校验turnId后仅置stopping并异步stop，结束前不Idle；core任务树已join后，side Session用forkable_messages收敛，保留已受理用户与完整配对。close先撤销generation/event sink，再stop；blocking late结果只有清理路径。usage从0，未拿final标incomplete，不伪造0。禁用auto compact/热重载/title/subagents。

```rust
if active.turn_id != requested { return Ok(()); }
active.stopping = true;
active.control.cancel();
// supervisor锁外stop_and_join(5s)，收敛session，再发布终态；
// 所有写入校验(side_id,generation,turn_id)仍匹配当前运行。
```

- [ ] Green：runtime lib、core managed tests；对stop后新轮验证 provider request无悬空tool-use、父Session/token/inbox/usage字节不变；等gate/等事件/工具forwarder都可取消。
- [ ] G；提交 `feat: run and stop isolated side turns`。

### Task 09：三端共享有界转录与稳定 item 切分

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-runtime/src/side_conversation/transcript.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/side_conversation.rs`
- Test: transcript tests。

**Interfaces:**
- Consumes: `SideRunnerEvent`、core AgentEvent；不依赖app-server。
- Produces: serde `SideItem { pub id: String, pub turn_id: String, pub ordinal: u64, pub content: SideItemContent }`；`SideItemContent::{UserMessage{text:String},AgentMessage{text:String},Thinking{text:String,redacted:bool},ToolCall{call_id:String,name:String,input:Value,result:Option<String>,status:SideToolStatus,truncated:bool},Notice{text:String}}`（tag=`type`，camelCase）。`SideToolStatus::{Running,Completed,Failed}`，serde snake_case；截断说明已经算进result/text，不在adapter后追加无预算文本。不含ThinkingToken/二进制/private path。
- `SideTranscript::new(limit_bytes: usize, max_item_bytes: usize) -> Self`；`begin_turn(&mut self, turn_id: &str, text: &str) -> Result<Vec<Arc<SideItem>>, SideError>`；`apply(&mut self, turn_id: &str, event: &AgentEvent) -> Result<Vec<Arc<SideItem>>, SideError>`；`items(&self) -> Vec<Arc<SideItem>>`；`retained_bytes(&self) -> usize`。返回更新后的完整items，用Arc新版本替换旧版本，不原地改共享snapshot。

- [ ] 写长Unicode/反斜杠序列化切分、稳定id、多次upsert、工具展示截断/core不截、图片只描述、16MiB+4KiB终止余量测试。

```rust
#[test]
fn side_transcript_splits_by_serialized_utf8_budget() {
    let mut transcript = SideTranscript::new(16 * 1024 * 1024, 128 * 1024);
    let text = "中\\\"".repeat(40_000);
    let updates = transcript.begin_turn("t", &text).unwrap();
    assert!(updates.len() > 1);
    for item in updates {
        assert!(serde_json::to_vec(&*item).unwrap().len() <= 128 * 1024);
    }
}
```

- [ ] Red：`cargo test -p yi-agent-runtime --lib side_transcript -- --nocapture`。
- [ ] 二分序列化后字节上限找UTF8边界；id为`turn_id/kind/logical_index/part_index`，ordinal单调；不断增长的文本仅更新最后一part，已完成part不可变。tool card有固定display budget与截断说明，保留name/status，thinking只展示允许内容不发凭证。控制空间拒给普通正文。

```rust
let encoded = serde_json::to_vec(&candidate).map_err(map_serialize)?;
if encoded.len() > self.max_item_bytes { split_at_smaller_utf8_boundary(); }
// 上述拆分在本模块实现：输入&str、字节预算，输出Vec<&str>；
// 每一候选都核对完整SideItem信封，不只str.len()。
```

- [ ] Green：transcript tests；重组所有文本等于原文、重复update不增文本、所有item序列化<=128KiB；两宿主后续只做形状适配，不重写切分/预算。
- [ ] G；提交 `feat: share bounded side transcript translation`。

---

## Phase B：app-server 宿主与协议

### Task 10：发布活跃父来源与严格无副作用冷读

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/side_conversation/source.rs`
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/side_conversation.rs`
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/server/tests/side.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/{session.rs,thread_store.rs,lib.rs}`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/{assembly.rs,runtime_binding.rs,turn/command.rs,tests/mod.rs}`
- Test: `server/tests/side.rs`、thread_store tests。

**Interfaces:**
- Consumes: `EffectiveSideSource/SideSourcePublisher`、`bootstrap::resolve_side_config`；现`BuildAgent<'a>`不可移进spawn。
- Produces: `SideParentResolver: Send + Sync { fn resolve<'a>(&'a self, parent: &'a str) -> BoxFuture<'a, Result<EffectiveSideSource, SideError>>; }`；生产工厂返回owned `Arc<dyn SideParentResolver>`，显式带RuntimeConfig/模型路径/项目索引，不捕获借用BuildAgent。测试注入独立mock resolver。
- ThreadSession增加`side_source: SideSourcePublisher`，活跃source的session是实际agent同一个Arc；宿主另存最小授权/lifecycle来源，卸载driver时移除热source/session强引用但保留授权源；已有side仅持自己的历史副本和共享撤权源，不靠保留整个父Session阻止卸载。
- `ThreadStore::read_side_snapshot_strict(&self, id: &str) -> io::Result<Option<StrictThreadSnapshot>>`；`StrictThreadSnapshot { pub meta:ThreadMeta, pub messages:Vec<Message>, pub attachments:Vec<Attachment>, pub stamp:ColdFileStamp }`、`ColdFileStamp`定义在thread_store.rs，stamp记录meta/log/partial的identity/len/mtime；不存在返回None、损坏返回Err，禁止meta重建。reader逐行提取持久UserMessage附件，仅保留安全历史仍含其manifest marker的记录，resolver转换这些Attachment为catalog，再结合宿主lifecycle generation，不让存储依赖runtime。

- [ ] 复用 **server/tests/mod.rs 的 Harness**，新增 `mod side;`；新增mock resolver注入构造，不把test_support.rs误当Harness位置。冷读测试在独立temp目录创建父meta/log/partial，记录全部文件bytes/mtime、writer lock状态、普通driver计数；损坏行、截断partial、配置覆盖、外部写者版本变化用barrier。

```rust
#[test]
fn side_strict_log_rejects_corruption() {
    // 新私有parser接口，thread_store.rs同模块tests可访问。
    let bytes = b"{not json}\n";
    assert!(decode_side_log_line_strict(bytes).is_err());
}
```

`decode_side_log_line_strict(bytes: &[u8]) -> io::Result<TurnLine>` 新增于thread_store，采用现有TurnLine，不重新定义另一种格式；空行/末行是否完整按写入格式严格核验，错误映射InvalidData。

- [ ] Red：`cargo test -p yi-agent-app-server --lib side_ -- --nocapture`，补最小reader后确认现read_log跳坏行会让目标断言失败。
- [ ] 活跃装配、SetModel/effort/preserve-thinking、compact、clear、MCP/skills改变有效prompt的入口统一begin_update/commit；源更新失败回滚，不混provider/config。冷读在宿主tracked有界blocking工作中严格decode meta/log/partial，逐行/逐文件限制读取与反序列化大小：meta最多1MiB、单条完整log/partial最多128MiB，使用take(limit+1)或有界read_until，不能先read_line分配任意长行。每行decode为一个TurnLine后折叠至最后权威messages/必要附件metadata，立即释放旧记录，不collect整个log/所有UI items；catalog元数据也计16MiB文本预算。用最后权威messages构建安全前缀并调用measure_side_history，不能先read_to_end整个无限日志再检查。超限返回snapshot_too_large，不跳过坏记录。用现partial合并规则但不promote/修复/写lock，前后stamp及宿主revision校验，最多3次；若期间热装载优先新活跃source。catalog从受理TurnPrompt附件登记，冷读从持久UserMessage附件恢复且与安全历史marker匹配。

```rust
for _ in 0..3 {
    let before = self.parent_stamp(parent)?;
    let cold = store.read_side_snapshot_strict(parent)?.ok_or_else(unknown_parent)?;
    let config = resolve_side_config(&self.base, &cwd, model, effort, thinking)?;
    if before == self.parent_stamp(parent)? && cold.stamp == store.side_stamp(parent)? {
        return self.assemble_cold_source(cold, config); // 不创建普通driver
    }
}
Err(SideError::new(SideErrorReason::SourceChanged, "父上下文正在更新"))
```

`parent_stamp(&self,&str)->Result<ParentStamp,SideError>`、`side_stamp(&self,id:&str)->io::Result<ColdFileStamp>`、`assemble_cold_source(&self,StrictThreadSnapshot,RuntimeConfig)->Result<EffectiveSideSource,SideError>` 都是本Task新增私有方法；ParentStamp包括generation/source_revision/hot-source身份；unknown_parent()为本模块私有SideError::UnknownThread构造。

- [ ] Green：`side_open_cold_parent_has_no_resume_side_effects`、source模型切换/partial/损坏/冷热竞争 tests；严禁调用resume/fork/interruption/标题/MCP；父文件无写入、空安全历史失败。
- [ ] G；提交 `feat: expose consistent parent sources for side snapshots`。

### Task 11：注册表、幂等受理与父生命周期屏障

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/side_conversation/registry.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/side_conversation.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/{transport/mod.rs,turn/command.rs}`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread/lifecycle/delete.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/tests/side.rs`
- Test: registry tests、server/tests/side.rs。

**Interfaces:**
- Consumes: Tasks08–10、host resources。
- Produces: `SideRegistry::new(resolver: Arc<dyn SideParentResolver>, resources: SideHostResources) -> Self`；async `open(&self,parent:&str)->Result<SideSummary,SideError>`、同步 `list(&self,parent:&str)->Vec<SideSummary>`、async `turn_start(&self,side:&str,text:String,request_id:String)->Result<String,SideError>`、async `interrupt(&self,side:&str,turn:&str)->Result<(),SideError>`、async `close(&self,side:&str,expected:Option<&str>)->Result<(),SideError>`；`SideSummary`字段完全对应spec §8.1（含stopping）：side_id/parent_thread_id为String，status为SideStatus，snapshot_revision/last_seq/generation为u64，active_turn_id为Option<String>，expires_at为Option<i64>（epoch ms；Running/Creating为null），stopping为bool。
- `begin_parent_change(&self,parent:&str,reason:CloseReason)->Result<ParentChangeGuard,SideError>`；`ParentChangeGuard::commit(self)`撤销generation/回收，Drop在提交前失败则解除屏障。`CloseReason`消费Task08 runtime定义的spec六种值；`async fn shutdown(&self) -> ()`。
- `SideRegistry` 与 `SideHostResources/SideBlockingPool` 在进程级启动入口只创建一次，向 stdio、relay、各 WS 连接 clone 同一个 Arc；不得每次 serve/initialize 建新 registry 或生成 host id，否则桌面/手机会各看见一个旁支。`open` 的scope由连接校验，资源与父索引不按连接复制。
- 去重记录/校验/hash/256上限消费Task06的SideRequestLedger/AcceptedRequest；TTL消费SideIdleDeadline。不再定义另一份AcceptedRequest或使用DefaultHasher；tombstone保留有界hash/id副本，不含正文。

- [ ] 建registry fixture使用Task10的注入resolver、temp resources，新增精确接口 `async fn registry_fixture() -> SideRegistry`；写并发open、Creating被close/父提交作废/父失败回滚、8槽隔离清理、TTL/list不续期、256request、tombstone1024/5min测试。

```rust
#[tokio::test(start_paused = true)]
async fn side_concurrent_open_returns_one_runner() {
    let registry = registry_fixture().await;
    let (a, b) = tokio::join!(registry.open("parent"), registry.open("parent"));
    assert_eq!(a.unwrap().side_id, b.unwrap().side_id);
    assert_eq!(registry.list("parent").len(), 1);
}
```

- [ ] Red：`cargo test -p yi-agent-app-server --lib side_ -- --nocapture`。
- [ ] 原子parent key创建占位+资源槽+generation；open等待共享completion但初始化在tracked worker，RPC主循环不await它（Task12）。30sec deadline撤销创建；list可拿Creating id；Closing阻止重建至真实资源owner排空。受理先校验原字节hash/旧id，再busy/256上限，原子写accepted/Running/turnId/changed后启动。失败回滚不得留下已受理但永远无终态记录。close CAS与进入Closing一个临界区，tombstone只id/hash，明确关闭后重试不跑模型。

```rust
if let Some(old) = accepted.get(&request_id) {
    return if old.digest == sha256(text.as_bytes()) { Ok(old.turn_id.clone()) }
        else { Err(invalid_params("requestId对应正文不同")) };
}
if entry.active_turn_id.is_some() { return Err(turn_in_progress()); }
// 两个私有错误构造返回SideError；sha256(&[u8])->[u8;32]。
```

- [ ] 逐个接clear/delete/worktree回收的**副作用提交前**屏障；失败Drop回滚，成功commit作废；compact仅更新未来source不关闭已有side。父unload保留授权源；shutdown跟踪全部初始化/runner/cleanup，不在RPC loop等5sec。
- [ ] Green：上述race及 `side_request_id_deduplicates_turn_start`、`side_rejects_second_inflight_turn`、`side_close_rejects_stale_turn_confirmation`；断线不close、旧interrupt不杀新轮、TTL仅open/受理turn/turn结束续期。
- [ ] G；提交 `feat: coordinate side lifecycle and idempotent turns`。

### Task 12：完整 wire 类型、域分发、scope 与能力

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/{side.rs,side_methods_baseline.txt}`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/{protocol.rs,rpc/mod.rs,server.rs,server/transport/mod.rs,side_conversation.rs}`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/tests/{mod.rs,side.rs}`
- Generate: `desktop/src/lib/generated/` 新side DTO与更新Notification；Modify: `desktop/src/lib/protocol.ts`
- Test: protocol/rpc side tests、Harness stdio/WS认证scope。

**Interfaces:**
- Consumes: Registry API；新增 `Domain::Side`、`domain_for("side/...")`、AppState registry Arc及实际transport dispatch。
- Produces: 六个RPC按spec §8.1精确请求/响应DTO；请求 `#[serde(rename_all="camelCase", deny_unknown_fields)]`，响应默认snake_case。所有跨语言DTO derive现有TS导出规则，不手改generated；u64/i64线上数值标注 `#[ts(type="number")]` 并将revision/seq/generation推进限制在JS安全整数内，超限受控终止不wrap。`expectedActiveTurnId` **必填可null**，普通Option serde不能区分缺省，使用自定义deserialize_required_nullable辅助或原Value先校验contains_key再typed decode。
- 明确新增wire DTO：`SideTurnStartResult { turn_id:String }`；`SideListResult { sides:Vec<SideSummary> }`；`SideReadItemsResult { items:Vec<Item>, snapshot_seq:u64, status:SideStatus, usage:SideUsageWire, active_turn_id:Option<String>, accepted_requests:BTreeMap<String,String>, effective_config:SideEffectiveConfigWire, snapshot_token:String, next_cursor:Option<String> }`。本实现每页均携同一个不可变metadata（第一页必须；后页重复也不改变快照），空items单页仍有metadata。`SideUsageWire`包含 `total:SideTokenTotalsWire, complete:bool`；totals wire逐项映射runtime u64字段，不借TokenUsage丢累计精度。`SideEffectiveConfigWire { model:String, reasoning_effort:Option<String>, preserve_thinking:bool, read_only:bool, tool_names:Vec<String> }`仅创建时固定值，不含密钥/API headers/完整prompt；供side `/config`，不调用parent config/read。
- 参数DTO对应表中六方法；open直接返回SideSummary，interrupt/close返回空对象。SideReadItemsParams拒cursor无token；accepted_requests key为规范request UUID。SideReadItemsResult元数据来自同一baseline，不能分页中重读运行态替换。
- `Notification`新增四类；公共fields/seq/turn/summary/item/usage严格按spec §8.2；`SideItem -> protocol::Item`只形状适配：User/Agent/Thinking直映射id/text，Tool映射call_id/name/input/result/status且images为空，Notice映射有说明前缀的AgentMessage（前缀在Task09预先计入）；去掉内部turn_id/ordinal/content wrapper后不增大。serialized Item再次复核<=128KiB，不在协议层任意二次截断。
- `initialize.capabilities.side_conversation_v1=true` 和 `sideHostInstanceId`由**同一宿主resources**产生，所有transport连接相同宿主同id，不能每initialize随机。

- [ ] Harness新增scope表测试，基线文件仅六个side methods，不改thread_methods_baseline的含义。精确测试缺expectedActiveTurnId、unknown附件字段、UUID大小写/格式/36ASCII、32KiB边界/空白、未知side/父、错误码冲突与data.reason。

```rust
#[test]
fn side_close_requires_explicit_nullable_confirmation() {
    let missing = serde_json::json!({"sideId":"00000000-0000-4000-8000-000000000001"});
    assert!(decode_side_close(missing).is_err());
    let idle = serde_json::json!({"sideId":"00000000-0000-4000-8000-000000000001",
        "expectedActiveTurnId":null});
    assert!(decode_side_close(idle).is_ok());
}
```

`decode_side_close(Value)->Result<SideCloseParams,SideError>`是rpc/side.rs新私有helper；规范UUID采用解析后`parsed.hyphenated().to_string()==input`，不trim text后hash。

- [ ] Red：`cargo test -p yi-agent-app-server --lib side_ -- --nocapture`；统计全RpcError常量后分配无冲突码，reason稳定，禁止直接猜数字。
- [ ] side请求/事件日志只记method/id/reason/计数，审计通用RPC tracing/transport诊断不输出text/items/config凭据，snapshot错误不回显正文。handler快速验证后将open/close/stop等work交给tracked task，completion经响应sender返回；不得借用 `AppState<'a>`跨spawn，把所需registry/connection/sender克隆成owned。read/list Observe，其余Control，认证先于scope，通知同样认证。直到Task13回放/广播可用之前capability在开发分支内部保持关闭，不发布半功能。

```rust
let registry = Arc::clone(&state.side_registry);
let responder = state.response_sender.clone(); // 在transport新增owned responder
state.side_tasks.spawn(async move {
    let result = registry.open(&params.parent_thread_id).await;
    responder.send(request_id, result).await;
}); // side_tasks是宿主JoinSet，退出统一join/abort
```

`SideResponseSender::send<T: Serialize>(&self, id: RequestId, result: Result<T,SideError>)`（async、返回`()`） 是transport/rpc_io新增适配，不是假定现有field；写出队列仍用原有连接关闭/大小校验路径。

- [ ] Green：所有methods/scopes/domain/unknown method、stdio与WS请求、creating响应不堵loop、错误表、TS导出；运行 `cargo nextest run --all-features -p yi-agent-app-server export_bindings`，review生成物，再 `npx tsc --noEmit`（先修现有判别联合穷尽分支，不用as any绕过）。
- [ ] G；提交 `feat: expose scoped side conversation protocol`。

### Task 13：修订序号、不可变分页、父订阅广播

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/side_conversation/replay.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/{side_conversation.rs,side_conversation/registry.rs,protocol.rs,broadcast.rs,rpc/side.rs}`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/{transport/mod.rs,tests/side.rs}`
- Test: replay tests与多连接Harness/WS。

**Interfaces:**
- Consumes: Task09不可变SideItem、Task11状态机、Task12DTO。
- Produces: `ReplayStore::new() -> Self`；`read_page(&mut self, connection: &str, side: &str, token: Option<&str>, cursor: Option<&str>, snapshot: Option<ReplayBaseline>) -> Result<SideReadItemsResult, SideError>`；`release_connection(&mut self, connection:&str)`、`release_side(&mut self, side:&str)`。`ReplayBaseline`包括Arc item索引、snapshot_seq、status、usage、active_turn_id、accepted_requests、effective_config、generation；第一次registry短锁捕获，锁外序列化。
- 发布器 `publish(&mut self, update: SidePublishedUpdate) -> Notification` 每次只推进一个seq；`SidePublishedUpdate::{Changed(SideSummary),Item(Arc<SideItem>),Completed(SideTurnOutcome),Closed(CloseReason)}`；`SideTurnOutcome`明确含turn_id/status/error/usage/累计usage。Notification::thread_key返回父id，不能None全广播。

- [ ] 写512KiB完整信封、128KiB item、转义Unicode、snapshot中更新、cursor跨side/token、60sec/5min、8token/128MiB、断连/末页/close释放、Observe订阅、未经认证收不到、尾帧丢失list水位测试。

```rust
#[test]
fn side_frame_size_uses_full_envelope() {
    let payload = serde_json::json!({"jsonrpc":"2.0","id":1,"result":{
        "items":[{"text":"\\\"中".repeat(180_000)}]}});
    assert!(encode_side_envelope(&payload).is_err());
}
```

`encode_side_envelope(value:&Value)->Result<Vec<u8>,SideError>`为replay.rs新增共用出口检查；分页构包也调用它，错误信封不携超大原文。

- [ ] Red：`cargo test -p yi-agent-app-server --lib side_ -- --nocapture`。
- [ ] snapshot token绑定connection/side/generation/immutable revision，cursor只接受该snapshot合法偏移；预估预算准入再捕获共享版本，按真实保留版本去重计128MiB，移除最后引用才释放。每页携同一状态/accepted/effective_config，分页按完整信封字节试装，最后页释放token但返回水位。changed原子受理先于模型启动；50ms最多一次完整upsert，结束强制flush后completed，再开放下一轮；critical closed不丢，出站拥塞则关闭连接让客户端重对账，不无声丢终态。

```rust
let next = state.seq.checked_add(1).ok_or_else(resource_limit)?;
let notification = update.into_notification(next, &state.parent_id, &state.side_id,
    state.generation);
state.seq = next; // 一个published revision恰好一帧；token不直接推进seq
```

`into_notification(self,u64,&str,&str,u64)->Notification`在SidePublishedUpdate实现；replay锁中只复制索引和metadata，不序列化16MiB。close直接撤销所有token，页请求返回side_not_found/expired，不从tombstone恢复正文。

- [ ] Green：`side_events_follow_parent_subscription_without_polluting_parent`、`side_reconnect_replays_current_items_without_duplicate_text`、`side_frames_stay_within_serialized_budget`、close/late completion；普通列表/未读/usage/父translator不变。此任务全部通过才启用capability。
- [ ] G；提交 `feat: replay bounded side snapshots through parent subscriptions`。

---

## Phase C：桌面与手机同期交付

### Task 14：每连接握手身份与独立 side store

**Files:**
- Create: `desktop/src/lib/sideConversation.ts`
- Create: `desktop/src/lib/sideConversation.test.ts`
- Modify: `desktop/src/hooks/{useHandshakeRecovery.ts,useNotificationRouter.ts}`
- Modify: `desktop/src/lib/{clientRegistry.ts,protocol.ts}`
- Modify: `desktop/src/hooks/connectionLifecycle.ts`
- Test: 上述新测试、现通知/连接测试。

**Interfaces:**
- Consumes: generated SideSummary/Notification/ReadItems DTO；现Slot/RpcClient。
- Produces: `type SideHostKey = string`（`JSON.stringify([slot, instanceId])`，不用有分隔符歧义的拼接）；`SideHostBinding { key: SideHostKey; slot: Slot; instanceId: string; client: RpcClient; supported: boolean; scope: "observe"|"control"|"admin" }`；`parseSideHandshake(slot: Slot, client: RpcClient, result: unknown, scope: SideHostBinding["scope"]): SideHostBinding | null`。
- `ConversationKey`判别main/side包含hostKey,parentId，side加sideId/generation。`SideConversationStore`：`bind(binding):void`、`apply(hostKey: SideHostKey, notification: SideNotification):void`、`dropHost(hostKey):void`、`read(hostKey,sideId):SideView|undefined`；`SideNotification = Extract<Notification,{method: \`side/${string}\`}>`；`SideView`包含summary/items/draft/pending/usage/effectiveConfig/error/anchor/sync状态，items明确为wire Item[]，绝不落普通缓存。`PendingSideSubmission { requestId:string; text:string; draftVersion:number; state:"submitting"|"uncertain" }`在本Task先定义，Task15实现状态转换，避免源码引用未定义类型。

- [ ] 用generated类型创建完整通知fixture（所有字段必填，别as any），测试相同sideId不同slot/host、不活动slot的closed、旧generation不复活、host重启清空、同host断线保留并禁发。

```ts
it("binds identities to slot and host, not global reader", () => {
  expect(sideHostKey("local" as Slot, "h1"))
    .not.toBe(sideHostKey("local" as Slot, "h2"));
});
```

`sideHostKey(slot: Slot, instanceId: string): SideHostKey`在新模块导出；测试用现 `LOCAL_SLOT` 常量替换上例断言中的字面值，以匹配实际Slot定义。

- [ ] Red：`npx vitest run src/lib/sideConversation.test.ts`。
- [ ] 主/旁路initialize都解析保存绑定，缺cap不支持而非fallback fork；onNotification **先判side namespace并按来源slot绑定路由，再ownsThreadView过滤**，side分支立即return，不feed Session.apply/cache/markRead。父普通通知照常后台累积。

```ts
if (n.method.startsWith("side/")) {
  sideRouter.accept(bindingFor(client, slot), n);
  return;
}
if (!ownsThreadView(slot, activeSlotRef.current, n.method)) return;
```

`sideRouter.accept(binding: SideHostBinding|null,n:Notification):void`、`bindingFor(client:RpcClient,slot:Slot):SideHostBinding|null`属于本Task新连接注册方法；accept内用判别type guard，不直接把startsWith当TS类型收窄。dispose旧client回调须校验transport仍绑定当前host。

- [ ] Green：store/handshake/router tests、`npx tsc --noEmit`；父cache/persist等spy均未收到side，后台slot.closed确实清理正确host。
- [ ] G；提交 `feat: isolate side views by connection and host lifetime`。

### Task 15：分页对账、连续 seq 与未确定提交

**Files:**
- Create: `desktop/src/lib/{sideSync.ts,sideSync.test.ts}`
- Create: `desktop/src/hooks/useSideConversation.ts`
- Modify: `desktop/src/lib/sideConversation.ts`
- Test: sideSync tests（fake timers/deferred promises）。

**Interfaces:**
- Consumes: Task14 binding/store；现父订阅入口。
- Produces: `SideSyncController` 构造参数 `{ binding: SideHostBinding; store: SideConversationStore; subscribeParent: (id:string)=>Promise<void>; clock:()=>number }`；`reconcile(parentId:string):Promise<void>`、`accept(notification:SideNotification):void`、`setVisible(parentId:string|null):void`、`dispose():void`、`submit(sideId:string,text:string,clientRequestId:string):Promise<void>`。`useSideConversation`是它的React生命周期适配，不把普通sess迁入side。
- Task14定义的 `PendingSideSubmission { requestId:string; text:string; draftVersion:number; state:"submitting"|"uncertain" }` 在本Task完成状态转换；store新增 `installBaseline(hostKey:SideHostKey, sideId:string, page:SideReadItemsResult, items:Item[]):void`、`setPending(hostKey:SideHostKey,sideId:string,pending:PendingSideSubmission|null):void`。任何异步回调capture binding+generation+submission snapshot。

- [ ] 用测试fixture `makeSideSyncFixture(): { controller:SideSyncController; store:SideConversationStore; hostKey:SideHostKey; rpc:ReturnType<typeof vi.fn>; deliver:(n:SideNotification)=>void }`，新测试文件内定义：RpcClient mock仅request/notifications，其它方法沿现rpc测试真实接口补齐，不伪造全局client。测试分页中upsert、缺号、重复、256/2MiB溢出、expired重开、closed取消迟到页、5秒尾帧poll、后台停poll/前台恢复、network失败非失效。

```ts
it("retries uncertain submission with the same request id", async () => {
  const f = makeSideSyncFixture();
  f.rpc.mockRejectedValueOnce(new Error("disconnected"));
  await f.controller.submit("s", "question", "00000000-0000-4000-8000-000000000001");
  const pending = f.store.read(f.hostKey, "s")!.pending!;
  expect(pending.state).toBe("uncertain");
  expect(pending.requestId).toBe("00000000-0000-4000-8000-000000000001");
});
```

fixture预装synced side，测试ID可用简写s作内存key，但wire测试用合法UUID。

- [ ] Red：`npx vitest run src/lib/sideSync.test.ts`。
- [ ] subscribe父→开始缓存→list→固定token分页→一次安装baseline→只应用连续`seq+1`；closed终态不等缺号；watermark差异禁发重对账。单个generation重对账互斥+AbortController，dispose/host变代撤销所有pages/timers；最后页正确释放由服务端负责。accepted_requests确认uncertain；没确认可同id/text重试，绝不自动产生新id或在新side重发。

```ts
if (event.seq <= view.seq) return;
if (event.seq !== view.seq + 1) {
  view.sync = "unsynced";
  scheduleFullReconcile(view.parentId);
  return;
}
applyContiguous(event);
```

`scheduleFullReconcile(parentId:string):void` 和 `applyContiguous(n:SideNotification):void`为controller私有方法；缓存全序处理，不能max(seq)跳洞。clear draft仅当原draftVersion/text仍匹配提交快照；明确拒绝后恢复问题正文，不覆盖新编辑。

- [ ] Green：新tests全场景、tsc；snapshot第一页面metadata只安装一次，read/list不调用open、不续TTL，Observe恢复不用Control RPC。
- [ ] G；提交 `feat: reconcile side snapshots and uncertain submissions`。

### Task 16：共享命令策略与手机 Stop-only 精确例外

**Files:**
- Create: `desktop/src/lib/{sideCommands.ts,sideCommands.test.ts}`
- Modify: `desktop/src/lib/{slash.ts,slash.test.ts}`
- Modify: `desktop/src/components/{MessageInput.tsx,MessageInput.test.tsx}`
- Test: 新policy与现输入框/命令测试。

**Interfaces:**
- Produces: `parseSideCommand(input:string): SideCommand|null`，`SideCommand = {kind:"open";question:string}|{kind:"back"}|{kind:"close"}|{kind:"invalid";message:string}`；`sideInputPolicy(input:{platform:"desktop"|"mobile";view:"main"|"side";running:boolean;attachmentCount:number;text:string;supported:boolean;canControl:boolean}):{action:"sendMain"|"sendSide"|"executeSideCommand"|"stop"|"blocked";reason?:string}`；MessageInput新增明确conversation context prop（旧调用默认main），runSlash/handleSendClick/disabled全走同一policy，不散落mobile例外。
- Consumes: Task14 capability/scope；不在输入组件直接选择全局RPC。

- [ ] 表格测试side/btw换行/空白、sidebar不是side、back带参invalid、side close是问题、无cap、Observe、嵌套/附件/busy拒绝且草稿不变、Tab填入不执行、确认补全执行一次。

```ts
it("keeps mobile main messages stop-only but permits exact side commands", () => {
  const base = {platform:"mobile",view:"main",running:true,attachmentCount:0,
    supported:true,canControl:true} as const;
  expect(sideInputPolicy({...base,text:"ordinary"}).action).toBe("stop");
  expect(sideInputPolicy({...base,text:"/btw question"}).action)
    .toBe("executeSideCommand");
  expect(sideInputPolicy({...base,text:"/sidebar"}).action).toBe("stop");
});
```

- [ ] Red：`npx vitest run src/lib/sideCommands.test.ts src/components/MessageInput.test.tsx src/lib/slash.test.ts`。
- [ ] slash canonical目录只增side/back/side-close，btw由alias map/显式搜索产生，HELP_GROUPS flatten等于canonical不重复。按首token精确分类，问题只trim命令边界不破坏用户原始问题字节。main中键入命令有附件一律blocked；独立按钮不进入handleSendClick不消费草稿。mobile Enter统一newline；side running可编辑，非命令发送按钮是自身Stop，back/close仍可执行。

```ts
if (command && input.attachmentCount > 0) return {action:"blocked",reason:"先处理待发附件"};
if (command) return validateSideCommand(command, input);
if (input.view === "side") return {action:input.running ? "stop" : "sendSide"};
if (input.platform === "mobile" && input.running) return {action:"stop"};
return {action:"sendMain"};
```

`validateSideCommand(command:SideCommand,input:Parameters<typeof sideInputPolicy>[0])`为本模块私有helper，返回同policy类型；running side拒嵌套不转主输入。side只允许help/config/usage/back/close，清理/模型/其他命令保留输入并拒绝。

- [ ] Green：上述所有tests、手机Enter/no interject spies、桌面onInterrupt未调用、附件保留、alias帮助；tsc。
- [ ] G；提交 `feat: route side commands without mobile interjection`。

### Task 17：App 双视图、Observe 与触控入口

**Files:**
- Create: `desktop/src/components/{SideConversationHeader.tsx,SideConversationHeader.test.tsx}`
- Modify: `desktop/src/{App.tsx,App.test.tsx}`
- Modify: `desktop/src/hooks/{useSideConversation.ts,useSessionState.ts,useNotificationRouter.ts}`
- Modify: `desktop/src/components/{ChatView.tsx,MessageInput.tsx}`
- Test: App现有fixture内新增describe，避免虚构可import的App fixture。

**Interfaces:**
- Consumes: Task14–16 controller/key/policy；ChatView复用Item呈现。
- Produces: `SideConversationHeader` props `{summary:SideSummary;parentStatus:string;canControl:boolean;onBack:()=>void;onClose:()=>void;onStop:()=>void}`；App持activeConversation，父currentId不替换成sideId。side renderer adapter `sideItemsForChat(items: readonly Item[]): Item[]`消费wire Item[]（不是runtime内部SideItem），只生成隔离展示数组，确认images/attachments为空，不调用普通Session.apply、不触发image/read；TS不import未生成的runtime类型。

- [ ] App.test.tsx复用文件内hoisted clients/requests/notifHandlers；新增桌面/手机/Observe参数化场景，RPC spy确保无parent interrupt/resume/fork/cache/markRead/trace/watch/gitDiff/upload/config写。独立按钮有父草稿+附件也能开空side且calls=0 turnStart；带参命令在baseline后只发一次。

```tsx
it("returns without cancelling the running side turn", async () => {
  // 在App.test.tsx已有render/选择父fixture基础上新增这三个本文件helper：
  // openSideViaButton():Promise<void>; resetRpcCalls():void;
  // sideRpcMethods():string[]，全部操作现有clients.requests与DOM。
  await openSideViaButton();
  resetRpcCalls();
  await userEvent.click(screen.getByRole("button", {name:"返回主会话"}));
  expect(sideRpcMethods()).not.toContain("side/turnInterrupt");
  expect(sideRpcMethods()).not.toContain("side/close");
});
```

helper实现要实际render App、注入initialize/list/readItems响应、选父并等待synced，不把操作藏成永远成功mock；同文件逐项断言实际RPC。

- [ ] Red：`npx vitest run src/App.test.tsx src/components/SideConversationHeader.test.tsx`。
- [ ] 视图按conversation key独立draft/anchor/error/usage/send锁，切side先收父右侧操作面板，父后台订阅不关。顶栏父状态链接只back，approval id只父处理；模型/effort/权限只读side有效值，help/config脱敏/usage独立；外部focusThread/gitDiff显式main。scope Observe只list/read查看，隐藏/disable Control；缺cap隐藏入口且命令明确不支持。

```tsx
const conversation = activeConversation.kind === "side"
  ? sideStore.read(activeConversation.hostKey, activeConversation.sideId)
  : mainView;
// send/stop/close函数均capture conversation key；不从await之后的currentId取目标。
// 图片继承只供模型，side ChatView传无ImageRef的展示item，不给私有image/read能力。
```

- [ ] close确认读取当前summary，idle传null，running提示后传所确认turnId；close_conflict重新展示最新状态再确认，不force。Creating可back/重复open共享、通过list取得id可close。手机窄屏同区切换，返回手势仅back；前后台对账区分network与host换代，uncertain不自动重发新side。
- [ ] Green：App/组件/reducer/router tests、`npx tsc --noEmit && npx vitest run && npm run build`；双设备mock同host open/send竞态覆盖，人工真机留Task21。
- [ ] G；提交 `feat: ship desktop and mobile side conversation views`。

---

## Phase D：原生 TUI，不依赖 WebSocket

### Task 18：独立旁支调度与定向事件

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/src/side_driver.rs`
- Create: `yi-agent-rs/crates/yi-agent/src/tui/side.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/{main.rs,tui/mod.rs,tui/app.rs}`
- Test: side_driver tests、tui::side tests。

**Interfaces:**
- Consumes: runtime source/runner/resources/transcript/gate，不引用app-server registry。
- Produces: `ConversationTarget::{Main,Side{side_id:String,generation:u64}}`；`ConversationEvent { target:ConversationTarget,event:ConversationPayload }`；`ConversationPayload::{Main(AgentEvent),SideItems(Vec<Arc<SideItem>>),SideStatus(SideStatus),SideUsage(SideUsage),SideClosed(CloseReason)}`。CloseReason直接消费Task06的runtime公共类型，由app reexport，避免TUI反向依赖服务端。
- `SideDriverCommand::{Open{question:Option<String>,request_id:String},Submit{text:String,request_id:String},Interrupt{turn_id:String},Close{expected_active_turn_id:Option<String>},Exit}`；`spawn_side_driver(source:SideSourcePublisher, resources:SideHostResources, commands:mpsc::Receiver<SideDriverCommand>, events:mpsc::Sender<ConversationEvent>) -> JoinHandle<()>`，返回真实调度句柄由main持有。
- TuiChannels增加可选side sender并更新from_senders测试构造默认None，原input/control/interrupt/kill/decision保持main专属；side事件接独立bounded channel，再由multiplexer合流。

- [ ] 以Notify阻塞主provider，在主idle控制不消费时发side命令，验证side创建/发送可完成；写side generation/target路由，main event继续到主TurnLog而side不会。mock主/side provider分别记录请求。

```rust
#[test]
fn side_command_while_main_running_does_not_interject_or_interrupt() {
    let route = classify_conversation_input("/btw 问题", false).unwrap();
    assert!(matches!(route, RoutedInput::Side(SideInput::Open { .. })));
}
```

`classify_conversation_input(text:&str,in_side:bool)->Result<RoutedInput,SideError>`、`RoutedInput::{Main(String),Side(SideInput)}`、`SideInput::{Open{question:Option<String>},Back,Close,Submit(String),LocalCommand(String)}`新增于tui/side.rs；Task19接UI，不能只测分类就声称实际driver并发通过。

- [ ] Red：`cargo test -p yi-agent --bin yi-agent side_ -- --nocapture`。
- [ ] run_tui_agent仍同步外壳/block_on，block_on内启动独立side driver；主LiveRebuild所有model/effort/compact/clear/MCP刷新入口发布source。TUI消费Task06的SideRequestLedger/SideIdleDeadline，不复制算法；当前单父仍遵守8槽/残留资源与256request/30sec create/30minTTL。clear通过SideDriverCommand::ParentBarrier先建立逻辑屏障，实际清空成功才commit作废；失败Drop回滚，确认屏障不等待5秒关闭。

```rust
let ledger = SideRequestLedger::new();
let idle = SideIdleDeadline::new(tokio::time::Instant::now());
// side driver独立select：commands、runner stream、create completion、TTL timer。
// 主source发布不经idle型DriverControl请求；side创建不等主整轮结束。
```

`SideDriverCommand::ParentBarrier { reason:CloseReason, reply:oneshot::Sender<ParentChangePermit> }` 在本Task加入枚举；`ParentChangePermit::commit(self)`同步发送提交消息，Drop发送回滚。side driver在回复permit前设置barrier，commit后推进generation并撤销Creating/Open；rollback解除barrier且保留旧side；用宿主tracked completion channel保证消息不丢，不用try_send失败后悄悄放行。只持逻辑状态，不持锁跨主磁盘清空。

- [ ] Green：实际运行并发、双向cancel、parent source更新、8槽残留、TTL与请求去重、source/side通道背压可取消；`cargo test -p yi-agent --bin yi-agent tui::side` 确认非零。
- [ ] G；提交 `feat: schedule TUI side turns independently of main control`。

### Task 19：双视图与父审批命令模式

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/{side.rs,app.rs,slash.rs,replay.rs}`
- Test: tui::side、app键路由tests、slash help tests。

**Interfaces:**
- Consumes: Task18定向channels/events、runtime SideItem。
- Produces: `TuiSideViews { main: ConversationUi, side: Option<ConversationUi>, active: ConversationTarget }`；`ConversationUi`整合HistoryState/InputLine/viewport/cost/status/running/error，不复制renderer；主SessionUi内配置/宿主信息保留主归属。`ApprovalInputMode::{Decision,Command}`（原草稿由ConversationUi的独立saved_approval_draft:Option<String>保存/恢复，纯key路由无需捏造草稿）；`route_approval_key(key:KeyEvent,mode:&mut ApprovalInputMode)->ApprovalKeyAction`，`ApprovalKeyAction::{DecisionKey(KeyEvent),EditCommand(KeyEvent),EnterCommandMode,ExitCommandMode,ApplicationExit,Consumed}`。
- `side_items_to_cells(items:&[Arc<SideItem>])->Vec<HistoryCell>`复用现replay呈现基本单元，不转成AgentEvent喂主TurnLog；新items完整upsert更新已存在逻辑item，不重复append。

- [ ] 写后台父events不改side running/history、back恢复主草稿/viewport、主approval在side不抢焦点、审批`/`→命令→Enter无decision、Esc恢复审批草稿、未入模式Enter仍批准。

```rust
#[test]
fn parent_approval_command_mode_does_not_approve_on_enter() {
    let mut mode = ApprovalInputMode::Decision;
    let slash = KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE);
    assert!(matches!(route_approval_key(slash, &mut mode),
        ApprovalKeyAction::EnterCommandMode));
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    assert!(matches!(route_approval_key(enter, &mut mode),
        ApprovalKeyAction::EditCommand(_)));
}
```

- [ ] Red：`cargo test -p yi-agent --bin yi-agent tui::side -- --nocapture` 与app审批现有测试。
- [ ] handle_key先应用应用级退出/弹窗优先规则，再active side分支，再父审批模式；side绝不走pending_permission。审批命令模式只side/btw/back/side-close/help/退出，其他拒绝保留输入，显示“命令，不会批准”。默认help目录side canonical，btw仅alias显式搜索；back/close无参、嵌套拒绝。进入side收父弹窗不停止父；父审批提示来源和back链接。

```rust
if active_is_side {
    return handle_side_key(key, side_view, side_sender);
}
if parent_has_permission {
    return handle_parent_approval_or_command(key, approval_mode);
}
```

两个handler在tui/side.rs定义：`handle_side_key(key:KeyEvent, ctx:&mut SideKeyContext<'_>)->Result<bool,SideError>`、`handle_parent_approval_or_command(key:KeyEvent, ctx:&mut ApprovalKeyContext<'_>)->Result<bool,SideError>`；SideKeyContext持active ConversationUi/side Sender可变借用，ApprovalKeyContext持主ConversationUi、ApprovalInputMode和主decision/side Sender。调用处从现KeyContext拆借用，不新增十几个参数；主事件分支永远写main，renderer仅借active view。上例分支展示路由顺序，实参按这两个ctx接口构造。

- [ ] Green：`background_parent_events_do_not_change_side_history_or_running`、`back_restores_main_draft_and_viewport`、两个approval命名验收、tui::slash/help/control_spec全match；两份history都受已有缓存上限且共享runtime16MiB上限。
- [ ] G；提交 `feat: isolate TUI side views and approval command entry`。

### Task 20：停止/退出、标题通知与主日志隔离 e2e

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/{main.rs,side_driver.rs}`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/{side.rs,app.rs,title.rs}`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/notify/mod.rs`
- Modify: `yi-agent-rs/crates/yi-agent/tests/e2e_resume.rs`
- Test: tui::side/TestBackend、e2e_resume。

**Interfaces:**
- Consumes: Task18/19定向状态与真实driver句柄。
- Produces: `TuiExitCoordinator`持main/side cancel+JoinHandle，`async fn stop_all(&mut self) -> Result<(), SideError>`；`visible_stop_target(active:&ConversationTarget,main_running:bool,side_running:bool)->Option<ConversationTarget>`；现Ctrl+C双击确认窗口规则不改数值。主TUI切换Task02的run_tracked_blocks，仅新增任务所有权、不改变原事件/正文策略；退出时先cancel两路、实际core任务树join，再完成TUI reactor teardown/MCP cleanup；pending blocking owner仍真实占用资源到结束，不能在退出验收声称已杀OS IO。

- [ ] 写Esc弹窗优先、side idle不exit、Ctrl+C首次只可见/二次全退出、Ctrl+Q/quit在审批命令模式可达、main running/side done标题仍running、来源通知、两view窄CJK/theme/resize TestBackend。

```rust
#[test]
fn side_esc_cancels_only_side() {
    let active = ConversationTarget::Side {side_id:"s".into(), generation:1};
    assert_eq!(visible_stop_target(&active, true, true), Some(active));
    assert_eq!(visible_stop_target(&ConversationTarget::Side {
        side_id:"s".into(),generation:1}, true, false), None);
}
```

ConversationTarget derive PartialEq/Eq/Clone；实际按键测试还断言main interrupt sender未收到，不能只测纯函数。

- [ ] Red：`cargo test -p yi-agent --bin yi-agent tui::side -- --nocapture`；`cargo test -p yi-agent --test e2e_resume`。
- [ ] 在**bin单测 tui::side::tests**新增 `side_events_never_reach_main_turn_log`：同一TempDir、真实TuiSessionStore/TurnLog、主driver+side driver与TestBackend/EventSource连通，父mock provider在Notify阻塞，发送side命令产唯一标记，back后释放父完成；检查父jsonl/meta/list仅主消息，并用TuiSessionStore::load_for_replay验证回放不含side、resume_hint仍主id。`e2e_resume.rs`只增强现headless CLI的run→resume不回归与父文件格式检查，不能直接调用二进制private TUI，也不虚构CLI `/side` 参数。两条测试链共同覆盖实际TUI输入/持久化与CLI恢复，真实终端流程再由Task21人工验证。

```rust
let parent_log = std::fs::read_to_string(parent_jsonl).unwrap();
assert!(parent_log.contains("MAIN_FINISHED"));
assert!(!parent_log.contains("SIDE_ONLY_MARKER"));
// fixture parent_jsonl来自隔离TempDir创建的真实主ThreadStore。
```

- [ ] Green：`quit_cleans_both_runners`、`side_events_never_reach_main_turn_log`、tui::side全测试、完整 `cargo test -p yi-agent`；主TurnLog/TuiSessionStore只main分支调用append/touch/promote_pending，退出清理包括创建中side。
- [ ] G；提交 `feat: complete TUI side cancellation and persistence isolation`。

---

## Phase E：联合验收与完成门

### Task 21：跨端故障演练、项目状态与最终门禁

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/tests/side.rs`
- Modify: `desktop/src/{App.test.tsx,lib/sideSync.test.ts}`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/side.rs`
- Create: `docs/testing/side-conversation-acceptance.md`
- Modify: `docs/project-management/{yi-agent-core,yi-agent-tools,yi-agent-runtime,yi-agent-app-server,desktop,yi-agent-tui,README}.md`
- Modify: `docs/superpowers/specs/2026-11-03-side-conversation-design.md`（只记录已验证差异/完成证据，不删原验收要求）。

**Interfaces:**
- Consumes: Tasks01–20的测试/真实入口；不产新功能接口。
- Produces: acceptance记录每条case的版本、环境、命令/操作、实际结果与未通过项；不能用计划checkbox代替功能测试。

- [ ] 自动矩阵：父idle/running/approval × desktop/mobile/TUI；同宿主双端open/send/close × 冷父/热父 × 网络断线/host重启。给每种故障加确定性barrier或fakeclock，覆盖spec §11表格，不能只跑happy path。

```text
父provider阻塞→桌面/手机同时open→同side id
→手机提交A、桌面提交B→仅一turn、另端保留草稿
→手机back不stop→桌面确认关闭旧turn，期间新轮开始
→close_conflict→再次明确确认→两端回父
→父仍完成，普通list/未读/费用/log无side正文
```

- [ ] Red→修复→Green：每个新回归先运行对应命名test证明red，再改实现；若修复改变spec契约，先写清设计变更评审，不能为了过测试偷换语义。
- [ ] 人工：桌面+真实手机同relay宿主完成spec §12.5原流程（运行中旁问/追问/返回/仅停side/主完成）；Observe设备已有side阅读与禁Control；冷父、手机后台唤醒、close冲突、无cap旧server；TUI审批/Esc/退出/resume、窄CJK终端。记录真机/终端实际证据，不能把Vitest视口当真机验收。
- [ ] 生成bindings并review/stage，然后从实施worktree执行完整命令，逐项记录非零测试数与退出码：

```sh
cd yi-agent-rs
cargo test -p yi-agent-core --lib side_
cargo test -p yi-agent-tools --lib side_
cargo test -p yi-agent-runtime --lib side_
cargo test -p yi-agent-app-server --lib side_
cargo test -p yi-agent --bin yi-agent tui::side
cargo test -p yi-agent --test e2e_resume
cargo nextest run --all-features -p yi-agent-app-server export_bindings
cargo fmt --all
# 审核并stage生成物/格式变化后执行；不stage用户无关文件。
just ci
cd ../desktop
npm ci
npx tsc --noEmit
npx vitest run
npm run build
```

- [ ] 若所有自动/人工验收绿，六模块条目标 `[x]`、加入真实file:symbol/可执行命令/acceptance证据；README分别增加完成数（总数此次不再加）。若真机缺失或有门禁失败，保持相应 `[ ]`，报告未完成，不整体宣布交付。诊断审计确认无正文/密钥，退出残留资源与slot符合实际保证。
- [ ] G；提交 `feat: verify side conversations across desktop mobile and TUI`。之后加载 finishing-a-development-branch/requesting-code-review，取得代码评审，按用户授权合并；合并前不擅自处理main的无关修改，合并结果必须再跑Rust+前端门禁，完成后移除已合并worktree/分支，不删除其它会话目录或进程。

---

## 附录 A：spec 到任务的覆盖索引

| spec | 实施任务 | 独立验收证据 |
| --- | --- | --- |
| §1–3 核心语义/非目标 | 01、06、08、11、17–20 | 空旁支0调用、只读registry、无持久thread、父继续 |
| §4.1 命令/附件草稿 | 16、17、18、19 | exact token/alias/补全/手机Stop-only/审批命令测试 |
| §4.2 生命周期/配额/父屏障 | 07、08、11、18、20 | 创建30sec、idle30min、8槽、父成功/失败rollback、shutdown |
| §5.1 活跃/冷父一致快照 | 01、06、10、18 | source版本、strict meta/log/partial、冷装载无副作用 |
| §5.2 配置/边界/thinking | 01、03、06、10 | 实际request model/effort/system、凭证字节、热刷新不覆盖 |
| §5.3 附件与所有预算 | 03、07、09、13 | source/catalog/copy/64MiB/16MiB/4KiB/128MiB snapshots |
| §6 文件/网络/授权/gate | 04、05、06、07 | fd路径race、根集交集/撤权、special files、blocking queue、gate1/2 |
| §7.1 task tree/终止/下一轮 | 02、03、05、07、08 | 实际root+forwarder join、blocking lease、旧工作不能写新轮 |
| §8.1 RPC/scope/幂等/CAS | 11、12 | 6方法baseline、认证Observe、request 256、tombstone1024、close conflict |
| §8.2 seq/分页/订阅/发现 | 09、13、14、15 | 固定snapshot_seq/连续seq/closed立即生效/尾帧poll/host归属 |
| §8.3 明确失败/未确定提交 | 11、12、15、17 | same-id hash/accepted_requests、新编辑不被旧回包清除 |
| §9 桌面/手机/Observe/隔离 | 14–17、21 | App与MessageInput、双端、禁止副作用spy、真机 |
| §10 TUI调度/审批/按键/渲染 | 18–20、21 | 独立driver、TestBackend、exit/approval、主log/resume |
| §11 故障竞争表 | 02–08、10–15、17–21 | barrier/fakeclock，不靠sleep |
| §12 自动与人工验收 | 各Task Green、21 | 完整Rust+前端门禁与真机/终端记录 |
| §13 项目管理 | 21 | 六模块完成证据+README计数，不把设计落盘当实现 |

### 命名测试落点补充（不以文字覆盖代替注册测试）

| 验收目标原名 | Task / 实际测试模块 |
| --- | --- |
| `side_snapshot_trims_unpaired_tool_use_without_touching_parent` | 01，core/side_context.rs::tests，ToolUse末项裁剪与父序列化字节相等 |
| `side_uses_effective_model_and_keeps_boundary_after_prompt_refresh` | 06/10，runtime source/policy tests与server/tests/side.rs的重建覆盖 |
| `side_cancel_does_not_cancel_parent` | 08，runtime runner tests，两个独立mock provider/token/inbox双向验证 |
| `side_respects_shared_provider_admission` | 06/08，mock实现core ProviderTurnGate，同一Arc容量1/2与等待取消 |
| `side_open_during_running_turn_does_not_interrupt_or_persist` | 11–13，server/tests/side.rs：Harness父provider阻塞，side/open立即完成、父不中断、无新普通meta/jsonl |

以上五项明确纳入对应任务Green，实施时必须注册实际test；其余spec命名测试在各Task red/green中给定。spec §12命名验收测试须使用原名或在acceptance记录明确等价映射，不能只有表格提及却没有可执行测试；尤其 `side_open_during_parent_approval_keeps_decision_routing`、`side_close_never_cancels_parent_and_drops_late_events` 与 `parent_approval_cannot_be_answered_from_side` 必须实际注册运行。

## 附录 B：实施前检查与计划审查记录

- [ ] 实施者再次确认当前代码基线/AGENTS.md/工具链/真实文件存在；若仓库演进造成落点变化，仅更新精确落点，不擅自缩功能。
- [ ] 所有新增类型先定义后消费；runtime不依赖app-server，tools不依赖runtime，TUI不消费WS分页。
- [ ] 每Task独立red/green，测试过滤非零；全门禁失败不提交，真机未验收不整体完成。

### 书面审查结果

1. 代码落点轮：修正Cargo.toml层级、connectionLifecycle在hooks、notify是目录、TurnLine日志类型、forkable_messages(&Session)真实签名；Modify目标必须是现有文件或前置任务Create文件。
2. 接口/依赖轮：复用已有ProviderTurnGate而非重造；统一std::sync::Mutex；提前在runtime定义CloseReason/ledger/TTL；每轮IO token与core token分开；typed nullable close参数；wire Item与runtime内部SideItem不混用；Task14先定义pending类型。
3. 端到端轮：补side创建时脱敏effective_config的回放入口并同步spec；手机命令与附件/Stop-only、后台slot通知先路由、uncertain同id重试、TUI审批Enter与headless/TUI测试可达性逐项核对。
4. 资源/失败轮：专用有配额blocking线程而非依赖Tokio abort；预留清理队列容量，最后owner后异步删且槽/锁延迟释放；cold log逐行128MiB/meta1MiB上限，不聚合全部旧记录；源捕获source+revision原子读取；主TUI退出使用track-only任务所有权、不套side配额。
5. 收敛轮：按附录A与29个命名验收目标逐项映射，检查21任务的Files/Interfaces/red/green/提交门、所有链接、Create/Modify顺序与六模块计数。当前书面审查范围内无已知阻断性计划问题；这不是对实现正确性/性能/真机体验的证明。

本次仅完成spec/plan及项目管理链接，所有实施checkbox保持未勾选；未运行这些尚不存在的新功能测试，也未声明完整CI通过。
