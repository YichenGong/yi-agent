# Cron Scheduled Roots Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver durable conservative Cron-scheduled roots and an LLM natural-language preview that requires explicit user confirmation.

**Architecture:** Store owns five-field Cron validation, SQLite state, occurrence claims, and policy outcomes. The coordinator creates isolated roots for claimed fires. The app calls its configured provider only to extract a preview and sends the validated result to daemon IPC after confirmation.

**Tech Stack:** Rust 2024, `croner` 3.x, chrono local time, SQLite/rusqlite, serde, existing Provider and daemon IPC APIs.

---

### Task 1: Schedule Contract

**Files:** `yi-agent-rs/Cargo.toml`, `yi-agent-rs/crates/yi-agent-store/Cargo.toml`, `yi-agent-rs/crates/yi-agent-store/src/schedule.rs`, and `yi-agent-rs/crates/yi-agent-store/tests/scheduler.rs`.

- [ ] Add failing tests for `ScheduleDefinition::new("0 9 * * 1-5", objective)` acceptance, six-field rejection, and defaults of read-only/background/four residents/30 turns/900 seconds.
- [ ] Run `cargo test -p yi-agent-store --test scheduler schedule_definition`; observe RED because contract types do not exist.
- [ ] Add workspace dependency `croner = "3"` and serializable definition, contract, policy, and validation types. Validation accepts exactly five fields, nonempty objective, and computes local next run.
- [ ] Run the focused test again and observe GREEN.
- [ ] Commit `feat: validate cron schedule definitions`.

### Task 2: Durable Definitions And Occurrences

**Files:** `yi-agent-rs/crates/yi-agent-store/src/repository.rs`, `yi-agent-rs/crates/yi-agent-store/src/schedule.rs`, and `yi-agent-rs/crates/yi-agent-store/tests/scheduler.rs`.

- [ ] Add failing tests that reopening retains a schedule, elevated defaults are rejected without approval, and a `(schedule_id, due_at)` occurrence claims exactly once.
- [ ] Run `cargo test -p yi-agent-store --test scheduler schedule_occurrence`; observe RED because schema and APIs are absent.
- [ ] Add a migration for definition, contract, and next due data plus `schedule_occurrences` with `UNIQUE(schedule_id, due_at)`. Implement atomic create/load-due/claim/advance operations and durable non-secret events for fired, skipped-overlap, missed, and catch-up occurrences.
- [ ] Run `cargo test -p yi-agent-store --test scheduler schedule_`; observe persistence and idempotency GREEN.
- [ ] Commit `feat: persist cron schedule occurrences`.

### Task 3: Runtime Fire Evaluation

**Files:** `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`, `yi-agent-rs/crates/yi-agent-store/src/repository.rs`, and `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`.

- [ ] Add failing tests proving a due schedule creates a distinct root with its objective/contract snapshot, duplicate evaluation creates no duplicate, active roots use default overlap skip, and missed/catch-up policy works after daemon downtime.
- [ ] Run `cargo test -p yi-agent-store --test runtime_coordinator schedule_`; observe RED because `evaluate_schedules` is absent.
- [ ] Add `RuntimeCoordinator::create_schedule` and `evaluate_schedules(now_local)`. Claim first, check prior tree terminal state, persist skip/miss/catch-up outcome, and use `create_session_with_objective` for a fresh root before normal admission.
- [ ] Advance across all elapsed times; `catch_up_once` may create only the newest missed root.
- [ ] Run the focused test and observe GREEN.
- [ ] Commit `feat: fire conservative scheduled roots`.

### Task 4: IPC And Daemon Tick

**Files:** `yi-agent-rs/crates/yi-agent-store/src/ipc.rs` and `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`.

- [ ] Add failing IPC tests for valid create/list/delete, six-field validation error, and idempotency when one minute is evaluated twice.
- [ ] Run `cargo test -p yi-agent-store --test runtime_ipc schedule`; observe RED.
- [ ] Add `CreateSchedule`, `ListSchedules`, and `DeleteSchedule` IPC variants. Map invalid input to `Validation` and unapproved policy elevation to `ConfirmationRequired`.
- [ ] Tick `evaluate_schedules(Local::now())` from the daemon reconciliation loop with a last-evaluated-minute guard.
- [ ] Run the focused test and observe GREEN.
- [ ] Commit `feat: expose cron schedules through daemon ipc`.

### Task 5: LLM Preview And Confirmation UX

**Files:** create `yi-agent-rs/crates/yi-agent/src/schedule_intent.rs`; modify `yi-agent-rs/crates/yi-agent/src/main.rs`, `yi-agent-rs/crates/yi-agent/src/config.rs`, `yi-agent-rs/crates/yi-agent/src/tui/slash.rs`, and `yi-agent-rs/crates/yi-agent/src/tui/app.rs`.

- [ ] Add scripted-Provider tests where JSON cron/objective yields a preview but no creation IPC, while malformed or ambiguous output becomes `ClarificationRequired`.
- [ ] Run `cargo test -p yi-agent --bin yi-agent schedule_intent`; observe RED.
- [ ] Implement `ScheduleIntentParser` around the configured Provider. Use a fixed JSON-only extraction prompt with local time zone and conservative defaults; only parse cron/objective and revalidate through the store contract. Model output cannot set policy, authority, limits, or provider profile.
- [ ] Add CLI `schedule add <natural-language>` plus explicit `--confirm`, then TUI `/schedule <natural-language>` preview/confirmation. Keep the preview client-local; cancellation and ambiguity send no create IPC.
- [ ] Run `cargo test -p yi-agent --bin yi-agent schedule`; observe GREEN without real API keys.
- [ ] Commit `feat: preview natural language schedules`.

### Task 6: Final Evidence

**Files:** `docs/project-management/subagent-runtime.md` and `docs/project-management/README.md`.

- [ ] Record the completed scheduled-root feature with exact commands and update its count.
- [ ] Before each test, run `ps aux | rg '[c]argo|[r]ustc|yi_agent' || true`; then run `cargo test -p yi-agent-store --test scheduler`, `cargo test -p yi-agent-store --test runtime_coordinator`, `cargo test -p yi-agent-store --test runtime_ipc`, `cargo test -p yi-agent`, `cargo fmt --all`, `just fmt-check`, and `git diff --check` serially.
- [ ] Report any pre-existing warnings separately and commit `docs: record scheduled runtime evidence`.
