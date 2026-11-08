# Session 显示收敛 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 客户端已有的普通转录通过滚动自行覆盖视口，并保住阅读锚点，不要求再发送或切回会话。

**Architecture:** 保持原生滚动、spacer 与恒挂 LiveTail。将绝对索引测量、有限窗口收敛、用户意图/位置写入和会话级尺寸账本分开；ChatView 只接线，不另造补偿规则。

**Tech Stack:** React 19、TypeScript、Vitest、项目 CSS、Node 22+ 原生 WebSocket/CDP、Tauri WKWebView。

## Global Constraints

- 真源：[Spec rev2](../specs/2026-11-08-session-completeness-repair-design.md) §4、D01–D09；[审核记录](../reports/2026-11-08-session-completeness-spec-review.md)。本 Plan 不实现 RPC 同步，后者见 [总入口/数据 Plan](2026-11-08-session-completeness-repair-impl.md)。
- initialSize=60、pageSize=200、maxMounted=400、buffer=20；实际 DOM≤400，tail 在历史窗口外时预留 1。
- 不替换原生滚动、不全量渲染、不为本工作引入新依赖。真实几何不能在 jsdom 验收。
- 本文所有步骤未执行；当前诊断并不是修复回归。正式 app 和生产会话数据禁止用于自动 fixture。
- 在执行时新建代码 worktree；不能在 docs 分支或 main 写实现。每个实现提交前 `cd yi-agent-rs && cargo fmt --all && just ci`；同时通过本 Task 的前端测试。提交前同步对应进度描述，但全部判据未通过前不要勾选 feature。
- 不把另一个会话/宿主的回调写到当前 scroller；正文暂时 stale 与旧发送门控分开。

---

## 文件职责与接口冻结

| 文件 | 操作 | 职责 |
| --- | --- | --- |
| `desktop/src/lib/transcriptGeometry.ts` | 新建 | 扫描≤400挂载行，输出真实可视范围、历史边界与 tail 几何 |
| `desktop/src/lib/transcriptWindow.ts` | 修改 | 绝对索引纯计算、一方向开窗、trim 和 tail 名额 |
| `desktop/src/lib/transcriptConvergence.ts` | 新建 | 一次调度周期的预算、无进展与取消状态机 |
| `desktop/src/hooks/useTranscriptWindow.ts` | 修改 | 仅安排帧和 post-commit 补测，不写 scrollTop |
| `desktop/src/lib/transcriptScroll.ts` | 新建 | 阅读意图与唯一位置决策；不直接持 DOM |
| `desktop/src/hooks/useTranscriptScroll.ts` | 新建 | DOM 写入、mounted-row ResizeObserver 和锚点恢复 |
| `desktop/src/lib/heightBook.ts` | 修改 | positive-height record、revision、稀疏高度与定位索引 |
| `desktop/src/components/ChatView.tsx` | 修改 | 给行绝对 index；用新几何/滚动协调器替换旧测量和多写入者 |
| `desktop/src/App.tsx` | 修改 | 传宿主归属、epoch/布局设置、保持发送/搜索命令接线 |
| `desktop/scripts/browser/session-completeness.{html,tsx}` | 新建 | 真 ChatView + CSS 的隔离固定资料 fixture |
| `desktop/scripts/browser/session-completeness-check.mjs` | 新建 | CDP 操作与 D 组报告，无生产 sidecar |

单测放在对应 `*.test.ts` / `*.test.tsx`。保持既有 `useStickToBottom.ts` 给其他消费者的公共行为，不做全项目滚动重构；ChatView 改用 `useTranscriptScroll` 的兼容命令名。

## 依赖与执行顺序

`A1 → A2 → A3 → A4 → A5`。A1/A2 只决定挂载，不宣称首次补偿已修；A3/A4 必须在真实 fixture 上获得 D05 判别性红证据。可与数据 Plan 的早期协议任务隔离并行，但两边都修改 ChatView/App 的阶段必须串行集成。

### Task A1：真实可视测量与有方向的窗口纯函数

**Files:** Create `desktop/src/lib/transcriptGeometry.ts`, `transcriptGeometry.test.ts`; Modify `desktop/src/lib/transcriptWindow.ts`, `transcriptWindow.test.ts`, `desktop/src/components/ChatView.tsx`（当前 measureVisible 与 HistoryWindow/LiveTail）；Test `desktop/src/components/ChatView.test.tsx`。

**Interfaces:**

```ts
export interface RowGeometry { id: string; index: number; top: number; bottom: number; region: "history" | "tail" }
export interface WindowMeasurement {
  visible: { first: number; last: number } | null; // absolute indexes, intersecting rows only
  history: { first: number; last: number; gapTopPx: number; gapBottomPx: number } | null;
  tailVisible: boolean;
  viewportTop: number;
  viewportBottom: number;
}
export function measureTranscript(scroller: HTMLElement): WindowMeasurement | null;
// Replace NextWindowInput.visible with WindowMeasurement|null; retire the old
// VisibleRange geometry fields from trim callers (visible indexes are absolute).
export interface NextWindowInput {
 current: WindowState|null; itemCount:number; limits:WindowLimits;
 visible:WindowMeasurement|null; following:boolean;
 direction?:"up"|"down"|null; focus?:FocusRequest|null;
}
```

- [ ] **Step 1 — 写失败测试**：可在单测注入 rect，但真实浏览器判据留给 A5。下面纯函数测试直接覆盖当前错误口径；补四个数据集：只有 tail 相交、视口在上下 spacer、tail 不在区间、逻辑区间到尾但 following=false。

```ts
it("history gap is not hidden by the disconnected tail", () => {
  const m: WindowMeasurement = {
    visible: null,
    history: { first: 100, last: 298, gapTopPx: -2000, gapBottomPx: 900 },
    tailVisible: false, viewportTop: 3000, viewportBottom: 3800,
  };
  const w = nextWindow({ current: {start:100,end:299,following:false},
    itemCount:600, limits:DEFAULT_LIMITS, visible:m, following:false, direction:"down" });
  expect(w.end).toBeGreaterThan(299);
  expect(w.following).toBe(false);
  expect(w.end-w.start + Number(w.end<600)).toBeLessThanOrEqual(400);
});
```

- [ ] **Step 2 — 验证红**：`cd desktop && npm test -- src/lib/transcriptWindow.test.ts src/lib/transcriptGeometry.test.ts`。先确认新接口类型/当前口径产生失败；保留日志，不能只看到零匹配就继续。
- [ ] **Step 3 — 最小实现**：行根节点同时输出 `data-item-id`, `data-item-index`, `data-transcript-region`。HistoryWindow 的 index 为原数组绝对位置，LiveTail index 为 `items.length-1`。几何使用 viewport rect，过滤 `bottom>viewTop && top<viewBottom`，无交集返回 null visible；历史 gap 仅由 history 区域首末行算。

```ts
const rect = scroller.getBoundingClientRect();
const viewTop = rect.top + scroller.clientTop;
const viewBottom = viewTop + scroller.clientHeight;
const visible = rows.filter(r => r.bottom > viewTop && r.top < viewBottom);
const reservedTail = end < itemCount ? 1 : 0;
const capacity = limits.maxMounted - reservedTail;
// One cycle has one direction; do not execute both up and down branches.
if (direction === "up") start = Math.max(0, start - limits.pageSize);
else if (direction === "down") end = Math.min(itemCount, end + limits.pageSize);
// Recompute reservedTail after extension, trim the opposite edge, retain visible indexes.
```

删除 hook 的 `base+raw.first/last` 换算；未布局 null 只暂停，不改变用户 following。nextWindow 不再在 end===itemCount 时恢复 following，focus 的意图由 A3 统一释放。trim 优先保朝向视口的新页，不能马上删掉刚加载页。
- [ ] **Step 4 — 验证绿**：上述命令、`npm test -- src/components/ChatView.test.tsx src/hooks/useTranscriptWindow.test.tsx`、`npx tsc --noEmit`。旧测试按新契约修改有符号 gap/绝对 index 的输入，不删除旧 spacer/owner 回归。
- [ ] **Step 5 — 提交**：完成全局门禁并更新 desktop 未完成项证据后，`git add desktop/src docs/project-management/desktop.md && git commit -m "fix: measure actual transcript viewport and reserve tail"`。

### Task A2：post-commit 有界收敛调度

**Files:** Create `desktop/src/lib/transcriptConvergence.ts`, `transcriptConvergence.test.ts`; Modify `desktop/src/hooks/useTranscriptWindow.ts`, `useTranscriptWindow.test.tsx`; Test `desktop/src/lib/transcriptWindow.test.ts`。

**Interfaces:**

```ts
export interface ConvergenceCycle {
  generation: number; owner: string; direction: "up"|"down"|null;
  remaining: number; lastSignature: string|null; focusedOnce: boolean;
}
export function beginConvergence(owner:string, itemCount:number, generation:number):ConvergenceCycle;
export type ConvergenceDecision = "settled"|"advance"|"focus"|"stalled"|"paused";
export function decideConvergence(c:ConvergenceCycle, signature:string,
  covered:boolean, layoutReady:boolean):ConvergenceDecision;
// Hook consumes following:()=>boolean and ownerKey:string; exposes displayError:string|null.
// nudge(reason?:"user"|"layout"|"commit"|"items"):void
```

- [ ] **Step 1 — 写失败测试**：fake rAF 只替代时钟，measure 返回一系列真实含义的 WindowMeasurement。第一次外部 nudge 后，模拟窗口提交改变测量，**不再次派发 scroll**；要求自行开至覆盖。另测固定签名无进展只允许一次 focus、user nudge 作废旧 cycle、hidden 暂停、StrictMode 清帧号。

```ts
function outsideMiddle():WindowMeasurement {
 return {visible:null,history:{first:340,last:598,gapTopPx:12000,gapBottomPx:-20000},
  tailVisible:false,viewportTop:60802,viewportBottom:61622};
}
function coveredMiddle():WindowMeasurement {
 return {visible:{first:297,last:302},history:{first:140,last:538,gapTopPx:-20000,gapBottomPx:-20000},
  tailVisible:false,viewportTop:60802,viewportBottom:61622};
}
async function flushFrame() {
 await act(async () => { vi.advanceTimersToNextFrame(); });
}
// beforeEach: vi.useFakeTimers(); stub rAF to setTimeout(cb,16), cancel to clearTimeout.
// afterEach: unmount/cleanup first, vi.unstubAllGlobals(), vi.useRealTimers().
it("a commit does not require a second external scroll", async () => {
  const measurement = vi.fn(() => outsideMiddle()); // define outsideMiddle in this test file using A1 shape
  const {result,rerender} = renderHook(() => useTranscriptWindow({
    ownerKey:"host-a/t/0", threadId:"t", itemCount:600,
    measure:measurement, book:createHeightBook(), following:()=>false,
  }));
  act(() => result.current.nudge("user"));
  await flushFrame(); // local helper uses act(() => vi.advanceTimersToNextFrame())
  measurement.mockReturnValue(coveredMiddle()); // same fields; visible={first:297,last:302}
  rerender();
  await flushFrame();
  expect(result.current.window.start).toBeLessThanOrEqual(297);
  expect(result.current.window.end).toBeGreaterThan(302);
});
```

`gapTopPx=historyFirst.top-viewportTop`，`gapBottomPx=viewportBottom-historyLast.bottom`；正值表示视口越过该历史边界进入 spacer，负值表示窗口尚覆盖该边界。`outsideMiddle` 与 `coveredMiddle` 仅用于方向与提交调度单测，真实几何在 A5 验证。hook 的新公开输出为 `window:WindowState`；未布局暂停后必须由可见/resize事件恢复调度。
- [ ] **Step 2 — 验证红**：`cd desktop && npm test -- src/hooks/useTranscriptWindow.test.tsx src/lib/transcriptConvergence.test.ts`。确认断言失败于“无第二个外部 scroll 不推进”，不只失败于 helper/导入错误。
- [ ] **Step 3 — 最小实现**：初始化 remaining=`Math.ceil(itemCount/200)+2`；只有有效开窗消耗预算，重复签名进入 focus/stalled，不用 rAF 重置整个预算。React layout effect 观察提交后的 owner/start/end，执行 `nudge("commit")`；与用户滚动、items、observer 合并同一帧。循环 owner/generation 保存在 ref，回调执行时核对。

```ts
if (!layoutReady) return "paused";
if (covered) return "settled";
if (signature === cycle.lastSignature)
  return cycle.focusedOnce ? "stalled" : "focus";
if (cycle.remaining <= 0) return "stalled";
return "advance";
```

无布局时保留目标但不安排连续帧；再次可见显式 nudge。stalled expose 中文“阅读位置暂时无法定位”，恢复按钮只触发 focus/重新测量，不调用 resume。cleanup 同时 cancelAnimationFrame 和 frameRef=null。
- [ ] **Step 4 — 验证绿与变异**：上述单测与 tsc；暂时移除 post-commit nudge，新行为测试必须红，恢复后绿。记录 cycle step 和 signature 数量，稳态不能持续安排 frame。
- [ ] **Step 5 — 提交**：全局门禁与进度证据后，`git add desktop/src docs/project-management/desktop.md && git commit -m "fix: converge transcript windows after layout commits"`。

### Task A3：用户意图与单个位置写入者

**Files:** Create `desktop/src/lib/transcriptScroll.ts`, `transcriptScroll.test.ts`, `desktop/src/hooks/useTranscriptScroll.ts`, `useTranscriptScroll.test.tsx`; Modify `desktop/src/components/ChatView.tsx`, `desktop/src/lib/viewAnchor.ts`, `viewAnchor.test.ts`, `desktop/src/App.tsx`; retain `desktop/src/lib/useStickToBottom.ts` for other consumers。

**Interfaces:**

```ts
export interface ScrollLayout { owner:string; generation:number; maxTop:number;
  scrollTop:number; anchor:ViewAnchor|null; anchorTop:number|null; }
export type PositionPlan = { kind:"none" } | { kind:"write"; top:number; following:boolean };
export class TranscriptScrollCoordinator {
  constructor(owner:string);
  isFollowing():boolean;
  acceptUserScroll(top:number, maxTop:number):void;
  releaseFollow():void;
  requestLatest():void;
  requestRestore(anchor:ViewAnchor):void;
  planLayout(layout:ScrollLayout):PositionPlan;
  acknowledgeWrite(actualTop:number,maxTop:number):void;
}
// useTranscriptScroll returns ref, scrollToBottom, followIfAtBottom, releaseFollow,
// isFollowing, requestRestore, beforeLayout, afterLayout, plus owner-aware cleanup.
```

- [ ] **Step 1 — 写失败测试**：定义 A3 上述 class 接口的单元输入，比较同一锚点在上方增高、下方增高、窗口 trim、入场恢复/贴底冲突时的结果。真实用户回底可恢复 following；程序化写到 maxTop 不能自动改变意图。

```ts
it("growth below the reading anchor cannot move the reader", () => {
  const c = new TranscriptScrollCoordinator("h/t/0/width800");
  c.releaseFollow();
  c.requestRestore({itemId:"i-200",offset:-10});
  expect(c.planLayout({owner:"h/t/0/width800",generation:1,maxTop:9000,
    scrollTop:5000,anchor:{itemId:"i-200",offset:-10},anchorTop:4990}))
    .toEqual({kind:"write",top:5000,following:false});
  c.acknowledgeWrite(5000,9000);
  expect(c.planLayout({owner:"h/t/0/width800",generation:2,maxTop:12000,
    scrollTop:5000,anchor:{itemId:"i-200",offset:-10},anchorTop:4990})).toEqual({kind:"none"});
});
```

新接口中的 `anchorTop` 是消息顶部在滚动内容坐标中的位置，等于当前 viewport-relative top 加 scrollTop；saved.offset 始终为 viewport-relative，不混用两种坐标。pending请求在可容纳写入并acknowledge后消费，随后保存为有效阅读锚点，不能每帧重复restore。

补空 spacer 无可视消息、迟到旧 owner layout、pending 锚点尚未挂载、未完整 baseline 不能宣布目标已删除四例。
- [ ] **Step 2 — 验证红**：`cd desktop && npm test -- src/lib/transcriptScroll.test.ts src/hooks/useTranscriptScroll.test.tsx src/lib/viewAnchor.test.ts`。A5 fixture 在此阶段先加入首次上翻 D05 场景并取得当前错误的红证据；若仅诊断疑点而无稳定红，不能删除旧补偿后宣称 D05 修好。
- [ ] **Step 3 — 最小实现**：统一使用 DOM viewport-relative offset。capture 仅接收相交且可锚行（加入 bottom 字段），没有相交行保留上一有效锚点，不 report null。决策优先 pending restore/focus→阅读锚点→仍 following 贴底；单次布局最多调用一次 scrollTop setter，并 acknowledge 浏览器 clamp 后的实际值。

```ts
const desired = anchorTop === null ? null : anchorTop - saved.offset;
if (desired !== null && desired >= 0 && desired <= maxTop)
  return {kind:"write",top:desired,following:false};
// Not enough history/layout yet: keep pending restore, do not mark restored.
```

用 new hook 替换 ChatView 的 useStickToBottom、prevHeight 差值 effect、pendingAnchor 写入、heldAnchor observer 的多处独立 setter；既有公开命令/搜索 focus 保留接线。所有普通布局补偿先读取最近 user intent，旧 scheduled pin 必须取消。App 传 `viewOwnerKey`（slot/host/thread/epoch）与 `baselineComplete`；未有新数据层时旧冷 replay 完成才置 baselineComplete，不能由 items 非空推导。
- [ ] **Step 4 — 验证绿**：单测、ChatView/App tests、`npm run test:browser:session-view`、`npm run test:browser:stream-live`、D05 fixture、tsc。回退整体高度差应使 D05 或下方增长锚点场景红；若不红补强测试，不修改正确期望。
- [ ] **Step 5 — 提交**：全局门禁后 `git add desktop/src desktop/scripts/browser docs/project-management/desktop.md && git commit -m "fix: coordinate transcript reading intent and anchor writes"`。

### Task A4：高度 revision、布局归属与动态观测

**Files:** Modify `desktop/src/lib/heightBook.ts`, `heightBook.test.ts`, `desktop/src/hooks/useTranscriptScroll.ts`, `desktop/src/components/ChatView.tsx`, `desktop/src/App.tsx`; Create `desktop/src/lib/transcriptPositionIndex.ts`, `transcriptPositionIndex.test.ts`。

**Interfaces:**

```ts
// Add to HeightBook; existing estimate/median methods stay available.
record(id:string,height:number):boolean; // false for unchanged/nonpositive/nonfinite
revision():number;
export interface TranscriptPositionIndex { rebuild(ids:readonly string[],book:HeightBook):void;
  indexAt(offsetPx:number):number; totalHeight():number; }
export function createTranscriptPositionIndex():TranscriptPositionIndex;
```

- [x] **Step 1 — 写失败测试**：相同高度不 bump，非有限/0 不入账；布局 owner 改变重新建 book；index 在高度估计边界返回正确 index，空资料不 focus。挂载行非 tail 增长触发一次 heightRevision 及 spacer 更新。

```ts
it("height changes have an observable revision", () => {
 const b=createHeightBook();
 expect(b.record("a",100)).toBe(true); const r=b.revision();
 expect(b.record("a",100)).toBe(false);
 expect(b.revision()).toBe(r);
 expect(b.record("a",0)).toBe(false);
 expect(b.record("a",140)).toBe(true);
 expect(b.revision()).toBe(r+1);
});
```

- [x] **Step 2 — 验证红**：`cd desktop && npm test -- src/lib/heightBook.test.ts src/lib/transcriptPositionIndex.test.ts src/hooks/useTranscriptScroll.test.tsx`，真实 D08 resize/image fixture 不得只 mock scrollHeight。
- [x] **Step 3 — 最小实现**：新 book memo 的 key 包含 slot/host/thread/epoch/有效宽度/字体等排版设置。ResizeObserver 只 observe 挂载消息和 viewport，window commit 更换观察集合，cleanup disconnect；每帧合并尺寸变化，positive measure 入账。每个 layout commit 更新上一次几何，即使 win.start/end 没变。

```ts
if (!Number.isFinite(height) || height <= 0 || heights.get(id) === height) return false;
heights.set(id,height); sortedCache=null; revision += 1; return true;
// Position index prefix sums rebuild only on structure/height revision changes.
// indexAt uses binary search on cumulative positive heights; not a full scan per scroll frame.
```

observer 回调只安排协调器布局，不直接再写位置。窗口 stalled 用 indexAt(scrollTop) 发一次 focus；不可布局期间停止观测循环，visibilitychange/viewport resize 恢复。历史数缩小时只在可信基线证明目标不存在时释放 pending anchor。
- [ ] **Step 4 — 验证绿与变异**：上述单测、D04/D05/D07/D08 browser cases、三个旧 browser scripts、tsc；分别回退 owner key、record revision、每次布局维护几何，相关测试必须红，恢复后绿。**已完成**：单测、三个旧 browser scripts（`session-completeness` D01/D02/D05/D09 4/4、`transcript-window` 4/4、`stream-live` 2/2）、tsc、以及三处变异全部取得红证据（owner key→`39200 to be 8400`、record revision→5 例、每次布局记账→2 例）。**未完成**：D04/D07/D08 用例仍随 A5 Step 1–2 补齐。
- [x] **Step 5 — 提交**：全局门禁后 `git add desktop/src docs/project-management/desktop.md && git commit -m "fix: scope transcript heights and observe mounted rows"`。

### Task A5：D 组组合回归、报告与 WKWebView 交接

**Files:** Create `desktop/scripts/browser/session-completeness.html`, `session-completeness.tsx`, `session-completeness-check.mjs`, `docs/testing/session-completeness.md`; Modify `desktop/package.json`, `desktop/scripts/browser/README.md`, `docs/project-management/desktop.md`, `docs/project-management/README.md`, `docs/bug-list.md`。

**Interfaces:** fixture 独立导出以下浏览器 API，CDP 只调用此 API 和 DOM，不注入修复：

```ts
interface DisplayProbe {
 select(id:"A"|"B"):void;
 setCount(n:number):void;
 replayChunk(n:number):void;
 grow(id:string,lines:number):void;
 focus(index:number):void;
 setWidth(px:number):void;
 snapshot():{count:number;mounted:number;visibleIds:string[];top:number;
   following:boolean;pendingFrames:number;displayError:string|null};
}
// window.displayProbe: DisplayProbe; fixture uses real ChatView + index.css.
```

- [x] **Step 1 — 写失败断言**：fixture 支持600/3000条、每条8行确定性文本、820px viewport、A/B切换、分帧回放与图片/代码块增高；关闭 `overflow-anchor`。按下面步骤逐例测试，不用追加 scroll 去“催一下”。

| IDs | 精确操作与断言 |
| --- | --- |
| D01 | A→B→A，scrollTop=60802 一次，等≤`ceil(600/200)+2`个有效开窗；visibleIds非空，count600，固定几何时top变化≤2px |
| D02–D03 | count3000，拖顶/中/底，再 focus100后向下跨历史窗口底；各次有可视行；方向反转后新窗口含目标且区间不振荡 |
| D04 | A/B等长及B短8条、同名thread不同viewOwner、clear、新width、StrictMode；旧帧不能写新top |
| D05 | fresh访问与空→60→200→600回放，median从40改成真实高度，真实wheel上翻；下一帧不能回底 |
| D06 | following下tail与非tail增长均贴底；release后同样增长锚点偏移≤2px；主动发送模拟命令恢复跟随 |
| D07 | 保存中段item的长负offset；冷回放目标晚到，pending期间不标restored；目标到达后偏移≤2px |
| D08 | 历史远离tail，图片迟到/折叠/resize/hidden→visible；mounted≤400，无横向溢出，不可见时pendingFrames归零 |
| D09 | 静止500ms，frame计数不增长；固定无进展测量最多一次focus后displayError非空，点击重定位能开启新cycle |

CDP 示例断言：

```js
await evaluate("displayProbe.select('A'); displayProbe.select('B'); displayProbe.select('A')");
await evaluate("document.querySelector('[data-transcript-scroller]').scrollTop=60802");
await waitUntil(async () => (await evaluate("displayProbe.snapshot()")).visibleIds.length>0, 5000);
const s=await evaluate("displayProbe.snapshot()");
assert.equal(s.count,600); assert.ok(s.mounted<=400);
// Geometry-dependent comparisons use fixture's frozen measurements; dynamic-height cases use anchor offset.
```

将 `waitUntil`、`evaluate` 和子进程清理写入本脚本（基于已读 `transcript-window-check.mjs` 的CDP协议），不能依赖另一个 fixture 的全局 probe。新增 npm script `test:browser:session-completeness`。固定端口1452/CDP9362，可由 `SESSION_COMPLETENESS_PORT`/`SESSION_COMPLETENESS_DEBUG_PORT`覆盖。
- [x] **Step 2 — 证明判别性**：在临时回退代码中执行新脚本，D01/D05必须红；对A3补偿根因按单点变异取得证据。若当前主线某场景已通过，把它记为兼容场景，不虚报新增修复。
- [x] **Step 3 — 完成脚本与报告**：缺Chrome退出非零；Runtime.exceptionThrown使失败；finally TERM→有界KILL所有自己spawn的进程并删临时profile。报告写 `.artifacts/session-completeness/display.json`，字段为ID/pass/geometry/steps/commit，不含用户正文。同步README命令与macOS人工表，保持全部未测真机行 `[ ]`。
- [ ] **Step 4 — 全部绿**：`cd desktop && npm test && npx tsc --noEmit && npm run build && npm run test:browser:session-completeness && npm run test:browser:session-view && npm run test:browser:transcript-window && npm run test:browser:stream-live`，随后Rust全局门禁。将D组逐行证据写入测试文档，不用“脚本退出0”替代单行结果。**已完成部分（2026-11-08）**：`npx vitest run` 191 文件 / 2783 passed（连跑两次稳定）、`npx tsc --noEmit`、`npm run build`、`session-completeness` 9/9、`transcript-window` 4/4、`stream-live` 2/2，逐行证据写入 [docs/testing/session-completeness.md](../../testing/session-completeness.md)。**未完成**：`test:browser:session-view` 的 `user scroll takes control from drift correction`（`expected 2000, got 2240`）在 A3/A4 基线与当前工作区同样失败（已逐一 checkout 验证），属既有基线问题，故该项仍不勾。
- [ ] **Step 5 — 独立WKWebView验收与提交**：用户授权测试安装后才打独立app，设独立bundle id及绝对 `YI_AGENT_HOME`。执行D01/D05/D06/D07，分别切session与切app/window；记录锚点偏移/截图/commit。未有真机结果不得勾选显示feature。提交 `test: cover session display convergence in real browsers`，不自动合main或替换正式app。

## 自审覆盖与收尾

- D01 A1/A2/A5；D02/D03 A1/A2/A5；D04 A2/A3/A4/A5；D05 A3/A4/A5；D06 A3/A4/A5；D07 A3/A4/A5；D08 A1/A4/A5；D09 A2/A5。
- Spec §4.1 A1，§4.2 A2，§4.3 A1/A3，§4.4 A3/A4；没有拿数据数量变化来替代窗口修复。
- A5结束只代表本显示单元验收；全部Session修复仍需数据Plan的S/R矩阵和组合真机门禁。实施后的正式合并由用户决定，合并后重新运行前端门禁与 `just ci`。
