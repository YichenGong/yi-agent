# Project AGENTS.md Prompt Loading Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Load `<workdir>/AGENTS.md` into the normal yi-agent system prompt, including when a user provides a custom prompt.

**Architecture:** Prompt construction stays in `crates/yi-agent/src/main.rs`. A focused helper reads only the configured workdir's root `AGENTS.md`; the shared resolver appends it after built-in and user prompt content, before date and skills catalog. `--naked` continues to return before all prompt construction.

**Tech Stack:** Rust 2024, `std::fs`, `tempfile`, `tracing`, Cargo test/fmt, Markdown.

---

## File Structure

- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs` - load instructions, compose prompts, and test behavior.
- Modify: `docs/project-management/yi-agent-tui.md` - record the feature and a focused verification command.
- Modify: `docs/project-management/README.md` - move TUI completion count from `20 / 21` to `21 / 21`.

### Task 1: Test Root-level Project Instructions

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:609-678`
- Test: `yi-agent-rs/crates/yi-agent/src/main.rs:741-812`

- [ ] **Step 1: Add failing unit tests using temporary project roots**

Inside `mod tests`, import `std::fs`, then add:

```rust
#[test]
fn resolve_system_prompt_appends_root_agents_md_after_custom_instructions() {
    let project = tempfile::tempdir().expect("create project root");
    fs::write(project.path().join("AGENTS.md"), "Always run focused tests.")
        .expect("write AGENTS.md");

    let resolved = resolve_system_prompt(Some("Use concise output.".into()), project.path());
    let prompt = resolved.expect("normal mode should have a system prompt");

    assert!(prompt.contains("User-provided instructions:\nUse concise output."));
    assert!(prompt.contains("Project instructions (AGENTS.md):\nAlways run focused tests."));
    assert!(
        prompt.find("User-provided instructions:").unwrap()
            < prompt.find("Project instructions (AGENTS.md):").unwrap()
    );
    assert!(
        prompt.find("Project instructions (AGENTS.md):").unwrap()
            < prompt.find("Current date:").unwrap()
    );
}

#[test]
fn resolve_system_prompt_ignores_missing_root_agents_md() {
    let project = tempfile::tempdir().expect("create project root");
    let prompt = resolve_system_prompt(None, project.path())
        .expect("normal mode should have a system prompt");

    assert!(!prompt.contains("Project instructions (AGENTS.md):"));
}
```

- [ ] **Step 2: Run the new tests and verify RED**

Run `cargo test -p yi-agent --bin yi-agent resolve_system_prompt_` from `yi-agent-rs/`.

Expected: compilation fails because `resolve_system_prompt` only accepts `user`, proving the tests require the new behavior.

### Task 2: Implement Shared Prompt Loading

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:93-106`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:264-278`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:609-678`
- Test: `yi-agent-rs/crates/yi-agent/src/main.rs:741-812`

- [ ] **Step 1: Add the bounded project-instructions helper and extend resolver signature**

Import `std::path::Path`, then use these functions:

```rust
fn load_project_instructions(workdir: &Path) -> Option<String> {
    let path = workdir.join("AGENTS.md");
    match std::fs::read_to_string(&path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            tracing::warn!(path = %path.display(), "failed to read project instructions: {error}");
            None
        }
    }
}

fn resolve_system_prompt(user: Option<String>, workdir: &Path) -> Option<String> {
    let mut base = yi_agent_core::AgentConfig::default_system_prompt();
    if let Some(user) = user {
        base.push_str("\n\nUser-provided instructions:\n");
        base.push_str(&user);
    }
    if let Some(instructions) = load_project_instructions(workdir) {
        base.push_str("\n\nProject instructions (AGENTS.md):\n");
        base.push_str(&instructions);
    }
    let today = chrono::Local::now().format("%Y-%m-%d");
    Some(format!("{base}\n\nCurrent date: {today}"))
}
```

Update `resolve_system_prompt_with_skills` to receive `workdir: &Path` after `user`, and call `resolve_system_prompt(user, workdir)`.

- [ ] **Step 2: Pass the configured workdir through both normal-mode call sites**

At TUI and `build_headless_setup` calls, use:

```rust
let system_prompt = resolve_system_prompt_with_skills(
    config.system_prompt.clone(),
    &config.workdir,
    &skills_service,
    config.skills_catalog_budget,
    config.skills_catalog_budget_explicit,
);
```

Update existing resolver unit tests to pass `Path::new("/definitely-missing-yi-agent-test-root")`. Do not alter `build_headless_setup(..., true)`: its early return keeps naked mode entirely prompt-free.

- [ ] **Step 3: Run GREEN verification**

Run these commands separately from `yi-agent-rs/`:

```sh
cargo test -p yi-agent --bin yi-agent resolve_system_prompt_
cargo test -p yi-agent --bin yi-agent build_headless_setup_naked_has_no_tools_and_no_system_prompt
```

Expected: both exit 0. The first validates injection, custom-prompt retention, and missing-file compatibility; the second validates naked mode.

- [ ] **Step 4: Format and commit code plus tests**

```sh
cargo fmt --all
git add yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "feat: load project AGENTS instructions"
```

### Task 3: Update Project Tracking

**Files:**
- Modify: `docs/project-management/yi-agent-tui.md:Features`
- Modify: `docs/project-management/README.md:module index`

- [ ] **Step 1: Add the completed feature entry**

Insert before the unchecked InlineRenderer item:

```markdown
- [x] 项目 AGENTS.md 提示词加载 — `main.rs::load_project_instructions()` 读取 `<workdir>/AGENTS.md` 并由 `resolve_system_prompt_with_skills()` 注入正常 TUI/run 会话；`--naked` 保持不加载；验证：`cargo test -p yi-agent --bin yi-agent resolve_system_prompt_`
```

- [ ] **Step 2: Change the TUI index count**

Replace the table row with:

```markdown
| yi-agent-tui | 21 / 21 | [详情](./yi-agent-tui.md) |
```

- [ ] **Step 3: Verify docs and commit**

Run `git diff --check`, inspect `git diff -- docs/project-management/yi-agent-tui.md docs/project-management/README.md`, then run:

```sh
cargo fmt --all
git add docs/project-management/yi-agent-tui.md docs/project-management/README.md
git commit -m "docs: track AGENTS prompt loading"
```

### Task 4: Final Verification

**Files:**
- Verify: `yi-agent-rs/crates/yi-agent/src/main.rs`
- Verify: `docs/project-management/yi-agent-tui.md`
- Verify: `docs/project-management/README.md`

- [ ] **Step 1: Run the finished crate test suite**

Run `cargo test -p yi-agent` from `yi-agent-rs/`.

Expected: exit 0 with all yi-agent binary tests passing.

- [ ] **Step 2: Check formatting**

Run these commands separately:

```sh
cargo fmt --all --check
just fmt-check
```

Expected: each exits 0.

- [ ] **Step 3: Confirm branch state**

Run `git status --short --branch` and `git log --oneline main..HEAD`.

Expected: clean `feat/agents-md-prompt` worktree with design, implementation, and project-tracking commits ahead of `main`.
