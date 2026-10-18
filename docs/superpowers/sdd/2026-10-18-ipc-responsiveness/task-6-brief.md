### Task 6: Verify end to end and record evidence

**Rationale:** The spec's §6 acceptance criteria and §6 evidence boundary must be produced as artifacts, and §7 delivery rules honoured (source only, no process restarts).

**Files:**
- Create: `docs/superpowers/plans/2026-10-18-ipc-responsiveness-evidence.md`
- Modify: `docs/project-management/` module file(s) if the repo tracks these fixes there.

**Interfaces:**
- Consumes: all prior tasks.
- Produces: no code.

- [ ] **Step 1: Run each affected crate's tests**

```bash
ps aux | grep -v grep | grep cargo   # must be empty
cd plugins/superpowers-kanban && cargo test
cd ../../yi-agent-rs && cargo test -p yi-agent-store --lib
cd ../../yi-agent-rs && cargo test -p yi-agent-app-server --lib
```
Expected: all PASS.

- [ ] **Step 2: Confirm formatting is clean**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/ipc-responsiveness/yi-agent-rs && cargo fmt --all --check
cd ../../plugins/superpowers-kanban && cargo fmt --all --check
```
Expected: no output, exit 0.

- [ ] **Step 3: Build the plugin binary and measure offline (does not touch the running plugin)**

```bash
cd plugins/superpowers-kanban && cargo build --release
```
Then run the new binary against a scratch state dir and measure `list` latency 30 times, asserting median < 5ms and no sample > 50ms. Record the raw numbers (this is the offline replacement for the running instance, which must not be restarted).

- [ ] **Step 4: Write the evidence file**

Record, in `docs/superpowers/plans/2026-10-18-ipc-responsiveness-evidence.md`: the pre-change baseline from the spec §1.1; the post-change offline results from Steps 1-3; and an explicit statement that Items 1 and 3 only take effect on the running desktop after the user restarts the plugin and daemon (per spec §6 evidence boundary and §7).

- [ ] **Step 5: Commit**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/ipc-responsiveness
git add docs/
git commit -m "docs(perf): record ipc responsiveness verification evidence"
```

---

## Self-Review Notes

- **Spec coverage:** Task 1 = spec §3 (Item 1). Task 2 + Task 3 = spec §5 (Item 3). Task 4 = spec §4.1-4.3 (Item 2, D). Task 5 = spec §4.4 (Item 2, A'). Task 6 = spec §6/§7. The `lead_ticker` note (spec §4.4) is informational; no task changes it, matching the spec.
- **Ordering:** Tasks 1-3 are independent. Task 4 must precede Task 5 only because both edit `server.rs`; either order works if rebased. No task depends on another's new API.
- **Naming consistency:** `BoardCardSnapshot` must be defined **once**, in `server.rs`, and imported in `card_scheduler.rs` via `use crate::server::BoardCardSnapshot;`. The `card_scheduler.rs` snippet in Task 5 Step 3 re-declares it — implementers must replace that line with the import. No other duplicate definitions exist.
