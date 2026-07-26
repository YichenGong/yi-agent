# Agent Completion Semantics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:executing-plans` task-by-task. Steps use checkbox syntax for tracking.

**Goal:** Prevent interrupted model output and unverified file changes from being reported as successful task completion.

**Architecture:** Preserve provider stop status through `accumulate_stream`; only an explicit provider `EndTurn` can be a normal completion. The loop injects one bounded continuation after token truncation and one bounded verification audit after a successful mutating tool. Base system instructions always precede optional user instructions.

**Tech Stack:** Rust, Tokio, futures streams, tracing, clap headless CLI, scripted provider tests.

---

### Task 1: Preserve provider terminal status

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/provider.rs`

- [ ] Add `accumulate_stream_eof_without_stop_is_abnormal`: a stream with a text delta and no Stop must return `Other("stream ended without stop")`.
- [ ] Run `cargo test -p yi-agent-core --lib provider::tests::accumulate_stream_eof_without_stop_is_abnormal -- --exact`; it must fail because EOF currently defaults to `EndTurn`.
- [ ] Add `received_stop` to `accumulate_stream`, set it on `ProviderEvent::Stop`, and map clean EOF without it to the stable `Other` reason.
- [ ] Run `cargo test -p yi-agent-core --lib provider::tests`, then commit `fix: preserve abnormal provider stream endings`.

### Task 2: Handle interrupted agent turns explicitly

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs`

- [ ] Add failing scripted-provider tests: `MaxTokens` then normal text must make two provider requests; `Other("idle timeout")` must not emit `Done(EndTurn)`.
- [ ] Run the targeted tests and confirm they fail on the current unconditional no-tool completion path.
- [ ] Add `DoneReason::Interrupted { reason: String }` and `CONTINUE_AFTER_TRUNCATION`. After session persistence, branch on stop reason: normal end continues to tool/completion logic; max tokens appends one controller user message and loops; stop sequence and other reasons emit `Interrupted` and return.
- [ ] Run `cargo test -p yi-agent-core --lib agent::tests`; commit `fix: distinguish interrupted agent completion`.

### Task 3: Require one post-mutation audit

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs`

- [ ] Add a failing test with a non-read-only test tool, a text-only end attempt, a controller audit, a read-only verification tool, and normal final text. Assert the audit content is present in request history.
- [ ] Run the new test and confirm it fails because the first text-only response ends the loop.
- [ ] Track `verification_pending` and `audit_attempted` in `run_loop`. A successful non-read-only tool sets pending; a successful read-only tool clears it. At an explicit text-only end, append `COMPLETION_AUDIT_PROMPT` once when pending, then loop.
- [ ] Add a bound test showing two consecutive text-only endings cause only one audit. Run `cargo test -p yi-agent-core --lib agent::tests`; commit `fix: audit file changes before task completion`.

### Task 4: Compose instructions and expose interrupted CLI exit

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`

- [ ] Add failing tests proving a custom system prompt retains default execution instructions and `DoneReason::Interrupted` returns human-mode exit code 1.
- [ ] Run the focused tests and confirm they fail.
- [ ] Update the default prompt to favor `write`/`edit` for file changes and require reread or relevant checks. Change prompt resolution to append custom content under `User-provided instructions:`. Print interruption diagnostics and set exit code 1 in human drainer.
- [ ] Run `cargo test -p yi-agent --lib`; commit `fix: retain task instructions and surface interruptions`.

### Task 5: Upgrade completion assertions and project records

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/tests/common/mod.rs`
- Modify: `yi-agent-rs/crates/yi-agent/tests/e2e_complex.rs`
- Modify: `docs/project-management/yi-agent-core.md`
- Modify: `docs/project-management/yi-agent-run.md`

- [ ] Add helpers and tests that require `Done(EndTurn)` rather than any Done event, and detect a verification call after the final mutating call.
- [ ] Run the helper tests and confirm they fail before implementation.
- [ ] Require normal end and write-after-verification evidence in every complex E2E case; update project-management acceptance criteria with core and CLI commands.
- [ ] Run `cargo test -p yi-agent-core && cargo test -p yi-agent`; commit `test: require verified completion in complex agent runs`.

### Task 6: Verify the branch

**Files:**
- Verify: all changed files

- [ ] Run `cargo fmt --all`.
- [ ] Run `cargo test -p yi-agent-core && cargo test -p yi-agent`.
- [ ] Run `cargo clippy -p yi-agent-core -p yi-agent -- -D warnings`.
- [ ] Run `git diff --check main...HEAD` and inspect `git status --short` plus `git log --oneline main..HEAD`.
