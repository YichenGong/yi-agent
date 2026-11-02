# 看板合并续跑原会话 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 用户授权合并后由原开发 thread 续跑；冷会话严格恢复同一 ID，不可恢复或执行不确定时阻塞而非另开会话。

**Architecture:** 插件发布通用 resume_append，宿主恢复真实会话并复用投递内核。插件以现有 lease/flock 加一份最小持久在飞记录守住执行边界，匹配结果之后另一次 idle 观测才允许释放；崩溃不自动重放。宿主不解释看板业务。

**Tech Stack:** Rust、serde/serde_json、tokio、Unix flock、现有 ThreadStore、mock provider、Cargo/just；不新增第三方运行时依赖。

## Global Constraints

- 实现卡的 `thread_id` 从 merge-request 到完成不变；`origin_thread_id` 仅为需求讨论来源。
- 实现卡 `workdir` 保留开发树；合并目标树另行传入，不覆盖开发绑定。
- 忙会话不被取消、重建、interject 或抢占。
- 不增加 CLI 动词，不引入 Merging 状态，不删除开发 worktree，不 push、不自动删除源分支，不重做跨进程 exactly-once 系统。
- 仅给已有 `merge-finish` 增加离线人工恢复选项；正常模型流程不使用此选项。
- 沿用原权限和 sandbox 规则，不把 prompt 授权当成文件系统权限提升。
- worktree：`.worktrees/kanban-merge-original-session`，分支 `fix/kanban-merge-original-session`；不得在 main 修改或提交。
- spec：[2026-11-03-kanban-merge-original-session-design.md](../specs/2026-11-03-kanban-merge-original-session-design.md)。本文只计划，不执行实现。
- 实施前比较 main 与当前基线的相关源码；其他 app-server 重构若已合入，重新定位入口并复核接口，不照抄过时行号。
- 每个任务先观察真实红灯再实现；下列 Expected 是未来验收，不是本轮执行结果。不能用“过滤出0个测试”当通过。
- 每次提交前执行 `cd yi-agent-rs && cargo fmt --all && just ci`；插件修改另跑其 workspace 检查。基线 audit 失败必须另行解决后才提交，不擅加豁免。任务中的 commit 是通过这些门禁后的检查点，可在门禁修复前保留未提交工作，不绕过要求。

---

## 0. 文件职责与执行依赖

以下路径均相对仓库根。符号定位优先于行号（基线 e6aab02b）。

| 文件 | 职责 |
|---|---|
| `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs` | 动作/结果、调度、pending/lease、合并安全结算 |
| 同目录 `dispatch.rs` | capabilities 参数与 merge_finish 查询结果 |
| 同目录 **新增** `merge_attempt.rs`、修改 `lib.rs` | 单一 journal 的严格读、原子写、CAS清除；不执行 Git、不持有 board mutex |
| 同目录 `persist.rs`、`merge_lock.rs` | 严格看板写入口、holder写失败可见；flock原语保持独立 |
| 同目录 `main.rs` | daemon 启动恢复门、离线恢复 flags/实际锁、输出 |
| 同目录 `tick.rs` | 原实现 objective 与自包含合并授权 |
| `plugins/superpowers-kanban/crates/superpowers-kanban-runner/contract/driver-protocol.json` | 两端共用 wire fixtures |
| `yi-agent-rs/crates/yi-agent-app-server/src/session_driver.rs` | 通用动作/能力/类型化错误、观测去重 |
| 同 crate `src/thread_store.rs` | 严格恢复读/partial升格；旧宽松load保留 |
| 同 crate **新增** `src/server/driver_resume.rs` | ServeLauncher 冷装配；局部子模块，不搬迁整个 server |
| 同 crate `src/server.rs` | 子模块声明、ResumeAppend分派、投递失败回滚、Activation装配诊断 |
| 同 crate `src/rpc/thread.rs` | 仅当提取共同纯逻辑需要时调用共享部分；保留原RPC热中断/响应/回放语义 |
| 同 crate `src/server/session_driver_tests.rs` | 真 launcher 热冷/故障测试及真实插件串联 |
| 同 crate `src/server/tests/thread_lifecycle.rs`、`process.rs` | 原恢复/partial/watcher回归 |
| 同 crate `Cargo.toml`、`yi-agent-rs/Cargo.lock` | 仅给集成测试增加本地 runner/core dev-dependency，不加宿主运行时业务依赖 |
| `plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md`、`README.md`、`INSTALL.md` | 授权、升级、恢复与降级操作说明 |
| `docs/project-management/{superpowers-kanban,yi-agent-app-server,README}.md` | 完成证据与模块计数 |

依赖：Task 1 → Task 3；Task 2 → Task 3；Task 4 → Task 5 → Task 6；Task 1 → Task 5；Tasks 3/6 → Task 7；Tasks 1–7 → Task 8。Task 2 与 Task 4 可独立审查，但同一工作树不要并行改共享文件。所有任务先单独审查，再串联验收。

## Task 1: 双端协议、能力与类型化错误

**Files:** 修改 `service.rs`、`dispatch.rs`、`session_driver.rs`、`contract/driver-protocol.json`。

**Interfaces:**

宿主新增 `DriverActionKind::ResumeAppend { thread_id: String, workdir: String, objective: String, env: Vec<(String, String)> }`。

插件新增 `DriverAction::ResumeAppend { action_id: String, card_id: String, thread_id: String, workdir: PathBuf, objective: String, env: Vec<(String,String)> }`，card_id 不进 wire。

插件新增 `#[derive(Default)] pub struct DriverCapabilities { pub actions: Vec<String> }`；`supports(&self, action: &str) -> bool` 精确匹配。将 `BoardService::driver_step` 扩为 `(threads: &[ThreadFacts], results: &[DriverResult], sessions_running: Option<u32>, capabilities: &DriverCapabilities) -> Result<Option<DriverAction>, String>`；机械更新全部调用点，旧宿主调用路径传 default，不能默认有能力。测试中需要能力的用例显式构造列表。

宿主 `SessionLauncher::launch` 仍返回 `anyhow::Result<String>`，新增 `DriverActionFailure { code: DriverErrorCode, message: String }` 实现 Display/Error，用 anyhow downcast 提取；不改为字符串匹配。两端 `ActionResult/DriverResult` 增加 `error_code: Option<String>`（插件 serde default）。宿主 `DriverErrorCode` 枚举 Busy/ThreadNotFound/WorkdirMissing/RestoreFailed/DeliveryFailed，`as_str(&self) -> &'static str` 映射 spec 拼写。

- [ ] **1.1 写红灯测试**：在宿主 session_driver 单元测试中加入：

```rust
#[test]
fn resume_append_is_a_distinct_action() {
    let value = serde_json::json!({
        "action_id": "act-resume", "kind": "resume_append",
        "thread_id": "thread-original", "workdir": "/w/card",
        "objective": "merge authorized", "env": {"CARD": "card-1"}
    });
    let action = parse_action(&value).expect("new action must parse");
    assert!(matches!(action.kind, DriverActionKind::ResumeAppend {
        thread_id, workdir, objective, env
    } if thread_id == "thread-original" && workdir == "/w/card"
        && objective == "merge authorized"
        && env == vec![("CARD".into(), "card-1".into())]));
}
```

插件契约测试对同一 JSON 做 `to_json` 对照。fixture 添加 answers.resume_append、request.capabilities、失败结果样例；保留 launch/nothing，不删除旧 append 解析测试。补无 capability、未知 kind、旧结果缺 error_code 的默认测试。

- [ ] **1.2 观察红灯**：`cd yi-agent-rs && cargo test -p yi-agent-app-server --lib resume_append_is_a_distinct_action`；先应编译失败（新variant尚无）或断言失败；记录不是环境失败。
- [ ] **1.3 实现协议**：parse_action 新分支按现有 env 透传约定；driver_step 请求固定声明三个支持动作；错误取 `downcast_ref::<DriverActionFailure>()`，未分类 error_code=None。成功 ResumeAppend 与 Launch 都去重登记 DriverThread；Append 保持原未知失败行为。

```rust
if matches!(action.kind,
    DriverActionKind::Launch { .. } | DriverActionKind::ResumeAppend { .. }) {
    if let Some(existing) = threads.iter_mut().find(|t| t.thread_id == thread_id) {
        existing.alive = true;
    } else {
        threads.push(DriverThread { thread_id: thread_id.clone(), alive: true });
    }
}
```

在现有成功分支内执行，thread_id 为 launcher 返回值。插件只在调度阶段使用 capabilities，协议层不猜卡片语义。此阶段 ResumeAppend 的宿主分派可先明确返回 RestoreFailed，直到 Task 3 接通，不得临时 Launch。

- [ ] **1.4 绿灯/回归**：宿主 `cargo test -p yi-agent-app-server --lib session_driver`；插件 `cargo test -p superpowers-kanban-runner`。断言能力实际进入 query 参数、新variant实际被测试，而不是只解析fixture。
- [ ] **1.5 检查点**：门禁通过后 `git commit -m "feat: define resume-append driver capability"`，仅 stage 本任务文件。

## Task 2: 严格存储恢复与 partial 安全升格

**Files:** `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`（实现与本模块测试）。

**Interfaces:** 新增 `ThreadStore::restore_for_driver(&self, id: &str, expected_cwd: &Path) -> io::Result<Option<LoadedThread>>`。调用者必须持有 `acquire_thread_lock(store.root(), id)` 返回的 Arc<ThreadWriterLock>；方法不重复抢另一把锁。还须修正 acquire_thread_lock 的非 Busy flock 错误分支为 `return Err(ThreadLockError::Io(error))`，不能维持当前打印后放行；原RPC调用者的降级选择不变。全部不存在返回 None；部分存在但非法返回 InvalidData/原I/O错误。LoadedThread 沿用原类型，pending_turn 保留恢复中断标记。

- [ ] **2.1 写红灯测试**（放在 thread_store 模块，能访问现有路径方法）：

```rust
#[test]
fn driver_restore_rejects_a_corrupt_log_without_rewriting_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = ThreadStore::new(dir.path());
    std::fs::create_dir_all(store.root()).unwrap();
    let bad = b"not-json\n";
    std::fs::write(store.log_path("thread-1"), bad).unwrap();
    assert!(store.restore_for_driver("thread-1", dir.path()).is_err());
    assert_eq!(std::fs::read(store.log_path("thread-1")).unwrap(), bad);
}
```

同一任务增加表驱动 fixture：用既有合法 ThreadMeta/TurnLine 测试构造样例分别写缺meta、坏meta、错ID、错cwd、空log、meta-only、有效partial-only、坏partial、log中坏中间行、items非空但messages缺失的旧日志。必须同时有合法meta+坏log用例，避免上述用例仅因缺meta就绿。测试通过 `std::fs::read` 验证失败不覆盖文件。

- [ ] **2.2 观察红灯**：`cargo test -p yi-agent-app-server --lib driver_restore_`，应因新入口不存在失败。
- [ ] **2.3 实现单次严格读**：不要先validate再宽松load。提取 TurnLine 归并与 partial 合流为内部纯函数，让旧 load 与新方法共用；I/O策略分别保留。顺序：读取/解析meta、log、partial → 校验meta ID/canonical cwd与恢复材料 → partial首item去重 → 必要时 `append_turn` → `clear_partial` → 返回LoadedThread。strict入口不用 read_partial().ok()，不能调用吞错的 read_log。

```rust
fn read_optional_text(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}
```

定义该内部函数，三个文件严格区分 NotFound 与错误。合法空log可返回空messages；meta-only拒绝；有效partial-only须含可采纳内容。坏partial即使有好log也拒绝，不能静默丢掉可能唯一的新turn。非空items却没有可恢复messages也拒绝；有效partial-only必须有可恢复messages，不能只因JSON能解析就绿。

- [ ] **2.4 锁与partial故障测试**：为锁原语新增非 Busy 错误拒绝用例（从无效fd的flock取得真实EBADF并走共享错误分类函数），锁路径I/O失败与跨进程Busy分别覆盖；不能只测文件open失败而遗漏flock失败。第一次append成功但clear失败，第二次读取按首item ID不得重复append；append失败不得clear；clear失败不得返回可投递结果。用临时目录把应删除的partial路径换成目录制造确定性错误，不靠chmod（root下不可靠）；模拟append失败可把log路径换目录。所有fixture修改在本地tempdir。验证旧 `thread/resume` 宽松load用例语义未变。
- [ ] **2.5 绿灯/检查点**：`cargo test -p yi-agent-app-server --lib thread_store` 和 `cargo test -p yi-agent-app-server --lib thread_lifecycle`，通过门禁后 commit `feat: add strict persisted thread restoration`。

## Task 3: 原会话热追加、冷装配与投递边界

**Files:** 新增 `src/server/driver_resume.rs`；修改同 crate `src/server.rs`、`src/server/session_driver_tests.rs`；按需要提取共享纯逻辑时修改 `src/rpc/thread.rs`，不改RPC行为。

**Interfaces:** 在server子模块中实现现有泛型 `impl<F> ServeLauncher<'_, F>`（where F沿用现有build_agent签名）的：
- `async fn resume_append_inner(&mut self, thread_id: &str, workdir: &str, objective: &str, env: Vec<(String,String)>) -> anyhow::Result<String>`；
- `fn restore_driver_thread(&mut self, thread_id: &str, workdir: &str, env: Vec<(String,String)>) -> anyhow::Result<()>`。

后者只装配，不发turn、不创建meta；冷装配失败回滚本次 runtime/root/watch/thread 登记，不移除其他thread共用的项目runtime binding。若装配确需异步停止本次 watcher，可改成async，但 Task 3内全部调用保持统一并在复核表登记，不能让计划与实现签名悄悄漂移。

`Activation` 新增 `tooling_error: Option<String>`，attach_delegation 的原fallback分支填写真实原因，成功填None，既有调用不改变fallback行为。resume冷装配在env非空时遇到该诊断必须RestoreFailed，不能丢env继续；env为空仍可采用现有无委派fallback，但不得改变原permission/extra_roots。不引入宿主对具体环境键的识别。

- [ ] **3.1 写热路径红灯测试**：在现有Host测试模块使用已存在的 `build_test_agent`、`launch_via_driver`、`settle_turn`：

```rust
#[tokio::test(flavor = "multi_thread")]
async fn resume_append_keeps_the_live_thread_identity() {
    let mut host = Host::new();
    let workdir = host.workdir.to_string_lossy().to_string();
    let tid = {
        let mut launcher = host.launcher(&build_test_agent);
        launch_via_driver(&mut launcher, &workdir, "original", "first turn", (None, None))
            .await.unwrap()
    };
    assert!(host.settle_turn(&tid, Duration::from_secs(10)).await);
    let action = DriverAction {
        action_id: "act-resume".into(),
        kind: DriverActionKind::ResumeAppend {
            thread_id: tid.clone(), workdir: workdir.clone(),
            objective: "authorized merge".into(), env: Vec::new(),
        },
    };
    let returned = SessionLauncher::launch(
        &mut host.launcher(&build_test_agent), &action).await.unwrap();
    assert_eq!(returned, tid);
    assert_eq!(host.threads.len(), 1);
    assert_eq!(crate::thread_store::ThreadStore::new(Path::new(&workdir))
        .list().unwrap().len(), 1);
    assert!(host.settle_turn(&tid, Duration::from_secs(10)).await);
}
```

- [ ] **3.2 观察红灯**：`cargo test -p yi-agent-app-server --lib resume_append_keeps_the_live_thread_identity`；Task1的保守拒绝分支应使其失败。
- [ ] **3.3 实现热与冷分派**：热分支先目录/ID校验，调用prepare_and_deliver，禁止RPC resume。冷分支先writer lock，再Task2严格入口；照现有RPC恢复构造Session（messages、last_input_tokens），作用域覆盖 model_ref/extra_roots/reasoning/preserve_thinking，`apply_session` 保住Arc，attach工具时传持久anchor与env。将writer_lock交给ThreadSession，注册pending_activation、watcher、run_thread_driver。不调用start_thread_core，不调用ThreadStore.create，不造UUID。子模块注册 `mod driver_resume;`，分派进入新内核；内核在校验热目录及busy后执行：

```rust
if !self.threads.contains_key(thread_id) {
    self.restore_driver_thread(thread_id, workdir, env)?;
}
self.prepare_and_deliver(thread_id, objective).await
```

- [ ] **3.4 投递失败归类和回滚**：TurnInProgress → Busy；无thread → ThreadNotFound；目录不存在 → WorkdirMissing；严格恢复/装配失败 → RestoreFailed；prepare其他错误及通知/send前失败 → DeliveryFailed。仅在prepare成功后记下本次turn_id；失败时比较active_turn_id与本次ID后才清占位与恢复Idle。已发TurnStarted则发对应Error/TurnCompleted；禁止对旧busy turn做任何清理。send成功立即Ok，之后不得转成“未接收”的错误。
- [ ] **3.5 冷与负面测试**：首轮完成并等持久化后关闭原driver/释放writer lock，再在新Host中指向同一temp workdir（旧TempDir必须继续存活）。注入可记录 `Option<Session>` 的工厂，断言第二轮收到first turn及工具结果，且meta数量与ID未变；逐字段断言配置/usage/anchor/Arc。复用thread_lifecycle/process里的合法持久化夹具，不靠 `threads.remove` 假装整个宿主重启（它不会自动结束driver释放锁）。

增加以下独立测试，名称以 `resume_append_` 开头：busy旧turn_id/cancel token不变；wrong cwd不投递；writer Busy与I/O拒绝；缺/坏历史；partial失败；prompt接收端关闭后只回滚自己的占位；通知thread归属、无RPC响应/全历史重放；观测去重。env为空的纯装配测试允许既有fallback；非空env的真实bash用例使用真实runtime装配，或在Task7串联夹具中执行，不能靠FakeDaemon失败fallback验证env。
- [ ] **3.6 绿灯/检查点**：`cargo test -p yi-agent-app-server --lib resume_append_`、`cargo test -p yi-agent-app-server --lib session_driver`、`cargo test -p yi-agent-app-server --lib thread_lifecycle`、`cargo test -p yi-agent-app-server --lib server::tests::process`。门禁后 commit `feat: resume original threads for driver turns`。

## Task 4: 项目级最小在飞记录与严格写入口

**Files:** 新增 runner `src/merge_attempt.rs`，修改 `lib.rs`、`persist.rs`、`merge_lock.rs`。

**Interfaces:** module merge_attempt定义下列类型/API；它们自身不获取board.lock，调用者通过persist::with_board持锁；list只读原子快照但对错误必须显式显示。

```rust
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergePhase { Pending, Accepted, Uncertain }

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MergeAttemptRecord {
    pub version: u32,
    pub action_id: String,
    pub card_id: String,
    pub thread_id: Option<String>,
    pub expected_workdir: std::path::PathBuf,
    pub phase: MergePhase,
    pub merge_queued_at: Option<chrono::DateTime<chrono::Local>>,
}
```

`read(state_dir: &Path) -> Result<Option<MergeAttemptRecord>, String>`；`save(state_dir: &Path, record: &MergeAttemptRecord) -> Result<(), String>`；`clear_if_matches(state_dir: &Path, card_id: &str, action_id: &str) -> Result<bool, String>`。拒绝version!=1、空ID、action_id=legacy、非绝对expected_workdir；Accepted必须有thread_id，thread初空只供独立Launch Pending/Uncertain。save只序列化/原子替换，不允许静默覆盖另一action：调用者必须在同一board锁内先read比较。read还校验语义合法性，不只检查JSON可解析。

merge_lock新增 `read_holder_strict(state_dir: &Path) -> Result<Option<Holder>, String>`，NotFound才None；启动/调度/offline恢复使用它，既有宽松read_holder仅允许用于非决策显示。坏holder且无journal同样阻塞，不能被旧版兼容分支略过。

persist新增 `load_board_strict(path: &Path) -> Result<Board, String>`，NotFound才new；with_board改用严格入口；显示层read_board的历史兼容可保留，但合并调度不使用其吞错结果决策。

- [ ] **4.1 写红灯测试**：

```rust
#[test]
fn corrupt_attempt_is_not_an_empty_merge_gate() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("merge.attempt.json"), b"{").unwrap();
    assert!(read(dir.path()).is_err());
}

#[test]
fn a_broken_existing_board_is_not_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("board.json");
    std::fs::write(&path, b"broken-board").unwrap();
    assert!(with_board(dir.path(), |_| Ok(())).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"broken-board");
}
```

两个测试分别放merge_attempt与persist现有测试模块。另补NotFound新建、合法往返、未知version、wrong action/card清除不生效、I/O错误、原子写失败不丢旧记录、坏holder严格读取返回错误。
- [ ] **4.2 观察红灯**：插件workspace `cargo test -p superpowers-kanban-runner corrupt_attempt` 与 `cargo test -p superpowers-kanban-runner a_broken_existing_board`。
- [ ] **4.3 实现**：atomic write沿用persist同目录temp+sync_all+rename；除NotFound不吞read错误。merge_lock的write_holder失败改为返回可传播错误，不能Granted但无可靠说明；变更 `acquire_with_holder` 为 `Result<MergeAttempt, String>`，同步生产与测试调用。journal是安全真相，holder仅显示/旧版恢复凭据，不能从holder PID推断工具已结束。
- [ ] **4.4 绿灯/检查点**：`cargo test -p superpowers-kanban-runner merge_attempt`、`cargo test -p superpowers-kanban-runner persist`、`cargo test -p superpowers-kanban-runner merge_lock`。通过门禁后 commit `fix: persist uncertain merge execution boundaries`。

## Task 5: 调度原会话与资源/CAS结算

**Files:** runner `service.rs`、`dispatch.rs`；沿用Task1/4接口。

**Interfaces:** 新增 PendingKind::ResumeForMerge，保存 `expected_thread_id: String`、`expected_workdir: PathBuf`、`merge_queued_at: Option<DateTime<Local>>`、`new_lease: bool`；独立LaunchForMerge保留。Inner增加 `driver_tick_seq: u64` 与 `merge_accepted_on_tick: Option<u64>`，每次driver_step单调递增（仅进程内，重启journal不靠此值恢复）。

- [ ] **5.1 修正夹具并写核心红灯测试**：旧 `service_with_awaiting_card` 只写thread、没有workdir，不能直接用作成功续跑夹具。新增下列绑定真实开发树的fixture；保留旧fixture用于缺绑定负面测试。已有合并happy-path测试迁移到新fixture时删掉额外 `git branch`，否则重复创建源分支会导致非业务红灯。

```rust
fn service_with_merge_bound_card(project: &Path) -> (BoardService, String) {
    let (service, source) = service_with_awaiting_card(project);
    let id = CardId::new("card-1");
    let development = crate::worktree::ensure_worktree(project, &id, &source).unwrap();
    persist::with_board(&service.state_dir, |board| {
        board.set_workdir(&id, development).map_err(|error| error.to_string())
    }).unwrap();
    (service, source)
}
```

然后加入需求红灯：

```rust
#[test]
fn an_implementation_merge_resumes_the_original_thread() {
    let dir = tempfile::tempdir().unwrap();
    let project = project_with_worktree(dir.path());
    let (service, _source) = service_with_merge_bound_card(&project);
    let before = service.list()["cards"][0]["workdir"].clone();
    service.merge_request("card-1").unwrap();
    let caps = DriverCapabilities { actions: vec!["resume_append".into()] };
    let action = service.driver_step(
        &[facts("thread-1", true, true)], &[], Some(0), &caps)
        .unwrap().expect("authorized merge turn");
    assert!(matches!(action, DriverAction::ResumeAppend {
        ref thread_id, ref card_id, ..
    } if thread_id == "thread-1" && card_id == "card-1"));
    assert_eq!(thread_of(&service, "card-1").as_deref(), Some("thread-1"));
    assert_eq!(service.list()["cards"][0]["workdir"], before);
}
```

Task1机械迁移时只改原测试调用参数，不把旧Launch断言当正确预期；现在将该原行为测试替换为上述需求测试。
- [ ] **5.2 观察红灯**：`cargo test -p superpowers-kanban-runner an_implementation_merge_resumes`，应因仍Launch失败。
- [ ] **5.3 实现调度**：能力/缺绑定检查先于预算返回；忙候选跳过。复用lease否则获取一份，再非阻塞gate；strict board与journal检查放在board锁下，重验state/thread/workdir/time；写journal Pending再写Running，两次成功才登记pending并返回ResumeAppend。遇到已有journal（即使卡消失）不发任何合并动作。独立kind Merge保留Launch，card.workdir按既有规则设为目标树且journal.expected_workdir同步该目标树；实现卡journal.expected_workdir是原开发树。只有独立新session结果才允许设置其thread_id。source/base/目标准备失败退还本次资源并CAS标NeedsYou；所有确定未交动作的失败分支在释放gate前清除匹配本次card的holder，不能留下假阻塞，更不能清他人的记录。

禁止沿用“prepare(base)后覆写实现卡workdir”；开发cwd仅定位，目标cwd只进入objective。lease reconcile把ResumeForMerge和LaunchForMerge在飞都视为占额，匹配accepted但仍忙的合并也不能因模型提前终态而释放本次lease。
- [ ] **5.4 结果结算**：action_id匹配pending与journal；成功Resume必须thread匹配，更新phase Accepted及accepted_on_tick但不覆写卡state；busy/确定未投递错误按spec CAS退队或NeedsYou后清本次journal，释放本次gate与新lease；未知code、错ID或记录不一致标Uncertain不重试。没有pending的迟到结果不能清新attempt。写盘失败保留安全门并显式显示，不把内存pending清完后假装已结算。write_observations不能用旧idle快照越过pending/accepted结算门；即使它显示NeedsYou/stale，journal与gate仍须按Task6判据处理。
- [ ] **5.5 边界红绿循环**：分别新增并运行 `merge_resume_` 前缀用例：热busy不取gate/队首跳过；缺观测冷投递；无cap零budget仍detail；peak1已有lease可跑；满额无gate；pending下一拍保lease且不重复；busy退回原时间且不覆盖用户done；成功结果不覆盖early done；退队CAS；重复/旧结果/错ID；journal写失败不交动作；独立Launch兼容。每个先确认旧行为红，再实现，不批量写完实现后补假红。
- [ ] **5.6 绿灯/检查点**：`cargo test -p superpowers-kanban-runner merge_resume_`、`cargo test -p superpowers-kanban-runner`、`cargo test -p superpowers-kanban-core`。门禁后 commit `fix: route authorized merges to original sessions`。

## Task 6: 安全释放、重启门与离线人工恢复

**Files:** runner `service.rs`、`main.rs`、`dispatch.rs`，使用merge_attempt与single_instance现有锁。

**Interfaces:**
- `BoardService::recover_merge_attempt_on_startup(&self) -> Result<(), String>`，仅daemon在取得plugin.lock后、接受查询前调用；不能放在BoardService::new（CLI同样构造service，会误伤活跃attempt）。
- `release_gate_when_the_merge_is_over(&self, threads: &[ThreadFacts], tick_seq: u64) -> Result<(), String>`，代替旧无参方法。
- `MergeFinish` 增加 `Waiting`；普通 `merge_finish(&self, card_id: &str)` 保留签名但不直接释放自动gate；dispatch/CLI支持status=waiting。
- `pub fn recover_offline_merge(state_dir: &Path, card_id: &str, action_id: &str, ack_stopped: bool) -> Result<(), String>` 放merge_attempt模块或main专用模块，**唯一**离线恢复入口。内部取得plugin.lock、非阻塞merge.lock、board.lock后重验；不要从普通IPC dispatch暴露强制清除。

- [ ] **6.1 写安全红灯序列测试**（使用Task5已定义夹具/API和真实success结果）：

```rust
#[test]
fn merge_resume_done_while_busy_keeps_the_project_gate() {
    let dir = tempfile::tempdir().unwrap();
    let project = project_with_worktree(dir.path());
    let (service, _source) = service_with_merge_bound_card(&project);
    let caps = DriverCapabilities { actions: vec!["resume_append".into()] };
    service.merge_request("card-1").unwrap();
    let action = service.driver_step(&[facts("thread-1", true, true)], &[], Some(0), &caps)
        .unwrap().unwrap().to_json();
    let result = DriverResult {
        action_id: action["action_id"].as_str().unwrap().into(), ok: true,
        thread_id: Some("thread-1".into()), error: None, error_code: None,
    };
    service.set_state("card-1", CardState::Done, None).unwrap();
    service.driver_step(&[facts("thread-1", false, true)], &[result], Some(1), &caps).unwrap();
    assert!(crate::merge_lock::acquire(&service.state_dir).is_none());
    assert!(crate::merge_attempt::read(&service.state_dir).unwrap().is_some());
    service.driver_step(&[facts("thread-1", true, true)], &[], Some(0), &caps).unwrap();
    assert!(crate::merge_attempt::read(&service.state_dir).unwrap().is_none());
}
```

- [ ] **6.2 观察红灯**：`cargo test -p superpowers-kanban-runner merge_resume_done_while_busy`，旧规则应提前清journal或释放gate。
- [ ] **6.3 实现正常结算**：匹配accepted、本拍seq严格大于accepted_on_tick、无pending、卡存在/绑定不变/非Running、matching thread alive&&idle才清journal并drop gate。结果同拍的idle快照不能释放。缺观测/dead/卡消失/绑定变化标Uncertain；普通merge-finish只Waiting且错card零副作用。不要先 `.take()` gate 再检查holder。将安全谓词抽为service私有纯函数，供释放与矩阵测试共用：

```rust
fn can_release_merge(
    phase: &crate::merge_attempt::MergePhase,
    pending: bool, accepted_on_tick: Option<u64>, tick_seq: u64,
    card_matches_and_finished: bool, thread: Option<&ThreadFacts>,
) -> bool {
    *phase == crate::merge_attempt::MergePhase::Accepted
        && !pending
        && accepted_on_tick.is_some_and(|accepted| tick_seq > accepted)
        && card_matches_and_finished
        && thread.is_some_and(|fact| fact.alive && fact.idle)
}
```

调用者先按journal.thread_id筛选匹配fact，不能传入其他thread的idle。清记录I/O失败保持gate，向list输出原因。list暴露 `merge_attempt` 或 `merge_blocked_reason` 顶层字段，卡消失仍可见，不依赖卡片detail承载唯一错误。main.rs的command_list需把顶层阻塞原因/attempt ID输出出来，不能只扩展JSON但让CLI渲染继续忽略它；卡存在时另写其detail供现有桌面卡片展示。
- [ ] **6.4 实现重启与离线flags**：main启动有旧journal则Uncertain；旧holder无journal也阻塞。merge-finish parser增加 --recover、--attempt、--ack-stopped，组合缺一拒绝；正常调用不走offline函数。实际取得plugin.lock保证daemon不在，取得merge.lock防其他持有者；journal内card/action精确匹配且卡不Running后才能清，卡缺失可清；legacy token只处理无journal匹配holder。删除holder前校验身份。未知/坏journal拒绝，不提供强行忽略。
- [ ] **6.5 故障测试矩阵**：新增 `merge_recovery_` 前缀：pending写完进程终止/发送后回报前终止/accepted后终止均在新service启动恢复时阻塞；仅drop服务不模拟git停止的证据；真子进程kill测试验证flock释放但journal仍拦后续卡。含错误attempt/card、未ack、daemon活着、merge锁忙、卡Running、卡被purge、坏journal、旧holder、无journal但holder损坏、恢复后可调度下一卡。CLI parse测试断言普通命令仍可解析，flags不能误入普通finish。normal finish不能清uncertain。
- [ ] **6.6 绿灯/检查点**：`cargo test -p superpowers-kanban-runner merge_recovery_`、`cargo test -p superpowers-kanban-runner merge_resume_`、插件全测。门禁后 commit `fix: guard merge release and crash recovery`。

## Task 7: 自包含授权、真实插件串联与发布文档

**Files:** runner `tick.rs`、调用方`service.rs`；宿主`Cargo.toml`、`Cargo.lock`、`src/server/session_driver_tests.rs`；插件skill、README、INSTALL。

**Interfaces:** 修改 `queued_merge_objective(card_id: &str, source: &str, base: &str, development_workdir: &Path, target_workdir: &Path, state_dir: &Path) -> String`。参数无空source；调用端保证source已经推导并验证。新增纯函数 `shell_quote(value: &str) -> String` 使用单引号、内部单引号替换为shell的 `'"'"'` 拼接；全部命令参数使用它，不只路径。实现内容：

```rust
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}
```


测试专用本地依赖：

```toml
[dev-dependencies.superpowers-kanban-runner]
path = "../../../plugins/superpowers-kanban/crates/superpowers-kanban-runner"

[dev-dependencies.superpowers-kanban-core]
path = "../../../plugins/superpowers-kanban/crates/superpowers-kanban-core"
```

不将看板类型加入宿主生产模块。现有FakeDaemon可作为socket传输适配器，但on_query必须调用真实 `dispatch_with_service(&service, &state_dir, None, method, params)`，不能自行构造resume动作。

- [ ] **7.1 写objective红灯测试**（tick模块）：

```rust
#[test]
fn merge_authorization_is_explicit_and_location_safe() {
    let text = queued_merge_objective(
        "card-1", "kanban/card-1", "main", Path::new("/w/dev"),
        Path::new("/w/base tree"), Path::new("/p/.yi-agent/kanban"));
    assert!(text.contains("card-1"));
    assert!(text.contains("--state-dir '/p/.yi-agent/kanban'"));
    assert!(text.contains("'/w/base tree'"));
    assert!(!text.contains("<card-id>"));
    assert!(!text.contains("state --self"));
    assert!(!text.contains("release the slot with"));
}
```

再加包含单引号、空格、美元符号的参数，使用 `sh -c` 的printf往返验证quoting不执行注入内容。
- [ ] **7.2 红绿与实现**：`cargo test -p superpowers-kanban-runner merge_authorization`。objective 明确用户授权替代旧任务范围、不替代sandbox；card/source/base/两个cwd/state-dir明确，已持slot、不再申请；最后state明确ID done/needs_you。合并与验证命令必须同步等待，不能留仍能写目标树的后台任务；明确idle不是任意外部进程退出证明。实现轮改“Never merge yourself”为“未经本卡授权禁止合并”，独立卡文案同样不要求跨进程解锁。skill说明热冷续跑和不可恢复阻塞，不能继续承诺新开合并session。
- [ ] **7.3 真插件热串联测试**：在session_driver_tests新增 `kanban_original_session_hot_round_trip`：临时git项目与spec/plan、隔离home、真实BoardService；首轮通过真实launch取得tid、settle并写AwaitingMerge，真实merge_request；driver_step的query通过真实dispatch；真launcher应收到ResumeAppend并回原tid。检查真实board开发cwd不变、meta只有一个、hub新objective归原thread、真实journal仍在；模型写Done且下一idle拍后journal清除。mock provider不执行真实生产merge，不使用个人API key。
- [ ] **7.4 真插件冷串联与bash环境**：新增 `kanban_original_session_cold_round_trip`：首轮持久化完毕关闭宿主，保留git树/board/历史；新Host与空DriverState通过真实插件生成ResumeAppend恢复原tid。工厂记录恢复Session旧消息/工具结果/配置，实际bash执行 `printf '%s' "$SUPERPOWERS_KANBAN_CARD"` 得原card；真实runtime/tooling装配可用，不能FakeDaemon attach失败却跳过env断言。fixture必须清理本次启动的daemon/driver/watcher，不能留下子进程。验证新DriverState去重登记，后续请求报告原tid alive/idle，无新meta。再加原目录删除/坏log的串联断言：NeedsYou且无Launch。
- [ ] **7.5 运行串联与检查**：`cd yi-agent-rs && cargo test -p yi-agent-app-server --lib kanban_original_session_ -- --nocapture`，必须至少热/冷两项命中且通过；同时宿主恢复测试、插件全测。真实bash用例只能使用tempdir/test env，不能修改进程全局HOME或读取用户模型key；通过现有isolated_models_path与注入home隔离。
- [ ] **7.6 发布文档**：INSTALL写宿主+插件+daemon+skill共同升级顺序；停插件排空/人工确认journal后才降级；meta-only、旧覆写thread与丢开发树不能自动恢复；offline恢复实际命令与人为确认责任；坏journal停机从备份恢复、不教用户自动rm安全记录。运行文档中每条CLI --help/parse测试核对参数存在。
- [ ] **7.7 检查点**：全部门禁后 commit `test: verify original-session merge round trips`（含本组行为文案和发布说明）。

## Task 8: 覆盖审阅、全量门禁与交付

**Files:** 进度文档、spec/plan验证记录；不顺手修改无关依赖。

- [ ] **8.1 审阅映射**：按下表每项找到实际测试名与输出，补进进度feature证据；运行尚未发生前不打[x]。

| Spec | Task | 必须观察的证据 |
|---|---|---|
| P1/H1 | 3、5、7 | 同ID、开发cwd不变、无新meta、旧Append未知仍失败 |
| P2 | 3、5 | busy零副作用、原顺位、跳过队首 |
| P3/C1 | 1、5 | 无cap零budget仍可见、旧宿主不丢动作、独立Launch |
| P4 | 5 | peak1复用、满额无gate、pending与accepted忙时lease保留 |
| P5 | 5、6 | 退队CAS、重复/旧/错ID回报、early done不回Running |
| P6 | 4、6 | journal写在动作前、崩溃/损坏/卡清除/legacy、离线真实锁 |
| P7 | 6 | pending不释放、结果同拍idle不释放、后续idle才释放、错卡零副作用 |
| P8 | 3、7 | 显式参数、shell引用、原权限与sandbox、不删开发树 |
| H2/H3 | 2、3、7 | 真实历史/配置/usage、锁先拿、partial幂等与失败不投递 |
| H4/H5 | 1、3、7 | bash真实env、注册去重、失败通知/占位回滚 |
| E1 | 7 | 真调度服务与真launcher串联，热/冷至少2测试 |
| §§9–10 | 7 | 升级/降级/旧覆写绑定/meta-only人工恢复说明 |

- [ ] **8.2 执行插件门禁**：

```bash
cd plugins/superpowers-kanban && cargo fmt --all -- --check && \
  cargo clippy --workspace --all-targets -- -D warnings && \
  cargo test -p superpowers-kanban-core && \
  cargo test -p superpowers-kanban-runner
```

- [ ] **8.3 执行宿主门禁**（同一target串行，不全workspace开多个test）：

```bash
unset YI_AGENT_THREAD_ID YI_AGENT_SESSION_ID
cd yi-agent-rs && cargo fmt --all && \
  cargo test -p yi-agent-app-server --lib session_driver && \
  cargo test -p yi-agent-app-server --lib resume_append_ && \
  cargo test -p yi-agent-app-server --lib thread_lifecycle && \
  cargo test -p yi-agent-app-server --lib kanban_original_session_ && \
  just ci
```

需网络时仅为当前验证进程提供已有代理，不写项目代理配置。audit若仍RUSTSEC-2026-0429，记录阻塞并停止提交/合并，不把3991基线通过当新实现证据。
- [ ] **8.4 进度与人工验收**：两个模块feature完成后才改[x]、索引计数分别19/20与80/80（实施时重数，不能覆盖其他会话新增条目）；记录执行命令/实际测试名。临时隔离实例验收热merge和重启后的冷merge同侧栏会话，再做不确定执行→阻塞→确认工具停止→离线恢复。未跑人工项就写未跑。
- [ ] **8.5 提交、集成与复验**：使用requesting-code-review与finishing-a-development-branch流程；确认主树无他人未提交冲突，不擅自stash；经授权合并后在合并结果再跑just ci。清理本任务worktree/分支及无进程使用的target，不能删尚待基线修复的工作。

## 文档复核记录（不是实现测试结果）

1. spec调用链复核：识别并修正无条件Launch、宽松恢复、只看状态早释放、热env误承诺、跨树sandbox边界。
2. 故障复核：写journal→写Running→交动作→结果→下一idle之间逐点崩溃均不自动重放；旧holder、坏读、卡purge与offline锁检查均有任务覆盖。
3. spec-plan交叉复核：P1–P8、H1–H5、C1、E1逐项映射；方法/variant/字段名称统一；新增测试依赖仅dev，不将看板业务引入宿主生产代码。
4. 计划可执行性复核：纠正旧fixture没有workdir的问题，新增service_with_merge_bound_card；修正底层flock错误仍放行的遗漏，journal补expected_workdir用于绑定比较。使用实际存在的Host/launch_via_driver/settle_turn、service_with_awaiting_card/project_with_worktree；新接口先定义再引用；红绿步骤、错误构造、真实串联与门禁区分清楚。
5. 最终设计级检查：释放条件矩阵540组合、崩溃切点5个、旧规则早释放反例3个；shell_quote Rust片段已单独编译并完成5组shell往返。链接/围栏/编号覆盖/进度计数/空白检查通过；不将模型检查当作实现正确性证明。
6. 文档集成验证：此前旧基线just ci在audit失败；同步main 997c8d4a（zip 9.2.2）后，cargo fmt --all与完整just ci通过（退出0，3998通过/28跳过，绑定检查18通过，审计通过）。仍未实现或运行本文定义的新测试，不能把基线通过当作方案B已完成。
