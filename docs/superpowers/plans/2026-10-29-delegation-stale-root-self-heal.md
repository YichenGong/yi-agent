# 委派工具陈旧 root 自愈 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让七个委派工具在 runtime 失效（daemon 死掉或 daemon 被替换导致 root 陈旧）时自愈一次，并在无法安全重放时给出可操作的错误，而不是永久失败。

**Architecture:** 在 `ThreadRoot` 上收口唯一入口 `send_as_root`，把「解析句柄 → 判定失败类别 → repair → 重挂 → 单次重试」封在一起；七个工具从 `handle()` + 裸 `send_request` 改为调用它。失败按传输层/身份层/业务层三分类，只有幂等请求允许在身份层重试。`detach()` 与 repair 脱钩，`current_or_repair` 死代码删除。

**Tech Stack:** Rust 1.98（本机 toolchain）、`tokio`、`async-trait`、`tempfile`、`yi-agent-store`（`std::os::unix::net` IPC）。

**Spec:** `docs/superpowers/specs/2026-10-29-delegation-stale-root-self-heal-design.md`

## Global Constraints

- **不动 wire 契约**：不新增/修改 `IpcRequest`、`IpcResponse`、`IpcErrorCode` 任何变体；不改 `PROTOCOL_VERSION = 2`（`yi-agent-store/src/ipc.rs:35`）。
- **失败判据固定**：身份层 = `Ok(IpcResponse::Error { code: NotFound | AuthorityDenied, .. })`。实测陈旧 root 在 `send_message` / `spawn_agent` / `wait_agent` / `detach` 四条路径上稳定返回 `NotFound`（见 spec §1.3）。
- **重试上限固定为每次调用 1 次**，不跨调用记忆（spec §3.3）。
- **只有幂等请求允许身份层重试**：`WaitAgent`、`InspectChild`、`CancelChild`（若幂等）、`ActivateApplicationRoot`、`AttachApplicationRoot`、`DetachApplicationRoot`。`SendApplicationMessage`、`SendMessage`、`SpawnApplicationChild`、fork 上传系列**禁止**重放（spec §3.2）。
- **不得把内部 repair 错误泄漏给模型**：fixed binding 无法 repair 时，返回原始业务错误而不是 `"this runtime binding cannot be repaired"`。
- **不许并行跑 `cargo test --workspace`**（会 OOM）；按 crate 跑，跑前 `ps aux | grep cargo` 确认无残留。
- **提交前必须 `cargo fmt --all`**（在 `yi-agent-rs/` 下）。
- 提交信息 conventional commits、中文，**不写** `Co-Authored-By`。
- **严禁在 `main` 上直接提交**：在 worktree `.worktrees/delegation-stale-root`（分支 `fix/delegation-stale-root-self-heal`）内执行。
- **测试基线**：`cargo test -p yi-agent-subagent`（lib 84 + e2e 9 全绿）；`cargo test -p yi-agent-app-server --lib`。新增用例只增不改既有断言。
- **cargo 不在默认 PATH**：本机必须 `export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin"`。

## 测试环境前提（执行者必读）

本模块的测试用 `tempfile::TempDir` 做 runtime 目录，socket 落在
`<TempDir>/runtime.sock`（路径短，不会触发 `$TMPDIR` 回退）。这与本次要复现的
故障无关：本次故障的判据是「daemon 进程退出 / daemon 被替换」，不是「目录被删」，
所以**不需要**像 `2026-10-18-stranded-daemon-self-stop.md` 那样构造深路径。
执行者不要照搬那份计划的 `DEEP_SLUG` 前提。

**验证 daemon 真的停了**必须轮询 `probe_runtime`，不要只 `sleep`：

```rust
fn wait_until_dead(socket: &Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while matches!(
        yi_agent_subagent::attach::probe_runtime(socket),
        yi_agent_subagent::attach::RuntimeProbe::Healthy
    ) {
        assert!(std::time::Instant::now() < deadline, "the daemon must stop");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
```

---

### Task 1: 让 `send_as_root` 可被工具复用，并补齐失败分类

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/thread_root.rs:217-230`（`send_as_root`）
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/binding.rs`（新增 `is_stale_root_response`）
- Test: `yi-agent-rs/crates/yi-agent-subagent/src/binding.rs` 的 `mod tests`

**Interfaces:**
- Consumes: `RuntimeBinding::repair()`（`binding.rs:226`）、`RuntimeBinding::generation()`（`binding.rs:181`）、`is_liveness_error`（`binding.rs:340`）、`ThreadRoot::live_cached_root`（`thread_root.rs:104`）、`ThreadRoot::handle`（`thread_root.rs:176`）。
- Produces:
  - `pub fn is_stale_root_response(response: &IpcResponse) -> bool`（`binding.rs`）
  - `pub(crate) enum RetryPolicy { Idempotent, SingleShot }`（`thread_root.rs`）
  - `pub(crate) fn send_as_root<F>(&self, policy: RetryPolicy, build: F) -> Result<IpcResponse, String>`（`thread_root.rs`）
  - `pub(crate) fn stale_root_hint() -> &'static str`（`thread_root.rs`）

- [ ] **Step 1: 写失败的测试（身份层判据）**

在 `binding.rs` 的 `mod tests`（`:358`）末尾追加：

```rust
    /// 陈旧 root 的判据：daemon 明确说「这个 session / root 我不认识」。
    /// 实测替换后的 daemon 对 send_message / spawn_agent / wait_agent / detach
    /// 四条路径稳定返回 NotFound（见 spec §1.3）。
    #[test]
    fn a_not_found_response_marks_the_root_stale() {
        let response = IpcResponse::Error {
            code: yi_agent_store::ipc::IpcErrorCode::NotFound,
            message: None,
        };
        assert!(is_stale_root_response(&response));
    }

    #[test]
    fn an_authority_denied_response_marks_the_root_stale() {
        let response = IpcResponse::Error {
            code: yi_agent_store::ipc::IpcErrorCode::AuthorityDenied,
            message: None,
        };
        assert!(is_stale_root_response(&response));
    }

    /// 业务拒绝绝不能被当成「root 过期」，否则一次参数错误就会触发 repair。
    #[test]
    fn a_business_rejection_is_not_a_stale_root() {
        for code in [
            yi_agent_store::ipc::IpcErrorCode::Validation,
            yi_agent_store::ipc::IpcErrorCode::RateLimited,
            yi_agent_store::ipc::IpcErrorCode::InvalidState,
            yi_agent_store::ipc::IpcErrorCode::Conflict,
            yi_agent_store::ipc::IpcErrorCode::Internal,
        ] {
            let response = IpcResponse::Error {
                code,
                message: Some("business".into()),
            };
            assert!(!is_stale_root_response(&response), "{code} is business-layer");
        }
        assert!(!is_stale_root_response(&IpcResponse::Stopping));
    }
```

- [ ] **Step 2: 运行测试确认失败**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin"
cd yi-agent-rs && cargo test -p yi-agent-subagent --lib stale_root -- --nocapture
```

Expected: 编译错误 `cannot find function is_stale_root_response in this scope`。

- [ ] **Step 3: 实现 `is_stale_root_response`**

在 `binding.rs` 的 `is_liveness_error`（`:340`）之后追加：

```rust
/// 这个应答是否在说「你手里的 root 我不认识」。
///
/// 与 [`is_liveness_error`] 互补：那个判传输层（socket 没了），这个判身份层
/// （socket 很好，但这个 session / root 不是我的）。替换后的 daemon 会为新
/// root 重建 session 与 capability，旧 root 的三元组在它的
/// `application_root_attachments` 里查不到，于是稳定返回 `NotFound`；权限
/// 摘要不匹配则是 `AuthorityDenied`。两者对客户端是同一件事：手里的 root 过期了。
///
/// 业务拒绝（`Validation`、`RateLimited`、`InvalidState`、`Conflict`、`Internal`）
/// **不**属于这一类：那些错误重发一次不会好，触发 repair 只是浪费一次 daemon 启动。
pub fn is_stale_root_response(response: &IpcResponse) -> bool {
    matches!(
        response,
        IpcResponse::Error {
            code: yi_agent_store::ipc::IpcErrorCode::NotFound
                | yi_agent_store::ipc::IpcErrorCode::AuthorityDenied,
            ..
        }
    )
}
```

- [ ] **Step 4: 运行测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent --lib stale_root
```

Expected: `3 passed`。

- [ ] **Step 5a: 在模块级加入 `RetryPolicy` 与 `stale_root_hint`**

在 `thread_root.rs` 中 `fn cached_root_is_live`（`:53`）**之前**（即 `use` 之后、
任何 `impl` 之外）插入这两个条目。它们是模块级条目，**不能**写在 `impl ThreadRoot`
里面：

```rust
/// 身份层失败时对模型说的话。
///
/// 不可重放的请求（发消息、spawn）遇到陈旧 root 时，重放会产生真实的第二次副作用，
/// 所以只做「丢弃缓存 + 修好运行时」，然后把下一步交给调用方。这句话必须说清
/// 「已经修好了、再来一次就行」，否则模型会以为这个能力坏了。
pub(crate) fn stale_root_hint() -> &'static str {
    "the conversation root was replaced by a restarted runtime and has been \
     re-attached; the request was NOT re-sent because it changes daemon state. \
     Retry this call once."
}

/// 一条请求能否在身份层失败后被重发。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryPolicy {
    /// 请求本身幂等（只读查询，或在 daemon 侧按稳定键幂等）：可以安全重发。
    Idempotent,
    /// 请求会改变 daemon 状态（发消息、spawn、fork 上传）：绝不重发，
    /// 只修好运行时并把决定权交回调用方。
    SingleShot,
}
```

- [ ] **Step 5b: 替换 `send_as_root` 的函数体**

把 `thread_root.rs:217-230` 的 `send_as_root` 整体替换为下面这段。**注意**它
不带任何循环或二次重试：`Ok` 分支里的那一行 `send_request` 是**唯一**的
重发点，因此「每次调用最多 1 次」是由结构保证的，不靠计数器。

```rust
    /// 以本会话 root 的身份发送一条请求；运行时失效时自愈一次并按 `policy` 决定
    /// 是否重发。
    ///
    /// 这是委派工具通往 wire 的唯一入口。`handle()` 只解析句柄、不处理失效，
    /// 七个工具过去各自 `handle()` + 裸 `send_request`，结果是**没人**处理失效。
    ///
    /// 三条分支：
    /// 1. 传输层失败（`is_liveness_error`）：与过去一致，重取句柄后重发一次。
    /// 2. 身份层失败（`is_stale_root_response`）：丢弃缓存 root、修好运行时；
    ///    `Idempotent` 重发一次，`SingleShot` 返回 [`stale_root_hint`]。
    /// 3. 其余：原样返回，绝不重试。
    fn send_as_root<F>(&self, policy: RetryPolicy, build: F) -> Result<IpcResponse, String>
    where
        F: Fn(&RuntimeHandle) -> IpcRequest,
    {
        let handle = self.handle()?;
        match send_request(&handle.socket_path, build(&handle)) {
            Ok(response) if is_stale_root_response(&response) => {
                // 先丢弃归属旧 runtime 的 root：无论接下来能否修好运行时，
                // 这个 root 都已经不能再用。
                *self.lock_cached() = None;
                // 修不好（fixed binding 没有 repair plan，或 repair 本身失败）时
                // **不能**把内部错误抛给模型：关于这次调用的事实是原始业务应答。
                if let Err(error) = self.binding.repair() {
                    tracing::debug!(%error, "could not repair the runtime for a stale root");
                    return Ok(response);
                }
                if policy == RetryPolicy::SingleShot {
                    return Err(stale_root_hint().to_string());
                }
                // 缓存已被清空，`handle()` 会先 `attach()` 再取句柄：重挂到
                // 修好后的 runtime 上。这里只重发一次，没有递归。
                let handle = self.handle()?;
                send_request(&handle.socket_path, build(&handle)).map_err(|e| e.to_string())
            }
            Ok(response) => Ok(response),
            Err(error) if is_liveness_error(&error) => {
                // 传输层失败说明请求根本没送达，重发一次是安全的。但**不能**直接
                // `handle()`：缓存 root 仍属于当前 generation，`attach()` 会短路
                // 返回 Ok，重发于是又打向同一个死 socket——这正是要修的那个 bug。
                // 必须先 `repair()`（它推进 generation，使缓存失效），再重挂。
                if let Err(repair_error) = self.binding.repair() {
                    // fixed binding 无从 repair（TUI / daemon worker 的情形）。
                    // 这里返回**原始传输错误**，不要把内部措辞泄漏给模型；
                    // 跳过重发也不可能成功，发了只是多一次失败。
                    tracing::debug!(%repair_error, "could not repair a dead runtime");
                    return Err(error.to_string());
                }
                let handle = self.handle()?;
                send_request(&handle.socket_path, build(&handle)).map_err(|e| e.to_string())
            }
            Err(error) => Err(error.to_string()),
        }
    }
```

**为什么 liveness 分支要先 repair**：`handle()` → `attach()` 在
`live_cached_root(generation).is_some()` 时会直接 `return Ok(())`
（`thread_root.rs:128`）。daemon 刚死而缓存仍属于当届 generation 时，缓存
「看起来」有效，`attach()` 短路，重发必然再次打到死 socket。只有 `repair()`
推进 generation 之后，下一次 `attach()` 才会真正重挂。这是 Task 3 的测试能变绿
的唯一途径——**不要**把它简化成 `let handle = self.handle()?`。

同时把 `use` 补上：`thread_root.rs` 顶部已 `use crate::binding::{RuntimeBinding, RuntimeHandle, is_liveness_error};`，
改成 `use crate::binding::{RuntimeBinding, RuntimeHandle, is_liveness_error, is_stale_root_response};`。

**为什么不需要 generation 断言**：spec §3.3 要求「防永远重挂」，而这里的结构本身
就满足——`send_as_root` 的重发点是单条的、不在循环里，也没有递归调用自己。
第二次仍失败时直接 `map_err` 返回，不会第三次。

- [ ] **Step 6: 更新 `activate` 的调用点**

`thread_root.rs:235` 的 `self.send_as_root(|handle| ...)` 改为
`self.send_as_root(RetryPolicy::Idempotent, |handle| ...)`。

- [ ] **Step 7: 运行测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent --lib
```

Expected: 全绿（84 + 新增 3）。若 `activate` 相关测试因签名变化编译失败，按 Step 6 修完再跑。

- [ ] **Step 8: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-subagent/src/binding.rs crates/yi-agent-subagent/src/thread_root.rs
git commit -m "feat(subagent): classify a stale root as a recoverable failure"
```

---

### Task 2: `detach` 与 repair 脱钩

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/thread_root.rs:251-267`（`detach`）
- Test: `yi-agent-rs/crates/yi-agent-subagent/tests/attach_delegation.rs`（追加）

**Interfaces:**
- Consumes: `RuntimeBinding::current()`（`binding.rs:125`）、`RuntimeBinding::generation()`、`ThreadRoot::live_cached_root`。
- Produces: 无新 API。行为变更：`detach()` 不再 attach、不再 repair。

- [ ] **Step 1: 写失败的测试**

在 `tests/attach_delegation.rs` 末尾追加（需要 `use std::time::Duration;`，文件顶部已有 `Instant`/`Duration` 的用法时按现有 import 补）：

```rust
/// 删除会话时会调用 `ThreadRoot::detach()`。过去它会先 `handle()` → `attach()`
/// → `binding.send`，于是**把已退休的 daemon 重新拉起来**，并幂等地重建那个
/// 本该被删掉的 root——回收循环（`evict_idle_runtimes`）的努力被这一步直接抵消。
///
/// 实测（2026-10-29）：retire 后 detach，daemon 复活且 generation 跳到 2。
#[test]
fn detaching_a_retired_root_does_not_start_a_daemon() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let cfg = config_for(repo.path());
    let models = isolated_models_path(repo.path());

    let attached = Arc::new(
        yi_agent_subagent::attach::attach_project_runtime(
            &cfg,
            runtime.path().to_path_buf(),
            &models,
        )
        .expect("a clean git repo must attach"),
    );
    let socket = attached.socket_path.clone();
    let binding = yi_agent_subagent::binding::RuntimeBinding::managed(
        &cfg,
        runtime.path().to_path_buf(),
        models,
        Arc::clone(&attached),
    );
    let root = ThreadRoot::from_handle(Arc::clone(&binding), attached.attached_root.clone());
    drop(attached);

    binding.retire();
    let generation_after_retire = binding.generation();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while matches!(
        yi_agent_subagent::attach::probe_runtime(&socket),
        yi_agent_subagent::attach::RuntimeProbe::Healthy
    ) {
        assert!(std::time::Instant::now() < deadline, "retire must stop the daemon");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // 这正是 `thread/delete` 做的事。
    root.detach();

    assert!(
        !matches!(
            yi_agent_subagent::attach::probe_runtime(&socket),
            yi_agent_subagent::attach::RuntimeProbe::Healthy
        ),
        "detaching a retired root must not start a daemon back up"
    );
    assert_eq!(
        binding.generation(),
        generation_after_retire,
        "detach must not repair, so it must not install a new runtime"
    );
}
```

- [ ] **Step 2: 运行测试确认失败**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin"
cd yi-agent-rs && cargo test -p yi-agent-subagent --test attach_delegation detaching_a_retired_root -- --nocapture
```

Expected: FAIL，`detaching a retired root must not start a daemon back up`
（实测 daemon 会复活、generation 跳到 2）。

- [ ] **Step 3: 改 `detach`**

把 `thread_root.rs:251-267` 的 `detach` 整体替换为：

```rust
    /// Detaches this conversation's root. Best effort: a failure is a trace
    /// line, never an error that masks why the conversation is winding down.
    ///
    /// **不 attach、不 repair。** 过去的实现先 `handle()`（内部会 `attach()` →
    /// `binding.send`，失效即 repair），于是在 runtime 已退休时会冷启动一个
    /// daemon，并幂等地把本该删掉的 root 重建出来——「回收空闲项目」被这一步
    /// 直接抵消（见 `docs/bug-list.md` 的「监督线程只增不减」）。
    ///
    /// 缓存里没有属于当届 runtime 的 root 时，daemon 侧要么不认识它、要么整个
    /// runtime 都没了：这两种情况都没有东西可 detach。
    pub fn detach(&self) {
        let Some(root) = self.live_cached_root(self.binding.generation()) else {
            *self.lock_cached() = None;
            return;
        };
        let Ok(handle) = self.binding.current() else {
            return;
        };
        if let Err(error) = send_request(
            &handle.socket_path,
            IpcRequest::DetachApplicationRoot {
                session_id: root.session_id.clone(),
                root_task_id: root.task_id.clone(),
                capability: root.capability.clone(),
            },
        ) {
            tracing::warn!(%error, "could not detach the conversation root");
        }
        *self.lock_cached() = None;
    }
```

- [ ] **Step 4: 运行测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent --test attach_delegation
```

Expected: 10 passed（原有 9 + 新增 1）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-subagent/src/thread_root.rs crates/yi-agent-subagent/tests/attach_delegation.rs
git commit -m "fix(subagent): stop detach from restarting a retired daemon"
```

---

### Task 3: 让 `send_message` 在 daemon 死后自愈（形态 A）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:1452-1484`（`DaemonApplicationSendMessageTool::call`）
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:1509-1544`（`DaemonSendMessageTool::call`）
- Test: `yi-agent-rs/crates/yi-agent-subagent/tests/attach_delegation.rs`（追加）

**Interfaces:**
- Consumes: `ThreadRoot::send_as_root(RetryPolicy, F)`（Task 1 产出）、`RetryPolicy::SingleShot`、`stale_root_hint()`。
- Produces: 无新 API。行为变更：两个 `send_message` 工具在 runtime 失效时自愈。

- [ ] **Step 1: 写失败的测试**

在 `tests/attach_delegation.rs` 末尾追加：

```rust
/// 形态 A：daemon 死了，但 binding 不知道（没走 `retire`，没有 generation 变化）。
/// 这是「承载 daemon 的终端被关掉」的真实形状。修复前实测返回
/// `daemon is unavailable: No such file or directory (os error 2)`。
///
/// 断言只钉「自愈确实发生了」这四件事，不钉「调用成功」：`send_message` 是
/// `SingleShot`，第一次请求已因 liveness 失败而作废，重挂之后重发的那一次才会
/// 拿到真实业务结论——而本用例的 recipient 是随机 UUID，业务结论必然是拒绝。
/// 把「重发」和「调用语义成功」混在一起断言，会让用例随 daemon 的消息校验规则
/// 漂移。身份层陈旧导致的「不重放 + 提示」由 Task 6 的形态 B 用例负责。
#[tokio::test]
async fn a_tool_repairs_a_daemon_that_died() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let cfg = config_for(repo.path());
    let models = isolated_models_path(repo.path());

    let attached = Arc::new(
        yi_agent_subagent::attach::attach_project_runtime(
            &cfg,
            runtime.path().to_path_buf(),
            &models,
        )
        .expect("a clean git repo must attach"),
    );
    let socket = attached.socket_path.clone();
    let binding = yi_agent_subagent::binding::RuntimeBinding::managed(
        &cfg,
        runtime.path().to_path_buf(),
        models,
        Arc::clone(&attached),
    );
    let root = ThreadRoot::from_handle(Arc::clone(&binding), attached.attached_root.clone());
    let mut registry = yi_agent_core::ToolRegistry::new();
    yi_agent_subagent::register_application_subagent_tools(
        &mut registry,
        Arc::clone(&root),
        yi_agent_tools::SandboxController::new(
            yi_agent_core::autonomy::YoloSwitch::new(false),
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            false,
        ),
        yi_agent_subagent::CallerContext::unbound(),
    );
    let tool = registry.get("send_message").expect("send_message is registered");
    let generation_before = binding.generation();

    // daemon 死了，binding 未被告知。
    let _ = yi_agent_store::ipc::send_request(&socket, yi_agent_store::ipc::IpcRequest::Stop);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while matches!(
        yi_agent_subagent::attach::probe_runtime(&socket),
        yi_agent_subagent::attach::RuntimeProbe::Healthy
    ) {
        assert!(std::time::Instant::now() < deadline, "the daemon must stop");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(binding.generation(), generation_before, "premise: the binding did not notice");

    let result = tool
        .call(serde_json::json!({
            "recipient": "11111111-1111-4111-8111-111111111111",
            "message": "hi",
        }))
        .await;

    let text = format!("{:?}", result.content);
    assert!(
        !text.contains("daemon is unavailable"),
        "a dead runtime must be healed instead of surfacing a transport error: {text}"
    );
    assert!(
        binding.generation() > generation_before,
        "healing must install a new runtime, so the generation must move"
    );
    assert!(
        matches!(
            yi_agent_subagent::attach::probe_runtime(&socket),
            yi_agent_subagent::attach::RuntimeProbe::Healthy
        ),
        "the runtime must be up again after the call"
    );

    // 自愈后这个 root 必须真的可用：用它发一条只读请求，应当得到
    // `TaskSummaries`（业务结论），而不是「会话不认识你」（身份失效）。
    let handle = root.handle().expect("the healed root resolves a handle");
    let probe = yi_agent_store::ipc::send_request(
        &handle.socket_path,
        yi_agent_store::ipc::IpcRequest::ListTaskSummaries {
            session_id: Some(handle.session_id.clone()),
            active_only: false,
        },
    )
    .expect("the socket answers");
    assert!(
        matches!(probe, yi_agent_store::ipc::IpcResponse::TaskSummaries { .. }),
        "the healed root must be usable, got {probe:?}"
    );
}
```

**不要**为此改 `Cargo.toml`：`serde_json` 已在 `[dependencies]`（`Cargo.toml:22`），
集成测试可以直接用。`tokio` 同理（workspace 定义带 `features = ["full"]`，
`#[tokio::test]` 可用）。

- [ ] **Step 2: 运行测试确认失败**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin"
cd yi-agent-rs && cargo test -p yi-agent-subagent --test attach_delegation a_tool_repairs_a_daemon_that_died -- --nocapture
```

Expected: FAIL，`a dead runtime must be healed instead of surfacing a transport error`
（实测内容为 `error: daemon is unavailable: No such file or directory (os error 2)`）。

- [ ] **Step 3: 改 `DaemonApplicationSendMessageTool::call`（`lib.rs:1452-1484`）**

把从 `let handle = match self.root.handle() { ... };` 到函数结尾的整段替换为：

```rust
        // 发消息会改变 daemon 状态：身份层失败不重放，只修好运行时并让模型重试。
        match self.root.send_as_root(RetryPolicy::SingleShot, |handle| {
            yi_agent_store::ipc::IpcRequest::SendApplicationMessage {
                session_id: handle.session_id.clone(),
                sender_task_id: handle.task_id.clone(),
                capability: handle.capability.clone(),
                recipient_task_id: recipient.clone(),
                message: message.clone(),
            }
        }) {
            Ok(yi_agent_store::ipc::IpcResponse::MessageQueued) => {
                ToolResult::text("message queued")
            }
            Ok(other) => ToolResult::error(format_ipc_rejection("message", &other)),
            Err(error) => ToolResult::error(format!("daemon is unavailable: {error}")),
        }
```

- [ ] **Step 4: 改 `DaemonSendMessageTool::call`（`lib.rs:1509-1544`）**

同样把 `let handle = ...` 之后的整段替换为：

```rust
        match self.root.send_as_root(RetryPolicy::SingleShot, |handle| {
            yi_agent_store::ipc::IpcRequest::SendMessage {
                session_id: handle.session_id.clone(),
                sender_task_id: handle.task_id.clone(),
                worker_capability: handle.capability.clone(),
                recipient_task_id: recipient.clone(),
                message: message.clone(),
            }
        }) {
            Ok(yi_agent_store::ipc::IpcResponse::MessageQueued) => {
                ToolResult::text("message queued")
            }
            Ok(other) => ToolResult::error(format_ipc_rejection("message", &other)),
            Err(error) => ToolResult::error(format!("daemon is unavailable: {error}")),
        }
```

- [ ] **Step 5: 补 import**

`lib.rs` 顶部（`use crate::thread_root` 相关处）确保 `RetryPolicy` 可见。
`lib.rs` 里已有 `crate::thread_root::ThreadRoot` 的全路径用法，因此直接用
`crate::thread_root::RetryPolicy::SingleShot` 即可，不必加 `use`。

- [ ] **Step 6: 运行测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent --test attach_delegation
cd yi-agent-rs && cargo test -p yi-agent-subagent --lib
```

Expected: e2e 11 passed；lib 全绿。

- [ ] **Step 7: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-subagent/src/lib.rs crates/yi-agent-subagent/tests/attach_delegation.rs crates/yi-agent-subagent/Cargo.toml
git commit -m "fix(subagent): heal a dead runtime from the send_message tools"
```

---

### Task 4: 让 `spawn_agent` 在 daemon 死后自愈（形态 A，含 fork 上传顺序）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:2358-2413`（`DaemonSpawnAgentTool::call`）
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:2082-2148`（`DaemonApplicationSpawnAgentTool::call`）
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:1950-1985`（`spawn_outcome` 增加 single-shot 提示识别）

**Interfaces:**
- Consumes: `ThreadRoot::send_as_root`、`RetryPolicy::SingleShot`、`stale_root_hint()`、`spawn_fork_token`（`lib.rs:1987`）。
- Produces: 无新公开 API。

- [ ] **Step 1: 写失败的测试**

在 `tests/attach_delegation.rs` 末尾追加：

```rust
/// `spawn_agent` 同样必须自愈。fork 上传必须先于 spawn，所以句柄在
/// `send_as_root` 之前就要拿到——这条测试同时钉住「自愈后 socket 换过，
/// fork token 仍然有效」。
#[tokio::test]
async fn a_spawn_tool_repairs_a_daemon_that_died() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let cfg = config_for(repo.path());
    let models = isolated_models_path(repo.path());

    let attached = Arc::new(
        yi_agent_subagent::attach::attach_project_runtime(
            &cfg,
            runtime.path().to_path_buf(),
            &models,
        )
        .expect("a clean git repo must attach"),
    );
    let socket = attached.socket_path.clone();
    let binding = yi_agent_subagent::binding::RuntimeBinding::managed(
        &cfg,
        runtime.path().to_path_buf(),
        models,
        Arc::clone(&attached),
    );
    let root = ThreadRoot::from_handle(Arc::clone(&binding), attached.attached_root.clone());
    let mut registry = yi_agent_core::ToolRegistry::new();
    yi_agent_subagent::register_application_subagent_tools(
        &mut registry,
        Arc::clone(&root),
        yi_agent_tools::SandboxController::new(
            yi_agent_core::autonomy::YoloSwitch::new(false),
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            false,
        ),
        yi_agent_subagent::CallerContext::unbound(),
    );
    let tool = registry.get("spawn_agent").expect("spawn_agent is registered");
    let generation_before = binding.generation();

    let _ = yi_agent_store::ipc::send_request(&socket, yi_agent_store::ipc::IpcRequest::Stop);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while matches!(
        yi_agent_subagent::attach::probe_runtime(&socket),
        yi_agent_subagent::attach::RuntimeProbe::Healthy
    ) {
        assert!(std::time::Instant::now() < deadline, "the daemon must stop");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let result = tool
        .call(serde_json::json!({ "task": "say hello", "fork": false }))
        .await;

    let text = format!("{:?}", result.content);
    assert!(
        !text.contains("daemon is unavailable"),
        "spawn must repair a dead runtime instead of surfacing a transport error: {text}"
    );
    assert!(binding.generation() > generation_before);
    assert!(
        matches!(
            yi_agent_subagent::attach::probe_runtime(&socket),
            yi_agent_subagent::attach::RuntimeProbe::Healthy
        ),
        "the runtime must be up again after the call"
    );
    // 自愈后必须真的能 spawn：成功时载荷里带 `"status":"queued"`，
    // 失败时是拒绝原因。这不依赖 recipient/task 的具体校验规则。
    assert!(
        text.contains("queued"),
        "a healed runtime must accept the spawn, got {text}"
    );
}
```

- [ ] **Step 2: 运行测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent --test attach_delegation a_spawn_tool_repairs -- --nocapture
```

Expected: FAIL，`spawn must repair a dead runtime instead of surfacing a transport error`。

- [ ] **Step 3: 让 `spawn_outcome` 识别 single-shot 提示**

在 `lib.rs:1950` 的 `spawn_outcome` 里，`Err(error)` 分支（`:1975-1982`）改为：

```rust
        Err(error) => {
            abort_unconsumed_fork(socket, fork_token);
            // `send_as_root` 在身份层失败且策略为 SingleShot 时返回的提示已经
            // 说明「没有重发」；这里原样透出，不要包上 "daemon is unavailable"，
            // 否则模型会以为运行时挂了而实际上它刚被修好。
            if error.contains("was NOT re-sent") {
                return ToolResult::error(error);
            }
            let message = format!("daemon is unavailable: {error}");
            ToolResult::error(coding_requires_git_payload(&message).unwrap_or(message))
        }
```

- [ ] **Step 4: 改 `DaemonSpawnAgentTool::call`（`lib.rs:2358-2413`）**

把 `let handle = match self.root.handle() { ... };` 到结尾的整段替换为：

```rust
        // fork 上传必须先于 spawn，且它自己也走 wire：先把句柄拿到，再上传
        // （上传失败时 `spawn_fork_token` 已经按 fork 策略降级或报错）。
        let handle = match self.root.handle() {
            Ok(handle) => handle,
            Err(error) => return ToolResult::error(format!("daemon is unavailable: {error}")),
        };
        let fork_token = match spawn_fork_token(&args, &handle, &self.caller) {
            Ok(fork_token) => fork_token,
            Err(error) => return error,
        };
        // spawn 会创建子任务：身份层失败绝不重放，只修好运行时并让模型重试。
        let response = self.root.send_as_root(RetryPolicy::SingleShot, |handle| {
            yi_agent_store::ipc::IpcRequest::SpawnChild {
                session_id: handle.session_id.clone(),
                parent_task_id: handle.task_id.clone(),
                objective: objective.clone(),
                mode: Some(mode.clone()),
                model: model.clone(),
                workdir: workdir.clone(),
                sandbox: sandbox.clone(),
                fork_token: fork_token.clone(),
            }
        });
        match response {
            Ok(response) => spawn_outcome(Ok(response), &handle.socket_path, &fork_token, &objective),
            Err(error) => spawn_outcome(Err(error), &handle.socket_path, &fork_token, &objective),
        }
```

- [ ] **Step 5: 把 `spawn_outcome` 的第一个参数改成 `Result<IpcResponse, String>`**

否则 Step 4 传不进去。把签名（`lib.rs:1950`）改为：

```rust
fn spawn_outcome(
    response: Result<yi_agent_store::ipc::IpcResponse, String>,
    socket: &Path,
    fork_token: &Option<String>,
    objective: &str,
) -> ToolResult {
```

其 `Ok(other)` 与 `Err(error)` 分支保持不变（`Err` 里 `error` 现在是 `String`，
`format!("daemon is unavailable: {error}")` 仍然成立）。同时把既有的
`spawn_outcome(Err(...))` 调用点（若有）从 `IpcError` 改为 `.to_string()`。

- [ ] **Step 6: 同样改 `DaemonApplicationSpawnAgentTool::call`（`lib.rs:2082-2148`）**

与 Step 4 同构，唯一的差别是请求变体为 `SpawnApplicationChild` 且带 `thread_id`：

```rust
        let response = self.root.send_as_root(RetryPolicy::SingleShot, |handle| {
            yi_agent_store::ipc::IpcRequest::SpawnApplicationChild {
                session_id: handle.session_id.clone(),
                parent_task_id: handle.task_id.clone(),
                capability: handle.capability.clone(),
                objective: objective.clone(),
                mode: Some(mode.clone()),
                model: model.clone(),
                workdir: workdir.clone(),
                sandbox: sandbox.clone(),
                fork_token: fork_token.clone(),
                thread_id: self.thread_id.clone(),
            }
        });
```

- [ ] **Step 7: 运行测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent
```

Expected: lib 全绿 + e2e 12 passed。

- [ ] **Step 8: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-subagent/src/lib.rs crates/yi-agent-subagent/tests/attach_delegation.rs
git commit -m "fix(subagent): heal a dead runtime from the spawn tools"
```

---

### Task 5: 让只读工具与 `inspect` / `cancel` / `wait` 重挂陈旧 root（形态 B）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:2174-2227`（`DaemonInspectAgentTool::call`）
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:2253-2289`（`DaemonCancelAgentTool::call`）
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:2308-2344`（`DaemonWaitAgentTool::call`）
- Test: `yi-agent-rs/crates/yi-agent-subagent/tests/attach_delegation.rs`（追加）

**Interfaces:**
- Consumes: `ThreadRoot::send_as_root`、`RetryPolicy::Idempotent`。
- Produces: 无新公开 API。

**幂等性论证（执行者必读）：** `InspectChild` 只读；`CancelChild` 对同一 task 重复取消
在 daemon 侧是幂等的（第二次找不到可取消的目标即返回，不会产生第二个副作用）；
`WaitAgent` 是查询，重复一次只是多等一次。三者都可安全重放。若实现时发现
`CancelChild` 在 daemon 侧非幂等，**降级为 `SingleShot`** 并在提交信息里说明。

- [ ] **Step 1: 写失败的测试**

在 `tests/attach_delegation.rs` 末尾追加：

```rust
/// 形态 B：daemon 被**同伴**替换。socket 全程健康，所以不出现 liveness 错误，
/// generation 也不动——缓存 root 会永久失效。实测 `inspect_agent` 返回 `not_found`
/// 且之后每次调用都重复失败。
///
/// 修复后：身份层失败被识别为「root 过期」，重挂一次后成功。
#[tokio::test]
async fn a_replaced_daemon_invalidates_the_cached_root() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let cfg = config_for(repo.path());
    let models = isolated_models_path(repo.path());

    let attached = Arc::new(
        yi_agent_subagent::attach::attach_project_runtime(
            &cfg,
            runtime.path().to_path_buf(),
            &models,
        )
        .expect("a clean git repo must attach"),
    );
    let socket = attached.socket_path.clone();
    let old_session = attached.attached_root.session_id.clone();
    let binding = yi_agent_subagent::binding::RuntimeBinding::managed(
        &cfg,
        runtime.path().to_path_buf(),
        models.clone(),
        Arc::clone(&attached),
    );
    let root = ThreadRoot::from_handle(Arc::clone(&binding), attached.attached_root.clone());
    let mut registry = yi_agent_core::ToolRegistry::new();
    yi_agent_subagent::register_application_subagent_tools(
        &mut registry,
        Arc::clone(&root),
        yi_agent_tools::SandboxController::new(
            yi_agent_core::autonomy::YoloSwitch::new(false),
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            false,
        ),
        yi_agent_subagent::CallerContext::unbound(),
    );
    let inspect = registry.get("inspect_agent").expect("inspect_agent is registered");

    // 本进程停掉 daemon，然后由「同伴」在同一 runtime 目录起一个新的。
    let _ = yi_agent_store::ipc::send_request(&socket, yi_agent_store::ipc::IpcRequest::Stop);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while matches!(
        yi_agent_subagent::attach::probe_runtime(&socket),
        yi_agent_subagent::attach::RuntimeProbe::Healthy
    ) {
        assert!(std::time::Instant::now() < deadline, "the old daemon must stop");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let fresh = yi_agent_subagent::attach::attach_project_runtime(
        &cfg,
        runtime.path().to_path_buf(),
        &models,
    )
    .expect("a peer brings a fresh daemon up");
    assert_ne!(
        fresh.attached_root.session_id, old_session,
        "the replacement minted a new session, so the old root is gone"
    );
    let _fresh = Arc::new(fresh);

    // 前提：socket 全程健康，没有触发过任何 liveness 错误——这正是形态 B：
    // 传输层看起来完美，只有身份层知道 root 已经不作数。
    assert!(matches!(
        yi_agent_subagent::attach::probe_runtime(&socket),
        yi_agent_subagent::attach::RuntimeProbe::Healthy
    ));
    let root_before = root.root_task_id().expect("the root is cached");

    // 为什么不断言错误码：这里传的是随机 UUID，**即使**重挂成功，重试也会因
    // 「没有这个 task」而返回 `not_found`——与「陈旧 root」的错误码完全一样。
    // 想用真实 child 造更强的判据也不成立：陈旧 root 造不出 child（旧 daemon
    // 已停），而用同伴的 root 造出来的 child 又不归本 root 管（会得到
    // `authority_denied`）。所以形状 B 的可观测判据只能是「root 换过了、
    // 且换过之后可用」，本用例就钉这两条。
    let _ = inspect
        .call(serde_json::json!({
            "task_id": "11111111-1111-4111-8111-111111111111",
        }))
        .await;

    let root_after = root.root_task_id().expect("the root is re-attached");
    assert_ne!(
        root_after, root_before,
        "a stale root must be replaced by one minted against the new runtime"
    );
    let handle = root.handle().expect("the re-attached root resolves a handle");
    let probe = yi_agent_store::ipc::send_request(
        &handle.socket_path,
        yi_agent_store::ipc::IpcRequest::ListTaskSummaries {
            session_id: Some(handle.session_id.clone()),
            active_only: false,
        },
    )
    .expect("the socket answers");
    assert!(
        matches!(
            probe,
            yi_agent_store::ipc::IpcResponse::TaskSummaries { .. }
        ),
        "the re-attached root must be usable, got {probe:?}"
    );
}
```

- [ ] **Step 2: 运行测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent --test attach_delegation a_replaced_daemon_invalidates -- --nocapture
```

Expected: FAIL，`a stale root must be replaced by one minted against the new runtime`
（修复前 `root.attach()` 在生成内短路，`root_task_id()` 不变）。

**为什么这里能红**：`child_count`/`root_task_id` 这类断言在修复前必然失败，
因为它们要求「缓存被换成新 runtime 铸的 root」。**不要**改成断言错误码——
见上面「为什么不断言错误码」那段。

- [ ] **Step 3: 改 `DaemonInspectAgentTool::call`**

把 `let handle = match self.root.handle() { ... };` 之后的 `send_request` 段替换为：

```rust
        let response = self.root.send_as_root(RetryPolicy::Idempotent, |handle| {
            yi_agent_store::ipc::IpcRequest::InspectChild {
                session_id: handle.session_id.clone(),
                caller_task_id: handle.task_id.clone(),
                capability: handle.capability.clone(),
                task_id: task_id.clone(),
            }
        });
        // 注意应答变体是 `TaskDetail`，不是 `ChildInspected`（`ipc.rs:3912`）。
        let detail = match response {
            Ok(yi_agent_store::ipc::IpcResponse::TaskDetail(detail)) => detail,
            Ok(other) => return ToolResult::error(format_ipc_rejection("inspect request", &other)),
            Err(error) => return ToolResult::error(format!("daemon is unavailable: {error}")),
        };
```

**执行者注意**：既有 `call`（`lib.rs:2196` 起）在 `let detail = ...` 之后还有一段
`include_diff` 分支，它用 `handle.socket_path` 发 `ReadTaskDiff`。那段**必须保留**，
但要把 `handle` 的来源改掉：因为 `send_as_root` 不返回句柄，需要在
`include_diff` 为真时用 `self.root.handle()` 重新取一次当届句柄
（此时 root 已确认可用）。其余组装逻辑（`delivery_json`、`text_completion_report`、
`payload`）原样接回，不要重写。

- [ ] **Step 4: 改 `DaemonCancelAgentTool::call`** 与 **`DaemonWaitAgentTool::call`**

同构替换：

```rust
        // cancel / wait
        let response = self.root.send_as_root(RetryPolicy::Idempotent, |handle| {
            yi_agent_store::ipc::IpcRequest::CancelChild {
                session_id: handle.session_id.clone(),
                caller_task_id: handle.task_id.clone(),
                capability: handle.capability.clone(),
                task_id: task_id.clone(),
                recursive,
            }
        });
```

```rust
        let response = self.root.send_as_root(RetryPolicy::Idempotent, |handle| {
            yi_agent_store::ipc::IpcRequest::WaitAgent {
                session_id: handle.session_id.clone(),
                caller_task_id: handle.task_id.clone(),
                capability: handle.capability.clone(),
                mode: mode.clone(),
                timeout_ms: Some(TUI_WAIT_AGENT_TIMEOUT_MS),
            }
        });
```

两者的 `match` 分支（`Ok(...)` 组装、`Ok(other)` 拒绝、`Err` 不可用）保持原样。

- [ ] **Step 5: 运行测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent
```

Expected: lib 全绿 + e2e 13 passed。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-subagent/src/lib.rs crates/yi-agent-subagent/tests/attach_delegation.rs
git commit -m "fix(subagent): re-attach a stale conversation root from the read-only tools"
```

---

### Task 6: 不可重放的调用必须只发一次并给出指引

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/tests/attach_delegation.rs`（追加）
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/thread_root.rs`（变异验证用，最终不改）

**Interfaces:**
- Consumes: Task 1 的 `stale_root_hint()`、Task 3 / Task 4 的 `SingleShot` 接线、
  `IpcRequest::ListTaskSummaries`（`ipc.rs:387`）、`IpcResponse::TaskSummaries`
  （`ipc.rs:536`）、`IpcTaskSummary::parent_task_id`（`ipc.rs:680`）。
- Produces: 无新 API。

**执行者必读：这个用例为什么必须是「形态 B」**

要证明「没有重放」，必须让**重放本身可观测**。

- 形态 A（daemon 死了）：请求根本没收发出去，重发是设计上正确且必要的，
  重发后恰好产生一个子任务。重放与否在 daemon 侧不留任何差异 → 测不出来。
- 形态 B（daemon 被同伴替换）：第一次请求**已送达**并被旧身份拒绝
  （`not_found`）。此时若重放，新 runtime 会真的执行它 → **多出一个子任务**；
  若不重放，子任务数为 0。这才是可观测判据。

因此本用例走形态 B，并在调用**之后**（而不是之前）读 root 与子任务数——
调用前先 `attach()` 会把缓存刷成同伴的新 root，形态 B 的前提当场消失。

- [ ] **Step 1: 写测试**

在 `tests/attach_delegation.rs` 末尾追加（完整代码，不含占位）：

```rust
/// 不可重放的请求在陈旧 root 上**只能发一次**：重放会造出第二个子任务。
/// 同时错误必须说清「已修好、再试一次」，而不是裸 `not_found`。
///
/// 这里用形态 B（daemon 被同伴替换）：第一次 `spawn` 已送达并被旧身份拒绝，
/// 若实现重放，新 runtime 会真的建出一个子任务，于是「子任务数为 0」就失败。
/// 形态 A 测不出这件事——那边重发是必要的，且恰好只产生一个子任务。
#[tokio::test]
async fn a_non_idempotent_call_reports_the_stale_root_instead_of_replaying() {
    /// 当前缓存 root 名下的子任务数。
    fn child_count(root: &ThreadRoot) -> usize {
        // integration test 是外部 crate，只能走公开的 `handle()`；这里只是
        // 只读计数，不需要自愈通道。
        let handle = root.handle().expect("the root is attached");
        let response = yi_agent_store::ipc::send_request(
            &handle.socket_path,
            yi_agent_store::ipc::IpcRequest::ListTaskSummaries {
                session_id: Some(handle.session_id.clone()),
                active_only: false,
            },
        )
        .expect("the socket answers");
        match response {
            yi_agent_store::ipc::IpcResponse::TaskSummaries { tasks } => tasks
                .iter()
                .filter(|task| task.parent_task_id.as_deref() == Some(handle.task_id.as_str()))
                .count(),
            other => panic!("unexpected response: {other:?}"),
        }
    }

    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let cfg = config_for(repo.path());
    let models = isolated_models_path(repo.path());

    let attached = Arc::new(
        yi_agent_subagent::attach::attach_project_runtime(
            &cfg,
            runtime.path().to_path_buf(),
            &models,
        )
        .expect("a clean git repo must attach"),
    );
    let socket = attached.socket_path.clone();
    let old_session = attached.attached_root.session_id.clone();
    let binding = yi_agent_subagent::binding::RuntimeBinding::managed(
        &cfg,
        runtime.path().to_path_buf(),
        models.clone(),
        Arc::clone(&attached),
    );
    let root = ThreadRoot::from_handle(Arc::clone(&binding), attached.attached_root.clone());
    let mut registry = yi_agent_core::ToolRegistry::new();
    yi_agent_subagent::register_application_subagent_tools(
        &mut registry,
        Arc::clone(&root),
        yi_agent_tools::SandboxController::new(
            yi_agent_core::autonomy::YoloSwitch::new(false),
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            false,
        ),
        yi_agent_subagent::CallerContext::unbound(),
    );
    let spawn = registry.get("spawn_agent").expect("spawn_agent is registered");

    // 停掉本进程的 daemon，由同伴在同一 runtime 目录起一个新的 ⇒ 缓存 root 过期，
    // 但 socket 全程健康（形态 B）。
    let _ = yi_agent_store::ipc::send_request(&socket, yi_agent_store::ipc::IpcRequest::Stop);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while matches!(
        yi_agent_subagent::attach::probe_runtime(&socket),
        yi_agent_subagent::attach::RuntimeProbe::Healthy
    ) {
        assert!(std::time::Instant::now() < deadline, "the old daemon must stop");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let fresh = yi_agent_subagent::attach::attach_project_runtime(
        &cfg,
        runtime.path().to_path_buf(),
        &models,
    )
    .expect("a peer brings a fresh daemon up");
    assert_ne!(fresh.attached_root.session_id, old_session);
    let _fresh = Arc::new(fresh);

    // 注意：这里**不要**先 `root.attach()`。那会把缓存刷成同伴的 root，形态 B
    // 的前提就没了。直接调用即可。
    let result = spawn
        .call(serde_json::json!({ "task": "must not run twice", "fork": false }))
        .await;

    assert!(
        result.is_error,
        "a stale-root spawn must not silently succeed: {:?}",
        result.content
    );
    let text = format!("{:?}", result.content);
    assert!(
        text.contains("Retry this call once"),
        "the error must tell the model to retry once: {text}"
    );
    // 调用已经触发了 repair + 重挂，此时 `handle()` 拿到的是新 runtime 上的 root。
    assert_eq!(
        child_count(&root),
        0,
        "the spawn must NOT have been replayed: no child may exist under the new root"
    );
}
```

- [ ] **Step 2: 运行测试**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin"
cd yi-agent-rs && cargo test -p yi-agent-subagent --test attach_delegation a_non_idempotent -- --nocapture
```

Expected: PASS（`SingleShot` 分支在 Task 1 已落地，本任务是给那条分支补上
「不重放」的证据，不是新写实现）。若 FAIL 且缺 `Retry this call once`，说明
Task 4 的 `spawn_outcome` 没透出 `stale_root_hint()`，回 Task 4 Step 3 修。

- [ ] **Step 3: 反向验证（本任务的核心交付）**

把 `thread_root.rs` 里 `SingleShot` 分支的这三行临时删掉（或改成不返回）：

```rust
                if policy == RetryPolicy::SingleShot {
                    return Err(stale_root_hint().to_string());
                }
```

重跑 Step 2 的命令。

Expected: **FAIL**，`the spawn must NOT have been replayed: no child may exist`。
恢复这三行后重跑：**PASS**。两次输出都要贴进提交信息。

- [ ] **Step 4: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-subagent/tests/attach_delegation.rs
git commit -m "test(subagent): pin that a stale root is never replayed for a spawn"
```

---

### Task 7: 删除死代码 `current_or_repair` 并修正失真文档

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/binding.rs:262-267`（删除）
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/binding.rs:64-69`（注释）
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/binding.rs:456-458`（注释）
- Modify: `docs/project-management/yi-agent-subagent.md`
- Modify: `docs/bug-list.md`

**Interfaces:**
- Consumes: 无。
- Produces: 无。公开 API **移除** `RuntimeBinding::current_or_repair`（全仓库无调用点，已确认）。

- [ ] **Step 1: 确认无调用点**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
grep -rn "current_or_repair" --include=*.rs yi-agent-rs/crates/ | grep -v "/.worktrees/"
```

Expected: 仅 `binding.rs:262`（定义）与两处注释。**若有别的调用点，停下来不要删**，
改用「让 `current()` 能返回 Err」的方案（spec §3.4 的另一条路）。

- [ ] **Step 2: 删除并改注释**

删除 `binding.rs:262-267` 的整个 `current_or_repair`。
把 `binding.rs:66` 的

```
    /// 由宿主在取句柄时刷新（`current` / `current_or_repair` / `send` 都经过
```

改为

```
    /// 由宿主在取句柄时刷新（`current` / `send` 都经过
```

把 `binding.rs:458` 的「下次真要委派时 `current_or_repair` / `send`」改为
「下次真要委派时 `send`」。

- [ ] **Step 3: 改模块文档**

`docs/project-management/yi-agent-subagent.md` 中把

> 委派工具持 `Arc<RuntimeBinding>`，每次调用经 `current_or_repair` 解析当届 socket 与 root

改为

> 委派工具持 `Arc<ThreadRoot>`，每次调用经 `ThreadRoot::send_as_root` 解析当届
> socket 与 root；传输层失败（`is_liveness_error`）或身份层失败
> （`IpcResponse::Error { code: NotFound | AuthorityDenied }`）都会自愈一次。
> 身份层失败只在请求幂等时重发，否则返回 `stale_root_hint()` 让模型重试一次。

并在该文件 Features 列表末尾追加一条 `[x]`，注明代码位置与验证命令。

- [ ] **Step 4: 记入 bug-list**

`docs/bug-list.md` 追加两条 `[x]`，写清根因、代码位置、验证命令、
以及「逐条回退即红」的证据。

- [ ] **Step 5: 运行全量验证**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin"
ps aux | grep -v grep | grep -E "cargo|rustc" | head
cd yi-agent-rs && cargo test -p yi-agent-subagent && cargo test -p yi-agent-app-server --lib
```

Expected: 两个 crate 全绿，含 `a_dead_runtime_is_repaired`。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add -A
git commit -m "docs(subagent): drop the dead current_or_repair path and record the fix"
```

---

## 完成判据

- `cargo test -p yi-agent-subagent`：lib 87 + e2e 16 全绿（84+3 / 9+7，按实际增量为准）。
- `cargo test -p yi-agent-app-server --lib`：全绿。
- 四个新 e2e 用例逐条反向验证（Task 2 Step 2 的真红、Task 6 Step 3 的变异红）。
- `grep -rn "current_or_repair" yi-agent-rs/crates/` 无命中。
- `cargo fmt --all` 干净。
