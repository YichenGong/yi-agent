# Smoke Plan — board launch verification

Follow the spec at `docs/superpowers/smoke/2026-10-03-board-smoke.spec.md`.

## Steps

1. **Create** `SMOKE-DONE.md` at the worktree root, containing exactly:

   ```
   board smoke ok
   ```

2. **Stage and commit** only that file on the current branch:

   ```bash
   git add SMOKE-DONE.md
   git commit -m "chore(smoke): verify the kanban launch path"
   ```

3. **Verify**: `git log --oneline -1` shows the commit; `git status` is clean.

4. **Stop.** Do not merge. Do not run the test suite. Report the branch name and
   the commit hash, then stop so the card can settle to `awaiting_merge`.

## Guardrails

- Exactly one new file (`SMOKE-DONE.md`). No other file may change.
- No product code, no tests, no dependency changes.
- Never merge.
