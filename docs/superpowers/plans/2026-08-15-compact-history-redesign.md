# Compact History Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make yi-agent auto-compaction safely handle long single-user tool loops and pre-turn context growth by preserving user intent, a rolling handoff summary, and a budgeted complete tool suffix.

**Architecture:** `yi-agent-core::compact` becomes a pure planner/reconstructor plus an async summary wrapper. `Session` owns the last provider input-token count so the agent can run the same validated compaction check before every THINK across `Agent::run()` calls. The compacted sequence has one merged User anchor and either a summary Assistant or summary merged into the first retained tool-call Assistant, making it serializable by both provider adapters.

**Tech Stack:** Rust 2024, Tokio, async-trait, serde_json, Clap, yi-agent-core, yi-agent-llm.

## Global Constraints

- Work only in the isolated `feat/compact-history-redesign` worktree; do not modify `main` directly.
- Use `cargo fmt --all` before every commit and verify with `cargo fmt --all -- --check`.
- Do not run multiple Cargo test commands concurrently; inspect for residual cargo/rustc/yi_agent processes first.
- Preserve complete ToolUse/ToolResult pairs; never truncate a retained tool interaction unit.
- Preserve real User messages atomically under a `20_000` approximate-token budget; do not locally truncate them.
- Retain the newest complete tool suffix under a `12_000` approximate-token budget; the newest single oversized unit is the sole allowed budget exception.
- Ask the summary model to target at most `8_000` tokens but do not locally truncate its response.
- A compacted history must be valid for both Anthropic and OpenAI serialization.
- Update `docs/project-management/yi-agent-core.md` and `docs/project-management/README.md` in the implementation commit.

---

## File Structure

| File | Responsibility |
|---|---|
| `yi-agent-rs/crates/yi-agent-core/src/compact.rs` | Summary prompt, approximate counting, pure compaction planning, reconstruction, validation, and provider summary call. |
| `yi-agent-rs/crates/yi-agent-core/src/agent.rs` | Session-level usage state and shared pre-THINK automatic compaction path. |
| `yi-agent-rs/crates/yi-agent/src/config.rs` | CLI/environment parsing and validation for the new preservation budgets; deprecated old setting warning. |
| `yi-agent-rs/crates/yi-agent/src/main.rs` | Maps runtime config to core config and uses the new manual compact signature. |
| `yi-agent-rs/crates/yi-agent-llm/src/anthropic/types.rs` | Serialization regression test for compacted tool history. |
| `yi-agent-rs/crates/yi-agent-llm/src/openai/types.rs` | Serialization regression test for compacted tool history. |
| `.env.example` | Documents new environment variables and deprecates old retention setting. |
| `docs/project-management/yi-agent-core.md` | Updates completed auto-compact completion criterion. |
| `docs/project-management/README.md` | Keeps module completion count synchronized. |

## Task 1: Add pure compaction planning, reconstruction, and validation

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/compact.rs:1-421`
- Test: `yi-agent-rs/crates/yi-agent-core/src/compact.rs:156-421`

**Interfaces:**
- Consumes: `Message`, `Role`, `ContentBlock` from `crate::message`.
- Produces:
  ```rust
  pub const DEFAULT_COMPACT_USER_BUDGET_TOKENS: usize = 20_000;
  pub const DEFAULT_COMPACT_TOOL_BUDGET_TOKENS: usize = 12_000;

  pub struct CompactionPlan { /* private fields */ }
  pub fn plan_compaction(messages: &[Message], user_budget: usize, tool_budget: usize) -> Option<CompactionPlan>;
  pub fn build_compacted_messages(plan: &CompactionPlan, summary: &str) -> Result<Vec<Message>, CompactError>;
  pub fn validate_compacted_messages(messages: &[Message]) -> Result<(), CompactError>;
  ```
- Consumed later by: Task 2's `compact_session`, Task 3's auto-compaction helper, Task 5 provider serialization tests.

- [x] **Step 1: Write the complete failing planner/validator test batch before adding production APIs**

  Add helpers in `compact.rs` tests:

  ```rust
  fn tool_use(id: &str, name: &str) -> ContentBlock {
      ContentBlock::ToolUse {
          id: id.into(),
          name: name.into(),
          input: serde_json::json!({"path": "src/lib.rs"}),
      }
  }

  fn tool_result(id: &str, text: &str) -> ContentBlock {
      ContentBlock::ToolResult {
          tool_use_id: id.into(),
          content: vec![ContentBlock::Text(text.into())],
          is_error: false,
      }
  }
  ```

  Add a test with `[User(task), Assistant(tool_use t1), Tool(result t1), Assistant(tool_use t2), Tool(result t2), Assistant(tool_use t3), Tool(result t3)]`, and use a tool budget that retains only the newest `t3` unit. Call:

  ```rust
  let newest_unit_tokens = estimate_message_tokens(&messages[5])
      + estimate_message_tokens(&messages[6]);
  let plan = plan_compaction(&messages, 20_000, newest_unit_tokens)
      .expect("must compact");
  let compacted = build_compacted_messages(&plan, "checkpoint").unwrap();
  ```

  Assert `compacted.len() < messages.len()`, `compacted[0]` is one User message containing `task`; the first Assistant contains both `Text("[对话摘要]\ncheckpoint")` and `ToolUse { id: "t3", .. }`; every `ToolResult` has its matching retained `ToolUse`; and `validate_compacted_messages(&compacted).is_ok()`. This verifies actual shrinking rather than treating a three-message `User -> Assistant(tool use) -> Tool` exchange as a compaction.

- [x] **Step 2: Add all remaining Task 1 boundary tests before running the red test batch**

Add the following exact tests before any production planner, token estimator, extractor, reconstruction, or validator implementation exists:

```rust
retained_tool_suffix_uses_token_budget_not_unit_count
newest_oversized_tool_unit_is_retained_whole
parallel_tool_uses_and_results_are_retained_as_one_unit
retained_users_are_newest_first_and_never_truncated
existing_summary_is_not_retained_as_real_user_message
empty_tool_suffix_builds_user_then_summary_assistant
validator_rejects_orphan_results_unmatched_uses_and_adjacent_roles
plan_returns_none_when_reconstruction_cannot_shrink_history
```

Use deliberately large `Text("x".repeat(...))` values and a local test-only estimate calculation or explicit tiny budgets to verify boundaries without relying on a provider tokenizer. The no-reduction test uses only `[Message::user("task")]` and asserts no plan. The validation test directly constructs invalid message vectors and asserts `Err(CompactError::InvalidHistory(_))`. The parallel test puts two ToolUse blocks in one Assistant and their two ToolResult blocks in one Tool message, then asserts they are retained together.

- [x] **Step 3: Run the complete red test batch and verify every new test fails because the planner API is absent**

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent-core --lib compact::tests -- --nocapture
```

Expected: compilation fails with missing `plan_compaction`, `build_compacted_messages`, `validate_compacted_messages`, `CompactError`, and, where used by tests, `estimate_message_tokens`. Do not add production code until this output has been read and recorded.

- [x] **Step 4: Define planner types and approximate counting helpers**

  In `compact.rs`, replace the old `find_safe_split_point` logic with private types:

  ```rust
  #[derive(Debug, Clone)]
  struct ToolInteractionUnit {
      messages: Vec<Message>,
      token_estimate: usize,
  }

  #[derive(Debug, Clone)]
  pub struct CompactionPlan {
      summary_input: Vec<Message>,
      retained_user: Message,
      retained_tool_suffix: Vec<Message>,
      retained_user_tokens: usize,
      retained_tool_tokens: usize,
  }

  #[derive(Debug, thiserror::Error)]
  pub enum CompactError {
      #[error("compaction would not reduce history")]
      NoReduction,
      #[error("compacted history is invalid: {0}")]
      InvalidHistory(String),
  }
  ```

  Add `estimate_text_tokens(text: &str) -> usize` using the same documented heuristic as `agent.rs`: ASCII character count divided by 4 plus non-ASCII count divided by 1.5. Add `estimate_block_tokens` and `estimate_message_tokens`; count text, tool names, JSON inputs, nested tool-result text, and image as zero.

- [x] **Step 5: Implement real-user selection and one merged User anchor**

  Implement `is_summary_message(message: &Message) -> bool` for an Assistant text message whose first text block starts with `[对话摘要]\n`. Implement `select_retained_user_message(messages, budget)`:

  - return `None` when `budget == 0` or no real user message fits;
  - walk real `Role::User` messages newest-first;
  - append only a complete message if its estimated tokens fit the remaining budget;
  - restore selected messages to chronological order;
  - combine every text block from every retained message in one `Message::user`, separated by `"[用户消息]\n"` and `"\n\n"`;
  - do not include synthetic continuation/audit prompts as special cases: they are User-role messages and are intentionally represented in the checkpoint unless budget excludes them.

  Keep the selected token estimate in `CompactionPlan` for tracing.

- [x] **Step 6: Implement complete tool-unit extraction and budgeted suffix selection**

  Implement a private `extract_complete_tool_units(messages) -> Vec<ToolInteractionUnit>` that scans forward:

  - an Assistant message starts a candidate only if it contains one or more `ContentBlock::ToolUse` IDs;
  - consume all immediately following `Role::Tool` messages whose `ToolResult.tool_use_id` values are members of that exact ID set;
  - accept the unit only when each ToolUse ID has exactly one matching result and no result references an unknown ID;
  - do not include a candidate with missing, duplicate, or unrelated results;
  - leave plain Assistant text outside all units.

  Implement newest-first suffix selection:

  ```rust
  fn select_retained_tool_suffix(units: &[ToolInteractionUnit], budget: usize) -> (Vec<Message>, usize)
  ```

  Return no suffix for `budget == 0`. Otherwise retain each whole newest unit while it fits. If the newest unit alone exceeds `budget`, retain it alone. Stop at the first older unit that does not fit, then restore original chronological ordering.

- [x] **Step 7: Implement reconstruction and provider-neutral validation**

  `plan_compaction` must select the merged User anchor and tool suffix, build a provisional compacted shape with a sentinel summary, and return `None` unless its message count is strictly lower than the original history count.

  `build_compacted_messages` must:

  ```rust
  let summary_block = ContentBlock::Text(format!("[对话摘要]\n{summary}"));
  ```

  - start with the plan's merged User anchor;
  - if the suffix is empty, append `Message::assistant(vec![summary_block])`;
  - if nonempty, require its first message to be Assistant with ToolUse blocks, insert `summary_block` as content index 0 in that message, then append the entire suffix;
  - invoke `validate_compacted_messages` before returning.

  `validate_compacted_messages` must reject an empty list, a non-User first role, adjacent User roles, adjacent Assistant roles, Tool messages not immediately preceded by an Assistant containing all corresponding ToolUse IDs, unmatched ToolUse IDs, duplicate ToolResult IDs, and ToolResult content blocks outside `Role::Tool` messages. It must allow the normal `Assistant -> Tool -> Assistant` sequence and both providers' core-role representation.

- [x] **Step 8: Run the complete Task 1 test batch and verify it passes**

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent-core --lib compact::tests
```

Expected: every pre-existing compact test and every new planner/validator test passes.

- [x] **Step 9: Commit the pure planner deliverable**

  ```bash
  cd yi-agent-rs && cargo fmt --all
  cd ..
  git add yi-agent-rs/crates/yi-agent-core/src/compact.rs
  git commit -m "feat(core): plan safe compacted histories"
  ```

## Task 2: Use the planner for summary requests and manual `/compact`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/compact.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:499-539,653-666`
- Test: `yi-agent-rs/crates/yi-agent-core/src/compact.rs`
- Test: `yi-agent-rs/crates/yi-agent/src/main.rs:799-817`

**Interfaces:**
- Consumes: Task 1's `CompactionPlan`, `plan_compaction`, and `build_compacted_messages`.
- Produces:
  ```rust
  pub async fn compact_session(
      provider: &Arc<dyn Provider>,
      config: &AgentConfig,
      session: &Session,
  ) -> Result<Option<Session>, AgentError>;
  ```
- Consumed later by: Task 3 automatic compact check and manual compact TUI driver.

- [x] **Step 1: Write failing tests for handoff prompt and no-op provider avoidance**

  Add a compact test provider with an atomic call counter. Add:

  ```rust
  #[tokio::test]
  async fn compact_session_sends_handoff_prompt_and_rebuilds_valid_history() { /* ... */ }

  #[tokio::test]
  async fn compact_session_returns_none_without_calling_provider_for_no_reduction() { /* ... */ }
  ```

  The first test must inspect its `ProviderRequest`: `system.is_none()`, `tools.is_empty()`, exactly one synthetic User message, and prompt text containing `"CONTEXT CHECKPOINT"`, `"8,000"`, `"当前状态"`, and `"下一步"`. Return `"checkpoint"`, then assert the returned session has validated compacted messages. The second test uses one User message and asserts `Ok(None)` and call count zero.

- [x] **Step 2: Run the new tests and verify they fail**

  Run:

  ```bash
  cd yi-agent-rs && cargo test -p yi-agent-core --lib 'compact::tests::compact_session_' -- --nocapture
  ```

  Expected: failure because the current signature accepts `keep_turns` and returns `Session`, not `Option<Session>`.

- [x] **Step 3: Replace the old summary prompt and compact signature**

  Replace `SUMMARY_PROMPT_TEMPLATE` with a Chinese handoff checkpoint prompt that explicitly says:

  ```text
  你正在执行 CONTEXT CHECKPOINT COMPACTION，为下一位继续任务的 LLM 写交接摘要。
  摘要不是给用户的最终回答；请保留目标、约束、进度、关键决策、文件/命令/测试结果、失败、未完成工作与下一步。
  最近部分完整工具记录会原样保留，不要复述其大段输出；必须覆盖将被丢弃的工具信息。
  使用结构化、简洁的格式，目标最多 8,000 tokens。
  ```

  Keep `format_messages_for_summary` as the full original history formatter. Change `compact_session` to call `plan_compaction(messages, config.compact_user_budget_tokens, config.compact_tool_budget_tokens)`. Return `Ok(None)` for no plan. For a plan, call `provider.call`, concatenate returned text, call `build_compacted_messages`, then create a new `Session` from the reconstructed messages while preserving no stale token usage (Task 3 adds session state). Map validation errors to an `AgentError` variant added in this task:

  ```rust
  #[error("compaction error: {0}")]
  Compact(#[from] crate::compact::CompactError),
  ```

- [x] **Step 4: Adapt manual compact to distinguish no-op from success**

  In `main.rs`, remove `keep_turns` calculation and call the new signature. For `Ok(Some(new_session))`, rebuild the agent and emit `ManualCompacted`. For `Ok(None)`, do not rebuild the agent; emit `ManualCompactFailed { message: "没有可压缩的历史".into() }`. Preserve the existing provider-error branch.

  Update `manual_compaction_outcome_event` tests to cover the no-op error message as the manual command's observable behavior.

- [x] **Step 5: Run focused core and CLI tests**

  Run:

  ```bash
  cd yi-agent-rs && cargo test -p yi-agent-core --lib compact::tests && cargo test -p yi-agent --bin yi-agent manual_compaction_outcome_events_preserve_counts_and_errors -- --exact
  ```

  Expected: PASS.

- [x] **Step 6: Commit the summary and manual compact deliverable**

  ```bash
  cd yi-agent-rs && cargo fmt --all
  cd ..
  git add yi-agent-rs/crates/yi-agent-core/src/compact.rs yi-agent-rs/crates/yi-agent-core/src/agent.rs yi-agent-rs/crates/yi-agent/src/main.rs
  git commit -m "feat: rebuild compact sessions from checkpoint summaries"
  ```

## Task 3: Persist usage in Session and run pre-turn and mid-turn auto-compaction

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs:24-87,287-827,2400-2935`
- Test: `yi-agent-rs/crates/yi-agent-core/src/agent.rs:997-1050,2400-2935`

**Interfaces:**
- Consumes: Task 2's `compact_session(...) -> Result<Option<Session>, AgentError>`.
- Produces:
  ```rust
  impl Session {
      pub fn last_input_tokens(&self) -> Option<u32>;
      pub fn set_last_input_tokens(&mut self, tokens: Option<u32>);
      pub fn replace_messages(&mut self, messages: Vec<Message>);
  }
  ```
  and `async fn maybe_auto_compact(...) -> Option<Vec<Message>>` private to `agent.rs`.
- Consumed later by: normal `Agent::run` and Task 4 configuration mapping.

- [x] **Step 1: Write a failing session-state test**

  Add:

  ```rust
  #[test]
  fn session_tracks_and_clears_last_input_tokens() {
      let mut session = Session::new();
      assert_eq!(session.last_input_tokens(), None);
      session.set_last_input_tokens(Some(160_000));
      assert_eq!(session.last_input_tokens(), Some(160_000));
      session.set_last_input_tokens(None);
      assert_eq!(session.last_input_tokens(), None);
  }
  ```

- [x] **Step 2: Run it and verify it fails**

  Run:

  ```bash
  cd yi-agent-rs && cargo test -p yi-agent-core --lib agent::tests::session_tracks_and_clears_last_input_tokens -- --exact
  ```

  Expected: compile failure because the accessors do not exist.

- [x] **Step 3: Add Session token state and safe replacement methods**

  Change `Session` to:

  ```rust
  pub struct Session {
      messages: Vec<Message>,
      last_input_tokens: Option<u32>,
  }
  ```

  Retain `Default`, ensuring it initializes token usage to `None`. Add the three public methods from the interface. `replace_messages` must replace only messages; callers explicitly clear token state after successful compaction. Ensure `truncate` leaves token state unchanged because cancellation must not erase the last valid provider usage.

- [x] **Step 4: Write failing mid-turn and pre-turn regression tests**

  Replace old turn-count-based tests with these exact tests using `ScriptedProvider`, `UpperEchoTool`, and `collect_events`:

  ```rust
  #[tokio::test(flavor = "multi_thread")]
  async fn auto_compact_single_user_mid_turn_preserves_raw_tool_suffix_and_completes() { /* ... */ }

  #[tokio::test(flavor = "multi_thread")]
  async fn auto_compact_runs_before_second_user_turn_from_session_usage() { /* ... */ }

  #[tokio::test(flavor = "multi_thread")]
  async fn auto_compact_success_clears_session_usage_before_next_think() { /* ... */ }

  #[tokio::test(flavor = "multi_thread")]
  async fn auto_compact_failure_keeps_session_usage_and_continues() { /* ... */ }

  #[tokio::test(flavor = "multi_thread")]
  async fn auto_compact_does_not_emit_event_when_plan_is_noop() { /* ... */ }
  ```

  For mid-turn, script: first THINK emits `ToolUse(t1)`, `Usage(input_tokens=200)`, stop; compact call returns `checkpoint`; second THINK emits text `done`, stop. Config uses `compact_threshold: Some(100)`, user budget `20_000`, tool budget `12_000`. Assert an `AutoCompacting` event with old count greater than new count, final `Done(EndTurn)`, and final session retains User, summary-bearing assistant tool use, and matching tool result.

  For pre-turn, run the same agent twice: first `run("first")` returns an EndTurn response with `Usage(200)`; second `run("second")` must consume a compact summary call before its regular THINK response. Assert exactly one auto event belongs to the second stream and the summary request occurs before the second normal request.

  For clearing, use a call-counting provider and assert a successful compact does not immediately issue a second compact request before a subsequent provider Usage event. For failure, make the summary `provider.call` return `ProviderError::Auth`; assert no auto event, `session.last_input_tokens() == Some(200)`, and final Done. For no-op, create only one User message with persisted high usage and assert no compact provider call and no event.

- [x] **Step 5: Run the new tests and verify they fail**

  Run:

  ```bash
  cd yi-agent-rs && cargo test -p yi-agent-core --lib 'agent::tests::auto_compact_' -- --nocapture
  ```

  Expected: old logic fails pre-turn/no-op/single-user assertions because `last_input_tokens` is local and old `compact_session` needs multiple user turns.

- [x] **Step 6: Replace local token logic with one shared pre-THINK helper**

  Delete local `let mut last_input_tokens` from `run_loop`. Add a private helper called at the top of every loop iteration after cancellation and before `turn += 1`:

  ```rust
  async fn maybe_auto_compact(
      tx: &mpsc::Sender<AgentEvent>,
      provider: &Arc<dyn Provider>,
      config: &AgentConfig,
      session: &Arc<Mutex<Session>>,
  ) -> Option<Vec<Message>>
  ```

  Its behavior:

  - read `session.last_input_tokens()` and only proceed for an enabled positive threshold that has been reached;
  - clone the session snapshot without holding the mutex across `.await`;
  - call `compact_session`;
  - on `Ok(Some(new_session))`, require `new_session.len() < old_count`, replace session messages, clear `last_input_tokens`, emit `AutoCompacting`, and return `Some(new_messages)`;
  - on `Ok(None)`, return `None` without emitting an event or changing token state;
  - on error, log `warn!` and return `None` without changing session/token state.

  In `run_loop`, if it returns messages, set `messages` and reset `last_logged = 0`; otherwise re-read no data. This single placement is both pre-turn (first loop iteration after `Agent::run` pushes User) and mid-turn (the next iteration after tool observation).

- [x] **Step 7: Persist provider usage and ensure state replacement is not overwritten**

  After `accumulate_provider_stream`, set usage only when present:

  ```rust
  if let Some(usage) = last_usage {
      session.lock().unwrap().set_last_input_tokens(Some(usage.input_tokens));
  }
  ```

  Do not write `None` when a provider omits usage; that preserves the last known session value for a later pre-turn check. This assignment must happen before assistant/tool append logic and never inside the compact helper.

- [x] **Step 8: Run all auto-compaction and Session tests**

  Before running Cargo, check for leftover processes:

  ```bash
  ps aux | grep -v grep | grep -E "[c]argo|[r]ustc|yi_agent" || true
  cd yi-agent-rs && cargo test -p yi-agent-core --lib 'agent::tests::auto_compact_' -- --nocapture
  cargo test -p yi-agent-core --lib agent::tests::session_tracks_and_clears_last_input_tokens -- --exact
  ```

  Expected: PASS.

- [x] **Step 9: Commit session-scoped automatic compaction**

  ```bash
  cd yi-agent-rs && cargo fmt --all
  cd ..
  git add yi-agent-rs/crates/yi-agent-core/src/agent.rs
  git commit -m "fix(core): compact across turns and tool loops"
  ```

## Task 4: Replace turn retention configuration with token budgets

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs:54-87,1684-1690`
- Modify: `yi-agent-rs/crates/yi-agent/src/config.rs:8-98,278-349,480-1265`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:124-131,499-539,940-960`
- Modify: `.env.example:11-18`
- Test: `yi-agent-rs/crates/yi-agent/src/config.rs:480-1265`

**Interfaces:**
- Produces:
  ```rust
  pub struct AgentConfig {
      pub compact_user_budget_tokens: usize,
      pub compact_tool_budget_tokens: usize,
      // compact_keep_turns removed
  }

  pub struct Config {
      pub compact_user_budget_tokens: usize,
      pub compact_tool_budget_tokens: usize,
  }
  ```
- Consumes: defaults from `yi_agent_core::compact` or duplicate documented CLI defaults only at the binary boundary.

- [x] **Step 1: Write failing configuration tests**

  Update the `Cli` test constructor helper (or add `fn test_cli() -> Cli`) so all existing tests use the new fields. Add:

  ```rust
  #[test]
  fn load_includes_compact_token_budget_defaults() {
      let config = load(&test_cli_with_key()).unwrap();
      assert_eq!(config.compact_user_budget_tokens, 20_000);
      assert_eq!(config.compact_tool_budget_tokens, 12_000);
  }

  #[test]
  fn load_compact_token_budget_cli_overrides_environment() { /* CLI 30_000/9_000 */ }

  #[test]
  fn load_rejects_zero_user_compact_token_budget() { /* assert error text */ }

  #[test]
  fn load_allows_zero_tool_compact_token_budget() { /* assert 0 */ }
  ```

  In the environment test, set both `YI_AGENT_COMPACT_USER_BUDGET_TOKENS` and `YI_AGENT_COMPACT_TOOL_BUDGET_TOKENS`, then supply different CLI values and assert CLI wins.

- [x] **Step 2: Run configuration tests and verify they fail**

  Run:

  ```bash
  cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent config::tests::load_includes_compact_token_budget_defaults -- --exact
  ```

  Expected: compile failure because the new Config/Cli fields do not exist.

- [x] **Step 3: Add core and binary configuration fields**

  In `AgentConfig`, replace `compact_keep_turns: Option<u32>` with:

  ```rust
  pub compact_user_budget_tokens: usize,
  pub compact_tool_budget_tokens: usize,
  ```

  Use defaults `20_000` and `12_000`. Update `agent_config_has_compact_fields` to assert both values.

  In binary `Config`, replace `compact_keep_turns: u32` with the same two `usize` fields. In `Cli`, add:

  ```rust
  #[arg(long)]
  pub compact_user_budget_tokens: Option<usize>,
  #[arg(long)]
  pub compact_tool_budget_tokens: Option<usize>,
  ```

  Keep `compact_keep_turns: Option<u32>` as hidden compatibility input:

  ```rust
  #[arg(long, hide = true)]
  pub compact_keep_turns: Option<u32>,
  ```

- [x] **Step 4: Implement parsing, validation, and one-time deprecation warning**

  In `config::load`, resolve new fields with exact precedence CLI > environment > defaults:

  ```rust
  let compact_user_budget_tokens = cli.compact_user_budget_tokens
      .or_else(|| env_usize("YI_AGENT_COMPACT_USER_BUDGET_TOKENS"))
      .unwrap_or(20_000);
  if compact_user_budget_tokens == 0 {
      bail!("compact user budget tokens must be greater than zero");
  }

  let compact_tool_budget_tokens = cli.compact_tool_budget_tokens
      .or_else(|| env_usize("YI_AGENT_COMPACT_TOOL_BUDGET_TOKENS"))
      .unwrap_or(12_000);
  ```

  Reuse a small private `env_usize(name: &str) -> Option<usize>` parser rather than duplicating parsing chains.

  If `cli.compact_keep_turns.is_some()` or `YI_AGENT_COMPACT_KEEP_TURNS` is present and nonempty, print exactly once during config load:

  ```text
  warning: YI_AGENT_COMPACT_KEEP_TURNS/--compact-keep-turns is deprecated and ignored; use compact token budgets instead
  ```

  Do not write `compact_keep_turns` into `Config`; it must not reach core behavior.

- [x] **Step 5: Wire new config through main and update documented environment variables**

  In `main.rs`, initialize `AgentConfig` with the two budget values. Remove all `keep_turns` use from the `/compact` branch because Task 2's compact function no longer accepts it. Update manual-test `Config` literals.

  In `.env.example`, replace:

  ```text
  YI_AGENT_COMPACT_KEEP_TURNS=4
  ```

  with:

  ```text
  YI_AGENT_COMPACT_USER_BUDGET_TOKENS=20000
  YI_AGENT_COMPACT_TOOL_BUDGET_TOKENS=12000
  ```

- [x] **Step 6: Run core and binary configuration tests**

  Run:

  ```bash
  cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent config::tests -- --nocapture
  cargo test -p yi-agent-core --lib agent::tests::agent_config_has_compact_fields -- --exact
  ```

  Expected: PASS.

- [x] **Step 7: Commit token-budget configuration**

  ```bash
  cd yi-agent-rs && cargo fmt --all
  cd ..
  git add yi-agent-rs/crates/yi-agent-core/src/agent.rs yi-agent-rs/crates/yi-agent/src/config.rs yi-agent-rs/crates/yi-agent/src/main.rs .env.example
  git commit -m "feat: configure compact retention token budgets"
  ```

## Task 5: Add provider serialization regression tests for compacted history

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/types.rs:219-315`
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/types.rs:245-437`

**Interfaces:**
- Consumes: valid compacted `Vec<Message>` shape from Task 1.
- Produces: serialization regression coverage for Anthropic and OpenAI requests.

- [x] **Step 1: Add failing Anthropic serialization test**

  In `anthropic/types.rs` tests, construct:

  ```rust
  let messages = vec![
      Message::user("[用户消息]\nfix the test"),
      Message::assistant(vec![
          ContentBlock::Text("[对话摘要]\nprevious work".into()),
          ContentBlock::ToolUse { id: "call_1".into(), name: "read".into(), input: serde_json::json!({"path":"src/lib.rs"}) },
      ]),
      Message::tool_results(vec![ContentBlock::ToolResult {
          tool_use_id: "call_1".into(), content: vec![ContentBlock::Text("contents".into())], is_error: false,
      }]),
  ];
  ```

  Convert a `ProviderRequest` and assert external roles equal `["user", "assistant", "user"]`; verify assistant content has `tool_use` id `call_1`; verify final user content has `tool_result` referencing `call_1`.

- [x] **Step 2: Add failing OpenAI serialization test**

  Use the same core messages in `openai/types.rs`. Assert roles equal `["user", "assistant", "tool"]`, assistant `tool_calls[0].id == "call_1"`, and final `tool_call_id == Some("call_1")`.

- [x] **Step 3: Run provider tests and verify they pass**

  Run:

  ```bash
  cd yi-agent-rs && cargo test -p yi-agent-llm anthropic::types::tests -- --nocapture
  cargo test -p yi-agent-llm openai::types::tests -- --nocapture
  ```

  Expected: PASS. These should pass once the core message shape is established; if they fail, correct only serialization incompatibilities, not the validated core history shape without updating its Task 1 tests.

- [x] **Step 4: Commit provider compatibility coverage**

  ```bash
  cd yi-agent-rs && cargo fmt --all
  cd ..
  git add yi-agent-rs/crates/yi-agent-llm/src/anthropic/types.rs yi-agent-rs/crates/yi-agent-llm/src/openai/types.rs
  git commit -m "test(llm): cover compacted tool history serialization"
  ```

## Task 6: Update project tracking and run final verification

**Files:**
- Modify: `docs/project-management/yi-agent-core.md:32`
- Modify: `docs/project-management/README.md:12`

**Interfaces:**
- Consumes: completed implementation from Tasks 1-5.
- Produces: verifiable project-management completion record.

- [x] **Step 1: Update the yi-agent-core feature criterion**

  Replace the existing auto-compact bullet with one `[x]` entry that names the new behavior and executable verification:

  ```markdown
  - [x] Codex 式 auto-compact — `compact.rs::plan_compaction` 保留最多 20,000 token 的真实 User 输入、12,000 token 的完整工具后缀并生成 handoff 摘要；`agent.rs::Session` 在 pre-turn/mid-turn 基于持久化 `Usage.input_tokens` 触发；验证：`cd yi-agent-rs && cargo test -p yi-agent-core --lib 'compact::tests|agent::tests::auto_compact_'`
  ```

  Keep its design link and add `[设计](../superpowers/specs/2026-08-15-compact-history-redesign-design.md)`.

- [x] **Step 2: Confirm project-management count remains accurate**

  This redesign replaces the already-completed auto-compact feature rather than adding a new feature. Leave `yi-agent-core | 13 / 15` unchanged in `docs/project-management/README.md`; update the row only if the preceding Task 1-5 work reveals a newly completed feature not represented in the module list.

- [x] **Step 3: Run formatting and focused regression suites**

  First check for active or stranded processes:

  ```bash
  cd yi-agent-rs
  ps aux | grep -v grep | grep -E "[c]argo|[r]ustc|yi_agent" || true
  ```

  Then run sequentially:

  ```bash
  cargo fmt --all -- --check
  cargo test -p yi-agent-core --lib compact::tests
  cargo test -p yi-agent-core --lib 'agent::tests::auto_compact_'
  cargo test -p yi-agent --bin yi-agent config::tests
  cargo test -p yi-agent-llm anthropic::types::tests
  cargo test -p yi-agent-llm openai::types::tests
  ```

  Expected: every command exits 0. Do not run `cargo test --workspace`.

- [x] **Step 4: Inspect final changes and commit tracking updates**

  ```bash
  cd ..
  git diff --check
  git diff -- docs/project-management/yi-agent-core.md docs/project-management/README.md
  git add docs/project-management/yi-agent-core.md docs/project-management/README.md
  git commit -m "docs: record compact history redesign"
  git status --short --branch
  ```

  Expected: no unstaged or untracked files after the commit.
