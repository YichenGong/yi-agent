# YOLO Sandbox Semantics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `--yolo` bypass yi-agent's shell sandbox by default while preserving explicitly configured sandbox modes.

**Architecture:** Permission approval and sandbox policy stay separate. Configuration loading treats `cli.yolo` as a default-selection shortcut: choose `DangerFullAccess` only when neither `--sandbox` nor `YI_AGENT_SANDBOX` chose a mode. TUI and headless registration already propagate `Config::sandbox` and need no code change.

**Tech Stack:** Rust, clap, tokio tests, macOS Seatbelt/Bubblewrap sandbox abstraction.

---

### Task 1: Specify YOLO's configuration precedence

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/config.rs:1120-1260`

- [ ] **Step 1: Write the failing default-mapping test**

Change `load_yolo_from_cli_flag` so its fixture has no sandbox configured and add:

```rust
assert!(config.yolo);
assert_eq!(config.sandbox, yi_agent_tools::SandboxMode::DangerFullAccess);
```

- [ ] **Step 2: Run it and verify RED**

Run `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent config::tests::load_yolo_from_cli_flag`.

Expected: FAIL because the current result is `WorkspaceWrite`.

- [ ] **Step 3: Add explicit-configuration coverage**

Add a test helper, then the following independent tests. The environment test must retain the mutex guard for its full scope so parallel tests cannot observe its temporary variable.

```rust
fn test_cli() -> Cli {
    Cli {
        command: None,
        provider: None,
        api_url: None,
        api_key: Some("test-key".into()),
        model: None,
        max_turns: None,
        workdir: Some(PathBuf::from(".")),
        system_prompt: None,
        model_context_length: None,
        compact_ratio: None,
        compact_keep_turns: None,
        yolo: false,
        sandbox: None,
        sandbox_writable_roots: Vec::new(),
        skip_permissions: false,
        skills_catalog_budget: None,
        debug: false,
    }
}

#[test]
fn explicit_cli_sandbox_overrides_yolo() {
    let mut cli = test_cli();
    cli.yolo = true;
    cli.sandbox = Some(yi_agent_tools::SandboxMode::ReadOnly);
    assert_eq!(load(&cli).unwrap().sandbox, yi_agent_tools::SandboxMode::ReadOnly);
}

#[test]
fn environment_sandbox_overrides_yolo() {
    let _lock = ENV_TEST_MUTEX.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut env = EnvVarGuard::new(["YI_AGENT_SANDBOX"]);
    env.set("YI_AGENT_SANDBOX", "read-only");
    let mut cli = test_cli();
    cli.yolo = true;
    assert_eq!(load(&cli).unwrap().sandbox, yi_agent_tools::SandboxMode::ReadOnly);
}

#[test]
fn skip_permissions_keeps_default_sandbox() {
    let mut cli = test_cli();
    cli.skip_permissions = true;
    assert_eq!(load(&cli).unwrap().sandbox, yi_agent_tools::SandboxMode::WorkspaceWrite);
}
```

### Task 2: Resolve sandbox with a YOLO-aware default

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/config.rs:321-337`
- Test: `yi-agent-rs/crates/yi-agent/src/config.rs:1120-1260`

- [ ] **Step 1: Implement the minimum precedence change**

Use this final fallback after explicit CLI and environment configuration:

```rust
Err(_) if cli.yolo => yi_agent_tools::SandboxMode::DangerFullAccess,
Err(_) => yi_agent_tools::SandboxMode::default(),
```

Keep the existing invalid-environment error. Do not use the derived `yolo` value here: `--dangerously-skip-permissions` and `YI_AGENT_YOLO=true` remain confirmation-only.

- [ ] **Step 2: Run GREEN verification**

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent config::tests::load_yolo_from_cli_flag
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent config::tests::explicit_cli_sandbox_overrides_yolo
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent config::tests::environment_sandbox_overrides_yolo
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent config::tests::skip_permissions_keeps_default_sandbox
```

Expected: all four tests PASS.

- [ ] **Step 3: Validate the command-launcher invariant**

Run `cd yi-agent-rs && cargo test -p yi-agent-tools dangerous_mode_runs_sh_without_a_wrapper`.

Expected: PASS; `DangerFullAccess` invokes `sh -c`, not `sandbox-exec`, allowing shell redirections to `/dev/null`.

### Task 3: Record completion and verify the branch

**Files:**
- Modify: `docs/project-management/yi-agent-tools.md:30`
- Modify: `docs/project-management/README.md:14`
- Modify: `docs/bug-list.md` only after safely incorporating the user's current uncommitted entry.

- [ ] **Step 1: Update module tracking**

Add this completed item to `docs/project-management/yi-agent-tools.md`:

```markdown
- [x] YOLO full bypass default — `crates/yi-agent/src/config.rs` selects `danger-full-access` for `--yolo` unless `--sandbox` or `YI_AGENT_SANDBOX` explicitly chooses a mode; verify with `cargo test -p yi-agent --bin yi-agent config::tests::load_yolo_from_cli_flag`
```

Update `docs/project-management/README.md` so `yi-agent-tools` is `7 / 7`.

- [ ] **Step 2: Format and run targeted verification**

Run:

```bash
cd yi-agent-rs && cargo fmt --all
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent config::tests
cd yi-agent-rs && cargo test -p yi-agent-tools dangerous_mode_runs_sh_without_a_wrapper
```

Expected: formatter succeeds and all selected tests PASS.

- [ ] **Step 3: Inspect and commit**

Run:

```bash
git diff --check
git status --short
git add yi-agent-rs/crates/yi-agent/src/config.rs docs/project-management/yi-agent-tools.md docs/project-management/README.md
git commit -m "feat(cli): make yolo bypass sandbox by default"
```

Add `docs/bug-list.md` only after reconciling the user's existing uncommitted entry; never overwrite it.
