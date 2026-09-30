# Mid-Turn User Interjection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let user input submitted while a turn is in flight land in the conversation context before the next provider request, instead of only taking effect after the whole cycle ends.

**Architecture:** `yi-agent-core` gains a bounded `Inbox` that `run_loop` drains before each provider request, before the `EndTurn` decision, and returns to the frontend on any terminal exit. Both frontends reach core through their existing driver channels (TUI reuses `input_tx`; app-server adds `interject_tx` symmetric to `interrupt_tx`), so no component needs a handle that survives `Agent` rebuilds.

**Tech Stack:** Rust (tokio, async-trait, tokio-stream, serde), React + TypeScript + Vitest (desktop), JSON-RPC 2.0 over stdio (app-server).

**Spec:** `docs/superpowers/specs/2026-09-30-mid-turn-user-interjection-design.md` (commit `b61be4f`)

## Global Constraints

- All line citations below are pinned to worktree base `656673c`. Verify with the symbol name, not the number.
- **Do not modify `AgentEvent::Cancelled`.** It has 24 references repo-wide and is used as a turn-end predicate (`tui/app.rs:355`, `:374`, `:1280`). Interjection data travels on two new dedicated events.
- `InterjectionsReturned` **must be emitted before** the terminal event (`Cancelled` / `Done`) at every exit point. Violating this silently drops the user's text.
- Run `cargo fmt --all` in `yi-agent-rs/` before every commit (CLAUDE.md).
- Never run repo-wide `cargo test --workspace` (OOM/deadlock risk). Run per-crate. Check `ps aux | grep cargo` first.
- Inbox capacity is 16, matching `PendingQueue::CAPACITY` (`tui/queued.rs:28`) and `input_tx` (`main.rs:1321`).
- Interjection prefix wording (D3) must name "revision of the current task" and "do not repeat completed work".
- Update `docs/project-management/` + `README.md` index counts in the same PR (CLAUDE.md).

---

### Task 1: Core inbox, interjection type, and the two new events

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs` (add `Interjection`, `InterjectError`, `Inbox`, `InboxHandle`; two `AgentEvent` variants; `Agent.inbox` field; `Agent::interject`, `Agent::inbox_handle`; export in `lib.rs`)
- Test: `yi-agent-rs/crates/yi-agent-core/src/agent.rs` (`mod tests`)

**Interfaces:**
- Consumes: nothing (foundation task).
- Produces: `Interjection { seq: u64, text: String, tag: Option<String> }` (Clone, Debug, PartialEq, Serialize); `InterjectError { Full, NotRunning }`; `Agent::interject(&self, text: String, tag: Option<String>) -> Result<u64, InterjectError>`; `Agent::inbox_handle(&self) -> Option<InboxHandle>`; `AgentEvent::InterjectionAccepted { seq: u64, text: String, tag: Option<String> }`; `AgentEvent::InterjectionsReturned { items: Vec<Interjection> }`.

- [x] **Step 1: Write the failing test**

Append inside `mod tests` in `agent.rs`:

```rust
    #[test]
    fn inbox_assigns_monotonic_seq_and_rejects_when_full() {
        let mut inbox = Inbox::new();
        assert_eq!(inbox.push("a".into(), None).unwrap(), 1);
        assert_eq!(inbox.push("b".into(), Some("tag-1".into())).unwrap(), 2);
        for i in 0..(Inbox::CAPACITY - 2) {
            inbox.push(format!("m{i}"), None).unwrap();
        }
        assert_eq!(
            inbox.push("overflow".into(), None),
            Err(InterjectError::Full)
        );
        assert_eq!(inbox.len(), Inbox::CAPACITY);
        let drained = inbox.drain_all();
        assert_eq!(drained.len(), Inbox::CAPACITY);
        assert_eq!(drained[0].text, "a");
        assert_eq!(drained[1].tag.as_deref(), Some("tag-1"));
        assert!(inbox.drain_all().is_empty());
    }

    #[tokio::test]
    async fn interject_before_first_run_reports_not_running() {
        let provider = ScriptedProvider::new(vec![]);
        let agent = Agent::new(
            Arc::new(provider),
            Arc::new(ToolRegistry::new()),
            AgentConfig::default(),
        );
        assert!(matches!(
            agent.interject("late".into(), None),
            Err(InterjectError::NotRunning)
        ));
        assert!(agent.inbox_handle().is_none());
    }

    #[tokio::test]
    async fn interject_after_run_starts_is_accepted_in_order() {
        let provider = ScriptedProvider::new(vec![vec![ProviderEvent::Stop {
            reason: StopReason::EndTurn,
        }]]);
        let mut agent = Agent::new(
            Arc::new(provider),
            Arc::new(ToolRegistry::new()),
            AgentConfig::default(),
        );
        let stream = agent.run("hello".into()).await.unwrap();
        assert_eq!(agent.interject("first".into(), None).unwrap(), 1);
        assert_eq!(agent.interject("second".into(), Some("t".into())).unwrap(), 2);
        let handle = agent.inbox_handle().expect("handle after run starts");
        assert_eq!(handle.lock().unwrap().len(), 2);
        drop(stream);
    }
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib agent::tests::inbox_assigns_monotonic_seq_and_rejects_when_full agent::tests::interject_`
Expected: FAIL to compile — `cannot find type Inbox`, `cannot find function interject`.

- [x] **Step 3: Write minimal implementation**

In `agent.rs`, next to the existing constants (`CONTINUE_AFTER_TRUNCATION` at `:313`, `COMPLETION_AUDIT_PROMPT` at `:315`), add:

```rust
/// Wrapper placed around user text that arrives mid-turn, so the model reads it
/// as a revision of the current task rather than a brand-new request.
const INTERJECTION_PREFIX: &str = "The user added the following requirement while you were working on this task. \
Fold it into the current task without repeating work you have already completed:\n";

/// One user message submitted while a turn was in flight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Interjection {
    /// Monotonic token assigned by `Inbox`; the frontend's reconciliation key.
    pub seq: u64,
    pub text: String,
    /// Caller-supplied reconciliation id. `None` when the caller does not need
    /// cross-process matching (the TUI).
    pub tag: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InterjectError {
    #[error("interjection inbox is full")]
    Full,
    #[error("no run is active")]
    NotRunning,
}

/// Bounded FIFO of interjections, drained by `run_loop`.
#[derive(Debug)]
pub struct Inbox {
    items: VecDeque<Interjection>,
    next_seq: u64,
}

impl Inbox {
    /// Matches `PendingQueue::CAPACITY` and the TUI input channel capacity.
    pub const CAPACITY: usize = 16;

    pub fn new() -> Self {
        Self {
            items: VecDeque::new(),
            next_seq: 0,
        }
    }

    pub fn push(&mut self, text: String, tag: Option<String>) -> Result<u64, InterjectError> {
        if self.items.len() >= Self::CAPACITY {
            return Err(InterjectError::Full);
        }
        self.next_seq += 1;
        let seq = self.next_seq;
        self.items.push_back(Interjection { seq, text, tag });
        Ok(seq)
    }

    pub fn drain_all(&mut self) -> Vec<Interjection> {
        self.items.drain(..).collect()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

impl Default for Inbox {
    fn default() -> Self {
        Self::new()
    }
}

/// Cloneable delivery handle for the active run's inbox.
#[derive(Debug, Clone)]
pub struct InboxHandle(Arc<Mutex<Inbox>>);

impl InboxHandle {
    pub fn lock(&self) -> std::sync::MutexGuard<'_, Inbox> {
        self.0.lock().unwrap()
    }

    pub fn interject(&self, text: String, tag: Option<String>) -> Result<u64, InterjectError> {
        self.lock().push(text, tag)
    }
}
```

Add `use std::collections::VecDeque;` to the file's imports (next to `use std::sync::{Arc, Mutex};`).

Add to the `AgentEvent` enum, immediately after the existing `Cancelled,` variant (`:269`):

```rust
    /// A mid-turn user message was pushed into the session as a user message.
    /// Emitted only after the text is actually in the transcript, so consumers
    /// can promote a "delivered, pending" entry to a real one.
    InterjectionAccepted {
        seq: u64,
        text: String,
        tag: Option<String>,
    },
    /// Mid-turn user messages that were never consumed, handed back on any
    /// terminal exit. MUST be emitted before `Cancelled` / `Done`.
    InterjectionsReturned {
        items: Vec<Interjection>,
    },
```

Add a field to `struct Agent` and initialize it in `Agent::new`:

```rust
    /// Interjection inbox for the active run. Replaced per run in `start_run`.
    inbox: Option<InboxHandle>,
```

```rust
            provider_turn_gate: None,
            inbox: None,
```

Add the two public methods next to `pub fn cancel_token`:

```rust
    /// Deliver a mid-turn user message. Returns the inbox sequence number, or
    /// `NotRunning` when no run is active / `Full` when the inbox is saturated.
    pub fn interject(&self, text: String, tag: Option<String>) -> Result<u64, InterjectError> {
        match &self.inbox {
            Some(handle) => handle.interject(text, tag),
            None => Err(InterjectError::NotRunning),
        }
    }

    /// Handle for the active run's inbox. Drivers should take it right after
    /// `run()` returns and drop it when the event stream ends.
    pub fn inbox_handle(&self) -> Option<InboxHandle> {
        self.inbox.clone()
    }
```

In `start_run` (`:409`), right after `self.cancel_token = CancellationToken::new();` (`:414`), create the per-run inbox:

```rust
        // Each run gets a fresh inbox: a handle from the previous run must not
        // feed the next one.
        self.inbox = Some(InboxHandle(Arc::new(Mutex::new(Inbox::new()))));
```

Then clone it into the spawned task and pass it to `run_loop`:

```rust
        let inbox = self.inbox.clone();
```

```rust
            run_loop(
                tx,
                provider,
                tools,
                session,
                config,
                cancel_token,
                permission_checker,
                decision_rx,
                provider_turn_gate,
                inbox,
            )
            .await;
```

Add the parameter to `run_loop` (`:523`) — append after `provider_turn_gate`:

```rust
    inbox: Option<InboxHandle>,
```

Finally export the new public types in `yi-agent-core/src/lib.rs` by extending the existing `pub use agent::{...}` list:

```rust
pub use agent::{
    Agent, AgentConfig, AgentError, AgentEvent, DoneReason, Inbox, InboxHandle, InterjectError,
    Interjection, ProviderTurnGate, ProviderTurnLease, RetryCause, Session,
};
```

- [x] **Step 4: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib agent::tests::inbox_ agent::tests::interject_`
Expected: PASS (3 tests).

- [x] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-core/src/agent.rs crates/yi-agent-core/src/lib.rs
git commit -m "feat(core): add the interjection inbox and its two events"
```

---

### Task 2: Drain the inbox before every provider request

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs` (add `inject_pending`, call it in `run_loop`)
- Test: `yi-agent-rs/crates/yi-agent-core/src/agent.rs` (`mod tests`)

**Interfaces:**
- Consumes: `Inbox`, `InboxHandle`, `Interjection`, `InterjectError`, `AgentEvent::InterjectionAccepted`, `INTERJECTION_PREFIX` from Task 1.
- Produces: `async fn inject_pending(tx: &EventTx, inbox: &Option<InboxHandle>, messages: &mut Vec<Message>, session: &Arc<Mutex<Session>>) -> bool` — returns `true` when at least one message was injected. Task 3 calls it from the `EndTurn` check.
- Produces: a provider recording the messages it was called with, so Task 2's test can assert on the request body. Define it test-only:

```rust
    /// Records the `messages` of every request so a test can assert what the
    /// provider actually saw, rather than only what events came out.
    struct RecordingProvider {
        seen: std::sync::Mutex<Vec<Vec<Message>>>,
        calls: std::sync::Mutex<usize>,
    }

    impl RecordingProvider {
        fn new() -> Self {
            Self {
                seen: std::sync::Mutex::new(Vec::new()),
                calls: std::sync::Mutex::new(0),
            }
        }

        fn request_count(&self) -> usize {
            *self.calls.lock().unwrap()
        }

        fn messages_of(&self, index: usize) -> Vec<Message> {
            self.seen.lock().unwrap()[index].clone()
        }
    }

    #[async_trait]
    impl Provider for RecordingProvider {
        async fn call_stream(
            &self,
            req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            self.seen.lock().unwrap().push(req.messages.clone());
            let mut calls = self.calls.lock().unwrap();
            // First request stalls the turn by asking for a tool; the second
            // ends it. That leaves a window between them for an interjection.
            let script = if *calls == 0 {
                vec![
                    ProviderEvent::ToolUseStart {
                        id: "t1".into(),
                        name: "noop".into(),
                    },
                    ProviderEvent::ToolUseEnd { id: "t1".into() },
                    ProviderEvent::Stop {
                        reason: StopReason::EndTurn,
                    },
                ]
            } else {
                vec![ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                }]
            };
            *calls += 1;
            Ok(futures::stream::iter(script).boxed())
        }
    }

    struct NoopTool;

    #[async_trait]
    impl Tool for NoopTool {
        fn name(&self) -> &str {
            "noop"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        fn description(&self) -> &str {
            "Does nothing"
        }
        async fn call(&self, _args: serde_json::Value) -> ToolResult {
            ToolResult::text("noop")
        }
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                read_only: true,
                ..Default::default()
            }
        }
    }
```

- [x] **Step 1: Write the failing test**

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn interjection_lands_in_context_before_the_next_request() {
        let provider = Arc::new(RecordingProvider::new());
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(NoopTool));
        let mut agent = Agent::new(
            Arc::new(provider.clone()),
            Arc::new(tools),
            AgentConfig::default(),
        );

        let mut stream = Box::pin(agent.run("original task".into()).await.unwrap());
        // Let the first request go out, then interject while tool work happens.
        let mut injected = false;
        let events: Vec<AgentEvent> = {
            let mut collected = Vec::new();
            while let Some(ev) = stream.next().await {
                if !injected && matches!(ev, AgentEvent::ToolCall { .. }) {
                    agent.interject("also check the logs".into(), None).unwrap();
                    injected = true;
                }
                collected.push(ev);
            }
            collected
        };
        drop(stream);

        assert!(injected, "test never reached the tool call");
        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::InterjectionAccepted { .. })),
            "expected InterjectionAccepted, got: {events:?}"
        );
        let second = provider.messages_of(1);
        let injected_msg = second
            .iter()
            .find(|m| m.content.iter().any(|b| matches!(
                b, ContentBlock::Text(t) if t.contains("also check the logs")
            )))
            .expect("second request must carry the interjection");
        assert!(
            injected_msg
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text(t) if t.contains("without repeating"))),
            "the interjection must carry the prefix: {injected_msg:?}"
        );
    }
```

- [x] **Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib agent::tests::interjection_lands_in_context_before_the_next_request`
Expected: FAIL — assertion "expected InterjectionAccepted" fails (nothing drains the inbox yet).

- [x] **Step 3: Write minimal implementation**

Add the helper above `run_loop`:

```rust
/// Move every queued interjection into the transcript, announcing each one.
///
/// Returns `true` when at least one message was injected, so callers can decide
/// whether to keep looping (the `EndTurn` check) or continue normally.
async fn inject_pending(
    tx: &EventTx,
    inbox: &Option<InboxHandle>,
    messages: &mut Vec<Message>,
    session: &Arc<Mutex<Session>>,
) -> bool {
    let Some(handle) = inbox else {
        return false;
    };
    let pending = handle.lock().drain_all();
    if pending.is_empty() {
        return false;
    }
    for item in pending {
        let text = format!("{INTERJECTION_PREFIX}{}", item.text);
        messages.push(Message::user(text.clone()));
        session.lock().unwrap().push(Message::user(text));
        let _ = tx
            .send(AgentEvent::InterjectionAccepted {
                seq: item.seq,
                text: item.text,
                tag: item.tag,
            })
            .await;
    }
    true
}
```

In `run_loop`, insert the drain **after** `turn += 1;` (`:560`) and the `max_turns` block, but **before** the `debug!` request-delta log (`:576`) so the injections appear in that log:

```rust
        inject_pending(&tx, &inbox, &mut messages, &session).await;
```

- [x] **Step 4: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib agent::tests::interjection_lands_in_context_before_the_next_request`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-core/src/agent.rs
git commit -m "feat(core): drain the interjection inbox before each request"
```

---

### Task 3: Drain before the EndTurn decision so a late interjection does not end the turn

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs` (the `if tool_uses.is_empty()` block at `:841`)
- Test: `yi-agent-rs/crates/yi-agent-core/src/agent.rs` (`mod tests`)

**Interfaces:**
- Consumes: `inject_pending` from Task 2 (signature is unchanged).
- Produces: no new API. Behaviour: when an interjection arrives after the model's final text but before the loop decides to finish, the loop injects it and continues instead of emitting `Done`.

- [x] **Step 1: Write the failing test**

This test must isolate the **`EndTurn` drain** from Task 2's loop-top drain. It does so by
driving the provider's response item-by-item from the test: the interjection is delivered
while the only request is still streaming, and the run must then issue a second request
instead of ending. With only Task 2 in place the run ends, so the test fails for the right
reason.

```rust
    /// A provider whose response is fed event-by-event from the test, so the test
    /// knows precisely when the stream has been consumed.
    struct FedProvider {
        tx: tokio::sync::Mutex<Option<mpsc::Sender<ProviderEvent>>>,
        seen: std::sync::Mutex<Vec<Vec<Message>>>,
    }

    impl FedProvider {
        fn new() -> Self {
            Self {
                tx: tokio::sync::Mutex::new(None),
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn request_count(&self) -> usize {
            self.seen.lock().unwrap().len()
        }

        fn messages_of(&self, index: usize) -> Vec<Message> {
            self.seen.lock().unwrap()[index].clone()
        }

        /// Wait until request `index` is open, then hand it one event.
        async fn feed_to(&self, index: usize, event: ProviderEvent) {
            loop {
                if self.request_count() > index {
                    let guard = self.tx.lock().await;
                    if let Some(tx) = guard.as_ref() {
                        tx.send(event).await.unwrap();
                        return;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }
    }

    #[async_trait]
    impl Provider for FedProvider {
        async fn call_stream(
            &self,
            req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            self.seen.lock().unwrap().push(req.messages.clone());
            let (tx, rx) = mpsc::channel(8);
            *self.tx.lock().await = Some(tx);
            Ok(tokio_stream::wrappers::ReceiverStream::new(rx).boxed())
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn interjection_before_end_turn_keeps_the_turn_alive() {
        let provider = Arc::new(FedProvider::new());
        let mut agent = Agent::new(
            Arc::new(provider.clone()),
            Arc::new(ToolRegistry::new()),
            AgentConfig::default(),
        );

        let stream = agent.run("task".into()).await.unwrap();
        // The stream owns the run; the agent stays available for `interject`.
        let collector = tokio::spawn(async move { collect_events_async(stream).await });

        // Let request #1 open, then deliver the interjection while it streams.
        while provider.request_count() == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        agent.interject("one more thing".into(), None).unwrap();

        // Now let request #1 finish. The model answers with no tool calls, so the
        // only thing standing between us and `Done` is the EndTurn drain.
        provider
            .feed_to(0, ProviderEvent::Stop { reason: StopReason::EndTurn })
            .await;

        // With the drain in place a second request must open; feed it too.
        for _ in 0..200 {
            if provider.request_count() >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(
            provider.request_count(),
            2,
            "the interjection must open a second request instead of ending the turn"
        );
        provider
            .feed_to(1, ProviderEvent::Stop { reason: StopReason::EndTurn })
            .await;

        let events = collector.await.unwrap();

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::InterjectionAccepted { .. })),
            "expected InterjectionAccepted: {events:?}"
        );
        let second = provider.messages_of(1);
        assert!(
            second.iter().any(|m| m.content.iter().any(|b| matches!(
                b, ContentBlock::Text(t) if t.contains("one more thing")
            ))),
            "the follow-up request must carry the interjection: {second:?}"
        );
        assert!(
            matches!(
                events.last(),
                Some(AgentEvent::Done {
                    reason: DoneReason::EndTurn
                })
            ),
            "the turn must still finish: {events:?}"
        );
    }
```

- [x] **Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib agent::tests::interjection_before_end_turn_keeps_the_turn_alive`
Expected: FAIL — `provider.seen` has only 1 entry (the turn ended without a second request).

- [x] **Step 3: Write minimal implementation**

In `run_loop`, in the `if tool_uses.is_empty() {` block at `:841`, insert the drain **before** the audit check at `:842`:

```rust
        if tool_uses.is_empty() {
            // A message can land after the model's last tool call but before the
            // loop decides to finish. Check once more so it joins this turn
            // instead of being replayed as a brand-new prompt later.
            if inject_pending(&tx, &inbox, &mut messages, &session).await {
                continue;
            }
            if verification_pending && !audit_attempted {
```

- [x] **Step 4: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib agent::tests::interjection_before_end_turn_keeps_the_turn_alive`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-core/src/agent.rs
git commit -m "fix(core): check for interjections before deciding the turn ended"
```

---

### Task 4: Hand unconsumed interjections back on every terminal exit

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs` (new `flush_unconsumed`; call before all five `Cancelled` sends and before both `Done` sends that end a run)
- Test: `yi-agent-rs/crates/yi-agent-core/src/agent.rs` (`mod tests`)

**Interfaces:**
- Consumes: `Inbox`, `InboxHandle`, `Interjection`, `AgentEvent::InterjectionsReturned` from Task 1.
- Produces: `async fn flush_unconsumed(tx: &EventTx, inbox: &Option<InboxHandle>)`; emits `AgentEvent::InterjectionsReturned { items }` when (and only when) the inbox is non-empty. The ordering contract — returned **before** the terminal event — is what Task 9 and Task 10 rely on.

- [x] **Step 1: Write the failing test**

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_returns_unconsumed_interjections_before_cancelled() {
        let provider = Arc::new(StallOnceThenSucceedProvider::new());
        let mut agent = Agent::new(
            Arc::new(provider),
            Arc::new(ToolRegistry::new()),
            fast_stall_config(),
        );

        let stream = agent.run("task".into()).await.unwrap();
        // Deliver while the first attempt is stalling, then cancel immediately so
        // the backoff cancel point is the one that fires.
        agent.interject("never consumed".into(), Some("tag-9".into())).unwrap();
        let token = agent.cancel_token();
        token.cancel();

        let events = collect_events_async(stream).await;
        drop(agent);

        let returned_at = events.iter().position(
            |e| matches!(e, AgentEvent::InterjectionsReturned { items }
                if items.len() == 1 && items[0].text == "never consumed"
                   && items[0].tag.as_deref() == Some("tag-9")),
        );
        let cancelled_at = events
            .iter()
            .position(|e| matches!(e, AgentEvent::Cancelled));

        assert!(
            returned_at.is_some(),
            "the unconsumed interjection must come back: {events:?}"
        );
        assert!(cancelled_at.is_some(), "expected Cancelled: {events:?}");
        assert!(
            returned_at.unwrap() < cancelled_at.unwrap(),
            "InterjectionsReturned must precede Cancelled: {events:?}"
        );
    }
```

Note: `collect_events_async` already exists in `mod tests` (`fn collect_events_async`). Reuse it.

- [x] **Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib agent::tests::cancel_returns_unconsumed_interjections_before_cancelled`
Expected: FAIL — "the unconsumed interjection must come back" (nothing emits `InterjectionsReturned`).

- [x] **Step 3: Write minimal implementation**

Add the helper above `run_loop`:

```rust
/// Give back every interjection that never made it into the transcript.
///
/// Called before each terminal event. A user message that is neither accepted
/// nor returned is silently lost, so this must run on every exit path.
async fn flush_unconsumed(tx: &EventTx, inbox: &Option<InboxHandle>) {
    let Some(handle) = inbox else {
        return;
    };
    let items = handle.lock().drain_all();
    if items.is_empty() {
        return;
    }
    let _ = tx.send(AgentEvent::InterjectionsReturned { items }).await;
}
```

Then insert `flush_unconsumed(&tx, &inbox).await;` immediately before **each** of these sends in `run_loop`:

1. `:549` — the pre-think cancel.
2. `:612` — the cancel while acquiring the provider-turn lease.
3. `:669` — the cancel during THINK.
4. `:726` — the cancel during stall backoff.
5. `:1062` — the cancel during ACT.
6. The `Done { reason: DoneReason::EndTurn }` send in the `tool_uses.is_empty()` tail.
7. The `Done { reason: DoneReason::MaxTurns }` send in the `max_turns` block.

For example, the first one becomes:

```rust
        if cancel_token.is_cancelled() {
            info!(turn, "agent loop cancelled before think");
            flush_unconsumed(&tx, &inbox).await;
            let _ = tx.send(AgentEvent::Cancelled).await;
            return;
        }
```

- [x] **Step 4: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib agent::tests::cancel_returns_unconsumed_interjections_before_cancelled`
Expected: PASS.

- [x] **Step 5: Run the whole core crate to catch regressions**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core`
Expected: PASS. The five existing `AgentEvent::Cancelled` assertions (`:2225`, `:2425`, `:2527`, `:2656`, `:3047`) must stay green — `Cancelled` itself is unchanged.

- [x] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-core/src/agent.rs
git commit -m "fix(core): return unconsumed interjections before ending a turn"
```

---

### Task 5: app-server `turn/interject` RPC and the interject channel

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs` (`RpcError::not_running`)
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/session.rs` (`ThreadSession.interject_tx` + `InterjectionRequest`)
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` (`turn/interject` dispatch; `run_thread_driver` gains `interject_rx`; both `tokio::spawn(run_thread_driver(...))` call sites)
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` (`mod tests`)

**Interfaces:**
- Consumes: `Agent::interject` from Task 1.
- Produces: `RpcError::not_running() -> Self` (code `-32013`); `pub(crate) struct InterjectionRequest { pub turn_id: String, pub interjection_id: String, pub text: String }`; `ThreadSession.interject_tx: mpsc::Sender<InterjectionRequest>`; the `turn/interject` method responding `{ "turn_id": <active turn>, "interjection_id": <new id> }`.

- [x] **Step 1: Write the failing test**

Add to `mod tests` in `server.rs`:

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn turn_interject_while_running_is_accepted_and_keeps_the_turn() {
        let mut h = Harness::with_factory(build_slow_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":11,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;
        let started_turn = loop {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(11)) {
                break v["result"]["turn_id"].as_str().unwrap().to_string();
            }
        };

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":12,"method":"turn/interject","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"also do X"}}]}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(12)) {
                assert_eq!(
                    v["result"]["turn_id"].as_str().unwrap(),
                    started_turn,
                    "an interjection must not open a new turn: {v}"
                );
                assert!(
                    v["result"]["interjection_id"].as_str().is_some(),
                    "expected an interjection_id: {v}"
                );
                break;
            }
        }
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_interject_without_an_active_turn_is_rejected() {
        let mut h = Harness::with_factory(build_slow_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":13,"method":"turn/interject","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"too early"}}]}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(13)) {
                assert_eq!(v["error"]["code"], -32013, "expected not running: {v}");
                break;
            }
        }
        h.shutdown().await;
    }
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib server::tests::turn_interject`
Expected: FAIL — both cases get `-32601 method not found`.

- [x] **Step 3: Write minimal implementation**

In `protocol.rs`, after `turn_in_progress` (`:99`):

```rust
    pub fn not_running() -> Self {
        Self::new(-32013, "no turn is running".to_string())
    }
```

In `session.rs`, add the request type and the field:

```rust
/// A mid-turn user message on its way to the thread's driver.
#[derive(Debug)]
pub struct InterjectionRequest {
    /// The turn this message is being folded into.
    pub turn_id: String,
    /// Protocol-level item id, minted at the RPC boundary so the client can
    /// match the eventual `Item::UserInterjection` exactly.
    pub interjection_id: String,
    pub text: String,
}
```

```rust
    /// 向该 thread 的 driver 投递中途追加消息（与 `prompt_tx`/`interrupt_tx` 同类）。
    pub(crate) interject_tx: mpsc::Sender<InterjectionRequest>,
```

In `server.rs`, at each `ThreadSession { ... }` construction (`thread/start` around `:439`, `thread/resume` around `:598`), create the channel next to the existing `prompt_tx`/`interrupt_tx` pair and pass `interject_tx` into both the struct literal and `run_thread_driver`:

```rust
                        let (interject_tx, interject_rx) = mpsc::channel::<InterjectionRequest>(16);
```

Add `interject_rx` as the parameter after `interrupt_rx` in `run_thread_driver` (`:1084`).

In `run_thread_driver`, extend **both** inner `select!` blocks — the permission-wait one (around `:1191`) and the main stream one (`:1246`) — with a branch next to the existing interrupt arm. For the main loop:

```rust
                Some(request) = interject_rx.recv() => {
                    // Only fold into the turn this request names; a stale
                    // request from a previous turn is dropped rather than
                    // applied to the wrong context.
                    if request.turn_id == turn_id {
                        match agent.interject(request.text, Some(request.interjection_id)) {
                            Ok(_) => {}
                            Err(e) => eprintln!(
                                "[app-server] interjection rejected for {thread_id}: {e}"
                            ),
                        }
                    } else {
                        // Hand it straight back so the client can restore input.
                        for n in translator.on_event(
                            yi_agent_core::AgentEvent::InterjectionsReturned {
                                items: vec![yi_agent_core::Interjection {
                                    seq: 0,
                                    text: format!("(stale) {}", request.text),
                                    tag: Some(request.interjection_id),
                                }],
                            },
                        ) {
                            let _ = write_notification(&writer, &n).await;
                        }
                    }
                }
```

Add the `turn/interject` arm to the main dispatch loop, right after the `turn/interrupt` arm:

```rust
                    "turn/interject" => {
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        let text = match extract_prompt(&req.params) {
                            Some(p) => p,
                            None => {
                                write_response(
                                    &writer,
                                    err_response(
                                        id,
                                        RpcError::invalid_params("missing or empty input text"),
                                    ),
                                )
                                .await?;
                                continue;
                            }
                        };
                        let (tx, turn_id) = {
                            let Some(session) = threads.get(&thread_id) else {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::unknown_thread(&thread_id)),
                                )
                                .await?;
                                continue;
                            };
                            match session.active_turn_id.clone() {
                                Some(turn_id) => (session.interject_tx.clone(), turn_id),
                                None => {
                                    write_response(
                                        &writer,
                                        err_response(id, RpcError::not_running()),
                                    )
                                    .await?;
                                    continue;
                                }
                            }
                        };
                        let interjection_id = format!(
                            "interject-{}-{}",
                            turn_id,
                            uuid::Uuid::new_v4()
                        );
                        if tx
                            .send(InterjectionRequest {
                                turn_id: turn_id.clone(),
                                interjection_id: interjection_id.clone(),
                                text,
                            })
                            .await
                            .is_err()
                        {
                            write_response(&writer, err_response(id, RpcError::not_running()))
                                .await?;
                            continue;
                        }
                        write_response(
                            &writer,
                            ok_response(
                                id,
                                json!({
                                    "turn_id": turn_id,
                                    "interjection_id": interjection_id,
                                }),
                            ),
                        )
                        .await?;
                    }
```

Add the import at the top of `server.rs`:

```rust
use crate::session::{InterjectionRequest, ThreadSession, TurnPrompt};
```

- [x] **Step 4: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib server::tests::turn_interject`
Expected: PASS (2 tests).

- [x] **Step 5: Verify the `-32012` contract is intact**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib server::tests::turn_start_while_turn_in_progress_returns_turn_in_progress`
Expected: PASS. `turn/start` still refuses a concurrent turn.

- [x] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/protocol.rs crates/yi-agent-app-server/src/session.rs crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): add turn/interject folding into the active turn"
```

---

### Task 6: Render interjections as their own protocol item

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs` (`Item::UserInterjection`)
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/translate.rs` (handle `InterjectionAccepted`; buffer and re-emit on `InterjectionsReturned`)
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/translate.rs` (`mod tests`)

**Interfaces:**
- Consumes: `AgentEvent::InterjectionAccepted { seq, text, tag }` and `AgentEvent::InterjectionsReturned { items }` from Task 1; `tag` is the `interjection_id` minted in Task 5.
- Produces: `Item::UserInterjection { id: String, text: String }` in the protocol.

- [x] **Step 1: Write the failing test**

Add to `mod tests` in `translate.rs`:

```rust
    #[test]
    fn interjection_accepted_becomes_a_user_interjection_item() {
        let mut t = Translator::new("t1".into());
        t.set_turn("turn-3".into());
        let out = t.on_event(AgentEvent::InterjectionAccepted {
            seq: 1,
            text: "also do X".into(),
            tag: Some("interject-turn-3-abc".into()),
        });
        let item = out
            .iter()
            .find_map(|n| match n {
                crate::protocol::Notification::ItemStarted { item, .. } => Some(item.clone()),
                _ => None,
            })
            .expect("expected an ItemStarted for the interjection");
        assert_eq!(
            item,
            crate::protocol::Item::UserInterjection {
                id: "interject-turn-3-abc".into(),
                text: "also do X".into(),
            }
        );
    }

    #[test]
    fn interjections_returned_is_reported_to_the_client() {
        let mut t = Translator::new("t1".into());
        t.set_turn("turn-3".into());
        let out = t.on_event(AgentEvent::InterjectionsReturned {
            items: vec![yi_agent_core::Interjection {
                seq: 1,
                text: "never consumed".into(),
                tag: Some("interject-turn-3-abc".into()),
            }],
        });
        let reported = out.iter().find_map(|n| match n {
            crate::protocol::Notification::InterjectionsReturned { items, .. } => Some(items.clone()),
            _ => None,
        });
        assert_eq!(
            reported.expect("expected the returned interjections to reach the client"),
            vec!["interject-turn-3-abc".to_string()]
        );
    }
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib translate::tests::interjection`
Expected: FAIL to compile — no `Item::UserInterjection`, no `Notification::InterjectionsReturned`.

- [x] **Step 3: Write minimal implementation**

In `protocol.rs`, extend `Item` (`:206`) with a fourth variant:

```rust
    /// A user message that arrived mid-turn, rendered as its own bubble so it is
    /// not confused with the message that opened the turn.
    UserInterjection {
        id: String,
        text: String,
    },
```

Add the notification to `Notification`:

```rust
    /// Mid-turn user messages the server could not consume, handed back so the
    /// client can restore them to the input box. Carries the `tag` ids because
    /// those are what the client knows about.
    #[serde(rename = "turn/interjectionsReturned")]
    InterjectionsReturned {
        thread_id: String,
        turn_id: String,
        items: Vec<String>,
    },
```

In `translate.rs`, in `on_event` next to the `AgentEvent::Cancelled` arm (`:308`):

```rust
            AgentEvent::InterjectionAccepted { text, tag, .. } => {
                // `tag` is the interjection_id minted at the RPC boundary; fall
                // back to the internal item namespace when a caller did not
                // supply one (the TUI does not need cross-process matching).
                let id = tag.unwrap_or_else(|| self.alloc_item_id());
                let item = crate::protocol::Item::UserInterjection { id, text };
                out.push(Notification::ItemStarted {
                    thread_id: self.thread_id.clone(),
                    item: item.clone(),
                });
                out.push(Notification::ItemCompleted {
                    thread_id: self.thread_id.clone(),
                    item,
                });
            }
            AgentEvent::InterjectionsReturned { items } => {
                out.push(Notification::InterjectionsReturned {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    items: items.into_iter().map(|i| i.tag.unwrap_or_default()).collect(),
                });
            }
```

Note: `Notification::ItemStarted` / `ItemCompleted` carry **only** `thread_id` and `item`
(`protocol.rs:122`, `:130`) — do not add a `turn_id` field to them. Reuse the existing
`alloc_item_id` helper (`translate.rs:121`) rather than introducing a second id minter.

- [x] **Step 4: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib translate::`
Expected: PASS, including the three pre-existing `Cancelled` tests (`:725`, `:1090`, `:1142`).

- [x] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/protocol.rs crates/yi-agent-app-server/src/translate.rs
git commit -m "feat(app-server): surface mid-turn interjections as their own item"
```

---

### Task 7: Route the TUI's busy submit into the inbox

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs` (take the handle after `run()`; add a third `select!` arm)
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/queued.rs` (`PendingQueue` → `DeliveredInterjections`)
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs` (busy submit sends; rename call sites)
- Test: `yi-agent-rs/crates/yi-agent/src/tui/queued.rs`, `yi-agent-rs/crates/yi-agent/src/tui/app.rs`

**Interfaces:**
- Consumes: `Agent::inbox_handle` / `Agent::interject` from Task 1.
- Produces: `DeliveredInterjections` with the same `submit -> SubmitOutcome`, `on_turn_end`, `len`, `is_empty`, `items`, `clear` surface as `PendingQueue`, plus `CAPACITY = 16`. `SubmitOutcome::{Sent, Queued, Rejected}` keeps its three states, but `Queued` now means "handed to the driver, awaiting receipt" instead of "buffered locally".

- [x] **Step 1: Write the failing test**

In `queued.rs`'s `mod tests`, rename the existing tests that reference `PendingQueue` and add:

```rust
    #[test]
    fn busy_submit_is_reported_as_queued_not_sent() {
        let mut q = DeliveredInterjections::new();
        assert_eq!(q.submit("first".into()), SubmitOutcome::Sent);
        // The caller is responsible for sending on `Queued`; the queue only
        // records that the message is outstanding.
        assert_eq!(q.submit("second".into()), SubmitOutcome::Queued);
        assert_eq!(q.len(), 1);
        assert_eq!(q.items(), ["second".to_string()]);
    }

    #[test]
    fn accepted_receipt_retires_the_oldest_delivery() {
        let mut q = DeliveredInterjections::new();
        q.submit("first".into());
        q.submit("second".into());
        q.submit("third".into());
        // FIFO: the inbox hands receipts back in delivery order.
        q.on_receipt();
        assert_eq!(q.items(), ["second".to_string(), "third".to_string()]);
        q.on_receipt();
        q.on_receipt();
        assert!(q.is_empty());
        // Receipts do not end the turn; only `on_turn_end` clears `in_flight`.
        assert_eq!(q.submit("fourth".into()), SubmitOutcome::Queued);
    }

    #[test]
    fn returned_items_come_back_in_delivery_order() {
        let mut q = DeliveredInterjections::new();
        q.submit("first".into());
        q.submit("second".into());
        let restored = q.take_returned(2);
        assert_eq!(restored, vec!["first".to_string(), "second".to_string()]);
        assert!(q.is_empty());
    }
```

In `app.rs`'s `mod tests`, add (copy the harness shape from
`submit_while_idle_goes_to_history_not_queue`, `app.rs:7089`):

```rust
    #[test]
    fn busy_submit_reaches_the_agent_channel() {
        let (input_tx, mut input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut pending_quit = false;
        let mut popup = None;

        // Establish an in-flight turn and drain the opening message, so the
        // next submit is a mid-turn delivery.
        assert_eq!(queued.submit("opening".into()), SubmitOutcome::Sent);
        let _ = input_tx.try_send("opening".into());
        let _ = input_rx.try_recv();

        input.buffer = "follow-up".to_string();
        input.cursor = input.buffer.len();

        let _ = handle_key(
            make_key(KeyCode::Enter, KeyModifiers::NONE),
            &mut input,
            &mut history,
            1000,
            80,
            24,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
            &std::env::temp_dir(),
            &yi_agent_mcp::McpManager::empty(),
        );

        assert_eq!(
            input_rx.try_recv().ok(),
            Some("follow-up".to_string()),
            "a busy submit must be delivered, not merely previewed"
        );
        assert_eq!(queued.items(), ["follow-up".to_string()]);
    }
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent -- tui::queued::tests::busy_submit_is_reported tui::app::tests::busy_submit_reaches_the_agent_channel`
Expected: FAIL to compile — `DeliveredInterjections` does not exist.

- [x] **Step 3: Write minimal implementation**

In `queued.rs`, rename the type and add the two receipt operations, keeping `submit`/`on_turn_end`/`len`/`is_empty`/`items`/`clear` behaviour identical:

```rust
/// Mid-turn messages handed to the driver, awaiting a receipt from core.
///
/// This replaced a queue that held messages back until the turn ended. Messages
/// now go out immediately; what is tracked here is only *accounting* — which
/// deliveries core has neither accepted nor handed back yet. The preview area
/// renders this list, so the count the user sees is "delivered, pending".
pub struct DeliveredInterjections {
    items: Vec<String>,
    in_flight: bool,
}
```

```rust
    /// A delivery was accepted by core (``AgentEvent::InterjectionAccepted``):
    /// retire the oldest outstanding entry. Receipts arrive in delivery order
    /// because the inbox is FIFO and this process is its only producer.
    pub fn on_receipt(&mut self) {
        if !self.items.is_empty() {
            self.items.remove(0);
        }
    }

    /// Hand back `count` entries core could not consume, oldest first.
    pub fn take_returned(&mut self, count: usize) -> Vec<String> {
        let take = count.min(self.items.len());
        self.items.drain(0..take).collect()
    }
```

Update the `impl Default` block and any `PendingQueue` references in this file to `DeliveredInterjections`.

In `app.rs`, change the busy-submit arm so the text is actually delivered:

```rust
                SubmitOutcome::Queued => {
                    // Delivered to the driver, which folds it into the running
                    // turn before the next provider request. It stays in the
                    // preview until core confirms with a receipt.
                    if input_tx.try_send(text.clone()).is_err() {
                        // Channel full or closed: put the text back rather than
                        // losing it silently.
                        input.insert_str(&text);
                        history.push(
                            HistoryCell::Separator {
                                label: Some(format!(
                                    "追加未送达（通道已满 {}），已退回输入框",
                                    crate::tui::queued::DeliveredInterjections::CAPACITY
                                )),
                            },
                            history_width,
                        );
                    }
                }
```

Rename the remaining `PendingQueue` identifiers in `app.rs` to `DeliveredInterjections` (call sites at the `handle_key` signature, the `run_loop` locals, and the test helpers).

In `main.rs`, inside the run forwarding loop, take the handle after `run()` returns and add the third arm:

```rust
                match agent.run(text).await {
                    Ok(stream) => {
                        let mut stream = Box::pin(stream);
                        // Taken once per run: the handle is valid for this
                        // stream's lifetime and is re-taken on the next run.
                        let inbox = agent.inbox_handle();
                        loop {
                            tokio::select! {
                                event = stream.next() => { /* unchanged */ }
                                _ = interrupt_rx.recv() => { /* unchanged */ }
                                Some(text) = input_rx.recv() => {
                                    // A submit while this turn is in flight: fold
                                    // it into the running turn instead of letting
                                    // the driver treat it as the next prompt.
                                    let Some(inbox) = &inbox else { break };
                                    if let Err(e) = inbox.interject(text, None) {
                                        let _ = agent_tx
                                            .send(yi_agent_core::AgentEvent::InterjectionsReturned {
                                                items: vec![yi_agent_core::Interjection {
                                                    seq: 0,
                                                    text,
                                                    tag: None,
                                                }],
                                            })
                                            .await;
                                        tracing::warn!(error = %e, "interjection rejected");
                                    }
                                }
                            }
                        }
                    }
```

- [x] **Step 4: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent -- tui::`
Expected: PASS. The six existing queue tests in `app.rs` (`:4490`, `:7006`, `:7089`, `:7139`, `:7246`, `:7297`) need their expectations updated for "busy submit now delivers"; update their assertions rather than deleting them, and keep the `full_queue_rejects_and_restores_input_without_blocking` intent by asserting the restore path.

- [x] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/main.rs crates/yi-agent/src/tui/queued.rs crates/yi-agent/src/tui/app.rs
git commit -m "feat(tui): deliver busy submits into the running turn"
```

---

### Task 8: Render receipts and returns in the TUI

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/queued.rs` (preview header)
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs` (handle both events; order matters against the `Done | Cancelled | Error` predicate at `:355`, `:374`, `:1280`)
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/history.rs` (interjection cell)
- Test: same files

**Interfaces:**
- Consumes: `AgentEvent::InterjectionAccepted` / `InterjectionsReturned` (Task 1), `DeliveredInterjections::on_receipt` / `take_returned` (Task 7).
- Produces: no new API. Behaviour: the preview count falls as receipts arrive; returned text is restored to the input box and reported in history.

- [x] **Step 1: Write the failing test**

In `queued.rs`:

```rust
    #[test]
    fn header_says_delivered_pending_not_queued() {
        let lines = render_queued_preview(&["one".to_string()], 40);
        let header = lines[0].spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        assert_eq!(header, "⌛ 已送达，待生效 (1)");
    }
```

In `app.rs`:

```rust
    #[test]
    fn returned_interjection_goes_back_to_the_input_box() {
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut input = InputLine::new();
        let mut history = HistoryState::new();
        assert_eq!(queued.submit("unconsumed".into()), SubmitOutcome::Sent);

        apply_interjection_event(
            &AgentEvent::InterjectionsReturned {
                items: vec![yi_agent_core::Interjection {
                    seq: 1,
                    text: "unconsumed".into(),
                    tag: None,
                }],
            },
            &mut queued,
            &mut input,
            &mut history,
            80,
        );

        assert_eq!(input.buffer, "unconsumed");
        assert!(queued.is_empty());
    }
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent -- tui::queued::tests::header_says tui::app::tests::returned_interjection`
Expected: FAIL — header still reads `排队中`, and `apply_interjection_event` does not exist.

- [x] **Step 3: Write minimal implementation**

In `queued.rs`, change the header and its doc comment:

```rust
/// - 标题行:`⌛ 已送达，待生效 (N)`,dim,N = 总数
```

```rust
        fold_to_width(&format!("⌛ 已送达，待生效 ({})", items.len()), w),
```

Update the two existing assertions at `:302` and `:336` to the new string.

In `app.rs`, add the helper and call it from the event loop **before** the `is_turn_end`
handling, so receipts and returns are applied regardless of whether the terminal event is
present. The run loop's locals are named `history` / `input` (`app.rs:211-212`), and the
event loop currently calls `route_event`, `history.push_event`, then the `is_turn_end`
block:

```rust
/// Apply the two interjection lifecycle events. Must run before the turn-end
/// handling: `InterjectionsReturned` is emitted ahead of `Cancelled`/`Done`,
/// and a turn-end branch that ran first would skip it and lose the text.
fn apply_interjection_event(
    event: &AgentEvent,
    queued: &mut crate::tui::queued::DeliveredInterjections,
    input: &mut InputLine,
    history: &mut HistoryState,
    width: u16,
) {
    match event {
        AgentEvent::InterjectionAccepted { .. } => queued.on_receipt(),
        AgentEvent::InterjectionsReturned { items } => {
            let restored = queued.take_returned(items.len());
            if restored.is_empty() {
                return;
            }
            // The input box is single-line; a re-delivered batch joins with
            // newlines, which `wrap_input_buffer` already folds.
            for (i, text) in restored.iter().enumerate() {
                if i > 0 {
                    input.insert_str("\n");
                }
                input.insert_str(text);
            }
            history.push(
                HistoryCell::Separator {
                    label: Some(format!("{} 条追加未生效，已退回输入框", restored.len())),
                },
                width,
            );
        }
        _ => {}
    }
}
```

Call it in the event loop before the turn-end block:

```rust
            route_event(
                &mut task_registry,
                &mut statusbar_state,
                &mut cost_tracker,
                &event,
            );
            apply_interjection_event(&event, queued, input, &mut history, final_history_area.width);
            history.push_event(event, final_history_area.width);
```

In `history.rs`, add an arm next to `AgentEvent::Cancelled` (`:452`) so the accepted message is visible in the transcript:

```rust
            AgentEvent::InterjectionAccepted { text, .. } => {
                self.cells.push(HistoryCell::Separator {
                    label: Some(format!("追加: {text}")),
                });
            }
            AgentEvent::InterjectionsReturned { .. } => {}
```

- [x] **Step 4: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent -- tui::`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/tui/queued.rs crates/yi-agent/src/tui/app.rs crates/yi-agent/src/tui/history.rs
git commit -m "feat(tui): show interjection receipts and restore returned text"
```

---

### Task 9: Desktop switches to `turn/interject` while a turn is active

**Files:**
- Modify: `desktop/src/lib/protocol.ts` (new methods/notifications)
- Modify: `desktop/src/lib/session.ts` (apply `Item::UserInterjection` and `turn/interjectionsReturned`)
- Modify: `desktop/src/App.tsx` (`send` picks the method from the active status)
- Test: `desktop/src/lib/session.test.ts`, `desktop/src/App.test.tsx`

**Interfaces:**
- Consumes: RPC `turn/interject` (Task 5) and notifications `Item::UserInterjection` / `turn/interjectionsReturned` (Task 6).
- Produces: `Session.apply` handles `user_interjection` items and restores returned text into `session.pendingInput` (a `string` field the input component reads to re-seed its draft).

- [x] **Step 1: Write the failing test**

In `desktop/src/lib/session.test.ts`:

```ts
  it("renders a mid-turn interjection as its own user bubble", () => {
    const s = new Session();
    s.apply({ method: "turn/started", params: { thread_id: "t", turn_id: "u1" } });
    s.apply({
      method: "item/started",
      params: {
        thread_id: "t",
        turn_id: "u1",
        item: { type: "user_interjection", id: "interject-u1-1", text: "also do X" },
      },
    } as never);
    const texts = s.items.filter((i) => i.type === "userMessage").map((i) => (i as { text: string }).text);
    expect(texts).toContain("also do X");
  });

  it("restores returned interjections to the pending input", () => {
    const s = new Session();
    s.apply({ method: "turn/started", params: { thread_id: "t", turn_id: "u1" } });
    s.apply({
      method: "turn/interjectionsReturned",
      params: { thread_id: "t", turn_id: "u1", items: ["interject-u1-1"] },
    } as never);
    expect(s.returnedInterjections).toEqual(["interject-u1-1"]);
  });
```

In `desktop/src/App.test.tsx`:

```ts
  it("uses turn/interject instead of turn/start while a turn is running", async () => {
    // Reuse the existing test harness setup that mounts App with a mock RPC
    // client, then flip the thread to Running as the status tests do.
    const calls: string[] = [];
    // ...arrange client.request to record the method and resolve...
    await sendFromInput("follow-up");
    expect(calls).toContain("turn/interject");
    expect(calls).not.toContain("turn/start");
  });
```

Copy the harness/arrangement from the existing `App.test.tsx` cases that assert on `turn/start` and on the `-32012` rollback, and adapt them.

- [x] **Step 2: Run tests to verify they fail**

Run: `cd desktop && npm test -- session App`
Expected: FAIL — unknown item type / unknown notification, and `turn/start` is still the only method called.

- [x] **Step 3: Write minimal implementation**

`desktop/src/lib/protocol.ts`: add to the item union

```ts
  | { type: "user_interjection"; id: string; text: string }
```

and to the notification union

```ts
  | {
      method: "turn/interjectionsReturned";
      params: { thread_id: string; turn_id: string; items: string[] };
    }
```

`desktop/src/lib/session.ts`: add a field and handle both cases.

```ts
  /** Ids of interjections the server could not consume and handed back. */
  returnedInterjections: string[] = [];
```

```ts
    if (msg.method === "turn/interjectionsReturned") {
      this.returnedInterjections = [...msg.params.items];
      return;
    }
```

and in the item-applying branch, treat `user_interjection` exactly like `userMessage` so it renders as a bubble:

```ts
    if (item.type === "userMessage" || item.type === "user_interjection") {
      this.items.push({ type: "userMessage", id: item.id, text: item.text });
      return;
    }
```

`desktop/src/App.tsx`: pick the method from the live status.

```ts
  const send = async (text: string): Promise<boolean> => {
    const id = store.currentId;
    const c = clientRef.current;
    if (!id || !c) return false;
    const session = store.view(id).session;
    session.addUserMessage(text);
    force((v) => v + 1);
    // Fold into the running turn when there is one: the server would otherwise
    // reject the call with -32012 and the user's text would take effect only
    // after the whole cycle finished.
    const running = statuses.get(id) === "running";
    const method = running ? "turn/interject" : "turn/start";
    try {
      await c.request(method, {
        threadId: id,
        input: [{ type: "text", text }],
      });
      return true;
    } catch (e) {
      session.lastError = formatError(e);
      const last = session.items[session.items.length - 1];
      if (last && last.type === "userMessage" && last.text === text) session.items.pop();
      force((v) => v + 1);
      return false;
    }
  };
```

- [x] **Step 4: Run tests to verify they pass**

Run: `cd desktop && npm test`
Expected: PASS.

- [x] **Step 5: Build the desktop app**

Run: `cd desktop && npm run build`
Expected: exit 0.

- [x] **Step 6: Commit**

```bash
git add desktop/src/lib/protocol.ts desktop/src/lib/session.ts desktop/src/App.tsx desktop/src/lib/session.test.ts desktop/src/App.test.tsx
git commit -m "feat(desktop): fold follow-up input into the running turn"
```

---

### Task 10: End-to-end verification and project-management sync

**Files:**
- Modify: `docs/project-management/yi-agent-core.md`
- Modify: `docs/project-management/yi-agent-app-server.md`
- Modify: `docs/project-management/yi-agent-tui.md`
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/project-management/README.md` (index counts)
- Modify: `docs/bug-list.md` (mark the reported bug fixed)

**Interfaces:**
- Consumes: everything above.
- Produces: no code. A closed loop proving the reported symptom is gone, plus the documentation CLAUDE.md requires.

- [x] **Step 1: Run every affected test suite**

```bash
cd yi-agent-rs && ps aux | grep -v grep | grep -c cargo   # must be 0
cargo test -p yi-agent-core
cargo test -p yi-agent-app-server
cargo test -p yi-agent --bin yi-agent
cd ../desktop && npm test
```

Expected: all PASS. Record the counts.

- [x] **Step 2: Prove the original symptom is fixed end to end**

```bash
cd yi-agent-rs && cargo test -p yi-agent-core --lib agent::tests::interjection_ -- --nocapture
```
Expected: 3 PASS, and the `interjection_lands_in_context_before_the_next_request` case shows the interjection in the **second** request's messages — that is the reported bug, inverted.

- [x] **Step 3: Update the module files**

Add one line per module recording the capability with its verification command, following the existing format in those files (each bullet ends with `验证：<command>`):

- `yi-agent-core.md`: `Inbox` + `Agent::interject` + the two events, with
  `验证：cargo test -p yi-agent-core --lib agent::tests::interjection_` and
  `cargo test -p yi-agent-core --lib agent::tests::cancel_returns_unconsumed_interjections_before_cancelled`.
- `yi-agent-app-server.md`: `turn/interject`, `-32013`, `Item::UserInterjection`, with
  `验证：cargo test -p yi-agent-app-server --lib server::tests::turn_interject` and
  `cargo test -p yi-agent-app-server --lib translate::tests::interjection`.
- `yi-agent-tui.md`: replace the `输入排队` bullet with the delivery semantics, with
  `验证：cargo test -p yi-agent --bin yi-agent -- tui::`.
- `desktop.md`: active-turn follow-up via `turn/interject`, with `验证：cd desktop && npm test`.
- `README.md`: bump each affected module's `完成 / 总计` count.

- [x] **Step 4: Close the bug entry**

In `docs/bug-list.md`, change the user-input entry from `[ ]` to `[x]` and record: root cause (no drain point inside `run_loop`), the fix (inbox drained before each request, before the `EndTurn` decision, and returned on every terminal exit), and the verification commands from Step 1. Do not touch any other entry in that file.

- [x] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add docs/project-management docs/bug-list.md
git commit -m "docs: record mid-turn interjection support"
```

---

## Spec coverage

Every spec decision maps to a task. Use this to check the plan against
`docs/superpowers/specs/2026-09-30-mid-turn-user-interjection-design.md` §2.

| Spec decision | Task |
|---|---|
| D1 timing: request-before + EndTurn-before | T2, T3 |
| D2/D10/D11 TUI routing and `DeliveredInterjections` | T7 |
| D3 injected message shape (prefix) | T1 (`INTERJECTION_PREFIX`), T2 (applies it) |
| D4 same turn, own item | T5 (turn_id unchanged), T6 (`Item::UserInterjection`) |
| D5 return unconsumed text | T4 (core), T8 (TUI restores), T9 (desktop restores) |
| D6 separate events, `Cancelled` untouched | T1, plus the Global Constraints |
| D7 `turn/interject`, `-32013`, `-32012` preserved | T5 (Steps 4-5) |
| D8 `InterjectionAccepted` after the push | T1, T2, T7 (`on_receipt`), T8 |
| D9 `seq` + `tag` reconciliation | T1 (`seq`), T5 (`tag` = `interjection_id`), T6 |
| D12 drain after compact | T2 (placement is after `turn += 1`) |
| D13 injection counts toward `max_turns` | T2 (same placement) |
| D14 interjection outranks the completion audit | T3 (insert before the audit check) |
| D15 `InterjectionsReturned` precedes `Cancelled` | T4 |
| Spec §5 error table | T1 (`Full`/`NotRunning`), T5 (`-32013`), T7 (restore on failure) |
| Spec §6 impact list | each task's Files block |
| Spec §7 test plan (11 cases) | T1-T9; §7 case 8 is T5 Step 5 |
| Spec §8 non-goals | Deferred section below |

---

## Completion status

All ten tasks are implemented and verified on branch `docs/mid-turn-interjection`.

| Task | Commit | Evidence |
|---|---|---|
| 1 core inbox + events | `65398d8` | `cargo test -p yi-agent-core --lib agent::tests::inbox_` / `interjection_before_first_run` / `interject_after_run_starts` |
| 2+3 the two drain points | `43d032b` | `cargo test -p yi-agent-core --lib agent::tests::interjection_` (2 cases; isolated red/green per drain point) |
| 4 return on every exit | `20e762c` | `cargo test -p yi-agent-core --lib agent::tests::cancel_returns_unconsumed_interjections_before_cancelled` |
| 5+6 app-server RPC + item | `a1b3b56` | `cargo test -p yi-agent-app-server --lib server::tests::turn_interject` + `translate::tests::interjection` |
| 7+8 TUI delivery + rendering | `5e135a9` | `cargo test -p yi-agent --bin yi-agent -- tui::` (400) |
| 9 desktop | `ecb59bf` | `cd desktop && npm test` (131) + `npm run build` |
| 10 docs sync | `857bc00` | suites re-run: core 221 lib, app-server 154 lib, yi-agent 502, desktop 131 |

### Where the implementation diverged from this plan

Two plan steps could not be followed literally. Both are recorded here rather than
silently worked around.

1. **Task 7's `DeliveredInterjections` tests contradicted each other.** Step 1's
   first test asserted that a `Sent` message is untracked (`q.len() == 1` after
   one `Sent` plus one `Queued`), while its second and third tests assumed the
   `Sent` message *was* tracked (expecting `["first", "second"]`). Only one
   reading can hold. The implementation takes the first: the message that opens
   a turn goes out via `Agent::run`, which never emits `InterjectionAccepted`, so
   tracking it would strand a phantom entry in the preview forever, and it would
   also break `on_turn_end`'s promotion contract. `items` therefore holds
   mid-turn deliveries only. The tests were written to that semantics.

2. **Task 8's ordering claim is not observable in the TUI.** The plan says a
   turn-end branch running first "would skip it and lose the text". That is true
   for a batch processed within one frame, but `run_loop` drains `agent_rx` once
   per iteration, so `InterjectionsReturned` and `Cancelled` normally arrive in
   separate iterations and the relative order of the two statements does not
   change the outcome. The call is still placed before the turn-end handling
   (correct, and robust if events are ever batched), but the comment and test
   say what is actually true: the test fails when `apply_interjection_event` is
   absent from the event loop, not when it merely runs later.

Also corrected from the plan text: `feed_to` must clear its sender on
`ProviderEvent::Stop` or the stream never ends, and the test provider is wrapped
once (`provider.clone()`), not twice.

### Post-merge note

`main` removed the completion-audit self-check (`69a81b9`) and merged it as
`64c02d4`; this branch merged that in. The conflict was in the single spot the
two changes shared: the EndTurn drain sat immediately before the audit block
that no longer exists. The drain is kept and the audit block dropped, so the
`EndTurn` drain is now simply the last `continue` guard before the turn ends.
The spec's D14 (audit outranked by a pending interjection) is retired with the
audit itself -- nothing to outrank. Core's lib count drops 221 -> 220 because
of the test that commit deleted; every interjection test still passes.

## Deferred (do not implement in this plan)

- Unifying the subagent mailbox path (`subagent_runtime.rs:620-626`, `:735`, cancel-then-restart) with the inbox.
- Interrupting an in-flight stream or tool execution when a message arrives.
- Editing or cancelling a delivered-but-unconsumed interjection.
- Any change to the headless single-prompt mode.
