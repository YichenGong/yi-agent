# Desktop 子 agent 执行面板重新设计 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 desktop 子 agent 侧栏改成默认可读的执行时间线，兼顾窄栏、任务导航、实时阅读、草稿和安全停止操作。

**Architecture:** 纯函数负责状态、事件解析与快照合并；专用 controller 组合订阅序列和任务动作记录，React hook 只负责稳定实例及订阅渲染。展示拆成列表、事件、时间线、消息区与详情组合；App/router 只传递捕获的连接身份和生命周期。Rust 仅修正 trace unwatch 的资源清理边界。

**Tech Stack:** React 19、TypeScript 6、Tailwind 4、Vitest 3、Testing Library、现有 MarkdownText/useStickToBottom、Rust/Tokio、Node 原生 WebSocket + Chrome CDP。无新增依赖。

## Global Constraints

- 设计依据：[spec](../specs/2026-11-07-desktop-subagent-trace-redesign-design.md)；审查依据：[review](../specs/2026-11-07-desktop-subagent-trace-redesign-review.md)。冲突时以 spec 为准，先修订计划再实现。
- 只做 desktop，不新增手机入口，不修改 Git Diff/后台进程的内容布局。
- 保留三个 Tab、栏宽持久化与拖拽（260–720px，默认 360px）。
- 不增加 RPC 方法、字段或第三方依赖；不手改 generated bindings，不补 daemon 目标元数据。
- 当前 objective/lastStep 为空时真实 UI 是 taskId + 中文状态，不虚构目标、步骤、时间、耗时、配对或完整工具输出。
- 首次只挂载最新 200 块，每次载入更早最多 200 块；全量数据保留，追加不自动收窄范围。持续追加/用户全载后的 DOM 增长是已知边界，不做虚拟列表。
- 长结果：逻辑行超过 3 行或超过 240 个 Unicode code point 才折叠；第一逻辑行最多 240 code point 作预览。
- 消息与订阅的 UI 等待提示为 15 秒，不以 Promise.race 释放真实请求锁；现有服务端同步 IPC 的 reply read timeout 为 30 秒，非端到端上界。
- highWaterId = 当前任务快照末行 eventId，空快照为 0；不是行数或全局最大值。live 通知可能丢帧，不做缺号补拉/轮询，只手动刷新快照校准。
- 普通重连保留草稿，换绑/重配失效相应记录；草稿不写磁盘，不占用主会话 ThreadStore.draft。
- 复用 useStickToBottom 的 4px 容差，不修改共享 hook；真实几何用浏览器测，不以 jsdom 替代。
- 深浅主题文字对实际背景对比度至少 4.5:1；最小窗口 900×600、大字号默认正文可见高度至少 120px。
- Mock/deferred/fake timers 测试，不调用真实 LLM、生产 sidecar 或生产会话。
- 所有变更在隔离 worktree；禁止直接修改 main。当前工作目录为 `.worktrees/desktop-subagent-trace-redesign`，分支 `feat/desktop-subagent-trace-redesign`。执行时先确认它仍未被占用/合并；不重置别人的改动。
- 每个实现提交前在 `yi-agent-rs/` 运行 `cargo fmt --all && just ci`，通过后才提交；以下定向命令是迭代反馈，不代替此门禁。每组需求同步模块进度文档；全部验收前保持 `[ ]`，不使用进行中状态标记。

## 0. 文件结构与固定接口

### 文件职责

| 文件 | 操作 | 职责 |
|---|---|---|
| `desktop/src/lib/subagents.ts`、`.test.ts` | 修改 | 共享状态词汇、终态/活动判断；保留 rail store，迁移旧 trace helper 调用点 |
| `desktop/src/lib/subagentTraceModel.ts`、`.test.ts` | 新建 | 严格事件解析、连续文本折叠、快照合并、摘要预览、TTL 纯函数 |
| `desktop/src/lib/subagentTraceSubscription.ts`、`.test.ts` | 新建 | 拥有者、watch/unwatch 队列、缓冲、超时、连接代际 |
| `desktop/src/lib/subagentTraceActions.ts`、`.test.ts` | 新建 | 分任务草稿/revision、pending 守卫、消息与取消生命周期 |
| `desktop/src/hooks/useSubagentTrace.ts`、`.test.tsx` | 新建 | 组合 controller、稳定事件入口、React 订阅与清理 |
| `desktop/src/test/subagentTraceFixtures.ts` | 新建 | 本功能共享 typed fixture/deferred/mock client，不引入通用测试框架 |
| `desktop/src/components/SubagentRail.tsx`、`.test.tsx` | 修改 | 细分隔列表、中文状态、真实空元数据降级 |
| `desktop/src/components/TraceEvent.tsx`、`.test.tsx` | 新建 | Markdown/工具调用/结果摘要/原始载荷的按值 memo 展示 |
| `desktop/src/components/TraceTimeline.tsx`、`.test.tsx` | 新建 | 渐进展示、唯一正文滚动区、完整信息导航 |
| `desktop/src/hooks/useTraceScroll.ts`、`.test.tsx` | 新建 | 本栏阅读锚点和内容 ResizeObserver，复用共享 sticky hook |
| `desktop/src/components/TraceComposer.tsx`、`.test.tsx` | 新建 | 受控输入、IME、内联状态、两阶段停止确认与焦点 |
| `desktop/src/components/SubagentDetailView.tsx`、`.test.tsx` | 临时新建后迁移 | Task 9 独立完成新详情；Task 10 迁移到 SubagentTrace，移除临时文件，保证每步可编译 |
| `desktop/src/components/SubagentTrace.tsx`、`.test.tsx` | 修改 | Task 10 替换旧详情，最终组合紧凑摘要、父子导航、时间线和 composer |
| `desktop/src/components/ThreadDetailPanel.tsx`、`.test.tsx` | 修改 | 原三 Tab/拖拽外壳、返回焦点、传递新 trace props |
| `desktop/src/index.css`、`desktop/src/lib/themeTokens.test.ts` | 修改 | 局部 trace token/窄栏 prose/折行/字号，不改主对话样式 |
| `desktop/src/App.tsx`、`.test.tsx` | 修改 | 导航/关闭/删除/重配接线，移除可选链写操作 |
| `desktop/src/hooks/useSessionState.ts`、`.test.tsx` | 修改 | 移走 trace 状态和旧 read→watch，只保留会话/rail/面板编排 |
| `desktop/src/hooks/useNotificationRouter.ts`、`.test.ts` | 修改/新建测试 | 带源 client/slot 委派 trace、终态与断开通知 |
| `desktop/src/hooks/connectionLifecycle.ts`、`desktop/src/hooks/connectionLifecycle.test.ts` | 修改/新建测试 | 每次新 client 登记连接代际，不把重连当换绑 |
| `desktop/scripts/browser/subagent-trace.{html,tsx}`、`subagent-trace-check.mjs` | 新建 | 真实组件/CSS 的独立浏览器验收 |
| `desktop/package.json`、`desktop/scripts/browser/README.md` | 修改 | 浏览器脚本入口与运行说明，不增加依赖 |
| `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` | 修改 | 显式 re-export 两个清理 helper |
| `yi-agent-rs/crates/yi-agent-app-server/src/server/watchers.rs` | 修改 | 窄清理 helper；不改实际 watcher 推流语义 |
| `yi-agent-rs/crates/yi-agent-app-server/src/rpc/agent.rs` | 修改 | unwatch 只调用 trace 清理 |
| `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread/lifecycle/delete.rs` | 修改 | 调用全量清理 helper，保持旧语义 |
| `yi-agent-rs/crates/yi-agent-app-server/src/rpc/mod.rs` | 修改 | 修正 watcher 生命周期注释 |
| `yi-agent-rs/crates/yi-agent-app-server/src/server/tests/process.rs` | 修改 | unwatch 保留其它 watcher、删除全清、RPC 幂等回归 |
| `docs/project-management/{desktop.md,README.md}` | 修改 | 同组提交记录验收依据，最终同步完成数 |

### 跨任务类型契约

以下是将要实现的本地接口，不是修改 RPC。先建立这些类型，再让后续任务依赖；不能各自发明另一套同义名字。

```ts
// subagentTraceModel.ts
import type { AgentTraceRow, AgentTraceSnapshotResult } from "./protocol";
export interface TraceBlock {
  key: string;                 // taskId + 首 eventId，不使用数组下标
  firstEventId: number;
  kind: "assistant_text" | "tool_call" | "tool_result" | "state_note" | "raw";
  sourceKind: string;
  text: string;
  name: string | null;
  isError: boolean;
  rawPayload: string | null;   // raw kind 保存原文，其它为 null
}
export interface SummaryPreview { folded: boolean; preview: string; full: string }
export function decodeTraceRow(row: AgentTraceRow): TraceBlock;
export function foldTraceRows(rows: readonly AgentTraceRow[]): TraceBlock[];
export function mergeTraceSnapshot(
  taskId: string, snapshot: AgentTraceSnapshotResult,
  buffered: readonly AgentTraceRow[],
): AgentTraceRow[];
export function mergeTraceEvent(
  taskId: string, rows: readonly AgentTraceRow[], event: AgentTraceRow,
): AgentTraceRow[];
export function summarizeTrace(text: string): SummaryPreview;
export function cancelDeadline(startedAt: number, ttlSeconds: number): number | null;
```

```ts
// subagentTraceSubscription.ts
import type { Slot } from "./slots";
import type { AgentTraceRow } from "./protocol";
import type { RpcClient } from "./rpc";
export type TraceClient = Pick<RpcClient, "request">;
export interface TraceTarget {
  slot: Slot;
  serverBindingGeneration: number;
  threadId: string;
  taskId: string;
}
export function traceTargetKey(target: TraceTarget): string;
export interface TraceConnection {
  slot: Slot;
  serverBindingGeneration: number;
  connectionGeneration: number;
  client: TraceClient;
}
export interface TraceOwner extends TraceTarget {
  client: TraceClient;         // 对象 identity；不序列化
  connectionGeneration: number;
  requestGeneration: number;
}
export interface TraceSubscriptionSnapshot {
  owner: TraceOwner | null;
  phase: "idle" | "loading" | "ready" | "error";
  rows: readonly AgentTraceRow[];
  error: string | null;
  disconnected: boolean;
  waitingForPriorRequest: boolean;
}
export interface TraceEventParams {
  threadId: string; taskId: string; row: AgentTraceRow;
}
export class TraceSubscription {
  constructor();
  subscribe(listener: () => void): () => void;
  getSnapshot(): TraceSubscriptionSnapshot;
  open(target: TraceTarget, connection: TraceConnection | null): void;
  retry(connection: TraceConnection | null): void;
  close(): void;
  accept(connection: TraceConnection, params: TraceEventParams): void;
  disconnected(connection: TraceConnection): void;
  dispose(): void;
}
```

```ts
// subagentTraceActions.ts
import type { SubagentRow } from "./subagents";
import type { TraceTarget, TraceConnection } from "./subagentTraceSubscription";
export type TraceOperation = "message" | "preview" | "confirm";
export interface CancelConfirmation {
  token: string; taskIds: readonly string[]; deadline: number;
}
export interface TraceActionSnapshot {
  draft: string;
  draftRevision: number;
  messagePending: boolean;
  previewPending: boolean;
  confirmPending: boolean;
  confirmation: CancelConfirmation | null;
  error: string | null;
  notice: string | null;
}
export class TraceActions {
  constructor();
  subscribe(listener: () => void): () => void;
  read(target: TraceTarget): TraceActionSnapshot;
  setVisible(target: TraceTarget | null): void;
  setDraft(target: TraceTarget, text: string): void;
  updateTask(target: TraceTarget, row: SubagentRow | null): void;
  send(target: TraceTarget, connection: TraceConnection | null): Promise<void>;
  preview(target: TraceTarget, connection: TraceConnection | null): Promise<void>;
  confirm(target: TraceTarget, connection: TraceConnection | null): Promise<void>;
  dismiss(target: TraceTarget): void;
  disconnected(connection: TraceConnection): void;
  dropThread(slot: string, bindingGeneration: number, threadId: string): void;
  dropSlot(slot: string): void;
  clear(): void;
}
```

`traceTargetKey(target)` 的实现固定为 `JSON.stringify([target.slot, target.serverBindingGeneration, target.threadId, target.taskId])`。UI 的 identityKey 使用这个值，不含 connection/requestGeneration，因此普通刷新不重挂，而切任务/换绑会重置展示。Controller 在 Task 6 导出；UI 不再自己发 RPC 或维护另一份草稿。`TraceActions` 的合法 send/preview/confirm 成功 resolve void，连接/服务端失败 reject；controller/UI 边界 catch 并展示原记录 error，不漏 unhandled rejection。UI 超时只发布状态，不人为 resolve/reject 底层 RPC。

## 开始实现前的基线

- [ ] 在 worktree 执行并记录输出；已存在的失败单独登记，不能悄悄把本任务替换成修复无关功能。

```bash
git status --short && git branch --show-current
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cd desktop && npm ci && npx tsc --noEmit
npx vitest run src/lib/subagents.test.ts src/components/SubagentRail.test.tsx src/components/SubagentTrace.test.tsx src/components/ThreadDetailPanel.test.tsx
```

预期：依赖安装/类型检查/现有定向测试成功；若环境或基线失败，先使用 systematic-debugging 找根因，不声称本次红测已建立。查源码调用点用 `grep -R -n` 限定 desktop/src，跳过 node_modules/dist；本机无 rg 时不扩大到系统目录搜索。

### Task 1: 统一状态呈现与真实元数据降级（A01）

**Files:** 修改 `desktop/src/lib/subagents.ts`、`desktop/src/lib/subagents.test.ts`。

**Interfaces:** 保留 `isFinished(state: string): boolean`、`toRow(child: AgentChild): SubagentRow`。新增 `subagentState(state: string): {label: string; tone: "neutral" | "success" | "warning" | "danger"; finished: boolean; active: boolean}`；`TERMINAL_STATES` 从同一表派生。未知状态保留原值/空值为「状态未知」。

- [ ] **Step 1: 写失败测试。** 把旧的四终态用例换成 spec 全 15 状态表，并加入以下明确边界：

```ts
it("classifies paused, terminal waits and unknown without counting them active", () => {
  expect(isFinished("blocked")).toBe(true);
  expect(isFinished("recovery_required")).toBe(true);
  expect(subagentState("paused")).toMatchObject({ label: "已暂停", active: false, finished: false });
  expect(subagentState("waiting_for_permission")).toMatchObject({ label: "等待授权", active: true });
  expect(subagentState("future_state")).toMatchObject({ label: "future_state", active: false, finished: false });
  expect(subagentState("").label).toBe("状态未知");
});
```

- [ ] **Step 2: 确认 RED。** `cd desktop && npx vitest run src/lib/subagents.test.ts`；应因缺少 subagentState/blocked 被误判失败，而非测试收集错误。
- [ ] **Step 3: 建立唯一状态表。** 按 spec §3.1 原样录入 15 条；label/finished/active/tone 共用该表，`isFinished` 查询表；`toRow` 对所有九终态清 lastStep。把“never leaves”改为“当前执行终态”，不承诺容器不可恢复。

```ts
const UNKNOWN = { label: "状态未知", tone: "neutral", finished: false, active: false } as const;
export function subagentState(state: string) {
  return STATE_PRESENTATION[state] ?? { ...UNKNOWN, label: state || UNKNOWN.label };
}
export function isFinished(state: string): boolean {
  return subagentState(state).finished;
}
```

`STATE_PRESENTATION` 的类型是 `Readonly<Record<string, {label: string; tone: "neutral" | "success" | "warning" | "danger"; finished: boolean; active: boolean}>>`；全表内容以 spec 为唯一来源。查所有 `TERMINAL_STATES` 使用点，不能保留旧四态分支。
- [ ] **Step 4: 确认 GREEN。** 重跑本文件；追加断言所有九终态清 lastStep、paused 不计入 active、objective/lastStep null 不生成假文字。
- [ ] **Step 5: 更新模块证据并提交。** 在 desktop 未完成条目记本任务测试命令，仍 `[ ]`；执行全局提交门禁，再 `git add` 这两个源码文件与模块文档，`git commit -m "fix: align subagent state presentation with core"`。

### Task 2: 严格解析事件、合并快照与摘要（A02/A03/A06/A10）

**Files:** 新建 `desktop/src/lib/subagentTraceModel.ts`、`.test.ts`、`desktop/src/test/subagentTraceFixtures.ts`；迁移 `desktop/src/lib/subagents.ts`、`.test.ts` 的旧 trace helper；修改旧组件 import 保持本任务结束可编译。

**Interfaces:** 实现 §0 的 model 契约；fixture 导出 `traceRow(eventId, kind, payload, taskId = "task-a")`、`deferred<T>()`。旧 `toTraceLine` 删除并迁移所有调用点，不保留会吞载荷的兼容分支。

- [ ] **Step 1: 写失败测试与 fixture。**

```ts
export function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
export function traceRow(eventId: number, kind: string, payload: unknown, taskId = "task-a") {
  return { eventId, taskId, kind, payloadJson: JSON.stringify(payload) };
}
```

```ts
it("preserves malformed known events instead of rendering empty text", () => {
  for (const payload of [null, [], { text: 42 }, { type: "tool_call", text: "wrong tag" }]) {
    const row = traceRow(7, "assistant_text", payload);
    expect(decodeTraceRow(row)).toMatchObject({ kind: "raw", rawPayload: row.payloadJson });
  }
  const broken = { ...traceRow(8, "assistant_text", {}), payloadJson: "{broken" };
  expect(decodeTraceRow(broken).rawPayload).toBe("{broken");
});
it("uses snapshot precedence and only post-watermark buffered events", () => {
  const old = traceRow(10, "state_note", { note: "old" });
  const authoritative = traceRow(10, "state_note", { note: "snapshot" });
  const next = traceRow(24, "assistant_text", { text: "new" });
  const rows = mergeTraceSnapshot("task-a", { rows: [authoritative], highWaterId: 10 }, [old, next, next]);
  expect(rows.map(r => r.eventId)).toEqual([10, 24]);
  expect(rows[0]).toEqual(authoritative);
});
it("counts Unicode code points and explicit logical lines", () => {
  expect(summarizeTrace("😀".repeat(240)).folded).toBe(false);
  expect(summarizeTrace("😀".repeat(241)).preview).toBe("😀".repeat(240) + "…");
  expect(summarizeTrace("a\nb\nc").folded).toBe(false);
  expect(summarizeTrace("a\nb\nc\nd")).toMatchObject({ folded: true, preview: "a…" });
});
```

补字段矩阵：assistant text:string；call name/summary:string；result name/summary:string/is_error:boolean；note note:string；type 缺省允许、有则与 kind 一致。数组/null/损坏/错标签/错字段全部 raw。未知 kind raw，且不得与相邻 assistant 合并。
- [ ] **Step 2: 确认 RED。** `cd desktop && npx vitest run src/lib/subagentTraceModel.test.ts src/lib/subagents.test.ts`。旧测试“坏已知载荷显示空”的期待必须反转。
- [ ] **Step 3: 实现纯函数。** raw key 用 `JSON.stringify([row.taskId, row.eventId])`，保留 sourceKind/rawPayload；只合并相邻两个有效 assistant_text，沿用首块 key。merge 用 eventId Map 去重并数值排序，先过滤 taskId；快照合并只接纳 buffered.eventId > highWaterId。live 已有 ID 原样返回旧 rows 引用，不能二次拼文本。摘要 `Array.from(text)` 数 code point，换行用 `/\r\n|\r|\n/`。

```ts
export function cancelDeadline(startedAt: number, ttlSeconds: number): number | null {
  if (!Number.isFinite(startedAt) || !Number.isFinite(ttlSeconds) || ttlSeconds <= 0) return null;
  const deadline = startedAt + ttlSeconds * 1000;
  return Number.isFinite(deadline) ? deadline : null;
}
```

- [ ] **Step 4: 确认 GREEN。** 测试 snapshot 自身重复/乱序、wrong task、空水位0、不同 ID 不连续正常；结果自身 name、不配对；两段有效文本合并/坏载荷隔断；空摘要/首行空、240/241、3/4行、CRLF；TTL 0/负数/NaN/Infinity/已延迟过期。
- [ ] **Step 5: 更新模块证据、门禁与提交。** `git commit -m "refactor: model subagent trace events without data loss"`。提交前 `npx tsc --noEmit` 确认旧组件调用点迁移完整，然后执行全局提交门禁。

### Task 3: 修正服务端 watcher 清理边界（A08）

**Files:** 修改 `src/server.rs`、`src/server/watchers.rs`、`src/rpc/agent.rs`、`src/rpc/mod.rs`、`src/rpc/thread/lifecycle/delete.rs`、`src/server/tests/process.rs`（均在 `yi-agent-rs/crates/yi-agent-app-server/`）。

**Interfaces:** 在 server/watchers.rs 导出 `pub(crate) async fn stop_trace_watch(thread_id: &str, traces: &mut HashMap<String, TraceWatch>)` 和 `pub(crate) async fn stop_thread_watches(thread_id: &str, traces: &mut HashMap<String, TraceWatch>, children: &mut HashMap<String, ChildrenWatch>, processes: &mut HashMap<String, ProcessWatch>)`；在 server.rs 的 `pub(crate) use watchers::{...}` 显式加入这两个名称供 RPC 使用。后者调用前者再停另两类；不改变外部协议。

- [ ] **Step 1: 写失败回归。** 在 process.rs 造三个 mpsc 接收循环 task，各收到 ping 时送 ack，保存 AbortHandle。用相同 thread key 放三类 map，另放 thread-b 证明不误清。测试名固定 `trace_unwatch_preserves_children_and_process_watchers`、`thread_delete_stops_all_watchers`。RED 阶段先以三类 map 参数的过渡签名 `stop_trace_watch(thread_id, traces, children, processes)` 把原 unwatch 三类清理原样抽取，RPC 与测试都传三类 map，确认行为未变。以下是 GREEN 后收窄为仅 trace 参数的最终断言；RED 阶段首行同样传三类 map，其余断言不变，确保失败来自真实旧行为：

```rust
stop_trace_watch("thread-a", &mut traces).await;
assert!(traces.get("thread-a").is_none());
assert!(children.contains_key("thread-a"));
assert!(processes.contains_key("thread-a"));
children_ping.send(()).await.unwrap();
process_ping.send(()).await.unwrap();
assert_eq!(children_ack.recv().await, Some(()));
assert_eq!(process_ack.recv().await, Some(()));
assert!(!children_abort.is_finished());
assert!(!process_abort.is_finished());
```

夹具初始化的 task 用 `tokio::spawn(async move { while ping_rx.recv().await.is_some() { if ack_tx.send(()).await.is_err() { break; } } })`；channel 容量1，外层用 `tokio::time::timeout(Duration::from_secs(2), ...)`，测试失败不能永等。三类 watcher 持有 task；保存 `task.abort_handle()` 作终止断言。
- [ ] **Step 2: 确认 RED。** `cd yi-agent-rs && cargo test -p yi-agent-app-server trace_unwatch_`。失败必须是 children/process 被停，非符号缺失后不再继续。
- [ ] **Step 3: 最小修复。** GREEN 时把 stop_trace_watch 签名收窄为只接收 traces，同时机械更新测试和 RPC 调用，helper 只接触 traces；agent/unwatch 保留 known-thread 检查和 `{stopped:true}`，删除其它两类清理。delete.rs 用 stop_thread_watches 保持原先全清。修 rpc/mod.rs 的 process/children 注释，不改 watcher 所有权、多客户端共享模型或 daemon。

```rust
pub(crate) async fn stop_trace_watch(
    thread_id: &str,
    traces: &mut std::collections::HashMap<String, crate::server::TraceWatch>,
) {
    if let Some(watch) = traces.remove(thread_id) {
        watch.stop().await;
    }
}
```

- [ ] **Step 4: 确认 GREEN。** 跑 `cargo test -p yi-agent-app-server trace_unwatch_ && cargo test -p yi-agent-app-server thread_delete_stops_all_watchers`。保留现有真实 Harness 测试 `agent_trace_unwatch_is_idempotent`；另跑 `cargo test -p yi-agent-app-server thread_delete` 保护删除分派。检查真实 RPC 确实调用对应 helper，不能只测未被生产使用的函数。
- [ ] **Step 5: 文档、格式与提交。** desktop 条目链接本修复命令；`cargo fmt --all && just ci` 后 `git commit -m "fix: keep sibling watchers alive when unwatching trace"`。

### Task 4: 建立串行订阅与身份门禁（A06/A07/A08）

**Files:** 新建 `desktop/src/lib/subagentTraceSubscription.ts`、`.test.ts`；扩展共享 fixture 的 scripted client。

**Interfaces:** 实现 §0 TraceSubscription。每个 client 对象一条 promise chain，owner 同步写；subscribe/getSnapshot 方法身份稳定，未变化时返回同一 snapshot 引用。

- [ ] **Step 1: 写 deferred 失败用例。** fixture 新建 `ScriptedTraceClient implements TraceClient`：公开 `calls: {method:string; params:unknown}[]` 与 `responses: Map<string, Promise<unknown>[]>`，`queue(method, response)` 入队；`request<T>(method: string, params: unknown): Promise<T>` 记录 calls、shift 对应 promise，无预设响应 reject `new Error("unexpected RPC: " + method)`，有则 `return response as Promise<T>`。

```ts
it("does not let stale cleanup unwatch a reopened task", async () => {
  const first = deferred<AgentTraceSnapshotResult>();
  const stop = deferred<{ stopped: boolean }>();
  const second = deferred<AgentTraceSnapshotResult>();
  client.queue("agent/trace/watch", first.promise);
  client.queue("agent/trace/unwatch", stop.promise);
  client.queue("agent/trace/watch", second.promise);
  sub.open(target, connection);
  await Promise.resolve();
  sub.close();
  sub.open(target, connection);
  first.resolve({ rows: [], highWaterId: 0 });
  await vi.waitFor(() => expect(client.calls.map(c => c.method)).toEqual([
    "agent/trace/watch", "agent/trace/unwatch",
  ]));
  stop.resolve({ stopped: true });
  await vi.waitFor(() => expect(client.calls).toHaveLength(3));
  second.resolve({ rows: [traceRow(5, "assistant_text", { text: "current" })], highWaterId: 5 });
  await vi.waitFor(() => expect(sub.getSnapshot().phase).toBe("ready"));
  expect(sub.getSnapshot().rows.map(r => r.eventId)).toEqual([5]);
});
```

定义 `target = {slot:"local",serverBindingGeneration:1,threadId:"thread-a",taskId:"task-a"}`，`connection = {slot:target.slot,serverBindingGeneration:target.serverBindingGeneration,connectionGeneration:1,client}`；不把 thread/task 放进 TraceConnection。
- [ ] **Step 2: 确认 RED。** `cd desktop && npx vitest run src/lib/subagentTraceSubscription.test.ts`。
- [ ] **Step 3: 实现订阅状态机。** open 同步更新 requestGeneration/owner 和缓冲并启动意图计时；旧排队 watch 执行前核对意图。发出 watch 前即开始缓冲；ack 仅当前 owner 接纳 model 合并结果。过期已发请求必须 settle，补偿 unwatch 必须在本条 chain 内 await 后才能让后续 watch 运行；同 thread 紧随新 watch 可直接替换，但先采用总是补偿的简单路径也合法。

```ts
// 每个 client 队列：错误只终止本操作，不 poison 下一操作。
const previous = queues.get(client) ?? Promise.resolve();
const operation = previous.then(runIntent, runIntent);
queues.set(client, operation.catch(() => undefined));
```

`runIntent` 只有在捕获 owner 仍当前时发布 snapshot/error；旧 cleanup 错误存清理诊断，不污染新视图。close 同步发布 idle、清缓冲/timer，清理排入旧 client 队列；无 client 只清本地。15秒 timer 只发布超时提示，不使唯一当前意图过期、不清它的buffer，也不返回/放行 await。无新意图时迟到成功正常ready并清提示；有新意图才使旧请求失效并先清理。下一意图可排队但不能超车。disconnect 校验源身份，保历史/选择、标断线；retry 用握手完成后的新连接生成新 owner；不自动重订阅。dispose 关闭并清 listener/timer，不强制 dispose 主 client。
- [ ] **Step 4: 确认 GREEN 与竞态矩阵。** A→B、A→B→close、close→A、同任务重试、旧client同ID、不同thread同task、buffer前/后水位、重复、乱序、read零调用。用 fake timers 验证15秒排队超时、无新意图迟到成功恢复ready、旧promise30秒后settle也不超车、失败前buffer不复活；模拟live缺一行不报错，手动retry快照补回。检验 disconnect 保历史但旧事件全拒。
- [ ] **Step 5: 文档、门禁、提交。** 记录定向命令，执行全局提交门禁，`git commit -m "fix: serialize subagent trace subscriptions by owner"`。

### Task 5: 草稿版本与两阶段动作记录（A09/A10）

**Files:** 新建 `desktop/src/lib/subagentTraceActions.ts`、`.test.ts`；复用 model/订阅类型与 fixture。

**Interfaces:** 实现 §0 TraceActions；key 复用Task4导出的 `traceTargetKey(target)`，不复制键算法。record 内保留生命周期序号及每操作唯一 operationId，不能只存三个 bool。连接取自调用参数并验证其 slot/binding 与 target 一致。

- [ ] **Step 1: 写 RED 测试。**

```ts
it("settles the submitted revision without clearing a newer draft", async () => {
  const pending = deferred<{ queued: boolean }>();
  client.queue("agent/message", pending.promise);
  actions.setDraft(target, "first");
  const sent = actions.send(target, connection);
  actions.setDraft(target, "second");
  pending.resolve({ queued: true });
  await sent;
  expect(actions.read(target).draft).toBe("second");
  expect(actions.read(target).notice).toBe("消息已入队");
});
it("never treats a missing connection as a successful message", async () => {
  actions.setDraft(target, "keep me");
  await expect(actions.send(target, null)).rejects.toBeInstanceOf(ConnectionError);
  expect(actions.read(target).draft).toBe("keep me");
});
it("discards a preview that arrives after the task finishes", async () => {
  const pending = deferred<AgentCancelPreviewResult>();
  client.queue("agent/cancel/preview", pending.promise);
  actions.setVisible(target);
  const preview = actions.preview(target, connection);
  actions.updateTask(target, { taskId: target.taskId, state: "completed", finished: true,
    objective: null, lastStep: null, parentTaskId: null });
  pending.resolve({ confirmationToken: "old", taskIds: [target.taskId], expiresInSecs: 60 });
  await preview;
  expect(actions.read(target).confirmation).toBeNull();
});
```

直接 `import type { AgentCancelPreviewResult } from "./protocol"`；该类型已存在，不修改 generated 文件。
- [ ] **Step 2: 确认 RED。** `cd desktop && npx vitest run src/lib/subagentTraceActions.test.ts`。
- [ ] **Step 3: 实现记录与操作。** setDraft 内容真变时 revision+1；请求前同步设置 operationId 防双击，捕获 client/target/lifetime/revision/text；finally 仅相同 operationId 清锁。成功清稿还需 revision 未变且未超时；失败 formatError 归原key。空白不发。消息无 thread/task/client/匹配绑定均 reject ConnectionError，不允许可选链假成功。

preview 捕获 performance.now，响应后复查可见身份/预览代际/终态，使用 cancelDeadline 并设到期timer；实际请求参数 `{threadId,taskId}`，确认额外 `{confirmationToken}`，不添加 recursive 参数。taskIds 去重；0禁确认、多个只防御呈现。confirm 前再验期限、token、client与target；调用 `agent/cancel` 一次，成功 notice「取消请求已提交」，失败清token并必须重preview。pending confirm 的 dismiss 无效，不暗示可撤销。

```ts
const submission = { lifetime: record.lifetime, operationId: ++nextOperationId,
  revision: record.draftRevision, text: record.draft, timedOut: false };
// 只在同 record 生命周期、同 operationId 且未超时、revision 未改时清稿。
```

15秒timer只发布结果不确定并标 timedOut，不清真实pending；迟到preview token丢弃。setVisible离开使预览代际失效，保留草稿和在途operation。dropThread/dropSlot/clear先让生命周期失效再删除；迟到回包只读现存记录，禁止重新创建。普通 disconnect结算该client已销毁请求的锁但保草稿；换绑走drop，不把旧failure写新记录。
- [ ] **Step 4: 确认 GREEN。** 加 A→B→A在途守卫、提交后改回同文案revision仍不清、删除后迟到/重配同ID、任务/slot同ID不串稿、超时后成功留稿、预览TTL请求起点与NaN/0/延迟过期、终态/Tab作废、确认在途不得放弃、取消失败强制重preview；双Enter/双击RPC恰1次。每例清timer并恢复real timers；TTL用 `vi.useFakeTimers({toFake:["setTimeout","clearTimeout","performance"]})` 同步控制performance.now与计时器，不能只推进Date。
- [ ] **Step 5: 文档、门禁、提交。** 记录定向命令，执行全局提交门禁，`git commit -m "fix: preserve subagent drafts and scope task actions"`。

### Task 6: 组合专用 React controller（A06–A10）

**Files:** 新建 `desktop/src/hooks/useSubagentTrace.ts`、`.test.tsx`。

**Interfaces:** 导出 `SubagentTraceController`、`useSubagentTrace(): {controller: SubagentTraceController; view: SubagentTraceView}`。view 为 `{subscription:TraceSubscriptionSnapshot; actions:TraceActionSnapshot|null; selected:TraceTarget|null}`。导出工厂 `createSubagentTraceController(): SubagentTraceController`，hook创建并持有其实例。controller 组合单个 TraceSubscription/TraceActions，稳定暴露：

```ts
interface SubagentTraceController {
  subscribe(listener: () => void): () => void;
  getSnapshot(): SubagentTraceView;
  setConnection(connection: TraceConnection): void;
  connectionFor(client: TraceClient, slot: string): TraceConnection | null;
  open(target: TraceTarget): void;
  retry(): void;
  close(): void;
  tabChanged(visible: boolean): void;
  accept(connection: TraceConnection, params: TraceEventParams): void;
  disconnected(connection: TraceConnection): void;
  updateTask(target: TraceTarget, row: SubagentRow | null): void;
  setDraft(target: TraceTarget, text: string): void;
  send(target: TraceTarget): Promise<void>;
  preview(target: TraceTarget): Promise<void>;
  confirm(target: TraceTarget): Promise<void>;
  dismiss(target: TraceTarget): void;
  dropThread(slot: string, bindingGeneration: number, threadId: string): void;
  dropSlot(slot: string): void;
  resetBindings(): void;
  dispose(): void;
}
```

- [ ] **Step 1: 写 hook RED。** `renderHook(() => useSubagentTrace())`；open、setDraft、close后open同target仍有草稿；tabChanged(false)保owner/watch但使confirmation失效；旧connection通知不更新view。用 `<StrictMode>` wrapper 验证无双watch、卸载后旧ack不发布。

```tsx
it("retains draft while closing the panel but not its selection", () => {
  const { result } = renderHook(() => useSubagentTrace());
  act(() => {
    result.current.controller.setConnection(connection);
    result.current.controller.open(target);
    result.current.controller.setDraft(target, "resume later");
    result.current.controller.close();
  });
  expect(result.current.view.selected).toBeNull();
  act(() => result.current.controller.open(target));
  expect(result.current.view.actions?.draft).toBe("resume later");
});
```

- [ ] **Step 2: 确认 RED。** `cd desktop && npx vitest run src/hooks/useSubagentTrace.test.tsx`。
- [ ] **Step 3: 实现组合，不复制算法。** controller 按slot保存当前connection，并按client对象登记源记录供connectionFor查询；未知client/slot返回null，不能回退当前连接。connectionGeneration由App持有的递增ref分配，普通重连不改变serverBindingGeneration。只给匹配target的当前连接发动作。hook用稳定实例、`useSyncExternalStore`和缓存view，变更由两子模块统一emit；无变化不能无限新建snapshot。订阅记录的requestGeneration变化使可见预览失效，但不清草稿。task更新透传原target，不能读取当前任务替代来源。

```tsx
const view = useSyncExternalStore(controller.subscribe, controller.getSnapshot, controller.getSnapshot);
```

effect清理dispose只取消局部listener/timer/selection并排旧client清理，不dispose主client；StrictMode setup-cleanup-setup必须可重新挂接或重建实例，不能复用永久destroyed实例。无选择或传入target与当前可见target不一致的send/preview/confirm reject ConnectionError；UI调用处catch而非沉默返回undefined。setDraft始终写显式传入的有效绑定target，不读取可变current选择；旧渲染的回调不得把A草稿写入B。
- [ ] **Step 4: 确认 GREEN。** hooks测试与Task4/5测试同时跑，确认组合不改变独立状态机语义；clear/drop保存在途记录的失效代际规则。
- [ ] **Step 5: 文档、门禁、提交。** `git commit -m "refactor: isolate subagent trace controller lifecycle"`，先执行全局提交门禁。

### Task 7: 重做列表与语义事件展示（A01/A02/A03/A12）

**Files:** 修改 `SubagentRail.tsx`、`.test.tsx`、`index.css`、`themeTokens.test.ts`；新建 `TraceEvent.tsx`、`.test.tsx`（均在 desktop/src 对应目录）。

**Interfaces:** Rail 保留现有 row/onOpen API，新增可选 `headingRef` 供返回焦点；`TraceEvent({block}: {block:TraceBlock})`。memo比较所有primitive字段，不能只比较对象引用，也不能比较key后忽略流式text。

- [ ] **Step 1: 写 RED。** 列表metadata null显示ID和中文状态、没有假步骤、active count只计五态；事件默认Markdown、结果标题含自身name、错误首行可见、raw通过原文按钮可达。

```tsx
it("keeps a finalised result expanded when another block arrives", () => {
  const block = decodeTraceRow(traceRow(3, "tool_result", {
    name: "read", summary: "first\nsecond\nthird\nfourth", is_error: false,
  }));
  const { rerender } = render(<TraceEvent block={block} />);
  fireEvent.click(screen.getByRole("button", { name: /展开结果摘要/ }));
  const before = screen.getByText(/fourth/);
  rerender(<TraceEvent block={{ ...block }} />);
  expect(screen.getByText(/fourth/)).toBe(before);
  expect(screen.getByRole("button", { name: /收起结果摘要/ }).getAttribute("aria-expanded")).toBe("true");
});
```

- [ ] **Step 2: 确认 RED。** `cd desktop && npx vitest run src/components/SubagentRail.test.tsx src/components/TraceEvent.test.tsx src/lib/themeTokens.test.ts`。
- [ ] **Step 3: 实现视觉骨架。** 列表ul/li+整行button，细分隔、目标最多2行、status文字+装饰点aria-hidden；无目标用ID，暂停/终态按spec隐藏旧步骤。事件assistant复用MarkdownText；tool call/name与summary分层；tool result只叫结果摘要，长结果折叠、展开后有服务端摘要提示；raw用pre/textContent不作为Markdown/HTML。用React useId绑定aria-controls；更新block不重置local expanded。

```css
.subagent-trace { min-width: 0; overflow-x: hidden; }
.subagent-trace .trace-prose { min-width: 0; overflow-wrap: anywhere; }
.subagent-trace .trace-prose pre { max-width: 100%; overflow-x: auto; }
.subagent-trace .trace-prose table { max-width: 100%; }
```

在现有主题变量区新增 `--trace-success/--trace-warning/--trace-danger`，深浅各给实际颜色并由浏览器Task11验证4.5:1；不在通用`.prose`上改规则、不使用固定Tailwind色阶替代语义token。
- [ ] **Step 4: 确认 GREEN。** 补unknown/state_note、known-label原name保留、空成功/错误摘要、未知原文入口、不可执行HTML、安全外链继承MarkdownText；键盘可达与aria，不给整时间线live。运行MarkdownText既有测试防共享回归。
- [ ] **Step 5: 文档、门禁、提交。** `git commit -m "feat: present subagent tasks and semantic trace events"`，先执行全局提交门禁。

### Task 8: 时间线渐进展示与阅读锚点（A03/A04/A11/A12）

**Files:** 新建 `desktop/src/components/TraceTimeline.tsx`、`.test.tsx`、`desktop/src/hooks/useTraceScroll.ts`、`.test.tsx`。

**Interfaces:** `TraceTimelineProps = {identityKey:string; phase:TraceSubscriptionSnapshot["phase"]; blocks:readonly TraceBlock[]; taskInfo:{taskId:string;objective:string|null;lastStep:string|null}; after:ReactNode}`。用ref导出 `TraceTimelineHandle = {showTaskInfo():void}`。`after` 是子任务区/尾部状态，位于同一滚动区。`useTraceScroll(identityKey:string, ready:boolean)`返回 `{ref,contentRef,followIfAtBottom,scrollToBottom,isFollowing, preserve(change:()=>void):void, showTaskInfo(reveal:()=>void):void, restoreTaskInfo(hide:()=>void):void}`，其中ref沿用共享sticky hook；contentRef观察内容高度。

- [ ] **Step 1: 写 RED。**

```tsx
it("initially mounts 200 blocks and never shrinks the visible suffix on append", () => {
  const blocks = Array.from({ length: 1000 }, (_, i) => decodeTraceRow(
    traceRow(i + 1, "state_note", { note: `event ${i + 1}` }),
  ));
  const props = { identityKey: "a", phase: "ready" as const, blocks,
    taskInfo: { taskId: "task-a", objective: null, lastStep: null }, after: null };
  const { rerender } = render(<TraceTimeline {...props} />);
  expect(screen.getAllByRole("listitem")).toHaveLength(200);
  const appended = Array.from({ length: 100 }, (_, i) => decodeTraceRow(
    traceRow(1001 + i, "state_note", { note: `new ${i}` }),
  ));
  rerender(<TraceTimeline {...props} blocks={[...blocks, ...appended]} />);
  expect(screen.getAllByRole("listitem")).toHaveLength(300);
  fireEvent.click(screen.getByRole("button", { name: "载入更早 200 段" }));
  expect(screen.getAllByRole("listitem")).toHaveLength(500);
});
```

scroll测试用`Object.defineProperty`给真实scroll div定义可控scrollHeight/clientHeight/scrollTop；render初始loading后rerender ready证明首次布局后贴底；同block.text增长证明不只依赖length。
- [ ] **Step 2: 确认 RED。** `cd desktop && npx vitest run src/components/TraceTimeline.test.tsx src/hooks/useTraceScroll.test.tsx`。
- [ ] **Step 3: 实现窗口和锚点。** 全量fold后按稳定首key保存visibleStart，而不是每次`slice(-200)`；新增块不前移start，载早向前最多200。组件key使用identityKey，切任务/Tab重挂重置。首个ready useLayoutEffect一次scrollToBottom，后续按block内容变化followIfAtBottom。

锚点为 `{eventKey, offsetPx}`，取首个与scroll viewport相交的 `[data-trace-key]`，`offsetPx = itemRect.top - viewportRect.top`；变高/载早后恢复为 `scrollTop += newOffset - oldOffset`，找不到key时选最近保留key并clamp至合法scroll范围。preserve先capture、releaseFollow、变更后layout恢复，并在程序scroll后再次releaseFollow，防误回跟随。观察content wrapper和viewport的ResizeObserver；图片load、字号/宽度变化触发同一路径，清理observer/rAF/listener。不要改useStickToBottom文件。

showTaskInfo捕获阅读锚点、releaseFollow、展开后scrollIntoView+focus标题；收起恢复先前锚点，这是显式导航例外。其它展开保持当前锚点。载早入口消失且持焦时聚焦新批次首段 `tabIndex=-1`；不移动用户正在编辑的textarea焦点。按钮悬浮而非正文流，尾部留安全空隙。
- [ ] **Step 4: 确认 GREEN。** 持续追加+暂停、4px回底、宽度/字号ResizeObserver、图片load、展开/收起、载早锚点、完整任务信息往返、卸载清理。`npx vitest run src/lib/useStickToBottom.test.ts`确认共享行为未改；真实几何留Task11。
- [ ] **Step 5: 文档、门禁、提交。** `git commit -m "feat: preserve reading position in subagent timeline"`，先执行全局提交门禁。

### Task 9: 常驻消息区与紧凑详情组合（A05/A09/A10/A12）

**Files:** 新建 `desktop/src/components/TraceComposer.tsx`、`.test.tsx`、`desktop/src/components/SubagentDetailView.tsx`、`.test.tsx`。本任务不改旧 SubagentTrace/ThreadDetailPanel/App，避免在接线前破坏现有 props；Task 10 再把新详情迁移为正式 SubagentTrace。

**Interfaces:** `TraceComposerProps = {value:TraceActionSnapshot; finished:boolean; onDraftChange(text:string):void; onSend():Promise<void>; onPreview():Promise<void>; onConfirm():Promise<void>; onDismiss():void; onFallbackFocus():void}`。临时 `SubagentDetailViewProps`（Task 10 改名为 `SubagentTraceProps`）为 `{identityKey,taskId,row:SubagentRow|null,parent:SubagentRow|null,children:SubagentRow[],subscription:TraceSubscriptionSnapshot,actions:TraceActionSnapshot,onDrill(taskId:string):void,onBack():void,onRetry():void,onDraftChange(text:string):void,onSend():Promise<void>,onPreview():Promise<void>,onConfirm():Promise<void>,onDismiss():void}`；identityKey/taskId均string。用 `ComponentProps<typeof SubagentTrace>` 给外壳，不复制props定义。

- [ ] **Step 1: 在 SubagentDetailView.test.tsx 写新详情 RED。** 旧摘要门测试暂留到 Task 10 一并替换，防中间任务编译断点。

```tsx
it("shows the timeline immediately instead of a whole-trace expansion gate", () => {
  render(<SubagentDetailView {...traceProps} />);
  expect(screen.getByText("first line")).toBeTruthy();
  expect(screen.queryByRole("button", { name: "展开轨迹" })).toBeNull();
  expect(screen.getByLabelText("向这个 agent 发送消息")).toBeTruthy();
});
```

`traceProps`在测试内按上述完整契约创建：subscription ready包含textRow、actions全false/空串/null、row running、parent null、children []，所有Promise callback用`vi.fn().mockResolvedValue(undefined)`，其它callback `vi.fn()`。另测failed send不清受控value；compositionStart/Enter/keyCode229不发，compositionEnd后普通Enter发、ShiftEnter不发。
- [ ] **Step 2: 确认 RED。** `cd desktop && npx vitest run src/components/SubagentDetailView.test.tsx src/components/TraceComposer.test.tsx`。
- [ ] **Step 3: 实现详情布局与受控输入。** 外层flex-col/min-h-0，header和composer shrink-0，Timeline flex-1/min-h-0；header标题3行/step2行/短ID+完整信息入口，父链接只在列表找到非自身时显示。childrenOf过滤自身，子任务默认展开、同一滚动区；尾部文案只看共享状态表。loading/empty/error/disconnected分别展示，ready保留刷新入口。

Composer label独立；两行起、高度上限四行、内容内滚。复用useImeGuard/isImeEnter。事件handler调用Promise必须catch，错误已由原key记录展示，不能加主会话error。发送中仍能编辑；禁发空白/重复发送。确认区替代动作行，textarea不卸载；预览显示真实scope，confirmPending禁放弃；deadline到期UI禁确认。单个polite status，错误alert。仅焦点在消失/禁用确认控件时回停止或标题，正常terminal通知不得抢textarea焦点。

新详情的导航进入聚焦标题tabIndex=-1；父/子统一onDrill，不恢复虚构历史栈。ThreadDetailPanel 的返回列表焦点与 Tab 接线留在 Task 10，本任务独立渲染新组件测试。
- [ ] **Step 4: 确认 GREEN。** 完整跑上述两个文件+Rail/Timeline；加缺父/自引用、关闭确认/终态/过期焦点、pending不撤回、Tab前后草稿、未给row不声称运行、键盘button空间/Enter。新测试覆盖原消息/取消/连续文本能力，并记录 Task 10 需替换的旧“最近步骤/展开整个轨迹/read回灌”断言，不提前删除旧测试。
- [ ] **Step 5: 文档、门禁、提交。** `git commit -m "feat: compose subagent detail with persistent actions"`，先执行全局提交门禁。

### Task 10: App、router 与所有退出路径接线（A05–A10）

**Files:** 将 Task 9 的 `SubagentDetailView.tsx` 实现迁入 `desktop/src/components/SubagentTrace.tsx` 并改导出名，测试迁入 `SubagentTrace.test.tsx`，移除两个临时文件；修改 `ThreadDetailPanel.tsx`、`.test.tsx`、`desktop/src/App.tsx`、`.test.tsx`、`desktop/src/hooks/useSessionState.ts`、`.test.tsx`、`desktop/src/hooks/useNotificationRouter.ts`、`.test.ts`、`desktop/src/hooks/connectionLifecycle.ts`；新建 `desktop/src/hooks/connectionLifecycle.test.ts` 和 `desktop/src/hooks/useNotificationRouter.test.ts`（当前基线没有这两份测试）；引用Task6 controller。

**Interfaces:** NotificationRouterDeps新增 `trace: SubagentTraceController`；ConnectionLifecycleDeps新增 `onTraceConnection(connection: TraceConnection):void`、`traceBindingGeneration(slot:Slot):number`、`nextTraceConnectionGeneration():number`。connectSlot每次创建新client分配递增connectionGeneration，通过 onTraceConnection 登记捕获的 `{slot,serverBindingGeneration,connectionGeneration,client}`，登记必须先于wireClient，router的wireClient闭包使用这一来源而非读取当前clientRef。App持有serverBindingGeneration Map与单调递增connectionGeneration ref；前者仅pairingVersion/slot绑定目标改变递增（普通重连不变）。wireClient被实际调用时立即用Task6的 `trace.connectionFor(client,slot)`捕获来源记录，后续回调使用这份常量；即使router工厂先构造，也不能在每帧读取当前client。未知源拒绝。

- [ ] **Step 1: 写集成 RED。** 把 `App.test.tsx` 现有“点子agent卡片…” fake从read改watch返回完整snapshot，并断言默认见轨迹、`agent/trace/read`零次；两个关闭入口/切thread/new/delete/重配分别让旧deferred回包不污染。加返回列表后children/process仍更新的前端回归，不用重新订阅掩盖后端问题。

```ts
expect(calls.filter(c => c.method === "agent/trace/read")).toHaveLength(0);
expect(calls.filter(c => c.method === "agent/trace/watch")).toHaveLength(1);
// 关闭后解决旧快照，标题/消息错误不能重新出现。
lateWatch.resolve({ rows: [traceRow(9, "assistant_text", { text: "stale" })], highWaterId: 9 });
await waitFor(() => expect(screen.queryByText("stale")).toBeNull());
```

使用现有App fake transport收集RPC和投递notification，不另造第二个App测试框架。
- [ ] **Step 2: 确认 RED。** `cd desktop && npx vitest run src/App.test.tsx -t '子agent|子 agent|trace'`，同时运行router/lifecycle目标文件，确认新测试确实收集（不能0 tests当通过）。
- [ ] **Step 3: 移动责任并同步失效。** 先完成新详情迁移与旧测试替换；ThreadDetailPanel保持Tab顺序/宽度/isMobile约定，避免和详情重复渲染旧返回导航。返回列表聚焦来源任务按钮或列表标题。useSessionState移除detailStack/traceRows/openTaskId/旧openDetail与closeDetail的RPC，不移railRows/refreshSubagents/面板宽度。App通过controller打开目标，传受控view到Trace。router对trace/event调用trace.accept(source,params)，不能setTraceRows；children更新后用来源target同步终态给actions；断开先trace.disconnected(source)再既有dispose/reconnect。

所有路径在任何await、setCurrentId或setPanelOpen前先close/invalidate：详情返回、面板X、状态栏toggle关闭、selectThread、newThread、delete当前thread、slot变更、重配、App卸载。删除任意thread还dropThread；移除slot先dropSlot；重配resetBindings。Tab切diff/process只tabChanged(false)（保watch/选择，清预览），回Tab tabChanged(true)，组件重挂重置最新位置。普通重连保选中/历史/草稿并提示手动retry，不在新client上unwatch旧thread。

```ts
const closeSubagentPanel = () => {
  trace.controller.close();  // 同步更新owner，再隐藏UI
  sess.setPanelOpen(false);
};
```

写动作不再由App可选链代理RPC；统一controller.send(target)/preview(target)/confirm(target)，各回调闭包捕获当时渲染的selected target；onDraftChange同样传捕获target。missing或不再可见的动作target必须reject ConnectionError，不能在回调时偷读新选择。删除 trace 消息、预览与确认中所有 `clientRef.current?.request` 路径；children/list 等无关读取不扩大改动。不要动主会话draft/搜索/旁问路由。
- [ ] **Step 4: 确认 GREEN。** 运行App/router/lifecycle/useSessionState/useSubagentTrace定向全量，再`npx tsc --noEmit && npm test && npm run build`。grep确认旧`setTraceRows/openTaskId/trace/read`不在生产trace入口；read保留协议类型不删除。增加旧client相同thread/task通知、普通重连草稿保留、换绑同ID清理测试。
- [ ] **Step 5: 文档、门禁、提交。** 更新desktop未完成条目的实际测试证据；执行全局提交门禁，`git commit -m "feat: wire subagent trace ownership into desktop navigation"`。

### Task 11: 真实浏览器矩阵与最终验收记录（A01–A12）

**Files:** 新建 `desktop/scripts/browser/subagent-trace.html`、`subagent-trace.tsx`、`subagent-trace-check.mjs`；修改 `desktop/package.json`、`desktop/scripts/browser/README.md`；新建 `docs/superpowers/reviews/2026-11-07-desktop-subagent-trace-acceptance.md`；同步 `docs/project-management/{desktop.md,README.md}`。

**Interfaces:** fixture `window.traceProbe` 暴露 `setTheme("dark"|"light")`、`setScale("small"|"standard"|"large")`、`setPanelWidth(260|360|720)`、`appendText(text:string)`、`appendEvents(count:number)`、`loadScenario(name:"long"|"emptyMetadata"|"error"|"children"|"images")`、`metrics(): {panelOverflow:number;bodyHeight:number;bottomGap:number;anchorKey:string|null;anchorOffset:number;messageReachable:boolean}`。渲染真实SubagentTrace/ThreadDetailPanel与index.css，用mock controller/action数据，不启动sidecar。fixture不注册生产事件listener。

- [ ] **Step 1: 写 browser RED断言并先验证能失败。** 沿用现有stream-live-check.mjs的Vite+Chrome+原生WebSocket方式，独立profile，端口环境变量 `SUBAGENT_TRACE_PORT` 默认1449、`SUBAGENT_TRACE_DEBUG_PORT` 默认9359，支持CHROME_BIN。

```js
assert.ok(metrics.panelOverflow <= 1, "panel must not overflow horizontally");
assert.ok(metrics.bodyHeight >= 120, "default timeline needs 120px at 900x600");
assert.equal(metrics.messageReachable, true);
assert.ok(Math.abs(after.anchorOffset - before.anchorOffset) <= 2, "paused anchor moved");
```

未有fixture或暂去掉本栏min-width/锚点恢复时应非零退出，保存失败输出；这是浏览器几何红测，不能用mockscroll伪造结果。脚本等待runtime异常也必须fail。
- [ ] **Step 2: 完成fixture与脚本。** 主题2×栏宽3×字号3共18种，每种900×600并检查默认min正文/溢出/动作。long scenario含长中文、ID、无空格路径、Markdown表格/代码/错误、多个子任务，另加真实空元数据scenario；字体用项目现有设置入口，不能只改外层fontSize掩盖prose问题。颜色从getComputedStyle取实际前景/背景合成，按WCAG相对亮度算contrast>=4.5，不能只检token存在。

滚动独立序列：ready贴底→append同块text贴底→真实wheel/PageUp暂停→连续append不移动anchor→摘要展开/载早保持→图片异步变高保持→resize/字体保持→点回最新恢复；完整信息显式导航往返单测和浏览器均验证。1000块初始200、追加100为300，载早最终可访问1100，不宣称固定DOM上界。用CDP键盘测试Tab/Enter/Space、取消focus、textarea焦点不被terminal抢、reduced-motion模拟。

所有spawn均push到children，finally关闭ws、先TERM后超时KILL子进程并删临时profile；每个等待有界，stderr/Runtime.exceptionThrown保留。脚本把矩阵metrics与失败case输出JSON到调用者指定的 `SUBAGENT_TRACE_REPORT` 文件；默认放desktop/.artifacts下并确认ignored，不提交截图/巨型日志。
- [ ] **Step 3: 跑真实验证。** package.json新增 `"test:browser:subagent-trace": "node scripts/browser/subagent-trace-check.mjs"`。运行 `cd desktop && npm run test:browser:subagent-trace`；保存18组合结果、锚点误差、对比度和使用的Chrome/Node版本。缺Chrome必须明确失败/未验，不回退jsdom伪通过。
- [ ] **Step 4: 全量门禁与人工边界。**

```bash
cd desktop && npx tsc --noEmit && npm test && npm run build
npm run test:browser:subagent-trace
cd ../yi-agent-rs && cargo test -p yi-agent-app-server trace_unwatch_
cargo test -p yi-agent-app-server thread_delete_stops_all_watchers
cargo fmt --all && just ci
```

真实WKWebView按spec §7另记深浅主题/最小窗/大字号/IME/实时追加/关闭再打开；没执行就记录「未验」，Chrome不能替代。若本机原生未验，项目条目仍`[ ]`并明确仅剩哪项，不标全部完成。不要为验收打独立App；用户没有要求打包。
- [ ] **Step 5: 更新验收文档与提交。** 记录每条A01–A12对应测试名、实际命令、日期和结果，不能预填passed。全部要求证实后才把desktop条目`[ ]`改`[x]`、索引121/137改122/137（实施时先读取最新计数，不能覆盖其它并行需求）；未全验则保持原计数。执行全局提交门禁后 `git commit -m "test: verify desktop subagent trace interactions"`。交付前用requesting-code-review与verification-before-completion；合并按finishing-a-development-branch流程且须用户选择，不自动合主线。合并获准后`git merge --no-ff`，合并结果再跑just ci，清理本分支worktree遵循项目规则，不删除仍在使用的target。

## 覆盖映射与计划自检

| 验收 | 实现任务 | 主要证据 |
|---|---|---|
| A01 | 1、7、11 | 15状态表、真实空metadata、活动计数、列表单测+矩阵 |
| A02 | 2、7、9 | malformed/raw矩阵、连续有效文本、Markdown、默认时间线 |
| A03 | 2、7、8、11 | 工具不配对/摘要阈值、memo稳定、1000→200/追加→300/载早可达 |
| A04 | 8、11 | mock几何确定性测试+Chrome真实滚动/ResizeObserver/图片 |
| A05 | 6、9、10 | 父子/列表导航、缺父/自引用、条件焦点恢复 |
| A06 | 2、4、6、10 | watch权威快照、buffer/高水位/缺帧刷新、loading/error/15秒栅栏 |
| A07 | 4、6、10 | deferred竞态、完整owner、同ID旧client/slot |
| A08 | 3、4、6、10 | 窄清理/删除全清、所有退出与Tab保watch |
| A09 | 5、6、9、10 | revision/IME/双击/草稿生命周期/迟到/无连接 |
| A10 | 2、5、9、10 | preview→confirm、单目标非递归、TTL、过期/终态/超时与守卫 |
| A11 | 7、8、9、11 | 18组合、900×600、正文≥120、无横溢出、4.5:1 |
| A12 | 7、8、9、11 | 语义/label/焦点/status、原文入口、键盘与reduced-motion |

执行依赖：1→2；3可独立；2→4→5→6；1/2→7；7→8；6/8→9；3/6/9→10；10→11。4与5即使可技术并行，也按顺序定义共享身份类型后再实现；App接线单任务单作者，避免共享文件冲突。

计划自审须逐项确认：spec每条能映射至上表；所有新函数/props均在本文件定义；旧trace helper调用点全部迁移；没有新增协议或依赖；没有把手工/浏览器检查当作已执行证据。本计划完成不等于代码完成。
