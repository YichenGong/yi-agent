# 手机端「黑屏」的根治：加载态、停滞检测与错误兜底（设计）

> 目标：手机（iOS App，`desktop/` 前端经 Tauri 打包）在**切会话**与**不切会话**
> 两种情形下都会出现一片全黑。本文给出四条**已用真实代码路径复现**的根因与修法。
>
> 核心判断：**黑屏不是渲染坏了，而是「本该有内容的地方什么都没有」，而客户端把
> 「没有内容」与「还没拿到内容」当成同一件事。** 手机 `body` 底色是深色
> （`desktop/src/index.css:7` 的 `--panel: #171717`，由
> `[data-mobile="true"] body { background: var(--panel) }` 应用，见
> `desktop/src/mobileLayout.test.ts:80` 的断言），于是「空」与「黑」在视觉上无法
> 区分。真问题是缺少一个显式的**加载中 / 失败 / 可重试**状态机。
>
> 范围：**界面类改动（骨架 / 失败占位 / boundary / `CacheNotice`）几乎全部落在
> 远端路径上，桌面端视觉不变。** 唯一会触及桌面端的是 `syncState === "failed"`
> 这个内部状态：resume 的失败/成功收尾是**两种客户端共享**的代码
> （`App.tsx:1093` 的 `await` 到 `:1154` 的 `finally`，不在
> `isRemoteClient()` 之内），见 §6.1 的逐条说明。
> 桌面端另有一个独立的 resume 无超时问题，不在本文范围。
>
> 日期：2026-11-01
>
> 相关：[2026-10-11-mobile-local-cache-design.md](./2026-10-11-mobile-local-cache-design.md)
> （三段式首屏：先显示缓存 → 再对账）、
> [2026-11-01-mobile-cache-hardening-design.md](./2026-11-01-mobile-cache-hardening-design.md)
> （缓存 LRU 与作用域隔离；本文**依赖**其 `syncState` 语义）、
> [2026-10-17-client-state-negotiation-design.md](./2026-10-17-client-state-negotiation-design.md)
> （`syncPlan` 裁决）、
> [2026-10-18-ios-qr-scan-usability-design.md](./2026-10-18-ios-qr-scan-usability-design.md)
> （同一个「无 boundary → 整树卸载 → 黑屏」模式的首次记录）

## 1. 问题

用户报告两个现象：

1. **切会话时**可能出现黑屏。
2. **不切会话**也可能出现黑屏。

两者看似同一件事，实际是**四条独立机制**（其中三条会造成黑屏，R3 不会——它是
同一个状态机缺陷的另一面，连带修掉）：

| 编号 | 机制 | 典型触发 | 是黑屏吗 |
|---|---|---|---|
| R1 | 同步中转录区**真的没有内容**，而没有任何加载态 | 切冷会话 / 大会话 | **是**（主因） |
| R2 | `thread/resume` 无超时，**阻塞握手** | 切后台再回前台 | **是**（主因） |
| R3 | 一次失败即**不再重试**，再点无效 | 服务端拒绝 resume | **不是**——见下 |
| R4 | 无根级 ErrorBoundary，**一次异常整树卸载** | 任意渲染期异常 | **是** |

**R3 必须说清楚，否则会误导**：PROBE-J 实测，resume 被拒后转录区**不是空的**——
它渲染了一条错误横幅（内容形如 `unknown thread`），输入框也可用：

```
PROBE-J transcript text: "unknown thread"
PROBE-J transcript children: 1
PROBE-J textarea present: true
```

所以 R3 的用户体验是「一条英文错误信息 + 无法重试 + 顶部提示消失」，**不是一片
黑**。它仍然要修（因为用户被卡死、且看到的是未翻译的服务端原文），但它和「黑屏」
是两个症状，不应混为一谈。本文把它列为 R3 是因为它与 R1/R2 共享同一个状态机
缺陷，修法也只有一处（`syncState` + `forget`）。

## 2. 证据（可复现）

本节所有结论都来自**真实代码路径**，用仓库既有的
`desktop/src/App.test.tsx` 桩（真实 `App` 组件 + 内存 RPC 客户端）测得，
不是静态推测。探针代码见附录 A，任何人可用同一条命令复现。

运行方式：

```bash
cd desktop && npx vitest run src/App.test.tsx -t "PROBE"
```

### 2.1 实测输出（修复前，当前 `main`）

```
PROBE-A resume counts: { before: 1, mid: 2, after: 2 }
PROBE-A bodyText: "+ New thread ... one× two× unknown thread 附加文件 Normal ... 已连接 /w"

PROBE-B bodyText:  "+ New thread ... one× 内容未缓存 · 连接断开 ... Send 正在连接…/w 重新配对"
PROBE-B statusbar: "正在连接…/w 重新配对"
PROBE-B resume count: 1

PROBE-C scroller text during sync: ""
PROBE-C scroller childCount: 0

PROBE-D scroller text: ""            # 真·空会话：空是正确的
PROBE-D scroller childCount: 0

PROBE-G notice: "内容未缓存 · 连接断开"

PROBE-E innerHTML: ""               # 整棵树消失
PROBE-E sidebar present: false
PROBE-F sidebar present: true        # 有 boundary 时侧栏还在
PROBE-F fallback present: true
```

逐条对应：

- **PROBE-C**：`thread/resume` 挂起期间，转录滚动容器 `textContent` 为 `""`、
  `childElementCount` 为 `0`。这就是「切会话黑屏」的直接画面——不是渲染坏了，
  是**真的什么都没渲染**。
- **PROBE-B**：`thread/resume` 永不返回时，底栏停在「正在连接…」，顶部提示说
  「内容未缓存 · 连接断开」。用户面对一片空白，且**永远等不到恢复**。
- **PROBE-A**：resume 被拒后再点两次同一会话，`resume` 请求总数固定为 2
  （初始 1 次 + 切换 1 次），**再点完全没有重试**。
- **PROBE-G**：同步挂起时顶部提示的文案是「内容未缓存 · 连接断开」——**说的是
  错误的原因**（连接其实是通的，只是 resume 在途）。
- **PROBE-E**：无 boundary 时一次渲染异常后 `container.innerHTML === ""`，
  侧栏一并消失；**PROBE-F** 证明只要包一层 `ErrorBoundary`，侧栏就还在。

> 说明：`scroller.clientHeight` 在 jsdom 里恒为 0（无布局引擎），**不作为证据**。
> 视觉上的「大片黑」由「容器为空 + `body` 深色底」推出，而非测量得出。

### 2.2 修复方案本身也验证过（红绿）

设计不能只在纸面上成立。本文提出的三个新构件都在 worktree 里写了真实实现并跑通：

| 构件 | 验证方式 | 结果 |
|---|---|---|
| `createWatchdog`（§4.2） | 6 条单测：超时触发 / `touch` 续命 / `stop` 抑制延迟触发 / fire 后 `touch` 无效 / `stop` 后 `touch` 不复活 / fire 后 `stop` 不抛 | 6/6 通过 |
| `ErrorBoundary.resetKey`（§4.4） | 5 条单测：不传 `resetKey` 保持原行为 / `onError` 只调一次 / 正常渲染子树 / `resetKey` 变化时清 `hasError` / 不变时不清 | 5/5 通过 |
| `SubscriptionWindow.forget`（§4.1） | 4 条单测：删除生效 / 其余顺序不变 / 未知 id 无副作用 / 删后 LRU 上限仍生效 | 4/4 通过 |

**最关键的红绿验证（R3 的修复是否真的解锁重试）**：

```
修复前： 点击失败会话 → resume 总数 2 → 再点 → 仍为 2   （卡死）
打上 forget + failed 补丁后：
         点击失败会话 → resume 总数 2 → 再点 → 变 3     （解锁）
```

补丁后跑 `App.test.tsx` 全量：**234 passed / 0 failed**，证明该修法不破坏既有
行为。补丁随后已回退（本文只出设计，不含实现）。

> 这三处「先写出来再写进 spec」的代价很小，但它排除了一个常见失败模式——方案
> 在文档里自洽，落地时才发现 `resetKey` 会连带重挂载子树、或 `forget` 之后
> LRU 顺序被打乱。

## 3. 根因

### R1 同步中主区真的什么都没有（切会话黑屏的主因）

`ChatView` 的 render（`desktop/src/components/ChatView.tsx:742`）只输出四样东西：
`HistoryPrefix`、`LiveTail`、`retrying` 横幅、`error` 横幅。**没有任何加载占位**。

`items` 为空时 `HistoryPrefix` 的 `count = items.length - 1 = -1`，被守卫掉；
`tail` 为 `undefined`；`retrying` / `error` 通常为 `null`。于是整个容器是空的
（PROBE-C 实测 `childElementCount === 0`）。

`ChatView` 的 `syncing` prop **只参与滚动控制，从不渲染任何可见内容**
（`ChatView.tsx:519`、`:538`、`:554`、`:638`、`:740` 五处全部是滚动逻辑）。这
意味着「正在同步」这个事实**根本没有被画到屏幕上**。

同步期间的唯一反馈是顶部一条 1 行 `CacheNotice`（`App.tsx:3116-3126`），而它的
文案在冷启动路径上是**错的**：

```tsx
{/* App.tsx:3116-3126 */}
{sess.currentId !== null &&
  ((offline && cachedAt.current.has(sess.currentId)) ||
    store.peek(sess.currentId)?.syncState === "syncing") && (
    <CacheNotice
      savedAt={cachedAt.current.get(sess.currentId) ?? null}
      syncing={!offline && store.peek(sess.currentId)?.syncState === "syncing"}
    />
  )}
```

`syncing` 要求的条件是 `!offline`；而**冷启动/重连时的握手正在 `await
selectThread`**（见 R2），`status` 还不是 `"connected"`，于是 `offline` 为真
（`App.tsx:310`：`const offline = isRemoteClient() && status !== "connected"`）。
结果 `syncing` 恒为 `false`，`CacheNotice` 落到 `offlineLabel(null)`，显示
「内容未缓存 · 连接断开」。

**PROBE-G 实测就是这个字符串。** 连接明明是通的，却告诉用户「已断开」。

> 若用户是**已连接后**切会话，`offline` 为假，此时显示「正在同步…」——这条
> 才是对的。所以同一个界面在两条路径上说了两种话，其中一种是假话。

**服务端回放是分块下发的**：`REPLAY_CHUNK_BYTES = 256 KiB`、
`REPLAY_CHUNK_MAX_ITEMS = 200`（`yi-agent-app-server/src/server.rs` 的
`REPLAY_CHUNK_BYTES` / `REPLAY_CHUNK_MAX_ITEMS` 两个常量；行号会随该文件变动而
漂移，按名字检索即可）。从点击到首块
渲染之间隔着一个「中继 RTT + 服务端读盘 + 首块序列化」的空窗，冷会话或大会话时
不是毫秒级。这段空窗**没有任何可见内容**。

> **补充（桌面端同病）**：`CacheNotice` 渲染处的条件（`App.tsx:3116`）只有
> `sess.currentId !== null` 与两个同步判据，**没有 `isRemoteClient()`**。PROBE-I
> 实测：未配置远端（桌面端）且 resume 挂起时，页面同样渲染出「正在同步…」。
> 也就是说桌面端在长回放期间同样只有顶部一行提示、主区一片空白——同一个 R1，只是
> 桌面端回放通常更快、暴露得不那么明显。本文的修复通过共享的 `CacheNotice` 与
> 「有内容就不打扰」的占位规则，**顺带**改善桌面端，但桌面端不加骨架屏（§6.1）。

> **已排除的方向**（避免后续走偏）：
> - **不是**内容缓存设计问题：缓存有 LRU（`contentStore.ts:9`，
>   `maxCount: 10, maxBytes: 5 * 1024 * 1024`）。
> - **不是** `item` 查找的算法问题：`Session.upsertItems` 的 `findIndex`
>   （`session.ts:460`）只在单条 item 上扫数组，是 O(N)/条，不是 O(N²)/块。
>   长期性能另有专门基线（`desktop/scripts/perf/`），不在本轮。

### R2 `thread/resume` 无超时 → 握手被永久阻塞（不切会话黑屏的主因）

`RpcClient.request()`（`desktop/src/lib/rpc.ts:164`）返回的 Promise **没有任何
超时**，`requestOnce` 直接等 transport 回包。而 `handshake()` 里这一句是 `await`
的：

```ts
if (kept) {
  await selectThread(keptId!, { leaveBoard: false });   // App.tsx:2122
}
// ...
attempt = 0;
setStatus("connected");                                  // App.tsx:2131 —— 永不执行
```

于是 `thread/resume` 一旦不回包，整条握手链就停在那里：

- `setStatus("connected")` 永不执行 → 底栏永久停在「正在连接…」
  （PROBE-B 实测 `statusbar: "正在连接…/w 重新配对"`）；
- 因 `offline`（`App.tsx:310`）恒为真，顶部提示说「内容未缓存 · 连接断开」——
  它把「resume 没回包」误报成「连接断了」；
- **这正是「不切会话也黑」的成因**：`handshake` 会 `await selectThread(kptId)`，
  即**当前正在看的那条会话**。它卡住，主区就是空的。

**为什么 resume 会不回包**：手机切后台后 WebSocket 常常**半开**——TCP 层未收到
FIN，`onclose` 不触发（`desktop/src/wsTransport.ts` 里没有任何 Ping/heartbeat
保活），客户端这边的 `dispose` 也就不会被调用，在途请求悬着。服务端回环侧有
30s Ping（`yi-agent-rs/crates/yi-agent-relay/src/client.rs:105`，`PING_INTERVAL`），
但那只覆盖「电脑 ↔ 中继」一段，管不到「手机 ↔ 中继」。

即使不半开，每次 `visibilitychange` 都会 `reconnectNow` → `handshake` → 再一次
`await selectThread`。所以「切回前台就黑」和「不切也黑」是同一个机制。

### R3 一次失败就不再重试，再点无效（**不是黑屏**，同一状态机的缺陷）

`selectThread` 的 resume 失败分支把 `syncState` 置回了 `"synced"`——**谎报对账
完成**：

```ts
} catch (e) {
  replaySession.endReplay();
  const view = store.peek(id);
  if (view) {
    view.session.lastError = formatError(e);
    view.syncState = "synced";     // App.tsx:1149 —— 实际是失败
  }
```

而成功路径才 `warm.add(id)`（`App.tsx:1114`），失败路径不加。更关键的是**早退
判据用的是 `inWindow || running`，不是 `warm`**：

```ts
const inWindow = subWindow.current.has(id);   // App.tsx:977
const win = subWindow.current.touch(id);      // App.tsx:978 —— 无条件记录
// ...
if ((inWindow || running) && !opts?.forceResume) return;   // App.tsx:1040
```

`touch` 在进入时**无条件**执行，所以失败后该 id 已在窗口里，`inWindow` 恒为真。
用户再点多少次都会被 `:1040` 早退挡掉。**PROBE-A 实测：连点 3 次，`resume` 总数
固定为 2。**

`"synced"` 的语义是「已与服务端对账完成」（`threadStore.ts:48` 的注释：
*"Whether this thread's transcript has been reconciled with the server on this
connection"*）。失败路径复用它会同时造成两个后果：

1. **`CacheNotice` 消失**（它的条件是 `syncing`，或 `offline && has(cache)`），
   顶部那行提示没了；
2. 下一次点击被 `inWindow` 挡住，**没有任何重试入口**；
3. 屏幕上留下的是 `session.lastError` 渲染出的转录区横幅，内容常常是**服务端
   原文**（PROBE-J 的 `unknown thread`），未翻译、也没告诉用户该怎么办。

注意第 3 点：转录区**有内容**（那条横幅），所以这不是 R1 那种空容器。

**适用边界（必须说清，避免夸大）**：

- 若失败是**连接类**（`ConnectionError` / `no computer connected` /
  `rpc client disposed`），会走重连，而 `subWindow` 在重连时被整体换新
  （`App.tsx:1935`：`subWindow.current = new SubscriptionWindow()`），重连后即可
  重试。这类能自愈。
- 若失败是**应用层拒绝**（如服务端回 `-32602 unknown thread`、`-32013` 等），
  连接仍然健康、不会重连，`subWindow` 不清空 → **用户被永久卡住**，唯一出路是
  切一次前后台触发重连。PROBE-A 用的就是这条路径。

### R4 无根级 ErrorBoundary → 一次渲染异常即整树卸载

`desktop/src/main.tsx:12-16` 直接 `createRoot(...).render(<App/>)`。全仓库唯一的
`ErrorBoundary` 只包了扫码器（`src/components/PairingScreen.tsx:307`），也没有
`window.onerror` / `unhandledrejection` 兜底。

React 19 的语义：渲染期异常若无 boundary 捕获，**卸载整棵树**。PROBE-E 实测：

```
PROBE-E innerHTML: ""
PROBE-E sidebar present: false
```

手机上就是一片黑，且**没有任何恢复入口**，必须杀掉 App 重开。项目在 2026-10-18
为扫码黑屏单点补过 boundary，但根级仍缺——那次修的是「一处已知抛点」，不是
「未知抛点」。

## 4. 设计

一句话：**把「空」和「黑」拆开**。凡是「本该有内容但此刻没有」的状态，都必须
在屏幕上说清楚是三件事之一——**正在加载**、**加载失败（可重试）**、
**渲染异常（可恢复）**。

| # | 改动 | 修哪条 | 落点 |
|---|---|---|---|
| 1 | `syncState` 增加 `failed` + 失败时清窗口记忆 | R3 | `threadStore.ts` / `subscriptionWindow.ts` / `App.tsx` |
| 2 | 停滞检测（idle watchdog）包住 resume | R2 | 新 `lib/syncWatchdog.ts` + `App.tsx` |
| 3 | 主区三种显式态：骨架 / 失败重试 / 异常 | R1、R4 | 新 `components/TranscriptPlaceholder.tsx` + `ChatView` |
| 4 | 根级 + 转录区两层 ErrorBoundary | R4 | `main.tsx` + `App.tsx` + `ui/ErrorBoundary.tsx` |
| 5 | `CacheNotice` 增加失败态文案，修掉「说错原因」 | R1、R2 | `components/CacheNotice.tsx` |

### 4.1 `syncState` 增加失败态

`ThreadView.syncState` 目前是 `"syncing" | "synced"`（`threadStore.ts:48`）。新增
`"failed"`，并补两个字段让界面能说清原因：

```ts
syncState: "syncing" | "synced" | "failed";
/** 上次对账失败的原因（已格式化，可直接上屏）。仅 failed 时有意义。 */
syncError?: string | null;
```

改动点（**注意：`thread/resume` 的收尾是两种客户端共享的**，见 §6.1；下面逐条
标注影响面）：

1. `App.tsx:1149` 失败路径：`"synced"` → `"failed"`，并写
   `view.syncError = formatError(e)`。
   **保留** `session.lastError` 的写入：连接类错误仍要靠它触发既有的
   `clearConnectionErrorFrom` 自愈逻辑。
2. `App.tsx:1027`（readItems 补齐的 `finally`）：目前**无条件**置 `"synced"`，
   即使 `catch` 里什么都没补到。改为：成功才 `"synced"`，失败且视图仍空时
   `"failed"`。**这条只在 `isRemoteClient()` 内**（`readItems` 是远端专属补齐
   路径），桌面端不受影响。
3. **清窗口记忆**，这是 R3 的直接修法。给 `SubscriptionWindow` 加：

```ts
/** 忘掉某条 thread（对账失败：下次点击必须重走 resume，不能被窗口早退挡掉）。 */
forget(id: string): void {
  this.ids = this.ids.filter((x) => x !== id);
}
```

并抽一个收尾函数，保证「置 failed」与「清窗口」永远成对发生：

```ts
/** 对账失败的唯一收尾路径：置失败态 + 解开早退，使下次点击重新走 resume。 */
function failSync(id: string, error: string): void {
  const v = store.peek(id);
  if (v) {
    v.syncState = "failed";
    v.syncError = error;
    v.session.lastError = error;
  }
  subWindow.current.forget(id);
  warm.current.delete(id);
  force((n) => n + 1);
}
```

> **为什么扩展枚举而不是加布尔量**：现有读点都在问「这个会话的内容可靠吗」。
> 再加一个布尔量会产生两个真相源；扩展枚举让 `switch` 时编译器提醒每一处。

**`syncState` 的完整读点清单**（改枚举前必须逐个处理，共 6 处生产代码）：

| 位置 | 现用途 | 加 `failed` 后要怎么改 |
|---|---|---|
| `App.tsx:999` / `:1086` / `:2498` | 写 `"syncing"` | 不变 |
| `App.tsx:1027` / `:1116` / `:1149` | 写 `"synced"` | `:1027`、`:1149` 改 `failSync`；`:1116` 是真成功，不变 |
| `App.tsx:2411` | **拦发送**：`offline \|\| syncing` | **必须改**，见下 |
| `App.tsx:3118` / `:3123` | `CacheNotice` 条件与文案 | 加 `failed` 分支（§4.5） |
| `App.tsx:3154` | `ChatView` 的 `syncing` prop | 见下 |
| `threadStore.ts:48` / `:81` | 类型与默认值 | 加 `"failed"`，默认仍 `"synced"` |

两处**必须一起改**，否则引入 `failed` 会造出新窟窿：

**① 发送拦截（`App.tsx:2411`）**——现在的判据是 `syncing`：

```ts
if (offline || store.peek(id)?.syncState === "syncing") {
  store.view(id).session.lastError = offline ? "正在连接，请稍候" : "正在同步，请稍候";
  return false;
}
```

`"failed"` 不在这个条件里，于是**进入失败态后反而允许发送**。这是错的：失败态
意味着「屏幕上的内容可能与服务端不一致」，正是既有注释要拦的那种情形
（*"屏上可能还是旧缓存，此刻发出去的上下文与服务端实际上下文可能不一致"*）。
必须扩展为：

```ts
const state = store.peek(id)?.syncState;
if (offline || state === "syncing" || state === "failed") {
  store.view(id).session.lastError =
    offline ? "正在连接，请稍候"
    : state === "failed" ? "内容尚未加载完成，请先重试"
    : "正在同步，请稍候";
  return false;
}
```

**② `ChatView` 的 `syncing` prop（`App.tsx:3154`）**——它只控制滚动（§3 R1 已
证明不渲染任何可见内容），`failed` 时应当按「没就绪」处理，避免恢复锚点把空
转录当成「已到底」：

```ts
syncing={sess.currentId
  ? (() => { const s = store.peek(sess.currentId)?.syncState;
             return s === "syncing" || s === "failed"; })()
  : false}
```

### 4.2 停滞检测（idle watchdog）

**不用「总时长超时」**：大会话的正常回放可能超过十几秒，一刀切会频繁误判——这正是
「有时」的来源，不能用错误判据去修。

**判据**：resume 发起后记录**最近一次活动**的时间戳，收到**该 thread 的任何帧**
即刷新（`thread/started` / `items/completed` 携带该 id，以及 resume 的响应）。
连续 `SYNC_IDLE_TIMEOUT_MS` 无活动 → 判死。

```ts
/** 手机端 resume 的停滞阈值：连续这么久收不到该 thread 的任何帧就判死。 */
export const SYNC_IDLE_TIMEOUT_MS = 15_000;
```

放置位置：新纯函数模块 `desktop/src/lib/syncWatchdog.ts`，不 import React 与 RPC，
便于单测：

```ts
export interface Watchdog {
  touch(): void;
  stop(): void;
  readonly fired: boolean;
}
export function createWatchdog(
  timeoutMs: number,
  onTimeout: () => void,
  timer?: { setTimeout: typeof setTimeout; clearTimeout: typeof clearTimeout },
): Watchdog;
```

接线（`selectThread` 的 resume 分支）：

```ts
const watchdog = createWatchdog(SYNC_IDLE_TIMEOUT_MS, () =>
  failSync(id, "内容加载超时，请重试"),
);
syncWatchdogs.current.set(id, watchdog);
try {
  const resumed = await c.request(...);
  // ... 成功路径 ...
} catch (e) {
  failSync(id, formatError(e));
} finally {
  watchdog.stop();
  syncWatchdogs.current.delete(id);
  inFlightResume.current.delete(id);
}
```

**活动信号的接法**：resume 的响应靠 `await` 本身即可；回放帧是通知，走
`client.onNotification`。为避免在通知大 switch 里再插特判，把活动注册到 per-thread
的注册表——**在 `ownsThreadView` 判定之后**加两行：

```ts
const wd = syncWatchdogs.current.get(n.params?.thread_id);
if (wd) wd.touch();
```

> **为什么放在 `ownsThreadView` 之后**：非活动槽位的帧本来就不写本视图，用它们
> 刷新计时器会掩盖「活动槽位真的卡住」的事实，制造假阴性。

**在 `finally` 里停机是硬要求**：否则定时器在 resume 落定后仍会触发，把一次成功
的加载翻成失败。

**超时值 15s 的依据**：分块间隔在真实环境里是毫秒到百毫秒级；15s 是「三倍于任何
合理的单块等待」，同时短于用户认为「卡死了」的耐心阈值。**不要低于 10s**——高丢包
移动网络下单块重传就可能占几秒。

**不做自动重试**：自动重试会让用户在长会话上看到「闪一下又黑」，且放大服务端压力。
失败态给出显式按钮，由用户决定。

### 4.3 主区的三种显式态

新增 `desktop/src/components/TranscriptPlaceholder.tsx`，由 `ChatView` 在**转录
为空**时渲染：

```ts
export type PlaceholderKind = "loading" | "failed" | "crashed";
```

- **`loading`**：骨架屏。四条左对齐圆角灰条（宽度 88% / 64% / 78% / 42%），
  `animate-pulse`；`prefers-reduced-motion: reduce` 时改为静态。上下留白与真实
  消息一致，避免内容到达时跳动。
- **`failed`**：居中一块。标题「内容加载失败」+ 一行原因（`syncError`）+ 主按钮
  「重试」。`onRetry` 调 `selectThread(id, { forceResume: true })`——复用既有的
  强制全量重同步路径，`forceResume` 会跳过**全部**早退（`App.tsx:1040` 的
  `!opts?.forceResume`），因此不受 R3 的 `inWindow` 阻碍。
- **`crashed`**：4.4 的转录区 boundary 使用。标题「这段内容渲染失败」+ 原因 +
  「重试」（重置 boundary 的 `hasError`）。

**判定顺序**（保证不重复提示）：

```
items 非空                        → 不渲染占位（原样，绝不遮挡正在读的内容）
!isRemoteClient()                 → 不渲染占位（桌面端不加骨架屏，§6.1）
syncState === failed              → failed
syncState === syncing             → loading
其余（synced 且空）                → 不渲染占位
```

**第二条是刻意的**：桌面端不渲染骨架屏。理由是桌面端没有本地缓存、内容恒来自
服务端，回放通常更快；冒然给桌面端加骨架会改变既有观感，而本文的目标是修**手机**
的黑屏。桌面端确实也有 R1（PROBE-I），但它的修法是另一件事（需要先决定桌面端到
底要不要缓存与占位），本文只保证桌面端**不出现新的视觉元素**。

最后一条很重要：**空会话（新建 / `/clear` 之后）本来就是空的**（PROBE-D 证明它
与「同步中」在 DOM 上完全一致，都是空容器）。给它挂骨架会让用户以为一直在加载。
此时维持现状——空就空着，这是正确语义。

### 4.4 两层 ErrorBoundary

复用现有 `desktop/src/ui/ErrorBoundary.tsx`。现有 `children` / `onError` /
`fallback` 三个属性够用，只需**增量**一个可选 `resetKey`（见下），不改既有语义。

**第一层（根级）**，`main.tsx`：

```tsx
ReactDOM.createRoot(el).render(
  <React.StrictMode>
    <ErrorBoundary onError={logCrash} fallback={<AppCrashedScreen />}>
      <App />
    </ErrorBoundary>
  </React.StrictMode>,
);
```

`AppCrashedScreen`（新组件 `desktop/src/ui/AppCrashedScreen.tsx`）：满屏
`bg-surface`、居中说明 + 一个「重新加载」按钮（`location.reload()`）。
**必须不依赖任何 App 状态**——它可能在 App 初始化到一半时被挂上。样式只用
`index.css` 的 token 类。PROBE-F 已证明：包一层 boundary 后，未出错的部分仍然
存活。

**第二层（转录区）**，`App.tsx` 包住 `ChatView`（现位于 `App.tsx:3135`，外层
容器在 `:3134`）：

```tsx
<ErrorBoundary
  resetKey={sess.currentId ?? "none"}
  fallback={<TranscriptPlaceholder kind="crashed" onRetry={...} />}
>
  <ChatView ... />
</ErrorBoundary>
```

价值：一次畸形 item 导致的异常只毁掉转录区，**侧栏、输入框、底栏、「重新配对」
入口全都还在**，用户能切走、能重连、能报错。这正是「全黑」与「可自救」的差别。

**为什么用新增的 `resetKey` 而不用 `key`**：boundary 的 `hasError` 一旦置位就
粘住，切会话必须重置——但用 `key` 会**连子元素一起重挂载**，等于每次切会话都强制
重新挂 `ChatView`，把正在读的滚动位置、展开的 reasoning 块全部丢掉；那恰好是本次
要修的「切换体验」路径。给 `ErrorBoundary` 增一个可选属性即可只重置错误位：

```ts
export interface ErrorBoundaryProps {
  children: ReactNode;
  onError?: (error: unknown) => void;
  fallback?: ReactNode;
  /** 该值变化时清掉 `hasError`，让子树在原位置恢复渲染。 */
  resetKey?: unknown;
}
// getDerivedStateFromProps(props, state):
//   props.resetKey !== state.seenKey → { hasError: false, seenKey: props.resetKey }
//   else → null                       （必须返回 null，不能返回 {}）
```

这是**向后兼容的增量**：`resetKey` 可选，配对页那处调用（`PairingScreen.tsx:307`）
不传即维持现状，语义完全不变。

**`unhandledrejection` / `window.onerror`**：只在 `main.tsx` 注册**日志**回调
（`console.error`），**不改渲染**。理由：这两类事件不参与 React 渲染，接住它们
也不会让界面变好；而它们常常是业务预期内的失败（一次失败的 RPC）。加渲染会制造
新的误报。本条属于「留证据」。

### 4.5 `CacheNotice` 修掉「说错原因」

现状（`components/CacheNotice.tsx`）只有两态：`syncing` 与「离线/未缓存」。
PROBE-G 证明：冷启动同步期间它显示「内容未缓存 · 连接断开」——连接是通的。

改为三态，新增 `failed`：

```ts
export function CacheNotice({ savedAt, syncing, failed }: {
  savedAt?: number | null; syncing?: boolean; failed?: boolean;
})
// failed  → "内容加载失败 · 可重试"
// syncing → "正在同步…"
// 其余    → offlineLabel(savedAt)
```

同时修掉 `App.tsx:3121-3124` 的调用：`syncing` 不应再要求 `!offline`——**冷启动**
时 `offline` 为真只是因为握手还没走完（`status` 尚未置 `connected`），而
`syncState === "syncing"` 本身就是更强的证据。改为：

```tsx
syncing={store.peek(sess.currentId)?.syncState === "syncing"}
failed={store.peek(sess.currentId)?.syncState === "failed"}
```

> **注意适用范围**：这条只解释「冷启动/重连期间」为何说错话（PROBE-G 的场景）。
> 已连接后切会话时 `offline` 为假，`syncing` 正常为真，显示的就是「正在同步…」。
> 桌面端不受 `offline` 影响（`offline` 的定义本身就含 `isRemoteClient()`，见
> `App.tsx:310`），所以桌面端一直显示「正在同步…」（PROBE-I 实测）。

## 5. 数据流

切到冷会话（手机端，修复后）：

```
点击侧栏
  └ selectThread(id)
      ├ store.select(id)                    立刻切视图
      ├ prefillFromCache(id)                有缓存 → 立即上屏（既有三段式）
      ├ subWindow.touch(id) + subscribe     订阅窗口
      ├ syncState = "syncing"                ← 4.1
      ├ watchdog.start()                     ← 4.2
      │    └ 每帧（该 thread）→ touch()       回放期间不断续命
      ├ suspend: await thread/resume
      │     ├ 成功   → "synced"，warm.add，watchdog.stop()
      │     └ 失败/停滞 → failSync(): "failed" + forget(id)，watchdog.stop()
      └ 渲染：
           items 空 + syncing → 骨架屏                    ← 4.3
           items 空 + failed  → 「内容加载失败」+ 重试      ← 4.3
           items 非空          → 原样
           渲染抛异常          → 转录区 boundary（侧栏/输入框仍在） ← 4.4
           更外层抛异常        → 根级 boundary（崩溃页 + 重新加载）← 4.4
```

关键点：`watchdog` 的续命信号与**渲染**解耦——它看传输层的帧，不看 React 状态，
因此即使主线程被长回放占满，超时判定也不会被 UI 卡顿误导（定时器在 macrotask
里排队，恢复后按真实时间差判定）。

## 6. 边界与不变量

### 6.1 桌面端到底哪里会变（必须精确，不能含糊）

`selectThread` 的结构值得说清楚，因为它决定了改动的真实影响面：

```
selectThread(id, opts)
├── if (isRemoteClient()) { ... subscribe / 窗口早退 / readItems ... }   ← 仅远端
├── if (running && !isRemoteClient()) { ... readItems ... ; return }      ← 仅桌面
├── if (warm.has || inFlightResume.has) return                            ← 共享
├── coldView.syncState = "syncing"                                        ← 共享
└── try { await thread/resume } catch { ... } finally { ... }             ← 共享！
```

**`thread/resume` 的 try/catch 在 `isRemoteClient()` 之外**（`await` 在
`App.tsx:1093`，`catch` 在 `:1141`，`finally` 在 `:1154`）。所以：

| 改动 | 桌面端会变吗 | 说明 |
|---|---|---|
| `syncState = "failed"`（`:1149`） | **会变（状态）** | 桌面端 resume 失败后 `syncState` 从 `"synced"` 变 `"failed"` |
| `forget(id)` / `warm.delete(id)` | **会变（行为）** | 桌面端失败后重试也变可行——这是**改善**，不是回归 |
| `TranscriptPlaceholder`（骨架/失败） | **不变** | 渲染条件里显式要求 `isRemoteClient()`（§4.3 判定顺序第二条），桌面端永不渲染 |
| 转录区 / 根级 ErrorBoundary | **不变视觉，但会生效** | boundary 是安全网，任何客户端都该有；桌面端以前没有，现在有了。转录区 boundary 的 `fallback` 是 `crashed` 占位——它只在**真出错**时出现，属修复而非回归 |
| watchdog | **不变** | 只在远端分支里创建（`isRemoteClient()` 内） |
| `CacheNotice` | **会变（文案）** | **它并不受 `isRemoteClient()` 约束**——实测桌面端也会渲染（PROBE-I 输出了「正在同步…」）。加 `failed` 分支后桌面端也会显示「内容加载失败 · 可重试」。这是**改善**：桌面端此前同样缺加载反馈 |
| `send` 的 `failed` 拦截（`:2411`） | **会变（行为）** | 桌面端 resume 失败后也会拦发送——也是改善：内容没同步成功就不该发 |

**结论**：桌面端的**视觉**逐像素不变；**内部状态与失败后的可恢复性**有变化，且
都是朝好的方向（以前失败即静默置 `"synced"` 且不可重试，现在明确置 `"failed"`
且可重试）。实现时必须为这三条「会变」的项补桌面端用例（§7.2 末三行），
不能靠「远端才走这条路径」的假设蒙混过去。

1. **桌面端：视觉零变化，状态有变化**（详见 §6.1）。不能笼统说「零变化」——
   resume 的收尾是两种客户端共享的，桌面端也会经到 `"failed"`。必须逐条交代。
2. **有内容就不打扰**：`items.length > 0` 时占位一律不渲染。用户正在读的转录不会
   被盖住。
3. **空会话不加骨架**：`synced` 且空 = 真的没有内容（新建 / `/clear`），维持现状。
4. **失败必可重试**：进入 `failed` 必须同时 `subWindow.forget(id)`。否则
   `App.tsx:1040` 的早退会吞掉重试——这是 R3 的唯一修法，必须有测试钉死。
5. **定时器必须停**：`watchdog.stop()` 在 `finally` 里；`syncWatchdogs` 在
   `store.drop(id)`（`App.tsx:931`、`:1525`）与会话重置（`App.tsx:2906` 一带）
   时清空对应条目，防泄漏。
6. **boundary 不吞错误**：`componentDidCatch` 仍要 `console.error`，保留现场。
7. **`ErrorBoundary` 的改动是纯增量**：`resetKey` 可选，不传即保持既有行为；
   配对页那处调用不加属性，语义与今天逐字节一致。
8. **失败态不许发送**：`failed` 必须与 `syncing` 一样拦在 `send` 之外
   （`App.tsx:2411`）。这是加 `failed` 时最容易漏掉的一处——漏了就会从「卡住」
   变成「对着不一致的上下文发消息」，比原问题更糟。

## 7. 测试

全部在 `desktop/`（`npx vitest run`）。每条新增用例必须能在**回退修复后变红**。

### 7.1 `lib/syncWatchdog.test.ts`（新增）

| 用例 | 断言 |
|---|---|
| `fires after the timeout with no activity` | 注入 fake 定时器，15s 后 `onTimeout` 被调、`fired === true` |
| `touch resets the deadline` | 每 10s `touch()`，跑 60s 仍不 fire |
| `stop prevents a late fire` | `stop()` 后推进时间，`onTimeout` 不被调用 |
| `touch after fire is a no-op` | 已 fire 后再 `touch()` 不改状态（防重入） |

### 7.2 `App.test.tsx` 行为用例

| 用例 | 断言（对应根因） |
|---|---|
| `shows a skeleton, not a blank pane, while a cold thread syncs` | resume 挂起 → `[data-testid="sync-skeleton"]` 存在（**R1**） |
| `shows a retry affordance when resume never settles` | 推进 15s → `[data-testid="sync-failed"]` + 「重试」存在（**R2**） |
| `retrying from the failed state re-issues thread/resume` | 点「重试」→ resume 请求数 +1（**R2/R3**） |
| `tapping a failed thread again retries instead of bailing` | 失败后点侧栏同一行 → resume 数 +1（**R3**，直接对应 PROBE-A） |
| `a failing resume is not reported as synced` | 失败后 `syncState === "failed"`（**R3**） |
| `a stalled resume still reaches connected` | resume 停滞 → 底栏最终显示「已连接」（**R2** 握手阻塞） |
| `keeps the transcript when content already arrived` | items 非空 → 无占位 |
| `does not show a skeleton for a genuinely empty thread` | `synced` + 空 → 无骨架、无失败态（对应 PROBE-D） |
| `the cache notice names the real reason while syncing` | 冷启动同步中 → 文案不含「连接断开」（对应 PROBE-G） |
| `desktop: a failed resume marks the thread failed and allows retry` | 桌面端失败后 `syncState === "failed"`，再点能重发 resume（**§6.1**，确认改善而非回归） |
| `desktop: a failed resume blocks sending` | 桌面端失败后 `send` 返回 `false` 且提示「内容尚未加载完成」（**§6.1**） |
| `desktop: no skeleton or failure pane is ever rendered` | 桌面端任何 `syncState` 下都不出现 `[data-testid="sync-skeleton"]` / `sync-failed` |

### 7.3 `ErrorBoundary` 用例

| 用例 | 断言（对应根因） |
|---|---|
| `a render throw inside ChatView leaves the sidebar and composer mounted` | 塞一个会抛的 item → 侧栏、`textarea`、`.app-statusbar` 仍在，转录区显示「这段内容渲染失败」（**R4**，对应 PROBE-F） |
| `switching threads resets the transcript boundary` | 出错后切走再切回 → 占位消失、内容正常 |
| `a throw above App renders the crash screen instead of a blank page` | 让 `App` 子树抛 → `#root` 非空且含「重新加载」（**R4**，对应 PROBE-E） |

## 8. 验证步骤

```bash
cd desktop
npx tsc --noEmit
npx vitest run          # 全量，含新增用例
npm run build
```

真机验收（需要 iPhone + 中继，本文不含自动化）：

1. 切到一条长会话：应立刻看到骨架，内容到达后骨架消失。
2. 手机切后台 30s 再回前台：应看到骨架 → 内容，而不是一屏黑。
3. 在电脑端 kill 中继/桥接后再切会话：15s 内出现「内容加载失败 + 重试」；
   恢复连接后点重试能加载出来。
4. 人为塞一条非法 item（仅测试构建）：转录区显示「渲染失败」，侧栏与输入框仍可用。

## 9. 明确不做

- **不做长会话渲染优化**（虚拟化 / 增量前缀 / 回放分块合并）。它与本问题**不共享
  机制**：黑屏的判据是「容器为空」（PROBE-C 实测 `childElementCount === 0`），
  而渲染优化解决的是「有内容但卡」。它是独立的性能题，另有基线脚本
  （`desktop/scripts/perf/`），另立卡片。
- **不做自动重试**。理由见 §4.2。
- **不改桌面端**。桌面端同样的 resume 无超时问题不在本轮。
- **不改 `SubscriptionWindow` 的窗口算法**（`SUBSCRIPTION_WINDOW_SIZE`）。只新增
  `forget()`，不动 `touch`/`has`/`current` 的语义。
- **不接错误上报服务**。`unhandledrejection` 只留 `console.error`，不新增网络请求。

## 附录 A：取证探针（可复现）

以下代码临时追加到 `desktop/src/App.test.tsx` 末尾即可复现 §2.1 的输出；**取证后
必须删除**，不要提交。它复用的是该文件既有的 `clients` / `state` 桩，跑的是真实
`App` 组件。

```ts
describe("PROBE blank screen", () => {
  const asRemote = () =>
    localStorage.setItem(
      "yi-agent.remote",
      JSON.stringify({ url: "wss://relay.test/ws", token: "yia_tok" }),
    );

  it("PROBE-A: 失败后再次点击不重试", async () => {
    asRemote();
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    state.dataSources["thread/resume"] = () => {
      throw { code: -32602, message: "unknown thread" };   // 应用层拒绝
    };
    render(<App />);
    await waitFor(() => expect(clients.length).toBeGreaterThan(0));
    await new Promise((r) => setTimeout(r, 50));
    const before = clients[0].requests.filter((r) => r.method === "thread/resume").length;
    fireEvent.click(screen.getByText("two"));
    await new Promise((r) => setTimeout(r, 50));
    const mid = clients[0].requests.filter((r) => r.method === "thread/resume").length;
    fireEvent.click(screen.getByText("two"));
    await new Promise((r) => setTimeout(r, 50));
    fireEvent.click(screen.getByText("two"));
    await new Promise((r) => setTimeout(r, 50));
    const after = clients[0].requests.filter((r) => r.method === "thread/resume").length;
    console.log("PROBE-A", { before, mid, after });   // 1 / 2 / 2
  });

  it("PROBE-B: resume 永不返回", async () => {
    asRemote();
    state.dataSources["thread/resume"] = () => new Promise(() => {});
    render(<App />);
    await waitFor(() => expect(clients.length).toBeGreaterThan(0));
    await new Promise((r) => setTimeout(r, 300));
    console.log("PROBE-B body", document.body.textContent);
    console.log("PROBE-B status",
      document.querySelector(".app-statusbar")?.textContent);
  });

  it("PROBE-I: 桌面端是否渲染 CacheNotice", async () => {
    // 不写 yi-agent.remote → 桌面端
    state.dataSources["thread/resume"] = () => new Promise(() => {});
    render(<App />);
    await waitFor(() => expect(clients.length).toBeGreaterThan(0));
    await new Promise((r) => setTimeout(r, 300));
    // → "正在同步…"（说明 CacheNotice 不受 isRemoteClient() 约束）
    console.log("PROBE-I", document.querySelector('[role="status"]')?.textContent);
  });

  it("PROBE-J: 失败后转录区是否真的空", async () => {
    asRemote();
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    state.dataSources["thread/resume"] = () => {
      throw { code: -32602, message: "unknown thread" };
    };
    render(<App />);
    await waitFor(() => expect(clients.length).toBeGreaterThan(0));
    await new Promise((r) => setTimeout(r, 60));
    fireEvent.click(screen.getByText("two"));
    await new Promise((r) => setTimeout(r, 60));
    const scroller = document.querySelector("[data-thread-id]");
    // → text "unknown thread" / childElementCount 1 ⇒ **不是黑屏**
    console.log("PROBE-J", scroller?.textContent, scroller?.childElementCount);
  });

  it("PROBE-C/D: 同步中与真·空会话的主区", async () => {
    asRemote();
    state.dataSources["thread/resume"] = () =>
      new Promise((resolve) =>
        setTimeout(() => resolve({
          thread_id: "t1", cwd: "/w", model: "m", permission_mode: "normal",
        }), 3000));
    render(<App />);
    await waitFor(() => expect(clients.length).toBeGreaterThan(0));
    await new Promise((r) => setTimeout(r, 500));
    const scroller = document.querySelector("[data-thread-id]");
    console.log("PROBE-C", scroller?.textContent, scroller?.childElementCount); // "" / 0
  });
});
```

`PROBE-E/F` 用一个最小文件验证「有/无 boundary」：

```tsx
/** @vitest-environment jsdom */
const Boom = () => { throw new Error("boom"); };

// 无 boundary
render(<div><aside data-testid="sb">s</aside><Boom /></div>, { container });
// → container.innerHTML === ""，sidebar present: false

// 有 boundary
render(
  <div>
    <aside data-testid="sb">s</aside>
    <ErrorBoundary fallback={<div data-testid="fb">failed</div>}><Boom /></ErrorBoundary>
  </div>,
  { container },
);
// → sidebar present: true，fallback present: true
```
