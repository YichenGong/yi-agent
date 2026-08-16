# Project-Local Runtime Directory Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make each effective project workdir use its own default Runtime socket and SQLite state so a daemon started in one project cannot allocate worktrees for another project.

**Architecture:** Keep the Runtime daemon, IPC protocol, database schema, and workspace factory unchanged. Replace the CLI's user-global Runtime-directory fallback with `<effective-workdir>/.yi-agent/runtime`, while preserving a non-empty `YI_AGENT_RUNTIME_DIR` as an explicit override. Resolve the configuration once per CLI command path and pass its `workdir` into the Runtime-directory helper.

**Tech Stack:** Rust, Clap, Tokio, Unix-domain sockets, SQLite/rusqlite, cargo test.

## Global Constraints

- Runtime directory precedence is non-empty `YI_AGENT_RUNTIME_DIR`, then `<effective-workdir>/.yi-agent/runtime`.
- Effective workdir remains CLI `--workdir`, then non-empty `YI_AGENT_WORKDIR`, then process current directory.
- Do not change IPC request/response types, SQLite schema, or `AgentWorkspaceService`.
- Do not migrate or delete legacy `~/.yi-agent/runtime` state.
- A manually supplied `YI_AGENT_RUNTIME_DIR` deliberately permits sharing a Runtime across projects.
- Keep all Runtime commands (`daemon`, `agents`, `agent`, `schedule`, TUI attach, and headless `--subagents`) on the same resolver.

---

## File Structure

- Modify `yi-agent-rs/crates/yi-agent/src/main.rs`: replace the home-directory fallback resolver; obtain `Config.workdir` before every Runtime IPC/daemon operation; update all runtime call sites; add resolver tests.
- Modify `yi-agent-rs/crates/yi-agent/src/config.rs`: no functional change expected; use its existing `load(&Cli) -> Result<Config>` as the single source of the effective workdir.
- No changes to `yi-agent-store`: the Runtime endpoint remains directory-based and its IPC/database contracts are intentionally untouched.

### Task 1: Add a project-local Runtime resolver with regression tests

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:536-548`
- Test: `yi-agent-rs/crates/yi-agent/src/main.rs` existing `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `std::env::var_os("YI_AGENT_RUNTIME_DIR")` and an effective `&Path` workdir.
- Produces: `fn runtime_directory_for(workdir: &std::path::Path) -> std::path::PathBuf`.
- Produces: `fn runtime_directory_from(override_path: Option<PathBuf>, workdir: &Path) -> PathBuf` for deterministic tests.

- [ ] **Step 1: Write failing resolver tests**

Add these tests to the `tests` module in `main.rs`:

```rust
#[test]
fn runtime_directory_uses_workdir_local_default() {
    assert_eq!(
        runtime_directory_from(None, std::path::Path::new("/tmp/project-a")),
        std::path::PathBuf::from("/tmp/project-a/.yi-agent/runtime"),
    );
}

#[test]
fn runtime_directory_prefers_a_nonempty_explicit_override() {
    assert_eq!(
        runtime_directory_from(
            Some(std::path::PathBuf::from("/tmp/shared-runtime")),
            std::path::Path::new("/tmp/project-a"),
        ),
        std::path::PathBuf::from("/tmp/shared-runtime"),
    );
}

#[test]
fn runtime_directory_isolated_between_workdirs() {
    let first = runtime_directory_from(None, std::path::Path::new("/tmp/project-a"));
    let second = runtime_directory_from(None, std::path::Path::new("/tmp/project-b"));
    assert_ne!(first, second);
}
```

- [ ] **Step 2: Run the focused tests and verify they fail**

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent runtime_directory_ -- --nocapture
```

Expected: compilation failure because `runtime_directory_from` still accepts an optional home directory rather than an effective workdir, and the project-local expected result is unsupported.

- [ ] **Step 3: Implement the minimal resolver change**

Replace the existing resolver with this shape:

```rust
fn runtime_directory_for(workdir: &std::path::Path) -> std::path::PathBuf {
    runtime_directory_from(
        std::env::var_os("YI_AGENT_RUNTIME_DIR")
            .filter(|value| !value.is_empty())
            .map(std::path::PathBuf::from),
        workdir,
    )
}

fn runtime_directory_from(
    override_path: Option<std::path::PathBuf>,
    workdir: &std::path::Path,
) -> std::path::PathBuf {
    override_path.unwrap_or_else(|| workdir.join(".yi-agent/runtime"))
}
```

Do not call `dirs::home_dir()` from this resolver. Retain the exact supplied override path; do not canonicalize it or create directories here.

- [ ] **Step 4: Run the focused tests and verify they pass**

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent runtime_directory_ -- --nocapture
```

Expected: all three focused resolver tests pass.

- [ ] **Step 5: Commit the isolated resolver**

```bash
git add yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "fix: default runtime state to project workdir"
```

### Task 2: Thread the effective workdir through every Runtime command path

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:29-83`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:438-478`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:602-735`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:738-775`
- Test: `yi-agent-rs/crates/yi-agent/src/main.rs` existing `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `config::load(&Cli) -> anyhow::Result<config::Config>`.
- Consumes: `runtime_directory_for(&config.workdir) -> PathBuf` from Task 1.
- Produces: every daemon start/status/stop, Runtime task control, TUI attachment, and headless attachment uses the same workdir-derived `runtime.sock`.

- [ ] **Step 1: Write a failing helper-level consistency test**

Add a test that proves the configuration-derived workdir controls the default runtime directory, while an explicit override still wins. Reuse the existing `EnvVarGuard` test utility from `config.rs` only if it is accessible; otherwise construct the resolver inputs directly:

```rust
#[test]
fn runtime_directory_for_workdir_uses_the_same_project_path_for_daemon_and_attachment() {
    let workdir = std::path::PathBuf::from("/tmp/isolated-project");
    let daemon_runtime = runtime_directory_from(None, &workdir);
    let attachment_runtime = runtime_directory_from(None, &workdir);

    assert_eq!(daemon_runtime, attachment_runtime);
    assert_eq!(
        daemon_runtime,
        std::path::PathBuf::from("/tmp/isolated-project/.yi-agent/runtime"),
    );
}
```

- [ ] **Step 2: Run the test before changing call sites**

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent runtime_directory_for_workdir_uses_the_same_project_path_for_daemon_and_attachment -- --exact
```

Expected: PASS after Task 1. This is a characterization guard before changing call-site plumbing.

- [ ] **Step 3: Update each CLI path to load config and use `runtime_directory_for`**

Make these exact ownership changes:

1. Change `control_daemon(cli, action)` to load `let config = config::load(cli)?;` and derive `let runtime_dir = runtime_directory_for(&config.workdir);`.
2. Change `control_agents(project, all)` to accept `&Cli`, load config, and derive the socket from `runtime_directory_for(&config.workdir)`. Update the `Command::Agents` match arm accordingly.
3. Change `control_agent(action)` to accept `&Cli`, load config, and derive its socket identically. Update the `Command::Agent` match arm accordingly.
4. Change `control_schedule(cli, action)` to use the already-loaded command config or load it once and derive its socket with `runtime_directory_for(&config.workdir)`; do not leave a call to the old zero-argument resolver.
5. In `attach_headless_runtime`, replace `runtime_directory()?` with `runtime_directory_for(&config.workdir)`.
6. In `attach_tui_runtime`, replace `runtime_directory()?` with `runtime_directory_for(&config.workdir)`.
7. Search `main.rs` for `runtime_directory(` and ensure no production call uses a zero-argument global fallback.

The command controls must use config loading even when their operation does not need provider credentials. If current `config::load` requires an API key for those command paths, extract a narrow `resolve_workdir(cli) -> Result<PathBuf>` helper in `config.rs` that implements only the documented workdir precedence and directory validation; then make `config::load` call that helper and make Runtime command paths call it. Do not require an API key merely to inspect, stop, or list a local daemon.

- [ ] **Step 4: Add direct command-resolution tests if a narrow workdir helper was added**

If Task 3 extracted `config::resolve_workdir`, add these `config.rs` tests:

```rust
#[test]
fn resolve_workdir_prefers_cli_value() {
    // Build a Cli with workdir set to a TempDir path and assert the same path is returned.
}

#[test]
fn resolve_workdir_uses_nonempty_environment_value() {
    // Set YI_AGENT_WORKDIR to a TempDir path with EnvVarGuard; use a Cli without --workdir.
}
```

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent resolve_workdir_ -- --nocapture
```

Expected: tests pass, proving daemon/task control commands can determine their project runtime without loading provider credentials.

- [ ] **Step 5: Run the CLI unit suite**

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent
```

Expected: all existing CLI/TUI/runtime unit tests and the new resolver tests pass.

- [ ] **Step 6: Commit the call-site plumbing**

```bash
git add yi-agent-rs/crates/yi-agent/src/main.rs yi-agent-rs/crates/yi-agent/src/config.rs
git commit -m "fix: resolve runtime commands per project"
```

### Task 3: Prove no Runtime contract regression and document the behavior

**Files:**
- Modify: `README.md` only if it contains Runtime environment-variable documentation; otherwise no README change.
- Modify: `docs/superpowers/specs/2026-08-16-project-local-runtime-directory-design.md` only if implementation revealed a necessary design correction.
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`

**Interfaces:**
- Consumes: unchanged `IpcRequest::AttachApplicationRoot`, `Daemon::start_with_factory`, and Runtime database schema.
- Produces: verified project-local selection without an IPC/database protocol change.

- [ ] **Step 1: Inspect documented runtime configuration before editing**

Run:

```bash
rg -n -i 'YI_AGENT_RUNTIME_DIR|runtime directory|runtime.sock|daemon' README.md docs yi-agent-rs/crates/yi-agent/src/config.rs
```

Expected: identify the user-facing location, if any, that describes `YI_AGENT_RUNTIME_DIR` or the default Runtime directory.

- [ ] **Step 2: Update existing user-facing documentation only where it exists**

If a user-facing document mentions `YI_AGENT_RUNTIME_DIR` or says the default is `~/.yi-agent/runtime`, replace that description with:

```text
By default Runtime state is stored in <workdir>/.yi-agent/runtime. Set YI_AGENT_RUNTIME_DIR to explicitly use another (including shared) Runtime directory.
```

Do not add duplicate documentation to unrelated files.

- [ ] **Step 3: Run Runtime IPC regression tests**

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc
```

Expected: all Runtime IPC tests pass unchanged, demonstrating that this fix did not alter IPC attachment, capability, lifecycle, or SQLite behavior.

- [ ] **Step 4: Run format, build, and final targeted tests**

Run:

```bash
cd yi-agent-rs && cargo fmt --all -- --check && cargo build -p yi-agent && cargo test -p yi-agent --bin yi-agent && cargo test -p yi-agent-store --test runtime_ipc
```

Expected: formatting passes, build succeeds, and both test suites pass. Existing unrelated dead-code warnings may remain but no new warnings/errors should be introduced by this change.

- [ ] **Step 5: Inspect the final diff and commit**

Run:

```bash
git diff --check
git diff -- yi-agent-rs/crates/yi-agent/src/main.rs yi-agent-rs/crates/yi-agent/src/config.rs README.md docs/superpowers/specs/2026-08-16-project-local-runtime-directory-design.md
git status --short
```

Expected: diff contains only project-local Runtime resolution, its tests, and any necessary matching documentation.

Then commit:

```bash
git add yi-agent-rs/crates/yi-agent/src/main.rs yi-agent-rs/crates/yi-agent/src/config.rs README.md docs/superpowers/specs/2026-08-16-project-local-runtime-directory-design.md
git commit -m "docs: explain project-local runtime state"
```

If README and design spec are unchanged in this task, omit them from `git add`; still make the commit only if Task 3 changed documentation.
