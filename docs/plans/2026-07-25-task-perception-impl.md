# 任务执行感知改进 Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 在 ratatui TUI 模式下,实时可视化 LLM token 用量(prefill/decode)与 bash 命令执行耗时,并提供 Ctrl+P 全屏弹窗查看 bash stdout/stderr 实时增量与手动 kill。

**Architecture:** 扩展 Tool trait 增加流式 `call_stream` 方法;BashTool 改为 spawn 进程 + tokio::select 驱动 stdout/stderr 增量推送 + 无输出 watchdog 超时;AgentEvent 新增 ToolOutputDelta/ToolExit 转发流式事件;TUI 新增 RunningTaskRegistry 追踪任务状态、StatusBar 底部状态栏(30hz tick + token 线性插值 + 颜色渐变 spinner)、BashPopup 全屏弹窗(列表态+详情态,Ctrl+P 触发)。

**Tech Stack:** Rust 2024, ratatui 0.29, tokio (async), crossterm 0.28

**Worktree:** `.worktrees/task-perception` (branch `feature/task-perception`)

**Baseline:** 593 tests passing, 0 failures

---

## Task 1: 扩展 Tool trait — 新增 ToolEvent 与 call_stream

**Files:**
- Modify: `crates/yi-agent-core/src/tool.rs`
- Modify: `crates/yi-agent-core/src/lib.rs` (re-export ToolEvent)

### Step 1: 写失败测试

Create `crates/yi-agent-core/tests/tool_stream.rs`:

```rust
use yi_agent_core::{Tool, ToolEvent, ToolResult, OutputStream};
use serde_json::json;
use futures::StreamExt;

struct EchoTool;

#[async_trait::async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str { "echo" }
    fn schema(&self) -> serde_json::Value { json!({"type": "object", "properties": {}}) }
    fn description(&self) -> &str { "echo" }
    async fn call(&self, _args: serde_json::Value) -> ToolResult {
        ToolResult::text("done")
    }
}

#[tokio::test]
async fn test_default_call_stream_no_events() {
    let tool = EchoTool;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ToolEvent>(16);
    let result = tool.call_stream(json!({}), tx).await;
    assert_eq!(result.content.len(), 1);
    // default impl sends no events
    assert!(rx.try_recv().is_err());
}

#[test]
fn test_tool_event_variants() {
    let e = ToolEvent::OutputDelta { stream: OutputStream::Stdout, text: "hi".into() };
    assert!(matches!(e, ToolEvent::OutputDelta { stream: OutputStream::Stdout, .. }));
    let e = ToolEvent::Exit { code: Some(0) };
    assert!(matches!(e, ToolEvent::Exit { code: Some(0) }));
    let e = ToolEvent::Timeout;
    assert!(matches!(e, ToolEvent::Timeout));
    let e = ToolEvent::Truncated { stream: OutputStream::Stderr, skipped_bytes: 100 };
    assert!(matches!(e, ToolEvent::Truncated { skipped_bytes: 100, .. }));
}
```

### Step 2: 运行测试验证失败

Run: `cargo test -p yi-agent-core --test tool_stream`
Expected: FAIL — `ToolEvent` / `OutputStream` not found

### Step 3: 实现

In `crates/yi-agent-core/src/tool.rs`, add at top (after `use` statements, before `ToolResult`):

```rust
/// Output stream type for tool streaming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// Streaming events emitted by `Tool::call_stream`.
#[derive(Debug, Clone)]
pub enum ToolEvent {
    /// Incremental output from the tool process.
    OutputDelta { stream: OutputStream, text: String },
    /// Process exited with optional code (None = killed).
    Exit { code: Option<i32> },
    /// Watchdog killed the process (no output within expected window).
    Timeout,
    /// A stream was truncated at the output cap.
    Truncated { stream: OutputStream, skipped_bytes: usize },
}
```

Modify the `Tool` trait (lines 75-85) to add `call_stream`:

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn schema(&self) -> Value;
    fn description(&self) -> &str;
    async fn call(&self, args: Value) -> ToolResult;
    fn metadata(&self) -> ToolMetadata { ToolMetadata::default() }

    /// Streaming variant. Default implementation just calls `call` with no stream events.
    /// Tools that produce incremental output should override this.
    async fn call_stream(
        &self,
        args: Value,
        _tx: tokio::sync::mpsc::Sender<ToolEvent>,
    ) -> ToolResult {
        self.call(args).await
    }
}
```

In `crates/yi-agent-core/src/lib.rs`, add to re-exports:

```rust
pub use tool::{OutputStream, Tool, ToolEvent, ToolMetadata, ToolRegistry, ToolResult, ToolSchema, ToolSource};
```

### Step 4: 运行测试验证通过

Run: `cargo test -p yi-agent-core --test tool_stream`
Expected: PASS

### Step 5: 验证不破坏现有测试

Run: `cargo test --workspace`
Expected: 593 tests pass (same as baseline)

### Step 6: Commit

```bash
git add crates/yi-agent-core/src/tool.rs crates/yi-agent-core/src/lib.rs crates/yi-agent-core/tests/tool_stream.rs
git commit -m "feat(core): add ToolEvent and call_stream to Tool trait"
```

---

## Task 2: BashTool 流式改造 — call_stream + expected_timeout_sec + watchdog

**Files:**
- Modify: `crates/yi-agent-tools/src/shell/bash.rs`

### Step 1: 写失败测试

Create `crates/yi-agent-tools/tests/bash_stream.rs`:

```rust
use yi_agent_core::{OutputStream, Tool, ToolEvent};
use yi_agent_tools::BashTool;
use yi_agent_tools::context::ToolsContext;
use std::sync::Arc;
use std::time::Duration;
use futures::StreamExt;

fn make_tool() -> BashTool {
    let cwd = std::env::temp_dir();
    BashTool::new(Arc::new(ToolsContext::new(cwd)))
}

#[tokio::test]
async fn test_bash_stream_emits_stdout_delta() {
    let tool = make_tool();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ToolEvent>(64);
    let result = tool.call_stream(serde_json::json!({"command": "echo hello"}), tx).await;
    assert!(!result.is_error);
    // Should have received at least one OutputDelta with "hello"
    let mut got_hello = false;
    while let Ok(ev) = rx.try_recv() {
        match ev {
            ToolEvent::OutputDelta { stream: OutputStream::Stdout, text } if text.contains("hello") => {
                got_hello = true;
            }
            _ => {}
        }
    }
    assert!(got_hello, "expected stdout delta containing 'hello'");
}

#[tokio::test]
async fn test_bash_stream_emits_exit_code() {
    let tool = make_tool();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ToolEvent>(64);
    let _ = tool.call_stream(serde_json::json!({"command": "true"}), tx).await;
    let mut exit_code = None;
    while let Ok(ev) = rx.try_recv() {
        if let ToolEvent::Exit { code } = ev { exit_code = code; }
    }
    assert_eq!(exit_code, Some(0));
}

#[tokio::test]
async fn test_bash_stream_expected_timeout_kills_on_no_output() {
    let tool = make_tool();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ToolEvent>(64);
    // sleep 10s but expected_timeout=1, so 1.5s no-output watchdog should fire
    let start = std::time::Instant::now();
    let result = tool.call_stream(
        serde_json::json!({"command": "sleep 10", "expected_timeout_sec": 1}),
        tx,
    ).await;
    let elapsed = start.elapsed();
    assert!(elapsed < Duration::from_secs(5), "watchdog should kill within ~1.5s, took {elapsed:?}");
    assert!(result.is_error);
    let mut got_timeout = false;
    while let Ok(ev) = rx.try_recv() {
        if matches!(ev, ToolEvent::Timeout) { got_timeout = true; }
    }
    assert!(got_timeout, "expected Timeout event");
}

#[tokio::test]
async fn test_bash_stream_expected_timeout_allows_long_output() {
    let tool = make_tool();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ToolEvent>(64);
    // Produces output every 200ms for 3s. expected=1, so watchdog would fire at 1.5s
    // but output resets it, so should complete successfully.
    let cmd = "for i in $(seq 1 15); do echo $i; sleep 0.2; done";
    let result = tool.call_stream(
        serde_json::json!({"command": cmd, "expected_timeout_sec": 1}),
        tx,
    ).await;
    assert!(!result.is_error, "should complete despite exceeding expected_timeout, because output resets watchdog");
}
```

### Step 2: 运行测试验证失败

Run: `cargo test -p yi-agent-tools --test bash_stream`
Expected: FAIL — `expected_timeout_sec` not in schema, call_stream not overridden

### Step 3: 实现

In `crates/yi-agent-tools/src/shell/bash.rs`:

**修改 BashArgs (lines 28-33):**

```rust
#[derive(Deserialize)]
struct BashArgs {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
    /// LLM-declared expected runtime. Default 120s. If no output for
    /// expected_timeout_sec * 1.5, process is killed.
    #[serde(default)]
    expected_timeout_sec: Option<u32>,
}
```

**修改 input_schema (lines 45-53):** 加入 `expected_timeout_sec`:

```rust
fn schema(&self) -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": { "type": "string", "description": "The bash command to execute" },
            "timeout": { "type": "integer", "description": "Optional timeout in seconds (legacy hard timeout)", "default": 120 },
            "expected_timeout_sec": {
                "type": "integer",
                "description": "Expected runtime in seconds. If no stdout/stderr output for expected_timeout_sec * 1.5, the process is killed as stuck. Default 120.",
                "default": 120
            }
        },
        "required": ["command"]
    })
}
```

**重写 call() 逻辑为新 call_stream() + 保留旧 call()**: 保留 `call` 调用 `call_stream` 并丢弃事件:

```rust
async fn call(&self, args: Value) -> ToolResult {
    let (tx, _rx) = tokio::sync::mpsc::channel::<ToolEvent>(1);
    self.call_stream(args, tx).await
}
```

**实现 call_stream()**: 在 `impl Tool for BashTool` 块内新增。参考原 `call()` lines 56-122 的流程,但改为:

```rust
async fn call_stream(
    &self,
    args: Value,
    tx: tokio::sync::mpsc::Sender<ToolEvent>,
) -> ToolResult {
    let BashArgs { command, timeout, expected_timeout_sec } = match serde_json::from_value::<BashArgs>(args) {
        Ok(a) => a,
        Err(e) => return ToolResult::error(format!("invalid args: {e}")),
    };
    if let Some(reason) = is_blocked(&command) {
        return ToolResult::error(format!("blocked: {reason}"));
    }
    let cwd = self.ctx.cwd();
    let hard_timeout = Duration::from_secs(timeout.unwrap_or(DEFAULT_TIMEOUT));
    let expected = expected_timeout_sec.unwrap_or(DEFAULT_TIMEOUT as u32);
    let idle_limit = Duration::from_secs((expected as u64) * 3 / 2); // expected * 1.5

    let mut child = match Command::new("sh").arg("-c").arg(&command)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return ToolResult::error(format!("spawn failed: {e}")),
    };
    if let Some(new_cwd) = parse_cd_target(&command, &cwd) {
        self.ctx.set_cwd(new_cwd);
    }
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();

    let mut stdout_buf = Vec::<u8>::new();
    let mut stderr_buf = Vec::<u8>::new();
    let mut stdout_truncated = false;
    let mut stderr_truncated = false;
    let mut exit_code: Option<i32> = None;
    let mut timed_out = false;

    let mut stdout_chunk = vec![0u8; 4096];
    let mut stderr_chunk = vec![0u8; 4096];
    let mut idle = tokio::time::interval(idle_limit);
    idle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let _ = idle.tick(); // first tick is immediate, discard
    let hard_deadline = tokio::time::Instant::now() + hard_timeout;

    loop {
        tokio::select! {
            biased;
            n = stdout.read(&mut stdout_chunk) => {
                match n {
                    Ok(0) => break,
                    Ok(n) => {
                        let data = &stdout_chunk[..n];
                        if !stdout_truncated {
                            if stdout_buf.len() + data.len() > MAX_OUTPUT_BYTES {
                let kept = MAX_OUTPUT_BYTES.saturating_sub(stdout_buf.len());
                stdout_buf.extend_from_slice(&data[..kept]);
                let skipped = data.len() - kept;
                stdout_truncated = true;
                let _ = tx.send(ToolEvent::Truncated { stream: OutputStream::Stdout, skipped_bytes: skipped }).await;
            } else {
                stdout_buf.extend_from_slice(data);
            }
            let _ = tx.send(ToolEvent::OutputDelta { stream: OutputStream::Stdout, text: String::from_utf8_lossy(data).into_owned() }).await;
                        }
                        let _ = idle.reset(); // reset watchdog on output
                    }
                    Err(_) => break,
                }
            }
            n = stderr.read(&mut stderr_chunk) => {
                match n {
                    Ok(0) => break,
                    Ok(n) => {
                        let data = &stderr_chunk[..n];
                        if !stderr_truncated {
            if stderr_buf.len() + data.len() > MAX_OUTPUT_BYTES {
                let kept = MAX_OUTPUT_BYTES.saturating_sub(stderr_buf.len());
                stderr_buf.extend_from_slice(&data[..kept]);
                let skipped = data.len() - kept;
                stderr_truncated = true;
                let _ = tx.send(ToolEvent::Truncated { stream: OutputStream::Stderr, skipped_bytes: skipped }).await;
            } else {
                stderr_buf.extend_from_slice(data);
            }
            let _ = tx.send(ToolEvent::OutputDelta { stream: OutputStream::Stderr, text: String::from_utf8_lossy(data).into_owned() }).await;
                        }
                        let _ = idle.reset();
                    }
                    Err(_) => break,
                }
            }
            _ = idle.tick() => {
                // no output for idle_limit → stuck
                let _ = child.kill().await;
                timed_out = true;
                let _ = tx.send(ToolEvent::Timeout).await;
                break;
            }
            _ = tokio::time::sleep_until(hard_deadline) => {
                let _ = child.kill().await;
                timed_out = true;
                let _ = tx.send(ToolEvent::Timeout).await;
                break;
            }
            status = child.wait() => {
                match status {
                    Ok(s) => { exit_code = s.code(); break; }
                    Err(_) => break,
                }
            }
        }
    }
    // ensure child reaped
    let _ = child.wait().await;
    let _ = tx.send(ToolEvent::Exit { code: exit_code }).await;

    if timed_out {
        return ToolResult::error(format!("command timed out after {}s (no output for {}s)", expected, idle_limit.as_secs()));
    }
    let stdout_str = String::from_utf8_lossy(&stdout_buf);
    let stderr_str = String::from_utf8_lossy(&stderr_buf);
    let text = format!("exit: {}\nstdout:\n{}\nstderr:\n{}", exit_code.map(|c| c.to_string()).unwrap_or_else(|| "killed".into()), stdout_str, stderr_str);
    ToolResult::text(text)
}
```

> **注意:** `tokio::time::Interval::reset()` 需确认 API 存在。若不存在,改用 `tokio::time::Instant` + `tokio::time::sleep_until` 手动管理 idle deadline:
> ```rust
> let mut next_idle_deadline = tokio::time::Instant::now() + idle_limit;
> // in loop:
> _ = tokio::time::sleep_until(next_idle_deadline) => { /* kill */ }
> // on each output: next_idle_deadline = tokio::time::Instant::now() + idle_limit;
> ```
> 实现时以 API 可用版本为准。

### Step 4: 运行测试验证通过

Run: `cargo test -p yi-agent-tools --test bash_stream`
Expected: PASS

### Step 5: 验证现有 bash 测试仍通过

Run: `cargo test -p yi-agent-tools`
Expected: PASS (现有 bash 测试通过 — `call` 仍可用,内部调 `call_stream`)

### Step 6: Commit

```bash
git add crates/yi-agent-tools/src/shell/bash.rs crates/yi-agent-tools/tests/bash_stream.rs
git commit -m "feat(tools): bash tool streams stdout/stderr with expected_timeout watchdog"
```

---

## Task 3: Agent loop 转发流式事件

**Files:**
- Modify: `crates/yi-agent-core/src/agent.rs`

### Step 1: 写失败测试

Create `crates/yi-agent-core/tests/agent_tool_stream.rs`:

```rust
use yi_agent_core::{Agent, AgentEvent, OutputStream, Tool, ToolEvent, ToolRegistry, ToolResult, ToolMetadata};
use yi_agent_core::provider::{Provider, ProviderEvent, ProviderRequest, ProviderResponse, TokenUsage};
use yi_agent_core::session::Session;
use std::sync::Arc;
use futures::StreamExt;
use serde_json::json;
use async_trait::async_trait;

struct DummyProvider;

#[async_trait]
impl Provider for DummyProvider {
    async fn call_stream(&self, _req: ProviderRequest) -> Result<futures::stream::BoxStream<'static, ProviderEvent>, yi_agent_core::provider::ProviderError> {
        // emit one text then a tool_use, then stop
        let events = vec![
            ProviderEvent::ToolUseStart { id: "t1".into(), name: "stream_tool".into() },
            ProviderEvent::ToolUseDelta { id: "t1".into(), partial_json: r#"{"command":"echo hi"}"#.into() },
            ProviderEvent::ToolUseEnd { id: "t1".into() },
            ProviderEvent::Stop { reason: yi_agent_core::provider::StopReason::ToolUse },
            ProviderEvent::Usage(TokenUsage { input_tokens: 10, output_tokens: 5, ..Default::default() }),
        ];
        Ok(futures::stream::iter(events).boxed())
    }
}

struct StreamingTool;

#[async_trait]
impl Tool for StreamingTool {
    fn name(&self) -> &str { "stream_tool" }
    fn schema(&self) -> serde_json::Value { json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}) }
    fn description(&self) -> &str { "stream" }
    async fn call(&self, _args: serde_json::Value) -> ToolResult { ToolResult::text("ok") }
    async fn call_stream(&self, _args: serde_json::Value, tx: tokio::sync::mpsc::Sender<ToolEvent>) -> ToolResult {
        let _ = tx.send(ToolEvent::OutputDelta { stream: OutputStream::Stdout, text: "hi".into() }).await;
        let _ = tx.send(ToolEvent::Exit { code: Some(0) }).await;
        ToolResult::text("ok")
    }
}

#[tokio::test]
async fn test_agent_forwards_tool_output_delta() {
    let mut registry = ToolRegistry::default();
    registry.register(Arc::new(StreamingTool));
    let session = Arc::new(tokio::sync::Mutex::new(Session::new()));
    let agent = Agent::new(
        Arc::new(DummyProvider),
        Arc::new(registry),
        session,
        yi_agent_core::agent::AgentConfig::default(),
        tokio_util::sync::CancellationToken::new(),
    );
    let mut stream = agent.run("test".into()).await.unwrap();
    let mut saw_output_delta = false;
    let mut saw_exit = false;
    while let Some(ev) = stream.next().await {
        match ev {
            AgentEvent::ToolOutputDelta { text, .. } if text.contains("hi") => { saw_output_delta = true; }
            AgentEvent::ToolExit { code: Some(0), .. } => { saw_exit = true; }
            _ => {}
        }
    }
    assert!(saw_output_delta, "expected ToolOutputDelta event");
    assert!(saw_exit, "expected ToolExit event");
}
```

> 注:`Agent::new` 签名以代码现状为准,如需调整参数顺序或 PermissionChecker,按实际签名修改测试。

### Step 2: 运行测试验证失败

Run: `cargo test -p yi-agent-core --test agent_tool_stream`
Expected: FAIL — `AgentEvent::ToolOutputDelta` / `ToolExit` 不存在

### Step 3: 实现

In `crates/yi-agent-core/src/agent.rs`:

**扩展 AgentEvent (lines 111-141):** 加入:

```rust
pub enum AgentEvent {
    // ... existing ...
    ToolOutputDelta {
        id: String,
        stream: crate::tool::OutputStream,
        text: String,
    },
    ToolExit {
        id: String,
        code: Option<i32>,
    },
    ToolTimeout {
        id: String,
    },
}
```

**修改 run_loop 中 tool 执行段 (lines 469-523):** 当前用 `futures::future::join_all` 并发执行。改为每个 tool call 创建 `mpsc::channel(64)`,spawn 一个转发 task 把 `ToolEvent` wrap 成 `AgentEvent::ToolOutputDelta/ToolExit/ToolTimeout` 发到 agent tx,然后调 `call_stream`:

```rust
let results: Vec<(String, ToolResult)> = futures::future::join_all(
    checked_uses.into_iter().map(|(id, name, input)| {
        let tool = self.tools.get(&name).cloned();
        let tx = tx.clone();
        async move {
            let tool = match tool {
                Some(t) => t,
                None => {
                    let _ = tx.send(AgentEvent::ToolResult {
                        id: id.clone(),
                        result: ToolResult::error(format!("tool '{name}' not found")),
                    }).await;
                    return (id, ToolResult::error("tool not found"));
                }
            };
            let (event_tx, mut event_rx) = mpsc::channel::<ToolEvent>(64);
            let fwd_tx = tx.clone();
            let fwd_id = id.clone();
            tokio::spawn(async move {
                while let Some(ev) = event_rx.recv().await {
                    let agent_ev = match ev {
                        ToolEvent::OutputDelta { stream, text } => AgentEvent::ToolOutputDelta { id: fwd_id.clone(), stream, text },
                        ToolEvent::Exit { code } => AgentEvent::ToolExit { id: fwd_id.clone(), code },
                        ToolEvent::Timeout => AgentEvent::ToolTimeout { id: fwd_id.clone() },
                        ToolEvent::Truncated { .. } => continue, // 可选: 转发或不转发
                    };
                    let _ = fwd_tx.send(agent_ev).await;
                }
            });
            let _ = tx.send(AgentEvent::ToolCall { id: id.clone(), name: name.clone(), input: input.clone() }).await;
            let result = tool.call_stream(input, event_tx).await;
            let _ = tx.send(AgentEvent::ToolResult { id: id.clone(), result: result.clone() }).await;
            (id, result)
        }
    }),
).await;
```

### Step 4: 运行测试验证通过

Run: `cargo test -p yi-agent-core --test agent_tool_stream`
Expected: PASS

### Step 5: 验证全量测试

Run: `cargo test --workspace`
Expected: PASS (可能有个别测试因 AgentEvent 新变体 match 不全而 warning,修正之)

### Step 6: Commit

```bash
git add crates/yi-agent-core/src/agent.rs crates/yi-agent-core/tests/agent_tool_stream.rs
git commit -m "feat(core): forward tool stream events as AgentEvent"
```

---

## Task 4: UsageStats 扩展 prefill/decode 分项

**Files:**
- Modify: `crates/yi-agent/src/app.rs`

### Step 1: 写失败测试

修改或新增 `crates/yi-agent/src/app.rs` 内 `UsageStats` 的测试模块:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_usage_stats_current_call() {
        let mut s = UsageStats::default();
        s.begin_call();
        s.add_current(TokenUsage { input_tokens: 100, output_tokens: 50, ..Default::default() });
        assert_eq!(s.current_input_tokens(), 100);
        assert_eq!(s.current_output_tokens(), 50);
        s.end_call();
        // after end, values frozen until next begin_call
        assert_eq!(s.current_input_tokens(), 100);
        s.begin_call();
        // new call resets
        assert_eq!(s.current_input_tokens(), 0);
    }
}
```

### Step 2: 运行测试验证失败

Run: `cargo test -p yi-agent --lib app::tests`
Expected: FAIL — `begin_call` / `add_current` / `current_input_tokens` 不存在

### Step 3: 实现

In `crates/yi-agent/src/app.rs`:

**扩展 UsageStats (lines 17-41):**

```rust
#[derive(Debug, Clone, Default)]
pub struct UsageStats {
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub last_input_tokens: u64,
    // current LLM call (for status bar display)
    current_input_tokens: u64,
    current_output_tokens: u64,
    call_active: bool,
}

impl UsageStats {
    pub fn begin_call(&mut self) {
        self.current_input_tokens = 0;
        self.current_output_tokens = 0;
        self.call_active = true;
    }
    pub fn add_current(&mut self, usage: TokenUsage) {
        if self.call_active {
            self.current_input_tokens = self.current_input_tokens.max(usage.input_tokens as u64);
            self.current_output_tokens = self.current_output_tokens.max(usage.output_tokens as u64);
        }
        self.last_input_tokens = self.last_input_tokens.max(usage.input_tokens as u64);
        self.total_input_tokens += usage.input_tokens as u64;
        self.total_output_tokens += usage.output_tokens as u64;
    }
    pub fn end_call(&mut self) { self.call_active = false; }
    pub fn current_input_tokens(&self) -> u64 { self.current_input_tokens }
    pub fn current_output_tokens(&self) -> u64 { self.current_output_tokens }
    // keep existing methods: add_usage, reset_session, last_context_tokens
}
```

> 注:`add_usage` 内部改为调 `add_current`。`begin_call` 在 `app.rs::run` 中每次新 prompt 进入时调用,`end_call` 在 `AgentEvent::Done/Cancelled/Error` 时调用。

**在 `app.rs::run` 的事件处理中调用 begin/end_call:** 当收到 `AgentEvent::Start` 时 `begin_call`,收到 `Done/Cancelled/Error` 时 `end_call`,收到 `Usage(u)` 时 `add_current(u)`。

### Step 4: 运行测试验证通过

Run: `cargo test -p yi-agent --lib app::tests`
Expected: PASS

### Step 5: Commit

```bash
git add crates/yi-agent/src/app.rs
git commit -m "feat(app): track per-call prefill/decode token counts"
```

---

## Task 5: RunningTaskRegistry

**Files:**
- Create: `crates/yi-agent/src/tui/state.rs`
- Modify: `crates/yi-agent/src/tui/mod.rs`

### Step 1: 写失败测试

Create `crates/yi-agent/src/tui/state.rs` with test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn test_registry_lifecycle() {
        let mut r = RunningTaskRegistry::new();
        r.on_tool_call("t1", "bash", "ls -la", 120);
        assert_eq!(r.running_count(), 1);
        r.on_output_delta("t1", OutputStream::Stdout, "hello\n");
        r.on_output_delta("t1", OutputStream::Stderr, "warn\n");
        let state = r.get("t1").unwrap();
        assert!(state.stdout.contains(b"hello"));
        assert!(state.stderr.contains(b"warn"));
        r.on_exit("t1", Some(0));
        assert_eq!(r.running_count(), 0);
        assert_eq!(r.get("t1").unwrap().status, TaskStatus::Done);
    }

    #[test]
    fn test_registry_truncation() {
        let mut r = RunningTaskRegistry::new();
        r.on_tool_call("t1", "bash", "cat big", 120);
        let big = "x".repeat(100 * 1024);
        r.on_output_delta("t1", OutputStream::Stdout, &big);
        let state = r.get("t1").unwrap();
        assert!(state.stdout.len() <= 64 * 1024 + 1024); // ~64KB cap
    }

    #[test]
    fn test_registry_listing_order() {
        let mut r = RunningTaskRegistry::new();
        r.on_tool_call("a", "bash", "cmd_a", 120);
        std::thread::sleep(std::time::Duration::from_millis(10));
        r.on_tool_call("b", "bash", "cmd_b", 120);
        let list = r.list();
        assert_eq!(list[0].id, "b"); // newest first
        assert_eq!(list[1].id, "a");
    }
}
```

### Step 2: 运行测试验证失败

Run: `cargo test -p yi-agent --lib tui::state`
Expected: FAIL — 模块不存在

### Step 3: 实现

Create `crates/yi-agent/src/tui/state.rs`:

```rust
use yi_agent_core::tool::OutputStream;
use std::time::Instant;

const MAX_STREAM_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus { Running, Done, Failed, Timeout }

#[derive(Debug, Clone)]
pub struct TaskState {
    pub id: String,
    pub tool_name: String,
    pub command: String,
    pub start_time: Instant,
    pub end_time: Option<Instant>,
    pub exit_code: Option<Option<i32>>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: TaskStatus,
    pub expected_timeout_sec: u32,
}

impl TaskState {
    pub fn elapsed(&self) -> std::time::Duration {
        match self.end_time {
            Some(end) => end.duration_since(self.start_time),
            None => self.start_time.elapsed(),
        }
    }
    pub fn exceeds_expected(&self) -> bool {
        self.status == TaskStatus::Running && self.elapsed().as_secs() > self.expected_timeout_sec as u64
    }
}

pub struct RunningTaskRegistry {
    tasks: std::collections::HashMap<String, TaskState>,
    order: Vec<String>, // insertion order for stable listing
}

impl RunningTaskRegistry {
    pub fn new() -> Self { Self { tasks: Default::default(), order: Vec::new() } }

    pub fn on_tool_call(&mut self, id: &str, tool_name: &str, command: &str, expected_timeout_sec: u32) {
        let state = TaskState {
            id: id.into(), tool_name: tool_name.into(), command: command.into(),
            start_time: Instant::now(), end_time: None, exit_code: None,
            stdout: Vec::new(), stderr: Vec::new(),
            status: TaskStatus::Running, expected_timeout_sec,
        };
        self.tasks.insert(id.into(), state);
        self.order.push(id.into());
    }
    pub fn on_output_delta(&mut self, id: &str, stream: OutputStream, text: &str) {
        if let Some(t) = self.tasks.get_mut(id) {
            let buf = match stream { OutputStream::Stdout => &mut t.stdout, OutputStream::Stderr => &mut t.stderr };
            buf.extend_from_slice(text.as_bytes());
            if buf.len() > MAX_STREAM_BYTES {
                let cut = buf.len() - MAX_STREAM_BYTES;
                buf.drain(..cut);
            }
        }
    }
    pub fn on_exit(&mut self, id: &str, code: Option<i32>) {
        if let Some(t) = self.tasks.get_mut(id) {
            t.end_time = Some(Instant::now());
            t.exit_code = Some(code);
            t.status = match code { Some(0) => TaskStatus::Done, Some(_) => TaskStatus::Failed, None => TaskStatus::Timeout };
        }
    }
    pub fn on_timeout(&mut self, id: &str) {
        if let Some(t) = self.tasks.get_mut(id) {
            t.end_time = Some(Instant::now());
            t.exit_code = None;
            t.status = TaskStatus::Timeout;
        }
    }
    pub fn running_count(&self) -> usize { self.tasks.values().filter(|t| t.status == TaskStatus::Running).count() }
    pub fn get(&self, id: &str) -> Option<&TaskState> { self.tasks.get(id) }
    pub fn list(&self) -> Vec<&TaskState> {
        // newest first
        self.order.iter().rev().filter_map(|id| self.tasks.get(id)).collect()
    }
}

impl Default for RunningTaskRegistry { fn default() -> Self { Self::new() } }
```

In `crates/yi-agent/src/tui/mod.rs`, add:
```rust
pub mod state;
```

### Step 4: 运行测试验证通过

Run: `cargo test -p yi-agent --lib tui::state`
Expected: PASS

### Step 5: Commit

```bash
git add crates/yi-agent/src/tui/state.rs crates/yi-agent/src/tui/mod.rs
git commit -m "feat(tui): RunningTaskRegistry tracks bash task states"
```

---

## Task 6: StatusBar 渲染

**Files:**
- Create: `crates/yi-agent/src/tui/statusbar.rs`
- Modify: `crates/yi-agent/src/tui/mod.rs`
- Modify: `crates/yi-agent/src/tui/app.rs`

### Step 1: 写失败测试

Create test in `crates/yi-agent/src/tui/statusbar.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_interpolation_approaches_target() {
        let mut s = StatusBarState::default();
        s.set_token_target(1000, 500);
        // tick several times
        for _ in 0..60 { s.tick(); }
        assert!((s.display_input_tokens() - 1000).abs() < 50);
        assert!((s.display_output_tokens() - 500).abs() < 30);
    }

    #[test]
    fn test_spinner_hue_advances() {
        let mut s = StatusBarState::default();
        let h1 = s.spinner_hue();
        s.tick();
        let h2 = s.spinner_hue();
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_format_thousands() {
        assert_eq!(format_thousands(0), "0");
        assert_eq!(format_thousands(999), "999");
        assert_eq!(format_thousands(1234), "1,234");
        assert_eq!(format_thousands(1000000), "1,000,000");
    }
}
```

### Step 2: 运行测试验证失败

Run: `cargo test -p yi-agent --lib tui::statusbar`
Expected: FAIL — 模块不存在

### Step 3: 实现

Create `crates/yi-agent/src/tui/statusbar.rs`:

```rust
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use crate::tui::state::{RunningTaskRegistry, TaskStatus};

#[derive(Debug, Clone, Default)]
pub struct StatusBarState {
    target_input: u64,
    target_output: u64,
    display_input: u64,
    display_output: u64,
    spinner_phase: u32, // 0..360
    last_usage_time: Option<std::time::Instant>,
}

impl StatusBarState {
    pub fn set_token_target(&mut self, input: u64, output: u64) {
        self.target_input = input.max(self.target_input);
        self.target_output = output.max(self.target_output);
        self.last_usage_time = Some(std::time::Instant::now());
    }
    pub fn tick(&mut self) {
        // linear interpolation: move 1/30 of the gap per tick
        let di = self.target_input.saturating_sub(self.display_input);
        let dd = self.target_output.saturating_sub(self.display_output);
        let step_i = (di / 30).max(1);
        let step_o = (dd / 30).max(1);
        self.display_input = self.display_input.saturating_add(step_i.min(di));
        self.display_output = self.display_output.saturating_add(step_o.min(dd));
        // spinner hue: 8° per tick, ~1.5s per cycle at 30hz
        self.spinner_phase = (self.spinner_phase + 8) % 360;
        // stop interpolation after 1s idle
        if let Some(t) = self.last_usage_time {
            if t.elapsed() > std::time::Duration::from_secs(1) {
                self.display_input = self.target_input;
                self.display_output = self.target_output;
            }
        }
    }
    pub fn reset_for_new_call(&mut self) {
        self.target_input = 0;
        self.target_output = 0;
        self.display_input = 0;
        self.display_output = 0;
        self.last_usage_time = None;
    }
    pub fn display_input_tokens(&self) -> u64 { self.display_input }
    pub fn display_output_tokens(&self) -> u64 { self.display_output }
    pub fn spinner_hue(&self) -> u32 { self.spinner_phase }
    pub fn spinner_color(&self) -> Color {
        let h = self.spinner_phase as f32 / 360.0;
        let (r, g, b) = hsl_to_rgb(h, 0.7, 0.6);
        Color::Rgb(r, g, b)
    }
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h * 6.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match (h * 6.0) as u32 {
        0 => (c, x, 0.0), 1 => (x, c, 0.0), 2 => (0.0, c, x),
        3 => (0.0, x, c), 4 => (x, 0.0, c), _ => (c, 0.0, x),
    };
    (((r + m) * 255.0) as u8, ((g + m) * 255.0) as u8, ((b + m) * 255.0) as u8)
}

pub fn format_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 { out.insert(0, ','); }
        out.insert(0, c);
    }
    out
}

pub fn render_statusbar<'a>(
    state: &'a StatusBarState,
    tasks: &'a RunningTaskRegistry,
    model: &'a str,
) -> Line<'a> {
    let mut spans = Vec::new();
    let running = tasks.list().into_iter().filter(|t| t.status == TaskStatus::Running).collect::<Vec<_>>();
    if !running.is_empty() {
        let dot = Span::styled("●", Style::new().fg(state.spinner_color()));
        let count = running.len();
        let oldest = running.iter().map(|t| t.elapsed()).max().unwrap_or_default();
        let secs = oldest.as_secs_f32();
        let label = if count == 1 { format!(" {} {:.1}s", running[0].tool_name, secs) } else { format!(" {}({}) {:.1}s", running[0].tool_name, count, secs) };
        spans.push(dot);
        spans.push(Span::raw(label));
        spans.push(Span::raw("  "));
    }
    spans.push(Span::styled("prefill ", Style::new().fg(Color::DarkGray)));
    spans.push(Span::styled(format_thousands(state.display_input_tokens()), Style::new().fg(Color::Cyan)));
    spans.push(Span::raw("  decode "));
    spans.push(Span::styled(format_thousands(state.display_output_tokens()), Style::new().fg(Color::Cyan)));
    spans.push(Span::raw("  "));
    spans.push(Span::styled(model, Style::new().fg(Color::DarkGray)));
    Line::from(spans)
}
```

In `crates/yi-agent/src/tui/mod.rs`, add: `pub mod statusbar;`

### Step 4: 运行测试验证通过

Run: `cargo test -p yi-agent --lib tui::statusbar`
Expected: PASS

### Step 5: 集成到 app.rs 渲染

Modify `crates/yi-agent/src/tui/app.rs::run_loop`:
- 在 `run_loop` 函数体内增加 `let mut statusbar_state = StatusBarState::default();`
- 将 `events.poll(Duration::from_millis(50))` 改为 `Duration::from_millis(33)` (30hz)
- 在每轮 loop 开始处(try_recv 之后)调 `statusbar_state.tick()`
- 在 `terminal.draw` 中,layout 改为 5 行:`[Min(3), Length(popup_height), Length(1) statusbar, Length(1) blank, Length(input_height)]`
- 在 statusbar chunk 渲染 `render_statusbar(&statusbar_state, &task_registry, model)`
- 收到 `AgentEvent::Usage(u)` 时 `statusbar_state.set_token_target(u.input_tokens as u64, u.output_tokens as u64)`
- 收到 `AgentEvent::Start` 时 `statusbar_state.reset_for_new_call()`

> 注:事件路由需要从当前 `history.push_event(event, width)` 改为先 match 新事件分流到 task_registry / statusbar_state,其余走 history。下一 Task 实现。

### Step 6: 验证编译

Run: `cargo build -p yi-agent`
Expected: 编译通过

### Step 7: Commit

```bash
git add crates/yi-agent/src/tui/statusbar.rs crates/yi-agent/src/tui/mod.rs crates/yi-agent/src/tui/app.rs
git commit -m "feat(tui): status bar with token interpolation and spinner"
```

---

## Task 7: 事件路由 — 分流到 task_registry / statusbar

**Files:**
- Modify: `crates/yi-agent/src/tui/app.rs`

### Step 1: 写失败测试

在 `tui/state.rs` tests 中补一个集成性测试,验证 AgentEvent 分流逻辑(可通过抽取一个纯函数 `route_event`):

```rust
#[cfg(test)]
mod route_tests {
    use super::*;
    use crate::tui::statusbar::StatusBarState;
    use yi_agent_core::AgentEvent;
    use yi_agent_core::tool::OutputStream;
    use yi_agent_core::provider::TokenUsage;
    use serde_json::json;

    fn route_event(registry: &mut RunningTaskRegistry, sb: &mut StatusBarState, ev: &AgentEvent) {
        match ev {
            AgentEvent::ToolCall { id, name, input } => {
                let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let exp = input.get("expected_timeout_sec").and_then(|v| v.as_u64()).unwrap_or(120) as u32;
                registry.on_tool_call(id, name, &cmd, exp);
            }
            AgentEvent::ToolOutputDelta { id, stream, text } => registry.on_output_delta(id, *stream, text),
            AgentEvent::ToolExit { id, code } => registry.on_exit(id, *code),
            AgentEvent::ToolTimeout { id } => registry.on_timeout(id),
            AgentEvent::Usage(u) => sb.set_token_target(u.input_tokens as u64, u.output_tokens as u64),
            _ => {}
        }
    }

    #[test]
    fn test_route_full_flow() {
        let mut r = RunningTaskRegistry::new();
        let mut sb = StatusBarState::default();
        route_event(&mut r, &mut sb, &AgentEvent::ToolCall { id: "t1".into(), name: "bash".into(), input: json!({"command":"echo hi","expected_timeout_sec":30}) });
        assert_eq!(r.running_count(), 1);
        route_event(&mut r, &mut sb, &AgentEvent::ToolOutputDelta { id: "t1".into(), stream: OutputStream::Stdout, text: "hi".into() });
        route_event(&mut r, &mut sb, &AgentEvent::ToolExit { id: "t1".into(), code: Some(0) });
        assert_eq!(r.get("t1").unwrap().status, TaskStatus::Done);
        assert!(r.get("t1").unwrap().stdout.contains(b"hi"));
        route_event(&mut r, &mut sb, &AgentEvent::Usage(TokenUsage { input_tokens: 100, output_tokens: 50, ..Default::default() }));
        sb.tick();
        assert!(sb.display_input_tokens() > 0);
    }
}
```

### Step 2: 运行测试验证失败

Run: `cargo test -p yi-agent --lib tui::state::route_tests`
Expected: FAIL — `route_event` 不存在或无法编译

### Step 3: 实现

在 `crates/yi-agent/src/tui/app.rs::run_loop` 中,把当前 `while let Ok(event) = agent_rx.try_recv() { history.push_event(event, width); }` 改为:

```rust
while let Ok(event) = agent_rx.try_recv() {
    match &event {
        AgentEvent::ToolCall { id, name, input } => {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let exp = input.get("expected_timeout_sec").and_then(|v| v.as_u64()).unwrap_or(120) as u32;
            task_registry.on_tool_call(id, name, &cmd, exp);
            statusbar_state.set_token_target(/* unchanged */);
        }
        AgentEvent::ToolOutputDelta { id, stream, text } => task_registry.on_output_delta(id, *stream, text),
        AgentEvent::ToolExit { id, code } => task_registry.on_exit(id, *code),
        AgentEvent::ToolTimeout { id } => task_registry.on_timeout(id),
        AgentEvent::Usage(u) => statusbar_state.set_token_target(u.input_tokens as u64, u.output_tokens as u64),
        AgentEvent::Start => statusbar_state.reset_for_new_call(),
        _ => {}
    }
    history.push_event(event, width);
}
```

> 注:`ToolOutputDelta/ToolExit/ToolTimeout` 不推入 history(避免污染),其余仍推入。可在 `history.rs::push_event` 中对这三种返回 early no-op,或在 app.rs 不调 push_event。

### Step 4: 运行测试验证通过

Run: `cargo test --workspace`
Expected: PASS

### Step 5: Commit

```bash
git add crates/yi-agent/src/tui/app.rs crates/yi-agent/src/tui/state.rs
git commit -m "feat(tui): route stream events to task registry and status bar"
```

---

## Task 8: BashPopup — 列表态 + 详情态

**Files:**
- Create: `crates/yi-agent/src/tui/bash_popup.rs`
- Modify: `crates/yi-agent/src/tui/mod.rs`

### Step 1: 写失败测试

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_popup_select() {
        let mut p = BashPopup::List(ListPopup::new(vec!["t1".into(), "t2".into()]));
        assert_eq!(p.selected_index(), 0);
        p.move_down();
        assert_eq!(p.selected_index(), 1);
        p.move_up();
        assert_eq!(p.selected_index(), 0);
    }

    #[test]
    fn test_detail_popup_scroll_lock() {
        let mut d = DetailPopup::new("t1".into());
        assert!(d.scroll_locked);
        d.scroll_up(1);
        assert!(!d.scroll_locked);
        d.scroll_to_bottom();
        assert!(d.scroll_locked);
    }
}
```

### Step 2: 运行测试验证失败

Run: `cargo test -p yi-agent --lib tui::bash_popup`
Expected: FAIL — 模块不存在

### Step 3: 实现

Create `crates/yi-agent/src/tui/bash_popup.rs`:

```rust
use crate::tui::state::{RunningTaskRegistry, TaskState, TaskStatus};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, List, ListItem, ListState};

pub enum BashPopup {
    None,
    List(ListPopup),
    Detail(DetailPopup),
    ConfirmKill(ConfirmKill),
}

pub struct ListPopup {
    pub selected: usize,
    pub task_ids: Vec<String>,
}
impl ListPopup {
    pub fn new(task_ids: Vec<String>) -> Self { Self { selected: 0, task_ids } }
    pub fn move_up(&mut self) { if self.selected > 0 { self.selected -= 1; } }
    pub fn move_down(&mut self) { if self.selected + 1 < self.task_ids.len() { self.selected += 1; } }
    pub fn selected_id(&self) -> Option<&str> { self.task_ids.get(self.selected).map(|s| s.as_str()) }
}

pub struct DetailPopup {
    pub task_id: String,
    pub scroll: usize,
    pub scroll_locked: bool,
    pub show_kill_confirm: bool,
}
impl DetailPopup {
    pub fn new(task_id: String) -> Self { Self { task_id, scroll: 0, scroll_locked: true, show_kill_confirm: false } }
    pub fn scroll_up(&mut self, n: usize) { self.scroll = self.scroll.saturating_sub(n); self.scroll_locked = false; }
    pub fn scroll_down(&mut self, n: usize, max: usize) {
        self.scroll = (self.scroll + n).min(max);
        if self.scroll >= max { self.scroll_locked = true; }
    }
    pub fn scroll_to_bottom(&mut self) { self.scroll_locked = true; }
}

pub struct ConfirmKill { pub task_id: String }

pub fn render_list_popup<'a>(area: Rect, popup: &'a ListPopup, tasks: &'a RunningTaskRegistry) -> Paragraph<'a> {
    let items: Vec<Line> = popup.task_ids.iter().enumerate().map(|(i, id)| {
        let t = match tasks.get(id) { Some(t) => t, None => return Line::raw(format!("? {}", id)) };
        let (sym, color) = match t.status {
            TaskStatus::Running => ("●", Color::Yellow),
            TaskStatus::Done => ("✓", Color::Green),
            TaskStatus::Failed => ("✗", Color::Red),
            TaskStatus::Timeout => ("✗", Color::Red),
        };
        let style = if i == popup.selected { Style::new().bg(Color::Blue).fg(Color::White) } else { Style::new().fg(color) };
        let secs = t.elapsed().as_secs_f32();
        let status_str = match t.status { TaskStatus::Running => "running", TaskStatus::Done => "done", TaskStatus::Failed => "failed", TaskStatus::Timeout => "timeout" };
        Line::styled(format!(" {} {:<8} {:<24} {:>6.1}s {}", sym, t.tool_name, truncate_str(&t.command, 24), secs, status_str), style)
    }).collect();
    Paragraph::new(items).block(Block::default().borders(Borders::ALL).title("bash tasks"))
}

pub fn render_detail_popup<'a>(area: Rect, popup: &'a DetailPopup, task: &'a TaskState) -> Paragraph<'a> {
    let mut lines = Vec::new();
    let (sym, color) = match task.status {
        TaskStatus::Running => ("●", Color::Yellow), TaskStatus::Done => ("✓", Color::Green),
        TaskStatus::Failed => ("✗", Color::Red), TaskStatus::Timeout => ("✗", Color::Red),
    };
    let secs = task.elapsed().as_secs_f32();
    let header = format!(" bash {} {} {:.1}s (expected {}s)", sym, match task.status { TaskStatus::Running => "running", _ => "done" }, secs, task.expected_timeout_sec);
    lines.push(Line::styled(header, Style::new().fg(color)));
    lines.push(Line::raw(format!(" $ {}", task.command)));
    lines.push(Line::raw(""));
    lines.push(Line::styled("stdout:", Style::new().fg(Color::DarkGray)));
    let stdout_str = String::from_utf8_lossy(&task.stdout);
    for l in stdout_str.lines() { lines.push(Line::raw(l.to_string())); }
    if stdout_str.is_empty() { lines.push(Line::raw("(empty)")); }
    lines.push(Line::raw(""));
    lines.push(Line::styled("stderr:", Style::new().fg(Color::DarkGray)));
    let stderr_str = String::from_utf8_lossy(&task.stderr);
    for l in stderr_str.lines() { lines.push(Line::raw(l.to_string())); }
    if stderr_str.is_empty() { lines.push(Line::raw("(empty)")); }
    lines.push(Line::raw(""));
    lines.push(Line::styled(" [q] back  [k] kill  [↑↓] scroll  [f] follow", Style::new().fg(Color::DarkGray)));
    if task.exceeds_expected() {
        lines.insert(1, Line::styled(" ! exceeds expected timeout", Style::new().fg(Color::Red)));
    }
    Paragraph::new(lines).scroll((popup.scroll, 0))
}

fn truncate_str(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { let mut t: String = s.chars().take(n).collect(); t.push_str("…"); t }
}
```

In `crates/yi-agent/src/tui/mod.rs`, add: `pub mod bash_popup;`

### Step 4: 运行测试验证通过

Run: `cargo test -p yi-agent --lib tui::bash_popup`
Expected: PASS

### Step 5: Commit

```bash
git add crates/yi-agent/src/tui/bash_popup.rs crates/yi-agent/src/tui/mod.rs
git commit -m "feat(tui): bash popup list and detail views"
```

---

## Task 9: Ctrl+P 键位 + 弹窗集成 + kill 二次确认

**Files:**
- Modify: `crates/yi-agent/src/tui/app.rs`

### Step 1: 实现(纯 UI 交互,手动验证为主)

在 `crates/yi-agent/src/tui/app.rs::run_loop` 中:

1. 新增 `let mut bash_popup: BashPopup = BashPopup::None;`
2. 全局键处理段(`handle_key` 之前)加入:
   ```rust
   KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
       let ids: Vec<String> = task_registry.list().iter().map(|t| t.id.clone()).collect();
       if !ids.is_empty() {
           bash_popup = BashPopup::List(ListPopup::new(ids));
       }
   }
   ```
3. 在 `bash_popup != None` 时,键事件先路由到 popup:
   - `List`: ↑/↓ 选择,Enter → `Detail::new(id)`,Esc/q → `None`
   - `Detail`: ↑/↓ scroll,`f` → lock to bottom,`k` → `ConfirmKill`,Esc/q → back to `List`
   - `ConfirmKill`: `y` → send kill(需要 agent channel 或新 kill_tx),`n`/Esc → back to `Detail`
4. 在 `terminal.draw` 中,若 `bash_popup != None`,用 `Clear + Block` 覆盖全屏 area,渲染对应 popup。

**Kill channel:** 需要一条从 TUI 到 agent 的 channel 发送 kill 请求。方案:
- 在 `app.rs::run` 启动时建 `mpsc::channel<String>(16)` (kill_tx, kill_rx),kill_tx 传入 `run_tui`
- agent 侧需要监听 kill_rx 并 kill 对应 child。**这需要 BashTool 暴露 kill 句柄。**
- 简化方案:BashTool 内部维护 `HashMap<ToolCallId, Child>` 的 Arc<Mutex>,但 call_stream 的 Child 在 task 内。更简单:在 agent 层面维护 `HashMap<String, oneshot::Sender<()>>`,call_stream 时注册一个 kill trigger,registry 收到时发信号,call_stream 内 select! 到 kill 信号就 kill child。

> 实现细节:在 Task 3 的 call_stream 执行段中,为每个 tool call 创建 `(kill_tx, kill_rx) = oneshot::channel()`,把 kill_tx 存入 `Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>`(agent 内),call_stream 内 `tokio::select!` 加 `kill_rx` 分支。TUI kill 请求通过 `interrupt_tx` 类似的新 channel 传到 agent,agent 查表发 oneshot。

### Step 2: 验证编译 + 手动测试

Run: `cargo build --workspace`
Expected: 编译通过

手动测试:
```
cargo run -- --tui
# 输入 "run ls -la"
# 任务运行中按 Ctrl+P,看到列表
# Enter 进入详情,看到 stdout 实时输出
# 按 q 返回列表,Esc 退出
# 再起一个 sleep 30 任务,Ctrl+P 进入详情,按 k 再按 y,确认 kill
```

### Step 3: Commit

```bash
git add crates/yi-agent/src/tui/app.rs crates/yi-agent/src/tui/bash_popup.rs crates/yi-agent/src/tui/state.rs
git commit -m "feat(tui): Ctrl+P bash popup with kill confirmation"
```

---

## Task 10: 集成验证 + 文档

### Step 1: 全量测试

Run: `cargo test --workspace`
Expected: 全部通过

### Step 2: 手动端到端测试

启动 TUI,验证:
- 状态栏显示 `prefill 0 decode 0 <model>`,无任务时
- 发 prompt,token 数字平滑增长
- bash 任务运行时,状态栏显示 `● bash 3.2s`,圆点颜色渐变
- Ctrl+P 打开弹窗,选择任务进入详情,看到 stdout/stderr
- 超时任务:发 `sleep 100` + `expected_timeout_sec=2`,2s 后状态栏显示"超出预期",3s 无输出后 kill
- kill 流程:Ctrl+P → 详情 → k → y 确认

### Step 3: 更新 docs/core-feature.md

如有必要,补充任务感知特性说明。

### Step 4: 最终 Commit

```bash
git add -A
git commit -m "feat(tui): task execution perception — token viz + bash status popup"
```

---

## 测试策略说明

- **数据层(Task 1-5)**: TDD,单元测试覆盖。ToolEvent、call_stream、watchdog、RunningTaskRegistry、UsageStats 都有自动化测试。
- **UI 层(Task 6-9)**: 数据状态有单元测试(statusbar tick/interpolation、popup select/scroll);渲染本身靠手动验证(ratatui 渲染测试成本高,收益低)。
- **集成(Task 10)**: 手动端到端 + cargo test --workspace 不退化。

## 风险点

1. **`tokio::time::Interval::reset()` API**: 若不可用,改用 `Instant + sleep_until` 手动管理 idle deadline(Task 2 注释已说明)。
2. **Agent::new 签名**: Task 3 测试中假设的构造函数签名可能与实际不符,按实际签名调整测试。
3. **Kill channel 架构**: Task 9 的 kill 机制需要在 agent 层面加 oneshot 注册表,改动跨 `agent.rs` 和 `app.rs`,是最复杂的一步。若时间紧张,可先不做 kill,只做查看(MVP)。
4. **30hz tick CPU**: 33ms poll 会让 TUI 持续 redraw。ratatui diff 渲染开销小,但若终端慢可能闪烁。可在 Task 6 验证阶段观察,必要时回退到 50ms(20hz,仍够用)。
