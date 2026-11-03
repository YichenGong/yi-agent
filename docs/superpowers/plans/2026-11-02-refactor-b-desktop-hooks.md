# B 块实施计划：桌面接线拆分（`desktop/src/App.tsx`）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 `App.tsx`（3509 行）里那个 632 行的巨型 `useEffect`（L1635–L2266）按四个关注点拆成独立 hook，让 `App.tsx` 降到 2950 行以内，且不改变任何运行时行为。

**Architecture:** 沿用仓库里**已经存在**的模式——`hooks/` 下的 5 个领域 hook（`useSessionState` / `useSettingsState` / `useBoardState` / `useDiffState` / `useProcessState`）都是「状态归自己、跨关注点的写入由调用方注入」。本块按同一套写法处理连接生命周期、前台唤醒、通知分派、握手恢复。

**Tech Stack:** React 18 + TypeScript，vitest，`npx vitest run`。

## Global Constraints

- **只搬不改**：禁止改逻辑、改文案、改注释语义、改变量名、改任何 `console` 输出。
- **行为零变化**：这是纯结构重构。任何「顺手修一下」都是越界。
- **现有 146 个测试文件 / 1932 个用例必须全绿**，断言一字不改。
- 本块**不碰** `setThreadMode`（654 行）和 `selectThread`（320 行）——它们是跨关注点编排，抽出去只会制造隐式依赖（这是 spec 的明确决策）。
- 每个 Task 结束必须满足：
  - `npx vitest run` → **146 文件 / 1932 用例全绿**
  - `npx tsc --noEmit` → 无错误（`npm run build` 的第一步就是 `tsc`）
  - `npm run lint` 若存在则跑（先 `python3 -c "import json;print(json.load(open('package.json')).get('scripts'))"` 确认）
- 提交信息用 conventional commits，首行 ≤72 字符，**不写** `Co-Authored-By`。
- 环境：
  ```bash
  cd desktop
  export PATH="/opt/homebrew/bin:$PATH"
  ```

## 实测结构（行号断言的唯一来源）

| 结构 | 行范围 | 行数 |
|---|---|---|
| `App.tsx` 总计 | L1–L3509 | **3509** |
| `useState` | — | **12** |
| `useRef` | — | **29** |
| `useCallback` | — | **12** |
| 小 `useEffect`（7 个） | L385, L714, L724, L737, L754, L773, L796 | 各 10–30 行 |
| **巨型 `useEffect`** | **L1635–L2266** | **632** |
| └ effect 依赖数组 | `}, [prefs.pairingVersion]);` | 仅 1 个依赖 |
| JSX `return` | L2917 起 | — |

### 巨型 effect 内部的 11 个闭包（实测行号）

| 闭包 | 行 | 行的职责 |
|---|---|---|
| `disposed` / `restartTimer` / `attempt` | L1642–1646 | 可取消标志 + 重连定时器 + 重试计数 |
| `reconnectSlot(slot)` | L1658 | 单个槽位重连 |
| `reconnectNow()` | L1673 | 清在途定时器后立刻重连当前槽位 |
| `scheduleReconnect()` | L1693 | 指数退避排期 |
| `onVisibilityChange()` | L1708 | **前台唤醒**：iOS 回前台时 socket 已被系统掐断 |
| `wireClient(client, slot)` | **L1723–L1943** | **通知/审批回调分派（221 行，最大的一块）** |
| `handshake(client)` | **L1944–L2181** | **握手后恢复编排（238 行）** |
| `connectSlot(slot)` | L2182 | 取/建客户端 |
| `connectRemoteTargets(overLocal)` | L2204 | 远端目标连接 |
| `syncActiveClient()` | L2240 | 把活动槽位的客户端同步到 ref |
| `connect()` | L2245 | 顶层入口 |
| `boardTimer` | L2255 | 看板轮询定时器 |

### `wireClient` 内的 12 个通知分支（实测，含 3 个 `turn/completed`）

```
L1742  thread/status/updated（带额外条件）
L1754  agent/children/updated
L1759  ui/settings/updated
L1768  model/capabilitiesChanged
L1781  ui/gitDiff/focus
L1807  agent/trace/event
L1817  process/updated
L1822  thread/titleChanged
L1837  thread/tagChanged
L1850  thread/groupsChanged
L1856  ui/tagNames/updated
L1872  turn/completed（块 1）
L1891  turn/completed（块 2，调 sess.refreshThreads）
```

### effect 内引用的外部 setter（提取时必须显式注入）

实测计数（出现次数）：

```
setStatus(6)  setGroups(4)  setPinned(3)  setTheme(2)  setTagNames(2)
setOnboarding(2)  setLeadAgentEnabled(2)  setBuildKind(2)  setWatchmanEnabled(1)
setTraceRows(1)  setRepairing(1)  setRepairNotice(1)  setRelayUrl(1)
setPendingDiffNote(1)  setPanelOpen(1)  setKanbanEnabled(1)  setDetailTab(1)
setCurrentError(1)  setApproval(1)  setActive(1)
```

**这 20 个 setter 是提取的依赖清单。** 抽 hook 时必须逐个列出并注入，不能靠闭包捕获——闭包捕获会让 hook 的依赖无法被 React 检查，也会让测试无法单独驱动。

---

## 文件结构（目标）

```
desktop/src/hooks/
├── useConnectionLifecycle.ts   # Task 2：reconnectSlot / reconnectNow / scheduleReconnect / connect / connectSlot / syncActiveClient / connectRemoteTargets / boardTimer
├── useForegroundWake.ts        # Task 3：onVisibilityChange
├── useNotificationRouter.ts    # Task 4：wireClient 的 12 个分支
└── useHandshakeRecovery.ts     # Task 5：handshake
```

---

## Task 1: 建立前端基线

**Files:**
- 不修改源文件

- [ ] **Step 1: 记录基线**

```bash
cd desktop
export PATH="/opt/homebrew/bin:$PATH"
npx vitest run 2>&1 | tail -8
```

记下 `Test Files  N passed` 与 `Tests  N passed`（本计划写作时为 **146 文件 / 1932 用例**）。若不符，以实测为准并更新本文档。

- [ ] **Step 2: 建立测试快照**

```bash
cd desktop
npx vitest run --reporter=json --outputFile=/tmp/b-baseline.json 2>&1 | tail -3
python3 - <<'PY'
import json
d = json.load(open('/tmp/b-baseline.json'))
names = sorted(t['fullName'] for r in d['testResults'] for t in r['assertionResults'])
open('/tmp/b-testnames.txt','w').write('\n'.join(names))
print("用例数", len(names))
PY
```

- [ ] **Step 3: 记录类型检查基线**

```bash
cd desktop
npx tsc --noEmit 2>&1 | head -20
echo "exit=$?"
```

记录当前是否干净。**若基线就有错，先记下错误清单**，否则改动后无法区分是自己引入的还是原本就有的。

- [ ] **Step 4: 建校验脚本**

写 `/tmp/b-check.sh`：

```bash
#!/bin/bash
set -e
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/desktop
export PATH="/opt/homebrew/bin:$PATH"

echo "== 用例名集合 =="
npx vitest run --reporter=json --outputFile=/tmp/b-now.json > /dev/null 2>&1
python3 - <<'PY'
import json
d = json.load(open('/tmp/b-now.json'))
names = sorted(t['fullName'] for r in d['testResults'] for t in r['assertionResults'])
open('/tmp/b-testnames-now.txt','w').write('\n'.join(names))
base = open('/tmp/b-testnames.txt').read().strip().split('\n')
now = names
if base == now:
    print(f"PASS: {len(now)} 个用例名一致")
else:
    only_base = set(base) - set(now)
    only_now = set(now) - set(base)
    print("FAIL")
    if only_base: print("消失:", sorted(only_base)[:10])
    if only_now:  print("新增:", sorted(only_now)[:10])
    raise SystemExit(1)
PY

echo "== 类型检查 =="
npx tsc --noEmit 2>&1 | head -20
```

```bash
chmod +x /tmp/b-check.sh
```

**本 Task 不产生 commit。**

---

## Task 2: 抽出 `useConnectionLifecycle`

**风险最低的一块**：连接生命周期是纯命令式逻辑，不碰任何 UI 状态，除了 `setStatus`（6 处）。

**Files:**
- Create: `desktop/src/hooks/useConnectionLifecycle.ts`
- Modify: `desktop/src/App.tsx`（L1635–L2266 内的相关闭包）

**Interfaces:**

```typescript
export interface ConnectionLifecycleOptions {
  /** 活动槽位变化时用来驱动 UI 的连接状态文字。 */
  setStatus: (v: string) => void;
  /** 当前活动槽位 id 的读取器（ref，避免把 slot 变化塞进 effect 依赖）。 */
  activeSlotRef: React.MutableRefObject<Slot>;
  /** 槽位注册表（App 持有，本 hook 只读遍历）。 */
  registryRef: React.MutableRefObject<Map<Slot, RpcClient>>;
  /** 建连所需的参数来源。 */
  prefs: Prefs;
  disposedRef: React.MutableRefObject<boolean>;
}

export interface ConnectionLifecycle {
  connect: () => RpcClient;
  connectSlot: (slot?: Slot) => RpcClient;
  reconnectNow: () => void;
  reconnectSlot: (slot: Slot) => void;
  scheduleReconnect: () => void;
  syncActiveClient: () => void;
  connectRemoteTargets: (overLocal: RpcClient) => Promise<void>;
  /** 由 useHandshakeRecovery 注入：拿到新客户端后要做的事。 */
  setHandshake: (fn: (client: RpcClient) => Promise<void>) => void;
  /** 由 useNotificationRouter 注入：给新客户端挂回调。 */
  setWireClient: (fn: (client: RpcClient, slot: Slot) => void) => void;
}

export function useConnectionLifecycle(
  opts: ConnectionLifecycleOptions
): ConnectionLifecycle;
```

**为什么用 `setHandshake` / `setWireClient` 注入**：`connect()` 需要在新客户端建好后调 `handshake` 和 `wireClient`，但这两个函数在别的 hook 里。用 setter 注入避免循环依赖（hook A 需要 hook B 的函数、hook B 又需要 hook A 的连接）。这是 React 里处理互相依赖的标准做法，比把三个 hook 合并成一个大 hook 更好——后者会退回原样。

- [ ] **Step 1: 跑基线**

```bash
bash /tmp/b-check.sh
```

- [ ] **Step 2: 创建 hook 文件，先搬最独立的三个闭包**

`reconnectSlot`（L1658）、`reconnectNow`（L1673）、`scheduleReconnect`（L1693）三者只互相调用 + 用 `registryRef`、`disposed`、`restartTimer`、`attempt`。

```typescript
import { useCallback, useRef } from "react";
import type { RpcClient } from "../lib/rpc";
import type { Slot } from "../lib/slot";

export function useConnectionLifecycle(opts: ConnectionLifecycleOptions): ConnectionLifecycle {
  const restartTimer = useRef<number | null>(null);
  const attempt = useRef(0);
  let handshakeFn: ((c: RpcClient) => Promise<void>) | null = null;
  let wireFn: ((c: RpcClient, s: Slot) => void) | null = null;

  // 逐字搬入 L1658-1706 的三个函数体，仅把
  //   disposed        → opts.disposedRef.current
  //   restartTimer    → restartTimer.current
  //   attempt         → attempt.current
  // 其余一个字符不改。
  ...
}
```

**注意：`attempt` 在搬移后必须变成 ref**。原代码里它是 effect 内的 `let attempt = 0`，活一次 effect 生命周期。变成 hook 后如果还是局部 `let`，每次渲染都会重置，重试计数就失效了。**这是本 Task 唯一有行为风险的地方。**

- [ ] **Step 3: 搬 `connectSlot` / `syncActiveClient` / `connect` / `connectRemoteTargets` / `boardTimer`**

`boardTimer`（L2255）是 `window.setInterval`，搬进 hook 后用 `useEffect` 的 cleanup 返回清除逻辑。原代码在 effect 的 cleanup 里清（L2266 之前），语义一致。

- [ ] **Step 4: 在 `App.tsx` 里换用新 hook**

```typescript
const lifecycle = useConnectionLifecycle({
  setStatus,
  activeSlotRef,
  registryRef,
  prefs,
  disposedRef,
});
```

然后：
- 删掉 L1658–L1706 三个闭包、L2182–L2265 五个闭包的原始定义
- 所有 `connect()` 调用改成 `lifecycle.connect()`，`connectSlot(x)` 改成 `lifecycle.connectSlot(x)`，等等

**不要改调用顺序**：`boardTimer` 的启动时机必须与原代码一致（在 effect 内、连接建立之后）。

- [ ] **Step 5: 检查新 hook 里有几个 `let` 被错误地带成局部**

```bash
cd desktop
grep -n "^\s*let " src/hooks/useConnectionLifecycle.ts
```

每一个 `let` 都要问：它在原 effect 里是否跨渲染保持？若是，必须是 ref。

- [ ] **Step 6: 跑检查**

```bash
bash /tmp/b-check.sh
```

- [ ] **Step 7: 确认 App.tsx 行数下降**

```bash
cd desktop
wc -l src/App.tsx
```

- [ ] **Step 8: Commit**

```bash
git add desktop/src/App.tsx desktop/src/hooks/useConnectionLifecycle.ts
git commit -m "refactor(desktop): 把连接生命周期抽成 useConnectionLifecycle

connectSlot/connect/reconnectNow/reconnectSlot/scheduleReconnect/
syncActiveClient/connectRemoteTargets/boardTimer 八个闭包从 App 的
632 行 effect 里搬出。

attempt 与 restartTimer 从 effect 内 let 变成 ref：它们的语义是「跨
重连保持」，留在局部会在每次渲染时归零，重试计数就失效了。

handshake 与 wireClient 由 setter 注入，避免三个 hook 循环依赖。"
```

---

## Task 3: 抽出 `useForegroundWake`

**Files:**
- Create: `desktop/src/hooks/useForegroundWake.ts`
- Modify: `desktop/src/App.tsx`

**Interfaces:**

```typescript
export interface ForegroundWakeOptions {
  enabled: boolean;
  /** 重连当前槽位（延迟清零）。 */
  reconnectNow: () => void;
  /** 重连指定槽位。 */
  reconnectSlot: (slot: Slot) => void;
  /** 全部槽位（远端的被系统掐断后不会自己恢复）。 */
  registryRef: React.MutableRefObject<Map<Slot, RpcClient>>;
  resetAttempt: () => void;
}

export function useForegroundWake(opts: ForegroundWakeOptions): void;
```

**这段逻辑的语义（搬移时逐字保留注释）：**

> iOS 从后台切回前台时 socket 常已被系统静默掐断，状态不会自己回来。这里故意**不看 `connected`**——被后台掐断的 socket 可能没有 close/exited 事件，那个标志会陈旧地停在 true（半开连接），信它就会把恰恰要修的场景漏过去。只在可见性事件上触发。`reconnectNow` 先清在途定时器再替换当前客户端，因此不会并发两条连接。

以及：

> 遍历**所有**槽位：被后台掐断的远端连接不会自己恢复。只重连当前槽位会让用户切回其它项目时的前几个请求全部失败。

- [ ] **Step 1: 建 hook，搬 L1708–L1721**

里面 `attempt = 0` 要改成 `resetAttempt()`（因为 `attempt` 现在是 `useConnectionLifecycle` 的 ref）。

- [ ] **Step 2: 注册监听并返回 cleanup**

```typescript
useEffect(() => {
  if (!opts.enabled) return;
  document.addEventListener("visibilitychange", onVisibilityChange);
  return () => document.removeEventListener("visibilitychange", onVisibilityChange);
}, [opts.enabled]);
```

原代码在巨型 effect 里注册，cleanup 时一并移除。语义一致。

- [ ] **Step 3: 检查 `disposed` 判断**

原代码 `if (document.visibilityState !== "visible" || disposed) return;`。`disposed` 是 effect 的局部标志，搬出后必须用 `opts.disposedRef.current`。**漏掉这一条，组件卸载后仍会触发重连**——这是本 Task 唯一的真实风险。

- [ ] **Step 4: 跑检查**

```bash
bash /tmp/b-check.sh
```

- [ ] **Step 5: Commit**

```bash
git add desktop/src/App.tsx desktop/src/hooks/useForegroundWake.ts
git commit -m "refactor(desktop): 把前台唤醒重连抽成 useForegroundWake

iOS 从后台切回前台时 socket 已被系统静默掐断，且可能没有 close 事件，
所以这条路径故意不看 connected。搬移时保留全部说明注释。

disposed 判断改为读 ref：漏掉它会导致卸载后仍触发重连。"
```

---

## Task 4: 抽出 `useNotificationRouter`（本块最大、风险最高）

**Files:**
- Create: `desktop/src/hooks/useNotificationRouter.ts`
- Modify: `desktop/src/App.tsx`

**Interfaces:**

```typescript
export interface NotificationRouterOptions {
  /** 当前活动槽位，用来判断通知是否属于本视图。 */
  activeSlotRef: React.MutableRefObject<Slot>;
  /** 20 个外部 setter（见 Global 结构表）。 */
  setStatus: (v: string) => void;
  setGroups: (fn: (v: unknown) => unknown) => void;
  setPinned: (fn: (v: unknown) => unknown) => void;
  // ... 逐项列出
  /** 会话 store。 */
  sess: SessionState;
  /** 强制重渲染。 */
  force: (fn: (v: number) => number) => void;
  /** 子 agent 暂留区。 */
  railStore: SubagentRailStore;
}

export interface NotificationRouter {
  /** 给一个客户端挂上全部回调。 */
  wireClient: (client: RpcClient, slot?: Slot) => void;
}

export function useNotificationRouter(
  opts: NotificationRouterOptions
): NotificationRouter;
```

- [ ] **Step 1: 先搬那些只写一个 setter 的简单分支**

按风险从低到高，先搬这 8 个（各只碰 1 个目标）：

```
L1754  agent/children/updated
L1817  process/updated
L1822  thread/titleChanged
L1837  thread/tagChanged
L1850  thread/groupsChanged
L1856  ui/tagNames/updated
L1768  model/capabilitiesChanged
L1781  ui/gitDiff/focus
```

每搬 2 个跑一次 `bash /tmp/b-check.sh`。不要求全搬完才测。

- [ ] **Step 2: 再搬带前置过滤的逻辑（L1742 / L1807 / L1759）**

`L1742 thread/status/updated` 带额外条件（`&&` 后面还有判断），`L1807 agent/trace/event` 涉及 `traceRows`。这两个依赖的上下文较多，单独搬。

- [ ] **Step 3: 最后搬三处 `turn/completed`（L1871 / L1872 / L1891）**

**这三处必须一起搬**，中间夹着别的逻辑：

```typescript
// L1871: 与前面的 changed 判断共用
if (changed || n.method === "turn/completed") force((v) => v + 1);
// L1872: 块 1
if (n.method === "turn/completed") { ... }
// L1891: 块 2
if (n.method === "turn/completed") void sess.refreshThreads();
```

**拆开它们会改变执行顺序。** 搬进 hook 后保持原有顺序，`force` 那一行不能挪到两个 `if` 之后。

- [ ] **Step 4: 逐字保留「非活动槽位的通知不写本视图」的守卫**

原代码（L1726 附近）：

```typescript
// 非活动槽位（别的远端项目、或本地而用户正看远端）的通知**不写**本视图。
//
// spec §4.6 判据 17：A 项目跑一个多步任务时，B 项目的会话视图里不得出现 A 的
// `turn/*`。所有槽位共用同一个 ThreadStore，而 thread id 在各 app-server 之间
// 没有任何全局唯一性保证——两个项目里都叫 t-1 完全可能。放进来不是「多一行」，
// 而是把两条对话的内容拼进同一份视图。
```

**这段注释和它守护的那个 early return 必须原样保留。** 这是整个 B 块最容易在搬移中被「顺手优化掉」的东西。

- [ ] **Step 5: 跑检查**

```bash
bash /tmp/b-check.sh
```

- [ ] **Step 6: Commit**

```bash
git add desktop/src/App.tsx desktop/src/hooks/useNotificationRouter.ts
git commit -m "refactor(desktop): 把通知分派抽成 useNotificationRouter

wireClient 的 12 个通知分支（221 行）搬进独立 hook：agents/ui/model/
gitDiff/trace/process/thread 七类通知与三处 turn/completed。

三处 turn/completed 保持原顺序搬移（中间那行 force 不能挪后）；
非活动槽位不写本视图的守卫连同其说明注释逐字保留。
20 个外部 setter 全部显式注入，不靠闭包捕获。"
```

---

## Task 5: 抽出 `useHandshakeRecovery`

**Files:**
- Create: `desktop/src/hooks/useHandshakeRecovery.ts`
- Modify: `desktop/src/App.tsx`

**Interfaces:**

```typescript
export interface HandshakeRecoveryOptions {
  sess: SessionState;
  registryRef: React.MutableRefObject<Map<Slot, RpcClient>>;
  activeSlotRef: React.MutableRefObject<Slot>;
  setStatus: (v: string) => void;
  // 握手后恢复编排涉及的所有 setter 与 store
}

export interface HandshakeRecovery {
  /** 握手 + 握手后的全部恢复动作。 */
  handshake: (client: RpcClient) => Promise<void>;
}

export function useHandshakeRecovery(
  opts: HandshakeRecoveryOptions
): HandshakeRecovery;
```

**`handshake` 内的四件事（L1944–L2181，238 行）：**

1. 重放会话（resume）
2. 补订阅
3. 清 warm 缓存
4. 拉 boards + 重新握手

- [ ] **Step 1: 先测绘 handshake 内部结构**

搬 238 行之前必须先看清里面有哪些顺序依赖：

```bash
cd desktop
awk 'NR>=1944 && NR<=2181 {if ($0 ~ /^      (\/\/|await|if|const|void|sess\.)/) print NR": "$0}' src/App.tsx | head -60
```

把输出的顺序写下来。**这四件事的先后顺序不能变**——重放必须在订阅之前（否则漏事件），清缓存必须在拉 boards 之后（否则立刻又被填满）。

- [ ] **Step 2: 整体搬移，不做拆分**

**这一步只搬不拆。** handshake 内部那四件事**不要**在这个 Task 里再分出子函数——它们共享大量局部变量（客户端、会话 id、板列表），强行拆会立刻需要传十几个参数，比现在更差。拆分留给后续单独评估。

- [ ] **Step 3: 跑检查**

```bash
bash /tmp/b-check.sh
```

- [ ] **Step 4: Commit**

```bash
git add desktop/src/App.tsx desktop/src/hooks/useHandshakeRecovery.ts
git commit -m "refactor(desktop): 把握手后恢复编排抽成 useHandshakeRecovery

handshake（238 行）整段搬出：重放会话、补订阅、清 warm 缓存、
拉 boards。四件事的顺序原样保留——重放必须在订阅之前，清缓存必须
在拉 boards 之后。

本步只搬不拆：内部四步共享大量局部变量，此处再拆会制造十几个参数。"
```

---

## Task 6: 收尾 —— 确认巨型 effect 已消失

**Files:**
- Modify: `desktop/src/App.tsx`

- [ ] **Step 1: 确认原 effect 已清空**

```bash
cd desktop
awk 'NR>=1600 && NR<=1700 {print NR": "$0}' src/App.tsx | head -40
```

原 L1635 的 `useEffect(` 应该已经不存在，或只剩几行胶水（把四个 hook 串起来）。

- [ ] **Step 2: 确认没有遗留的孤立闭包**

```bash
cd desktop
grep -n "const reconnectSlot\|const reconnectNow\|const scheduleReconnect\|const wireClient\|const handshake\|const connectSlot\|const connect = \|const syncActiveClient\|const connectRemoteTargets\|const onVisibilityChange" src/App.tsx
```

期望：**空输出**。有输出说明漏搬。

- [ ] **Step 3: 验证目标行数**

```bash
cd desktop
wc -l src/App.tsx
```

判据：**≤ 2950 行**（3509 − 632 + 约 40 行胶水 ≈ 2917）。

若超过 2950，说明还有大块没搬，用下面的命令找：

```bash
cd desktop
awk '{ if (length($0) > 0) print NR }' src/App.tsx | wc -l
grep -n "^  const [a-zA-Z]* = (" src/App.tsx | head -20
```

- [ ] **Step 4: 全量检查**

```bash
bash /tmp/b-check.sh
cd desktop && npx vitest run 2>&1 | tail -8
```

- [ ] **Step 5: Commit**

```bash
git add desktop/src/App.tsx
git commit -m "refactor(desktop): 巨型 effect 拆分收尾

632 行 effect 已由四个 hook 承担：useConnectionLifecycle /
useForegroundWake / useNotificationRouter / useHandshakeRecovery。
App.tsx 从 3509 行降到 2950 行以内。

146 个测试文件、1932 个用例全绿，断言零改动。"
```

---

## Task 7: 同步项目进度文档

**Files:**
- Modify: `docs/project-management/` 下对应的 desktop 模块文件

- [ ] **Step 1: 找到文件**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
ls docs/project-management/ | grep -i desktop
```

- [ ] **Step 2: 更新 App.tsx 的结构描述与行数**

- [ ] **Step 3: Commit**

```bash
git add docs/project-management/
git commit -m "docs: 同步 desktop 拆分后的 hook 结构"
```

---

## 完成判据（整块）

```bash
cd desktop
export PATH="/opt/homebrew/bin:$PATH"

# 1. 测试全绿
npx vitest run 2>&1 | tail -6                 # 146 files / 1932 tests passed

# 2. 用例名集合与开工前一致
bash /tmp/b-check.sh

# 3. 类型检查干净
npx tsc --noEmit && echo "TSC OK"

# 4. 巨型 effect 已不存在
grep -c "const wireClient\|const handshake\|const reconnectSlot" src/App.tsx   # 期望 0

# 5. 行数达标
wc -l src/App.tsx                             # 期望 ≤2950

# 6. 行为不变的旁证：几个关键守卫仍在
grep -c "非活动槽位" src/hooks/useNotificationRouter.ts   # 期望 ≥1
grep -c "故意不看" src/hooks/useForegroundWake.ts          # 期望 ≥1
```

第 6 项是本块唯一能机械检查「关键设计决策没被顺手优化掉」的手段。

---

## 风险与回退

| 风险 | 表现 | 处置 |
|---|---|---|
| `attempt` from `let` → ref 漏改 | 重试永不退避，或每次都从 0 开始 | Task 2 Step 5 的 `grep let` 检查；症状是断线重连变成死循环 |
| `disposed` 用局部而非 ref | 卸载后仍触发重连，测试报 act 警告 | Task 3 Step 3 已标注 |
| 三处 `turn/completed` 顺序被改 | 会话列表刷新早于渲染，偶发闪一下旧标题 | Task 4 Step 3 要求一起搬 |
| 非活动槽位守卫被删 | 多项目时 A 的通知污染 B 的视图 | 完成判据第 6 项机械检查 |
| 依赖数组写错（effect 重跑） | 重连风暴、socket 泄漏 | 新 hook 依赖数组只放 `enabled` 之类真需要重启的条件 |
| 20 个 setter 漏注入 | 编译错（TS 严格模式下会报未定义） | `tsc --noEmit` 会全部列出 |

**回退粒度：** 每个 Task 一个 commit，独立可回退。Task 2/3/4/5 之间**有依赖**（都往 `App.tsx` 的同一段删代码），回退必须从后往前；Task 4 是最大的，若它失败可先只回退它，前三个保留。

---

## 明确不做

- 不拆 `setThreadMode`（654 行）—— 跨关注点编排，抽出去只会制造隐式依赖
- 不拆 `selectThread`（320 行）—— 同上
- 不拆 `handshake` 内部四步（Task 5 Step 2）
- 不改任何 `ThreadStore` 的 `force()` 用法
- 不改任何 UI 行为、样式、文案
- 不新增测试（现有 1932 个用例是回归网，本块靠它们验证）
- 不动 7 个小 `useEffect`（L385 / L714 / L724 / L737 / L754 / L773 / L796）——它们各自只做一件事

---

## 与 spec 的数字差异（实施者必读）

本计划写于 main `3a1754cd`。写计划时重新实测了 `App.tsx`：

| 项目 | spec 写的 | 实测 | 说明 |
|---|---|---|---|
| 巨型 `useEffect` 行数 | 631 | **632**（L1635–L2266） | 差 1 行，无实质影响 |
| `App.tsx` 总行数 | 3,509 | **3,509** | 一致 |
| `useState` / `useRef` / `useCallback` | 未列 | **12 / 29 / 12** | 本计划新增的实测数据 |
| 现有 hook 数 | 5 | **5 领域 hook**（另有 2 个测试文件） | 一致 |
| `lib/` 模块数 | 未列 | **163 个 `.ts`** | 本计划新增 |
| vitest 规模 | 1,932 用例 | **1,932 用例 / 146 文件** | 一致 |

spec 关于 B 块的设计决策（抽四件事、不抽 `setThreadMode` 与 `selectThread`）完全适用。

**注意**：`desktop/src/App.tsx` 在本计划写作时未被 `fix/subagent-lease-leak` 触及，所以行号可直接使用。若实施时 `App.tsx` 已变化，**重新测绘**。

