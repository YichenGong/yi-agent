# Headless Subagent Runtime and Real Workflow Regressions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add opt-in headless subagent delegation and use it to run ignored real-provider delivery, rework, and concurrent-worktree workflow regressions under a local authorized-review harness.

**Architecture:** `yi-agent run --subagents` reuses the TUI's production daemon worker factory and application-root IPC lifecycle, while default headless runs retain their existing builtin-only registry. The flag adds only `spawn_agent`, `wait_agent`, and `send_message`; review controls remain unavailable to models. Each ignored real test creates one independent baseline Git repository and runtime directory under `TempDir`, runs the headless root with `--subagents`, and uses the corresponding daemon socket as the local-user harness to preview and confirm accept/rework actions.

**Tech Stack:** Rust 2024, Clap, `yi-agent-store` local Unix-socket IPC/SQLite daemon, existing production worker factory and tool registry, `tempfile`, local Git, existing ignored real-test resolver and `just` recipe.

## Global Constraints

- Work only in the existing `feat/subagent-core` linked worktree; never modify `main` directly.
- `--subagents` is an explicit opt-in. Without it, `yi-agent run` must not start, connect to, attach to, activate, detach, or stop a daemon, and must not expose `spawn_agent`, `wait_agent`, or `send_message`.
- With `--subagents`, use production `build_daemon_worker_factory`, `AttachApplicationRoot`, `ActivateApplicationRoot`, and `DetachApplicationRoot`; stop only a daemon created by the invoking headless process.
- Model-visible tools are exactly ordinary headless tools plus `spawn_agent`, `wait_agent`, and `send_message`. Do not expose accept, reject, rework, raw IPC, confirmation tokens, or user-review authority as tools.
- Test-harness review uses only `IpcRequest::PreviewReview` followed by `IpcRequest::ConfirmReview` with the returned single-use token. Never call a hidden direct acceptance path.
- Every ignored real workflow test uses a new `TempDir` containing a baseline Git commit, `runtime/` socket/lock directory, SQLite state file, and child worktrees. It sets `YI_AGENT_RUNTIME_DIR` to that test-local runtime directory and removes all artifacts on drop.
- Apply `RealLlmTestConfig` only to the spawned headless command. Never print, serialize, commit, or log credentials.
- All real tests remain `#[ignore]`; `just test-real-subagent` runs them serially using `--test-threads=1`. Normal test and CI execution makes no network request.
- Real agent commands use the existing 300-second owned-child timeout. Cargo commands run serially.
- Before each commit run `cd yi-agent-rs && cargo fmt --all`; update project-management evidence and the README literal count in the same feature commit.

---

## File structure

| File | Responsibility |
|---|---|
| Modify `yi-agent-rs/crates/yi-agent/src/config.rs` | Add `Run { subagents: bool }` Clap flag and parser tests. |
| Modify `yi-agent-rs/crates/yi-agent/src/main.rs` | Attach/detach opt-in headless runtime and rebuild the root tool registry. |
| Modify `yi-agent-rs/crates/yi-agent/tests/subagent_cli_controls.rs` | Black-box no-network CLI coverage for default/opt-in delegation behavior. |
| Modify `yi-agent-rs/crates/yi-agent/tests/subagent_real_e2e.rs` | Reuse committed Git fixture helpers; add test-local runtime, IPC review, and ignored workflow tests. |
| Modify `yi-agent-rs/justfile` | Document serial execution of all real subagent scenarios. |
| Modify `docs/project-management/subagent-runtime.md` | Record headless opt-in and all four real workflow acceptance criteria. |
| Modify `docs/project-management/README.md` | Recalculate the literal completed/total index after feature-row changes. |

## Task 1: Parse the explicit headless delegation flag

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/config.rs:104-121,1216-1247`

**Interfaces:**
- Produces: `Command::Run { prompt: Option<String>, json: bool, stdin: bool, naked: bool, subagents: bool }`.
- Consumed by: `main()` and `run_headless(..., subagents: bool)` in Task 2.

- [ ] **Step 1: Write parser tests before changing the command struct**

Add two tests beside `cli_parses_run_naked_flag`:

```rust
#[test]
fn cli_parses_run_subagents_flag() {
    let cli = Cli::parse_from(["yi-agent", "run", "--subagents", "delegate"]);
    let Some(Command::Run { subagents, .. }) = cli.command else {
        panic!("expected run command");
    };
    assert!(subagents);
}

#[test]
fn cli_defaults_run_subagents_to_false() {
    let cli = Cli::parse_from(["yi-agent", "run", "ordinary"]);
    let Some(Command::Run { subagents, .. }) = cli.command else {
        panic!("expected run command");
    };
    assert!(!subagents);
}
```

- [ ] **Step 2: Run the parser tests and verify RED**

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent cli_parses_run_subagents_flag
```

Expected: compilation fails because `Command::Run` has no `subagents` field.

- [ ] **Step 3: Add only the Clap field and thread it through `main()`**

Add the following to `Command::Run`, adjacent to `naked`, and destructure/pass it at `main.rs:46-53`:

```rust
/// Enable local subagent delegation for this headless run.
#[arg(long)]
subagents: bool,
```

Do not start a daemon or alter tool setup in this task.

- [ ] **Step 4: Verify GREEN and commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent cli_parses_run_subagents_flag
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent cli_defaults_run_subagents_to_false
```

```bash
git add yi-agent-rs/crates/yi-agent/src/config.rs yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "feat: add headless subagent opt-in flag"
```

## Task 2: Attach an opt-in headless application root

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:566-677,931-1067`
- Modify: `yi-agent-rs/crates/yi-agent/tests/subagent_cli_controls.rs`

**Interfaces:**
- Consumes: `runtime_directory`, `build_daemon_worker_factory`, `build_tui_root_tools`, `AttachedRoot`, `Daemon::start_with_factory`, `IpcRequest::{AttachApplicationRoot,ActivateApplicationRoot,DetachApplicationRoot}`.
- Produces: `HeadlessRuntimeSession { socket_path: PathBuf, attached_root: AttachedRoot, embedded_daemon: Option<Daemon> }`, `attach_headless_runtime(&Cli, &Config) -> Result<HeadlessRuntimeSession>`, and `run_headless(..., subagents: bool)` behavior.

- [ ] **Step 1: Write deterministic tests for the capability boundary**

Add testable helpers in `main.rs` and black-box control tests in `subagent_cli_controls.rs`. The unit test must prove the registry difference without a provider request:

```rust
#[test]
fn headless_root_tools_include_delegation_only_when_attached() {
    let ordinary = build_headless_setup(&test_config(), false).unwrap().tools;
    assert!(ordinary.get("spawn_agent").is_none());

    let attached = attached_root_for_main_tests();
    let delegated = build_headless_root_tools(&test_config(), &attached, PathBuf::from("/tmp/runtime.sock"));
    let names = delegated.schemas().into_iter().map(|schema| schema.name).collect::<Vec<_>>();
    assert!(names.contains(&"spawn_agent".into()));
    assert!(names.contains(&"wait_agent".into()));
    assert!(names.contains(&"send_message".into()));
    assert!(!names.contains(&"accept_review".into()));
}
```

Add a black-box test that runs `yi-agent run --help` and asserts `--subagents` appears. Do not send a real prompt in this task.

- [ ] **Step 2: Run tests and verify RED**

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent headless_root_tools_include_delegation_only_when_attached
cd yi-agent-rs && cargo test -p yi-agent --test subagent_cli_controls
```

Expected: compilation failure for `build_headless_root_tools` or assertion failure because headless lacks the attached-root registry builder.

- [ ] **Step 3: Implement the smallest shared headless lifecycle**

Extract or reuse a registry helper that starts from `build_headless_setup(config, false)`, clones its `ToolRegistry`, and calls:

```rust
crate::tui::subagents::register_attached_root_tools(
    &mut registry,
    runtime.socket_path.clone(),
    &runtime.attached_root,
);
```

`attach_headless_runtime` must mirror `attach_tui_runtime` but return `Result`, not a fallback `None`: if `--subagents` was explicitly requested and daemon start/attach/activate fails, return an error and do not run an ordinary agent. Use a distinct idempotency key prefix such as `headless:<pid>:<uuid>`.

In `run_headless`, when `subagents` is false keep `build_headless_setup` unchanged. When true: attach runtime, use `attached_root.workspace.path` as the root tool workspace, activate with `prompt_text`, build delegated tools, drain the stream, then call `DetachApplicationRoot`. Preserve the `HeadlessRuntimeSession` until after detach so its `embedded_daemon` drops/stops only if this invocation started it.

- [ ] **Step 4: Add failure-path assertion and verify GREEN**

With an invalid/locked test runtime directory, invoke `run --subagents` through an extracted fallible setup helper and assert its error contains `subagent runtime` or `attach`; assert it does not fall back to an ordinary tool registry. Run:

```bash
cd yi-agent-rs && cargo fmt --all
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent headless_root_tools_include_delegation_only_when_attached
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent headless_subagents_setup_failure_does_not_fall_back
cd yi-agent-rs && cargo test -p yi-agent --test subagent_cli_controls
```

- [ ] **Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/main.rs yi-agent-rs/crates/yi-agent/tests/subagent_cli_controls.rs
git commit -m "feat: enable headless subagent runtime"
```

## Task 3: Add test-local daemon and review helpers

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/tests/subagent_real_e2e.rs`

**Interfaces:**
- Consumes: committed `temporary_repository`, `git`, `run_command_with_timeout`, `RealLlmTestConfig`; `yi_agent_store::ipc::{send_request,Daemon,IpcRequest,IpcResponse,IpcReviewDecision}`.
- Produces: `RealRuntimeFixture { repository: TempDir, runtime_dir: PathBuf, daemon: Daemon }`, `run_real_subagent(..., runtime_dir: &Path, prompt: &str) -> Output`, `await_review(socket: &Path, task_id: &str) -> IpcTaskDetail`, `confirm_review(socket: &Path, task_id: &str, decision: IpcReviewDecision)`.

- [ ] **Step 1: Write a non-ignored local IPC test first**

Create a fixture that starts `Daemon::start` under `repository.path().join("runtime")` and proves its socket is private and isolated from the parent environment. Add a unit test that issues `IpcRequest::Status` to `fixture.daemon.socket_path()` and asserts `IpcResponse::Status { .. }`. Assert `fixture.runtime_dir.starts_with(fixture.repository.path())`.

- [ ] **Step 2: Run and verify RED**

```bash
cd yi-agent-rs && cargo test -p yi-agent --test subagent_real_e2e local_runtime_fixture_uses_a_tempdir_socket
```

Expected: compilation failure until `RealRuntimeFixture` exists.

- [ ] **Step 3: Implement fixture and review helpers**

Start the daemon with the production worker factory only after Task 2 exposes a testable shared factory constructor; if the constructor is private, make the narrowest `pub(crate)` helper used by both headless/TUI setup and the test binary. Do not use `Daemon::start` with `UnavailableWorkerFactory` for real workflows.

`await_review` must poll `InspectTask` only until `detail.state == "awaiting_parent_review"`, bounded by the existing 300-second deadline. `confirm_review` must first require `IpcResponse::ReviewPreview { confirmation_token, .. }`, then send the exact same decision/token in `ConfirmReview`, and assert `ReviewApproved` or `ReviewReworkRequested` by decision. Its test diagnostics must never include process environment values.

Modify `run_real_subagent` to add:

```rust
command
    .arg("--workdir").arg(repository)
    .arg("run").arg("--subagents")
    .arg(prompt)
    .env("YI_AGENT_RUNTIME_DIR", runtime_dir);
```

- [ ] **Step 4: Verify GREEN and commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd yi-agent-rs && cargo test -p yi-agent --test subagent_real_e2e local_runtime_fixture_uses_a_tempdir_socket
```

```bash
git add yi-agent-rs/crates/yi-agent/tests/subagent_real_e2e.rs yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "test: prepare local real subagent review harness"
```

## Task 4: Add real accepted-delivery ancestry regression

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/tests/subagent_real_e2e.rs`

**Interfaces:**
- Consumes: Task 3 fixture, `await_review`, `confirm_review`, and `IpcReviewDecision::Accept {}`.
- Produces: `#[ignore] fn real_subagent_accepts_delivery_into_parent_history()`.

- [ ] **Step 1: Write the ignored test with exact structural assertions**

Use `RealRuntimeFixture`. The prompt requires the root to call `spawn_agent` exactly once, requires the child to create and commit `real-subagent-delivery.txt` containing `REAL_SUBAGENT_DELIVERY_MARKER_V1\n`, and requires root to call `wait_agent`. It must not tell the root to review/accept.

After the root process completes, identify the only child from `ListTaskSummaries { session_id: None, active_only: false }`, wait for its review state, capture `detail.delivery_json` and its child `head_commit`, then have the harness accept with preview/confirm. Assert:

```rust
assert_eq!(
    std::fs::read_to_string(repository.path().join("real-subagent-delivery.txt")).unwrap(),
    "REAL_SUBAGENT_DELIVERY_MARKER_V1\n"
);
git(repository.path(), &["merge-base", "--is-ancestor", child_head, "HEAD"]);
assert!(git(repository.path(), &["status", "--porcelain"]).is_empty());
```

- [ ] **Step 2: Run to verify RED, then correct only evidence extraction/prompt wording**

```bash
cd yi-agent-rs && cargo test -p yi-agent --test subagent_real_e2e real_subagent_accepts_delivery_into_parent_history -- --ignored --test-threads=1
```

Expected before code: no matching test. Once present, preserve the child task, delivery state, marker, and ancestry assertions. If model behavior fails, make the task wording more explicit; do not loosen to prose checks or alter review authority.

- [ ] **Step 3: Verify GREEN and commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd yi-agent-rs && cargo test -p yi-agent --test subagent_real_e2e real_subagent_accepts_delivery_into_parent_history -- --ignored --test-threads=1
```

```bash
git add yi-agent-rs/crates/yi-agent/tests/subagent_real_e2e.rs
git commit -m "test: verify real subagent delivery acceptance"
```

## Task 5: Add real rework correction and concurrent-isolation regressions

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/tests/subagent_real_e2e.rs`

**Interfaces:**
- Consumes: Task 3 review helpers and Task 4 child-delivery inspection.
- Produces: `#[ignore] fn real_subagent_rework_replaces_incorrect_marker()` and `#[ignore] fn real_subagents_concurrently_deliver_isolated_worktrees()`.

- [ ] **Step 1: Write the rework test**

Prompt the real root to delegate one child that writes and commits `real-subagent-rework.txt` with exactly `REAL_SUBAGENT_WRONG_MARKER_V1\n`, then waits. Harness waits for first review and calls preview/confirm with:

```rust
IpcReviewDecision::Rework {
    feedback: "Replace the entire file content with exactly REAL_SUBAGENT_CORRECT_MARKER_V1 followed by one newline; commit and deliver the corrected result.".into(),
}
```

Poll the same child task until its successor delivery reaches `awaiting_parent_review`; accept that delivery through preview/confirm. Assert final parent content is exactly `REAL_SUBAGENT_CORRECT_MARKER_V1\n`, lacks the wrong marker, successor head is an ancestor of `HEAD`, and `git log --all -- real-subagent-rework.txt` has at least two commits.

- [ ] **Step 2: Run the rework test and verify RED/GREEN**

```bash
cd yi-agent-rs && cargo test -p yi-agent --test subagent_real_e2e real_subagent_rework_replaces_incorrect_marker -- --ignored --test-threads=1
```

Before the test exists, expected result is no matching test. After implementation, a direct root rewrite, acceptance of the initial delivery, or absent successor delivery is failure; preserve those checks.

- [ ] **Step 3: Write the concurrency/isolation test**

Prompt the root to call `spawn_agent` twice before `wait_agent`, with exact independent targets: `real-child-alpha.txt` / `REAL_CHILD_ALPHA_MARKER_V1\n` and `real-child-beta.txt` / `REAL_CHILD_BETA_MARKER_V1\n`. The harness identifies two direct children, waits for both reviews, and preview/confirms accept for each. Assert both exact files are at parent `HEAD`; delivery head commits differ; each is an ancestor of `HEAD`; the child task delivery evidence has distinct `workspace_lease_id` values; and `git branch --all` shows at least two `yi-agent-` child branches.

- [ ] **Step 4: Run concurrency test and verify GREEN**

```bash
cd yi-agent-rs && cargo test -p yi-agent --test subagent_real_e2e real_subagents_concurrently_deliver_isolated_worktrees -- --ignored --test-threads=1
```

Do not add sleeps beyond bounded state polling and do not run multiple Cargo tests concurrently.

- [ ] **Step 5: Commit the workflow tests**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent/tests/subagent_real_e2e.rs
git commit -m "test: verify real subagent rework and isolation"
```

## Task 6: Document, gate, and verify all scenarios

**Files:**
- Modify: `yi-agent-rs/justfile:94-100`
- Modify: `docs/project-management/subagent-runtime.md`
- Modify: `docs/project-management/README.md`

**Interfaces:**
- Consumes: one default-disabled headless path, one opt-in headless runtime, and four ignored real tests.
- Produces: an accurate serial `test-real-subagent` recipe and project-management criteria.

- [ ] **Step 1: Update the recipe and tracking criteria**

Retain the serial recipe command:

```make
cargo test -p yi-agent --test subagent_real_e2e -- --ignored --test-threads=1
```

Update its comment and `subagent-runtime.md` to name: `--subagents`; default-disabled behavior; README smoke; delivery accept/ancestry; rework successor correction; concurrent worktree isolation; local harness preview/confirm; test-local runtime dirs; no-key successful skip; and incomplete explicit-config failure. Count all literal `- [x]` and `- [-]` rows before updating the module count in `README.md`.

- [ ] **Step 2: Verify default remains network-free and delegation remains ignored**

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent
cd yi-agent-rs && cargo test -p yi-agent --test subagent_cli_controls
cd yi-agent-rs && cargo test -p yi-agent --test subagent_real_e2e
```

Expected: deterministic tests pass; four provider workflows are ignored; no provider request happens.

- [ ] **Step 3: Verify real configuration gates and full suite**

```bash
cd yi-agent-rs && env -i PATH="$PATH" HOME="$(mktemp -d)" cargo test -p yi-agent --test subagent_real_e2e -- --ignored --test-threads=1
cd yi-agent-rs && env -i PATH="$PATH" HOME="$(mktemp -d)" YI_AGENT_REAL_LLM_PROVIDER=anthropic cargo test -p yi-agent --test subagent_real_e2e -- --ignored --test-threads=1; test $? -ne 0
cd yi-agent-rs && just test-real-subagent
```

Expected: first command skips with no provider request; second fails naming missing explicit variables without key output; the configured command runs all four serially with zero failures. If a real workflow fails, retain the structural assertion and return to its owning task.

- [ ] **Step 4: Run final serial quality gate and commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd yi-agent-rs && cargo test -p yi-agent
cd yi-agent-rs && cargo clippy -p yi-agent --all-targets -- -D warnings
cd .. && git diff --check main...HEAD
git status --short --branch
```

```bash
git add yi-agent-rs/justfile docs/project-management/subagent-runtime.md docs/project-management/README.md
git commit -m "docs: record real subagent workflow coverage"
```
