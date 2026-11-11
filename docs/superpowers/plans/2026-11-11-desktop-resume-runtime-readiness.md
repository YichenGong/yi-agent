# Desktop Resume Runtime Readiness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 本地冷会话必须先获得项目 managedReady，再恢复会话；失败后一次显式操作即可重新准备并恢复，不自动发送消息。

**Architecture:** 复用 useRuntimeLease.prepareProject 和原生每 scope 准备入口，在现有 pendingResumes 激活屏障内增加准备阶段。历史回放 watchdog 仅在真正发送恢复请求时启动。独立的激活失败提示组件提供恢复操作，原生记录不含秘密的就绪拒绝诊断，现有授权检查保持 fail-closed。

**Tech Stack:** React 19、TypeScript、Vitest 3、Testing Library、Tauri 2、Rust、Tokio、serde_json；不增加依赖。

## Execution Notes (2026-11-11)

- 用户选择当前会话直接实施。Task 1–3 的阶段提交合并成一个实现提交，避免为每个小阶段重复全量 CI；红/绿测试仍逐阶段执行。提交前与合并后完整门禁要求不变。
- 干净前端基线：349 项通过。Task 1 红测捕获提前 resume 与陈旧 ready；Task 2 红测捕获缺恢复按钮；Task 3 红测捕获缺诊断接口。日志位于 worktree 的 ignored `.artifacts/resume-readiness/`。
- 补充红测捕获并修复：准备期间客户端重建时旧屏障阻塞新恢复；准备期间状态变为 running/awaiting_approval 时仍会 resume。均已用原红测验证转绿。
- 评审 I1 的两个红测覆盖 preparing/replaying 客户端被替换且下一次 `thread/listAll` 失败：替换回调为仍在途的准备留下显式恢复状态；RPC 被 `dispose()` 提前拒绝时，在 `catch` 保留恢复入口。该补充仅适用于本地 `ConnectionError`，普通历史读取失败继续使用原 `failSync` 语义（原断言未修改）。相关 4 项回归已通过。
- 原生完整 lib：294 项通过；最新前端全量：197 文件、2,945 项通过，`npm run build` 通过。测试中的 Tauri window mock 与 Vite chunk/dynamic-import 警告仍存在，非本次新增失败。未运行/替换正式 App，未触发真实 LLM API。

### 评审处理记录

- native/hook/恢复组件评审：无 Critical/Important，允许合并。诊断的成功路径字段分配、stderr 限流和 live-region 交互语义均为非阻断后续建议，本轮不扩大范围。
- superseded `null` 保持 fail-closed：同会话新 activation 使用新 token；当前尝试没有有效 `managedReady` 时必须保留发送屏障和显式重试，不能静默忽略后放行发送。`settleAcquire` 的陈旧结果不盲目释放同 scope 租约，避免影响 native 去重后的新尝试。
- App 初评无 Critical，I1（重连 retire 无可见兜底）已用失败握手红测确认并修复；收窄复核确认两种断开时序、普通历史失败语义及正常重连均有正确路径，无 Critical/Important，允许合并。其余关于固定失败文案、恢复等待完整 activation、普通发送有限等待预算及 phase 注释均为非阻断建议。复核新增的失败提示重复/运行中会话残留红条、未来远端复用时的 subWindow 注意点也保留为展示与演进建议，不在本轮扩大改动。
- 用户已选择本地合并到 `main`，代码以 `--no-ff` 合并为 `b1210923`（实现提交 `4e623e54`）。合并后前端 197 文件 / 2,945 项、构建、native lib 294 项、完整 `NEXTEST_TEST_THREADS=2 just ci` 均通过（exit 0；Rust 6,442 passed / 41 skipped，bindings 50 passed）。模块条目改为 `[x]`，索引完成数 69 → 70。
- 合并后 CI 的环境处理如实记录：首次 `fmt-check` 包含主工作区原有未跟踪 `probe_guardian.rs`，因此失败；未改写该文件，重跑时暂移到 ignored 目录，结束后自动原样还原并验证 SHA256 `1514599bb01c07fd45ca58c7ddd2c944dc0d70eab62cfcdcd0b3cc8eef3cbcde`。随后插件测试缓存二进制嵌入已不存在的 `desktop-startup-content` / `desktop-content-timeout` worktree 路径，造成 fixture/src 缺失；`cargo clean -p yi-agent-plugins --profile dev` 定点重建后插件 247 passed / 7 skipped，再完整重跑所有 CI 门禁通过。未跳过测试、修改断言或增加审计豁免。
- 已归档日志到主仓库 ignored `.artifacts/resume-readiness/`：`main-ci-final.log`、`main-frontend.log`、`main-build.log`、`main-native.log`、`main-plugins-rebuilt.log`；原 worktree 红绿与 CI 日志保存在 `worktree/`。清理前确认无 Cargo/rustc 进程，`just reclaim --target ../.worktrees/resume-runtime-readiness/yi-agent-rs/target` 仅预演；已移除实现 worktree 和已合并的修复分支，其他 worktree、主仓库产物与正式 App 未动。完成记录在独立纯文档 worktree 提交并合并；纯文档合并按项目规则免跑 CI/单测。

### 验收证据索引

| 设计 §6 | 自动验证 / 说明 |
| --- | --- |
| 1 | `keeps cached history and draft visible while local preparation is pending`、`waits for managed readiness before resuming a cold local thread` |
| 2 | `does not resume a non-ready cold local thread` 六种结果；慢准备放行后恰好一次 resume |
| 3 | `does not resume the old selection after preparation completes`、`retires an old preparation when navigating away and back`、`does not resume a thread deleted during local preparation`、`ignores preparation from an exited client after reconnect`；槽位由 `currentPreparation` 的 slot/client 双栅栏约束，当前 App 没有可驱动的 activeSlotRef 写入入口，不声称已做槽位切换 GUI 测试 |
| 4 | 前述导航重复点击、`activation recovery prepares and resumes once without sending the draft` 双击、hook 的 `returns null for a superseded on-demand success` |
| 5 | `does not charge preparation time to the history idle budget`：准备 60s 不超时，实际 replay 停滞 16s 报错 |
| 6 | 原 `desktop history remains synced when only runtime activation fails`；恢复入口测试还验证 credential-shaped 原始错误不上屏 |
| 7 | 两个 `activation recovery` 测试：成功/失败保留草稿和 PDF 附件，不发 turn/start 或 thread/start；切走后仅恢复新选择 |
| 8 | 既有本地 running 只读与远端测试；新增 `does not interrupt a thread that becomes running/awaiting_approval during preparation`，准备返回时状态变忙则重入只读分支 |
| 9 | 两个 `readiness_diagnostic_*` 覆盖三类原因与严格 JSON 字段集合，完整 native lib 294 项包含正常 root 授权与失效租约拒绝 |
| 10 | 前端全量 2,945 项及构建、native lib 294 项、最终 worktree `NEXTEST_TEST_THREADS=2 just ci` 已通过（exit 0）：Rust 6,442 项（41 skipped），bindings 50 项；允许的 rustix/chacha20 审计警告未新增豁免。评审及复核无 Critical/Important，允许合并；实现已合并为 `b1210923`，合并后完整 CI（Rust 6,442 passed / 41 skipped，bindings 50 passed）、前端 2,945 项及构建、native 294 项均通过；环境处理与归档日志见 Execution Notes |

## Global Constraints

- 设计真源：`docs/superpowers/specs/2026-11-11-desktop-resume-runtime-readiness-design.md`，尤其 §6 的十项验收。
- 仅本地桌面壳持有的会话运行环境；远端浏览器与远端槽位不调用本机准备命令。
- 只有 `managedReady` 是准备成功；blocked/unknown/starting/legacyChoiceRequired/null 不能通过。
- 不修改 guardian 授权规则，不降级 embedded，不自动确认版本切换或 legacy 接管。
- 保留历史、草稿、附件；失败恢复不自动发送、不自动创建会话。
- 运行中/等审批会话只读，不通过 thread/resume 中断现有任务。
- 同一会话准备和恢复合并为一次在途激活；客户端、槽位、会话对象、选择版本均是旧请求栅栏。
- 原生诊断不含 capability、generation secret、management secret、token、完整请求或用户正文。
- 在 `.worktrees/resume-runtime-readiness` / `fix/resume-runtime-readiness` 实施；不触碰主工作区现有未跟踪文件。
- 每个实现提交更新对应模块进度；提交前 `cargo fmt --all`、`just ci`；合并后再次 `just ci`。仅文档提交免 CI/单测。
- 不替用户停止正式 App、重置 runtime 或部署安装包。代码修复与安装版验收分开报告。

## File Structure

- Modify `desktop/src/hooks/useRuntimeLease.ts`：被新请求取代的准备结果不可返回成当前成功。
- Test `desktop/src/hooks/useRuntimeLease.test.tsx`：显式准备、后台申请、重试序号与迟到应答。
- Modify `desktop/src/App.tsx`：冷恢复准备阶段、有效请求判断、失败恢复动作和发送屏障。
- Modify `desktop/src/lib/threadStore.ts`：更新 activationError 的契约说明，允许准备失败；不持久化新的激活状态。
- Create `desktop/src/components/RuntimeActivationNotice.tsx` / `.test.tsx`：只负责本地失败恢复入口、忙状态与固定文案。
- Test `desktop/src/App.test.tsx`：真实 App wiring 回归，复用 runtimeNative.acquire、state.dataSources 和内容缓存替身。
- Modify `desktop/src-tauri/src/runtime_lifecycle.rs`：就绪判定共用结果、结构化拒绝诊断、原生单测；不增加 tracing 依赖。
- Modify `docs/project-management/subagent-runtime.md` 和 `README.md`：新增本轮 feature，完成时附可执行验证命令并更新索引计数。

## Task 1: 建立本地冷恢复准备屏障

**Interfaces:**
- Consumes `prepareProject(project: string): Promise<AcquireView | null>`，`usesLocalPaths(slot)`，ThreadView.info.cwd 和 thread summary.cwd。
- Produces pendingResumes 的 `phase: "preparing" | "replaying"`、`allowWaitingSend: boolean`；其余 token/client/historyReady/activation 字段保留。
- `selectThread(id, opts?: { forceResume?: boolean; recovery?: boolean }): Promise<void>`；recovery 表示失败恢复，不能成为绕过 running 只读路径的理由。

- [x] Step 1: 在 worktree 准备工具并执行干净基线。

```sh
export PATH="/opt/homebrew/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cd .worktrees/resume-runtime-readiness/desktop
npm ci
npm test -- src/hooks/useRuntimeLease.test.tsx src/App.test.tsx
```

基线失败先确认是否由环境/既有测试导致；不以变更断言掩盖。Rust 编译暂不并发启动。

- [x] Step 2: 在 App runtime preparation wiring 套件加入可控准备 Promise 的红测。

```tsx
it("waits for managed readiness before resuming a cold local thread", async () => {
  let release!: () => void;
  let first = true;
  runtimeNative.acquire = (project) => {
    const ready = {
      scopeKey: `${project}/.yi-agent/runtime`,
      attemptId: "attempt-cold",
      state: "managedReady", retry: "doNotRetry", held: true,
    };
    if (!first) return Promise.resolve(ready);
    first = false;
    return new Promise((resolve) => { release = () => resolve(ready); });
  };
  render(<App />);
  await waitFor(() => expect(runtimeNative.calls.some(
    (call) => call.cmd === "runtime_acquire_project",
  )).toBe(true));
  expect(clients[0].requests.filter((r) => r.method === "thread/resume")).toHaveLength(0);
  // 每个测试根据真实请求数量释放对应 acquire；不要只覆盖最后一个 resolver。
  await act(async () => { release(); });
  await waitFor(() => expect(clients[0].requests.filter(
    (r) => r.method === "thread/resume",
  )).toHaveLength(1));
});
```

夹具必须使用 resolver 队列或在首次阻塞后后续 acquire 直接 ready，避免后台申请+显式准备令测试悬挂。增加 `it.each(["starting", "blocked", "unknown", "legacyChoiceRequired"])`、throw、null 的不恢复断言；断言原生 reason 不上屏。

- [x] Step 3: 加入 useRuntimeLease 的陈旧成功红测：第一次 prepareProject 在途，第二次显式 prepareProject 已排队，第一次迟到 ready 不得授权调用方恢复。

现有 `requestSeq` 已在排队时递增，修复 acquireOnce 的返回口径而非仅 settleAcquire：

```ts
await settleAcquire(project, seq, view);
return acceptsStartupReply(requestSeq.current.get(project) ?? 0, seq)
  ? view
  : null;
```

测试必须断言旧调用返回 null、新调用返回新结果、issue map 只描述新结果、已持有租约未被错误清除。同项目两次冷会话点击通过 App 的 inFlightResume 合并，不把 prepareProject 改成永久 ready 缓存。

- [x] Step 4: 运行红测，记录真实失败原因。

```sh
cd desktop
npm test -- src/App.test.tsx -t 'waits for managed readiness|non-ready cold'
npm test -- src/hooks/useRuntimeLease.test.tsx -t 'superseded'
```

期望 App 当前代码提前发出 thread/resume；hook 当前代码返回过期 ready。不得把超时视为理想红测。

- [x] Step 5: 在现有冷恢复分支内部实现顺序，保留前面的 warm/running/remote 早退。准备 Promise 启动前就登记 pendingResumes 和 inFlightResume；replaySession.beginReplay 和 watchdog 延迟到准备成功后。

```ts
const pending = pendingResumes.current.get(id);
const currentAttempt = () => clientRef.current === c &&
  store.peek(id)?.session === replaySession &&
  pendingResumes.current.get(id)?.token === token;
const currentPreparation = () => currentAttempt() &&
  activeSlotRef.current === slot &&
  selectionRevision.current === revision && store.currentId === id;
```

捕获 `slot`、`revision` 于本次选择；准备前后使用 currentPreparation，已经发出的恢复响应仍使用 currentAttempt，避免合法后台历史被错误丢弃。被放弃准备的 finally 只清本 token 的记录。

本地路径优先 `store.peek(id)?.info?.cwd`，其次 pinned/groups 的相同 id 摘要。缺路径时记录固定 `activationError`，不猜 cwd、不请求恢复。准备失败显示固定的 startupIssueReason 或 ACQUIRE_FAILED_REASON；不要调用 failSync 将环境问题伪装成历史损坏。

准备阶段不修改原 syncState、不开始 beginReplay；通过 pendingResumes 阻止发送，失败则由 activationError 阻止发送。send 保留 offline/connecting 的最前置拦截；连接正常时先处理 activationError 与 pending 激活，再处理历史 syncing/failed，避免给准备问题显示“内容加载失败”，也避免历史初始 synced 造成越过屏障。只有真正开始恢复才将 syncState 置 syncing。正常在途激活保留有限等待规则；recovery 模式不接受自动等待发送。

watchdog 使用可选句柄，在准备通过且本次仍有效时创建，historyReady 和 finally 均可安全 stop。远端仍走旧逻辑，不因没有本机 cwd 失败。

- [x] Step 6: 增加并运行边界回归：准备超 15s 不触发历史超时；恢复开始后无历史帧仍超时；准备期间重复点击只恢复一次；切走、切走再切回、删除、槽位切换、重连后旧结果无副作用；本地 running/awaiting_approval 仍不 resume；远端不调用 acquire。

```sh
cd desktop
npm test -- src/hooks/useRuntimeLease.test.tsx src/App.test.tsx
npm run build
```

保持原有 historyReady token、历史已同步但激活失败、切换会话不自动发送测试全绿。

- [x] Step 7: 模块文件已登记本轮 `[ ]` 条目、已通过命令和剩余合并验收，索引总数同步。`cargo fmt --all` 与最终 `NEXTEST_TEST_THREADS=2 just ci` 通过；按 Execution Notes 合并提交 `fix: prepare runtime before resuming local conversations`。

## Task 2: 一次显式准备并恢复入口

**Interfaces:**
- Consumes Task 1 的 `selectThread(id, { recovery: true })`、pendingResumes、activationError 和现有项目准备 issues。
- Produces `RuntimeActivationNotice({ local, failed, busy, onRetry })`，四个 props 分别为 boolean、boolean、boolean、`() => void`；组件不发 RPC、不拿 native capability。
- App 内 `retryCurrentActivation(): Promise<void>` 只绑定操作发起时的当前失败会话；用同步 ref 防重复点击，finally 只释放自己的记录。

- [x] Step 1: 创建组件红测，证明仅本地失败显示按钮，busy 禁用，点击触发回调，不渲染原始错误。

```tsx
render(<RuntimeActivationNotice local failed busy={false} onRetry={retry} />);
fireEvent.click(screen.getByRole("button", { name: "准备运行环境并重试" }));
expect(retry).toHaveBeenCalledTimes(1);
```

附远端、非失败、不含 credential-shaped 文案的测试。busy 测试对同一个真实按钮点击，不直接调用回调绕过 disabled。

- [x] Step 2: 加入 App 红测：准备先 ready、thread/resume 在 historyReady 后返回 runtime unavailable；project issues 已清空，仍应出现恢复按钮。输入草稿并保留附件，按恢复按钮后 pending acquire 未完成不 resume；managedReady 后只增加一次恢复，成功不发 turn/start 或 thread/start。

```tsx
expect(await screen.findByRole("button", {
  name: "准备运行环境并重试",
})).toBeTruthy();
expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(false);
```

重试时准备失败、双击、切走、删除与再次重试均使用可控 Promise，不用 sleep 制造竞态。

- [x] Step 3: 执行红测。

```sh
cd desktop
npm test -- src/components/RuntimeActivationNotice.test.tsx
npm test -- src/App.test.tsx -t 'activation recovery'
```

- [x] Step 4: 组件采用固定文案和 button；App 的恢复动作只调用一次 selectThread，并设置 recovery 禁止等待自动发送。准备与恢复均由 Task 1 统一编排，按钮回调不另外先 prepare，防止连续两次准备。

```ts
const id = store.currentId;
const view = id ? store.peek(id) : undefined;
if (!id || !view?.activationError ||
    view.status === "running" || view.status === "awaiting_approval") return;
await selectThread(id, { recovery: true });
```

失败态不能被 warm 早退吞掉；不使用 forceResume 绕过 running 保护。准备成功不清理用户草稿或附件。将两处“请重新选择此会话重试”改为与实际恢复入口一致的固定指导。

现有 RuntimeStartupNotice.onRetry：被首次新建门禁阻止的其他目录保持 prepare-only；当前同项目失败会话走 retryCurrentActivation；没有失败会话的项目重试仍只准备。整个异步过程绑定原客户端、slot、session、selectionRevision，不能在准备返回后读取新选择并恢复它。

- [x] Step 5: 运行组件与 App 全部测试/build；特别保留首次新建不自动创建、远端没有本机重试、运行中会话不打断三类旧断言。

```sh
cd desktop
npm test -- src/components/RuntimeActivationNotice.test.tsx src/App.test.tsx
npm run build
```

- [x] Step 6: 模块阶段证据已同步，fmt/最终 CI 通过；显式恢复与 Task 1、3 合并为一个实现提交。

## Task 3: 原生就绪拒绝诊断，不放宽授权

**Interfaces:**
- Consumes DesktopRuntimeManager.project_state、ControlBackend.lease_is_live、forget_ready。
- Produces私有 `ReadinessDiagnostic`（serde::Serialize），字段为 `scope_key: String`、`attempt_id: Option<String>`、`state: Option<AcquireState>`、`held: bool`、`ready: bool`、`generation: Option<String>`、`lease_live: Option<bool>`、`reason: ReadinessFailure`。
- `ReadinessFailure` 为 `MissingState | ProjectNotReady | LeaseExpired`，serde camelCase。
- 私有 `check_lifecycle_ready(&self, scope: &RuntimeScope) -> Result<(), ReadinessDiagnostic>`；现有 `is_lifecycle_ready -> bool` 包装它，避免两次实时检查。

- [x] Step 1: 在 runtime_lifecycle.rs tests 中新增三类诊断红测，复用 Wired::connected、scope、root_intent、note_sidecar_epoch 等现有夹具。

```rust
assert_eq!(diagnostic.reason, ReadinessFailure::MissingState);
assert_eq!(diagnostic.lease_live, None);
```

未准备/blocked 时不做实时租约检查；已 ready 后将 `wired.control.knobs.lock().unwrap().served = false`，断言 LeaseExpired、lease_live=Some(false)、manager 内 ready 被清除。断言诊断 JSON 的键集合严格等于白名单字段；reason 是固定 enum，不携带原始错误/请求秘密。

- [x] Step 2: 在 worktree 准备 native build 要求并运行红测。先阅读项目已有 target-dir 配置，沿用而不建立新的共享 target。按 AGENTS.md 给本 worktree 补 sidecar 二进制链接，不复制正式运行状态。

```sh
mkdir -p desktop/src-tauri/binaries
ln -sf /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/desktop/src-tauri/binaries/yi-agent-aarch64-apple-darwin desktop/src-tauri/binaries/yi-agent-aarch64-apple-darwin
cargo test --manifest-path desktop/src-tauri/Cargo.toml --lib readiness_diagnostic
```

缺二进制时先构建测试所需产物，不链接不存在的目标后宣称测试通过。

- [x] Step 3: 将原 readiness 逻辑提取为单次求值 Result；lease 失效先捕获诊断所需旧状态再 forget_ready。authorize_intent 在原来门禁位置调用一次，失败时输出 JSON 诊断到 native stderr，继续返回原稳定错误。

```rust
if let Err(diagnostic) = self.check_lifecycle_ready(&scope) {
    if let Ok(line) = serde_json::to_string(&diagnostic) {
        eprintln!("[runtime-readiness] {line}");
    }
    return Err(format!(
        "{} is not lifecycle-ready, so no root may be authorized there",
        request.scope_key
    ));
}
```

不在授权入口自动 ensure_project、不增大授权预算；所有身份检查的顺序不改。日志落点是原生 stderr，不谎称自动写入 sidecar 的 tracing jsonl。

- [x] Step 4: 执行诊断、原生完整测试与格式检查，保持现有正常授权、租约失效拒绝、blocked 有租约仍不授权的测试通过。

```sh
cargo test --manifest-path desktop/src-tauri/Cargo.toml --lib
cargo fmt --manifest-path desktop/src-tauri/Cargo.toml -- --check
cd yi-agent-rs
cargo fmt --all
NEXTEST_TEST_THREADS=2 just ci
```

- [x] Step 5: 模块诊断验收证据已同步，fmt/最终 CI 通过；原生诊断与 Task 1、2 合并为一个实现提交。

## Task 4: 完整验证、评审、合并与收尾

**Files:** 本轮所有变更、`docs/project-management/subagent-runtime.md`、`docs/project-management/README.md`、本计划。

- [x] Step 1: 对照设计 §6 逐项记录测试名与结果；完整前端验证、native 测试和最终 Rust CI 均通过。不触发真实 LLM API。

```sh
cd desktop
npm test
npm run build
cd ..
cargo test --manifest-path desktop/src-tauri/Cargo.toml --lib
cargo fmt --manifest-path desktop/src-tauri/Cargo.toml -- --check
cd yi-agent-rs
cargo fmt --all
NEXTEST_TEST_THREADS=2 just ci
```

CI 默认并发导致超时则独占复跑，记录真实差异，不跳过失败。原生/前端构建不得运行时修改正式 App。

- [x] Step 2: 使用 requesting-code-review / receiving-code-review 完成评审与意见核对，I1 经红绿验证和独立复核关闭；pendingResumes、watchdog、陈旧租约结果与草稿发送边界均已审查。无 Critical/Important，允许合并。
- [x] Step 3: 全部代码验收通过后，模块 feature 更新为 `[x]`、索引 70 / 83；安装版 GUI 未验收/未部署明确保留为边界，不改变其他 feature 状态。验证记录在独立纯文档 worktree 提交，按项目纯文档例外免跑 CI/单测。
- [x] Step 4: 使用 finishing-a-development-branch，确认分支干净和原有探针内容未变；用户选择后以 `--no-ff` 合并代码为 `b1210923`，在 main 重跑完整 CI（见环境处理记录）通过。没有在 main 直接改代码。
- [x] Step 5: 合并验证通过，日志归档并确认无 Cargo/rustc 进程；本轮 `just reclaim` 预演完成，原实现 worktree 和已合并修复分支已移除（回收约 35 GiB 产物，不动其他 worktree）。本轮 diff 已打开；报告代码合并 `b1210923`、测试结果及未部署状态。

## Self-Review / Spec Coverage

- 设计 §1、§5 的诊断边界 → Task 3；不把缺 ready 直接断言为竞态或已修复全部 runtime 问题。
- §2、§3 的本地/远端、running、旧请求、缺 cwd、准备门禁、watchdog → Task 1。
- §4 的单次恢复、双击、草稿/附件、不自动发送、不自动创建 → Task 2。
- §6 的十项验收 → Task 1/2/3 的具体验证及 Task 4 的完整门禁。
- §7 的进度维护、评审、工作树/合并 → 每个任务提交步骤及 Task 4。
- 接口一致：prepareProject 返回 AcquireView|null；selectThread recovery 不等价 forceResume；RuntimeActivationNotice 只接四个展示 props；原生 Result 诊断在 UI 不透传。
