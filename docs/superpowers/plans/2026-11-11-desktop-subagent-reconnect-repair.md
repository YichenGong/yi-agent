# Desktop subagent reconnect repair implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to execute task-by-task. Steps use checkbox syntax.

**Goal:** 修复 desktop managed subagent 首次准备和重连后无法委派，并以实际 child 执行作为验收。

**Architecture:** 保持 guardian 授权检查先于持久化查询；已授权的稳定 root 身份用于恢复 attachment，而非把每代 capability 当作新任务。桌面准备错误必须可见、可显式重试，恢复只作用于下一次用户请求，不重放副作用。

**Tech Stack:** Rust/SQLite/Tauri/React/Vitest。

## Global constraints

- 在 `.worktrees/desktop-subagent-fix`、`fix/desktop-subagent` 工作，不修改已有未跟踪文件。
- 不降低 managed 授权、不自动杀正式实例、不删除历史、不重放 spawn。
- 不新增依赖；保留 schema/MSRV，优先使用已有附件行恢复。
- 默认测试不调用真实模型；真实 App 验收使用隔离实例。

## Task 1: authorized root reconnect

Files: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`, `src/repository/application_root.rs`, `tests/managed_attach_authority.rs`。

- [ ] 写行为测试：第一次授权 attach 后，重建 coordinator/ledger，以同 root ID、新 capability attach；要求返回原 session/root，并拒绝旧 capability。
- [ ] 运行 `cargo test --all-features -p yi-agent-store --test managed_attach_authority`，确认重复 tasks.id 的 red。
- [ ] 授权通过后按 root ID 查现有附件；沿现有 adopt 路径恢复 workspace/roots/supervisor/owner，detach 恢复使用附件原始键；不按未授权输入查 root。
- [ ] 复跑上述测试，增加错误 workspace、撤销、已有 capability-keyed 历史行覆盖。

## Task 2: desktop readiness/retry

Files: `desktop/src/hooks/useRuntimeLease.ts`, 对应测试、`desktop/src/App.tsx`，必要时 app-server turn/resume 装配路径。

- [ ] 对当前实测的首次 lifecycle-ready 竞态写失败回归：准备失败可见，同项目显式 retry 确实再次 acquire，迟到响应不覆盖当前项目，不自动循环。
- [ ] hook 返回按项目的准备反馈与 retry，App 显示失败与重试入口；成功重试后下一次显式 resume 重建工具，不重复发送用户消息。
- [ ] `npm test`、`npm run build`，并验证失败不影响只读历史。

## Task 3: complete delegation verification

Files: `yi-agent-rs/crates/yi-agent/tests/managed_production_integration.rs`，必要的隔离验收脚本/记录，`docs/project-management/subagent-runtime.md`、README 与 `docs/bug-list.md`。

- [ ] 扩展真实 sidecar fixture，要求 parent schema 含 spawn_agent，真实 child shell echo hello、parent wait 成功，而非仅 trace 不含错误。
- [ ] 用隔离 App 窗口验证首次与重开历史会话均能委派；无证据保持验收项未完成。
- [ ] `cargo fmt --all && just ci`、desktop 测试/build，独立审查，再提交；按本轮项目规则集成，合并结果复验。
