# Desktop subagent reconnect repair implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to execute task-by-task. Steps use checkbox syntax.

**Goal:** 修复 desktop managed subagent 首次准备和重连后无法委派，并以实际 child 执行作为验收。

**Architecture:** guardian 授权检查先于持久化查询；已授权的稳定 root 身份用于恢复 attachment，而非把每代 capability 当作新任务。桌面准备错误可见、可显式重试，恢复只作用于下一次用户请求，不重放副作用。

**Tech Stack:** Rust/SQLite/Tauri/React/Vitest。

## Global constraints

- 在 `.worktrees/desktop-subagent-fix`、`fix/desktop-subagent` 工作，不修改已有未跟踪文件。
- 不降低 managed 授权、不自动杀正式实例、不删除历史、不重放 spawn。
- 不新增依赖；保留 schema/MSRV，优先使用已有附件行恢复。
- 默认测试不调用付费模型；App 验收使用隔离实例。未经用户决定不合并或替换正式 App。

## Task 1: authorized root reconnect

Files: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`, `src/repository/application_root.rs`, `tests/managed_attach_authority.rs`，及 lifecycle authorization/gate。

- [x] 行为测试覆盖同稳定 root、新 capability、重建 coordinator、新 client、旧 capability 拒绝。
- [x] 记录重复 tasks.id 的 RED，授权后按 root ID 查既有 attachment，保持 message capability 与授权 capability 分离。
- [x] 恢复 workspace/linked roots/supervisor/owner；detach 恢复用附件原始键；错误 workspace、撤销、历史 capability-keyed 行保持安全边界。
- [x] 审查后补生产 RevokeClient 准入关闭、client/root tombstone 分离、owner 定向 gate、旧提交拒绝及未确认停止检查。27 条 managed attach 测试通过；precommit barrier 与 cancel-not-stop 有 RED/GREEN 证据。

## Task 2: desktop readiness/retry

Files: `desktop/src/hooks/useRuntimeLease.ts`、测试、`desktop/src/App.tsx`、native runtime_lifecycle；app-server start/resume/driver 装配路径。

- [x] 准备失败可见，同项目与尚未活动项目的显式 retry 确实再次 acquire，迟到响应隔离，不自动循环。
- [x] held 与 ready 分离，保留失败重试后的释放义务；首次 thread/start 等待准备，失败不发请求；重试不自动发消息或重建会话。
- [x] managed 装配失败明确返回错误，旧 root 登记不丢失，显式重试重新检查；只读历史保持 history-first。
- [x] frontend 196 files / 2919 tests、build 通过；native 292 tests 通过；app-server 1637 passed / 7 skipped。

## Task 3: complete delegation verification

Files: `yi-agent-rs/crates/yi-agent/tests/managed_production_integration.rs`、`managed_sidecar_harness/mod.rs`、项目管理与 bug-list。

- [x] 真实 managed sidecar parent schema 含 spawn_agent，实际 child shell echo hello、stdout/exit 0、parent 同 task wait 完成；重开历史再次委派。31 条该集成 target 的测试通过。
- [x] 有界帧读写、stderr drain、HTTP Content-Length、网关 RAII，授权请求严格匹配 secret/epoch/scope/client/thread/workspace/operation；意外 approval 明确失败，不夹具代批。
- [x] `cargo fmt --all`、完整 `NEXTEST_TEST_THREADS=2 just ci` 22 gates（6442 passed / 41 skipped）、desktop 测试/build、独立审查；默认高并发既有 WS pairing initialize reset 有记录，独占复跑与完整低并发 CI 通过，未跳过或放宽断言。
- [ ] 隔离 App 窗口验证首次与重开历史会话均能委派。当前辅助功能未授权，不能以 stdio 或 prepare 成功替代。独立 bundle 已构建，必须刷新至最终提交与前端再试用。
- [ ] 用户完成窗口验收后决定是否合并与正式部署；本轮不得自行合并或替换正式 App。

本地证据：`.superpowers/sdd/2026-11-11-desktop-subagent-reconnect-repair/`；进度索引见 `docs/project-management/subagent-runtime.md`。
