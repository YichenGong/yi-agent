# 通用会话驱动（宿主不认识看板）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让宿主不再认识看板——宿主只提供「新建会话 / 向已有会话追加一轮」两个通用原语并回答「会话忙不忙」，队列、状态、名额与 objective 文案全部归插件，合并改由会话里的一轮 turn 执行。

**Architecture:** 沿「拉」方向：宿主每 tick 对声明 `session_driver` 的插件调 `plugin_query(plugin, "driver/next", {threads, results})`，插件在自己的队列里决策并返回至多一个通用动作（`launch` / `append` / `null`），宿主无脑执行并在下一拍回报结果。转发复用现有 `plugin_query`（不新增 IPC 变体）。宿主侧的 `card_scheduler.rs`（803 行）被删除，替换为通用 `session_driver.rs`；插件的 `merge_next()`（裸 git 自动合并）退役。

**Tech Stack:** Rust（`yi-agent-app-server`：tokio、serde_json；`superpowers-kanban-core`/`-runner`：serde、chrono、flock）；测试用 `tokio::test` + 假 daemon socket（沿用 `card_scheduler.rs` 既有 `FakeBoard` 手法）。

**Spec:** `docs/superpowers/specs/2026-10-17-session-driver-decoupling-design.md`

## Global Constraints

- 不改核心 IPC 协议面：**不新增 `IpcRequest` 变体**，转发一律走现有 `IpcRequest::PluginQuery`。
- 宿主非测试代码中不得出现：`board.`（方法名）、`awaiting_merge`、`needs_you`、`merging`、`objective_for`、`KANBAN_PLUGIN`。
- 合并复核不变量：一律以 git 事实裁定（`git merge-base --is-ancestor <source> <base>`），**不看模型自述**。
- 不自动 push；不自动删源分支或 worktree。
- driver 内联在主循环 tick 内，**不得**新起任务并发访问 `threads`（沿用 `card_scheduler_tick` 的借用约束）。
- 动作失败只回报 `{ok:false, error}`，宿主**不得**改写插件状态。
- 命令一律在仓库根执行；测试命令用 `cargo test -p <crate> <filter>`。

---

### Task 1: 清单新增通用 `session_driver` 字段

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/manifest.rs`
- Test: `yi-agent-rs/crates/yi-agent-supervisors/src/manifest.rs`（同文件 `mod tests`）

**Interfaces:**
- Consumes: 无（本任务是最底层）。
- Produces: `SupervisorManifest.session_driver: bool`（`#[serde(default)]`，缺省 `false`）；
  供 Task 2 的发现逻辑读取。字段名 `session_driver` 是**通用**名，不含插件语义。

- [ ] **Step 1: 写失败测试**

在 `manifest.rs` 的 `mod tests` 里加：

```rust
    #[test]
    fn session_driver_defaults_to_false_for_a_manifest_without_the_field() {
        let json = r#"{
            "name": "demo",
            "command": "/bin/demo",
            "args": [],
            "switch_key": "demo",
            "restart_backoff_ms": 1000,
            "restart_backoff_max_ms": 30000,
            "stop_when_disabled": false
        }"#;
        let manifest: SupervisorManifest = serde_json::from_str(json).unwrap();
        assert!(!manifest.session_driver);
    }

    #[test]
    fn session_driver_is_read_when_present() {
        let json = r#"{
            "name": "demo",
            "command": "/bin/demo",
            "args": [],
            "switch_key": "demo",
            "restart_backoff_ms": 1000,
            "restart_backoff_max_ms": 30000,
            "stop_when_disabled": false,
            "session_driver": true
        }"#;
        let manifest: SupervisorManifest = serde_json::from_str(json).unwrap();
        assert!(manifest.session_driver);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-supervisors manifest::tests::session_driver -- --nocapture`
Expected: FAIL — `no field 'session_driver' on type 'SupervisorManifest'`（编译错误）

- [ ] **Step 3: 加字段**

在 `SupervisorManifest` 结构体末尾（`query_socket` 之后）加：

```rust
    /// Whether this process drives conversations: the host asks it (via the
    /// plugin query channel) for the next generic session action to perform.
    /// Generic on purpose -- it names no plugin and the host attaches no
    /// meaning to the action's contents beyond `launch`/`append`/`null`.
    /// Defaults to `false`, so an older manifest keeps loading.
    #[serde(default)]
    pub session_driver: bool,
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p yi-agent-supervisors`
Expected: PASS（既有测试 + 新增 2 条全绿）

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-supervisors/src/manifest.rs
git commit -m "feat(supervisors): add the generic session_driver manifest field"
```

---

### Task 2: 宿主 `session_driver.rs` 的协议类型与纯决策

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/session_driver.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`（`pub(crate) mod session_driver;`）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/session_driver.rs`（同文件 `mod tests`）

**Interfaces:**
- Consumes: 无。
- Produces:
  - `pub(crate) struct DriverAction { pub action_id: String, pub kind: DriverActionKind }`
  - `pub(crate) enum DriverActionKind { Launch { workdir: String, title: String, objective: String }, Append { thread_id: String, objective: String } }`
  - `pub(crate) fn parse_action(value: &serde_json::Value) -> Option<DriverAction>` —— `null`/无 `kind`/未知 `kind` → `None`
  - `pub(crate) struct DriverThread { pub thread_id: String }`（宿主侧通用记账）
  - `pub(crate) struct ActionResult { pub action_id: String, pub ok: bool, pub thread_id: Option<String>, pub error: Option<String> }`
  - `pub(crate) trait SessionLauncher { async fn launch(&mut self, action: &DriverAction) -> anyhow::Result<String>; }`

- [ ] **Step 1: 写失败测试**

创建 `session_driver.rs`，先只放测试：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_null_answer_yields_no_action() {
        assert!(parse_action(&json!({ "action": null })).is_none());
    }

    #[test]
    fn a_launch_answer_parses_its_fields() {
        let value = json!({
            "action_id": "a1", "kind": "launch",
            "workdir": "/w", "title": "T", "objective": "do it"
        });
        match parse_action(&value).unwrap().kind {
            DriverActionKind::Launch { workdir, title, objective } => {
                assert_eq!(workdir, "/w");
                assert_eq!(title, "T");
                assert_eq!(objective, "do it");
            }
            other => panic!("expected launch, got {other:?}"),
        }
    }

    #[test]
    fn an_append_answer_parses_its_fields() {
        let value = json!({
            "action_id": "a2", "kind": "append",
            "thread_id": "thread-1", "objective": "merge it"
        });
        match parse_action(&value).unwrap().kind {
            DriverActionKind::Append { thread_id, objective } => {
                assert_eq!(thread_id, "thread-1");
                assert_eq!(objective, "merge it");
            }
            other => panic!("expected append, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_kind_is_ignored_rather_than_panicking() {
        assert!(parse_action(&json!({ "action_id": "a3", "kind": "reboot" })).is_none());
    }

    #[test]
    fn a_missing_action_id_is_ignored() {
        assert!(parse_action(&json!({ "kind": "launch", "workdir": "/w" })).is_none());
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-app-server session_driver::tests -- --nocapture`
Expected: FAIL — `cannot find function 'parse_action'`（并把 `mod session_driver;` 加进 `lib.rs` 后仍失败）

- [ ] **Step 3: 写实现**

在 `session_driver.rs`（测试模块之前）加：

```rust
//! 通用会话驱动：把「该起/该继续哪个会话」交给插件决定。
//!
//! 本模块不认识看板、卡片、合并。它只认识两件事：宿主能提供的通用事实
//! （某个会话闲不闲），与宿主能执行的两个通用动作（新建会话 / 追加一轮）。

use serde_json::Value;

/// 宿主能执行的通用动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DriverActionKind {
    /// 在一个 workdir 里新建会话，并投递 `objective` 作为首轮。
    Launch {
        workdir: String,
        title: String,
        objective: String,
    },
    /// 向已有会话追加一轮。
    Append {
        thread_id: String,
        objective: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DriverAction {
    pub action_id: String,
    pub kind: DriverActionKind,
}

/// 解析插件的 `driver/next` 应答。
///
/// `{"action": null}`、缺 `action_id`、或未知 `kind` 一律返回 `None`：调度器
/// 前向兼容——插件日后新增动作类型时，旧宿主只是「这一拍什么都不做」。
pub(crate) fn parse_action(value: &Value) -> Option<DriverAction> {
    let action = value.get("action").unwrap_or(value);
    let action_id = action.get("action_id")?.as_str()?.to_string();
    let kind = action.get("kind")?.as_str()?;
    let kind = match kind {
        "launch" => DriverActionKind::Launch {
            workdir: action.get("workdir")?.as_str()?.to_string(),
            title: action
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            objective: action.get("objective")?.as_str()?.to_string(),
        },
        "append" => DriverActionKind::Append {
            thread_id: action.get("thread_id")?.as_str()?.to_string(),
            objective: action.get("objective")?.as_str()?.to_string(),
        },
        _ => return None,
    };
    Some(DriverAction { action_id, kind })
}

/// 宿主为本插件启动或追加过的会话（宿主自身的通用记账）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DriverThread {
    pub thread_id: String,
}

/// 一次动作的结果，下一拍回传插件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActionResult {
    pub action_id: String,
    pub ok: bool,
    pub thread_id: Option<String>,
    pub error: Option<String>,
}

/// 执行动作的副作用。真实现复用 `start_thread_core` / `prepare_turn_core` 的
/// 无响应帧路径（Task 5 接线）；测试用替身驱动纯逻辑。
pub(crate) trait SessionLauncher {
    async fn launch(&mut self, action: &DriverAction) -> anyhow::Result<String>;
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p yi-agent-app-server session_driver::tests`
Expected: PASS（5 条）

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/session_driver.rs yi-agent-rs/crates/yi-agent-app-server/src/lib.rs
git commit -m "feat(app-server): add the generic session_driver protocol types"
```

---

### Task 3: `driver_step` —— 拉一拍并执行至多一个动作

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/session_driver.rs`
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/session_driver.rs`

**Interfaces:**
- Consumes: Task 2 的 `parse_action` / `DriverAction` / `DriverThread` / `ActionResult` / `SessionLauncher`。
- Produces:
  - `pub(crate) async fn driver_step<L: SessionLauncher>(query: &dyn Fn(&Value) -> Result<Value, String>, threads: &mut Vec<DriverThread>, results: &mut Vec<ActionResult>, launcher: &mut L) -> Option<ActionResult>`
    —— 组装 `{threads, results}`、调 `query`、解析动作、执行、返回本次结果（并清空 `results` 已消费项）。
  - `pub(crate) fn threads_payload(threads: &[DriverThread], idle: &dyn Fn(&str) -> bool) -> Value`

- [ ] **Step 1: 写失败测试**

在 `session_driver.rs` 的 `mod tests` 加：

```rust
    use std::cell::RefCell;

    #[derive(Default)]
    struct FakeLauncher {
        seen: Vec<DriverAction>,
        thread_id: String,
        fail: Option<String>,
    }

    impl SessionLauncher for FakeLauncher {
        async fn launch(&mut self, action: &DriverAction) -> anyhow::Result<String> {
            self.seen.push(action.clone());
            match &self.fail {
                Some(detail) => Err(anyhow::anyhow!(detail.clone())),
                None => Ok(self.thread_id.clone()),
            }
        }
    }

    fn answering(reply: Value) -> impl Fn(&Value) -> Result<Value, String> {
        move |_params: &Value| Ok(reply.clone())
    }

    #[tokio::test]
    async fn a_launch_action_is_executed_and_reported_ok() {
        let reply = json!({
            "action_id": "a1", "kind": "launch",
            "workdir": "/w", "title": "T", "objective": "do it"
        });
        let mut threads = Vec::new();
        let mut results = Vec::new();
        let mut launcher = FakeLauncher { thread_id: "thread-9".into(), ..Default::default() };

        let outcome = driver_step(&answering(reply), &mut threads, &mut results, &mut launcher).await;

        assert_eq!(outcome.unwrap().action_id, "a1");
        assert!(outcome.unwrap().ok);
        assert_eq!(launcher.seen.len(), 1, "the launcher runs exactly once");
    }

    #[tokio::test]
    async fn a_failed_launch_is_reported_with_the_error_and_no_thread() {
        let reply = json!({
            "action_id": "a2", "kind": "launch",
            "workdir": "/w", "title": "T", "objective": "do it"
        });
        let mut threads = Vec::new();
        let mut results = Vec::new();
        let mut launcher = FakeLauncher { fail: Some("worktree gone".into()), ..Default::default() };

        let outcome = driver_step(&answering(reply), &mut threads, &mut results, &mut launcher).await.unwrap();

        assert!(!outcome.ok);
        assert_eq!(outcome.error.as_deref(), Some("worktree gone"));
        assert!(outcome.thread_id.is_none());
    }

    #[tokio::test]
    async fn a_null_answer_performs_no_side_effect() {
        let mut threads = Vec::new();
        let mut results = Vec::new();
        let mut launcher = FakeLauncher::default();

        let outcome = driver_step(&answering(json!({ "action": null })), &mut threads, &mut results, &mut launcher).await;

        assert!(outcome.is_none());
        assert!(launcher.seen.is_empty());
    }

    #[tokio::test]
    async fn pending_results_are_sent_on_the_next_call_and_then_cleared() {
        let reply = json!({ "action": null });
        let mut threads = Vec::new();
        let mut results = vec![ActionResult {
            action_id: "a0".into(), ok: false, thread_id: None, error: Some("boom".into()),
        }];
        let seen: RefCell<Vec<Value>> = RefCell::new(Vec::new());
        let query = |params: &Value| {
            seen.borrow_mut().push(params.clone());
            Ok(reply.clone())
        };
        let mut launcher = FakeLauncher::default();

        driver_step(&query, &mut threads, &mut results, &mut launcher).await;

        let sent = seen.borrow().clone();
        assert_eq!(sent[0]["results"][0]["action_id"], "a0");
        assert_eq!(sent[0]["results"][0]["ok"], false);
        assert!(results.is_empty(), "consumed results are cleared");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-app-server session_driver::tests -- --nocapture`
Expected: FAIL — `cannot find function 'driver_step'`

- [ ] **Step 3: 写实现**

```rust
/// 把宿主持有的会话事实编成 `driver/next` 的 `threads` 参数。
pub(crate) fn threads_payload(threads: &[DriverThread], idle: &dyn Fn(&str) -> bool) -> Value {
    Value::Array(
        threads
            .iter()
            .map(|thread| {
                serde_json::json!({
                    "thread_id": thread.thread_id,
                    "idle": idle(&thread.thread_id),
                })
            })
            .collect(),
    )
}

/// 拉一拍：问插件「下一步做什么」，执行至多一个动作，返回其结果。
///
/// `results` 里是上一拍尚未回报的结果；本拍把它们上送后清空。
pub(crate) async fn driver_step<L: SessionLauncher>(
    query: &dyn Fn(&Value) -> Result<Value, String>,
    threads: &mut Vec<DriverThread>,
    results: &mut Vec<ActionResult>,
    launcher: &mut L,
) -> Option<ActionResult> {
    let results_payload = Value::Array(
        results
            .iter()
            .map(|result| {
                serde_json::json!({
                    "action_id": result.action_id,
                    "ok": result.ok,
                    "thread_id": result.thread_id,
                    "error": result.error,
                })
            })
            .collect(),
    );
    let params = serde_json::json!({
        "threads": threads_payload(threads, &|_| false),
        "results": results_payload,
    });
    results.clear();

    let answer = match query(&params) {
        Ok(answer) => answer,
        // 插件/daemon 不可用：本拍无法推进，留给下一拍重试。
        Err(_) => return None,
    };
    let action = parse_action(&answer)?;

    match launcher.launch(&action).await {
        Ok(thread_id) => {
            if let DriverActionKind::Launch { .. } = &action.kind {
                threads.push(DriverThread { thread_id: thread_id.clone() });
            }
            Some(ActionResult {
                action_id: action.action_id,
                ok: true,
                thread_id: Some(thread_id),
                error: None,
            })
        }
        Err(error) => Some(ActionResult {
            action_id: action.action_id,
            ok: false,
            thread_id: None,
            error: Some(error.to_string()),
        }),
    }
}
```

注意：`driver_step` 里 `threads_payload` 的 `idle` 闭包是占位；真实空闲来自主循环快照
（Task 6 接线时改为传参）。本任务先让 `threads` 参数形状正确即可。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p yi-agent-app-server session_driver::tests`
Expected: PASS（5 + 4 = 9 条）

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/session_driver.rs
git commit -m "feat(app-server): pull one generic driver action per tick"
```

---

### Task 4: 发现「要驱动会话的插件」（通用登记 + 清单字段）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/session_driver.rs`
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/session_driver.rs`

**Interfaces:**
- Consumes: Task 1 的 `SupervisorManifest.session_driver`。
- Produces: `pub(crate) fn driver_plugins(scope: &Path) -> Vec<String>` —— 读
  `<scope>/.yi-agent/supervisors/` 下清单，返回 `session_driver == true` 的插件名（有序）。

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn only_plugins_declaring_session_driver_are_returned() {
        let dir = tempfile::tempdir().unwrap();
        let scope = dir.path();
        let manifests = scope.join(".yi-agent").join("supervisors");
        std::fs::create_dir_all(&manifests).unwrap();
        std::fs::write(
            manifests.join("driver.json"),
            r#"{"name":"driver","command":"/bin/d","args":[],"switch_key":"d",
                "restart_backoff_ms":1000,"restart_backoff_max_ms":30000,
                "stop_when_disabled":false,"session_driver":true}"#,
        )
        .unwrap();
        std::fs::write(
            manifests.join("plain.json"),
            r#"{"name":"plain","command":"/bin/p","args":[],"switch_key":"p",
                "restart_backoff_ms":1000,"restart_backoff_max_ms":30000,
                "stop_when_disabled":false}"#,
        )
        .unwrap();

        assert_eq!(driver_plugins(scope), vec!["driver".to_string()]);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-app-server session_driver::tests::only_plugins -- --nocapture`
Expected: FAIL — `cannot find function 'driver_plugins'`

- [ ] **Step 3: 写实现**

```rust
use std::path::Path;

/// 声明了 `session_driver` 的插件名（按清单文件名排序，稳定）。
///
/// 读的是**通用**清单目录；宿主不认识任何具体插件。
pub(crate) fn driver_plugins(scope: &Path) -> Vec<String> {
    let dir = scope.join(".yi-agent").join("supervisors");
    let mut names: Vec<String> = yi_agent_supervisors::manifest::load_manifests(&dir)
        .unwrap_or_default()
        .into_iter()
        .filter(|manifest| manifest.session_driver)
        .map(|manifest| manifest.name)
        .collect();
    names.sort();
    names
}
```

（`load_manifests` 的返回类型若与上不同，按其真实签名调整；调用点见 `server.rs:1213`。）

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p yi-agent-app-server session_driver::tests`
Expected: PASS（10 条）

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/session_driver.rs
git commit -m "feat(app-server): discover plugins that drive sessions"
```

---

### Task 5: 宿主真实 launcher —— 支持 `append` 到已有会话

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`ServeLauncher` → `SessionLauncher`）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`mod tests`）

**Interfaces:**
- Consumes: Task 2 的 `DriverAction`/`DriverActionKind`/`SessionLauncher`。
- Produces: `impl SessionLauncher for ServeSessionLauncher` —— `launch` 复用现有
  `launch_inner` 的无响应帧路径；`append` 用 `prepare_turn_core` + 与首轮同样的
  `update_status(Running)` / `emit_item(opening_user_item)` / `prompt_tx.send` 段。

- [ ] **Step 1: 写失败测试**

在 `server.rs` 的 `mod tests` 加（沿用既有 `FakeDaemon` + 真 socket 手法）：

```rust
    /// 一次 append 动作必须复用已有 thread，不新建。
    #[tokio::test]
    async fn an_append_action_reuses_the_existing_thread() {
        // 起一个真 serve（沿用既有 test harness），先 launch 一次拿到 thread_id，
        // 再让 driver 返回 {"kind":"append","thread_id":<tid>}，断言：
        //   - 没有新的 thread 出现在 thread/list；
        //   - 原 thread 的 turn 计数 +1。
        // 具体 harness 代码照抄本文件既有 `run_once` 集成测试的搭台方式
        // （`FakeDaemon::bind` + `serve_scoped_core`），只把插件应答从
        // `board.next_launch` 换成 `driver/next` 返回 append。
        todo!("按上述搭台实现；断言 thread 总数不变且原 thread 收到新一轮");
    }
```

> 注：本步的测试骨架依赖 Task 6 的接线（driver 在主循环里跑）。若在 Task 6 之前，
> 可先对 `ServeSessionLauncher::launch` 直接构造 `DriverAction` 单测（不经主循环）：
> 起 thread → 用 `append` 动作调 `launch` → 断言 `threads` 里 thread 数不变、
> `active_turn_id` 被占。两种写法都接受，但必须有**至少一条**证明「append 不新建会话」。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-app-server append_action -- --nocapture`
Expected: FAIL（未实现）

- [ ] **Step 3: 抽出公共段并实现**

把 `launch_inner` 里「`prepare_turn_core` → `update_status(Running)` → `emit_item(opening_user_item)` →
`prompt_tx.send`」这段抽成：

```rust
    /// 准备并投递一轮，**不发任何 RPC 响应帧**。
    ///
    /// 首轮（新建会话）与追问轮（已有会话）共用：两者只差 thread 是否已存在。
    async fn prepare_and_deliver(
        &mut self,
        thread_id: &str,
        objective: &str,
    ) -> anyhow::Result<String> { /* 抽出的公共段 */ }
```

`launch(action)`：
- `DriverActionKind::Launch { workdir, title, objective }` → 现有 `launch_inner` 语义
  （起 thread、设 meta/title、`prepare_and_deliver`）。
- `DriverActionKind::Append { thread_id, objective }` → 直接 `prepare_and_deliver`；
  thread 不存在 → `Err("unknown thread: {thread_id}")`（回报 `ok:false`，由插件决定重试）。

`launch_inner` 的 meta 落盘字段名去掉 `card_id`/`board_project`：
- `card_id` → 删（宿主不认识卡片）。
- `board_project` → 通用 `driver_project`（本插件作用的项目根）。
- `thread_store` 的 meta 结构若为此需要改，则一并改（`thread_store.rs`），并保留
  旧字段的**兼容读取**（旧会话仍能被读出）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p yi-agent-app-server`
Expected: PASS（含既有 `launch` 路径零回归）

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs
git commit -m "feat(app-server): support appending a generic turn to an existing session"
```

---

### Task 6: 主循环接线 —— 用 `session_driver` 替换卡片调度器

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`card_scheduler_tick` → `driver_tick`）
- Delete: `yi-agent-rs/crates/yi-agent-app-server/src/card_scheduler.rs`、`lib.rs` 的声明
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`

**Interfaces:**
- Consumes: Task 3 的 `driver_step`、Task 4 的 `driver_plugins`、Task 5 的 `SessionLauncher` 实现。
- Produces: `async fn driver_tick<F>(…)` —— 遍历 `yi_agent_store::resident::list` 的项目，
  对每个项目的每个 `driver_plugins(project)`，构造查询闭包（走通用 `plugin_query`）并
  `driver_step` 一拍。

- [ ] **Step 1: 写失败测试**

```rust
    /// 一轮 tick：插件回 append → 宿主对已有 thread 投递一轮，且**不调用**任何
    /// `board.*` 方法名。
    #[tokio::test]
    async fn a_tick_drives_the_plugin_through_driver_next_only() {
        // 搭台：FakeDaemon 记录 method；对 "driver/next" 回一个 append 动作。
        // 断言：记录到的 method 全是 "driver/next"，没有 "board.*"。
        todo!("按上述断言实现；关键断言是 `calls` 里不含任何以 board. 开头的方法");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-app-server a_tick_drives -- --nocapture`
Expected: FAIL

- [ ] **Step 3: 实现 `driver_tick` 并删除卡片调度器**

- 新 `driver_tick`：项目来源 `yi_agent_store::resident::list(resident_dir)`；每项目里
  `for plugin in driver_plugins(project)` → `driver_step(&query, threads, results, launcher)`，
  其中 `query` 调 `plugin_query(project, board_dir?, "driver/next", plugin, params)` 的**通用**版本
  （去掉 `board_dir` 参数与看板登记表门控，改走 `plugin_query_self` 同型路径）。
- 会话空闲快照：沿用 `card_scheduler_tick` 里 `snapshot` 的手法（`ThreadStatus::Idle`
  且 `active_turn_id.is_none()`），作为 `threads_payload` 的 `idle` 来源。
- 删除 `card_scheduler.rs`、`lib.rs` 的 `pub(crate) mod card_scheduler;`、
  `server.rs` 的 `use crate::card_scheduler::…`、`recover_orphan_cards`（交给插件自持）。
- 调用点 `card_scheduler_tick(...)` → `driver_tick(...)`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p yi-agent-app-server`
Expected: PASS；且 `grep -rn "card_scheduler" yi-agent-rs/` 无命中

- [ ] **Step 5: 提交**

```bash
git add -A yi-agent-rs/crates/yi-agent-app-server/
git commit -m "refactor(app-server): drive sessions generically, drop the card scheduler"
```

---

### Task 7: 插件侧 `driver/next` 决策入口

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/dispatch.rs`
- Test: 两个文件各自的 `mod tests`

**Interfaces:**
- Consumes: Task 1 起的宿主行为（请求/应答形状，见 spec §4.2/§4.3）。
- Produces:
  - `pub enum DriverAction { Launch { action_id, card_id, workdir, title, objective }, Append { action_id, card_id, thread_id, objective } }`
  - `pub fn driver_step(&self, threads: &[(String, bool)], results: &[DriverResult]) -> Result<Option<DriverAction>, String>`
  - `dispatch` 方法 `"driver/next"`

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn driver_step_launches_the_head_of_the_queue_when_a_slot_is_free() {
        // 队列里两张 queued 卡；driver_step 应只交出 order 最小的那张 launch。
    }

    #[test]
    fn driver_step_marks_a_running_card_awaiting_merge_when_its_session_goes_idle() {
        // 卡 running + threads 里 thread_id 的 idle=true → 状态变 awaiting_merge。
    }

    #[test]
    fn driver_step_emits_an_append_for_a_card_awaiting_merge_once_the_gate_is_held() {
        // 卡 awaiting_merge + 会话 idle → 返回 Append，objective 含 source 与 base；
        // 且卡转 merging。
    }

    #[test]
    fn driver_step_does_not_append_twice_while_a_merge_is_running() {
        // 已有卡 Merging（名额被占）→ 第二张 awaiting_merge 卡不出 Append。
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p superpowers-kanban-runner driver_step -- --nocapture`
Expected: FAIL

- [ ] **Step 3: 实现**

- `driver_step` 是本插件**唯一**的推进入口：合并原先散在宿主 `plan()` 的规则
  （队首、名额、`running+idle → awaiting_merge`、`awaiting_merge+idle → 合并轮`）。
- 复用既有内部函数 `next_launch` / `mark_running` / `mark_terminal` / `release` /
  `merge_request` / `merge_finish`（它们**保留**，只是不再由宿主直接调）。
- objective：`tick.rs` 拆出 `engagement_objective(card)`（原 `objective_for`，保留
  `Never merge yourself`）与 `merge_objective(source, base, workdir)`。
- `dispatch.rs` 加 `"driver/next"` 分支，参数 `{threads: [{thread_id, idle}], results: [...]}`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p superpowers-kanban-runner`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/
git commit -m "feat(kanban): drive the queue from driver/next instead of the host"
```

---

### Task 8: 退役 `merge_next()`（裸 git 自动合并）

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs`
- Test: `service.rs` 的 `mod tests`

**Interfaces:**
- Consumes: Task 7 的 `driver_step`（合并轮已由它驱动）。
- Produces: `merge_next()` / `MergeClaim` **删除**；主循环不再调它。

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn the_daemon_loop_no_longer_claims_a_merge_to_run_git_itself() {
        // 以源码断言：service.rs 不含 "fn merge_next"，main.rs 不含 "merge_next("。
        // （该能力的替代路径是 driver_step → append → 会话内合并，已在 Task 7 覆盖。）
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p superpowers-kanban-runner -- --nocapture`
Expected: FAIL（`merge_next` 仍存在）

- [ ] **Step 3: 删除**

- `service.rs`：删 `merge_next`、`MergeClaim`，及其单测（`merge_next` 相关）。
- `main.rs`：删主循环里的 `match service.merge_next()` 段。
- 保留：`merge_request` / `merge_finish` / `merge.lock` / git 复核 / `reconcile_merged`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p superpowers-kanban-runner`
Expected: PASS；`grep -rn "merge_next" plugins/` 无命中

- [ ] **Step 5: 提交**

```bash
git add plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/
git commit -m "refactor(kanban): retire the plugin's bare-git auto merge"
```

---

### Task 9: 清单模板声明 `session_driver` + 端到端验收

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-boards/src/scaffold.rs`（清单模板）
- Test: `scaffold.rs` 的 `mod tests`；`docs/bug-list.md` 收口

**Interfaces:**
- Consumes: Task 1 的字段、Task 4 的发现逻辑。
- Produces: 生成的 `superpowers-kanban.json` 含 `"session_driver": true`。

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn the_generated_manifest_declares_session_driver() {
        // 断言 install_manifest 写出的 JSON 里 session_driver == true。
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-boards the_generated_manifest -- --nocapture`
Expected: FAIL

- [ ] **Step 3: 改模板 + 收口 bug-list**

- `MANIFEST_TEMPLATE` 加 `"session_driver": true,`。
- `docs/bug-list.md`：两条条目改为 `[x]`，在条目里补一行「收口：由 G1（`2026-10-17-session-driver-decoupling-design.md`）落地」。
- 手工端到端（验收 11–14）：投两张实现卡 + 两张合并卡，观察严格串行、合并会话可见、
  base 有未跟踪文件时仍合并成功、卸载插件后宿主任正常。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p yi-agent-boards && cargo test -p superpowers-kanban-runner && cargo test -p yi-agent-app-server`
Expected: 全绿

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-boards/src/scaffold.rs docs/bug-list.md
git commit -m "feat(kanban): declare session_driver in the manifest; close the merge bugs"
```

---

## 自检

- **Spec 覆盖**：§4.2/4.3 协议 → Task 2/3；§4.4 执行语义 → Task 5；§4.5 发现 → Task 4；
  §4.6 插件队列与合并 → Task 7/8；§5 宿主改动 → Task 6；§6 插件改动 → Task 7/8/9；
  §8 验收 1–5 → Task 3；6 → Task 6；7–10 → Task 7/8；11–14 → Task 9。
- **类型一致**：`DriverAction`/`DriverActionKind`（宿主）与插件侧 `DriverAction` 是**两侧各自的**
  类型，跨进程只以 JSON 对齐（字段名一致：`action_id`/`kind`/`workdir`/`title`/`objective`/
  `thread_id`）。`ActionResult` 字段名与 spec §4.2 的 `results[]` 一致。
- **占位符**：Task 5/6 的集成测试用 `todo!()` 承载搭台骨架，但**给出了明确断言**与
  「抄既有 harness」的定位；实现者需按既有 `FakeDaemon` 手法补全。这不是「写测试」的空话，
  而是指向文件内既有代码的可执行指引。
- **顺序依赖**：Task 1 → 4；Task 2 → 3 → 6；Task 5 → 6；Task 7 → 8；Task 9 最后。
