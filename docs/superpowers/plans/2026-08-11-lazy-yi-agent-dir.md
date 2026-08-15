# Lazy .yi-agent Directory Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Starting yi-agent in a directory must not create `<workdir>/.yi-agent`; project-local `.yi-agent` is created only when yi-agent writes an actual project file.

**Architecture:** Keep `resolve_env_path` as the read location for local `.yi-agent/.env`, but remove eager directory creation from `config::load` fallback mode. Existing write paths such as permission persistence continue to create `.yi-agent` when they write `permissions.toml`.

**Tech Stack:** Rust, `anyhow`, `dotenvy`, Cargo unit tests.

---

### Task 1: Make Config Loading Read-Only For Local .yi-agent

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/config.rs`
- Test: `yi-agent-rs/crates/yi-agent/src/config.rs`
- Modify: `docs/project-management/yi-agent-tui.md`
- Modify: `docs/project-management/README.md`

- [ ] **Step 1: Write the failing test**

Add this test in `#[cfg(test)] mod tests` in `yi-agent-rs/crates/yi-agent/src/config.rs`, replacing the old eager-creation expectation if present:

```rust
#[test]
fn load_does_not_create_local_yi_agent_dir_in_fallback_mode() {
    let temp = temp_dir_path("load_no_create_local_yi_agent");
    let _guard = EnvGuard::new()
        .unset("YI_AGENT_WORKDIR")
        .set("HOME", temp.join("home"));
    std::fs::create_dir_all(&temp).unwrap();
    std::fs::create_dir_all(temp.join("home")).unwrap();
    let yi_agent_dir = temp.join(".yi-agent");
    assert!(!yi_agent_dir.exists());

    let _cwd = CwdGuard::new(&temp);
    let cli = Cli {
        api_key: Some("test-key".to_string()),
        ..Cli::parse_from(["yi-agent"])
    };

    let config = load(&cli).unwrap();

    assert_eq!(config.api_key, "test-key");
    assert!(
        !yi_agent_dir.exists(),
        ".yi-agent/ should not be created until yi-agent writes a project file"
    );
}
```

- [ ] **Step 2: Verify RED**

Run:

```bash
cd yi-agent-rs
cargo test -p yi-agent --bin yi-agent config::tests::load_does_not_create_local_yi_agent_dir_in_fallback_mode
```

Expected: FAIL because `config::load` still creates `<cwd>/.yi-agent` in fallback mode.

- [ ] **Step 3: Write minimal implementation**

In `yi-agent-rs/crates/yi-agent/src/config.rs`, remove the local fallback `ensure_dir_exists(parent)?;` call while keeping global env path resolution and dotenv loading:

```rust
let global_env_path = if is_workdir_explicit(cli) {
    None
} else {
    resolve_global_env_path()
};
```

If `ensure_dir_exists` becomes unused, remove the helper.

- [ ] **Step 4: Verify GREEN**

Run:

```bash
cd yi-agent-rs
cargo test -p yi-agent --bin yi-agent config::tests::load_does_not_create_local_yi_agent_dir_in_fallback_mode
```

Expected: PASS.

- [ ] **Step 5: Verify existing env behavior**

Run:

```bash
cd yi-agent-rs
cargo test -p yi-agent --bin yi-agent config::tests::load_merges_local_and_global_env_in_fallback_mode config::tests::load_uses_local_env_over_global_env
```

Expected: PASS; existing local `.yi-agent/.env` files are still read.

- [ ] **Step 6: Update project-management docs**

Add a completed `[x]` line to `docs/project-management/yi-agent-tui.md` with the behavior and verification command. Increment the module index count for `yi-agent-tui` in `docs/project-management/README.md`.

- [ ] **Step 7: Format and test**

Run:

```bash
cd yi-agent-rs
cargo fmt --all
cargo test -p yi-agent --bin yi-agent config::tests::load_
```

Expected: PASS for config load tests.
