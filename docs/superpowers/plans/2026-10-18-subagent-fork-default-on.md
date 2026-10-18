# Subagent 默认 fork 调用者上下文 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 `spawn_agent` 的 `fork` 从「默认关闭、显式可选」翻转为「默认开启、可用 `fork: false` 关闭」，默认路径上 fork 不可行时降级为无 fork 而不是让 spawn 失败。

**Architecture:** 默认值由 `spawn_fork()` 单点决定，改为返回三态 `ForkChoice`。`spawn_fork_token()` 据此在失败时区分「显式要求 → 报错」与「默认来的 → warn + 降级」。降级统一为「回落 `fork_token = None` 后照常发 spawn」，不需要本地镜像 daemon 上限、不需要重发请求、不依赖 spawn 幂等性——因为 fork 上传发生在 spawn 请求之前。模型面对的两处文案（工具 schema 参数描述、系统提示词）同步改写，成功返回新增 `forked` 字段使降级可观测。

**Tech Stack:** Rust workspace（`yi-agent-subagent`、`yi-agent-core`、`yi-agent`），cargo test，`tracing`，`serde_json`。

## Global Constraints

- 硬失败语义只对**显式** `fork: true` 保留；默认路径（省略 `fork`）上 fork 任何失败都必须降级为无 fork，spawn 必须成功。
- 显式非布尔 `fork` 值（如 `"yes"`、`1`）仍必须报错，不得忽略。
- schema `required` 保持 `["task"]`，`additionalProperties` 保持 `false`。
- 两个 spawn 工具（`DaemonSpawnAgentTool`、`DaemonApplicationSpawnAgentTool`）共用唯一的 `spawn_agent_schema()`；文案只改这一处。
- `AgentConfig::default_system_prompt()` 同时是 root 与 subagent 的提示词，只改这一处。
- 不改 fork 传输协议（分块 base64 上传）、不改 32 MiB 上限本身、不改 worker 的 provider/model 选择。
- 提交前在 `yi-agent-rs/` 下跑 `cargo fmt --all`。
- commit message 用 conventional commits，不写 `Co-Authored-By` 行。
- 所有路径相对 worktree 根 `/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fork-default-on`；cargo 命令在 `yi-agent-rs/` 下跑。
- 跑测试前先 `ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent"` 确认无残留进程；按 crate 跑，不要 `--workspace`。
- `RuntimeHandle` 的字段全部 `pub`（`binding.rs:18-25`），测试里可直接字面量构造。**不要**用 `test_root`（`lib.rs:2531`）——它返回 `Arc<ThreadRoot>`，不是 `RuntimeHandle`。

---

### Task 1: `ForkChoice` 三态、默认开启与按来源降级

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:1665-1673`（`spawn_fork` 及其文档注释）
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:1801-1812`（`spawn_fork_token`）
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:1753-1759`（`upload_forked_context` 的文档注释）
- Test: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs`（`mod tests`，紧邻 `daemon_spawn_mode_uses_the_canonical_parser`，约 2813 行）

**Interfaces:**
- Consumes: 无（本任务是最底层改动）。
- Produces:
  - `enum ForkChoice { Default, Yes, No }`（`#[derive(Debug, Clone, Copy, PartialEq, Eq)]`）
  - `fn spawn_fork(args: &Value) -> Result<ForkChoice, ToolResult>` —— 省略 → `Default`，`true` → `Yes`，`false` → `No`，非布尔 → `Err`。
  - `fn spawn_fork_token(args: &Value, handle: &RuntimeHandle, caller: &CallerContext) -> Result<Option<String>, ToolResult>` —— `No` → `Ok(None)`；`Yes` → 失败上抛；`Default` → 失败时 `tracing::warn!` + `Ok(None)`。

- [ ] **Step 1: 确认干净起点并跑基线测试**

Run:
```bash
cd yi-agent-rs && ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent" || echo "no stragglers"
cd yi-agent-rs && cargo test -p yi-agent-subagent --lib 2>&1 | tail -20
```
Expected: 编译通过、测试全绿（基线）。此时 `spawn_fork` 仍是 `Result<bool, ToolResult>`。

- [ ] **Step 2: 写失败测试（三态解析 + 默认值 + 降级 + 逃生口）**

在 `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs` 的 `mod tests` 内，紧邻 `daemon_spawn_mode_uses_the_canonical_parser`（约 2813 行）之后插入：

```rust
    /// A fresh handle over literals, for exercising the fork helpers without a
    /// live daemon. `test_root` cannot be used here: it is a `ThreadRoot`, not
    /// the `RuntimeHandle` these helpers take.
    fn fork_test_handle() -> crate::binding::RuntimeHandle {
        crate::binding::RuntimeHandle {
            socket_path: PathBuf::from("/tmp/unused.sock"),
            workspace_root: PathBuf::from("/tmp/unused-ws"),
            session_id: "s".into(),
            task_id: "t".into(),
            capability: "c".into(),
        }
    }

    #[test]
    fn daemon_spawn_fork_defaults_to_yes_and_parses_both_explicit_spellings() {
        assert_eq!(
            spawn_fork(&json!({})).unwrap(),
            ForkChoice::Default,
            "an omitted fork must take the new default-on path"
        );
        assert_eq!(
            spawn_fork(&json!({ "fork": true })).unwrap(),
            ForkChoice::Yes,
            "an explicit true is a caller requirement"
        );
        assert_eq!(
            spawn_fork(&json!({ "fork": false })).unwrap(),
            ForkChoice::No,
            "an explicit false is the escape hatch"
        );

        let bad = spawn_fork(&json!({ "fork": "yes" })).unwrap_err();
        assert!(bad.is_error);
        assert!(matches!(
            bad.content.as_slice(),
            [yi_agent_core::ContentBlock::Text(text)]
                if text.contains("fork must be a boolean")
        ));
    }

    #[test]
    fn an_explicit_fork_false_never_attempts_an_upload() {
        let handle = fork_test_handle();
        let caller = CallerContext::unbound();
        // `fork:false` must short-circuit before touching any socket, so the
        // unreachable path is fine: reaching it would be the failure.
        let token = spawn_fork_token(&json!({ "fork": false }), &handle, &caller)
            .expect("fork:false is a clean no-op, not an error");
        assert_eq!(token, None, "fork:false must not upload anything");
    }

    /// The caller's transcript being unavailable is fatal only when the caller
    /// demanded fork. On the default path it degrades: the delegation still
    /// happens, just without inherited context.
    #[test]
    fn an_unbound_caller_degrades_on_the_default_path_but_fails_when_demanded() {
        let handle = fork_test_handle();
        let caller = CallerContext::unbound();

        let default_token = spawn_fork_token(&json!({}), &handle, &caller)
            .expect("the default path must degrade, not fail, when no context is bound");
        assert_eq!(default_token, None, "a degraded spawn carries no fork token");

        let demanded = spawn_fork_token(&json!({ "fork": true }), &handle, &caller)
            .expect_err("an explicit fork:true must report the missing context");
        assert!(demanded.is_error);
        assert!(matches!(
            demanded.content.as_slice(),
            [yi_agent_core::ContentBlock::Text(text)]
                if text.contains("fork requested but no caller context available")
        ));
    }

    /// Every upload failure degrades on the default path, not just the
    /// "no context" one. A bound caller over an unreachable socket exercises the
    /// transport-failure class, which is the same class an oversized payload
    /// lands in (the daemon rejects it at `BeginForkUpload`) -- so this test
    /// covers the degradation policy without needing a daemon whose size cap is
    /// settable only through a process-global env var.
    #[test]
    fn an_unreachable_daemon_degrades_on_the_default_path_but_fails_when_demanded() {
        let handle = fork_test_handle();
        let session = Arc::new(Mutex::new(Session::from_messages(vec![Message::user(
            "parent history",
        )])));
        let caller = CallerContext::new(session);

        let default_token = spawn_fork_token(&json!({}), &handle, &caller)
            .expect("a transport failure must degrade on the default path");
        assert_eq!(default_token, None, "a degraded spawn carries no fork token");

        let demanded = spawn_fork_token(&json!({ "fork": true }), &handle, &caller)
            .expect_err("an explicit fork:true must report the transport failure");
        assert!(demanded.is_error);
        assert!(matches!(
            demanded.content.as_slice(),
            [yi_agent_core::ContentBlock::Text(text)]
                if text.contains("daemon is unavailable")
        ));
    }
```

- [ ] **Step 3: 跑测试确认失败**

Run:
```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent --lib daemon_spawn_fork_defaults 2>&1 | tail -20
```
Expected: 编译失败 —— `cannot find type ForkChoice in this scope`，以及 `assert_eq!` 两侧类型不匹配（`bool` vs `ForkChoice`）。这正是要修的状态。

- [ ] **Step 4: 定义 `ForkChoice` 并改 `spawn_fork`**

把 `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:1665-1673` 整块替换为：

```rust
/// Which fork behaviour a `spawn_agent` call asked for.
///
/// The distinction matters because the strict contract differs by origin: a
/// caller that explicitly asked to fork must never silently receive a
/// contextless child, while the default path must degrade to an unforked spawn
/// rather than fail a routine delegation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForkChoice {
    /// `fork` was omitted: fork, but degrade instead of failing.
    Default,
    /// `fork: true`: fork, and report a failure if that is impossible.
    Yes,
    /// `fork: false`: do not fork at all.
    No,
}

/// Resolves the optional `fork` argument for a daemon `spawn_agent` call.
///
/// Omitted means fork (the default); an explicit non-boolean value is rejected
/// rather than ignored.
fn spawn_fork(args: &Value) -> Result<ForkChoice, ToolResult> {
    match args.get("fork") {
        None => Ok(ForkChoice::Default),
        Some(Value::Bool(true)) => Ok(ForkChoice::Yes),
        Some(Value::Bool(false)) => Ok(ForkChoice::No),
        Some(_) => Err(ToolResult::error("fork must be a boolean")),
    }
}
```

- [ ] **Step 5: 改 `spawn_fork_token`**

把 `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:1801-1812` 整块替换为：

```rust
/// Resolves the `fork_token` a spawn request should carry: uploading the
/// caller's transcript when `fork` is set, and `None` otherwise.
///
/// A [`ForkChoice::Default`] attempt that cannot produce a payload degrades to
/// `None`; a [`ForkChoice::Yes`] attempt reports the failure, because a caller
/// that asked for a fork must not silently get a contextless child.
fn spawn_fork_token(
    args: &Value,
    handle: &crate::binding::RuntimeHandle,
    caller: &CallerContext,
) -> Result<Option<String>, ToolResult> {
    match spawn_fork(args)? {
        ForkChoice::No => Ok(None),
        ForkChoice::Yes => upload_forked_context(&handle.socket_path, handle, caller).map(Some),
        ForkChoice::Default => match upload_forked_context(&handle.socket_path, handle, caller) {
            Ok(token) => Ok(Some(token)),
            Err(error) => {
                tracing::warn!(
                    ?error,
                    "default fork is unavailable; spawning without inherited context"
                );
                Ok(None)
            }
        },
    }
}
```

- [ ] **Step 6: 更新 `upload_forked_context` 的文档注释**

`yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:1753-1759` 的注释现在写着「An unbound caller is an explicit error: a caller that asked for `fork:true` must never silently receive a contextless child.」——这句在默认路径降级后只对 `fork:true` 成立。替换为：

```rust
/// Uploads the caller's current transcript as a forked-context payload and
/// returns the token the spawn request names.
///
/// The snapshot is taken here, at call time, so the child inherits the caller's
/// history as of this delegation rather than at tool-assembly time.
///
/// Failures here are returned as `Err`; whether they are fatal is decided by the
/// caller ([`spawn_fork_token`]): a `fork:true` request reports them, while the
/// default path degrades to an unforked spawn. This function deliberately does
/// not classify its failures, because the degradation policy is identical for
/// both cases and the daemon's size cap is env-configurable
/// (`YI_AGENT_FORK_MAX_BYTES`), so a client-side mirror of it would only drift.
```

- [ ] **Step 7: 跑测试确认通过**

Run:
```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent --lib daemon_spawn_fork_defaults 2>&1 | tail -20
cd yi-agent-rs && cargo test -p yi-agent-subagent --lib an_explicit_fork_false_never_attempts 2>&1 | tail -20
cd yi-agent-rs && cargo test -p yi-agent-subagent --lib an_unbound_caller_degrades 2>&1 | tail -20
cd yi-agent-rs && cargo test -p yi-agent-subagent --lib an_unreachable_daemon_degrades 2>&1 | tail -20
```
Expected: 四个都 PASS。

- [ ] **Step 8: 跑整个 crate，确认范围内的既有测试未被打破**

Run:
```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent --lib 2>&1 | tail -30
```
Expected: **全绿**。`--lib` 范围内没有任何既有测试以「省略 `fork`」的方式调用 spawn 工具
（唯一的工具级调用是 `fork_requested_without_a_bound_caller_fails_explicitly`
（lib.rs:2362 附近），它显式传 `fork: true`，走 `ForkChoice::Yes` 路径，行为未变）。
schema 断言 `both_spawn_schemas_offer_fork_and_default_to_false` 此时仍应 **PASS**：
Task 1 只改 `spawn_fork`/`spawn_fork_token`，**没有**改 schema 默认值（那在 Task 2）。

关键：若 `fork_requested_without_a_bound_caller_fails_explicitly` 失败，说明 Step 5 的
`Yes` 分支写错了（把该报的错吞成了降级）。

**已知且刻意为之的红**：跨 crate 的 e2e 测试
`yi-agent-rs/crates/yi-agent/tests/subagent_fork_e2e.rs` 里的
`an_unforked_spawn_sends_only_the_objective_to_the_child` 会在本任务后失败——它断言
「省略 fork ⇒ 不继承」，正是本次要反转的语义。**Task 1 不跑、也不修它**；Task 3
会用带 `forked:true` 断言的新版本替换它。不要为了让它变绿而提前改动。

若出现上述两条以外的失败，停下来先查。

- [ ] **Step 9: Commit**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fork-default-on
git add yi-agent-rs/crates/yi-agent-subagent/src/lib.rs
git commit -m "feat(subagent): default fork on, degrading by origin

spawn_fork now returns a three-state ForkChoice so strictness can differ
by origin: an omitted fork degrades to an unforked spawn, while an
explicit fork:true keeps reporting failure.

Upload failures are deliberately not classified. The daemon's size cap is
env-configurable (YI_AGENT_FORK_MAX_BYTES), so a client-side mirror would
drift, and the degradation policy is identical either way. Because the
fork upload precedes the spawn request, degrading just drops the token
from a spawn that is still sent exactly once."
```

---

### Task 2: schema 文案与默认值（A）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:1873-1877`（`spawn_agent_schema()` 的 `fork` 属性）
- Test: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:2333-2343`（`both_spawn_schemas_offer_fork_and_default_to_false`）

**Interfaces:**
- Consumes: 无。
- Produces: `spawn_agent_schema()["properties"]["fork"]["default"] == true`——这是 Task 3 的 e2e 断言与 Task 4 文档描述的权威来源。

- [ ] **Step 1: 改既有测试断言（先让它失败）**

把 `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:2333-2343` 整块替换为：

```rust
    #[test]
    fn both_spawn_schemas_offer_fork_and_default_to_true() {
        for schema in [application_spawn_schema(), core_spawn_schema()] {
            assert_eq!(schema["properties"]["fork"]["type"], "boolean");
            assert_eq!(schema["properties"]["fork"]["default"], true);
            assert_eq!(
                schema["required"],
                json!(["task"]),
                "fork must stay optional"
            );
        }
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run:
```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent --lib both_spawn_schemas_offer_fork 2>&1 | tail -20
```
Expected: FAIL —— `left: Bool(false), right: Bool(true)`。这证明测试真的在盯这个字段。

- [ ] **Step 3: 改 schema**

把 `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:1873-1877` 的 `fork` 属性替换为：

```rust
            "fork": {
                "type": "boolean",
                "default": true,
                "description": "The child inherits this conversation's history by default. Pass false for an isolated child that should start clean."
            }
```

- [ ] **Step 4: 跑测试确认通过**

Run:
```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent --lib both_spawn_schemas_offer_fork 2>&1 | tail -20
cd yi-agent-rs && cargo test -p yi-agent-subagent --lib spawn_agent_schema_pins 2>&1 | tail -20
```
Expected: 两个都 PASS。第二个（`spawn_agent_schema_pins_the_wire_contract`，lib.rs:2345 附近）未改也应通过，它只断言 `type`/`required`/`additionalProperties`/`mode` enum/描述字符串。

- [ ] **Step 5: Commit**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fork-default-on
git add yi-agent-rs/crates/yi-agent-subagent/src/lib.rs
git commit -m "feat(subagent): flip the fork schema default to true

Rewrites the fork parameter description from a discouragement (\"pass true
only when the child genuinely needs the history\") into the default
behaviour, with false as the escape hatch. One schema feeds both spawn
tools, so the root and worker paths change together."
```

---

### Task 3: 系统提示词规则（B）、共用响应 helper 与 `forked` 返回字段

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs:217-225`（`default_system_prompt()` 的 "Subagent integration" 段）
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs`：新增 `spawn_outcome()` helper，并把两个 `call()` 的响应处理块（`:1947-1959`、`:2219-2231`）替换为对它的调用
- Modify: `yi-agent-rs/crates/yi-agent/tests/subagent_fork_e2e.rs`（文件头 doc、`an_unforked_spawn_sends_only_the_objective_to_the_child`、`fork_false_matches_the_unforked_path`、新增 helper）

**Interfaces:**
- Consumes: Task 1 的 `spawn_fork_token`（`Option<String>` 的 `Some`/`None` 决定 `forked`）；Task 2 的 schema 默认值。
- Produces:
  - `fn spawn_outcome(response: Result<IpcResponse, IpcError>, socket: &Path, fork_token: &Option<String>, objective: &str) -> ToolResult` —— 两个 spawn 工具共用的响应处理；成功时返回含 `"forked": fork_token.is_some()` 的 JSON。
  - `spawn_agent` 成功返回的 JSON 含 `"forked": bool`；`default_system_prompt()` 含一条 fork 规则。Task 4 的文档描述依赖这两者。

- [ ] **Step 1: 加 `fork_flag_of` helper**

在 `yi-agent-rs/crates/yi-agent/tests/subagent_fork_e2e.rs` 的 `texts_of` 函数（:91-101）之后插入：

```rust
/// Reads the `forked` flag out of a `spawn_agent` result payload.
///
/// The tool returns JSON as a text block, so the flag is parsed back out. A
/// missing flag is a hard failure rather than a defaulted `false`: the whole
/// point of the field is to tell the caller which path was taken.
fn fork_flag_of(result: &yi_agent_core::ToolResult) -> bool {
    let text = result
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::Text(text) => Some(text.clone()),
            _ => None,
        })
        .expect("a spawn result carries a text block");
    let payload: serde_json::Value = serde_json::from_str(&text).expect("a spawn result is JSON");
    payload
        .get("forked")
        .and_then(serde_json::Value::as_bool)
        .expect("a spawn result must report whether the child was forked")
}
```

- [ ] **Step 2: 改既有测试为「默认继承 + `forked:true`」**

把 `yi-agent-rs/crates/yi-agent/tests/subagent_fork_e2e.rs:305-333` 的 `an_unforked_spawn_sends_only_the_objective_to_the_child` 整块替换为：

```rust
/// The new default: omitting `fork` hands the caller's history to the child,
/// and the result says so.
#[test]
fn an_omitted_fork_inherits_the_caller_history_by_default() {
    let fixture = ForkFixture::start();

    // `fork` is omitted entirely, which is now the forking default.
    let result = fixture.spawn(serde_json::json!({ "task": OBJECTIVE }));
    assert!(
        !result.is_error,
        "a default spawn must be accepted: {:?}",
        result.content
    );
    assert!(
        fork_flag_of(&result),
        "the default path must report forked:true: {:?}",
        result.content
    );

    let texts = fixture.first_request_texts();
    assert!(
        texts.iter().any(|text| text.contains(PARENT_HISTORY)),
        "a default child must inherit the caller's history: {texts:?}"
    );
    assert!(
        texts.iter().any(|text| text.contains(OBJECTIVE)),
        "the objective must still reach the child: {texts:?}"
    );
}
```

- [ ] **Step 3: 新增「显式 `fork:true` 报 `forked:true`」测试**

紧接上面的测试之后插入：

```rust
/// An explicit `fork:true` means what it always did, and now also reports
/// `forked:true`.
#[test]
fn fork_true_reports_that_the_child_was_forked() {
    let fixture = ForkFixture::start();

    let result = fixture.spawn(serde_json::json!({
        "task": OBJECTIVE,
        "fork": true,
    }));
    assert!(
        !result.is_error,
        "a forked spawn must be accepted: {:?}",
        result.content
    );
    assert!(
        fork_flag_of(&result),
        "fork:true must report forked:true: {:?}",
        result.content
    );
}
```

- [ ] **Step 4: 给 `fork_false_matches_the_unforked_path` 补 `forked:false` 断言**

在 `yi-agent-rs/crates/yi-agent/tests/subagent_fork_e2e.rs` 的 `fork_false_matches_the_unforked_path`（:338 附近）里，`assert!(!result.is_error, ...)` 之后插入：

```rust
    assert!(
        !fork_flag_of(&result),
        "fork:false must report forked:false: {:?}",
        result.content
    );
```

- [ ] **Step 5: 更新文件头 doc**

把 `yi-agent-rs/crates/yi-agent/tests/subagent_fork_e2e.rs:15-16` 这两行：

```
//! A second run omits `fork` entirely and must reach the provider with only the
//! objective, proving the default stays the old, unforked behavior.
```

替换为：

```
//! Omitting `fork` now forks by default: the child inherits the caller's
//! history, and the result reports `forked: true`. An explicit `fork: false` is
//! the escape hatch that keeps the old, contextless behaviour.
```

- [ ] **Step 6: 跑测试确认失败**

Run:
```bash
cd yi-agent-rs && cargo test -p yi-agent --test subagent_fork_e2e an_omitted_fork_inherits 2>&1 | tail -20
```
Expected: FAIL —— 默认路径此时已经会 fork（Task 1 已改），历史断言会通过，但 `fork_flag_of` 找不到 `forked` 字段而 panic（`a spawn result must report whether the child was forked`）。这证明 `forked` 字段确实缺失。

- [ ] **Step 7: 抽出共用响应 helper `spawn_outcome`**

两个 spawn 工具的响应处理块逐字节相同（main 上就已如此），本任务要给它们都加
`forked` 字段——只写一处才不会漂移。在 `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs`
的 `abort_unconsumed_fork`（约 1790 行）之后插入：

```rust
/// Turns a spawn response into the tool result both spawn tools return.
///
/// Shared because the two tools differ only in which IPC request they send; the
/// response handling -- the queued payload, the abort of an unconsumed fork on
/// rejection, and the daemon-unavailable error -- is identical. Keeping it in one
/// place is what stops the `forked` contract from drifting between them.
fn spawn_outcome(
    response: Result<yi_agent_store::ipc::IpcResponse, yi_agent_store::ipc::IpcError>,
    socket: &Path,
    fork_token: &Option<String>,
    objective: &str,
) -> ToolResult {
    match response {
        Ok(yi_agent_store::ipc::IpcResponse::TaskSpawned { task_id }) => ToolResult::text(
            json!({
                "task_id": task_id,
                "objective": objective,
                "status": "queued",
                "forked": fork_token.is_some(),
            })
            .to_string(),
        ),
        Ok(other) => {
            abort_unconsumed_fork(socket, fork_token);
            ToolResult::error(format_ipc_rejection("spawn request", &other))
        }
        Err(error) => {
            abort_unconsumed_fork(socket, fork_token);
            ToolResult::error(format!("daemon is unavailable: {error}"))
        }
    }
}
```

然后把 `DaemonSpawnAgentTool::call`（`:1947-1959`）的整段 `match response { ... }`
替换为：

```rust
        spawn_outcome(response, &handle.socket_path, &fork_token, &objective)
```

把 `DaemonApplicationSpawnAgentTool::call`（`:2219-2231`）的整段 `match response { ... }`
替换为同样一行：

```rust
        spawn_outcome(response, &handle.socket_path, &fork_token, &objective)
```

注意：两处调用点上方都有 `let objective = task.to_string();`，所以 `&objective` 在
作用域内。`response` 是 `let response = yi_agent_store::ipc::send_request(...)` 的结果，
按值传给它即可（此后不再使用）。

- [ ] **Step 8: 跑 e2e 确认通过**

Run:
```bash
cd yi-agent-rs && cargo test -p yi-agent --test subagent_fork_e2e 2>&1 | tail -30
```
Expected: 全 PASS —— `an_omitted_fork_inherits_the_caller_history_by_default`、`fork_true_reports_that_the_child_was_forked`、`fork_true_spawns_a_child_whose_first_request_carries_the_caller_history`、`fork_false_matches_the_unforked_path`、`a_forked_read_only_child_still_receives_the_history`。

- [ ] **Step 9: 加系统提示词规则（B）**

在 `yi-agent-rs/crates/yi-agent-core/src/agent.rs`，把 `:217-225` 的 "Subagent integration" 段替换为：

```rust
Subagent integration:
- A delegated child inherits your current conversation by default, so it already
  knows what you have read and decided. Pass `fork: false` only when the child
  must start from a clean slate.
- A child runs in the `workdir` you pass to `spawn_agent`. When a change needs
  isolation, create that directory yourself first: `git worktree add <path> -b
  <branch>`, then pass `<path>` as the child's `workdir`. When it does not, pass
  the directory the child should work in directly.
- You are responsible for integrating a delivered commit: run
  `git merge --no-ff <commit>` yourself, resolve any conflicts, and re-run the
  relevant verification. The runtime never creates, tracks, or merges a
  worktree for you."#
```

- [ ] **Step 10: 跑 core 与全量相关测试**

Run:
```bash
cd yi-agent-rs && cargo test -p yi-agent-core --lib 2>&1 | tail -30
cd yi-agent-rs && cargo test -p yi-agent-subagent --lib 2>&1 | tail -20
cd yi-agent-rs && cargo test -p yi-agent --test subagent_fork_e2e 2>&1 | tail -20
```
Expected: 全绿。Task 1 Step 8 记录的失败此时应已全部消除。若 core 有测试逐字断言 system prompt 全文而失败，更新该断言以包含新规则（其余内容不变）。

- [ ] **Step 11: Commit**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fork-default-on
git add yi-agent-rs/crates/yi-agent-core/src/agent.rs yi-agent-rs/crates/yi-agent-subagent/src/lib.rs yi-agent-rs/crates/yi-agent/tests/subagent_fork_e2e.rs
git commit -m "feat(subagent): state the fork default in the prompt and report it

Adds the model-facing rule to the system prompt's Subagent integration
section, so the default is a stated working rule rather than a parameter
description the model may never read. The spawn result now carries
forked:true|false so a degraded spawn is visible to the parent instead of
silently contextless."
```

---

### Task 4: 项目进度文档同步

**Files:**
- Modify: `docs/project-management/yi-agent-subagent.md:65-80`
- Modify: `docs/project-management/README.md`（仅当模块计数需要动）

**Interfaces:**
- Consumes: Task 1-3 的全部行为与文件位置。
- Produces: 无代码符号。

- [ ] **Step 1: 取改动后的真实行号**

Run:
```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fork-default-on
grep -n "fn spawn_fork(\|enum ForkChoice\|fn spawn_fork_token\|fn upload_forked_context\|fn spawn_agent_schema" yi-agent-rs/crates/yi-agent-subagent/src/lib.rs
grep -n "A delegated child inherits" yi-agent-rs/crates/yi-agent-core/src/agent.rs
```
Expected: 得到一组新行号，用于 Step 2 的引用。

- [ ] **Step 2: 改写进度条目**

把 `docs/project-management/yi-agent-subagent.md:65-80` 那条整块替换为（`<行号>` 按 Step 1 的实测值填入）：

```markdown
- [x] `spawn_agent` 的 `fork` 参数与调用者上下文 fork — `fork` 现在**默认开启**
  （schema `default:true`，描述改为「子 agent 默认继承你的对话历史，`fork:false`
  是从零开始的逃生口」），省略即 fork；`CallerContext` 持调用者会话的**活
  `Arc<Mutex<Session>>`**，每次调用经 `snapshot()` 读取「此刻」的 `forkable_messages`
  （协议有效性裁剪，末位悬空 `tool_use` 丢弃），故 Agent 重建/compact 不会让工具读到
  陈旧转录；fork 时 `upload_forked_context`（`src/lib.rs:<行号>`）把 JSON 历史按
  256 KiB 原始切片（`FORK_CHUNK_BYTES`）base64 后逐块
  `BeginForkUpload`/`AppendForkChunk`，返回的 `fork_token` 填进 `SpawnApplicationChild`；
  任何 begin 之后的失败都 `abort_fork_upload` 回收 token（避免 daemon 进程内泄漏载荷）。
  **降级语义按来源区分**（`ForkChoice`，`src/lib.rs:<行号>`）：显式 `fork:true`
  遇 fork 不可行仍报错（无绑定 caller → `fork requested but no caller context available`）；
  默认路径（省略 `fork`）遇到任何 fork 失败都 `tracing::warn!` 后回落无 fork，
  spawn 照常成功并返回 `forked:false`。降级**不分类错误、不镜像 daemon 上限**：
  上限是 daemon 策略、可经 `YI_AGENT_FORK_MAX_BYTES` 覆盖，客户端硬编码会漂移；
  且超限在第一个 `BeginForkUpload` 就被拒（分配 token 之前），而 fork 上传发生在
  spawn 请求之前，故降级只需把 `fork_token` 置 `None`，spawn 总共只发一次。
  worker 侧 `start_with_provider_turn_gate`（`src/lib.rs:<行号>`）在
  `with_session(Session::from_messages(fork))` **之后** `worker_caller.bind(session_handle())`，
  把子 worker 自己的委派工具绑到其实时会话（顺带支持子 agent fork 自己）。模型面对
  的规则写在**两处**：工具 schema 的 `fork` 描述，以及系统提示词
  `AgentConfig::default_system_prompt()` 的 "Subagent integration" 段
  （`yi-agent-core/src/agent.rs:<行号>`）。验证：
  `cargo test -p yi-agent-subagent --lib`、`cargo test -p yi-agent-core --lib`、
  `cargo test -p yi-agent --test subagent_fork_e2e`（后者经**真实注册工具 + 真实
  daemon + 真实 worker 工厂**，断言省略 `fork` 时子 worker 首个 provider 请求含父历史、
  `fork:false` 时只含 objective，且两种结果分别报 `forked:true`/`false`）
  — [设计](../superpowers/specs/2026-10-18-subagent-fork-default-on-design.md)
```

- [ ] **Step 3: 检查计数是否需要更新**

Run:
```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fork-default-on
grep -n "yi-agent-subagent" docs/project-management/README.md
```
Expected: 看到索引行。本条是改写既有 `[x]`、未新增条目，计数**不应变化**。若计数确实不符，按实际 `[x]` 与总条目数修正。

- [ ] **Step 4: Commit**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fork-default-on
git add docs/project-management/yi-agent-subagent.md docs/project-management/README.md
git commit -m "docs(subagent): record fork as default-on with by-origin fallback"
```

---

### Task 5: 全量验证与 fmt

**Files:**
- 无文件改动（纯验证）。

**Interfaces:**
- Consumes: Task 1-4 的全部产出。
- Produces: 一份可交付的绿树。

- [ ] **Step 1: 确认无残留 cargo 进程**

Run:
```bash
ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent" || echo "no stragglers"
```
Expected: `no stragglers`，或只有本次会话自己的进程。若有中断残留的测试二进制（`yi_agent_*`），先 `kill <pid>`，必要时 `find yi-agent-rs/target -name "*.lock" -type f -delete`。

- [ ] **Step 2: fmt**

Run:
```bash
cd yi-agent-rs && cargo fmt --all && git -C .. status --short
```
Expected: fmt 后 `git status` 无改动，或只有本次改动本身的文件。

- [ ] **Step 3: 按 crate 跑全部相关测试**

Run:
```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent 2>&1 | tail -30
cd yi-agent-rs && cargo test -p yi-agent-core 2>&1 | tail -30
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib 2>&1 | tail -30
cd yi-agent-rs && cargo test -p yi-agent --test subagent_fork_e2e 2>&1 | tail -30
```
Expected: 全绿。`yi-agent-app-server` 也在列，因为它的 `rpc_tests` 断言过委派工具集（`:40`、`:51` 附近列出 `spawn_agent`），且它装配 root 工具链。

- [ ] **Step 4: clippy 确认无警告**

Run:
```bash
cd yi-agent-rs && cargo clippy -p yi-agent-subagent --all-targets 2>&1 | tail -20
```
Expected: 无 `error`。若新增的 `ForkChoice` 触发 dead-code 或 unused-variant 警告，说明有分支没被用到——回去检查 Task 1 Step 5 的三个分支是否都可达。

- [ ] **Step 5: 确认没有遗留旧文案**

Run:
```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fork-default-on
grep -rn "Off by default\|default_to_false\|an_unforked_spawn_sends_only" --include=*.rs yi-agent-rs/crates || echo "clean"
```
Expected: `clean`。**不要**去改 `docs/superpowers/specs/2026-10-02-*` 与 `docs/superpowers/plans/2026-10-02-*`——那些是当时决策的历史记录。

- [ ] **Step 6: 最终提交（若 fmt 或清理产生改动）**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fork-default-on
git add -A && git commit -m "chore(subagent): fmt and drop stale fork-default references" || echo "nothing to commit"
```
