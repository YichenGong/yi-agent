# 一次性对话快照旁问 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 用户在主任务运行中可针对当前对话的固定快照连续旁问，退出即取消并销毁，再进入获得全新环境，不读外部资料或改写父任务。

**Architecture:** 复用安全 fork、独立 runner、资源配额和父生命周期屏障；新增宿主签发入场票据与逐环境销毁收据，覆盖返回保留的旧语义。核心缓存父已渲染配置，宿主发布活跃父来源；桌面/手机与 TUI 使用同一取消、排空和代次隔离契约。它是一项纵向生命周期功能，不是可分别上线的四个独立功能，按依赖顺序交付测试闭环。

**Tech Stack:** Rust/Tokio、现有 core/runtime/app-server/CLI；React 19、TypeScript、Vitest；现有 JSON-RPC、ts-rs 和 nextest。无新依赖。

**Status:** 计划已写，未执行。依据 [spec](../specs/2026-11-09-side-snapshot-lifecycle-design.md) 与 [复核记录](../reviews/2026-11-09-side-snapshot-lifecycle-spec-review.md)。本文代码为待实现接口及关键测试/算法，不是已验证补丁。

## Global Constraints

- `side_conversation` 默认关闭，仅 dev 可启用，release 硬闸；本机开关不控制远端机器。
- `(hostInstanceId, parentSessionId)` 同时最多一个 Creating/Open/Closing 环境；清理失败仍占父位置和资源槽。
- 快照固定，主历史与主 task 不被修改/取消；不落普通持久 thread，不恢复旧正文/草稿/用量。
- registry 零工具；不读附件文件、工作区规则、skills/MCP，不将图片 URL 交给模型或自动加载。
- 未消费票据 60 秒过期；每宿主最多 256 张、每父最多 32 张，包含全部记录；活绑定固定保留，关闭记录最多 5 分钟且可提前淘汰。
- `SIDE_STOP_GRACE = 5 秒`；逐环境清理确认另一个 5 秒总预算；终态通知预算 1 秒。
- `side/exit` 的 `closing` 只代表受理，`closed` 才代表本代资源完成销毁；`failed` 保留残留与精确重试路径。
- `capabilities.side_snapshot_lifecycle_v1` 与 `side_conversation_v1` 同时为 true 才开放新入口；旧无票据 open 拒绝。
- Observe 只查看已有环境，本地离开不关闭共享环境；Control/Admin 任一端退出关闭本代共享环境。
- 手机后台/临时断线/macOS 窗口隐藏不算明确退出；断网明确退出清正文并保留最小退出目标。
- 主 session/daemon 等其他 agent 的热更新不回退；旁问不挂动态 prompt adapter，准入/取消与硬预算保留。
- 所有代码改动在独立 worktree 新分支；不得在 main 上改。提交前 `cargo fmt --all && just ci`，合并后再次 `just ci`；文档-only 提交免 CI。

---

## 工作树、路径与交付约定

- 文档当前位于 `.worktrees/docs-side-snapshot-lifecycle`；用户授权实施后加载 using-git-worktrees，从包含本 spec/plan 的提交创建 `feat/side-snapshot-lifecycle` 工作树，不在文档树写实现。
- 下文 `R` = `yi-agent-rs/crates`，`D` = `desktop/src`，命令仍写真实路径；Files 清单全部使用真实路径，不使用别名。
- Rust RED/GREEN 从实施 worktree 的 `yi-agent-rs/` 执行；所有 crate 测试加 `--all-features`，单 crate 串行。desktop 命令从 `desktop/` 执行。
- 每 task 先 RED，再最小 GREEN，再范围测试和 review；不得跳过 RED。代码示例里的新接口在本 task 或前置 task 的 Interfaces 定义。
- 每个代码提交同时更新对应 `docs/project-management/{yi-agent-runtime,yi-agent-app-server,yi-agent-tui,desktop}.md` 的该需求证据，尚未完成的 feature 保持 `[ ]`，不使用进行中状态符号；只有全项验收后改 `[x]` 并同步 README 计数。
- 每 task 的提交门禁：`cd yi-agent-rs && cargo fmt --all && just ci`；再回树根 `git diff --check && git add <本 task Files 与对应进度文档> && git commit -m '<下列 message>'`。CI 不通过不提交，不在 main 执行这些动作。

## 文件结构与依赖

| 单元 | 责任与结构决策 | 任务 |
|---|---|---|
| core side_context | 纯内存安全快照、递归媒体净化、能力说明 | 1 |
| core AgentSnapshotBindings | 已装配 provider/session/gate 与最近渲染 config；不触发动态扫描 | 2 |
| runtime policy/source/runner | 空工具 factory、独立固定快照、取消/join 证据 | 3、5 |
| runtime cleanup.rs（新）/resources | 每槽收据，owner 归零后删除；失败保留槽 | 4 |
| app-server live_parents.rs（新）/source | 活跃父发布与冷父严格只读解析 | 6 |
| app-server tickets.rs（新） | 纯票据表、容量/TTL、精确代次绑定 | 7 |
| app-server lifecycle.rs（新）/registry | 创建 owner、退出屏障、统一异步排空 | 8 |
| app-server protocol/rpc/transport | prepare/open/exit、新能力与生成绑定 | 9 |
| desktop sideExit.ts（新）/store/sync | 无正文退出账本、本次停留授权、重连对账 | 10 |
| desktop hooks/App/SideBackBar | 三端 UI 的进入/退出/卸载/权限接线 | 11 |
| CLI side_driver/TUI/main | 创建可取消与退出即销毁，不动父控制队列 | 12 |
| 集成与历史文案 | 变异证明、端到端验收、进度交付 | 13 |

依赖：1→3，2→6；3→4→5；5+6+7→8→9→10→11；1–5→12；全部→13。允许只读并行 review，不并行编辑相同 Files，不共享 CARGO_TARGET_DIR。

## 统一接口与状态命名

以下名称是实现者之间的契约，不能在后续 task 自行改名。

```rust
// runtime cleanup.rs；由 Task 4 定义并从 side_conversation.rs 导出
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SideCleanupState { Pending, Complete, Failed { code: &'static str } }
#[derive(Clone)]
pub struct SideCleanupReceipt {
    rx: tokio::sync::watch::Receiver<SideCleanupState>,
}
// impl SideCleanupReceipt
// async fn wait(&mut self, grace: Duration) -> SideResult<()>
// fn state(&self) -> SideCleanupState

// app-server tickets.rs；Task 7。只含身份，不含任何对话。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EntryTarget {
    pub host_instance_id: String,
    pub parent_thread_id: String,
    pub entry_id: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SideGeneration { pub side_id: String, pub generation: u64 }
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExitState { Closing, Closed, Failed }
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitReceipt { pub target: EntryTarget, pub state: ExitState }
```

`SideCleanupReceipt` 无正文；`ExitReceipt` 只有目标和状态。Closed 的去重记录无 runner/source/transcript/replay 引用。Open 的内部 `SideGeneration` 不可被客户端构造为任意 force-close 目标；RPC 只能用票据找宿主绑定。

### Task 1: 纯快照媒体净化与能力说明

**Files:**
- Modify/Test: `yi-agent-rs/crates/yi-agent-core/src/side_context.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/lib.rs`

**Interfaces:**
- Consumes: 现有 `snapshot_for_side(&Session) -> Result<SideContextSnapshot, SideContextError>`、`forkable_messages`、`ContentBlock`。
- Produces: `sanitize_side_blocks(blocks: &mut [ContentBlock])`（crate 内可见）；现有 snapshot/config 接口签名不变。

- [ ] **Step 1 — 写 RED：嵌套 URL 被替换且父保持不变。** 在现有 inline tests 新增：

```rust
// Task1 将 ImageDetail 加入现有 inline tests 的 yi_agent_core/message 导入。
#[test]
fn side_snapshot_never_exports_url_media_or_mutates_parent() {
    let parent = Session::from_messages(vec![Message {
        role: Role::User,
        content: vec![ContentBlock::Image {
            source: ImageSource::Url("https://invalid.test/a.png".into()),
            detail: ImageDetail::High,
            path: Some("private.png".into()),
        }],
    }]);
    let before = serde_json::to_vec(parent.messages()).unwrap();
    let side = snapshot_for_side(&parent).unwrap();
    assert!(matches!(&side.messages[0].content[0],
        ContentBlock::Text(t) if t.contains("图片内容未包含")));
    assert_eq!(serde_json::to_vec(parent.messages()).unwrap(), before);
}
```

再加完整 ToolUse/ToolResult 配对用例（复用本文件 `tool_use`/`tool_result`，将 result.content 改为同款 Image），断言递归净化；Base64 用例断言 source 内容不变且 path=None；未配对尾部仍过滤，boundary 仅一次。

- [ ] **Step 2 — 运行 RED。** `cargo test --all-features -p yi-agent-core side_snapshot_never_exports_url_media_or_mutates_parent`。预期现有 Image 留存断言失败，不是测试编译错误。
- [ ] **Step 3 — GREEN：只对 fork 结果递归净化。**

```rust
pub(crate) fn sanitize_side_blocks(blocks: &mut [ContentBlock]) {
    for block in blocks {
        match block {
            ContentBlock::Image { source: ImageSource::Url(_), .. } => {
                *block = ContentBlock::Text("图片内容未包含在快照中".into());
            }
            ContentBlock::Image { path, .. } => *path = None,
            ContentBlock::ToolResult { content, .. } => sanitize_side_blocks(content),
            _ => {}
        }
    }
}
```

`forkable_messages` 后、boundary 前遍历 messages.content；顶层导入精确改为 `use crate::message::{ContentBlock, ImageSource, Message, Role};`，tests 的既有message导入加ImageDetail。不对父执行 replace/push。将 SIDE_SYSTEM_POLICY 改为“只读进入时对话快照；可连续提问；无工具、不补读文件/链接、不跟随更新；未知进度明确未知；退出丢弃”；保留父提示词后缀幂等及 inherited history 仅参考文案。同步旧注释，不再说 side 重新 resolve skills。

- [ ] **Step 4 — 范围 GREEN。** `cargo test --all-features -p yi-agent-core side_context`，预期新媒体/父不变/安全 fork/边界测试全部通过，已有图片期望迁移为新语义。
- [ ] **Step 5 — 门禁并提交。** 使用统一提交门禁，message：`feat: constrain side snapshots to embedded conversation content`。

### Task 2: 缓存父已渲染配置，提供只读装配句柄

**Files:**
- Modify/Test: `yi-agent-rs/crates/yi-agent-core/src/agent.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/lib.rs`

**Interfaces:**
- Produces：下列 core 类型与 `Agent::snapshot_bindings(&self) -> AgentSnapshotBindings`。

```rust
#[derive(Clone)]
pub struct AgentSnapshotBindings {
    pub session: Arc<Mutex<Session>>,
    pub provider: Arc<dyn Provider>,
    pub effective_config: Arc<Mutex<AgentConfig>>,
    pub provider_gate: Option<Arc<dyn ProviderTurnGate>>,
}
```

- getter 只克隆句柄，不调用 provider 或 live settings source。缓存不在 Debug/trace 中输出 prompt；配置字段不包含认证凭证。

- [ ] **Step 1 — RED：装配值、更新值与 provider 身份。** 在 agent inline tests（已有 provider fixture）新增测试，从 Agent::new 后取 bindings，断言 Arc::ptr_eq(session_handle, bindings.session)、provider 指针相等、缓存 system_prompt=装配值。调用 `set_system_prompt(Some("updated".into()))` 后断言同一 bindings cache 更新。复用本文件 live settings 测试 source，让父正常请求采用 live-prompt 后断言 cache=live-prompt；读 getter 不增加该 source 调用计数。

```rust
// 在已有 set_system_prompt_is_used_by_next_run 测试中插入：
let published = agent.snapshot_bindings();
agent.set_system_prompt(Some("refreshed".into()));
assert_eq!(published.effective_config.lock().unwrap().system_prompt.as_deref(),
           Some("refreshed"));
assert!(Arc::ptr_eq(&published.session, &agent.session_handle()));
```

- [ ] **Step 2 — RED 运行。** `cargo test --all-features -p yi-agent-core set_system_prompt_is_used_by_next_run`，新增 getter 尚未定义会编译失败；定义只有静态值的最小 getter 后必须再见“更新值不相等”的行为 RED。
- [ ] **Step 3 — GREEN：Agent 初始化一个 cache，传给现有 core run task。** `Agent::new` 从 config.clone 初始化；set_system_prompt 和其他实际修改装配配置的 setter 更新 cache。在 THINK 请求最终 `effective` 决定后、构造 ProviderRequest 前写入：

```rust
// 现有 run task 额外收到 Agent 的 effective_config 克隆。
*effective_config.lock().unwrap_or_else(|e| e.into_inner()) = effective.clone();
```

不将普通 getter 改成重装配、不调用 live.snapshot 来取旁问 prompt。run 拥有 cache Arc，父 rebuild 得到新 cache 与新 provider；宿主随后原子替换整组 bindings。缓存写无 await，不与网络或 Session 长持锁；所有新增锁顺序固定“发布器→配置→Session”，写 cache 时不持 Session 锁。
- [ ] **Step 4 — GREEN 与热更新回归。** `cargo test --all-features -p yi-agent-core live_` 以及上面的 setter test。证明主任务正常 hot apply 仍有效，旁问读取 cache 不触发扫描。
- [ ] **Step 5 — 门禁并提交。** message：`feat: expose cached parent configuration for side snapshots`。

### Task 3: 空工具装配与不改写父的固定来源

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/side_conversation.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/side_conversation/policy.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/side_conversation/source.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/side_conversation/runner.rs`
- Modify/Test: `yi-agent-rs/crates/yi-agent-runtime/src/side_conversation/tests.rs`
- Modify/Test: `yi-agent-rs/crates/yi-agent-runtime/src/side_conversation/runner/tests.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/side_conversation/test_support.rs`

**Interfaces:**
- Produces `build_side_tool_setup() -> ToolRegistry`；`SideToolSetupFactory::new() -> Self`；`build_for_turn(&self, cancel: CancellationToken) -> SideResult<ToolRegistry>`（保留 per-turn 调用形状，输出永远空）。
- `EffectiveSideSource` 加 `effective_config: Option<Arc<Mutex<AgentConfig>>>`，由 Task 2 bindings 提供；其他 fixture 明确 None。`SideSourcePublisher::snapshot()` 返回时清为 None；源 cache 不被 side 保留。
- runner 不再调用 factory.source()，自身已有 captured source；删除 factory 的 access/pool/owner/source 字段与 web 注册分支，现有宿主调用点随签名一并迁移以保持可编译。

- [ ] **Step 1 — RED：父逐字节不变、empty registry。** 复用 runtime `source_with_history`，在 source/tests 所在现有 tests.rs 加：

```rust
// 本文件现有导入增加 yi_agent_core::Message；tokio test 已启用。
#[tokio::test]
async fn side_publisher_fork_never_replaces_parent_messages() {
    let source = source_with_history("主问题", "主进展");
    let parent = source.session.clone();
    // 制造应被安全 fork 过滤的尾部；父仍必须保留它。
    parent.lock().unwrap().push(Message::assistant(vec![ContentBlock::ToolUse {
        id: "unfinished".into(), name: "read".into(), input: serde_json::json!({}),
    }]));
    let before = serde_json::to_vec(parent.lock().unwrap().messages()).unwrap();
    let captured = SideSourcePublisher::new(source).snapshot().await.unwrap();
    assert!(!Arc::ptr_eq(&parent, &captured.session));
    assert_eq!(serde_json::to_vec(parent.lock().unwrap().messages()).unwrap(), before);
}
#[test]
fn side_registry_has_no_tools() {
    assert!(build_side_tool_setup().is_empty());
    assert!(SideToolPolicy::default().allowed_names().is_empty());
}
```

新空工具签名先产生 compile RED；改最小签名后仍应看到父不变的行为 RED。runner CapturingProvider（现有 test_support）实际请求断言 requests[n].tools.is_empty()、第二问同父原快照、不包含父后来新增消息，父原 Session 无旁问问答。
- [ ] **Step 2 — RED 运行。** `cargo test --all-features -p yi-agent-runtime side_publisher_fork_never_replaces_parent_messages`；记录当前共享 replace_messages 导致断言失败。
- [ ] **Step 3 — GREEN：去掉父写路径和动态 side prompt。**

```rust
// source.rs snapshot_with：读取并验证 revision 后
let mut captured = source;
if let Some(cache) = &captured.effective_config {
    captured.config = cache.lock().unwrap_or_else(|e| e.into_inner()).clone();
}
captured.effective_config = None;
captured.session = Arc::new(Mutex::new(Session::from_messages(messages)));
// 不得再 captured.session.lock().replace_messages(...)
```

缓存读取纳入 revision 前后检查；短锁内预算预检和 safe clone，后续净化/构造使用独立副本。runner 追加 boundary 一次，用 Task 1 side_agent_config 固定 prompt，不挂 source.live；继续 provider gate 与 ManagedRunLimits。`SideToolPolicy::allows` 永远 false、allowed_names 空；幻觉工具调用只得到 core 的未知工具错误，不转父 registry，不产生审批。历史工具证据保留，不重放执行。
- [ ] **Step 4 — 范围 GREEN。** `cargo test --all-features -p yi-agent-runtime side_`；迁移原白名单/热 side prompt 断言，不删除预算/隔离覆盖。schema 空、父不变、连续追问、固定 prompt 均实际检查 ProviderRequest。
- [ ] **Step 5 — 门禁并提交。** message：`feat: run side questions without external tools or live prompts`。

### Task 4: 无附件读取的资源准备与逐槽 cleanup receipt

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-runtime/src/side_conversation/cleanup.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/side_conversation.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/side_conversation/resources.rs`
- Modify/Test: `yi-agent-rs/crates/yi-agent-runtime/src/side_conversation/resources/tests.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/side_conversation/runner.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/side_conversation/registry.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/side_driver.rs`

**Interfaces:**
- Produces 前文 SideCleanupReceipt/State；`SideHostResources::cleanup_receipt(&self, side_id: &str) -> SideResult<SideCleanupReceipt>`；`retry_cleanup(self: &Arc<Self>, side_id: &str) -> SideResult<SideCleanupReceipt>`。
- `PreparedSideResources` 只含 `lease: Arc<SideResourceLease>, binary_bytes: usize`。现有 prepare 签名保持，取消已触发即拒绝，核算内嵌媒体后返回，不创建目录/SideReadAccess/复制附件。SideResourceLease 的 dir 仅用于兼容清理旧残留。
- receipt.watch sender 存 SlotEntry，receiver 不持 lease；最后一个 owner drop 才开始清理，成功才移 slot；receipt 独立可保留 Complete 无正文。

- [ ] **Step 1 — RED：无文件访问与失败仍占槽。** 用 resources tests 现有 TempDir/host fixture，给 source.attachments 放一个确定不存在但有匹配 manifest 的路径；prepare 应成功、binary_bytes 只等于内嵌数据、目录不存在。新增删除操作注入点 `#[cfg(test)] new_with_cleanup`，其签名与实现为：

```rust
// 仅 tests 可用，正常 new 固定使用 remove_tree。
fn new_with_cleanup(
    instance_home: PathBuf,
    pool: Arc<SideBlockingPool>,
    remove: Arc<dyn Fn(&Path) -> std::io::Result<()> + Send + Sync>,
) -> SideResult<Arc<SideHostResources>>;
// 故障闭包
let remove = Arc::new(|_: &Path| Err(std::io::Error::other("injected")));
```

reserve_slot → 先取 receipt → drop lease → wait，断言 Err、receipt.state=Failed、slots_in_use=1；切换注入闭包的 AtomicBool 后 retry_cleanup，断言 Complete、槽归零。额外 Arc lease 未 drop 时 receipt Pending；不存在目录作为成功处理；取消/超预算 prepare 不访问附件。
- [ ] **Step 2 — RED 运行。** `cargo test --all-features -p yi-agent-runtime side_conversation::resources`；先建立注入签名，再记录现实现删除失败 slots.remove 导致期望 1 实际 0 的 RED。
- [ ] **Step 3 — GREEN：收据与有限排空。**

```rust
impl SideCleanupReceipt {
    pub fn state(&self) -> SideCleanupState { self.rx.borrow().clone() }
    pub async fn wait(&mut self, grace: Duration) -> SideResult<()> {
        tokio::time::timeout(grace, async {
            loop {
                match self.state() {
                    SideCleanupState::Complete => return Ok(()),
                    SideCleanupState::Failed { code } => return Err(SideError::new(
                        SideErrorReason::ResourceLimit, code)),
                    SideCleanupState::Pending => {}
                }
                self.rx.changed().await.map_err(|_| SideError::new(
                    SideErrorReason::ResourceLimit, "cleanup_channel_closed"))?;
            }
        }).await.map_err(|_| SideError::new(
            SideErrorReason::ResourceLimit, "cleanup_timeout"))?
    }
}
```

`rx: tokio::sync::watch::Receiver<SideCleanupState>` 私有字段。Drop 任务在现有 blocking pool 执行，删除失败只写 Failed，保留 slot 和重试目录，不能清除 Weak owner 记账。retry 只允许 lease Weak 已无 owner且没有进行中 cleanup；失败池提交也记录 Failed。一个 shared deadline 等本代收据，不累计多个 5 秒；全局 shutdown 不在 Tokio reactor 同步等。
- [ ] **Step 4 — GREEN。** 上述 resources 测试及 `cargo test --all-features -p yi-agent-runtime side_`，迁移附件副本旧断言为“没有复制”；保留 inline 64 MiB、8 resource slots、文本预算和其他 host 配额。
- [ ] **Step 5 — 门禁并提交。** message：`fix: confirm per-side resource cleanup before releasing slots`。

### Task 5: runner close 必须证明真实 quiescence

**Files:**
- Modify/Test: `yi-agent-rs/crates/yi-agent-core/src/managed_run.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/side_conversation/runner.rs`
- Modify/Test: `yi-agent-rs/crates/yi-agent-runtime/src/side_conversation/runner/tests.rs`

**Interfaces:**
- Consumes Task 4 receipt，现有 `interrupt`、`wait_quiescent`、`last_turn_was_joined`。
- Produces core `ManagedRunControl::stop_and_join_bounded(&mut self, grace: Duration) -> bool`：true仅表示root/children均joined；false表示Pending、仍持owner。只用于side，原stop_and_join调用者行为保留。
- Produces 现有 `SideConversationRunner::close() -> SideResult<()>` 的更强语义：成功=真实核心 task joined；失败仍 Closing，保留未结束 task owner，重试可继续 wait；不由 runner 提前移 host slot。

- [ ] **Step 1 — RED：成功 close 要求 pending provider stream dropped。** 复用已有 Fixture/source_with_pending_provider：

```rust
#[tokio::test(start_paused = true)]
async fn side_close_proves_core_join_and_stream_drop() {
    let fixture = Fixture::new();
    let (source, provider) = source_with_pending_provider();
    let mut runner = fixture.runner(source).await;
    let events = runner.start_turn("a".into(), "问题".into()).await.unwrap();
    provider.entered().await;
    runner.close().await.unwrap();
    assert!(provider.dropped());
    assert!(runner.last_turn_was_joined());
    assert_eq!(runner.status(), SideStatus::Closed);
    drop(events);
}
```

另加受控 join 失败测试：在 runner tests 可见 Pump completion 中注入 `joined=false`，断言 close Err、status Closing、资源 lease 仍持有、不能 start_turn。不要只用 elapsed/sleep 猜取消完成。
- [ ] **Step 2 — RED 运行。** `cargo test --all-features -p yi-agent-runtime side_close_`。普通 pending 路径可已有通过，必须记录 injected unjoined 路径当前错误地 Closed 的 RED。
- [ ] **Step 3 — GREEN：先 cancellation 后 join，失败不丢 owner。** core control增加root_joined布尔字段（初始false），新方法使用同一个deadline；成功消费root结果立即置true，children tracker.wait取消安全，超时abort但保留control，重试跳过已消费root。现有stop_and_join同步维护root_joined以免重复轮询；不改变其既有grace语义。

```rust
pub async fn stop_and_join_bounded(&mut self, grace: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + grace;
    self.cancel(); // 本run取消并abort已知children
    if !self.root_joined {
        if tokio::time::timeout_at(deadline, &mut self.root).await.is_err() {
            self.root.abort();
            return false; // 未join，保留handle给同owner下次排空
        }
        self.root_joined = true; // 正常/取消/panic都已确认实际task结束
    }
    tokio::time::timeout_at(deadline, self.children.tracker.wait()).await.is_ok()
}
```

core inline测试：完成root的第二次调用不panic；非完成root超时false、仍有handle；释放受控task后重试true；children pending时false；总耗时按一个deadline。runtime close：

```rust
// runner.close：closing=true 后已有 wait_quiescent
self.wait_quiescent().await?;
if self.active.is_some() && !self.last_turn_was_joined() {
    self.status = SideStatus::Closing;
    return Err(SideError::new(SideErrorReason::SideClosing, "core_join_unconfirmed"));
}
// 仅在 joined 后清 active/history 中可释放的引用并设 Closed。
```

修 `wait_quiescent`：当前代码读取 completion 后无条件 active=None；改为仅 joined=true 才清 active，joined=false 保留 draining owner及完成订阅；用已记账标记保证重试不重复增加usage/completed_turns。未开始过 turn（没有需要 join 的owner）的空闲close允许成功，不能把初始 last_turn_joined=false 当失败。修 Pump 的 owner：调用新 `stop_and_join_bounded(SIDE_STOP_GRACE)`，替换当前 `stop_and_join(...).is_ok()`；false 时不能结束并丢control，转为原代draining owner，持续接受精确重试，直到实际 completion。首次 5 秒超时返回清理失败；后续精确退出重试继续等同一 owner。cancel 等待 provider gate 也可退出。停止单轮仍回本次 Idle，exit 才 Closed；主 provider Arc 从不全局 close。
- [ ] **Step 4 — GREEN 与资源联合。** `cargo test --all-features -p yi-agent-runtime side_`；测试至少覆盖正常结束后 close、running close、interrupt 后 close、重复 close、unjoined、等待准入取消、无消费者 stream 背压。
- [ ] **Step 5 — 门禁并提交。** message：`fix: retain side owners until cancellation is joined`。

### Task 6: 发布真实活跃父来源，冷父不扫描项目

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/side_conversation/live_parents.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/side_conversation.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/side_conversation/source.rs`
- Modify/Test: `yi-agent-rs/crates/yi-agent-app-server/src/side_conversation/source/tests.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/session.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/transport/mod.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/turn/start.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/turn/command.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/driver/turn_driver.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/driver_resume.rs`
- Modify/Test: `yi-agent-rs/crates/yi-agent-app-server/src/server/session_driver_tests.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`

**Interfaces:**
- `LiveSideParents` 持 `Mutex<HashMap<String, Arc<SideSourcePublisher>>>`；`new() -> Self`、`get(&self, parent: &str) -> Option<Arc<SideSourcePublisher>>`、`publish(&self, parent: String, publisher: Arc<SideSourcePublisher>)`、`remove_if(&self, parent: &str, publisher: &Arc<SideSourcePublisher>)`。remove_if 用 Arc::ptr_eq，不让旧 driver 退出移掉新发布。
- `WorkspaceResolver::new(workspaces, base, live: Arc<LiveSideParents>) -> Self`；live 存在但不可用返回其错误，不偷偷 fallback cold。
- `ThreadSession` 持 `side_source: Option<Arc<SideSourcePublisher>>`；disabled host 不构造发布器。driver 参数额外同一 publisher 和可用的 live map（Option），负责 replace/invalidate。
- `source_from_live_agent` 改为只用 `agent.snapshot_bindings()` 和宿主已知元数据；provider/cache/gate 不 rebuild。不改变返回 EffectiveSideSource 的接口，设置 effective_config=Some(cache)。

- [ ] **Step 1 — RED：已提交但未落盘的父消息进入新快照。** source tests 用现有 Agent/provider fixture，先 source_from_live_agent 发布，再在 agent.session_handle().lock().push 新完整消息，resolve live 后断言包含它；同时记录父磁盘内容 hash 不变、Arc::ptr_eq(provider) 为 true。冷父在规则/skills 路径放读取会失败的 fixture，strict history 仍成功，prompt 只有已知用户配置+side policy，不含规则 sentinel。

```rust
// LiveSideParents inline tests：旧 driver 退出不能移除替换值。
let map = LiveSideParents::new();
// p1/p2 为用 source tests 已有 fixture 构造的两个 publisher。
map.publish("parent".into(), p1.clone());
map.publish("parent".into(), p2.clone());
map.remove_if("parent", &p1);
assert!(Arc::ptr_eq(&map.get("parent").unwrap(), &p2));
```

p1/p2 fixture 具体构造使用 runtime source 在 host tests 本地已有配置/Session，不跨 crate 调用 cfg(test) 私有工具。
- [ ] **Step 2 — RED 运行。** `cargo test --all-features -p yi-agent-app-server side_conversation::source`。生产 WorkspaceResolver 冷回读漏新消息或 provider 指针不同必须有行为 RED。
- [ ] **Step 3 — GREEN：装配接线全覆盖。** 新建/resume 各在最终 attach 后、driver spawn 前发布；run_thread_driver 两处 apply_session_command 后、MCP/theme rebuild、skills 更新 set_system_prompt 后更新整组 source（begin_update→commit）。commit 不改变既有 side captured config，不因普通 setting save 关闭 side。父 clear/delete 用既有生命周期屏障，不把失败操作当 invalidation。driver退出条件 remove_if；新 publisher generation来自父生命周期，而非每 token。

```rust
// WorkspaceResolver::resolve 的第一分支
if let Some(publisher) = self.live.get(parent) {
    return publisher.snapshot().await;
}
// cold 分支保留严格指纹读取；assemble_cold_source 不再 resolve_side_prompt。
```

TUI main 的 side_source_snapshot 同步挂缓存，startup/切模型/重建发布使用同一 bindings；清除 side 动态prompt继承。cold `build_agent_config` 使用已知 RuntimeConfig/user prompt，不读 rules、skills 或建 MCP，不 resume/修 partial/写标题；有 unavailable live 时失败而不是旧盘代替。
- [ ] **Step 4 — GREEN 与 driver 集成。** `cargo test --all-features -p yi-agent-app-server side_`、`cargo test --all-features -p yi-agent-app-server session_driver`。证明真实 start/resume 链路发布、模型替换后新 side 用新 provider、旧 side 保持旧 snapshot/prompt，主 hot settings 测试仍通过。
- [ ] **Step 5 — 门禁并提交。** message：`feat: capture live parent progress without reloading project context`。

### Task 7: 有界宿主签票表（不含 runner）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/side_conversation/tickets.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/side_conversation.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/side_conversation.rs`（新增错误 reason）

**Interfaces:**
- Produces 统一 EntryTarget/SideGeneration/ExitState/ExitReceipt，以及 `EntryTickets::new(host: String) -> Self`。
- `prepare(&mut self, parent: &str, now: Instant) -> SideResult<EntryTarget>`。
- `bind(&mut self, target: &EntryTarget, side: SideGeneration, now: Instant) -> SideResult<SideGeneration>`：Prepared→Bound；同票重复 bind 原代则返回原代，其他代拒绝。
- `resolve(&mut self, target: &EntryTarget, now: Instant) -> SideResult<Option<SideGeneration>>`：Prepared返回None，Bound返回Some，Closed/未知/过期拒绝 open。
- `revoke(&mut self, target: &EntryTarget, now: Instant) -> SideResult<Option<SideGeneration>>`：Prepared撤销；Bound返回准确代次；Closed/已淘汰返回None；host/已知票据父不匹配拒绝。
- `finish(&mut self, side: &SideGeneration, now: Instant)`：本代所有绑定票据转 Closed，身份无正文；计数满足上限。
- errors：`entry_expired`、`entry_limit`、`entry_mismatch` 加 SideErrorReason；未知票据 open 使用 entry_expired，不泄露其他父身份。

- [ ] **Step 1 — RED：撤销/淘汰永不重建。** tickets inline tests，不用 sleep：

```rust
#[test]
fn revoked_ticket_can_never_open_another_generation() {
    let now = tokio::time::Instant::now();
    let mut tickets = EntryTickets::new("host".into());
    let target = tickets.prepare("p", now).unwrap();
    assert_eq!(tickets.revoke(&target, now).unwrap(), None);
    assert!(tickets.resolve(&target, now).is_err());
    let next = SideGeneration { side_id: "new".into(), generation: 2 };
    assert!(tickets.bind(&target, next, now).is_err());
}
```

再加 60秒边界、5分钟 Closed 淘汰、256/32全部记录、满且全pin拒绝、Closed LRU让位、错host/父、不同票同代、finish只改精确代、prepare无来源 resolver（类型不持resolver，故零调用）。
- [ ] **Step 2 — RED 运行。** `cargo test --all-features -p yi-agent-app-server revoked_ticket_can_never_open_another_generation`，缺新类型 compile RED。
- [ ] **Step 3 — GREEN：一个 map＋状态，不用未知 id 自动插入。**

```rust
// tickets 私有状态
#[derive(Clone)]
enum TicketState {
    Prepared { expires: Instant },
    Bound(SideGeneration),
    Closed { at: Instant },
}
// Record { parent: String, state: TicketState }；key 为随机 UUID string。
// prepare：prune expired Prepared / old Closed，再只淘汰 Closed 让位；
// 先检查 parent<32 和 total<256，随后 UUID::new_v4() 插入 Prepared。
// bind/resolve：get_mut 失败即 entry_expired，永不 entry(...).or_insert。
```

`revoke` 未消费改Closed，不碰父registry；Bound是否Closing由registry权威决定，票据仅定位。Closed finish 不持异常清理owner；cleanup未完成不能finish。host新实例不接受旧票，退出旧host返回entry_mismatch而不是假closed。除no-op未知退出外都验证已知票据父身份，仍需 RPC Control。
- [ ] **Step 4 — GREEN。** `cargo test --all-features -p yi-agent-app-server side_conversation::tickets`，预期所有时间/容量/ABA 用例通过。
- [ ] **Step 5 — 门禁并提交。** message：`feat: issue bounded side entry tickets with generation bindings`。

### Task 8: registry 生命周期屏障与异步排空协调器

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/side_conversation/lifecycle.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/side_conversation/registry.rs`
- Modify/Test: `yi-agent-rs/crates/yi-agent-app-server/src/side_conversation/registry/tests.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/side_conversation/replay.rs`
- Modify/Test: `yi-agent-rs/crates/yi-agent-app-server/src/side_conversation/replay/tests.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/side_conversation.rs`

**Interfaces:**
- `SideRegistry::prepare_entry(&self, parent: &str) -> SideResult<EntryTarget>`。
- `open_entry(self: &Arc<Self>, target: &EntryTarget) -> SideResult<SideSummaryWire>`；旧无票 open不作公开生产创建路径。
- `exit_entry(self: &Arc<Self>, target: &EntryTarget) -> SideResult<ExitReceipt>`：标记本代并spawn清理，首次可立刻返回Closing；closed/no-op返回Closed；失败重试同代排空。
- Lifecycle owner 持 `SideGeneration`、cancel token、构造 JoinHandle、runner/转发 JoinHandle、资源receipt和最小状态；创建前分配 sideId/generation。
- 同一 registry state mutex 内含 parents/tickets/代状态；不得在单独 ticket 锁与父锁之间出现 bind-create gap。网络/磁盘/runner.close不在该锁或主 RPC task 内 await。

- [ ] **Step 1 — RED：Creating 退出、旧票重试、第二环境阻断。** 现有 `registry_fixture()` 增一个 Arc 包装；prepare_entry后用 barrier resolver卡source，spawn open_entry，等待entered后exit，断言Closing、同父新票open拒绝SideClosing、首问provider call_count=0；释放barrier后重试exit直到Closed，重入获得新sideId。

```rust
#[tokio::test]
async fn old_exit_ticket_does_not_close_the_next_side() {
    let (_store, _home, raw) = registry_fixture().await;
    let registry = Arc::new(raw);
    let old = registry.prepare_entry("p1").unwrap();
    let first = registry.open_entry(&old).await.unwrap();
    let _ = registry.exit_entry(&old).await.unwrap();
    // 本测试用 receipt 轮询，固定 2 秒总预算，不任意 sleep 后猜成功。
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if registry.exit_entry(&old).await.unwrap().state == ExitState::Closed { break; }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    let next = registry.prepare_entry("p1").unwrap();
    let second = registry.open_entry(&next).await.unwrap();
    assert_ne!(first.side_id, second.side_id);
    assert_eq!(registry.exit_entry(&old).await.unwrap().state, ExitState::Closed);
    assert_eq!(registry.open_entry(&next).await.unwrap().side_id, second.side_id);
}
```

迁移现有 open fixtures为prepare/open_entry，不留测试-only路径掩盖生产无票据拒绝。加双票共享、waiter失败不loop下一代、关闭期间拒绝readItems/submit、删除失败占父、转发消费者阻塞、close_all/父屏障 rollback。
- [ ] **Step 2 — RED 运行。** `cargo test --all-features -p yi-agent-app-server old_exit_ticket_does_not_close_the_next_side`；新接口 compile RED后进一步跑barrier/占位行为 RED。
- [ ] **Step 3 — GREEN：以准确代次组织状态。**

```text
open_entry（短锁）:
  验票；Prepared 若父无环境则分配代次并bind→Creating(owner)
  父Creating/Open则bind到该代；父Closing/Failed-cleanup拒绝
  Bound只返回/等待原代结果，不loop重新创建
  锁外构造；完成时再验同代仍Creating且未cancel；否则回收构造产物
exit_entry（短锁）:
  Prepared→撤票并Closed/no-op；Bound→准确owner Closing+cancel
  禁止新turn/read正文；spawn同代cleanup；返回Closing
cleanup（锁外）:
  取消构造/runner，join真实owner，cancel并join转发
  丢replay/正文/ledger/runner，drop最后lease，等本代receipt
  成功→短锁finish票/移精确父占位→有界closed通知
  失败→只保留最小排空owner与Failed状态；下次同票重试
```

lifecycle.rs实现 owner 和 drain helper，registry保持准入/身份；不要把registry整个重写。主动cancel可不等runner mutex（runner请求正在等待准入时也取消）；每个 awaited任务有统一阶段deadline。cleanup通知用 `timeout(Duration::from_secs(1), deliver_control(...))`，确认 broadcaster取消安全：未送达的客户端通过exit/list查；不得以重试通知阻塞资源。clear/delete commit、close_all、shutdown、TTL/资源丢失统一此收尾，保留sweep_epoch和shutdown不可逆护栏。没有join证据不设Closed；创建失败也必须收据确认才释放slot。ticket binding完成须在fork前，不让票据撤销后又提交首问。
- [ ] **Step 4 — GREEN。** `cargo test --all-features -p yi-agent-app-server side_`。资源故障测试采用Task4注入，不靠真实权限/mac root差异。replay连接级token在Closing立刻无效，Closed后无正文引用，old read不会复活。
- [ ] **Step 5 — 门禁并提交。** message：`feat: dispose side generations through a unified exit barrier`。

### Task 9: prepare/open/exit RPC、新握手与生成绑定

**Files:**
- Modify/Test: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/side.rs`
- Modify/Test: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/side/tests.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/side_methods_baseline.txt`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/transport/mod.rs`
- Modify/Test: `yi-agent-rs/crates/yi-agent-app-server/src/server/tests/side.rs`
- Modify: `desktop/src/lib/protocol.ts`
- Modify: `desktop/src/lib/generated/SideOpenParams.ts`（生成）
- Create: `desktop/src/lib/generated/SidePrepareParams.ts`（生成）
- Create: `desktop/src/lib/generated/SidePrepareResult.ts`（生成）
- Create: `desktop/src/lib/generated/SideExitParams.ts`（生成）
- Create: `desktop/src/lib/generated/SideExitStateWire.ts`（生成）
- Create: `desktop/src/lib/generated/SideExitResult.ts`（生成）

**Interfaces:**
- wire全部新参数/result用camelCase、deny_unknown_fields，TS derive按protocol既有export路径。

```rust
pub struct SidePrepareParams { pub parent_thread_id: String }
pub struct SidePrepareResult {
    pub host_instance_id: String, pub parent_thread_id: String, pub entry_id: String,
}
pub struct SideOpenParams { pub parent_thread_id: String, pub entry_id: String }
pub struct SideExitParams {
    pub host_instance_id: String, pub parent_thread_id: String, pub entry_id: String,
}
#[serde(rename_all = "snake_case")]
pub enum SideExitStateWire { Closing, Closed, Failed }
pub struct SideExitResult {
    pub host_instance_id: String, pub parent_thread_id: String,
    pub entry_id: String, pub state: SideExitStateWire,
}
```

已有 summary的snake_case字段和通知/分页契约不改。prepare返回三个身份，客户端先校验响应与当前host/parent再open；open用实际宿主instance构造内部target。exit每次Control认证，不能票据代替授权。

- [ ] **Step 1 — RED：八方法flag优先、scope、旧open拒绝。** 扩展真实stdio Harness现有 every_side_method... baseline加入prepare/exit，Observe非法参数flagoff仍-32601；enabled Observe prepare/exit返回权限错误，list/readItems仍可Observe。enabled Control旧 `{parentThreadId}` open返回-32602，unknown票entry_expired。initialize断言新能力跟gate、hostId稳定。

```rust
// protocol inline tests（新结构先声明后运行行为RED）
#[test]
fn side_open_requires_a_host_issued_entry_ticket() {
    assert!(serde_json::from_value::<SideOpenParams>(
        serde_json::json!({"parentThreadId":"p"})).is_err());
    let parsed: SideOpenParams = serde_json::from_value(serde_json::json!({
        "parentThreadId":"p", "entryId":"ticket"
    })).unwrap();
    assert_eq!(parsed.entry_id, "ticket");
}
```

Harness两连接分别prepare同父→open共享；一端exit后旧票失效，再prepare新代；slow resolver期间父其他RPC仍可响应。
- [ ] **Step 2 — RED 运行。** `cargo test --all-features -p yi-agent-app-server side_open_requires_a_host_issued_entry_ticket` 与 `every_side_method_is_method_not_found_when_the_flag_is_off`。
- [ ] **Step 3 — GREEN：RPC gate→scope→parse→owned spawn。** prepare不解析父source；open/exit用Task8 owned Arc registry和SideResponseSender进入tasks.spawn，主state借用不能跨await。旧side/close保留required-nullable turn CAS，但通过相同drain协调器，不保留旧独立删除路径。握手新增 `(side_gate)()` 对应side_snapshot_lifecycle_v1。关闭gate后由close_all清理，不额外开放exit绕闸。
- [ ] **Step 4 — GREEN 与生成。** `cargo test --all-features -p yi-agent-app-server side_ && cargo nextest run --all-features -p yi-agent-app-server export_bindings && just bindings-check`；生成物检查camelCase/enum字符串，不手写修改generated。desktop protocol.ts只re-export新类型，summary兼容不变。
- [ ] **Step 5 — 门禁并提交。** message：`feat: negotiate disposable side lifecycle RPCs`。

### Task 10: 客户端无正文退出账本与旧代授权屏障

**Files:**
- Create: `desktop/src/lib/sideExit.ts`
- Create/Test: `desktop/src/lib/sideExit.test.ts`
- Modify: `desktop/src/lib/sideConversation.ts`
- Modify/Test: `desktop/src/lib/sideConversation.test.ts`
- Modify: `desktop/src/lib/sideSync.ts`
- Modify/Test: `desktop/src/lib/sideSync.test.ts`
- Modify: `desktop/src/lib/sideItems.ts`
- Modify/Test: `desktop/src/lib/sideItems.test.ts`

**Interfaces:**
- `SideHostBinding` 加 `snapshotLifecycle: boolean`，parseSideHandshake从双cap计算支持，不以旧supported绕过。
- `SideExitCoordinator` API：`constructor(onChange: () => void)`；`record(target: SideExitParams): void`；`blocked(host: string, parent: string): boolean`；`flush(binding: SideHostBinding): Promise<void>`；`hostReplaced(oldHost: string): void`；`dispose(): void`。模块仅存target/state/inflight身份，不存client正文或首问，绑定client由flush参数实时提供。
- `SideSyncController` 增 `invalidateGeneration(sideId: string, generation: number): void`；epoch失效、清pending/timers/replay，不撤销/关闭调用方RpcClient。
- store增加 `dropGeneration(hostKey: string, sideId: string, generation: number): void`；只清匹配代次、正文、草稿、usage、分页与view，不删host上别的父。

- [ ] **Step 1 — RED：断网退出blocked，closed才放行。**

```ts
import { describe, expect, it } from "vitest";
import { SideExitCoordinator } from "./sideExit";

describe("side exit intent", () => {
  it("blocks re-entry before a closed receipt", () => {
    const exits = new SideExitCoordinator(() => {});
    exits.record({hostInstanceId: "h", parentThreadId: "p", entryId: "e"});
    expect(exits.blocked("h", "p")).toBe(true);
    expect(exits.blocked("h", "another")).toBe(false);
    exits.dispose();
  });
});
```

再用fake binding.request依次返closing、failed、closed，flush后前两次保持blocked，closed放行；错响应host/entry不放行；旧client回包不当新binding结果；不同父/host互不影响。sync tests pending分页/submit响应在invalidateGeneration后不写store，unknown SideStatus不造view；sideItems URL图片替换文本不产生img，Base64仍允许。
- [ ] **Step 2 — RED 运行。** `npx vitest run src/lib/sideExit.test.ts src/lib/sideSync.test.ts src/lib/sideItems.test.ts`，模块missing后最小API下继续行为RED。
- [ ] **Step 3 — GREEN：按JSON tuple键记录，不保正文。**

```ts
const exitKey = (t: SideExitParams) => JSON.stringify([
  t.hostInstanceId, t.parentThreadId, t.entryId,
]);
// record 去重；flush 只处理同host的记录，scope observe不发exit。
// connected=false保留；request("side/exit", target)；验证返回三身份。
// state==="closed"才删除；closing/failed保留并通知用户，不自动open。
```

flush单目标最多一条inflight；在现有连接恢复/控制器timer触发下一次对账，disconnected立即停计时，不新增无上限重试队列。失败显示可重试按钮；不存浏览器持久问答、不导出旧缓存。exit unresolved数量不超过当前UI已打开/正在进入的目标，用户block期间不能新增同父意图。hostReplaced清旧最小目标并提示“旧宿主清理无法核实”，不用新host请求旧exit。
- [ ] **Step 4 — GREEN。** 上述Vitest加sideConversation tests；`npx tsc --noEmit`。新字段所有binding fixture明确填，不用optional true默认隐藏旧宿主不支持。
- [ ] **Step 5 — 门禁并提交。** message：`feat: discard side content while tracking exact exit receipts`。

### Task 11: 桌面/手机统一进入退出与重连接线

**Files:**
- Modify: `desktop/src/hooks/useSideConversationController.ts`
- Modify/Test: `desktop/src/hooks/useSideConversationController.test.tsx`
- Modify: `desktop/src/hooks/useSideConversation.ts`
- Modify/Test: `desktop/src/hooks/useSideConversation.test.tsx`
- Modify: `desktop/src/hooks/useHandshakeRecovery.ts`
- Modify: `desktop/src/App.tsx`
- Modify/Test: `desktop/src/App.test.tsx`
- Modify: `desktop/src/components/SideBackBar.tsx`
- Modify/Test: `desktop/src/components/SideBackBar.test.tsx`
- Modify: `desktop/src/lib/sideCommands.ts`
- Modify/Test: `desktop/src/lib/sideCommands.test.ts`
- Modify: `desktop/src/lib/currentConversation.ts`

**Interfaces:**
- controller deps增加Task10共享 `exits: SideExitCoordinator`，`controllerFor(binding: SideHostBinding, parentId: string): SideSyncController` 按host+parent获取既有同步器。
- 统一 `leaveSide(reason: "back" | "navigation" | "unmount"): void`；现有back/selectMain/requestClose/confirmClose入口迁移此函数，删除旧close二次确认类型与UI。
- 一次停留record包括entry token、captured host binding、parent、首问意图与可选summary；只在退出前保留首问，退出立即清。
- App owner持 exits 与side controllers，重连握手就绪→exits.flush(binding)→仅仍未退出停留reconcile。事件accept仅授权停留，不能任意controllerFor创建。

- [ ] **Step 1 — RED：迟到open不提问并补精确exit。** 在现有 controller test `setup`中加入exits和snapshotLifecycle，fakeClient自动prepare返三个身份。复用可控Promise（直接构造resolve变量），测试：

```ts
it("backs out during open without submitting a late first question", async () => {
  let resolveOpen!: (value: unknown) => void;
  const pending = new Promise(resolve => { resolveOpen = resolve; });
  const s = setup({responses: {
    "side/prepare": () => ({hostInstanceId: INSTANCE, parentThreadId: "t1", entryId: "e1"}),
    "side/open": () => pending,
    "side/exit": () => ({hostInstanceId: INSTANCE, parentThreadId: "t1", entryId: "e1", state: "closing"}),
  }});
  let opening!: Promise<boolean>;
  act(() => { opening = s.hook.result.current.open(s.binding, "t1", "首问"); });
  await act(async () => { await Promise.resolve(); });
  act(() => s.hook.result.current.back());
  await act(async () => {
    resolveOpen({side_id:"s1", parent_thread_id:"t1", generation:1, status:"idle"});
    await opening;
  });
  expect(s.client.requests.some(r => r.method === "side/exit")).toBe(true);
  expect(s.client.requests.some(r => r.method === "side/turn/start")).toBe(false);
  expect(s.controllers.every(c => c.submit.mock.calls.length === 0)).toBe(true);
  expect(s.hook.result.current.active.kind).toBe("main");
});
```

补prepare迟到（只有撤票）、旧响应不reconcile、创建后back停止、切项目用旧client、断网清正文重连先exit、Observe本地back零RPC、双cap不足不发父turn、localflagoff当前remote不back、StrictMode立即重新挂载零exit、最终unmount尽力exit、临时disconnect不exit。
- [ ] **Step 2 — RED 运行。** `npx vitest run src/hooks/useSideConversationController.test.tsx src/hooks/useSideConversation.test.tsx src/App.test.tsx`。先迁fake握手，再确认back只有指针时side/exit不存在的行为RED。
- [ ] **Step 3 — GREEN：prepare→验意图→open→验意图→首问。**

```ts
// leaveSide核心顺序（字段从当前停留record捕获，不从新选中session推断）
const old = currentEntry;
currentEntry = null; // 同步失效，任何迟到结果不能复活
if (old) {
  old.question = "";
  if (old.target && old.binding.scope !== "observe") exits.record(old.target);
  if (old.summary) {
    controllerFor(old.binding, old.parentId).invalidateGeneration(
      old.summary.side_id, old.summary.generation);
    store.dropGeneration(old.binding.key, old.summary.side_id, old.summary.generation);
  }
  void exits.flush(old.binding); // 方法内部吸收错误并发布收据，不unhandled reject
}
// 接着回主视图/导航，不等待网络
```

prepare迟到且停留已撤销仍record并flush其目标（无环境副作用）；open迟到同样补exit但不读正文、不存草稿、不submit。已有环境定位只用于本次仍授权停留，不把store缓存当可恢复环境。返回、/side-close、selectMain/新建普通session/切项目/宿主/删除入口先leave再导航。Observe离开只invalidate local view；Control共享终态清匹配一代。Stop仍interrupt不leave。

owner卸载：订阅effect cleanup仅dispose订阅；最终应用owner用可撤销 `setTimeout(..., 0)` 终结检查，立即同owner重新挂载清timer；真实pagehide尽力leave，不因visibilitychange或WebSocket断线leave。明确退出后on handshake先flush，任何恢复仅限当前有效停留；关闭localflag只处理local bindings，远端不受本机开关影响。文案采用spec原句，无继续旁问/返回保留提示；不做常驻side侧栏模块。
- [ ] **Step 4 — GREEN。** 上述tests＋`npx vitest run src/lib/sideCommands.test.ts src/components/SideBackBar.test.tsx && npx tsc --noEmit && npm run build`。真实手机布局行为在Task13验收，不用jsdom代替。
- [ ] **Step 5 — 门禁并提交。** message：`feat: make desktop and mobile side navigation dispose the session`。

### Task 12: TUI 可取消创建与退出即销毁

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/side_driver.rs`
- Modify/Test: `yi-agent-rs/crates/yi-agent/src/side_driver/tests.rs`
- Modify/Test: `yi-agent-rs/crates/yi-agent/src/tui/side.rs`
- Modify/Test: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`

**Interfaces:**
- Open增加本次 `entry_id: String`；保持request_id作为首问去重，不混用二者。仅新停留分配entry，当前停留重复/side沿用它只定位，携带的重复首问不自动提交。
- Submit改为 `Submit { entry_id: String, request_id: String, text: String }`，actor拒绝不匹配当前entry的迟到提交；Interrupt继续使用精确ConversationTarget+turn。
- `ConversationEvent` 增 `entry_id: Option<String>`；主事件None、所有side事件Some本次entry。更新其构造点，TUI先核对当前entry授权再处理payload；Creating完成/关闭失败也携相同entry，不能任意SideStatus造view。
- 新 `SideDriverCommand::Leave { entry_id: String }` 关闭一次环境，不结束整个actor；原Exit仅应用退出。
- Creating owner持 entry_id/cancel/JoinHandle，Opening completion带同entry_id；UI入场授权集合仅当前entry；旧帧即使SideStatus仍丢弃。
- 用Task4逐槽receipt与Task5close；TUI本地不做网络prepare票据，entry是进程内操作代次，不接受外部未知重试来创建。

- [ ] **Step 1 — RED：创建慢时Leave仍受理。** 在side_driver tests现有Fixture上新增creating barrier（创建future在oneshot等待，不让actor整体 await），send Open(entry=A)、等待entered后send Leave(A)，断言leave被处理且cancel触发，release后provider.calls=0、slot最终0；B新entry可进，A迟到status不重建view。借助Notify/oneshot，不靠测试一秒毫无输出来推断卡死。

```rust
// tui::side inline分类测试，与真实输入循环fixture联动
#[test]
fn side_back_and_close_are_disposable_exit_commands() {
    assert!(SIDE_HELP_ENTRIES.iter().all(|(_, description)| !description.contains("保留")));
    assert!(SIDE_HELP_ENTRIES.iter().any(|(command, description)|
        *command == "back" && description.contains("丢弃")));
}
```

输入循环测试分别输入/back与/side-close，断言两者发送Leave精确entry，不发送父interrupt/审批Decision；Esc只Side Interrupt，停后可继续问。
- [ ] **Step 2 — RED 运行。** `cargo test --all-features -p yi-agent --bin yi-agent side_driver` 与 `cargo test --all-features -p yi-agent --bin yi-agent tui::side`。原actor阻塞构造测试用timeout给出明确失败，不能永久挂。
- [ ] **Step 3 — GREEN：actor select 创建completion/命令/turn。**

```text
Open A: 若已有当前A定位；无环境则spawn Creating(A)并立即回命令loop
Leave A: 校验当前entry，撤UI授权，cancel构造/runner，spawn draining
Creating完成: 若entry不再有效则回收产物，不announce、不发首问
Draining完成: joined+cleanup Complete才清slot，通知1秒有界
新的Open: draining/failed时明确拒绝，旧A Leave不作用B
```

不要用abort创建future后丢掉owner；构造blocking工作/资源由原entry跟踪。父Barrier继续先屏障、主成功commit、失败rollback；不能为取消创建破坏父失败回滚。应用Exit排空当前entry后结束actor；全程主driver独立不等待旁问。UI SideStatus/apply_event匹配entry授权再建view；back立刻清history/input/usage/scroll/pending，in_side重复/side只定位不刷新。main发布来源挂Task2缓存，release/off不创建driver/resource；help移除返回保留和后台续问。
- [ ] **Step 4 — GREEN。** 两条范围测试＋`cargo test --all-features -p yi-agent --bin yi-agent side_`；保留父审批隔离/取消token reset测试，真实终端验收后才宣布产品路径完成。
- [ ] **Step 5 — 门禁并提交。** message：`feat: dispose TUI side sessions without blocking creation cancellation`。

### Task 13: 跨端回归、变异证明与交付证据

**Files:**
- Modify/Test: `yi-agent-rs/crates/yi-agent-app-server/src/server/tests/side.rs`
- Modify/Test: `desktop/src/App.test.tsx`
- Modify/Test: `yi-agent-rs/crates/yi-agent/src/side_driver/tests.rs`
- Create: `docs/superpowers/reviews/2026-11-09-side-snapshot-lifecycle-acceptance.md`
- Modify: `docs/project-management/yi-agent-runtime.md`
- Modify: `docs/project-management/yi-agent-app-server.md`
- Modify: `docs/project-management/yi-agent-tui.md`
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/project-management/README.md`
- Modify: `docs/superpowers/specs/2026-11-09-side-snapshot-lifecycle-design.md`
- Modify: `docs/superpowers/plans/2026-11-09-side-snapshot-lifecycle-impl.md`

**Interfaces:**
- 消费全部前置接口；不新增机制。验收记录只记实际执行的命令、退出码、场景证据和未验边界，不沿用历史just ci或测试数量。

- [ ] **Step 1 — 补垂直集成 RED。** stdio真实生产start链：父stream在barrier保持运行，追加一条完整进度A未落盘，prepare/open/首问记录ProviderRequest有A；父追加B，side追问不含B；running exit→stream dropped/joined/slot零；新票重入含B且不含旧旁问。两连接共同open，Control退出使两端本代失效；Observe read允许但prepare/turn/exit拒绝。测试provider复用server已有mock，不走真实付费API。

```text
必须断言：父interrupt计数=0；父序列化历史与预期一致；
side request.tools=[]；side1第二问无B；side2含B无side1问答；
closed响应之前不放行第二环境；最后本代资源槽、replay快照、转发owner归零。
```

- [ ] **Step 2 — RED/GREEN 与变异证明。** 单独临时变异下列生产分支，每次运行对应新测试必须失败，再恢复并复跑通过；不提交变异，不用git reset覆盖他人改动。

| 变异 | 必须击中的断言 |
|---|---|
| back仅导航不record exit | Task11 side/exit调用与无首问 |
| 空registry重新注册read | Task3 ProviderRequest.tools=[] |
| snapshot重新对父replace_messages | Task3父历史逐字节不变 |
| cleanup失败也移slot/父占位 | Task4/8失败仍阻断新环境 |
| unknown票据open当首次创建 | Task7淘汰/撤销票据拒绝 |
| 去掉旧entry/generation检查 | Task8/10/12旧事件不复活或杀新代 |

- [ ] **Step 3 — 完整自动检查。** 依次运行并记录：

```bash
cd yi-agent-rs
cargo fmt --all
just ci
cd ../desktop
npx tsc --noEmit
npx vitest run
npm run build
```

不并行全workspace cargo test；just ci是唯一CI入口，不能用几条范围tests代替。失败诊断后修复、独占重跑，记录真实结果。live API仅从 `just test-live` 入口运行，配置来自实例默认model；无key为SKIPPED，不写已通过。

- [ ] **Step 4 — 真实交互验收（用户可见路径）。** dev实验开关开启、主长任务→/side问进展→连续追问→主继续→side不刷新→回答中/back→确认真实本代stream和资源结束→再进取得新进展。桌面、手机同宿主、TUI各做；手机后台/短断线不exit，断网明确退出重连先补exit；切项目/父不混client；同机关flag不影响远端。release不开入口/RPC -32601。不通过只能保留未完成，不伪称真机测试由jsdom覆盖；不能控制供应商远端物理计算的限制写入记录。

- [ ] **Step 5 — 同步文档状态并提交。** 自动和真实验收全部满足才将对应feature `[ ]`→`[x]`，附测试名/命令/代码位置，重新计算README计数；若真机/模型不可用，保持对应验收未完成并明确阻塞。更新spec/plan状态只反映已执行事实。使用统一门禁，message：`test: verify disposable side snapshot lifecycle across clients`。
- [ ] **Step 6 — 集成收尾。** 加载 requesting-code-review 与 finishing-a-development-branch；取得用户/看板合并授权与merge slot后回main `git merge --no-ff feat/side-snapshot-lifecycle`，合并结果再`cd yi-agent-rs && just ci`及desktop检查。成功才移除implementation worktree和分支；先`just reclaim`预演，只按项目规则回收垃圾，不盲目cargo clean。不在本文编写阶段自动合并。

## Spec 覆盖与完成判据映射

| spec章节/验收行 | 实施任务/证据 |
|---|---|
| §1 运行中可问、固定fork、退出丢弃 | 3、6、8、11、12、13真实链 |
| §2 覆盖旧保留/白名单，dev-only仍硬闸 | 1、3、9、11、12；13迁移注释/帮助 |
| §3 现有缺口 | 3父写路径、4清理、6live接线、8registry、11back、12actor |
| §4.1 当前父、空问题、重复定位、Closing不新建 | 7–9协议；11客户端意图；12TUI |
| §4.2 连问、未知后续信息、计费/准入 | 1prompt、3真实请求/预算、5取消准入、13验收 |
| §4.3 back/close/导航/unmount/父/flag退出与Stop区别 | 5、8、11、12；13多端与release |
| §4.4 新sideId、无旧正文/草稿/usage、旧事件隔离 | 8、10、11、12、13 |
| §5.1 safe fork/工具证据/媒体/活父/冷父/有效config | 1–3、6；13生产start未落盘消息 |
| §5.2 无工具、幻觉工具拒绝、无父审批/inbox | 3、12审批隔离、13tools变异 |
| §5.3 父不变、短锁、预算、固定prompt/实时gate、释放引用 | 2–6、8、10；13父写路径变异 |
| §6.1 唯一性、残留占位、有界空闲兜底 | 4、5、7、8、12 |
| §6.2 prepare/open/exit、TTL/总容量/ABA/兼容/双cap | 7–11；9八方法baseline |
| §6.3 join/cleanup收据/5+5+1秒/故障背压 | 4、5、8、12；13slot变异 |
| §6.4 临时断线/明确退出重连/StrictMode/死亡边界 | 10、11、13 |
| §7 同父多端、Observe、准确host/本机flag不动remote | 8–11、13 |
| §8 UI错误/创建回滚/关flag不发父命令 | 8、9、11、12 |
| §9 全验收矩阵及四类重点变异 | 13聚合，前置任务分别贡献新断言 |
| §10 不增实时跟进/导出/文件/权限重构等能力 | 全局约束；每task review拒绝扩展范围 |

## Plan 自检记录与执行交接

- [x] 文件分工与接口已定义：新增 cleanup/live_parents/tickets/lifecycle/sideExit 各有单一责任；不创建不存在的 runner/pump.rs、side_driver/actor.rs 或根层 turn_driver.rs。
- [x] 规格覆盖表包含所有章节与验收类型；核心父历史误写、票据总容量、有效prompt缓存、StrictMode、真实cleanup均有专属断言。
- [x] 无剩余已知规格阻塞；没有新机制实现或产品验收通过声明。
- [ ] 实施时选择 Subagent-Driven 或 Inline Execution，再加载相应技能启动Task1。

计划完成只指文档交付；全部13个任务仍未执行。推荐 Inline Execution 分批检查（用户此前偏好），也可选择 Subagent-Driven 逐task双阶段review；本轮不自动进入任一执行模式。
