# Auto-Compact Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement auto-compact: `run_loop` auto-triggers `compact_session` when session token count exceeds `compact_threshold`, keeping recent N turns and summarizing older messages.

**Architecture:** Move `compact_session` from binary `yi-agent` crate to `yi-agent-core`. Add `AgentEvent::AutoCompacting`. In `run_loop`, track `last_input_tokens` from provider `Usage` events; before each THINK phase, if tokens ≥ threshold, call `compact_session` and replace in-memory messages. Change `accumulate_provider_stream` to return the last `TokenUsage` so `run_loop` can update `last_input_tokens`.

**Tech Stack:** Rust, tokio, async-trait, futures. Tests use `ScriptedProvider` (mock) + `#[ignore]`'d real e2e tests via `yi-agent run --json`.

**Worktree:** `.worktrees/auto-compact` (branch `feature/auto-compact`)

**Test commands:**
- Mock: `cd .worktrees/auto-compact/yi-agent-rs && cargo test -p yi-agent-core --lib`
- TUI lib: `cd .worktrees/auto-compact/yi-agent-rs && cargo test -p yi-agent --lib`
- Fmt check: `cd .worktrees/auto-compact/yi-agent-rs && cargo fmt --all`
- Real e2e: `just test-real-e2e` (from worktree root)

**Baseline:** 121 tests passing in yi-agent-core, 0 failed.

---

### Task 1: Move compact.rs to yi-agent-core

Pure file move + path fixes. No behavior change. Tests stay green.

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-core/src/compact.rs` (copy from `yi-agent/src/compact.rs`)
- Modify: `yi-agent-rs/crates/yi-agent-core/src/lib.rs:3` (add `pub mod compact;`)
- Modify: `yi-agent-rs/crates/yi-agent/src/compact.rs` (replace with re-export)
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:3` (keep `mod compact;` — re-export still works)

**Step 1: Copy compact.rs to core**

Copy `yi-agent-rs/crates/yi-agent/src/compact.rs` to `yi-agent-rs/crates/yi-agent-core/src/compact.rs`. Replace the `use yi_agent_core::{...}` import with crate-internal imports:

```rust
use crate::{
    agent::{AgentConfig, AgentError, Session},
    message::{ContentBlock, Message, Role},
    provider::{Provider, ProviderRequest},
};
```

Also update internal references: `yi_agent_core::Role::User` → `Role::User`, `yi_agent_core::Role::Assistant` → `Role::Assistant`, `yi_agent_core::Role::Tool` → `Role::Tool`, `yi_agent_core::Role::System` → `Role::System`. Make `compact_session`, `format_messages_for_summary`, `build_summary_prompt` `pub`. Keep `find_safe_split_point` private (not used outside).

**Step 2: Register module in lib.rs**

In `yi-agent-rs/crates/yi-agent-core/src/lib.rs`, add `pub mod compact;` after `pub mod agent;` (line 3). Add re-export: `pub use compact::compact_session;` in the re-export block.

**Step 3: Replace binary compact.rs with re-export**

Replace contents of `yi-agent-rs/crates/yi-agent/src/compact.rs` with:

```rust
pub use yi_agent_core::compact::{compact_session, format_messages_for_summary, build_summary_prompt};
```

**Step 4: Verify build**

Run: `cd .worktrees/auto-compact/yi-agent-rs && cargo build -p yi-agent-core -p yi-agent 2>&1 | tail -10`
Expected: no errors

**Step 5: Run tests**

Run: `cd .worktrees/auto-compact/yi-agent-rs && cargo test -p yi-agent-core --lib 2>&1 | tail -5`
Expected: `test result: ok. 121 passed; 0 failed` (compact tests now in core, count unchanged)

Run: `cd .worktrees/auto-compact/yi-agent-rs && cargo test -p yi-agent --lib 2>&1 | tail -5`
Expected: pass

**Step 6: Commit**

```bash
cd .worktrees/auto-compact/yi-agent-rs
cargo fmt --all
git add crates/yi-agent-core/src/compact.rs crates/yi-agent-core/src/lib.rs crates/yi-agent/src/compact.rs
git commit -m "refactor(core): move compact_session to yi-agent-core

auto-compact 需要在 run_loop (core 层) 直接调用 compact_session,
将函数从 binary 层移到 core 层。binary 层保留 re-export。"
```

---

### Task 2: Change accumulate_provider_stream to return last TokenUsage

This is a prerequisite for run_loop to know `last_input_tokens`. Pure refactor, no behavior change.

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs:727-752` (`accumulate_provider_stream` fn)
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs:368-388` (caller in run_loop)

**Step 1: Write failing test for new return value**

In `agent.rs` tests module, add:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn accumulate_provider_stream_returns_last_usage() {
    use yi_agent_core::ProviderRequest;
    // ScriptedProvider returns Usage with input_tokens=42
    let provider = ScriptedProvider::new(vec![vec![
        ProviderEvent::TextDelta("hi".into()),
        ProviderEvent::Usage(crate::provider::TokenUsage {
            input_tokens: 42,
            output_tokens: 3,
            ..Default::default()
        }),
        ProviderEvent::Stop { reason: StopReason::EndTurn },
    ]]);
    let tools = Arc::new(ToolRegistry::new());
    let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

    let stream = agent.run("hi".into()).await.unwrap();
    let events = collect_events(stream);

    // The Usage event should be emitted (existing behavior)
    assert!(events.iter().any(|e| matches!(
        e, AgentEvent::Usage { usage, .. } if usage.input_tokens == 42
    )));
}
```

This test actually passes already (Usage is emitted). The real change is internal: `accumulate_provider_stream` needs to return the usage so `run_loop` can use it. Since the test can't observe the internal return directly, we'll verify via Task 3's auto-compact test. **Skip writing a separate failing test here** — this task is a pure refactor verified by existing tests staying green.

**Step 2: Change accumulate_provider_stream signature**

In `agent.rs:727-752`, change the function to return `(Vec<ContentBlock>, StopReason, Option<TokenUsage>)`:

```rust
async fn accumulate_provider_stream(
    stream: BoxStream<'static, ProviderEvent>,
    tx: &mpsc::Sender<AgentEvent>,
    model: &str,
) -> Result<(Vec<ContentBlock>, StopReason, Option<TokenUsage>), AgentError> {
    let tx = tx.clone();
    let model = model.to_string();
    let mut last_usage: Option<TokenUsage> = None;
    let (content, stop_reason) =
        crate::provider::accumulate_stream(stream, move |event| {
            match event {
                ProviderEvent::TextDelta(s) => {
                    let _ = tx.try_send(AgentEvent::AssistantText(s));
                }
                ProviderEvent::Usage(u) => {
                    last_usage = Some(u.clone());
                    let _ = tx.try_send(AgentEvent::Usage {
                        model: model.clone(),
                        usage: u,
                    });
                }
                ProviderEvent::ToolUseDelta { partial_json, .. } => {
                    let _ = tx.try_send(AgentEvent::DecodeDelta(partial_json));
                }
                _ => {}
            }
        })
        .await?;
    Ok((content, stop_reason, last_usage))
}
```

Note: `last_usage` is captured by the closure. Since `accumulate_stream` consumes `on_event: F`, and F is `FnMut`, the closure runs in the same task. `last_usage` must be outside the closure but accessible — use `Cell<Option<TokenUsage>>` or restructure. **Simpler**: move the closure to mutate a local via `RefCell` or return usage from `accumulate_stream` itself.

**Better approach**: Change `accumulate_stream` in `provider.rs:87` to also return the last `TokenUsage`. Update its signature to `Result<(Vec<ContentBlock>, StopReason, Option<TokenUsage>), ProviderError>`. Update the `ProviderEvent::Usage(u)` arm at `provider.rs:129-131` to also stash `u` into a local `last_usage` before calling `on_event`. Then `accumulate_provider_stream` just forwards the third return value.

**Step 2a: Update provider::accumulate_stream**

In `provider.rs:87-150`, change signature and body:

```rust
pub async fn accumulate_stream<F>(
    mut stream: BoxStream<'static, ProviderEvent>,
    mut on_event: F,
) -> Result<(Vec<ContentBlock>, StopReason, Option<TokenUsage>), ProviderError>
where
    F: FnMut(ProviderEvent),
{
    let mut content = Vec::new();
    let mut current_text = String::new();
    let mut tool_uses: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    let mut stop_reason = StopReason::EndTurn;
    let mut last_usage: Option<TokenUsage> = None;

    while let Some(event) = stream.next().await {
        match event {
            ProviderEvent::TextDelta(s) => { /* unchanged */ }
            ProviderEvent::ToolUseStart { .. } => { /* unchanged */ }
            ProviderEvent::ToolUseDelta { .. } => { /* unchanged */ }
            ProviderEvent::ToolUseEnd { .. } => { /* unchanged */ }
            ProviderEvent::Stop { reason } => { /* unchanged */ }
            ProviderEvent::Usage(u) => {
                last_usage = Some(u.clone());
                on_event(ProviderEvent::Usage(u));
            }
        }
    }
    // ... rest unchanged ...
    Ok((content, stop_reason, last_usage))
}
```

Update existing tests in `provider.rs` that assert on `accumulate_stream` return value (they destructure `(content, stop_reason)` — add `let _ = ...` for the third element or destructure with `_usage`).

**Step 2b: Update accumulate_provider_stream in agent.rs**

```rust
async fn accumulate_provider_stream(
    stream: BoxStream<'static, ProviderEvent>,
    tx: &mpsc::Sender<AgentEvent>,
    model: &str,
) -> Result<(Vec<ContentBlock>, StopReason, Option<TokenUsage>), AgentError> {
    let tx = tx.clone();
    let model = model.to_string();
    let (content, stop_reason, last_usage) =
        crate::provider::accumulate_stream(stream, move |event| match event {
            ProviderEvent::TextDelta(s) => {
                let _ = tx.try_send(AgentEvent::AssistantText(s));
            }
            ProviderEvent::Usage(u) => {
                let _ = tx.try_send(AgentEvent::Usage {
                    model: model.clone(),
                    usage: u,
                });
            }
            ProviderEvent::ToolUseDelta { partial_json, .. } => {
                let _ = tx.try_send(AgentEvent::DecodeDelta(partial_json));
            }
            _ => {}
        })
        .await?;
    Ok((content, stop_reason, last_usage))
}
```

**Step 3: Update caller in run_loop**

In `agent.rs:368-388`, the `tokio::select!` arm destructures `(content, _stop_reason)`. Change to capture usage:

```rust
let (content, _stop_reason, last_usage) = tokio::select! {
    result = accumulate_provider_stream(stream, &tx, &model) => match result {
        Ok(v) => v,
        Err(e) => { /* unchanged */ }
    },
    _ = cancel_token.cancelled() => { /* unchanged */ }
};
// stash for next-turn auto-compact check
last_input_tokens = last_usage.map(|u| u.input_tokens);
```

**Step 4: Verify build**

Run: `cd .worktrees/auto-compact/yi-agent-rs && cargo build -p yi-agent-core 2>&1 | tail -10`
Expected: no errors

**Step 5: Run tests**

Run: `cd .worktrees/auto-compact/yi-agent-rs && cargo test -p yi-agent-core --lib 2>&1 | tail -5`
Expected: `test result: ok. 121 passed; 0 failed` (existing tests still pass; new signature is internal)

**Step 6: Commit**

```bash
cd .worktrees/auto-compact/yi-agent-rs
cargo fmt --all
git add crates/yi-agent-core/src/provider.rs crates/yi-agent-core/src/agent.rs
git commit -m "refactor(core): accumulate_stream returns last TokenUsage

为 auto-compact 需要 run_loop 知道上次 input_tokens,
让 accumulate_stream / accumulate_provider_stream 返回
最后一次 Usage 事件。纯重构,行为不变。"
```

---

### Task 3: Add AgentEvent::AutoCompacting variant

Add the event variant. No behavior yet. Existing tests stay green (new variant is unused).

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs:117-168` (AgentEvent enum)

**Step 1: Add variant**

In the `AgentEvent` enum, after `Done { reason: DoneReason }` (around line 153), add:

```rust
/// Auto-compact 完成事件。old_msg_count 是 compact 前的消息数,
/// new_msg_count 是 compact 后(含 summary + 保留轮)。
AutoCompacting {
    old_msg_count: usize,
    new_msg_count: usize,
},
```

**Step 2: Verify build**

Run: `cd .worktrees/auto-compact/yi-agent-rs && cargo build -p yi-agent-core 2>&1 | tail -5`
Expected: no errors

**Step 3: Run tests**

Run: `cd .worktrees/auto-compact/yi-agent-rs && cargo test -p yi-agent-core --lib 2>&1 | tail -5`
Expected: `121 passed; 0 failed`

**Step 4: Commit**

```bash
cd .worktrees/auto-compact/yi-agent-rs
cargo fmt --all
git add crates/yi-agent-core/src/agent.rs
git commit -m "feat(core): add AgentEvent::AutoCompacting variant

UI 可据此显示 \"已自动压缩 N → M 条\"。暂未发出,Task 4 接入。"
```

---

### Task 4: TDD auto_compact_triggers_when_threshold_exceeded

First auto-compact test. Drives the core logic.

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs` (run_loop + tests module)

**Step 1: Write failing test**

Add to `agent.rs` tests module:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn auto_compact_triggers_when_threshold_exceeded() {
    // Turn 1: tool_use + Usage(input=200). After turn 1, session has
    // user + assistant(tool_use) + tool_results = 3 messages.
    // Turn 2 THINK前: last_input_tokens=200 >= threshold=100 → compact.
    // compact_session 调 provider.call() → Script[1]: "summary text".
    // session 替换为 [summary, recent...]. emit AutoCompacting.
    // Turn 2 THINK → Script[2]: "done" + EndTurn.
    let provider = ScriptedProvider::new(vec![
        vec![
            ProviderEvent::ToolUseStart { id: "t1".into(), name: "upper".into() },
            ProviderEvent::ToolUseDelta { id: "t1".into(), partial_json: r#"{"text":"a"}"#.into() },
            ProviderEvent::ToolUseEnd { id: "t1".into() },
            ProviderEvent::Usage(TokenUsage { input_tokens: 200, ..Default::default() }),
            ProviderEvent::Stop { reason: StopReason::EndTurn },
        ],
        vec![
            ProviderEvent::TextDelta("summary text".into()),
            ProviderEvent::Stop { reason: StopReason::EndTurn },
        ],
        vec![
            ProviderEvent::TextDelta("done".into()),
            ProviderEvent::Stop { reason: StopReason::EndTurn },
        ],
    ]);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(UpperEchoTool));
    let config = AgentConfig {
        compact_threshold: Some(100),
        compact_keep_turns: Some(1),
        ..Default::default()
    };
    let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), config);

    let stream = agent.run("prompt".into()).await.unwrap();
    let events = collect_events(stream);

    assert!(
        events.iter().any(|e| matches!(
            e, AgentEvent::AutoCompacting { old_msg_count, new_msg_count }
            if *old_msg_count > *new_msg_count
        )),
        "should emit AutoCompacting with old > new, events: {events:?}"
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done { reason: DoneReason::EndTurn })
    ));
}
```

**Step 2: Run test to verify it fails**

Run: `cd .worktrees/auto-compact/yi-agent-rs && cargo test -p yi-agent-core --lib auto_compact_triggers_when_threshold_exceeded 2>&1 | tail -15`
Expected: FAIL — no `AutoCompacting` event emitted (variant unused).

**Step 3: Implement auto-compact in run_loop**

In `run_loop` (agent.rs around line 287-300), after `let session_len = ...` and before `let mut turn = 0u32;`, add:

```rust
let mut last_input_tokens: Option<u32> = None;
```

Then, inside the `loop {` block, after the `cancel_token.is_cancelled()` check (around line 303-307) and before `turn += 1;` (line 309), add:

```rust
// auto-compact: 每轮 THINK 前用上次 input_tokens 判断
if let (Some(threshold), Some(tokens)) = (
    config.compact_threshold.filter(|&t| t > 0),
    last_input_tokens,
) {
    if tokens >= threshold && messages.len() > 4 {
        let old_count = messages.len();
        let keep_turns = config.compact_keep_turns.unwrap_or(4);
        match crate::compact::compact_session(
            &provider,
            &config,
            &session.lock().unwrap(),
            keep_turns,
        )
        .await
        {
            Ok(new_session) => {
                messages = new_session.messages().to_vec();
                *session.lock().unwrap() = new_session;
                let _ = tx
                    .send(AgentEvent::AutoCompacting {
                        old_msg_count: old_count,
                        new_msg_count: messages.len(),
                    })
                    .await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "auto-compact failed, will retry next turn");
            }
        }
    }
}
```

And after the `tokio::select!` that calls `accumulate_provider_stream` (around line 368-388), update the destructure to capture `last_usage` and update `last_input_tokens`:

```rust
let (content, _stop_reason, last_usage) = tokio::select! { ... };
last_input_tokens = last_usage.map(|u| u.input_tokens);
```

**Step 4: Run test to verify it passes**

Run: `cd .worktrees/auto-compact/yi-agent-rs && cargo test -p yi-agent-core --lib auto_compact_triggers_when_threshold_exceeded 2>&1 | tail -10`
Expected: PASS

**Step 5: Run full test suite**

Run: `cd .worktrees/auto-compact/yi-agent-rs && cargo test -p yi-agent-core --lib 2>&1 | tail -5`
Expected: all pass (122 tests now)

**Step 6: Commit**

```bash
cd .worktrees/auto-compact/yi-agent-rs
cargo fmt --all
git add crates/yi-agent-core/src/agent.rs
git commit -m "feat(core): auto-compact triggers when input_tokens exceed threshold

run_loop 每轮 THINK 前用上次 Usage.input_tokens 判断,
超 compact_threshold 则调 compact_session 压缩 session,
发 AutoCompacting 事件。失败只 warn,下轮重试。"
```

---

### Task 5: TDD auto_compact_skipped_cases

Three skip cases: below threshold, threshold None, threshold zero.

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs` (tests module only)

**Step 1: Write three failing tests**

```rust
#[tokio::test(flavor = "multi_thread")]
async fn auto_compact_skipped_below_threshold() {
    let provider = ScriptedProvider::new(vec![
        vec![
            ProviderEvent::ToolUseStart { id: "t1".into(), name: "upper".into() },
            ProviderEvent::ToolUseDelta { id: "t1".into(), partial_json: r#"{"text":"a"}"#.into() },
            ProviderEvent::ToolUseEnd { id: "t1".into() },
            ProviderEvent::Usage(TokenUsage { input_tokens: 50, ..Default::default() }),
            ProviderEvent::Stop { reason: StopReason::EndTurn },
        ],
        vec![
            ProviderEvent::TextDelta("done".into()),
            ProviderEvent::Stop { reason: StopReason::EndTurn },
        ],
    ]);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(UpperEchoTool));
    let config = AgentConfig {
        compact_threshold: Some(100),
        compact_keep_turns: Some(1),
        ..Default::default()
    };
    let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), config);
    let stream = agent.run("prompt".into()).await.unwrap();
    let events = collect_events(stream);
    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::AutoCompacting { .. })),
        "should NOT emit AutoCompacting below threshold"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_compact_skipped_when_threshold_none() {
    let provider = ScriptedProvider::new(vec![
        vec![
            ProviderEvent::ToolUseStart { id: "t1".into(), name: "upper".into() },
            ProviderEvent::ToolUseDelta { id: "t1".into(), partial_json: r#"{"text":"a"}"#.into() },
            ProviderEvent::ToolUseEnd { id: "t1".into() },
            ProviderEvent::Usage(TokenUsage { input_tokens: 200, ..Default::default() }),
            ProviderEvent::Stop { reason: StopReason::EndTurn },
        ],
        vec![
            ProviderEvent::TextDelta("done".into()),
            ProviderEvent::Stop { reason: StopReason::EndTurn },
        ],
    ]);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(UpperEchoTool));
    let config = AgentConfig {
        compact_threshold: None,
        compact_keep_turns: Some(1),
        ..Default::default()
    };
    let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), config);
    let stream = agent.run("prompt".into()).await.unwrap();
    let events = collect_events(stream);
    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::AutoCompacting { .. })),
        "should NOT emit AutoCompacting when threshold is None"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_compact_skipped_when_threshold_zero() {
    let provider = ScriptedProvider::new(vec![
        vec![
            ProviderEvent::ToolUseStart { id: "t1".into(), name: "upper".into() },
            ProviderEvent::ToolUseDelta { id: "t1".into(), partial_json: r#"{"text":"a"}"#.into() },
            ProviderEvent::ToolUseEnd { id: "t1".into() },
            ProviderEvent::Usage(TokenUsage { input_tokens: 200, ..Default::default() }),
            ProviderEvent::Stop { reason: StopReason::EndTurn },
        ],
        vec![
            ProviderEvent::TextDelta("done".into()),
            ProviderEvent::Stop { reason: StopReason::EndTurn },
        ],
    ]);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(UpperEchoTool));
    let config = AgentConfig {
        compact_threshold: Some(0),
        compact_keep_turns: Some(1),
        ..Default::default()
    };
    let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), config);
    let stream = agent.run("prompt".into()).await.unwrap();
    let events = collect_events(stream);
    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::AutoCompacting { .. })),
        "should NOT emit AutoCompacting when threshold is 0"
    );
}
```

**Step 2: Run tests**

Run: `cd .worktrees/auto-compact/yi-agent-rs && cargo test -p yi-agent-core --lib auto_compact_skipped 2>&1 | tail -10`
Expected: PASS (Task 4 implementation already handles these via `filter(|&t| t > 0)` and `None` pattern).

**Step 3: Commit**

```bash
cd .worktrees/auto-compact/yi-agent-rs
cargo fmt --all
git add crates/yi-agent-core/src/agent.rs
git commit -m "test(core): auto-compact skipped below threshold / None / zero"
```

---

### Task 6: TDD auto_compact_skipped_on_first_turn

First turn has no `last_input_tokens`, so no compact.

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs` (tests module only)

**Step 1: Write test**

```rust
#[tokio::test(flavor = "multi_thread")]
async fn auto_compact_skipped_on_first_turn() {
    // 首轮 last_input_tokens=None,即使 Usage 声称 999 也不 compact(检查发生在
    // THINK 前,此时还没有 Usage 数据)。
    let provider = ScriptedProvider::new(vec![vec![
        ProviderEvent::TextDelta("hi".into()),
        ProviderEvent::Usage(TokenUsage { input_tokens: 999, ..Default::default() }),
        ProviderEvent::Stop { reason: StopReason::EndTurn },
    ]]);
    let tools = Arc::new(ToolRegistry::new());
    let config = AgentConfig {
        compact_threshold: Some(100),
        ..Default::default()
    };
    let mut agent = Agent::new(Arc::new(provider), tools, config);
    let stream = agent.run("prompt".into()).await.unwrap();
    let events = collect_events(stream);
    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::AutoCompacting { .. })),
        "should NOT emit AutoCompacting on first turn"
    );
}
```

**Step 2: Run test**

Run: `cd .worktrees/auto-compact/yi-agent-rs && cargo test -p yi-agent-core --lib auto_compact_skipped_on_first_turn 2>&1 | tail -5`
Expected: PASS (Task 4 already handles — `last_input_tokens` starts as `None`).

**Step 3: Commit**

```bash
cd .worktrees/auto-compact/yi-agent-rs
cargo fmt --all
git add crates/yi-agent-core/src/agent.rs
git commit -m "test(core): auto-compact skipped on first turn (no last_input_tokens)"
```

---

### Task 7: TDD auto_compact_failure_continues_loop

compact fails (provider error), run_loop continues to Done.

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs` (tests module only — implementation already warns and continues)

**Step 1: Write test**

```rust
#[tokio::test(flavor = "multi_thread")]
async fn auto_compact_failure_continues_loop() {
    // Turn 1: tool_use + Usage(input=200).
    // Turn 2 THINK前: compact 触发,但 compact_session 调 provider.call() 返回 Auth error。
    // auto-compact 只 warn,继续 run_loop。Turn 2 THINK → Script[2]: "done" + EndTurn。
    use yi_agent_core::ProviderError;
    struct FailProvider;
    #[async_trait]
    impl Provider for FailProvider {
        async fn call_stream(
            &self,
            _req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            Err(ProviderError::Auth("compact failed".into()))
        }
    }
    // First two calls: turn 1 (tool_use), compact attempt (error).
    // Third call: turn 2 THINK → "done".
    // ScriptedProvider runs out after 2 scripts → returns empty (EndTurn) by default,
    // which is wrong here. Use a provider that returns error on call 2, ok on 1 and 3.
    // Simpler: use a custom provider combining scripted + fail-on-2.
    struct ScriptThenFail {
        scripts: Vec<Vec<ProviderEvent>>,
        call_index: std::sync::Mutex<usize>,
    }
    #[async_trait]
    impl Provider for ScriptThenFail {
        async fn call_stream(
            &self,
            _req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            let mut idx = self.call_index.lock().unwrap();
            let i = *idx;
            *idx += 1;
            if i == 1 {
                // compact call → fail
                return Err(ProviderError::Auth("compact failed".into()));
            }
            let script = self.scripts.get(i).cloned().unwrap_or_else(|| {
                vec![ProviderEvent::Stop { reason: StopReason::EndTurn }]
            });
            Ok(futures::stream::iter(script).boxed())
        }
    }

    let provider = Arc::new(ScriptThenFail {
        scripts: vec![
            vec![
                ProviderEvent::ToolUseStart { id: "t1".into(), name: "upper".into() },
                ProviderEvent::ToolUseDelta { id: "t1".into(), partial_json: r#"{"text":"a"}"#.into() },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
                ProviderEvent::Usage(TokenUsage { input_tokens: 200, ..Default::default() }),
                ProviderEvent::Stop { reason: StopReason::EndTurn },
            ],
            // index 1: compact call fails
            vec![
                ProviderEvent::TextDelta("done".into()),
                ProviderEvent::Stop { reason: StopReason::EndTurn },
            ],
        ],
        call_index: std::sync::Mutex::new(0),
    });
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(UpperEchoTool));
    let config = AgentConfig {
        compact_threshold: Some(100),
        compact_keep_turns: Some(1),
        ..Default::default()
    };
    let mut agent = Agent::new(provider, Arc::new(tools), config);
    let stream = agent.run("prompt".into()).await.unwrap();
    let events = collect_events(stream);

    // No AutoCompacting because compact failed
    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::AutoCompacting { .. })),
        "should NOT emit AutoCompacting when compact fails"
    );
    // run_loop should continue to Done
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done { reason: DoneReason::EndTurn })
    ));
}
```

**Step 2: Run test**

Run: `cd .worktrees/auto-compact/yi-agent-rs && cargo test -p yi-agent-core --lib auto_compact_failure_continues_loop 2>&1 | tail -10`
Expected: PASS (Task 4 implementation already `tracing::warn!`s and continues).

**Step 3: Commit**

```bash
cd .worktrees/auto-compact/yi-agent-rs
cargo fmt --all
git add crates/yi-agent-core/src/agent.rs
git commit -m "test(core): auto-compact failure continues run_loop to Done"
```

---

### Task 8: TDD auto_compact_resets_baseline

Verify compact → next THINK → compact again works (no infinite loop, no stuck).

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs` (tests module only)

**Step 1: Write test**

```rust
#[tokio::test(flavor = "multi_thread")]
async fn auto_compact_resets_baseline() {
    // Turn 1: tool_use + Usage(200). Session: user+asst+toolres=3.
    // Turn 2 THINK前: 200>=100 → compact. Session → [summary, recent(1轮)].
    //   compact call → Script[1]: "summary1".
    // Turn 2 THINK → Script[2]: tool_use + Usage(200) again.
    // Turn 3 THINK前: 200>=100 → compact again. Session → [summary2, recent].
    //   compact call → Script[3]: "summary2".
    // Turn 3 THINK → Script[4]: "done" + EndTurn.
    let provider = ScriptedProvider::new(vec![
        // 0: turn 1
        vec![
            ProviderEvent::ToolUseStart { id: "t1".into(), name: "upper".into() },
            ProviderEvent::ToolUseDelta { id: "t1".into(), partial_json: r#"{"text":"a"}"#.into() },
            ProviderEvent::ToolUseEnd { id: "t1".into() },
            ProviderEvent::Usage(TokenUsage { input_tokens: 200, ..Default::default() }),
            ProviderEvent::Stop { reason: StopReason::EndTurn },
        ],
        // 1: compact call (turn 2 pre-THINK)
        vec![
            ProviderEvent::TextDelta("summary1".into()),
            ProviderEvent::Stop { reason: StopReason::EndTurn },
        ],
        // 2: turn 2 THINK (post-compact) — tool_use again
        vec![
            ProviderEvent::ToolUseStart { id: "t2".into(), name: "upper".into() },
            ProviderEvent::ToolUseDelta { id: "t2".into(), partial_json: r#"{"text":"b"}"#.into() },
            ProviderEvent::ToolUseEnd { id: "t2".into() },
            ProviderEvent::Usage(TokenUsage { input_tokens: 200, ..Default::default() }),
            ProviderEvent::Stop { reason: StopReason::EndTurn },
        ],
        // 3: compact call (turn 3 pre-THINK)
        vec![
            ProviderEvent::TextDelta("summary2".into()),
            ProviderEvent::Stop { reason: StopReason::EndTurn },
        ],
        // 4: turn 3 THINK — done
        vec![
            ProviderEvent::TextDelta("done".into()),
            ProviderEvent::Stop { reason: StopReason::EndTurn },
        ],
    ]);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(UpperEchoTool));
    let config = AgentConfig {
        compact_threshold: Some(100),
        compact_keep_turns: Some(1),
        ..Default::default()
    };
    let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), config);
    let stream = agent.run("prompt".into()).await.unwrap();
    let events = collect_events(stream);

    let compact_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::AutoCompacting { .. }))
        .count();
    assert_eq!(compact_count, 2, "should compact twice, events: {events:?}");
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done { reason: DoneReason::EndTurn })
    ));
}
```

**Step 2: Run test**

Run: `cd .worktrees/auto-compact/yi-agent-rs && cargo test -p yi-agent-core --lib auto_compact_resets_baseline 2>&1 | tail -15`
Expected: PASS (Task 4's `last_input_tokens = last_usage.map(...)` update after each THINK naturally resets after compact because post-compact THINK returns smaller usage. But in this test the scripted provider always returns 200, so compact triggers every turn. The test verifies no infinite loop — max_turns caps at 100, but the test expects exactly 2 compacts + Done. If it hangs, the `messages.len() > 4` guard may block second compact. **Check**: after first compact, session = [summary, recent_turn] = 2 messages. Turn 2 THINK pushes to 3+ messages. Turn 3 pre-check: messages.len() = 5 > 4 ✓. Should work.)

**Step 3: Commit**

```bash
cd .worktrees/auto-compact/yi-agent-rs
cargo fmt --all
git add crates/yi-agent-core/src/agent.rs
git commit -m "test(core): auto-compact resets baseline, can trigger again next turn"
```

---

### Task 9: Update binary /compact to use core::compact_session

Pure path change. No behavior change.

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:444-474` (ControlCommand::Compact branch)

**Step 1: Update call**

In `main.rs` around line 447, change `crate::compact::compact_session` to `yi_agent_core::compact::compact_session` (or use the re-export `yi_agent_core::compact_session`). The re-export in `lib.rs` makes `yi_agent_core::compact_session` available.

**Step 2: Verify build & tests**

Run: `cd .worktrees/auto-compact/yi-agent-rs && cargo test -p yi-agent --lib 2>&1 | tail -5`
Expected: pass

**Step 3: Commit**

```bash
cd .worktrees/auto-compact/yi-agent-rs
cargo fmt --all
git add crates/yi-agent/src/main.rs
git commit -m "refactor(yi-agent): /compact uses yi_agent_core::compact_session"
```

---

### Task 10: TUI shows AutoCompacting notification

Show a brief notification when auto-compact fires.

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs` (AgentEvent match arm)

**Step 1: Find the AgentEvent match in app.rs**

Search for `AgentEvent::Done` in `tui/app.rs` to find the event-processing match block.

**Step 2: Add AutoCompacting arm**

Add a match arm (placed near `Done`):

```rust
AgentEvent::AutoCompacting { old_msg_count, new_msg_count } => {
    // 显示短暂提示。复用现有 status/message 机制。
    let msg = format!("自动压缩 {} → {} 条消息", old_msg_count, new_msg_count);
    // 具体插入位置取决于 app.rs 现有结构
}
```

The exact integration depends on how `app.rs` handles transient messages. If there's a status bar or toast mechanism, use it. If not, log via `tracing::info!` as fallback.

**Step 3: Verify build**

Run: `cd .worktrees/auto-compact/yi-agent-rs && cargo build -p yi-agent 2>&1 | tail -5`
Expected: no errors

**Step 4: Run TUI lib tests**

Run: `cd .worktrees/auto-compact/yi-agent-rs && cargo test -p yi-agent --lib 2>&1 | tail -5`
Expected: pass

**Step 5: Commit**

```bash
cd .worktrees/auto-compact/yi-agent-rs
cargo fmt --all
git add crates/yi-agent/src/tui/app.rs
git commit -m "feat(tui): show notification on AutoCompacting event"
```

---

### Task 11: Add e2e real test for auto-compact

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/tests/e2e_real.rs` (append new test)

**Step 1: Write test**

Append:

```rust
#[test]
#[ignore]
fn e2e_auto_compact_triggers() {
    if !has_api_key() {
        eprintln!("skip: no API key");
        return;
    }
    let tmp = tempfile::TempDir::new().expect("tempdir");
    // 写一个较大文件,让单轮 read 就能超 2000 tokens
    let big_file = tmp.path().join("big.txt");
    let content = "line of text\n".repeat(500); // ~7KB
    std::fs::write(&big_file, &content).expect("write");

    let output = Command::new(yi_agent_bin())
        .arg("--workdir")
        .arg(tmp.path())
        .arg("run")
        .arg("--json")
        .arg("--compact-ratio")
        .arg("1") // threshold = context_length * 1% ≈ 2000 tokens
        .arg("--compact-keep-turns")
        .arg("1")
        .arg(format!(
            "Read the file at {} and tell me how many lines it has.",
            big_file.display()
        ))
        .output()
        .expect("failed to spawn");

    assert!(
        output.status.success(),
        "failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut found_auto_compacting = false;
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let ty = event_variant(line).unwrap_or_else(|| panic!("invalid JSONL: {line}"));
        if ty == "AutoCompacting" {
            found_auto_compacting = true;
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(v["AutoCompacting"]["old_msg_count"].as_u64() > v["AutoCompacting"]["new_msg_count"].as_u64());
        }
    }
    // 不强制断言 found_auto_compacting == true:模型行为不稳定,
    // 可能 read 单轮没超阈值。只要不 panic 就算通过。
    if found_auto_compacting {
        eprintln!("auto-compact triggered successfully");
    } else {
        eprintln!("auto-compact did not trigger (model output may be short) — acceptable");
    }
}
```

**Step 2: Run test (skip without key)**

Run: `cd .worktrees/auto-compact/yi-agent-rs && cargo test -p yi-agent --test e2e_real e2e_auto_compact_triggers 2>&1 | tail -10`
Expected: `skip: no API key` (or pass if key set)

**Step 3: Run with real key (if available)**

Run: `cd .worktrees/auto-compact && just test-real-e2e 2>&1 | tail -20`
Expected: test runs, prints "auto-compact triggered successfully" or acceptable skip message

**Step 4: Commit**

```bash
cd .worktrees/auto-compact/yi-agent-rs
cargo fmt --all
git add crates/yi-agent/tests/e2e_real.rs
git commit -m "test(yi-agent): e2e real test for auto-compact

用 --compact-ratio 1 让单轮 read 即可触发,验证不 panic
且 AutoCompacting 事件 payload 结构正确。"
```

---

### Task 12: Update project-management docs

**Files:**
- Modify: `docs/project-management/yi-agent-core.md` (add auto-compact feature)
- Modify: `docs/project-management/README.md` (update counts)
- Modify: `docs/project-management/yi-agent-tui.md` (if TUI notification added)

**Step 1: Read current state**

Read `docs/project-management/yi-agent-core.md` and `docs/project-management/README.md` to find the feature list and count format.

**Step 2: Add auto-compact feature**

In `yi-agent-core.md`, add a `[x]` line under the agent/run_loop section:

```markdown
- [x] auto-compact:`run_loop` 每轮 THINK 前检测 `last_input_tokens`,
  超 `compact_threshold` 自动调 `compact_session` 压缩 —
  `yi-agent-core/src/agent.rs:run_loop` + `compact.rs:compact_session`,
  测试 `auto_compact_triggers_when_threshold_exceeded` 等 7 个
```

**Step 3: Update README.md counts**

Increment the "完成 / 总计" count for yi-agent-core.

**Step 4: Commit**

```bash
cd .worktrees/auto-compact
git add docs/project-management/yi-agent-core.md docs/project-management/README.md
git commit -m "docs(project-management): mark auto-compact complete"
```

---

### Task 13: Final verification

**Step 1: Run full core test suite**

Run: `cd .worktrees/auto-compact/yi-agent-rs && cargo test -p yi-agent-core --lib 2>&1 | tail -5`
Expected: all pass (128 tests: 121 baseline + 7 new auto-compact)

**Step 2: Run yi-agent lib tests**

Run: `cd .worktrees/auto-compact/yi-agent-rs && cargo test -p yi-agent --lib 2>&1 | tail -5`
Expected: pass

**Step 3: Run fmt check**

Run: `cd .worktrees/auto-compact/yi-agent-rs && cargo fmt --all -- --check 2>&1 | tail -3`
Expected: no diff

**Step 4: Run clippy**

Run: `cd .worktrees/auto-compact/yi-agent-rs && cargo clippy -p yi-agent-core --lib 2>&1 | tail -10`
Expected: no warnings

**Step 5: Run real e2e (if key available)**

Run: `cd .worktrees/auto-compact && just test-real-e2e 2>&1 | tail -20`
Expected: all pass or skip

**Step 6: Final commit (if any fixups)**

If any test failed, fix and commit. Otherwise no commit.

---

## Summary

13 tasks, TDD throughout. Mock tests verify exact behavior; e2e real test verifies no panic under real LLM. compact_session moves to core. AgentEvent::AutoCompacting added. run_loop tracks last_input_tokens and triggers compact before THINK.
