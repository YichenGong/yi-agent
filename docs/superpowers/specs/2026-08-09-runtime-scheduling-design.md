# Runtime Scheduling And Budget Design

## References

- [Architecture decision index](2026-08-09-subagent-architecture-design.md)
- Current Cargo concurrency constraints in `CLAUDE.md`
- [Codex budget and execution research](../../research/2026-07-26-codex-long-running-task.md#2-多层-token-预算系统)

## Defaults

```text
resident subagents: 16 global, 16 maximum per root session
queued tasks:       64
depth/direct child: 2 / 4
LLM permits:        8 per provider/API-key; reserve 1 for coordination
coding permits:     6
host builds:        2
Cargo workspace:    1 exclusive lease for this project
scheduled default:  read-only, background, 4 residents, 30 turns, 15 minutes
```

Limits compose by minimum; path/tool authority composes by intersection. User
runtime ceilings > project ceilings > root-session selections > child contracts.
Only a user can raise a root selection within the user ceiling.

## Admission

Each resource has a queue. The Coordinator selects a root session by weighted
fair rotation with age boost, then selects a runnable subtree by the same rule.
Interactive roots outweigh background schedules; unused shares are borrowed and
reclaimed only on future admission, never by killing a running task. A task
holds only the permits it actively needs.

Resource requests are generic: `(scope, key, mode, units)`. Cargo is one
project adapter rule, not coordinator special logic. A parent waiting for child
results yields its LLM permit; one coordination permit protects completion,
permission, and review processing from leaf saturation.

## Watchdogs

Every attempt has turn/token/cost, wall-clock, idle-progress, retry, rework,
and resource-wait bounds. Meaningful progress excludes generated text and
repeated stdout. Exceeded limits create visible terminal states and release all
permits. Scheduled overlap defaults to skip; missed daemon-offline runs default
to skip.

## Required Tests

- Equal projects share capacity; an idle project lends capacity.
- Aged background work eventually admits.
- Depth-two descendants cannot evade root/global limits.
- Coordination permit admits an eligible parent under leaf saturation.
- Resource deadline and every watchdog release permits exactly once.
