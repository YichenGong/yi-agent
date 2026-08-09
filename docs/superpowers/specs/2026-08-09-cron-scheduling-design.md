# Cron Scheduled Root Design

## Scope

This design completes the schedule portion of the runtime-scheduling design.
It introduces durable, conservative, cron-triggered root sessions without
changing the default authority of interactive sessions.

## Cron Contract

Schedules accept exactly the five-field Cron form:

```text
minute hour day-of-month month day-of-week
```

The daemon evaluates expressions in its local time zone. Seconds are not
accepted. An invalid expression is rejected when a schedule is created or
updated; it is never stored for deferred interpretation. The implementation
uses a maintained Rust Cron parser rather than a project-local parser, with
its parser configuration constrained to this five-field contract.

## Natural-Language Creation

The CLI and TUI may accept a natural-language schedule request such as
"every weekday at 09:00, check project tasks." The parser produces a preview
containing the five-field Cron expression, local time zone, objective, and
conservative default policy. It does not create or modify a schedule.

The user must explicitly confirm that preview before the normal validated
schedule-create path persists it. Ambiguous input produces a clarification
request; it never guesses a cadence, time zone, or permission elevation. The
natural-language layer is therefore an ergonomic front end, not a second
execution format or an authority bypass.

## Durable Model

`schedules` owns an immutable `ScheduleDefinition`, a state, and the next due
time. The definition contains the Cron expression, objective, overlap and
missed-run policy, plus a contract snapshot. The snapshot defaults to
read-only authority, background priority, four resident workers, 30 turns,
and 900 seconds. Schedule creation rejects snapshots that elevate a daemon
provider, permission, or read-only default without explicit user approval.

Each due occurrence has a durable schedule event. Events distinguish normal
fires, skipped overlaps, missed daemon-offline occurrences, and an opt-in
single catch-up fire. Persisting the occurrence before creating a root prevents
duplicate roots when a daemon restarts during a tick.

## Fire State Machine

```text
due occurrence
  -> active instance exists and overlap=skip  -> ScheduleSkippedOverlap
  -> daemon was offline and missed=skip       -> ScheduleMissed
  -> daemon was offline and catch_up_once     -> one catch-up root
  -> otherwise                                -> new isolated root session
```

A new root is created through `RuntimeCoordinator::create_session_with_objective`.
It receives the scheduled objective and contract snapshot only. It does not
reuse an interactive session, mailbox, Agent session history, active task, or
provider turn lease. Normal runtime admission decides when the root can run.

After a fire, the repository advances `next_run_at` atomically to the next
Cron time. An occurrence key (`schedule_id`, due-at) is unique so evaluating a
tick more than once is idempotent.

## Public Boundaries

- `schedule.rs` defines validated schedule definitions, policies, and the Cron
  evaluator.
- `repository.rs` stores schedule definitions and atomically claims evaluated
  occurrences.
- `runtime.rs` turns a claimed occurrence into a fresh root session and starts
  normal admission.
- The daemon tick invokes the runtime evaluator using its local current time.
- IPC/CLI surface exposes only the validated five-field representation; it
  must not accept a second-based or system-cron alternative.
- Natural-language input returns a confirmation preview and calls the same
  validated create operation only after user confirmation.

## Failure Handling

No parser failure is deferred to a daemon tick. Repository transactions roll
back the occurrence claim if creating its root cannot be durably completed.
If the daemon was stopped, elapsed occurrences emit a visible missed event;
the default does not create backlog. `catch_up_once` creates at most one root
and advances past every earlier missed occurrence. A running root remains
visible to overlap logic until its task tree is terminal.

## Tests

- A five-field expression is accepted and a six-field expression is rejected.
- Natural-language input produces a five-field preview but creates no schedule
  until the user confirms it; ambiguous input requires clarification.
- The evaluator advances `next_run_at` in the daemon local time zone.
- One due occurrence creates one isolated root whose objective and contract
  snapshot match the schedule, with no interactive mailbox/history.
- Repeated evaluation at the same due time creates no duplicate root.
- A due schedule with an active instance emits `ScheduleSkippedOverlap` by
  default.
- Offline missed occurrences emit `ScheduleMissed` by default; `catch_up_once`
  makes exactly one isolated root.
- Default schedule contracts are read-only/background/four residents/30 turns/
  900 seconds, and unapproved elevation is rejected.
