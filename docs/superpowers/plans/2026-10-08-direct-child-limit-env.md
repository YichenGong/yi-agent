# Direct-Child Limit Env Wiring & TOML Dead-Layer Cleanup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Delete the never-wired TOML narrowing-policy layer from `yi-agent-store` and make the direct-child limit configurable via `YI_AGENT_MAX_DIRECT_CHILDREN` (default 4), mirroring the existing `YI_AGENT_MAX_RESIDENT_SUBAGENTS` path.

**Architecture:** Four sequential tasks. Task 1 removes dead code in isolation. Task 2 parametrizes the core `AgentSupervisor` (struct field + builder + `AgentWorkerFactory` trait method) and makes `SpawnError::DirectChildLimitReached` carry its limit so the wire message stops hardcoding "four". Task 3 threads the config value `RuntimeConfig` → factory → `RuntimeCoordinator` → supervisor and lands the env var. Task 4 updates user-facing docs.

**Tech Stack:** Rust (cargo workspace, `yi-agent-rs/`), `thiserror`, `serde`, `tokio`.

**Spec:** `docs/superpowers/specs/2026-10-08-direct-child-limit-env-design.md`

## Global Constraints

- Default direct-child limit is **4** everywhere (core constant, runtime constant, env fallback). Never introduce a second default value.
- The env var name is exactly `YI_AGENT_MAX_DIRECT_CHILDREN`.
- `SchedulePolicy` / `RuntimePolicy` serde shapes and `SchedulePolicy::default()` values stay unchanged (persisted at `repository.rs:690`; `max_turns`/`max_wall_time_secs` consumed at `repository.rs:703-704`). Do NOT delete these structs or their fields.
- Do NOT touch `max_depth` (it is the `TaskDepth` `Root/Child/Leaf` type machine in `core/src/subagent/task.rs`).
- Do NOT add a CLI flag for this knob.
- Error copy becomes `an agent may have at most {limit} direct children` — the literal word "four" must disappear from all sources.
- Run tests from `yi-agent-rs/`. Pre-existing local failures ("socket path too long") may occur; compare counts against baseline, do not chase them.

---

### Task 1: Delete the TOML narrowing-policy dead layer

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/schedule.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/Cargo.toml`
- Modify: `yi-agent-rs/crates/yi-agent-store/tests/scheduler.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `schedule.rs` retains `ScheduleDefinition`, `ScheduleDefinitionError`, `parse_five_field_cron`, `RetryFailure`, `RetryDecision`, `evaluate_retry`, `WatchdogLimits`, `WatchdogUsage`, `WatchdogObservation`, `WatchdogOutcome`, `evaluate_watchdog`, `SchedulePolicy` (+`Default`), `RuntimePolicy`, `SchedulePriority`, `OverlapPolicy`, `MissedRunPolicy`. All public signatures unchanged.

- [ ] **Step 1: Confirm nothing outside tests references the doomed symbols**

Run:
```bash
cd yi-agent-rs
for s in RuntimePolicyLayer EffectiveRuntimePolicy narrowed_by from_toml effective_with effective_schedule_with default_schedule_policy_with; do
  printf "%-32s " "$s"
  grep -rn "\b$s\b" crates --include=*.rs | grep -v /target/ | grep -v "src/schedule.rs:" | grep -v "tests/scheduler.rs:" | wc -l
done
```
Expected: every count is `0`. If any is non-zero, STOP — the spec's dead-code analysis is stale.

- [ ] **Step 2: Delete the dead items from `schedule.rs`**

Delete these contiguous ranges (work bottom-up so line numbers stay valid):
- `RuntimePolicy::narrowed_by` impl block (the `impl RuntimePolicy { ... }`, currently ~`:534-548`).
- Free function `narrow` (~`:521-523`).
- `impl RuntimePolicyLayer { ... }` (~`:332-519`), including `from_toml`, `effective_with`, `effective_schedule_with`, `default_schedule_policy_with`.
- `EffectiveRuntimePolicy` struct (~`:312-330`).
- The four `*LimitsLayer` structs `ScheduleDefaultsLayer`, `AttemptLimitsLayer`, `ResourceLimitsLayer`, `RuntimeLimitsLayer` (~`:273-310`).
- `RuntimePolicyLayer` struct (~`:259-271`).

After deletion the `impl RuntimePolicy` block is gone, so `RuntimePolicy` becomes a plain data struct:
```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimePolicy {
    pub max_resident_subagents: u16,
    pub max_turns: u32,
    pub max_wall_time_secs: u64,
    pub read_only: bool,
    pub allow_coding: bool,
}
```

- [ ] **Step 3: Add the clarifying doc comment on the persisted field**

On `RuntimePolicy.max_resident_subagents`, add:
```rust
    /// Conservative default for a *scheduled root* (4). This is NOT the daemon's
    /// resident capacity — that is `YI_AGENT_MAX_RESIDENT_SUBAGENTS`
    /// (`yi_agent_runtime::config::RESIDENT_SUBAGENTS_DEFAULT`, default 64) and
    /// it is what actually admits workers. Retained because this struct is
    /// persisted inside `ScheduleDefinition`.
    pub max_resident_subagents: u16,
```

- [ ] **Step 4: Drop the now-unused `toml` dependency**

In `yi-agent-rs/crates/yi-agent-store/Cargo.toml` remove the line `toml = "0.8"` (under `[dependencies]`).

- [ ] **Step 5: Confirm `toml` has no other user in the crate**

Run:
```bash
cd yi-agent-rs
grep -rn "toml" crates/yi-agent-store/src --include=*.rs
```
Expected: no matches. If something else matches, keep the dependency and skip Step 4.

- [ ] **Step 6: Delete the nine dead tests in `tests/scheduler.rs`**

Delete these test fns entirely: `toml_policy_layers_can_only_narrow_the_user_ceiling`, `effective_policy_narrows_resource_and_retry_limits`, `coordination_reserve_is_clamped_to_the_effective_llm_total`, `effective_policy_only_narrows_numeric_limits_and_capabilities`, `explicit_schedule_selection_is_only_narrowed_by_project_and_global_limits`, `schedule_defaults_seed_a_conservative_policy_without_an_explicit_selection`, `schedule_policy_is_clamped_to_effective_global_runtime_limits`, `explicit_user_schedule_selection_can_exceed_user_schedule_defaults`, `schedule_selection_intersects_global_project_capabilities`.

Keep: `schedule_definition_requires_exactly_five_cron_fields`, `schedule_definition_embeds_conservative_defaults`, `schedule_occurrence_claim_is_durable_and_idempotent`, `retry_policy_only_retries_explicit_transient_failures_with_bounded_backoff`, `watchdog_uses_persisted_progress_and_resource_queue_time`, `watchdog_classifies_turn_and_token_budgets_without_waiting_for_wall_clock`, `scheduled_policy_defaults_to_read_only_background_without_overlap_or_catch_up`.

- [ ] **Step 7: Fix the import list in `tests/scheduler.rs`**

Change the `use yi_agent_store::schedule::{...}` to exactly:
```rust
use yi_agent_store::schedule::{
    MissedRunPolicy, OverlapPolicy, RetryDecision, RetryFailure, ScheduleDefinition, SchedulePolicy,
    SchedulePriority, WatchdogLimits, WatchdogObservation, WatchdogOutcome, WatchdogUsage,
    evaluate_retry, evaluate_watchdog,
};
```
(`RuntimePolicy` and `RuntimePolicyLayer` are no longer referenced by any surviving test.)

- [ ] **Step 8: Verify the crate compiles and the surviving tests pass**

Run:
```bash
cd yi-agent-rs
cargo test -p yi-agent-store --test scheduler
cargo test -p yi-agent-store
```
Expected: `scheduler` shows 7 passing tests; the crate build has no `unused import` / `dead_code` warnings.

- [ ] **Step 9: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-store/src/schedule.rs yi-agent-rs/crates/yi-agent-store/Cargo.toml yi-agent-rs/crates/yi-agent-store/tests/scheduler.rs
git commit -m "refactor(store): remove never-wired TOML narrowing-policy layer"
```

---

### Task 2: Parametrize the core supervisor and carry the limit in the error

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs:2911-2914`
- Modify: `yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs:277-280`
- Modify: `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs:1687`
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:2480-2488`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub const MAX_DIRECT_CHILDREN: usize = 4` (unchanged, now the default).
  - `AgentSupervisor::with_max_direct_children(self, max_direct_children: usize) -> Self`.
  - `SpawnError::DirectChildLimitReached { limit: usize }` (was a unit variant).
  - `AgentWorkerFactory::max_direct_children(&self) -> usize` (default returns `MAX_DIRECT_CHILDREN`).

- [ ] **Step 1: Write the failing test**

Append to `yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs`:
```rust
#[test]
fn a_configured_direct_child_limit_is_enforced_and_reported() {
    let mut supervisor =
        AgentSupervisor::new(RootSessionId::new()).with_max_direct_children(2);
    let root = supervisor.root_task_id().clone();

    supervisor.spawn(root.clone()).unwrap();
    supervisor.spawn(root.clone()).unwrap();

    assert!(matches!(
        supervisor.spawn(root),
        Err(SpawnError::DirectChildLimitReached { limit: 2 })
    ));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --test subagent_supervisor a_configured_direct_child_limit_is_enforced_and_reported`
Expected: FAIL — no method `with_max_direct_children` / no field `limit` on the variant.

- [ ] **Step 3: Add the field and builder to `AgentSupervisor`**

In `supervisor.rs`, add to the struct (near `root_task_id`):
```rust
    max_direct_children: usize,
```
In `new_with_objective`, set it in the struct literal:
```rust
            max_direct_children: MAX_DIRECT_CHILDREN,
```
In `from_recovered_root`, the struct literal is a separate one — add the same line there. Then add the builder right after `new_with_objective`:
```rust
    /// Overrides how many non-terminal direct children one task may own.
    pub fn with_max_direct_children(mut self, max_direct_children: usize) -> Self {
        self.max_direct_children = max_direct_children;
        self
    }
```

- [ ] **Step 4: Carry the limit in the error variant**

Change the enum (`supervisor.rs:36-44`):
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SpawnError {
    #[error("parent task does not exist")]
    ParentNotFound,
    #[error("leaf tasks cannot spawn descendants")]
    MaximumDepthReached,
    #[error("an agent may have at most {limit} direct children")]
    DirectChildLimitReached { limit: usize },
}
```

- [ ] **Step 5: Use the field at the enforcement point**

Replace the check in `spawn_with_objective` (currently `if active_direct_children >= MAX_DIRECT_CHILDREN { return Err(SpawnError::DirectChildLimitReached); }`) with:
```rust
        if active_direct_children >= self.max_direct_children {
            return Err(SpawnError::DirectChildLimitReached {
                limit: self.max_direct_children,
            });
        }
```

- [ ] **Step 6: Add the factory trait method**

In `worker.rs`, after `max_resident_subagents` (add `use` of the supervisor constant via a full path to avoid a new import):
```rust
    /// How many non-terminal direct children one task may own. Mirrors
    /// `crate::subagent::supervisor::MAX_DIRECT_CHILDREN`; a factory that does
    /// not override it inherits that default.
    fn max_direct_children(&self) -> usize {
        crate::subagent::supervisor::MAX_DIRECT_CHILDREN
    }
```

- [ ] **Step 7: Update the IPC message to use the carried limit**

In `ipc.rs`, replace the unit-variant arm (`:2911-2914`):
```rust
        IpcError::Runtime(RuntimeCoordinatorError::Spawn(
            yi_agent_core::subagent::supervisor::SpawnError::DirectChildLimitReached { limit },
        )) => Some(format!("an agent may have at most {limit} direct children")),
```

- [ ] **Step 8: Update the three existing assertions**

`yi-agent-core/tests/subagent_supervisor.rs` — in `spawn_enforces_depth_two_and_four_direct_children`, change the match to:
```rust
    assert!(matches!(
        supervisor.spawn(root),
        Err(SpawnError::DirectChildLimitReached { limit: 4 })
    ));
```
`yi-agent-store/tests/runtime_ipc.rs:1687` — change to:
```rust
    assert!(message.unwrap().contains("at most 4 direct children"));
```
`yi-agent-subagent/src/lib.rs` — in `ipc_rejection_formatter_includes_error_message`, the fixture string becomes `"an agent may have at most 4 direct children"` and the expected output becomes `"daemon rejected spawn request: invalid_state: an agent may have at most 4 direct children"`.

- [ ] **Step 9: Run the tests to verify they pass**

Run:
```bash
cd yi-agent-rs
cargo test -p yi-agent-core --test subagent_supervisor
cargo test -p yi-agent-store --test runtime_ipc
cargo test -p yi-agent-subagent --lib
```
Expected: PASS, including the new test and the three updated assertions.

- [ ] **Step 10: Confirm no "four" remains in code**

Run:
```bash
cd yi-agent-rs
grep -rn "at most four" crates --include=*.rs | grep -v /target/
```
Expected: no matches.

- [ ] **Step 11: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs yi-agent-rs/crates/yi-agent-store/src/ipc.rs yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs yi-agent-rs/crates/yi-agent-subagent/src/lib.rs
git commit -m "feat(core): make direct-child limit configurable and carry it in SpawnError"
```

---

### Task 3: Wire `YI_AGENT_MAX_DIRECT_CHILDREN` end to end

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/config.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/models.rs:370-390`
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs` (factory field/builder/trait impl)
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/attach.rs` (`build_worker_factory` + two test literals)
- Modify: `yi-agent-rs/crates/yi-agent-subagent/tests/attach_delegation.rs:30-40`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` (test literals)
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:2738`

**Interfaces:**
- Consumes: `AgentSupervisor::with_max_direct_children` and `AgentWorkerFactory::max_direct_children` from Task 2.
- Produces:
  - `pub const DIRECT_CHILDREN_DEFAULT: u16 = 4;` in `yi_agent_runtime::config`.
  - `RuntimeConfig.max_direct_children: u16`.
  - `DaemonAgentWorkerFactory::with_max_direct_children(self, usize) -> Self` and its `fn max_direct_children(&self) -> usize` impl.
  - `RuntimeCoordinator` field `max_direct_children: usize`, injected into every supervisor it creates.

- [ ] **Step 1: Write the failing runtime test**

Append to `config.rs`'s test module, next to `the_environment_can_lower_the_resident_capacity`:
```rust
    #[test]
    fn max_direct_children_defaults_to_four_and_reads_the_env() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut env = isolated_config_env();
        let temp = tempfile::TempDir::new().expect("tempdir");
        let overrides = ConfigOverrides {
            api_key: Some("sk-test".into()),
            workdir: Some(temp.path().to_path_buf()),
            ..ConfigOverrides::default()
        };
        let config = RuntimeConfig::load(&overrides).expect("config loads");
        assert_eq!(config.max_direct_children, DIRECT_CHILDREN_DEFAULT);

        env.set("YI_AGENT_MAX_DIRECT_CHILDREN", "8");
        let config = RuntimeConfig::load(&overrides).expect("config loads");
        assert_eq!(config.max_direct_children, 8);
    }
```
Then add `"YI_AGENT_MAX_DIRECT_CHILDREN"` to BOTH string lists inside `isolated_config_env()` (the `EnvVarGuard::new([...])` array and the plain `for key in [...]` array), so the test cannot leak or inherit the variable.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime max_direct_children_defaults_to_four_and_reads_the_env`
Expected: FAIL — no `DIRECT_CHILDREN_DEFAULT`, no field `max_direct_children`.

- [ ] **Step 3: Add the constant and the config field**

In `config.rs`, next to `RESIDENT_SUBAGENTS_DEFAULT`:
```rust
/// Default direct-child limit, mirrored by
/// `AgentWorkerFactory::max_direct_children`'s default so a factory that does
/// not override it and a config that does not set it agree.
pub const DIRECT_CHILDREN_DEFAULT: u16 = 4;
```
In `RuntimeConfig`, after `max_resident_subagents`:
```rust
    /// How many non-terminal direct children one agent may own. Defaults to
    /// [`DIRECT_CHILDREN_DEFAULT`].
    pub max_direct_children: u16,
```

- [ ] **Step 4: Read the env var in `load()`**

Next to the `max_resident_subagents` read:
```rust
        let max_direct_children = std::env::var("YI_AGENT_MAX_DIRECT_CHILDREN")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DIRECT_CHILDREN_DEFAULT);
```
Add `max_direct_children,` to the `Ok(RuntimeConfig { .. })` literal, and `"max_direct_children": self.max_direct_children,` to `redacted_view()`.

- [ ] **Step 5: Update every remaining `RuntimeConfig` literal in the workspace**

Each needs one added line, `max_direct_children: DIRECT_CHILDREN_DEFAULT,` (or `yi_agent_runtime::config::DIRECT_CHILDREN_DEFAULT` where the path is already qualified):
- `yi-agent-runtime/src/config.rs` → `sample_config()`
- `yi-agent-runtime/src/models.rs:370` (`cfg()` test helper)
- `yi-agent-subagent/src/attach.rs:419` (`base_config`) and `:533` test literal
- `yi-agent-subagent/tests/attach_delegation.rs:30` (`config_for`)
- `yi-agent-app-server/src/server.rs:8030` and the `~:8824` region (`test_config` helpers)
- `yi-agent/src/main.rs:2738` (`Config {` test literal)

Find any stragglers with:
```bash
cd yi-agent-rs
grep -rn "max_resident_subagents:" crates --include=*.rs | grep -v /target/ | grep -v "src/schedule.rs"
```
Every file listed must also contain a `max_direct_children:` line.

- [ ] **Step 6: Add the factory field, builder, and trait impl**

In `yi-agent-subagent/src/lib.rs`, add the field after `max_resident_subagents`:
```rust
    /// Direct-child limit passed to each supervisor this daemon creates.
    /// Defaults to [`yi_agent_runtime::config::DIRECT_CHILDREN_DEFAULT`].
    max_direct_children: usize,
```
Initialise it in `new()`:
```rust
            max_direct_children: usize::from(yi_agent_runtime::config::DIRECT_CHILDREN_DEFAULT),
```
Add the builder next to `with_max_resident_subagents`:
```rust
    /// Configures how many non-terminal direct children one agent may own, so
    /// the configured value reaches the coordinator rather than the trait
    /// default.
    pub fn with_max_direct_children(mut self, max_direct_children: usize) -> Self {
        self.max_direct_children = max_direct_children;
        self
    }
```
Add the trait impl next to `fn max_resident_subagents`:
```rust
    fn max_direct_children(&self) -> usize {
        self.max_direct_children
    }
```

- [ ] **Step 7: Forward the config value in `build_worker_factory`**

In `yi-agent-subagent/src/attach.rs`, chain onto the factory (after `.with_max_resident_subagents(effective.max_resident_subagents)`):
```rust
    .with_max_direct_children(usize::from(effective.max_direct_children)))
```

- [ ] **Step 8: Store the value on `RuntimeCoordinator` and inject it**

In `yi-agent-store/src/runtime.rs`, add to the struct:
```rust
    /// Direct-child limit inherited by every supervisor this coordinator creates.
    max_direct_children: usize,
```
In `open()`, read it off the factory. Right before the `Ok(Self {` literal (alongside the existing `let fork_max_bytes = ...` line):
```rust
        let max_direct_children = factory.max_direct_children();
```
Then add `max_direct_children,` to that `Ok(Self { .. })` literal.

Then inject at the supervisor creation sites in the same file:
- `create_session_with_objective_and_mode` (`~:811`):
```rust
        let mut supervisor = AgentSupervisor::new_with_objective(session_id.clone(), objective.clone())
            .with_max_direct_children(self.max_direct_children);
```
- the scheduled-root site (`~:1503`):
```rust
            let supervisor = AgentSupervisor::new_with_objective(
                session.clone(),
                schedule.definition.objective.clone(),
            )
            .with_max_direct_children(self.max_direct_children);
```
- the recovery sites (`~:565-576`) create supervisors via `from_recovered_root` / `from_recovered_gated_root`. Add `.with_max_direct_children(self.max_direct_children)` to each. Read the surrounding code first: if either is inside a closure or `match` arm where `self` is not directly available, bind `let max_direct_children = self.max_direct_children;` before the block and use the local.

- [ ] **Step 9: Write the factory-forwarding test**

Add to `yi-agent-subagent/src/attach.rs` tests, mirroring `the_configured_resident_capacity_reaches_the_worker_factory`:
```rust
    #[test]
    fn the_configured_direct_child_limit_reaches_the_worker_factory() {
        let directory = tempfile::TempDir::new().unwrap();
        let cfg = yi_agent_runtime::config::RuntimeConfig {
            provider: "anthropic".into(),
            api_url: "https://api.anthropic.com".into(),
            api_key: String::new(),
            model: "test-model".into(),
            max_turns: 4,
            max_resident_subagents: 8,
            max_direct_children: 3,
            workdir: directory.path().to_path_buf(),
            system_prompt: None,
            compact_threshold: 160_000,
            compact_user_budget_tokens: 20_000,
            compact_tool_budget_tokens: 12_000,
            yolo: false,
            sandbox_promotable: true,
            sandbox: yi_agent_tools::SandboxMode::default(),
            sandbox_writable_roots: Vec::new(),
            skills_catalog_budget: 8192,
            skills_catalog_budget_explicit: true,
        };
        let socket = directory.path().join("runtime.sock");

        let factory = worker_factory(&cfg, socket).expect("factory");

        assert_eq!(
            factory.max_direct_children(),
            3,
            "the configured limit must reach the factory, not the trait default"
        );
    }
```

- [ ] **Step 10: Run the tests to verify they pass**

Run:
```bash
cd yi-agent-rs
cargo test -p yi-agent-runtime max_direct_children
cargo test -p yi-agent-subagent --lib
cargo test -p yi-agent-store --lib
```
Expected: PASS. The factory-forwarding test fails if Step 7 is skipped — that is the regression this task exists to prevent.

- [ ] **Step 11: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-runtime yi-agent-rs/crates/yi-agent-subagent yi-agent-rs/crates/yi-agent-store/src/runtime.rs yi-agent-rs/crates/yi-agent-app-server/src/server.rs yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "feat: wire YI_AGENT_MAX_DIRECT_CHILDREN from config to supervisor"
```

---

### Task 4: Document the new knob

**Files:**
- Modify: `README.md` (常用配置 table, ~line 195)
- Modify: `.env.example` (=== Agent === section)
- Modify: `docs/superpowers/specs/2026-08-09-runtime-scheduling-design.md:55-95`

**Interfaces:**
- Consumes: the final env var name and default from Task 3.
- Produces: user-facing docs only.

- [ ] **Step 1: Add the README row**

Under the `| 单轮最大步数 | YI_AGENT_MAX_TURNS（默认 200） | --max-turns |` row, add:
```markdown
| 每个 agent 的直接子任务上限 | `YI_AGENT_MAX_DIRECT_CHILDREN`（默认 4） | —— |
| 子 agent 常驻总容量 | `YI_AGENT_MAX_RESIDENT_SUBAGENTS`（默认 64） | —— |
```

- [ ] **Step 2: Add the `.env.example` lines**

In the `# === Agent ===` section:
```
YI_AGENT_MAX_DIRECT_CHILDREN=4
YI_AGENT_MAX_RESIDENT_SUBAGENTS=64
```

- [ ] **Step 3: Rewrite the stale TOML schema section**

In `docs/superpowers/specs/2026-08-09-runtime-scheduling-design.md`, replace the "Configuration Schema And Effective Policy" TOML block and the `min(user, project, ...)` paragraph with:
```markdown
## Configuration Schema And Effective Policy

These knobs are currently supplied as environment variables
(`YI_AGENT_MAX_RESIDENT_SUBAGENTS`, `YI_AGENT_MAX_DIRECT_CHILDREN`). The
multi-layer TOML narrowing model (`RuntimePolicyLayer` /
`EffectiveRuntimePolicy`) was parsed but never wired to a loader; it was
removed on 2026-10-08 rather than left as a knob that looked effective but was
not. Limits still compose by minimum and capabilities by intersection when the
layered model is reintroduced.
```

- [ ] **Step 4: Verify the full workspace builds and the suite is green**

Run:
```bash
cd yi-agent-rs
cargo test -p yi-agent-core -p yi-agent-runtime -p yi-agent-subagent -p yi-agent-store -p yi-agent-app-server
cargo clippy --workspace --all-targets -- -D warnings
```
Expected: pass, modulo pre-existing local socket-path failures (compare the count against a pre-change baseline).

- [ ] **Step 5: Commit**

```bash
git add README.md .env.example docs/superpowers/specs/2026-08-09-runtime-scheduling-design.md
git commit -m "docs: document YI_AGENT_MAX_DIRECT_CHILDREN and retire the TOML policy schema"
```

---

## Self-Review

**Spec coverage:**
- §1 background/diagnosis → Tasks 1-2 (not code, no task needed).
- §2 goals 1-4 → Task 3 (goal 1), Task 1 (goal 2), Task 2 (goal 3), Task 1 Step 3 + Task 2 Step 6 (goal 4).
- §2 non-goals → Global Constraints (no `max_depth`, no CLI flag).
- §3 D1 → Task 3; D2 → Task 1; D3 → Task 1 Steps 2-3; D4 → Task 2 Step 3/6 + Task 3 Step 3; D5 → Task 3 Steps 6-8; D6 → Task 2 Steps 4-5/7; D7 → Task 1 Steps 4-5.
- §4.1 → Task 1 Step 2; §4.2 → Task 1 Step 4; §4.3 → Task 1 Step 3; §4.4 → Task 1 Steps 6-7.
- §5 rows 1-6 → Task 3 Steps 3-8 (row 5 = Step 7, the historical failure point); literal fallout → Task 3 Step 5.
- §6 → Task 2 Steps 4-8.
- §7 cases 1-4 → Task 2 Step 1 (case 1), Task 3 Step 1 (case 2), Task 3 Step 9 (case 3), Task 2 Step 8 (case 4, via the runtime_ipc assertion).
- §8 → Task 4.
- §9 → Task 4 Step 4.
- §10 risks → addressed by Task 3 Step 9 (risk 1), Task 1 Step 3 + verification (risk 2), Task 2 Step 8 (risk 3).

**Placeholder scan:** No "TBD"/"implement later"/"add error handling". Every code step shows the literal code. Task 3 Step 8's recovery-site note names the exact symbols to touch and tells the implementer how to handle scope ("bind a local") rather than leaving it vague.

**Type consistency:** `MAX_DIRECT_CHILDREN: usize`, `DIRECT_CHILDREN_DEFAULT: u16`, `RuntimeConfig.max_direct_children: u16`, `DaemonAgentWorkerFactory.max_direct_children: usize`, `AgentWorkerFactory::max_direct_children() -> usize`, `RuntimeCoordinator.max_direct_children: usize`, `AgentSupervisor.max_direct_children: usize`, `SpawnError::DirectChildLimitReached { limit: usize }`. Conversion happens exactly once, at `attach.rs` (`usize::from(effective.max_direct_children)`) and once at factory init (`usize::from(DIRECT_CHILDREN_DEFAULT)`).
