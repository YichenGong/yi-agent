# 连接类错误对用户说人话

日期：2026-11-01

## 问题

手机端把 `rpc client disposed` 原样显示在对话的错误横幅里。用户既看不懂，也不知道该做什么。

根因不是某个 bug，而是**内部实现细节被直接当成了用户文案**：

1. `RpcClient.dispose()` 换客户端时，用 `new Error("rpc client disposed")` 拒绝所有在途请求
   （`desktop/src/lib/rpc.ts:129`）。
2. `connect()` 建新客户端前（`App.tsx:1865`）与收到 `exited` 时（`App.tsx:1658`）都会 dispose。
   也就是说，**断线、切回前台、握手重试这些正常路径**都会触发。
3. 各调用点 `catch` 后走 `formatError`（只取 `message`），写进 `session.lastError`。
4. `ChatView` 把它渲染成红色横幅（`desktop/src/components/ChatView.tsx:779`）。

同类问题不止这一句。会露英文的入口共 5 处，全在客户端侧：

| 来源 | 位置 | 原文 |
|---|---|---|
| 客户端被替换 | `lib/rpc.ts:129` | `rpc client disposed` |
| socket 断开 | `wsTransport.ts:66`/`:103` | `websocket closed (1006)` |
| 宿主接缝未连上 | `App.tsx` 8 处 + `hooks/useBoardState.ts:100` | `not connected` |
| 中继无电脑 | `yi-agent-rs/crates/yi-agent-relay/src/lib.rs:178` | `no computer connected for this session`（code -32000） |
| 状态栏 | `components/StatusBar.tsx` | `connecting` / `exited` / `error: …` |

## 方案

分三层，而不是把中文塞进抛错处。

**① 源头给结构。** 新增 `desktop/src/lib/connectionError.ts`：

- `ConnectionError` 类，`kind: "disconnected" | "no_computer"`；
- `connectionErrorKind(error)` 识别器：结构化优先（自己的类、中继的 `-32000`），
  再按字面量兜底（`rpc client disposed` / `websocket closed` / `not connected`），
  因为新老版本可能同时在线。

`message` 仍是英文——它要出现在日志、测试断言和 `title` 里。给人看的中文在边界处给。

**② 边界给文案。** `formatError` 先过 `connectionErrorText`，命中即返回中文：

- `disconnected` → 「与电脑的连接已断开，正在自动重连，无需操作。」
- `no_computer` → 「电脑端未连接。请确认桌面端 App 已打开，连上后会自动恢复。」

选 `formatError` 作为唯一出口，是因为它被 20 多个 `catch` 点共用（对话横幅、设置页、
diff 面板、图片、看板）。改一处，全覆盖。

**③ 过期即收起。** 握手成功后遍历 `store.ids()` 调 `clearConnectionErrorFrom`，只清
那两句常量。

理由：自动重连常常几秒就好。红条要是留着，反而像是一个还没解决的问题。
只认这两句常量，是为了不误伤业务错误（服务端拒绝、参数不对与连接无关，网通了也该留着）。

顺带把状态栏的原始状态串翻成中文（`connectionStatusLabel`），原文留在 `title` 里供排查。

## 不采用的方案

- **只改抛错处的字面量**：把中文烧进协议层，日志和按类型判断都得去匹配中文字符串。
- **只在渲染处做字符串黑名单**：新增一种断线就得再加一条，且没有类型约束。

## 为什么用户不需要做任何事

退避重连是自动的（`lib/reconnect.ts`：500ms → 1s → 2s → 4s → 8s，封顶 10s），
握手成功即清零。所以文案的重点是「在自动恢复」+「无需操作」，而不是让用户去点什么。
唯一需要人介入的是 `no_computer`：手机侧等多久都不会好，得去开桌面端。

## 测试

- `lib/connectionError.test.ts`：识别（含中继 -32000、字面量兜底、不误伤业务错误）、
  翻译、只清连接类文案。
- `lib/connectionStatus.test.ts`：三种已知状态、`error:` 前缀收成一句、未知状态透传。
- `App.test.tsx`：断线时显示中文且原始英文不露面；重连成功后自动收起；
  业务错误在重连后仍保留。
- 夹具订正：`-32000` 从「任意错误」的占位改成 `-32603`（前者在中继语义里专指
  「电脑未连接」，app-server 业务码段为 `-32001`~`-32025` 与 `-32600+`）；
  `App.test.tsx` 的假 `RpcClient` 补上「`dispose()` 拒绝在途请求」这一真实契约。
