# Subagent Runtime Project Isolation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ensure each application root attached to the shared daemon creates its worktree hierarchy in the invoking project's repository.

**Architecture:** Add the caller workspace to the application-root attach IPC request. The runtime asks the application worker factory for a session-specific workspace service and retains it per root session; the CLI factory constructs that service from the caller workspace. Root and child worktree preparation use the session's service instead of the daemon startup workspace.

**Tech Stack:** Rust, serde JSON IPC over Unix sockets, Tokio, SQLite-backed runtime coordinator, Cargo tests.

## Global Constraints

- The runtime socket/database remain user-local under `~/.yi-agent/runtime`.
- `yi-agent-store` must not depend on the `yi-agent` binary crate.
- The attach workspace is the resolved CLI `Config.workdir`; never derive it from the daemon process CWD.
- Root and child worktrees must remain in the repository of the project that attached the root.
- A reattach with a different project must fail rather than expose another project's root session.

---

## File Structure

- Modify `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs`: add the application factory boundary for creating a workspace service from an attaching client workspace.
- Modify `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`: require and forward `workspace` on `AttachApplicationRoot`.
- Modify `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`: bind a workspace service to each attached root and select it for root/child preparation.
- Modify `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`: construct `DaemonWorkspaceService` per requested caller workspace.
- Modify `yi-agent-rs/crates/yi-agent/src/main.rs`: include `config.workdir` in headless and TUI attach requests.
- Modify `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`: prove two attached projects use distinct root/child workspace services.
- Modify `yi-agent-rs/crates/yi-agent/src/main.rs` unit tests: prove request construction uses configured workdir.
- Create `docs/superpowers/specs/2026-08-16-subagent-runtime-project-isolation-design.md`: approved design record.

### Task 1: Test Cross-Project Workspace Assignment

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`

**Interfaces:**
- Consumes: `IpcRequest::AttachApplicationRoot { idempotency_key, workspace }`.
- Produces: a regression test that observes the factory workspace selected for two roots and a child.

- [ ] **Step 1: Write the failing test**

Add a `ProjectWorkspaceFactory` whose `workspace_service_for_application_root(&Path)` records the requested path and returns a service whose `prepare_root` and `prepare_child` put that path into `WorkerWorkspace.repository_root`. Attach `/tmp/project-a`, then `/tmp/project-b`, spawn a B child, and assert:

```rust
assert_eq!(first.workspace.repository_root, PathBuf::from("/tmp/project-a"));
assert_eq!(second.workspace.repository_root, PathBuf::from("/tmp/project-b"));
assert_eq!(child.workspace.repository_root, PathBuf::from("/tmp/project-b"));
```

- [ ] **Step 2: Run the test to verify it fails**

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc application_roots_use_their_attaching_project_workspace -- --exact
```

Expected: compilation failure because `AttachApplicationRoot` has no `workspace` field and the factory boundary does not exist.

- [ ] **Step 3: Implement the minimal core/store interface**

Add this default method to `AgentWorkerFactory`:

```rust
fn workspace_service_for_application_root(
    &self,
    _workspace: &std::path::Path,
) -> Option<Arc<dyn AgentWorkspaceService>> {
    self.workspace_service()
}
```

Add `workspace: PathBuf` to `IpcRequest::AttachApplicationRoot`. Thread it to `RuntimeCoordinator::attach_application_root`. Store a session-to-service mapping alongside supervisors, bind the returned service before root preparation, and use the root session's bound service in `prepare_task_workspace` for both root and descendants. For an idempotent attachment, reject a requested service whose root repository differs from the persisted root workspace repository.

- [ ] **Step 4: Run the regression test to verify it passes**

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc application_roots_use_their_attaching_project_workspace -- --exact
```

Expected: PASS.

- [ ] **Step 5: Commit the tested runtime boundary**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs yi-agent-rs/crates/yi-agent-store/src/ipc.rs yi-agent-rs/crates/yi-agent-store/src/runtime.rs yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs
git commit -m "fix: isolate application roots by project workspace"
```

### Task 2: Wire the CLI Factory and Attach Clients

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`

**Interfaces:**
- Consumes: `AgentWorkerFactory::workspace_service_for_application_root(&Path)` and `IpcRequest::AttachApplicationRoot { workspace }`.
- Produces: daemon root worktree services based on the attaching client's `config.workdir`.

- [ ] **Step 1: Write failing CLI-focused tests**

Add a `main.rs` unit test for an extracted request helper:

```rust
assert_eq!(
    attach_application_root_request("test-key", PathBuf::from("/projects/b")),
    IpcRequest::AttachApplicationRoot {
        idempotency_key: "test-key".into(),
        workspace: PathBuf::from("/projects/b"),
    }
);
```

- [ ] **Step 2: Run the test to verify it fails**

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent attach_application_root_request_uses_the_configured_workdir -- --exact
```

Expected: compilation failure because the helper and request workspace field do not yet exist.

- [ ] **Step 3: Implement the minimal CLI wiring**

Override the factory method in `DaemonAgentWorkerFactory`:

```rust
fn workspace_service_for_application_root(
    &self,
    workspace: &Path,
) -> Option<Arc<dyn AgentWorkspaceService>> {
    Some(Arc::new(DaemonWorkspaceService::new(workspace.to_path_buf())))
}
```

Extract an attach request helper in `main.rs` and have both `attach_headless_runtime` and `attach_tui_runtime` call it with `config.workdir.clone()`.

- [ ] **Step 4: Run the CLI-focused test to verify it passes**

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent attach_application_root_request_uses_the_configured_workdir -- --exact
```

Expected: PASS.

- [ ] **Step 5: Commit the CLI wiring**

```bash
git add yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "fix: pass client workspace to subagent runtime"
```

### Task 3: Update Protocol Call Sites and Verify

**Files:**
- Modify: all compile-identified `IpcRequest::AttachApplicationRoot` constructors under `yi-agent-rs/`.
- Modify: `docs/project-management/subagent-runtime.md` with the project-isolation completion criterion and test command.

**Interfaces:**
- Consumes: required `workspace: PathBuf` IPC field.
- Produces: a fully updated protocol test suite and project-management record.

- [ ] **Step 1: Update every remaining test request constructor**

For each `IpcRequest::AttachApplicationRoot` in store integration tests, add a deterministic test path such as:

```rust
workspace: directory.path().join("project"),
```

Use the same path for requests sharing one idempotency key.

- [ ] **Step 2: Run the relevant suites**

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc && cargo test -p yi-agent-store --test subagent_runtime_e2e && cargo test -p yi-agent
```

Expected: all selected tests pass.

- [ ] **Step 3: Format, inspect, and run final verification**

Run:

```bash
cd yi-agent-rs && cargo fmt --all && cargo test -p yi-agent-store --test runtime_ipc && cargo test -p yi-agent-store --test subagent_runtime_e2e && cargo test -p yi-agent && cargo clippy -p yi-agent-store -p yi-agent --all-targets -- -D warnings
cd .. && git diff --check && git diff -- docs/superpowers/specs/2026-08-16-subagent-runtime-project-isolation-design.md docs/superpowers/plans/2026-08-16-subagent-runtime-project-isolation.md yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs yi-agent-rs/crates/yi-agent-store/src/ipc.rs yi-agent-rs/crates/yi-agent-store/src/runtime.rs yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs yi-agent-rs/crates/yi-agent/src/main.rs yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs docs/project-management/subagent-runtime.md
```

Expected: formatting, tests, clippy, whitespace check, and diff inspection complete without failures.

- [ ] **Step 4: Commit documentation and protocol-call-site completion**

```bash
git add docs/superpowers/specs/2026-08-16-subagent-runtime-project-isolation-design.md docs/superpowers/plans/2026-08-16-subagent-runtime-project-isolation.md docs/project-management/subagent-runtime.md yi-agent-rs
git commit -m "docs: record subagent project isolation"
```
