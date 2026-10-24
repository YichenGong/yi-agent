# 会话页位置与选中会话的恢复 — 设计

日期：2026-10-25
范围：桌面端 / 手机端共用的 React 客户端（`desktop/src`）。

## 1. 问题

用户报两条：

1. **A → B → A 后不再停在 A 原来的阅读位置。**
2. **切到别的 app 再切回来，看到的不是离开时那个 session。**

两条都不是偶发缺陷，而是代码里两个显式选择。

### 1.1 阅读位置丢失

`desktop/src/components/ChatView.tsx:446` 算出 `threadChanged`，`:463` 据此无条件
`scrollToBottom()`：

```ts
if (firstRun || threadChanged || sentByUser || newError) scrollToBottom();
```

而 `desktop/src/lib/useStickToBottom.ts:66` 的 `scrollToBottom()` 直接把
`scrollTop` 置为 `scrollHeight`。同时**没有任何按会话保存位置的机制**：hook 里的
`atBottomRef` 是单例，`ChatView` 也没有按 `threadId` 分键。

实测（用现有测试手法给真实滚动节点装度量，模拟用户停在 1200px）：

```
PROBE after switch to B   scrollTop = 3000
PROBE after back to A     scrollTop = 3000   // 期望 1200
```

该行为被测试**固化为预期**：`desktop/src/components/ChatView.test.tsx:673`
`jumps to the newest content when the thread changes` 断言「切会话是明确意图：
要看到这个会话的最新内容，而不是沿用上一个会话的阅读位置」。

### 1.2 切回前台丢失选中会话

`desktop/src/App.tsx:1364` 的 `onVisibilityChange` 在**两个平台**上都无条件
`reconnectNow()`（重建客户端 + 重放整趟 handshake）。握手里选会话的那一步
（`App.tsx:1719`）：

```ts
const keptId = isRemoteClient() ? sess.currentIdRef.current : null;
```

`null` 就落到 `list.pinned[0] ?? groups.flatMap((g) => g.threads)[0]`
（`App.tsx:1726`）——**桌面端被显式重置为列表第一条**。注释也写着「桌面端保留原行为
（每次选第一条）」，来自提交 `1884aed4`（2026-10-11，只给手机端加了保持）。

该行为同样被测试固化：`desktop/src/App.test.tsx:5503`
`still selects the first thread on the desktop reconnect (unchanged)`。

### 1.3 冷启动

- `selectedId` 只在 `thread/listAll` 成功时写进缓存
  （`App.tsx:1675`、`hooks/useSessionState.ts:117`），`selectThread`
  （`App.tsx:668`）切换会话后**从不写缓存**——缓存里的 `selectedId` 是「上次列表
  刷新那一刻」，不是「最后看的」。
- 桌面端根本不读这份缓存：`App.tsx:208`
  `const initialListCache = useRef(isRemoteClient() ? readListCache() : null).current;`
  故冷启动一律落到列表第一条（`docs/bug-list.md:66` 已登记同类问题）。

### 1.4 附带缺陷：同一会话被重复选中也会清空附属视图

`App.tsx:696-703` 在 `selectThread` 里**无条件**清空右侧面板、diff 与进程详情：

```ts
sess.setPanelOpen(false);
diff.resetThreadDiff();
...
proc.setOpenProcessId(null);
proc.setProcessOutput(null);
```

这些重置的用意是「别把 A 的数据画在 B 上」。但切回前台的重连走的是**同一条会话**，
此时不存在错配风险，重置纯属多余；点侧栏里已经选中的那一条同样会被清空。

## 2. 目标与非目标

### 目标

1. A → B → A 回到 A 原来的位置（锚到当时视口顶部的那条消息）。
2. 离开时贴底 ⇒ 回来继续贴底并跟随新内容。
3. 切到后台再切回来（含重连），停留在离开时那个 session；两个平台一致。
4. 冷启动回到上次选中的 session（若它仍存在于服务端列表）。
5. 同一会话被重复选中时，不再清空右侧面板 / diff / 进程详情。

### 非目标

- 不做真正的逐 item 像素级锚定（仅顶部一条消息 + 偏移）。
- 不改其他页面（看板、子 agent 详情、设置页）的位置记忆——另立需求。
- 不改变「新会话 / 主动点侧栏切到**别的**会话 = 看到最新内容」这一既有语义。
- 不持久化滚动位置到磁盘：位置只在进程内存里，冷启动只恢复「哪条会话」。

## 3. 设计

### 3.1 位置模型：锚到视口顶部的消息

```ts
/** 一条会话的阅读位置。`null` 表示「离开时贴底」。 */
export interface ViewAnchor {
  /** 视口顶部那条消息的协议 id；`local-*` 不入锚（重开后会重建）。 */
  itemId: string;
  /** 该消息顶边相对视口顶部的偏移（负数 = 已滚过一部分）。 */
  offset: number;
}
```

- **贴底记为 `null`**，而不是记最后一条消息 + 偏移：贴底是个语义状态（「我要看最新」），
  用一条消息表达不了「之后又来了 20 条」这件事。
- 锚点只认**服务端 id**。`local-*` 是本地乐观回声/本地 notice，冷启与重放后会重建，
  用它做锚会在第二次进入时找不到目标。

### 3.2 采集：离开时记，不是滚动时记

在 `ChatView` 的卸载/切换前采集，而不是每次 `scroll` 都写一份：

- `scroll` 事件只做已有的事（维护 `atBottomRef`），不新增写入。
- 切换 `threadId` 时（`ChatView` 的 effect 里 `prevThreadRef` 变化那一刻）与卸载时
  （`useEffect` cleanup），把当前会话的位置交给宿主：贴底则 `null`，否则找出
  「顶边 ≤ 视口顶部」的最后一条消息。

查找方式：给每条消息的容器加 `data-item-id`，采集时用
`scroller.querySelectorAll("[data-item-id]")` 做一次线性扫描（只在切换/卸载时发生，
不在滚动热路径）。找不到任何可见消息（例如 transcript 为空）时记 `null`。

`data-item-id` 挂在哪一层要写死，否则恢复时的坐标会有系统性偏差：挂**消息自身的
根节点**（`ChatItem` 返回的那个 div），不是包一层的 fragment/wrapper。采集与恢复用
同一套坐标——`el.offsetTop`（相对滚动容器）与 `scroller.scrollTop`——所以挂在同一层
即可自洽，不引入额外的 padding 补偿。

滚动容器本身是 `ChatView` 的根 div（`ChatView.tsx:468`，`overflow-y-auto`），
`offsetTop` 需要相对它计算；若中间存在定位祖先，改用
`el.getBoundingClientRect().top - scroller.getBoundingClientRect().top + scroller.scrollTop`，
两条路径取其一并固定在实现里。

> 为什么不给每条消息加 ref 映射：`HistoryPrefix` 是刻意 memo 冻结的子树
> （`ChatView.tsx:314`），加 ref 回调会破坏它的「纯文本追加不重渲染」不变量。
> 属性扫描发生在离开时，代价可忽略。

### 3.3 恢复：进入时对位

`ChatView` 在 `threadId` 变化且拿到了该会话的锚时：

1. 等 transcript 有机会落定（`useLayoutEffect`，在 DOM 提交后、绘制前）。
2. 找到 `[data-item-id="<id>"]`，把 `scroller.scrollTop` 设为
   `element.offsetTop - offset`（相对滚动容器的偏移）。
3. 置 `atBottomRef.current = false`（用户回来是要读那一段，不该被新 delta 拽走）。

**事件已消失时**（消息被删、历史被截断、锚 id 不在 DOM 里）**回退到底部**——宁可
看到最新的，也不要停在无意义的像素位置上。这条回退也覆盖「恢复时内容还没加载完」：
恢复只尝试一次，不等异步补齐；补齐到达后用户若已在阅读位置，`scroll` 事件会把意图
重新写回 `atBottomRef`，流式增长不会把他拽走。

**贴底（锚为 `null`）**：走既有的 `scrollToBottom()`，语义不变。

### 3.3.1 与既有「切会话到底」的关系（含旧测试的反转）

现有语义有三条入口会强制到底：`firstRun`（首次挂载）、`threadChanged`（切会话）、
`sentByUser` / `newError`。本次只改 `threadChanged` 这一条：

| 场景 | 改前 | 改后 |
| --- | --- | --- |
| 首次进入某会话（无记录） | 到底 | 到底（不变） |
| A → B → A，A 有记录且非贴底 | 到底 | 恢复锚 |
| A → B → A，A 有记录且贴底 | 到底 | 到底（不变） |
| A → B → A，A 有记录但锚已消失 | 到底 | 到底（回退） |
| 用户按发送 / 出现新错误 | 到底 | 到底（不变） |

`firstRun` 保持「到底」是有意的：它现在只在**首次挂载**时命中（`ChatView` 不随会话
重挂载），与「首次进入无记录的会话」正是同一件事。

### 3.4 位置的归属：存在 `useSessionState`

`Record<string, ViewAnchor | null>` 放在 `hooks/useSessionState.ts`——它与
`currentId` 同属会话关注点，且要跨 `ChatView` 的挂载存活。接口：

```ts
rememberView: (threadId: string, anchor: ViewAnchor | null) => void;
anchorFor: (threadId: string) => ViewAnchor | null | undefined;
```

`undefined`（从没记过）与 `null`（记过且当时贴底）必须区分：前者走「首次进入」的
默认贴底，后者同样贴底但语义上明确。

内存里只在会话被删除时清理（`App.tsx:1215` 的 `deleteThread` 里与 `warm.current.delete(id)`
同处一行），不做容量上限：一个会话一条记录，随手会数量增长，与 `store` 同量级。

### 3.5 切回前台：桌面端也保持当前会话

`App.tsx:1719` 改为两个平台一致：

```ts
const keptId = sess.currentIdRef.current;
```

握手里的 `kept` 判据（当前会话仍在服务端列表里）保持不变；不在列表里才回落第一条。
**保留重连**：sidecar 崩溃时的自愈能力不变（用户已确认这一取舍）。

### 3.6 同一会话重复选中：不清附属视图

`App.tsx` 的 `selectThread` 里，只有 `id !== prevId` 时才做
`setPanelOpen(false)` / `resetThreadDiff()` / 清进程详情。同一会话时：

- 仍要走 resume / readItems 等补齐路径（重连后必须重新对上账）。
- 不动面板、diff、进程详情——它们本来就属于这条会话。

这同时修掉「点侧栏里已选中的会话会清空右侧面板」这一既有小毛病。

### 3.7 冷启动：新增 `selectedThread` 键

新增 `desktop/src/lib/selectedThread.ts`：

```ts
export const SELECTED_THREAD_KEY = "yi-agent.selectedThread";
export function readSelectedThread(storage?): string | null;
export function writeSelectedThread(storage, id: string | null): void;
```

- 平台无关（桌面端与手机端都写、都读），形如 `lib/theme.ts` 的「存储是缓存」契约：
  解析失败一律 `null`，绝不抛。
- 写入点：`selectThread` 与 `newThread` 成功后、`deleteThread` 当前会话时写 `null`。
- 读取点：`App.tsx` 的启动 effect（与现有 `initialListCache` 预填同一个 effect，
  `App.tsx:619`）。读取顺序：
  1. `selectedThread` 命中且该 id 在 `listAll` 的列表里 ⇒ 选中它；
  2. 否则退回现有行为（手机端 `initialListCache.selectedId`、桌面端列表第一条）。
  校验放在**握手之后**（列表是服务端权威），冷启动首帧仍走现有的缓存预填路径。

不把 `selectedId` 写进 `threadListCache`：那份缓存的形状被测试与解析器逐字段钉住
（`lib/sessionCache.test.ts`），塞一个「随时会变」的字段进去等于把两种语义混在
一个键里。

## 4. 测试

| 行为 | 用例 | 位置 |
| --- | --- | --- |
| A→B→A 锚定恢复 | `restores the reading position when returning to a thread` | `components/ChatView.test.tsx` |
| 贴底回来仍贴底并跟随 | `keeps following new content when the thread was left at the bottom` | 同上 |
| 锚点消失回退到底部 | `falls back to the bottom when the anchor item is gone` | 同上 |
| 离开时记录（含 `local-*` 不入选） | `remembers the top visible item when switching away` | 同上 |
| 位置存储的归属语义 | `distinguishes never-recorded from recorded-at-bottom` | `hooks/useSessionState.test.ts`（新建）或 App 层 |
| 桌面端也保持会话 | `keeps the current thread across a foreground reconnect (desktop too)` | `App.test.tsx` |
| **改**桌面端旧断言 | 删除/反转 `still selects the first thread on the desktop reconnect (unchanged)` | `App.test.tsx:5503` |
| **改**切会话跳到底部的旧断言 | `jumps to the newest content when the thread changes` 拆成「首次进入到底」与「回访恢复锚」 | `ChatView.test.tsx:673` |
| 同一会话不清面板 | `keeps the detail panel when the same thread is selected again` | `App.test.tsx` |
| 冷启动恢复 | `restores the last selected thread on cold start` / `falls back when it is gone` | `App.test.tsx` |
| `selectedThread` 解析 | 合法/缺失/坏 JSON/非字符串 | `lib/selectedThread.test.ts` |

验证命令：

```
cd desktop && npx vitest run src/components/ChatView.test.tsx src/lib/useStickToBottom.test.ts src/lib/selectedThread.test.ts src/App.test.tsx
cd desktop && npx tsc --noEmit && npm run build
```

两条被反转的旧断言必须在改动说明里点明：它们钉住的正是用户报的行为，反转是本次需求
本身，不是「顺手改测试」。

## 5. 风险

1. **恢复时机早于布局稳定。** 用 `useLayoutEffect` 并在找不到元素时回退到底部；
   恢复只尝试一次，不引入轮询。
2. **`HistoryPrefix` 的 memo 不变量。** 位置采集用属性扫描，不改动 memo 依赖；
   恢复是命令式 `scrollTop` 赋值，不引起重渲染。
3. **切回前台仍会重连**（已确认保留）。重连期间右侧面板的 diff 快照由既有逻辑重新
   读取；因 3.6 的改动，面板本身不再被收起。
4. **冷启动首帧闪烁。** 校验在握手后，首帧沿用现有缓存预填；若最终选中的会话与首帧
   不同，会出现一次会话切换——这与现在桌面端「先第一条再切」的行为相比不更差。
