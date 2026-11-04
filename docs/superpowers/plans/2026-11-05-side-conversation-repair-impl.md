# 临时旁问三端修复 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 移除三端常驻旁问模块，恢复旁问自身的发送和可见反馈，并使原生 TUI 的返回与应用退出可靠可达。

**Architecture:** 复用现有 SideConversationStore、SideSyncController、React controller、ChatView 与原生 side driver。前端将当前会话身份、草稿收据和呈现接成一条完整路径；TUI 将粘贴、补全和快捷键统一路由到可见会话。两条任务链不修改网络协议或 runner 安全边界，最终汇合做真实环境验收。

**Tech Stack:** React 19、TypeScript、Vitest/Testing Library、Tauri/iOS 共用前端；Rust、Tokio、crossterm、ratatui；项目 justfile CI。

## Global Constraints

- 依据 [修复 spec](../specs/2026-11-05-side-conversation-repair-design.md)，继承 [原 spec](../specs/2026-11-03-side-conversation-design.md) 未被覆盖的只读、快照、隔离与临时存储约束。
- 桌面、手机和原生 TUI 均需修复；手机共用 `desktop/` React bundle，不另建 mobile 应用。
- 不显示常驻“旁问／继续旁问／查看旁问”按钮、独立模块和介绍性横幅；不预留空白区域。
- 当前在旁问时，只在既有输入框 placeholder 和既有状态栏文本位标识 `/side`、只读及 `/back`；不增加布局行或固定高度。
- `/back` 纯本地立即返回，不停止/关闭；Web Running close 条件确认；TUI 命令即确认且立即返回，真实关闭等 driver 帧。
- 不新增 RPC、依赖、持久旁问、嵌套/多旁问、自动回写、旁问附件上传、写工具或旁问审批；不恢复已拆出的看板业务。
- 所有目标由 hostKey、parentId、sideId、generation 和捕获的 client 决定；不使用裸 sideId 跨宿主猜测。
- Creating/断线/同步中/Observe 仍可本地返回；操作锁不禁用整个编辑器。
- 手机 Enter 始终换行；精确可执行命令复用现有主动作，普通父 Running 文本仍 Stop-only。
- 不读取真实聊天或密钥，不更新正式 app，不干扰其他 worktree、relay 或进程。
- 所有产品实现另建 `fix/side-conversation-repair` worktree；当前文档 worktree 不实施产品代码。
- 每次提交前在 `yi-agent-rs/` 执行 `cargo fmt --all && just ci`，再提交；合并 main 后再运行 `just ci`。失败则不提交/不合并，不以局部绿代替门禁。
- 每个可交付任务同步更新相应项目管理条目的代码/命令判据；未完成三端验收时整体修复条目保持 `[ ]`，不使用 `[~]`。

---

## 0. 起点、文件职责与执行规则

调查基线 `6fb5507e`。实现开始时记录实际 HEAD；若相关代码变化，先对照 spec 调整计划，不覆盖其他人的修改。

```bash
export PATH="/opt/homebrew/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
git status --short
git worktree list
git worktree add .worktrees/side-conversation-repair -b fix/side-conversation-repair
```

先用 using-git-worktrees 检查 `.worktrees/` 已忽略；不要复用已有同名活动 worktree。实现分支必须能访问这份 spec/plan：若文档尚未提交，先由用户决定文档交付，再开始执行；不得自行把 main 的未提交文件拷入产品提交。文档未合入不是修改 main 的授权。

| 文件 | 职责与变更边界 |
| --- | --- |
| `desktop/src/lib/sideConversation.ts` | 完整身份、版本化草稿、未确定/明确拒绝提交的内存记录 |
| `desktop/src/lib/sideSync.ts` | 提交收据、同 id 对账、按快照清稿，不处理导航 |
| `desktop/src/hooks/useSideConversationController.ts` | 捕获目标、导航修订、Observe、关闭 CAS，不处理父模型消息 |
| `desktop/src/hooks/useSideConversation.ts` | 完整可见身份、controller 创建/换 client 后轮询与清理 |
| `desktop/src/lib/sideCommands.ts` | 所有输入入口共用裁决，能力/权限/命令例外 |
| `desktop/src/components/MessageInput.tsx` | 编辑、补全和单主动作；side 清稿由 store 收据负责 |
| `desktop/src/App.tsx` | 一处组合完整会话投影，接现有 store/controller/组件 |
| `desktop/src/components/StatusBar.tsx` | 在现有文本位显示会话，不篡改连接状态 |
| `yi-agent-rs/crates/yi-agent/src/tui/app.rs` | 可见输入事件、全局退出优先级、按需操作错误呈现 |
| `yi-agent-rs/crates/yi-agent/src/tui/side.rs` | 可见会话状态、分类/补全作用域与定向错误 |
| `yi-agent-rs/crates/yi-agent/src/side_driver.rs` | 保留既有 runner，补发本地定向操作失败事件 |
| `docs/testing/side-conversation-acceptance.md` | 记录本次真实执行证据，和历史记录分开 |

新增文件只限 `desktop/src/hooks/useSideConversation.test.tsx` 与必要的回归测试；不为测试重写 App，不抽第二套 controller/store。

### 执行与提交节奏

每任务先写下面规定的回归，再执行指定命令确认红，记录真实失败原因；若先通过，补足真实断点用例，不制造失败。随后实现、跑绿、审 diff。测试前确认 `desktop/node_modules` 存在，否则在实现 worktree `npm ci`；不要软链正式应用状态。Rust 使用 worktree 自己的 target，不并发抢同一个 target 构建锁。

下面每任务的“提交”都包含这条不可省略的门禁：

```bash
(cd yi-agent-rs && cargo fmt --all && just ci) && git diff --check
```

门禁未通过，任务停在未提交状态并诊断，不撤掉测试或修改无关功能求绿。第一任务允许暴露后续已有失败，先完成其余修复再按可审核任务组合提交；不谎称中间每项已独立通过全量 CI。每次实际 commit 前重新运行门禁。纯文档阶段不运行这些产品实施步骤。

## 1. 前端任务链

### Task 1: 恢复 App 测试收集，建立接线回归基线

**Files:**
- Modify: `desktop/src/App.tsx`（约 :3009 孤立 JSX 结束符及失效 imports）
- Test: `desktop/src/App.test.tsx`（现有 `App 旁支视图` 组）

**Interfaces:**
- Consumes: 既有 App 和 App 测试的 RPC mock `clients[].requests`、`state.notifHandlers`。
- Produces: 可被 TypeScript/Vitest 解析的 App；保留现有旁问测试组，不添加产品接口。

- [ ] **Step 1:** 保留并运行现有 App 旁支测试，不先删“缺失旁问内容”断言。新增一条导入/渲染回归，沿用文件内既有初始化方式。检查返回 JSX 结构，而不是用快照吞下 parser error。

```tsx
it("App 模块可导入", async () => {
  const module = await import("./App");
  expect(typeof module.default).toBe("function");
});
```

- [ ] **Step 2:** `cd desktop && npx vitest run src/App.test.tsx -t 'App 模块可导入|App 旁支视图'`。基线预期 JSX transform error；若新基线已修复该错误，记录并跳过重复修语法，继续确认功能回归红。
- [ ] **Step 3:** 对照 `git diff 7e71f238^1 7e71f238 -- desktop/src/App.tsx`，仅移除孤立的结束符使 JSX 成对；不用该提交的整文件反向覆盖。把无用 header import 留到 Task 7 与组件一起移除，避免本任务混入行为变化。
- [ ] **Step 4:** 重跑导入用例并 `npx tsc --noEmit`。导入必须通过；记录后续缺失接线断言，不将其改成 skip。不得把 parser fail 当用户问题的唯一根因。
- [ ] **Step 5:** 更新 desktop 进度条目中的阶段证据；执行统一门禁，通过后提交 `fix: restore App JSX test collection`。

### Task 2: 版本化旁问草稿与三态提交收据

**Files:**
- Modify: `desktop/src/lib/sideConversation.ts`, `desktop/src/lib/sideSync.ts`
- Test: `desktop/src/lib/sideConversation.test.ts`, `desktop/src/lib/sideSync.test.ts`

**Interfaces:**
- Produces（导出于 sideConversation.ts）：

```ts
export type SideTarget = Extract<ConversationKey, { kind: "side" }>;
export interface SideDraftSnapshot {
  target: SideTarget;
  text: string;
  draftVersion: number;
}
export type SideSubmitReceipt =
  | { kind: "accepted"; requestId: string }
  | { kind: "uncertain"; requestId: string }
  | { kind: "rejected"; requestId: string; message: string };
```

- `SideView` 增加 `draftVersion: number`（初始 0）和 `rejectedSubmission: { requestId: string; text: string; message: string } | null`（初始 null）；仅内存保存。
- `SideSyncController.submit(sideId: string, text: string, clientRequestId: string): Promise<SideSubmitReceipt>` 替代原 Promise<void>；签名参数不增，内部捕获 view 完整目标/版本。
- `SideConversationStore.setDraft(hostKey: SideHostKey, sideId: string, draft: string): void` 保持签名；文字变化才递增版本。同 generation 基线替换/帧应用不能重置本地版本、pending 或 rejectedSubmission；换代必须隔离旧状态，不能把旧 pending 搬进新旁问。自动首问进入 submit 前在目标草稿未变化时安装问题正文/版本；已有新稿时不覆盖，问题快照单独跟踪。
- Consumes: 既有 `installBaseline`、`adopt`、`setSync`、`accepted_requests`、SideSyncController.client。

- [ ] **Step 1:** 在现有 `makeSideSyncFixture()` 内新增以下断言；补齐成功、明确拒绝、网络丢应答三组。通过 fixture 已有 RPC handler 返回真实 wire 结构，不 mock submit 本身。

```ts
const f = makeSideSyncFixture();
await f.controller.reconcile(PARENT);
f.store.setDraft(f.hostKey, SIDE, "question");
const before = f.store.read(f.hostKey, SIDE)!.draftVersion;
f.store.setDraft(f.hostKey, SIDE, "other");
f.store.setDraft(f.hostKey, SIDE, "question");
expect(f.store.read(f.hostKey, SIDE)!.draftVersion).toBe(before + 2);
```

另用 deferred RPC：捕获提交后把草稿改为别的文字再改回相同文字，resolve 成功后断言原文还在。对明确拒绝：新稿为空且版本匹配才恢复问题；已有新稿时保留新稿且 `rejectedSubmission.text === "question"`。对 uncertain：保留同 requestId，`confirmPending` 沿同 id 对账；generation 换代后旧成功/拒绝都不能动新 view。

- [ ] **Step 2:** `cd desktop && npx vitest run src/lib/sideConversation.test.ts src/lib/sideSync.test.ts`。预期缺失版本/返回收据或相同文字版本回退断言失败。
- [ ] **Step 3:** 将 `draftVersion: 0` 替换成真实捕获版本；所有异步后置写入先验证目标 generation、pending.requestId。成功与对账确认都用同一条件清稿函数，检查 requestId、generation、text、draftVersion；只有 pending 匹配才清 pending。明确拒绝返回 rejected 并记录原因；网络失败返回 uncertain、不抛成可新发错误。同步/权限前置拒绝也转为 rejected（请求未发，调用方可保留原稿）；disposed 不写 store。

```ts
const matches = current?.generation === snapshot.target.generation
  && current.pending?.requestId === snapshot.requestId;
if (matches) {
  this.store.setPending(snapshot.target.hostKey, sideId, null);
  if (current.draftVersion === snapshot.draftVersion
      && current.draft === snapshot.text) {
    this.store.setDraft(snapshot.target.hostKey, sideId, "");
  }
}
```

这里 `snapshot` 在 submit 内由 target、text、真实 draftVersion、requestId 构成；禁止额外读全局 active。unknown accepted_requests 读取失败不产生新 id；同 id 重试仍需身份未换代。原始错误到 UI 的 message 保留，避免 `[object Object]`。
- [ ] **Step 4:** 重跑两个文件并 `npx tsc --noEmit`；更新 controller 测试的 fake submit 返回 `{kind:"accepted",requestId}`，更新所有真实调用处类型使编译通过，不能 `as any` 掩盖 void/receipt 不一致。
- [ ] **Step 5:** 更新 desktop 进度判据，统一门禁后提交 `fix: preserve side drafts with typed submission receipts`。

### Task 3: 固定操作目标和导航修订，取消迟到入场

**Files:**
- Modify: `desktop/src/hooks/useSideConversationController.ts`
- Test: `desktop/src/hooks/useSideConversationController.test.tsx`

**Interfaces:**
- Consumes: Task 2 的 SideTarget/SideSubmitReceipt 与 submit 三参新返回值。
- Produces: `submit(text: string): Promise<SideSubmitReceipt>`；`open(binding: SideHostBinding, parentId: string, question: string): Promise<boolean>` 保持签名，boolean 只表示命令已接纳，首问结果留在精确 SideView。
- Produces: `selectMain(binding: SideHostBinding, parentId: string): void`，供 App 真实父/宿主导航调用；`back(): void` 连 Creating 也可撤销入场意图；`enter(...)` 保持原签名。
- 内部每次用户导航递增 `navigationRevision`；每条变更锁按完整来源身份，不用一个 busy 禁全部输入。全局 `busy` 只作显示，不作导航开关。

- [ ] **Step 1:** 用现有 setup/fakeClient + deferred open 加回归：

```tsx
let release!: (value: unknown) => void;
const wait = new Promise((resolve) => { release = resolve; });
const s = setup({ responses: { "side/open": () => wait } });
let opening!: Promise<boolean>;
act(() => { opening = s.hook.result.current.open(s.binding, "t1", ""); });
act(() => { s.hook.result.current.back(); });
await act(async () => {
  release({ side_id: "s1", parent_thread_id: "t1", generation: 1,
    status: "idle", snapshot_revision: 1, last_seq: 0,
    active_turn_id: null, expires_at: null, stopping: false });
  await opening;
});
expect(s.hook.result.current.active?.kind).not.toBe("side");
```

再添加两个 host 各有同名 s1 的 submit/stop 测试：捕获 A，await 时 enter B，A RPC 仍打 A；同 host generation 更换则拒绝旧收据。Creating 带首问时 `/back` 后释放 open，断言零 turnStart 且原问题留在 A 草稿/待处理记录。已经 turnStart 后返回不能重复/撤回提交。
- [ ] **Step 2:** `cd desktop && npx vitest run src/hooks/useSideConversationController.test.tsx`；预期迟到 open 抢视图、fake submit 参数/receipt 不匹配或裸 sideId 跨 host 失败。
- [ ] **Step 3:** 去掉 submitInternal/findView(sideId)；用 `submitInternal(target: SideTarget, text: string): Promise<SideSubmitReceipt>`，从 store.read(target.hostKey,target.sideId) 校验 parent/generation，捕获 binding.client。open 捕获来源/navigationRevision，在 reconcile 后仅相同导航意图才 enter/发首问；取消意图只安装原 store。用户显式 enter/selectMain/back 都更新修订；后台通知不调用这些方法。重复 open 合并同一在途操作，不丢第二个问题；重复首问在 pending/Running 时明确拒绝并留原输入。
- [ ] **Step 4:** 重跑 controller、sideSync 测试；断言零父 RPC，A/B 同 sideId 不串线，旧 generation 不清新稿。错误按来源保存到 SideView/父侧命令反馈，不使用裸全局 error 冒充别的会话错误。
- [ ] **Step 5:** 更新 desktop 阶段证据，统一门禁后提交 `fix: bind side actions to source and navigation intent`。

### Task 4: Observe 查看与绑定原目标的关闭确认

**Files:**
- Modify: `desktop/src/hooks/useSideConversationController.ts`
- Test: `desktop/src/hooks/useSideConversationController.test.tsx`

**Interfaces:**
- Consumes: Task 3 navigationRevision、selectMain、SideTarget。
- Produces:

```ts
export interface CloseConfirmation {
  target: SideTarget;
  client: RpcClient;
  navigationRevision: number;
  activeTurnId: string | null;
  running: boolean;
}
// parent 参数用来支持主视图关闭后台旁问；内部 list 后捕获精确 target。
requestClose(binding: SideHostBinding, parentId: string): Promise<CloseConfirmation | null>;
confirmClose(confirmation: CloseConfirmation): Promise<boolean>;
```

没有旁问时 null + 按需幂等提示，不虚构关闭；Running false 由 App 立即 confirmClose，true 交确认对话框。实现时同步删除旧零参调用。

- [ ] **Step 1:** 增加 Observe 无参 open 的请求断言：仅 side/list/readItems，零 side/open/turnStart；带问题明确拒绝。增加 A 中 requestClose，切 B 后 confirmClose：请求仍打 A，active 仍 B。增加关闭等待期间 A→B→A 重入：旧导航修订的成功不能强制回 Main；已换代的 confirmation 拒绝，零 close。

```tsx
const confirmation = await s.hook.result.current.requestClose(s.binding, "t1");
expect(confirmation?.target.hostKey).toBe(s.binding.key);
expect(confirmation?.client).toBe(s.client);
expect(confirmation?.activeTurnId).toBe("turn-1");
```

给该用例的 fake side/list 返回 generation=1、status=running、active_turn_id=turn-1 的 s1 摘要；先用 installBaseline/adopt 建立同代 store，与现有 fixture 一致。另覆盖主视图 close、list 空、断线、cancel、close conflict，新轮不得 force close。
- [ ] **Step 2:** 跑 controller 测试，预期旧确认无 target/client 或依赖当前 active 的断言失败。
- [ ] **Step 3:** Observe open 分支只 list→找到父对应摘要→reconcile→允许导航，不调用 open 或续期。close 读取最新摘要并检查 generation；发请求前验证 binding 未换 client/权限/代际，使用 confirmation.client 和 expectedActiveTurnId；迟到结果只更新原对象。仅可见身份与 navigationRevision 同时匹配才回父。后台 close 无需先 enter；缺失目标幂等反馈，不能清别的父 view。
- [ ] **Step 4:** controller + sideSync 全绿；用 spy 精确确认没有 parent send/interrupt、新 client close、强制关闭或 Observe mutation。
- [ ] **Step 5:** 更新 desktop 判据，统一门禁后提交 `fix: scope side observation and close confirmations`。

### Task 5: 按完整身份恢复尾帧检查与重连

**Files:**
- Modify: `desktop/src/hooks/useSideConversation.ts`, `desktop/src/App.tsx`（hook 参数）
- Create: `desktop/src/hooks/useSideConversation.test.tsx`

**Interfaces:**
- Replace: `UseSideConversationDeps.visibleParent: string|null` → `visible: { hostKey: SideHostKey; parentId: string } | null`。
- Consumes: `SideSyncController.setVisible(parentId: string|null): void`, `.client`, `.dispose()`；不改网络协议。
- Produces: controllerFor 新建/重连时立即应用当前 visible，effect 按完整身份清理旧轮询；subscribeParent 使用已有订阅窗口合并入口，不替换全部订阅。

- [ ] **Step 1:** 新测试用 renderHook + 真实 store、mock SideSyncController 构造（记录 setVisible/dispose）覆盖：先 render visible 后首次 controllerFor；相同 parentId 不同 host；同 host/parent 换 client；unmount。构造 mock 仅用于生命周期，不代替 Task 2 流式测试。

```tsx
const visible = { hostKey: binding.key, parentId: "p1" };
const hook = renderHook(() => useSideConversation({
  store, onChange: vi.fn(), subscribeParent: vi.fn(async () => {}), visible,
}));
act(() => { hook.result.current.controllerFor(binding, "p1"); });
// 断言构造 mock 记录的此实例 setVisible 最后一次为 p1。
```

在测试文件定义 `vi.hoisted` 实例数组，mock 类实现 client、setVisible=vi.fn、dispose=vi.fn、accept=vi.fn；真实 binding 按 controller 测试 setup 的 SideHostBinding 结构建立，不依赖 App。
- [ ] **Step 2:** `cd desktop && npx vitest run src/hooks/useSideConversation.test.tsx`；预期旧 visibleParent 参数/新实例未启动轮询失败。
- [ ] **Step 3:** 用 latest.current.visible 同时匹配 hostKey/parentId；新实例 setVisible 时立即应用，旧实例 dispose。effect 只让精确一对可见；删除旧 host controller 时 dispose，unmount 全 dispose。避免 effect 每次 render 重启相同 timer；不对后台 host 调 setVisible(parentId)。
- [ ] **Step 4:** 跑新增 hook + sideSync fake timer 用例；5 秒推进确认缺尾帧能对账，切后台后无旧 RPC。保留现有固定水位分页、seq/终态测试。
- [ ] **Step 5:** 更新 desktop 判据，统一门禁后提交 `fix: restore visible side polling after reconnect`。

### Task 6: 统一命令裁决、单主动作和不阻塞本地返回

**Files:**
- Modify: `desktop/src/lib/sideCommands.ts`, `desktop/src/components/MessageInput.tsx`
- Test: `desktop/src/lib/sideCommands.test.ts`, `desktop/src/components/MessageInput.test.tsx`

**Interfaces:**
- Consumes: SideSubmitReceipt、SideCommand、现有 sideInputPolicy。
- Produces: MessageInput 新可选 prop `onSubmitSide?: (text: string) => Promise<SideSubmitReceipt>`；main `onSend: (text:string)=>Promise<boolean>` 保持兼容。side 必须用 onSubmitSide，由 store 管清稿，不调用普通 onSend 清稿分支。
- `onSideCommand` 保持 bool/Promise<bool> 但包含 scoped help/config/usage；不得转 onSlashCommand。
- 新 `conversationKey?: string`，主为父完整身份、side 为完整身份序列化，用来隔离本地 latch；side textarea 不因 sending disabled，mutation 按目标锁，back 不读 sending。

- [ ] **Step 1:** 在现有 side routing/mobile side commands 测试组添加 policy 表：main close、Observe noarg side、失能仍 back、Running command、父附件拒绝。添加 Creating 的 pending onSideCommand 未 resolve 时，仍可编辑 `/back` 并执行。手机断言只有一个主动作按钮（执行命令或 Stop），无额外命令按钮；Enter 换行、IME 不发送。

```ts
expect(sideInputPolicy({ platform: "mobile", view: "side", running: true,
  attachmentCount: 0, text: "/back", supported: false, canControl: false
}).action).toBe("executeSideCommand");
```

用现有组件 render helper 传 `onSideCommand=vi.fn()`、`onSlashCommand=vi.fn()`，输入 `/usage` 后执行，断言仅前者收到 `{kind:"command",name:"usage",args:null}`。blocked reason 必须在可访问错误文本中可见，稿不清。
- [ ] **Step 2:** `cd desktop && npx vitest run src/lib/sideCommands.test.ts src/components/MessageInput.test.tsx`；预期权限顺序、额外按钮、全输入锁或 scoped handler 断言失败。
- [ ] **Step 3:** policy 优先识别本地 back，再能力/权限；Observe 无参 open 为查看例外，有参拒绝；main close 允许控制端；命令补全按 scope/capability 过滤。重排 runSideCommand 的 command 分支到 onSideCommand，拒绝提示不清稿。手机单个 action：精确可执行命令优先，否则 Running Stop，否则 Send；不能根据前缀 `/b` 绕过 Stop-only。side onSubmitSide 三态都不做组件层清稿，等待 store；仅 accepted 完成发送锁，uncertain 保持 pending 禁重复 mutation。创建和提交期间允许编辑/local back；按 conversationKey 清理局部 sending 状态，旧 promise finally 不清新会话 latch。
- [ ] **Step 4:** 重跑完整 MessageInput（不只 grep 子集）及 sideCommands；主会话 send、附件、desktop slash 和手机 Stop-only 原用例必须保留通过。
- [ ] **Step 5:** 更新 desktop 判据，统一门禁后提交 `fix: keep side navigation reachable in the composer`。

### Task 7: 完整会话投影与轻量 UI 实际接线

**Files:**
- Modify: `desktop/src/App.tsx`, `desktop/src/App.test.tsx`
- Modify: `desktop/src/components/StatusBar.tsx`, `desktop/src/components/StatusBar.test.tsx`
- Delete: `desktop/src/components/SideConversationHeader.tsx`, `desktop/src/components/SideConversationHeader.test.tsx`
- Reuse: `desktop/src/lib/sideItems.ts`

**Interfaces:**
- Consumes: Tasks 2–6 全部接口；现有 ChatView/MessageInput/StatusBar props 不改为父/旁混用。
- Produces: 一处当前会话判别投影；`StatusBar.conversationLabel?: string` 只替换已有文本位，status 仍是连接状态；`usageId` 使用完整会话 key。
- side ChatView 必须使用 `sideItemsForChat(view.items)`；ImageReadCall 必填，使用稳定拒绝函数，不传父 imageCall。

- [ ] **Step 1:** 更新旧“有 header/按钮”断言为无模块断言，不删 `App 旁支视图` 组。使用现有 RPC mock 返回真实 side/list/readItems、用 notifHandlers 投 side/itemUpdated，断言界面出现旁问分块答案且父文本未冒充答案。记录 clients[].requests，强制断言 side/turnStart 一次、父 turn/start/interject/interrupt 零次。

```tsx
expect(screen.queryByRole("button", { name: /继续旁问|查看旁问/ })).toBeNull();
expect(screen.queryByTestId("side-conversation-header")).toBeNull();
expect(screen.getByText("SIDE_ANSWER_CHUNK")).toBeTruthy();
```

状态栏单测：conversationLabel 为 `/side · 只读 · /back 返回` 时 connected dot 仍绿，父 usage 不叠进 side，读屏含当前会话。App 覆盖宽/窄视口、父待审批、side Stop、主 close、确认取消/冲突、Observe、首问失败、新编辑与切父。测试不能 mock ChatView 为永远显示固定文本。
- [ ] **Step 2:** `cd desktop && npx vitest run src/App.test.tsx -t 'App 旁支视图'`；预期 side 内容和 handler 路由断言红，而非 parser error。
- [ ] **Step 3:** 在现有 JSX 组合位置只构造一次判别投影：main 从父 current/session，side 必须精确 read(hostKey,sideId) 且 parent/generation 匹配；目标失效显示原因/回父，禁止 fallback 父 send。各分支同时绑定 items/value/onDraftChange/submit/interrupt/turnActive/usage/anchor/展开状态。父主导航显式 selectMain；side 命令写正确来源反馈。首问父命令清理捕获父文本+修订，用户在 await 期间编辑过则不清；若父 store 无修订字段，用 App 内按完整父身份的编辑计数 ref，不持久化。

```ts
const conversationKey = JSON.stringify(active.kind === "side"
  ? ["side", active.hostKey, active.parentId, active.sideId, active.generation]
  : ["main", active.hostKey, active.parentId]);
```

side 不接父附件/模式/modelPicker/拖拽/drop/image reader；父附件原对象保留。运行/待确认/明确错误在当前来源会话按需可见，不能把 hook.error 直接并入父 lastError。ChatView key、滚动锚点和错误边界均用 conversationKey；side anchor 仅内存，不调用父持久化。父审批 UI 仅 main，side 状态位可提示需返回。关闭对话框仅操作捕获 confirmation，取消不 back。

删除 header 组件及引用，删除常驻 sidebarEntry/继续入口；不删除 RuntimeVersionBanner 或看板组件。side usage.total 的 cache_creation/cache_read 映射为 UsageTotals.cacheWrite/cacheRead，subagentTotals 传零值、count=0、fallbackUsage=null；不沿用父模型/用量。无参空 side 是空态，不显示模型等待。
- [ ] **Step 4:** `cd desktop && npx tsc --noEmit && npx vitest run && npm run build`。另外断言 side 事件不会进入父摘要、未读、排序、搜索及缓存 writer；原主发送、审批、运行时版本、看板插件、恢复测试保持绿。
- [ ] **Step 5:** 更新 desktop 判据和验收记录的自动测试证据（真机未跑仍未完成），统一门禁后提交 `fix: render and route the active side conversation`。

## 2. 原生 TUI 任务链

Task 8–10 可与前端 Task 2–7 并行，但不能由两个代理同时修改 app.rs。TUI 不是独立 crate，命令使用 `-p yi-agent --bin yi-agent`。

### Task 8: 统一可见输入、Paste 和补全，证明实际返回

**Files:**
- Modify/Test: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`
- Modify/Test: `yi-agent-rs/crates/yi-agent/src/tui/side.rs`

**Interfaces:**
- Consumes: 既有 SideSession.input/history/views、InputLine、classify_conversation_input、sync_popup、run_loop_with_side、ScriptedEvents、TestBackend。
- Produces: Event::Paste 依据 side.views.active 路由；补全显示和执行使用同一可见 input；不改 SideDriverCommand。

- [x] **Step 1:** 在 app.rs tests 复制既有 `side_events_never_reach_main_turn_log` 的完整 run_loop_with_side 初始化（保留真实 channels/SideRuntime），新增命名含 `side_repair_` 的循环回归。初始化父草稿 `PARENT_DRAFT`，预投有效 side status 帧并显式进入 side；事件按交付顺序逆序存进 ScriptedEvents。最小序列为 Paste(`/back`)、Enter、Ctrl+Q；断言最终父 input 为 PARENT_DRAFT，最后绘制是 Main，side_cmd_rx 没收到 Interrupt/Close，decision_rx 为空。另加逐字符 `/back`、Tab补全后 Enter；不能只直接调 handle_side_key。

```rust
let mut delivered = vec![
    Event::Paste("/back".into()),
    Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
];
delivered.reverse();
let events = ScriptedEvents { events: Rc::new(RefCell::new(delivered)) };
```

为最终屏幕检查保留 TestBackend buffer，在退出前至少一次绘制（用既有 loop 的事件轮次，不加任意 sleep）。父 pending approval 再跑相同序列，decision channel 零消息；拒绝的 nested side/管理命令和 Running 文本恢复原稿，不被 take_submitted 吞掉。
- [x] **Step 2:** `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent side_repair_`；预期 Paste 写错父草稿/不能回 Main 或输入被审批吞掉断言失败。若用例没执行到侧视图，先修测试时序，不能宣布红已证实。
- [x] **Step 3:** Paste 在 side 分支调用 side.input 的粘贴 API，按普通编辑保留字节，不自动执行；不得调用父 handle_paste 的审批守卫。补全从同一可见 input 获取 query，side 候选限制为 back/side-close/help/config/cost/quit/q，Tab写入不提交，完整命令一次 Enter 执行。切视图清 popup 的候选/选中索引。提交前保存 text；返回消费命令，拒绝/发送未投递恢复 text；不声称找回用户手动覆盖掉的旧文字。滚动与折叠继续只改可见 history。
- [x] **Step 4:** 跑 `cargo test -p yi-agent --bin yi-agent tui::` 与 side_repair_；覆盖 resize、Shift滚动和父审批 Command/Decision 原规则不变，断线 back 不等 driver。
- [x] **Step 5:** 更新 TUI 模块阶段证据，统一门禁后提交 `fix: route TUI side paste and completion to visible input`。

**证据（2026-11-05,本 worktree）:**
- 循环回归（真实 `run_loop_with_side` + `ScriptedEvents` + `TestBackend`）:
  `side_repair_paste_back_through_the_loop_returns_to_main_and_keeps_the_parent_draft`、
  `side_repair_paste_back_survives_a_parent_approval_through_the_loop`、
  `side_repair_typed_and_completed_back_work_through_the_loop`。
- 按键级:`side_repair_completion_offers_only_executable_side_commands`、
  `side_repair_tab_completes_then_enter_executes`、`side_repair_tab_without_popup_does_not_edit_the_draft`、
  `side_repair_esc_closes_the_popup_before_stopping`、`side_repair_management_commands_are_refused_without_clearing_the_draft`、
  `side_repair_nested_side_is_refused_without_clearing_the_draft`、`side_repair_exit_commands_quit_instead_of_reaching_the_model`、
  `side_repair_close_reports_an_undelivered_command`。
- 红已证实（不是「没跑到」）:临时把 `Event::Paste` 路由退回父 `handle_paste` →
  `side_repair_paste_back_through_the_loop_...` FAILED；临时把按键/粘贴路由退回本帧开头
  的 `in_side` 快照 → 两条循环用例同时 FAILED。恢复后全绿。
- `cargo test -p yi-agent --bin yi-agent side_repair_` = 14 passed；
  `cargo test -p yi-agent --bin yi-agent tui::` = 697 passed, 0 failed。

### Task 9: 全局退出优先级、两步 Ctrl+C 与失败反馈

**Files:**
- Modify/Test: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`, `yi-agent-rs/crates/yi-agent/src/tui/side.rs`
- Modify/Test: `yi-agent-rs/crates/yi-agent/src/side_driver.rs`
- Verify only: main 中现有 `TuiExitCoordinator` 与租约统一收尾入口（不另建退出器）

**Interfaces:**
- Consumes: loop_bk.pending_quit 单一状态、KeyOutcome::Quit、SideDriverCommand::{Interrupt,Close,Exit}、ConversationEvent.target/turn_id。
- Produces: 本地 `ConversationPayload::SideOperationError { operation: SideOperation, message: String }`，`SideOperation` 在 tui/side.rs 定义为 `Open | Submit | Interrupt | Close`；仅 Rust 内存事件，不改生成协议。
- SideSession 记录带 target 的待显示操作错误，Main 可呈现后台 Close 失败；不能把错误当父 transcript 或落盘。
- 本任务最小扩展本地 `SideDriverCommand::Close { expected_target: ConversationTarget, expected_active_turn_id: Option<String> }` 与 `Interrupt { expected_target: ConversationTarget, turn_id: String }`。driver 处理前核对真实 side_id/generation，不匹配则产生该旧目标的操作失败，不作用到当前 runner。更新全部构造点及测试；该 Rust 进程内类型不生成网络绑定，原 RPC 不变。

- [x] **Step 1:** 用 Task 8 的完整循环用例初始化方法新增：side Idle/Running Ctrl+C、Ctrl+C→编辑→Ctrl+C、runtime popup/父审批弹窗 Ctrl+Q，以及 `/q`、`/quit`。断言仅第一次 Ctrl+C 向可见 side Interrupt，第二次走 Quit；父 interrupt_rx 为空。Close/Interrupt 用容量1且预填满的 channel、drop receiver 两组，命令仍立刻返回/可退出，屏幕可见失败。

```rust
let (side_tx, mut side_rx) = tokio::sync::mpsc::channel(1);
let target = ConversationTarget::Side { side_id: "side-1".into(), generation: 1 };
side_tx.try_send(crate::side_driver::SideDriverCommand::Interrupt {
    expected_target: target.clone(),
    turn_id: "occupied".into(),
}).unwrap();
assert!(side_tx.try_send(crate::side_driver::SideDriverCommand::Close {
    expected_target: target,
    expected_active_turn_id: Some("side-turn".into()),
}).is_err());
assert!(side_rx.try_recv().is_ok());
```

这段验证 fixture 确实满；产品回归必须通过循环输入 `/side-close` 并断言 Main + “已返回，旁问关闭未送达”，不是以此通道单测替代。driver 模拟 close conflict 要产生 SideOperationError(Close)，在 Main 可见且仍能 `/side-close` 重试；旧代错误不污染新代。
- [x] **Step 2:** `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent side_repair_`；预期 Ctrl+C 在 side 无效、popup 吞退出或失败无反馈。
- [x] **Step 3:** 将 Ctrl+Q/Ctrl+C 的应用级判断放在 handle_key_intercepts/runtime popup 之前，只维护 loop_bk.pending_quit；第一次按 visible_stop_target 分派当前轮、第二次 KeyOutcome::Quit，普通有效编辑/导航/提交清确认。退出命令在 side 分类前处理，不将其投给模型。Esc仍先关 popup，再仅停可见 side；Idle无动作。

所有 side try_send 改为显式 match Full/Closed，失败保存 target/message 并呈现。Close 无论投递与否立即 back，不能标 closed；保留原后台 side 可重试。driver handle_command 在各错误分支向 report_error 传 operation；report_error 保留 transcript Notice，另 dispatch 带原目标的 SideOperationError，即使无 active turn 也可显示操作失败。Open 尚无 runner 时发 `ConversationEvent { target: ConversationTarget::Main, turn_id: None, event: ConversationPayload::SideOperationError { operation: SideOperation::Open, message } }`；主循环须在父事件/落盘管线前单独识别此本地操作反馈，写入按需 notice，不交给丢弃 Main 的 SideSession::apply_event，也不伪造 side 身份。已有 runner 的错误使用其真实 side_id/generation。主视图只渲染该后台操作错误，不写 HistoryState/TurnLog。close 成功/新重试受理后清对应提示，其他 side 状态帧不能导航。
- [x] **Step 4:** 跑 TUI 全组及 side_driver 测试；验证关闭后重开、旧 generation 帧、后台状态不拉回。使用真实清理器测试确认 Quit 取消父/side、有界等待、租约释放；不能只断言 KeyOutcome::Quit 或用 kill -9 代替收尾。未建立租约 fixture 时先补现有清理器测试，不删 spec 的租约断言。
- [x] **Step 5:** 更新 TUI 判据，统一门禁后提交 `fix: make TUI side exit and command failures observable`。

**证据（2026-11-05,本 worktree）:**

发现并修掉三处真实缺陷（不是「测试写错了」）：

1. **旁支通知从未渲染。** `/cost`、管理命令拒绝、`/side-close` 未送达都写进
   `SideSession::notice`，而绘制只调 `side_header_line(...)`——这个字段从来没被
   读出来。用户看到的是「按了没反应」。现在 `side_status_line` 在有通知时**只画
   通知**（状态行只有一行，把长中文 header 接在前面会把错误尾部裁掉，「未送达」
   恰好在末尾）。
2. **过期目标的错误被静默丢弃。** `apply_event` 里 `stale` 检查在
   `SideOperationError` 分支之前 `return Discard`，于是「旧代关闭失败」——最需
   要告诉用户的那种——永远到不了界面。错误帧现在先于过期检查处理，并按目标分流：
   当前可见旁支进 `notice`，主视图或旧旁支进 `main_notice`。
3. **关闭未送达的提示落在不可见处。** `submit_side_draft` 先 `views.back()` 再写
   `side.notice`，那一刻用户已在主视图、旁支状态行不再渲染。改为投递结果落在
   切换之后可见的位置（`main_notice`），并带上 operation + target。

新增/更新用例：
- `side_repair_first_ctrl_c_in_side_stops_only_the_side`（第一下只发 side
  Interrupt、父 `interrupt_rx` 为空；第二下退出，用「后续输入不再产生 Submit」
  判定，不用退出后的屏幕内容——退出后循环不重绘，屏幕停在前一帧的旁支视图）
- `side_repair_ctrl_q_quits_even_with_the_runtime_popup_open`、
  `side_repair_ctrl_q_quits_with_a_pending_parent_approval`、
  `side_repair_quit_commands_leave_the_side_without_a_submit`、
  `side_repair_editing_after_ctrl_c_rearms_the_confirmation`
- `side_repair_close_survives_a_full_side_channel`（容量 1 预填满）、
  `side_repair_close_survives_a_dropped_side_receiver`（drop receiver）
- `visible_operation_error_goes_to_the_side_notice`、
  `stale_operation_error_does_not_touch_the_current_side_view`
- driver：`stale_close_target_reports_error_and_keeps_the_live_side`、
  `stale_interrupt_does_not_stop_the_live_turn`（空 turn_id 也不能误停）、
  `failed_open_reports_a_main_target_error`（无 runner 时发 Main 目标错误）

红已证实（临时改回旧行为，跑完即恢复）：
- 把 `SideOperationError` 分支移回 `stale` 检查之后 →
  `stale_operation_error_does_not_touch_the_current_side_view` FAILED。
- 把关闭未送达提示写回 `side.notice` →
  `side_repair_close_survives_a_full_side_channel` 与
  `..._a_dropped_side_receiver` 同时 FAILED。

测试基础设施修正（真实缺陷，非放宽断言）：`frame_text` 之前逐格拼接缓冲区，
而 ratatui 的 `Buffer::diff` 会**跳过**紧跟宽字符的那一格（真实终端画汉字时连带
覆盖右侧格，故不下发），`TestBackend::draw` 只是逐格赋值，那一格保留上一帧内容。
于是把「旁问（只读）…」换成「关闭旁问失败…」时读出来会交错成
「旁·问父关会闭话未空送闲达」。`frame_text` 现在按终端语义推进 x（宽字符占两列），
这与真实终端显示一致。

门禁：`cargo test -p yi-agent --bin yi-agent tui::` = 706 passed, 0 failed；
`cargo test -p yi-agent --bin yi-agent side_repair_` = 21 passed；
`cargo test -p yi-agent --bin yi-agent side_driver` = 9 passed；
`cargo fmt --all` + `just fmt-check` 通过；`cargo clippy -p yi-agent --all-targets` 无告警。

### Task 10: 删除 TUI 介绍条，保持既有状态栏与落盘隔离

**Files:**
- Modify/Test: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`, `yi-agent-rs/crates/yi-agent/src/tui/side.rs`

**Interfaces:**
- Consumes: 既有 render_statusbar_with、HistoryView、side_header_line 调用位置、Task 9 按需错误。
- Produces: 原状态行内 `/side · /back 返回` 短标签；不增加 Rect/行高，不遮历史首行。

- [ ] **Step 1:** TestBackend 80×24、32×12 各用深/浅主题绘制，旁问首条文本设 `FIRST_SIDE_ITEM`，断言首条不被 Clear/header 覆盖；status 有 `/side` 与 `/back`，普通父视图无常驻旁问提示。CJK 字符宽度使用现有渲染工具，不以 byte len 截断。

```rust
assert!(screen.contains("FIRST_SIDE_ITEM"));
assert!(screen.contains("/side"));
assert!(screen.contains("/back"));
assert!(!screen.contains("旁支可用工具（只读）"));
```

`screen` 从该用例实际 terminal.backend().buffer() 按行收集可见字符得到；帮助命令主动显示工具说明不在此禁止范围。
- [ ] **Step 2:** `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent side_repair_`；预期旧独立介绍条遮行/占位失败。
- [ ] **Step 3:** 去掉 HistoryView 上额外 Clear/Paragraph 的 header 绘制；在已有状态文本预算中选 side 标签，窄屏优先身份/返回，错误按需显示且不被父费用覆盖。父视图照旧，side helper 不再用于宣传条，删除无调用代码/测试，不移除主动 `/help`。
- [ ] **Step 4:** `cargo test -p yi-agent --bin yi-agent tui::`，再 `cargo test -p yi-agent --bin yi-agent side_events_never_reach_main_turn_log`；确认过滤器实际执行至少一条测试。检查 side 错误/正文没有进入父 resume/TurnLog。
- [ ] **Step 5:** 更新 TUI 阶段证据，统一门禁后提交 `fix: keep TUI side identity in the existing status line`。

## 3. 汇合验证与交付

### Task 11: 三端证据、验收覆盖和全量门禁

**Files:**
- Modify: `docs/testing/side-conversation-acceptance.md`
- Modify: `docs/project-management/desktop.md`, `docs/project-management/yi-agent-tui.md`, `docs/project-management/README.md`
- Modify: `docs/superpowers/specs/2026-11-03-side-conversation-design.md`（保持覆盖引用，不再要求 header）

**Interfaces:**
- Consumes: Tasks 1–10，spec W01–W16/T01–T10。
- Produces: 每个验收 ID 的测试全名、命令、实际结果和证据路径；未执行项明确记未执行，不以 mock/viewport 代替真机。

- [ ] **Step 1:** 在 acceptance 文档新增 `2026-11-05 修复验收` 章节，先将所有目标列为未执行，记录 git HEAD、桌面/手机/TUI/app-server 构建身份、hostKey 的脱敏标识、是否同一 relay。不要复制用户真实问题或完整配置。
- [ ] **Step 2:** 顺序执行自动验证并留日志，任何失败先诊断；禁止把基线 App parser 错误当成可接受失败。

```bash
(cd desktop && npx tsc --noEmit && npx vitest run && npm run build)
(cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::)
(cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent side_events_never_reach_main_turn_log)
(cd yi-agent-rs && cargo fmt --all && just desktop-check && just ci)
git diff --check
```

- [ ] **Step 3:** 按 spec §8.3 在隔离 YI_AGENT_HOME 验证桌面真实 WebView、iPhone/relay、真实 PTY。PTY bracketed paste 输入字节为 `\x1b[200~/back\x1b[201~\r` 和 `\x1b[200~/side-close\x1b[201~\r`，分别确认返回与关闭；再测 Esc、Ctrl+C×2、Ctrl+Q、退出命令。先 mock provider；经授权后至少一组真实模型流式回答。无真机/授权就明确未执行，整体功能不勾完成、不宣布修复验收通过。
- [ ] **Step 4:** 将 W/T 对应测试名与真实证据填入下表所列覆盖路径；检查父任务持续完成、父草稿附件逐字保留、父日志不含 side marker。无静默错误、无宣传横幅、无错会话 Stop。同步模块计数：只按真实 `[x]`/`[ ]`/`[-]` 统计，不因写了 spec/plan 加一个产品完成数。
- [ ] **Step 5:** 发起 requesting-code-review，修复被证实的问题并重跑门禁；通过后提交 `test: verify side repair across desktop mobile and TUI`。按 finishing-a-development-branch 给用户集成选择，不自动合 main/入队。获批准合并后在合并结果再次 `just ci`，确认完成再清 worktree/分支；清理 target 前检查进程和 dry-run，不动仍活跃目录。

## 4. 验收到任务的追踪表

| 验收 ID | 实施任务 | 自动验证与实机补充 |
| --- | --- | --- |
| W01 | 7 | App/StatusBar 结构与宽窄视口；真实 WebView+iPhone |
| W02 | 3,6,7 | 命令/控制器/App 请求精确计数与零父 RPC |
| W03 | 7 | App 实际 itemUpdated 分块显示；真实模型 |
| W04 | 6,7 | App 父 Running/approval 与 side Stop |
| W05 | 3,7 | App 草稿/附件/锚点/展开状态与后台通知 |
| W06 | 2,3,4,7 | 双 host 同 sideId、generation、迟到成功/失败 |
| W07 | 2,6,7 | rejected/policy/sync/model error 可见与保稿 |
| W08 | 2,7 | sideSync same requestId；App 待确认显示；手机断网 |
| W09 | 5 | 新 hook 生命周期与 sideSync fake timers；手机唤醒 |
| W10 | 6,7 | MessageInput 单主动作与 IME/Enter；iPhone |
| W11 | 4,6,7 | Observe read-only、失能/断线仍可返回 |
| W12 | 6,7 | scoped 命令只读数据，不落父 slash handler |
| W13 | 2,3,6,7 | ABA草稿版本、双击、Creating back、自动首问失败 |
| W14 | 4,7 | close CAS/取消/冲突、导航修订、跨端关闭 |
| W15 | 6,7 | 附件与拒绝 reader、父持久化 spy |
| W16 | 4,6,7 | Main后台close与list空幂等反馈 |
| T01 | 8 | 完整循环手打/Paste/补全；真实PTY |
| T02 | 8,9 | 完整循环Close投递/即时返回；真实PTY |
| T03 | 8 | 父审批状态下Paste与decision零消息 |
| T04 | 8,9 | 多状态本地返回、旧帧/重开 |
| T05 | 8 | 完整循环补全、草稿、resize |
| T06 | 9 | 循环Ctrl+C确认/解除；真实退出清理器 |
| T07 | 9 | 弹窗/审批优先级、退出命令与租约收尾 |
| T08 | 8,9 | popup优先Esc、可见轮停止，不误停父 |
| T09 | 9 | Full/Closed/driver拒绝可见，不假报关闭 |
| T10 | 10 | 窄屏/主题/CJK/父落盘测试；实机屏幕 |

## 5. 计划复核与交付边界

- 第一轮：逐段对照 spec，补齐 Creating 下本地返回、首问取消入场、父附件、Observe、主视图关闭后台旁问与真实测试入口。
- 第二轮：核对接口，receipt 在 store/sync/controller/composer 一致；CloseConfirmation 不再混用旧 sideId-only 结构；visible 改完整 host+parent；所有新字段有初始化，旧 fake submit 需随类型更新。
- 第三轮：核对导航修订、ABA草稿版本、迟到关闭不抢视图、TUI take_submitted 保稿、driver 失败可见性与退出收尾；明确未知真实环境不能标通过。
- 红绿步骤中的代码为实施用约束/关键断言；实际执行必须使用文件内完整 fixture，测试名与本计划/验收记录同步。不得只添加断言但不执行目标用例，或以 mocked success 替代实际 App/loop。
- 这是实施计划，不是实施结果；编写时未运行以上产品修复测试，没有把文档审查写成代码/真机通过。
