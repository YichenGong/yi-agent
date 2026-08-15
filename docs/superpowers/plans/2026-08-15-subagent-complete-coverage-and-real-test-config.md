# Subagent Complete Coverage and Real-Test Configuration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete deterministic subagent acceptance coverage, CLI/TUI controls, ignored real-LLM subagent E2E, and a secure Web `Real LLM Tests` configuration tab.

**Architecture:** Keep each test boundary isolated: runtime races and socket replay use `yi-agent-store`; real Git behavior uses `yi-agent-tools`; production child sandbox injection uses `yi-agent`; CLI/TUI controls remain beside their existing command and app tests. Introduce one dedicated real-test configuration resolver shared by ignored tests and the Web backend, while keeping full keys write-only and separate from ordinary runtime configuration.

**Tech Stack:** Rust 2024, `tokio`, `axum`/existing `yi-agent-web`, `serde`, `tempfile`, local Git, SQLite, Unix sockets, existing `just` recipes.

## Global Constraints

- Work only in the existing `feat/subagent-core` linked worktree.
- Default tests use mocks, temporary SQLite databases, local Unix sockets, and temporary Git repositories; they must not call a real LLM or external network.
- Ignored real-LLM tests use only `YI_AGENT_REAL_LLM_PROVIDER`, `YI_AGENT_REAL_LLM_API_URL`, `YI_AGENT_REAL_LLM_MODEL`, and `YI_AGENT_REAL_LLM_API_KEY` when explicit configuration is selected.
- If `YI_AGENT_REAL_LLM_PROVIDER` is set, missing URL, model, or key must fail and name every missing variable; no skip is permitted in this explicit mode.
- If no dedicated provider is set, use `ANTHROPIC_API_KEY`, then `OPENAI_API_KEY`; if neither exists, print a skip message and return before any network request.
- Never return, log, render, serialize, or include an API key in test diagnostics.
- The Web `Real LLM Tests` tab saves only user-level `.yi-agent/.env`/existing secret storage, never project configuration; it does not run tests or issue LLM calls.
- Run Cargo commands serially. Before every commit run `cargo fmt --all` from `yi-agent-rs/`.

---

## File structure

| File | Responsibility |
|---|---|
| Modify `yi-agent-rs/crates/yi-agent-store/tests/subagent_runtime_e2e.rs` | Concurrent cancel/review confirmation and rework lifecycle assertions. |
| Modify `yi-agent-rs/crates/yi-agent-store/tests/subagent_ipc_protocol.rs` | Subscription cursor reconnect replay assertions. |
| Modify `yi-agent-rs/crates/yi-agent-tools/tests/subagent_worktree.rs` | Parent ancestry/content delivery acceptance acceptance criterion. |
| Modify/create `yi-agent-rs/crates/yi-agent/tests/subagent_sandbox_integration.rs` | Production child factory sandbox injection tests. |
| Modify `yi-agent-rs/crates/yi-agent/src/{config.rs,main.rs,tui/app.rs,tui/slash.rs}` | CLI/TUI control test gaps and real-test config resolver. |
| Modify `yi-agent-rs/crates/yi-agent-web/src/*` | Dedicated Web config DTO/routes/tab and secret-safe persistence behavior. |
| Modify `yi-agent-rs/crates/yi-agent/tests/{common/mod.rs,e2e_real.rs}` and create `subagent_real_e2e.rs` | Shared real config resolver tests and ignored subagent E2E tests. |
| Modify `yi-agent-rs/justfile` | Dedicated real-subagent recipe and explicit-config failure propagation. |
| Modify `docs/project-management/{subagent-runtime.md,README.md}` | Completion evidence and accurate count. |

## Task 1: Add runtime race and rework E2E tests

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/tests/subagent_runtime_e2e.rs`

**Interfaces:**
- Consumes: `Daemon`, `send_request`, `IpcRequest`, `IpcResponse`, `IpcReviewDecision`, `WorkerHandle`, `Barrier`, and the committed `DeliveryFactory`.
- Produces: deterministic tests named `concurrent_cancel_confirmation_has_one_winner`, `competing_review_confirmations_have_one_winner`, and `rework_creates_one_successor_attempt_with_feedback`.

- [ ] **Step 1: Write the failing concurrent cancel test**

Create an active application child. Obtain exactly one `IpcResponse::CancelPreview { confirmation_token, .. }`; send two identical `ConfirmCancel` requests from separate threads after `Barrier::new(2).wait()`. Assert the intended behavior before implementation changes:

```rust
let responses = [first.join().unwrap(), second.join().unwrap()];
assert_eq!(responses.iter().filter(|r| matches!(r, IpcResponse::TaskCancelled)).count(), 1);
assert_eq!(responses.iter().filter(|r| matches!(r, IpcResponse::Error { .. })).count(), 1);
```

Read task events and assert one cancellation terminal event; restart daemon and assert the inspected terminal state persists.

- [ ] **Step 2: Run the new cancel race test**

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test subagent_runtime_e2e concurrent_cancel_confirmation_has_one_winner
```

Expected: compilation or assertion failure reveals the exact event kind/value returned by the current daemon. Do not loosen the one-winner assertion.

- [ ] **Step 3: Complete test helpers and apply only a test-proven atomicity fix**

Use `std::thread::scope` and separate socket clients. If both confirmations can succeed, change only the confirmation consume/transaction boundary so removal and durable terminal transition have one winner. Keep confirmation tokens opaque and single-use.

- [ ] **Step 4: Write and run competing review confirmation test**

Prepare one coding delivery with `DeliveryFactory`. Request both preview tokens before confirmation:

```rust
IpcRequest::PreviewReview { decision: IpcReviewDecision::Accept {}, .. }
IpcRequest::PreviewReview { decision: IpcReviewDecision::Reject { reason: "race".into() }, .. }
```

Barrier-synchronize confirms. Assert one `ReviewApproved` or `ReviewRejected`, one `Error`, and exactly one durable `reviews` row queried from the temporary SQLite database. Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test subagent_runtime_e2e competing_review_confirmations_have_one_winner
```

- [ ] **Step 5: Write rework successor test**

Prepare a delivery, preview/confirm `IpcReviewDecision::Rework { feedback: "replace marker".into() }`, then assert:

```rust
assert_eq!(attempt_count_after, attempt_count_before + 1);
assert!(factory.starts.lock().unwrap()[1]
    .initial_user_messages
    .iter()
    .any(|message| message.body.contains("replace marker")));
```

Confirming the original review token/delivery again must produce `IpcResponse::Error`.

- [ ] **Step 6: Verify and commit runtime E2E coverage**

Run:

```bash
cd yi-agent-rs && cargo fmt --all && cargo test -p yi-agent-store --test subagent_runtime_e2e
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc
```

```bash
git add yi-agent-rs/crates/yi-agent-store/tests/subagent_runtime_e2e.rs yi-agent-rs/crates/yi-agent-store/src/{ipc.rs,runtime.rs}
git commit -m "test: cover concurrent subagent controls"
```

## Task 2: Prove socket subscription reconnect ordering

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/tests/subagent_ipc_protocol.rs`

**Interfaces:**
- Consumes: existing raw Unix socket helpers, public `subscribe`, `Subscription::next_frame`, `IpcRequest`, and `IpcResponse::Event` frames.
- Produces: `subscription_reconnect_replays_strictly_ordered_events_without_duplicates`.

- [ ] **Step 1: Write the failing reconnect test**

Create a daemon and emit at least one durable task event. Subscribe from cursor `0`, read one event, retain `event_id`, and drop the subscription. Cause two more task events, then subscribe from the retained cursor:

```rust
let replay_ids = read_event_ids(&mut resumed, 2);
assert!(replay_ids.windows(2).all(|pair| pair[0] < pair[1]));
assert!(replay_ids.iter().all(|id| *id > saved_event_id));
assert_eq!(unique_count(&replay_ids), replay_ids.len());
```

Emit one additional event after reconnect and assert it arrives as a live frame with a larger ID.

- [ ] **Step 2: Run the reconnect test**

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test subagent_ipc_protocol subscription_reconnect_replays_strictly_ordered_events_without_duplicates
```

Expected: compile failure identifies the existing subscription frame parsing API, or assertion failure exposes replay duplication/order defects.

- [ ] **Step 3: Implement test-local finite reads; fix only proven replay defects**

Use the existing socket read timeout helper. Do not expose private IPC framing APIs. If replay returns duplicates/out-of-order IDs, fix the durable cursor query/order clause rather than filtering IDs in the client test.

- [ ] **Step 4: Verify and commit**

```bash
cd yi-agent-rs && cargo fmt --all && cargo test -p yi-agent-store --test subagent_ipc_protocol
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc
```

```bash
git add yi-agent-rs/crates/yi-agent-store/tests/subagent_ipc_protocol.rs yi-agent-rs/crates/yi-agent-store/src/{ipc.rs,repository.rs}
git commit -m "test: verify subagent subscription reconnect replay"
```

## Task 3: Add explicit parent-history Git acceptance coverage

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/tests/subagent_worktree.rs`

**Interfaces:**
- Consumes: `repository()`, `git()`, `WorktreeService::{create_child,inspect_delivery,merge_inspected_delivery}`.
- Produces: `accepted_delivery_is_visible_in_parent_history_once` and `unaccepted_delivery_leaves_parent_history_unchanged`.

- [ ] **Step 1: Write a failing parent-history test**

Create child worktree, commit `accepted.txt`, inspect and merge it. Capture parent HEAD before/after and assert:

```rust
assert_ne!(parent_after, parent_before);
assert_eq!(git(repo.path(), &["show", "HEAD:accepted.txt"]), "accepted\n");
git(repo.path(), &["merge-base", "--is-ancestor", &delivery.head_commit, "HEAD"]);
```

Call the same inspected merge again and assert error plus unchanged parent HEAD.

- [ ] **Step 2: Write unaccepted-history test and run both**

Prepare separate child delivery and inspect it without merge; record parent HEAD/content. Simulate reject/rework by not invoking merge and assert both remain unchanged. Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent-tools --test subagent_worktree accepted_delivery_is_visible_in_parent_history_once
cd yi-agent-rs && cargo test -p yi-agent-tools --test subagent_worktree unaccepted_delivery_leaves_parent_history_unchanged
```

- [ ] **Step 3: Fix only a false merge/history result, then verify and commit**

```bash
cd yi-agent-rs && cargo fmt --all && cargo test -p yi-agent-tools --test subagent_worktree
cd yi-agent-rs && cargo test -p yi-agent-tools
```

```bash
git add yi-agent-rs/crates/yi-agent-tools/tests/subagent_worktree.rs yi-agent-rs/crates/yi-agent-tools/src/worktree.rs
git commit -m "test: verify accepted subagent delivery ancestry"
```

## Task 4: Test sandbox injection through production child setup

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/tests/subagent_sandbox_integration.rs`
- Modify: `yi-agent-rs/crates/yi-agent/Cargo.toml` only if existing test dependencies are not already available.

**Interfaces:**
- Consumes: production `DaemonWorkerFactory::with_workspace(...).with_sandbox(...)`, registered `ToolRegistry`, `SandboxMode`, and temporary Git worktree paths.
- Produces: tests `workspace_write_child_cannot_escape_assigned_workspace` and `read_only_child_omits_mutating_tools`.

- [ ] **Step 1: Write failing workspace-write escape test**

Build a child factory assigned a temporary child path and `SandboxMode::WorkspaceWrite`. Obtain the same tool registry construction used by worker startup. Call `write` for `inside.txt`, `../parent.txt`, and an absolute file outside the child path:

```rust
assert!(write_inside.is_ok());
assert!(write_parent.is_err());
assert!(write_outside.is_err());
assert!(!parent_path.join("parent.txt").exists());
```

- [ ] **Step 2: Run the test to discover production construction surface**

```bash
cd yi-agent-rs && cargo test -p yi-agent --test subagent_sandbox_integration workspace_write_child_cannot_escape_assigned_workspace
```

Expected: compilation error identifies whether a test-only public builder is needed. Do not duplicate sandbox setup outside the production path.

- [ ] **Step 3: Expose the smallest production-equivalent test seam if needed**

If registry construction is private, extract a `pub(crate)` helper used by both worker startup and the test, accepting `(workspace: &Path, sandbox: SandboxMode, writable_roots: Vec<PathBuf>) -> ToolRegistry`. Do not make it public outside the crate.

- [ ] **Step 4: Add read-only tool assertion and verify**

Build the same factory with `SandboxMode::ReadOnly`; assert `registry.get("write").is_none()` or invoke the exposed write tool and assert documented denial. Verify:

```bash
cd yi-agent-rs && cargo fmt --all && cargo test -p yi-agent --test subagent_sandbox_integration
```

- [ ] **Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/tests/subagent_sandbox_integration.rs yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs yi-agent-rs/crates/yi-agent/Cargo.toml
git commit -m "test: cover subagent sandbox isolation"
```

## Task 5: Complete CLI, TUI, and slash control coverage

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/config.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/{app.rs,slash.rs}`
- Create: `yi-agent-rs/crates/yi-agent/tests/subagent_cli_controls.rs`

**Interfaces:**
- Consumes: `Cli::parse_from`, `Command::Agents`, `Command::Agent`, `DaemonAction`, `AgentAction`, `control_commands::ControlCommand`, existing TUI daemon fixture helpers.
- Produces: parser/render/control tests for all documented task-control actions and slash catalog completion.

- [ ] **Step 1: Write table-driven CLI parser tests**

In `config.rs` tests, parse each command shape:

```rust
for argv in [
    ["yi-agent", "agents"],
    ["yi-agent", "agent", "inspect", "TASK"],
    ["yi-agent", "agent", "cancel", "TASK"],
    ["yi-agent", "agent", "review", "TASK", "reject", "reason"],
    ["yi-agent", "daemon", "status"],
] { assert!(Cli::try_parse_from(argv).is_ok()); }
```

Also assert cancel/review `--yes` without confirmation takes the documented preview path rather than silently confirming.

- [ ] **Step 2: Write failing CLI-to-daemon integration tests**

Create a temporary daemon/mock task and invoke the binary command handler through its existing testable entry point or `assert_cmd` pattern already used by project tests. Assert `agents`, `inspect`, `events`, `mailbox`, and `diff` output contains expected stable IDs/state but does not contain a sentinel secret. Assert missing confirmation token yields readable error.

- [ ] **Step 3: Complete the minimal command seam and make CLI tests pass**

If `main` has no callable command function, extract `pub(crate) fn run_command(cli: Cli) -> Result<()>` from `main`, keeping `main` as a thin caller. Use this same function in tests; do not spawn a shell process merely to parse output.

- [ ] **Step 4: Add TUI/slash failure and catalog tests**

In `tui/app.rs`, exercise `/agents`, `/events`, `/mailbox`, `/diff`, `/cancel`, `/accept`, `/rework`, `/reject` with daemon fixture. Assert missing task ID/token/reason becomes a visible deterministic error. In `tui/slash.rs`, assert every `ControlCommand::all()` entry maps to a slash name and completion candidate.

- [ ] **Step 5: Verify and commit control coverage**

```bash
cd yi-agent-rs && cargo fmt --all && cargo test -p yi-agent --bin yi-agent
cd yi-agent-rs && cargo test -p yi-agent --test subagent_cli_controls
```

```bash
git add yi-agent-rs/crates/yi-agent/src/{config.rs,main.rs,tui/app.rs,tui/slash.rs,control_commands.rs} yi-agent-rs/crates/yi-agent/tests/subagent_cli_controls.rs
git commit -m "test: cover subagent CLI and TUI controls"
```

## Task 6: Implement secure real-test config and Web tab

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/tests/common/mod.rs`
- Modify: `yi-agent-rs/crates/yi-agent/Cargo.toml`
- Modify: `yi-agent-rs/crates/yi-agent-web/src/lib.rs` and existing route/template/static files discovered during implementation
- Create: `yi-agent-rs/crates/yi-agent-web/tests/real_llm_test_config.rs`

**Interfaces:**
- Produces: `RealLlmTestConfig { provider: String, api_url: String, model: String, api_key: SecretString }`, `resolve_real_llm_test_config() -> Result<Option<RealLlmTestConfig>, RealLlmTestConfigError>`, and Web DTOs that expose `api_key_configured: bool` but never `api_key`.

- [ ] **Step 1: Write resolver failure and fallback tests**

Add process-isolated env tests that assert explicit provider with missing URL/model/key returns one error containing each missing variable:

```rust
assert!(error.to_string().contains("YI_AGENT_REAL_LLM_API_URL"));
assert!(error.to_string().contains("YI_AGENT_REAL_LLM_MODEL"));
assert!(error.to_string().contains("YI_AGENT_REAL_LLM_API_KEY"));
```

Add fallback tests for Anthropic first, OpenAI second, and no-key `Ok(None)` skip outcome. Assert debug/display output does not contain the supplied sentinel key.

- [ ] **Step 2: Run resolver tests before implementation**

```bash
cd yi-agent-rs && cargo test -p yi-agent --test e2e_real real_llm_config
```

Expected: compile failure until shared resolver/helper exists.

- [ ] **Step 3: Implement resolver and update existing real harness**

Implement dedicated-env precedence in `tests/common/mod.rs` or a test support module. Preserve no-key skip behavior. Use `secrecy::SecretString` or an existing equivalent; add a dependency only if absent. Do not route dedicated values through ordinary `Config::load` environment names.

- [ ] **Step 4: Write failing Web API tests**

Add Web integration tests for:

```rust
GET /api/real-llm-test-config
PUT /api/real-llm-test-config
POST /api/real-llm-test-config/validate
DELETE /api/real-llm-test-config/api-key
```

Assert read JSON has `api_key_configured` and lacks the sentinel key and an `api_key` field. Assert blank save retains existing key; delete requires explicit confirmation and clears it; invalid provider, non-HTTP URL, blank model, and absent key fail validation without a network call.

- [ ] **Step 5: Implement Web DTO/routes/tab with secret-safe persistence**

Follow existing `yi-agent-web` config persistence endpoints. Store only `YI_AGENT_REAL_LLM_*` in user-level env. Add peer `Real LLM Tests` tab with provider select, URL/model inputs, password key input, Save, Validate, and explicit Clear API Key confirmation. Do not add test execution UI.

- [ ] **Step 6: Verify resolver/Web tests and commit**

```bash
cd yi-agent-rs && cargo fmt --all && cargo test -p yi-agent --test e2e_real real_llm_config
cd yi-agent-rs && cargo test -p yi-agent-web --test real_llm_test_config
```

```bash
git add yi-agent-rs/crates/yi-agent/tests/common/mod.rs yi-agent-rs/crates/yi-agent/tests/e2e_real.rs yi-agent-rs/crates/yi-agent/Cargo.toml yi-agent-rs/crates/yi-agent-web
git commit -m "feat: configure real LLM regression tests"
```

## Task 7: Add ignored real-LLM subagent E2E and recipe

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/tests/subagent_real_e2e.rs`
- Modify: `yi-agent-rs/crates/yi-agent/tests/common/mod.rs`
- Modify: `yi-agent-rs/justfile`

**Interfaces:**
- Consumes: `resolve_real_llm_test_config`, existing `run_agent_with_timeout`, temporary Git repository helper, `yi-agent run`, and `#[ignore]` test gate.
- Produces: ignored tests `real_subagent_reports_readme_without_changes`, `real_subagent_delivery_is_accepted_into_parent`, and `real_subagent_rework_accepts_corrected_marker`; recipe `just test-real-subagent`.

- [ ] **Step 1: Write ignored README delegation E2E**

Create temporary Git repo with `README.md`, run parent prompt requiring exactly one subagent to inspect it and return a report, use 300-second timeout, then assert output has a terminal result and `git status --porcelain` is empty. At the test start:

```rust
let Some(config) = resolve_real_llm_test_config()? else {
    eprintln!("SKIPPED: no real LLM API key configured");
    return Ok(());
};
```

- [ ] **Step 2: Run ignored test with no configuration**

Run in a shell with all dedicated and fallback key variables removed:

```bash
cd yi-agent-rs && env -u YI_AGENT_REAL_LLM_PROVIDER -u YI_AGENT_REAL_LLM_API_KEY -u ANTHROPIC_API_KEY -u OPENAI_API_KEY cargo test -p yi-agent --test subagent_real_e2e -- --ignored
```

Expected: test returns success after one skip message and makes no network call.

- [ ] **Step 3: Add delivery and rework structural tests**

Delivery test prompt requires child create `subagent-delivery.txt`, commit, and present delivery; parent accepts. Assert file content and `git merge-base --is-ancestor` after run. Rework test starts with marker `WRONG`, requires replacement with `CORRECT`, and asserts final parent file contains `CORRECT` but not `WRONG`. Never assert exact prose.

- [ ] **Step 4: Add recipe with explicit configuration semantics**

Add:

```make
# Runs ignored real subagent E2E; resolver skips only when no dedicated or fallback key exists.
test-real-subagent:
    cargo test -p yi-agent --test subagent_real_e2e -- --ignored
```

Do not `unset YI_AGENT_REAL_LLM_*`. Let the resolver return an error for incomplete explicit configuration.

- [ ] **Step 5: Verify no-key skip, explicit-missing failure, and commit**

Run:

```bash
cd yi-agent-rs && env -u YI_AGENT_REAL_LLM_PROVIDER -u YI_AGENT_REAL_LLM_API_KEY -u ANTHROPIC_API_KEY -u OPENAI_API_KEY just test-real-subagent
cd yi-agent-rs && YI_AGENT_REAL_LLM_PROVIDER=anthropic env -u YI_AGENT_REAL_LLM_API_URL -u YI_AGENT_REAL_LLM_MODEL -u YI_AGENT_REAL_LLM_API_KEY just test-real-subagent; test $? -ne 0
```

```bash
git add yi-agent-rs/crates/yi-agent/tests/{common/mod.rs,subagent_real_e2e.rs} yi-agent-rs/justfile
git commit -m "test: add real LLM subagent regression suite"
```

## Task 8: Update tracking and run full serial gate

**Files:**
- Modify: `docs/project-management/subagent-runtime.md`
- Modify: `docs/project-management/README.md`

**Interfaces:**
- Consumes: passed verification evidence from Tasks 1-7.
- Produces: completed CLI/TUI/slash row, real-LLM subagent row, exact commands, and matching count.

- [ ] **Step 1: Update completed criteria with exact commands**

Replace the unchecked CLI/TUI/Slash row only after Task 5 passes, with a `[x]` entry naming `cargo test -p yi-agent --bin yi-agent` and `cargo test -p yi-agent --test subagent_cli_controls`. Add a separate `[x]` real-LLM subagent row naming `just test-real-subagent` and `#[ignore]` gate. Add deterministic race/reconnect/sandbox/Git commands to the existing hardening evidence.

- [ ] **Step 2: Recalculate module count from literal feature rows**

Count only `- [x]` and `- [-]` as completed in `subagent-runtime.md`. Update the `subagent-runtime` row in `docs/project-management/README.md` to exactly `completed / total`; do not use an estimated count.

- [ ] **Step 3: Run complete deterministic serial verification**

```bash
cd yi-agent-rs && cargo test -p yi-agent-core
cd yi-agent-rs && cargo test -p yi-agent-tools
cd yi-agent-rs && cargo test -p yi-agent-store
cd yi-agent-rs && cargo test -p yi-agent
cd yi-agent-rs && cargo fmt --all -- --check
cd yi-agent-rs && cargo clippy -p yi-agent-core -p yi-agent-tools -p yi-agent-store -p yi-agent -- -D warnings
git diff --check main...HEAD
git status --short --branch
```

Expected: every deterministic command exits 0 and status is clean on `feat/subagent-core`.

- [ ] **Step 4: Run real recipe configuration gates**

Run the no-key skip and explicit-missing commands from Task 7 again. Run a real provider only when a user has intentionally supplied a valid dedicated configuration; record provider/model but never key in the final report.

- [ ] **Step 5: Commit tracking update**

```bash
git add docs/project-management/subagent-runtime.md docs/project-management/README.md
git commit -m "docs: record complete subagent test coverage"
```
