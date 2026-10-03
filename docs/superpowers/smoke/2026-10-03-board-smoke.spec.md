# Smoke Spec — board launch verification

**Goal:** verify that a queued card is claimed by the scheduler, runs as a visible
session, and settles to `awaiting_merge` — without touching any product code.

## Scope

- Produce exactly one new file at the **worktree root**: `SMOKE-DONE.md`.
- Its content is a single line: `board smoke ok`.
- Commit that file on the card's own branch.
- Change nothing else. Do not edit product code, tests, or docs.
- Do not merge. Report the branch name and the commit hash.

## Acceptance

1. `SMOKE-DONE.md` exists at the worktree root with the expected line.
2. One commit exists on the card's branch adding only that file.
3. The session reports `awaiting_merge` (integration left to the human).
