# 手机端重连/重配对路径设计（"连不上桌面"不再是无路可走）

> 目标：手机端（iOS WebView 客户端）一旦连不上桌面，用户始终有一条明确的出路——
> 重新扫码或重新填表；同时把三处"卡住且无声"的故障（中继电脑未连接的静默丢帧、
> 握手失败不重试、凭据被撤销后无限重试）修掉，令其复用既有的退避重连回环。
> 桌面端行为逐字节不变。
>
> 日期：2026-10-17

## 1. 问题

手机端是 app-server 的**远程客户端**。配对成功后凭据落盘
（`localStorage["yi-agent.remote"]`，`desktop/src/lib/remoteConfig.ts`），此后
`transportFactory` 据此选择 ws 传输（`desktop/src/transportFactory.ts`）。

配对表单的**唯一**出现条件是"iOS 且尚无持久化配置"：

- `PairingScreen.needsPairing`（`desktop/src/components/PairingScreen.tsx:303`）
  = `isIos && storedRemoteConfig(storage) === null`
- `App.tsx:1933` 渲染 `PairingScreen`；`App.tsx:1265` 的连接 effect 同样据此提前返回。

配对成功那一刻 `saveRemoteConfig` 写入 `{url, token}`，于是
`storedRemoteConfig` **永远非 null**、`needsPairing` **永远为 false**——配对表单
再无出现路径。用户因此**没有**重新扫码、重新填表、解除配对的任何入口。

失败后的全部行为是"无限重试同一份坏配置"：

- `App.tsx:1385` 收到 `exited` → `dispose` + 清 warm/订阅窗口 + `scheduleReconnect()`
- `desktop/src/lib/reconnect.ts` 500ms 起翻倍、封顶 10s，**只要组件挂着就绝不停下**
- `desktop/src/lib/remoteConfig.ts:74` 的 `clearRemoteConfig`（解除配对）**已存在但全仓库无 UI 调用**

### 1.1 三处"无声卡死"（实机故障的真实形状）

**(a) 中继路径上"电脑未连接"被静默丢弃。**
中继**不校验手机 token**：手机侧 `/ws` 只认 `session`
（`yi-agent-rs/crates/yi-agent-relay/src/server.rs:69`，`app_upgrade` 全函数无 token
字眼）；校验发生在**电脑侧桥接**那条连接上，用的是本地现铸的 token
（`yi-agent-rs/crates/yi-agent-relay/src/client.rs:13-19`）。因此：

- **4401 只在直连路径出现**（`yi-agent-rs/crates/yi-agent-app-server/src/ws.rs:243-246`）；
- 中继路径上"电脑没开"回一帧**无 id** 的错误
  `{"error":{"code":-32000,"message":"no computer connected for this session"}}`
  （`yi-agent-rs/crates/yi-agent-relay/src/lib.rs:38,168`，中继不解析 JSON-RPC 语义）。

这帧在客户端**两边都不匹配**：`RpcClient.onMessage` 要求 `id !== undefined` 才算
响应（`desktop/src/lib/rpc.ts:208`）、要求 `method !== undefined` 才算通知
（`rpc.ts:220`），两者皆不满足 → 什么都不做。结果 `handshake` 里的
`await client.request("initialize")` **永不 settle**：状态停在离线态、无报错，
且**桌面重新开机后手机不会自愈**（自愈只由前台唤醒触发，`App.tsx` 的
`onVisibilityChange`），因为中继不会补发。

**(b) 握手以错误结束不排重连。** `handshake` 的 catch（`App.tsx` 内
`setStatus("error: …")`）只设状态、**没有** `scheduleReconnect()`；握手失败
即永久卡住，直到用户手动切前后台。

**(c) 凭据被撤销后无限重试。** `wsTransport` 把服务端 4401 关闭上报成
`{state:"exited", code:4401}`（`desktop/src/wsTransport.ts:8,102`，已被
`wsTransport.test.ts` 钉住），但 App 不读 `code`，于是"token 已被桌面移除"与
"网络抖了一下"走同一条无限重试路径。

### 1.2 现有可用件（复用而非新造）

- `RpcClient` 已具备"同一 socket 上重跑 `initialize` 再重发请求"的既有模式
  （`-32010` 自愈，`rpc.ts:133-150`，被 `rpc.test.ts` 钉住）。
- `PairingScreen` 已具备地址预填（`initialUrl`）、扫码、错误映射
  （4401→`INVALID_CODE_TEXT`、其余→`CONNECT_FAILED_TEXT`）全部能力。
- 退避重连回环（`exited` → `scheduleReconnect` → `handshake`）已完备。

## 2. 目标与非目标

**目标**

- 手机端任何"连不上"的状态下，用户**始终**有一个可见的「重新配对」入口。
- 凭据被服务端明确拒绝（4401，直连路径）时**自动**把用户送上配对界面。
- 中继"电脑未连接"从"静默卡死"变为"可见的离线态 + 桌面回来即自愈"。
- 握手失败不再永久卡住。
- 桌面端行为逐字节不变；**iOS 首启配对路径逐字节不变**（QR 配对的硬不变量）。

**非目标**

- 不改中继与 app-server（Rust 侧零改动）。
- 不做"连接正常时主动换服务器"（留待以后；YAGNI）。
- 不做自动清除已存配置：重配是**非破坏性**的，提交成功才覆盖，返回即回原状。
- 不新增第二套重连状态机（一律复用既有 `exited` → 退避 → `handshake`）。

## 3. 设计

四块改动，全部在 `desktop/src/`。

### 3.1 手动入口：状态栏常驻「重新配对」

- `StatusBar` 新增可选 `onRepair?: () => void`；**传入才渲染**按钮
  （不传时 DOM 与现在一致）。刻意**不**塞进既有的通用 `actions?: ReactNode`：
  按钮的可见性由连接状态决定，做成组件的自身语义比让 App 手写条件更难漏。
- App 仅在 `isRemoteClient() && status !== "connected"` 时传入（即
  `App.tsx:200` 已有的 `offline` 判定）。选状态栏的理由：它**无条件渲染**、
  离线时必然在屏上，且与"显示连接状态"同源——状态显示在哪，脱困入口就在哪
  （仓库既有约定，见 `App.tsx` 中"收起动作够不着的地方必须留一条回去的路"）。

### 3.2 重配界面：复用 `PairingScreen`

- `PairingScreen` 新增可选 `onCancel?: () => void`；**传入才渲染**「返回」按钮。
  不传（首启）时 DOM 与现在**完全一致**。
- App 新增 state `repairing: boolean`，渲染优先级：
  1. `needsPairing(isIos())` → 首启全屏配对（**不变**，优先）
  2. 否则 `repairing` → 重配全屏配对（`onCancel` + `initialUrl` = 当前已存 url）
  3. 否则正常主界面
- 重配提交成功后，**必须**按下述方式重建连接（见 §3.5 的坑）。

### 3.3 自动识别：直连 4401

- App 的 `onStatus` 开始读 `s.code`：当
  `isRemoteClient() && s.code === 4401 && status !== "connected"` 时自动进入重配
  界面，并显示"此设备已被桌面移除，请重新配对"（与配对码失效文案区分）。
- **不清配置**；用户点「返回」即回到原离线态继续退避重连。
- 限定离线态，确保绝不在用户正在输入/发消息时抢焦点。
- 中继路径不涉及 4401（§1.1a），故该自动分支只在直连场景生效。

### 3.4 中继"电脑未连接"：接住无 id 错误帧

- `RpcClient` 接住"有 `error`、**无 `id`**、无 `method`"的帧（新增分支，位置在
  `rpc.ts:208` 前后）：
  1. 用它 reject 掉**所有在途请求**（于是 `initialize` 立刻以该错误失败而非悬挂）；
  2. 对该 client 实例**报告一次**"会话级故障"（幂等，第二次忽略）。
- **上报机制（必须新增，现无此出口）**：`RpcClient.onStatus` 目前只是转发
  `transport.onStatus`（`rpc.ts:124-126`），没有自己的状态源。故 `RpcClient`
  改为持有**自己的**状态订阅集合：`onStatus` 同时扇出 transport 的状态与自身的
  合成状态；`dispose` 一并清空。合成状态形状沿用既有约定
  `{ state: "exited", code: -32000 }`（`code` 仅用于显示，判定不依赖它）。
- App 既有的 `exited` 分支（`App.tsx:1385`）随即接管：`dispose` + 退避重连。
  桌面回来后重试自然成功——**复用既有回环，不新增状态机**。
- 判定按**帧结构**（缺 `id`）而非错误码，理由有两条：
  1. **中继不解析 JSON-RPC 语义**（`relay/src/lib.rs` 模块注释），它发出的错误帧
     天然没有 `id`——"缺 `id`"正是中继这一侧的通用形状，不需要把 `-32000` 写死；
  2. app-server 的业务错误码段是 `-32001`~`-32025`、`-32600`+（`protocol.rs:108-146`），
     **并不使用 `-32000`**；测试替身里出现 `-32000` 的地方（`App.test.tsx:1260`、
     `useImageData.test.ts:139`）都是**带 `id`** 的普通响应拒绝。按 `id` 判天然
     不误伤它们，而按错误码判则会把"中继 no-agent"与这些替身错误混为一谈。
- 配套：`handshake` 的 catch 补 `scheduleReconnect()`，否则握手以错误结束时仍永久
  卡住（§1.1b）——不补这条，本节改动在"握手失败"这条路上仍是死的。

### 3.5 重配成功后如何真正重连（必须显式处理）

连接 effect 有 `inited.current` 守卫且只依赖 `[pairingVersion]`
（`App.tsx:1266`、`1524`）。手机**已配对过**时 `inited.current === true`，单纯
`setPairingVersion(v => v + 1)` 会被守卫挡掉、**什么都不会发生**。因此重配成功的
回调必须依次：

1. `inited.current = false`
2. `setRepairing(false)`
3. `setPairingVersion(v => v + 1)`

让旧 effect 走 cleanup、新 effect 重建 client 并重新握手。

## 4. 错误处理与边界

| 情形 | 行为 |
| --- | --- |
| 重配提交失败（码错/过期） | 沿用 `PairingScreen` 既有映射（`INVALID_CODE_TEXT`）；**不动**已存配置 |
| 重配提交失败（网络等） | `CONNECT_FAILED_TEXT`；**不动**已存配置 |
| 重配界面打开期间再收到 4401 / `exited` | 忽略（`repairing` 已 true）——界面不被反复重建 |
| 重配界面点「返回」 | `setRepairing(false)`；**不清配置、不 bump 版本号**；回原离线态 |
| 桌面端（`isRemoteClient()` 恒 false） | `onRepair` 永不传入、4401 分支永不触发 → 行为零变化 |

## 5. 测试策略（先写失败测试）

1. `StatusBar.test.tsx`：传 `onRepair` 才出现按钮；不传时不出现。
2. `PairingScreen.test.tsx`：传 `onCancel` 才出现「返回」并回调；**不传时无按钮、
   既有断言不变**（守首启不变量）。
3. `App.test.tsx`（远程态）：
   - 离线 + `exited code:4401` → 自动出现配对界面与"已被桌面移除"文案；
   - 离线 + 点状态栏「重新配对」→ 出现配对界面且地址预填当前配置；
   - 重配成功后 → 重建 client（走了一次新的 `initialize`）；
   - 点「返回」→ 回主界面且 `localStorage` 配置**未被清除**；
   - 桌面态：以上分支一个都不触发。
4. `rpc.test.ts`：无 id + 有 error 的帧 → 在途请求被 reject **且** `onStatus`
   收到一次合成 `exited`（第二次再收到同类帧不重复上报，幂等）；
   `dispose` 后不再上报。带 `id` 的 `-32000` 响应拒绝**不受影响**（在途请求照常
   reject 为该错误、**不**产生合成 `exited`，也不影响其它在途请求）。
   同时断言 `onStatus` 仍照常扇出 transport 自身的状态（既有语义不回归）。
5. `App.test.tsx`：握手以错误结束（`initialize` reject）→ 排了重连。

**验收命令**：`cd desktop && npx tsc --noEmit && npx vitest run`（基线 1047 项全绿，
新增用例只增不改既有断言）。

## 6. 影响面

- 改动文件：`desktop/src/components/StatusBar.tsx`、
  `desktop/src/components/PairingScreen.tsx`、`desktop/src/App.tsx`、
  `desktop/src/lib/rpc.ts`，以及对应测试。
- Rust 零改动；`wsTransport.ts` 零改动（4401 已随 `exited` 上报）。