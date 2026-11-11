# Desktop content timeout: dependency clearance and release plan

**Goal:** Finish the management-reader starvation fix without weakening authorization or extending watchdogs; clear the existing dependency-audit blockers before integration.

**Architecture:** Keep local authenticated management dispatch independent of the business loop. Upgrade only SQLite and MCP dependencies required by the security advisories. Preserve workspace MSRV 1.89 and existing bundled SQLite behavior.

## Checklist

- [x] Reproduce business-loop starvation and reader shutdown failures; implement management ingress fix and regression tests.
- [x] Upgrade both rusqlite declarations to a version using libsqlite3-sys >=0.39; upgrade both rmcp declarations to >=3.9; update only the affected lockfile closure.
- [x] Run MCP, store, supervisors and management regressions with inherited YI_AGENT_* variables removed. Resolve demonstrated compatibility failures only.
- [x] Run cargo fmt --all, cargo audit, and full just ci including MSRV verification. Update module progress records with actual results.
- [ ] Request independent review, commit, merge --no-ff, and repeat just ci on the merge result.
- [ ] Build and verify signed runtime bundle without invoking the process-stopping rebuild wrapper. Deploy with a controlled runtime handover; do not terminate the guardian hosting this active task blindly.
- [ ] Verify runtime identity and same-version content recovery. Report separately any visual/device verification unavailable in this environment.

Security acceptance: no new advisory ignores; RUSTSEC-2026-0459/0457/0462/0461 must disappear from cargo audit. Existing RED baseline: audit fails with four vulnerabilities; management ingress regressions fail on the original reader.

Commands run from yi-agent-rs in the isolated fix/desktop-content-timeout worktree. Reuse the main target cache serially; scrub YI_AGENT_* only in validation child processes. Do not modify the user's untracked probe_guardian.rs.
