# Desktop Resume Runtime Readiness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 本地冷会话必须先获得项目 managedReady，再恢复会话；失败后一次显式操作即可重新准备并恢复，不自动发送消息。

**Architecture:** 复用 useRuntimeLease.prepareProject 和原生每 scope 准备入口，在现有 pendingResumes 激活屏障内增加准备阶段。历史回放 watchdog 仅在真正发送恢复请求时启动。独立的激活失败提示组件提供恢复操作，原生记录不含秘密的就绪拒绝诊断，现有授权检查保持 fail-closed。

**Tech Stack:** React 19、TypeScript、Vitest 3、Testing Library、Tauri 2、Rust、Tokio、serde_json；不增加依赖。

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

- [ ] Step 1: 在 worktree 准备工具并执行干净基线。

```sh
export PATH="/opt/homebrew/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cd .worktrees/resume-runtime-readiness/desktop
npm ci
npm test -- src/hooks/useRuntimeLease.test.tsx src/App.test.tsx
```

基线失败先确认是否由环境/既有测试导致；不以变更断言掩盖。Rust 编译暂不并发启动。

- [ ] Step 2: 在 App runtime preparation wiring 套件加入可控准备 Promise 的红测。

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

- [ ] Step 3: 加入 useRuntimeLease 的陈旧成功红测：第一次 prepareProject 在途，第二次显式 prepareProject 已排队，第一次迟到 ready 不得授权调用方恢复。

现有 `requestSeq` 已在排队时递增，修复 acquireOnce 的返回口径而非仅 settleAcquire：

```ts
await settleAcquire(project, seq, view);
return acceptsStartupReply(requestSeq.current.get(project) ?? 0, seq)
  ? view
  : null;
```

测试必须断言旧调用返回 null、新调用返回新结果、issue map 只描述新结果、已持有租约未被错误清除。同项目两次冷会话点击通过 App 的 inFlightResume 合并，不把 prepareProject 改成永久 ready 缓存。

- [ ] Step 4: 运行红测，记录真实失败原因。

```sh
cd desktop
npm test -- src/App.test.tsx -t 'waits for managed readiness|non-ready cold'
npm test -- src/hooks/useRuntimeLease.test.tsx -t 'superseded'
```

期望 App 当前代码提前发出 thread/resume；hook 当前代码返回过期 ready。不得把超时视为理想红测。

- [ ] Step 5: 在现有冷恢复分支内部实现顺序，保留前面的 warm/running/remote 早退。准备 Promise 启动前就登记 pendingResumes 和 inFlightResume；replaySession.beginReplay 和 watchdog 延迟到准备成功后。

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

- [ ] Step 6: 增加并运行边界回归：准备超 15s 不触发历史超时；恢复开始后无历史帧仍超时；准备期间重复点击只恢复一次；切走、切走再切回、删除、槽位切换、重连后旧结果无副作用；本地 running/awaiting_approval 仍不 resume；远端不调用 acquire。

```sh
cd desktop
npm test -- src/hooks/useRuntimeLease.test.tsx src/App.test.tsx
npm run build
```

保持原有 historyReady token、历史已同步但激活失败、切换会话不自动发送测试全绿。

- [ ] Step 7: 更新模块文件，将本轮 feature 先登记为 `[ ]`，列出已通过命令与剩余验收；同步索引总数。运行 `cargo fmt --all` 与 `NEXTEST_TEST_THREADS=2 just ci` 后提交 `fix: prepare local runtime before resuming conversations`。若 CI 不绿，不提交“完成”。

## Task 2: 一次显式准备并恢复入口

**Interfaces:**
- Consumes Task 1 的 `selectThread(id, { recovery: true })`、pendingResumes、activationError 和现有项目准备 issues。
- Produces `RuntimeActivationNotice({ local, failed, busy, onRetry })`，四个 props 分别为 boolean、boolean、boolean、`() => void`；组件不发 RPC、不拿 native capability。
- App 内 `retryCurrentActivation(): Promise<void>` 只绑定操作发起时的当前失败会话；用同步 ref 防重复点击，finally 只释放自己的记录。

- [ ] Step 1: 创建组件红测，证明仅本地失败显示按钮，busy 禁用，点击触发回调，不渲染原始错误。

```tsx
render(<RuntimeActivationNotice local failed busy={false} onRetry={retry} />);
fireEvent.click(screen.getByRole("button", { name: "准备运行环境并重试" }));
expect(retry).toHaveBeenCalledTimes(1);
```

附远端、非失败、不含 credential-shaped 文案的测试。busy 测试对同一个真实按钮点击，不直接调用回调绕过 disabled。

- [ ] Step 2: 加入 App 红测：准备先 ready、thread/resume 在 historyReady 后返回 runtime unavailable；project issues 已清空，仍应出现恢复按钮。输入草稿并保留附件，按恢复按钮后 pending acquire 未完成不 resume；managedReady 后只增加一次恢复，成功不发 turn/start 或 thread/start。

```tsx
expect(await screen.findByRole("button", {
  name: "准备运行环境并重试",
})).toBeTruthy();
expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(false);
```

重试时准备失败、双击、切走、删除与再次重试均使用可控 Promise，不用 sleep 制造竞态。

- [ ] Step 3: 执行红测。

```sh
cd desktop
npm test -- src/components/RuntimeActivationNotice.test.tsx
npm test -- src/App.test.tsx -t 'activation recovery'
```

- [ ] Step 4: 组件采用固定文案和 button；App 的恢复动作只调用一次 selectThread，并设置 recovery 禁止等待自动发送。准备与恢复均由 Task 1 统一编排，按钮回调不另外先 prepare，防止连续两次准备。

```ts
const id = store.currentId;
const view = id ? store.peek(id) : undefined;
if (!id || !view?.activationError ||
    view.status === "running" || view.status === "awaiting_approval") return;
await selectThread(id, { recovery: true });
```

失败态不能被 warm 早退吞掉；不使用 forceResume 绕过 running 保护。准备成功不清理用户草稿或附件。将两处“请重新选择此会话重试”改为与实际恢复入口一致的固定指导。

现有 RuntimeStartupNotice.onRetry：被首次新建门禁阻止的其他目录保持 prepare-only；当前同项目失败会话走 retryCurrentActivation；没有失败会话的项目重试仍只准备。整个异步过程绑定原客户端、slot、session、selectionRevision，不能在准备返回后读取新选择并恢复它。

- [ ] Step 5: 运行组件与 App 全部测试/build；特别保留首次新建不自动创建、远端没有本机重试、运行中会话不打断三类旧断言。

```sh
cd desktop
npm test -- src/components/RuntimeActivationNotice.test.tsx src/App.test.tsx
npm run build
```

- [ ] Step 6: 同步模块的本轮阶段证据，`cargo fmt --all`、`NEXTEST_TEST_THREADS=2 just ci` 后提交 `fix: retry failed conversation activation explicitly`。

## Task 3: 原生就绪拒绝诊断，不放宽授权

**Interfaces:**
- Consumes DesktopRuntimeManager.project_state、ControlBackend.lease_is_live、forget_ready。
- Produces私有 `ReadinessDiagnostic`（serde::Serialize），字段为 `scope_key: String`、`attempt_id: Option<String>`、`state: Option<AcquireState>`、`held: bool`、`ready: bool`、`generation: Option<String>`、`lease_live: Option<bool>`、`reason: ReadinessFailure`。
- `ReadinessFailure` 为 `MissingState | ProjectNotReady | LeaseExpired`，serde camelCase。
- 私有 `check_lifecycle_ready(&self, scope: &RuntimeScope) -> Result<(), ReadinessDiagnostic>`；现有 `is_lifecycle_ready -> bool` 包装它，避免两次实时检查。

- [ ] Step 1: 在 runtime_lifecycle.rs tests 中新增三类诊断红测，复用 Wired::connected、scope、root_intent、note_sidecar_epoch 等现有夹具。

```rust
assert_eq!(diagnostic.reason, ReadinessFailure::MissingState);
assert_eq!(diagnostic.lease_live, None);
```

未准备/blocked 时不做实时租约检查；已 ready 后将 `wired.control.knobs.lock().unwrap().served = false`，断言 LeaseExpired、lease_live=Some(false)、manager 内 ready 被清除。断言诊断 JSON 的键集合严格等于白名单字段；reason 是固定 enum，不携带原始错误/请求秘密。

- [ ] Step 2: 在 worktree 准备 native build 要求并运行红测。先阅读项目已有 target-dir 配置，沿用而不建立新的共享 target。按 AGENTS.md 给本 worktree 补 sidecar 二进制链接，不复制正式运行状态。

```sh
mkdir -p desktop/src-tauri/binaries
ln -sf /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/desktop/src-tauri/binaries/yi-agent-aarch64-apple-darwin desktop/src-tauri/binaries/yi-agent-aarch64-apple-darwin
cargo test --manifest-path desktop/src-tauri/Cargo.toml --lib readiness_diagnostic
```

缺二进制时先构建测试所需产物，不链接不存在的目标后宣称测试通过。

- [ ] Step 3: 将原 readiness 逻辑提取为单次求值 Result；lease 失效先捕获诊断所需旧状态再 forget_ready。authorize_intent 在原来门禁位置调用一次，失败时输出 JSON 诊断到 native stderr，继续返回原稳定错误。

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

- [ ] Step 4: 执行诊断、原生完整测试与格式检查，保持现有正常授权、租约失效拒绝、blocked 有租约仍不授权的测试通过。

```sh
cargo test --manifest-path desktop/src-tauri/Cargo.toml --lib
cargo fmt --manifest-path desktop/src-tauri/Cargo.toml -- --check
cd yi-agent-rs
cargo fmt --all
NEXTEST_TEST_THREADS=2 just ci
```

- [ ] Step 5: 同步模块诊断验收证据，提交 `fix: diagnose managed runtime readiness refusals`。

## Task 4: 完整验证、评审、合并与收尾

**Files:** 本轮所有变更、`docs/project-management/subagent-runtime.md`、`docs/project-management/README.md`、本计划。

- [ ] Step 1: 对照设计 §6 逐项记录测试名与结果；运行完整前端验证、native 测试和 Rust CI。不触发真实 LLM API。

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

- [ ] Step 2: 使用 requesting-code-review，评审需求和代码；修复意见依 receiving-code-review 核对证据。重点审查 pendingResumes 清理、watchdog 开始时机、租约陈旧结果、正常等待发送与失败恢复不自动发送的区别。
- [ ] Step 3: 完成全部验收后更新 feature 为 `[x]`、索引完成数和计划清单；未做安装版 GUI 验收明确写成限制，不改已有其他 feature 的状态。按项目要求再次跑 fmt/ci 后提交验证记录。
- [ ] Step 4: 使用 finishing-a-development-branch，回主仓库前检查本轮分支干净和用户原有未跟踪文件仍原样；以 `git merge --no-ff fix/resume-runtime-readiness` 合并，再在 main 的 yi-agent-rs 运行 `NEXTEST_TEST_THREADS=2 just ci`。不得在 main 直接修代码；有问题回 worktree 修。
- [ ] Step 5: 合并验证通过后移除本 worktree 和已合并分支；先看构建进程及磁盘，只用 `just reclaim` 预演确认垃圾，确认无本轮编译再按项目规则清理。打开本轮 diff 并向用户报告测试、合并 commit 和未部署状态。

## Self-Review / Spec Coverage

- 设计 §1、§5 的诊断边界 → Task 3；不把缺 ready 直接断言为竞态或已修复全部 runtime 问题。
- §2、§3 的本地/远端、running、旧请求、缺 cwd、准备门禁、watchdog → Task 1。
- §4 的单次恢复、双击、草稿/附件、不自动发送、不自动创建 → Task 2。
- §6 的十项验收 → Task 1/2/3 的具体验证及 Task 4 的完整门禁。
- §7 的进度维护、评审、工作树/合并 → 每个任务提交步骤及 Task 4。
- 接口一致：prepareProject 返回 AcquireView|null；selectThread recovery 不等价 forceResume；RuntimeActivationNotice 只接四个展示 props；原生 Result 诊断在 UI 不透传。
