# Desktop history-first recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox syntax for tracking.

**Goal:** 重启 desktop / 打开历史会话先看到缓存，后台逐块更新，不等待运行环境准备。

**Architecture:** 本地独立有界快照，沿用现有缓存归一化与 LRU；本地 opt-in historyReady 解耦正文同步与运行环境激活。Remote 旧协议保持不变。

**Tech Stack:** React / TypeScript / IndexedDB / Vitest，Rust / Tokio / ts-rs。

## Global constraints

用户已批准设计 `docs/superpowers/specs/2026-11-11-desktop-history-first-design.md`。不触碰真实活跃会话、正式 App 或密钥；worktree 开发，提交前与合并后完整 just ci。缓存仅 LOCAL_SLOT，独立数据库与列表键；10 会话 / 5 MiB / 单条目标 1 MiB，节流 800 ms。

## Evidence

- 2026-11-11 isolated release-sidecar probe, copy of 34.85 MiB history: initialize 6 ms, readItems 96 ms, embedded resume 98 ms (first replay 79 ms). Not a native managed-runtime timing claim.
- Existing resume awaits factory and managed runtime/root attachment before emitting history. Frontend includes that time in transcript sync and startup handshake.
- Preserve strict activation metadata, model checks, bounded/chunked replay, crashed partial recovery, running-thread read-only behavior, and authority gates.

## Work checklist

- [x] RED/GREEN backend regression: block factory, observe complete history + request-correlated historyReady before releasing activation; legacy clients retain original ordering.
- [x] Opt-in historyFirst + opaque historyToken; targeted historyReady must not unlock another client or replay.
- [x] RED/GREEN UI regression: transcript synchronization and startup readiness complete before activation; sending awaits successful activation and preserves draft on error/switch/reconnect.
- [x] Keep activation failure visible, no timeout retroactively invalidating already-loaded history, no stale/duplicate replay items.
- [x] Run frontend tests/build, backend tests/bindings/fmt, full just ci before commit, update project-management counts and verification notes.
- [x] Desktop 快照 RED/GREEN：`App.test.tsx -t 'App desktop history snapshot'`，initialize/listAll 被扣住仍画正文，回放批次就地更新，historyReady 才删 ghost，页面隐藏后重挂载恢复，remote 缓存不受影响。
- [x] 本地与远端分库分键；保持 10 条 / 5 MiB / 单条目标 1 MiB 上限与 800 ms 防抖；缓存异常降级，不阻塞握手。
- [x] 裁剪尾部回放顺序 RED/GREEN：`session.test.ts` 覆盖 batch/单条全量 replay 把旧头部插在 cached tail 前，不丢本地 echo/notice。运行中裁剪缓存用只读全量 readItems，不 resume。
- [x] 本地定向 historyFirst 补强：非 local 忽略 opt-in；订阅过滤不能让 ready 超前于正文；partial / usage 只回放一次，激活前不 promote。
- [x] 原生双启动验收：第一次保存真实 IDB 快照，第二次延迟启动 RPC 5 秒，在放行前已见正文；保留长历史 3251 项完整回放证据。
- [x] Package/isolated native verification before claiming installed desktop fixed; do not terminate the active host or overwrite user data.

## Integration checkpoint

- [x] Independent final review completed; findings covered by bounded activation wait, competing-read guard, full read before cache metadata, linear snapshot trimming, and stale-client list regression tests.
- [x] Commit verified implementation and project-management / regression records on `fix/desktop-startup-content`.
- [x] User selected local integration: code merged with `--no-ff` as `f6f4fea4`; complete `just ci` passed on that exact merged commit in a clean worktree (6415 tests + 50 bindings). Main-directory frontend passed 2879 tests and build. See regression record for the unrelated untracked-file fmt obstruction and verification logs.
- Formal App deployment is deliberately not included: the active host and current work must not be interrupted.
