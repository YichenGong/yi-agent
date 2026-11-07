# iOS 扫码识别后黑屏：实施验证记录

- 日期：2026-11-07
- 关联：[spec](../specs/2026-11-07-ios-qr-recognition-black-screen-design.md) · [实施 plan](../plans/2026-11-07-ios-qr-recognition-black-screen-impl.md)
- 用途：本文件是五个任务的**持久证据**，按任务追加；每条都记录真实命令与真实输出摘要。
- 证据边界：全部为 jsdom + 注入 `deps` 的行为测试，**不含**真机相机、WebKit 像素或 iOS 打包验收；不记录任何真实配对码、token 或含 session 的完整 URL。

## 环境

```sh
export PATH="/opt/homebrew/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
```

基线（任务开始前）：`(cd desktop && npm test -- --environment jsdom src/lib/qrScanner.test.ts src/components/PairingScreen.test.tsx)` = **29 passed**（2 文件）。

---

## Task 1：扫描结果接收与停机隔离

改动文件：

- `desktop/src/lib/qrScanner.ts`
- `desktop/src/lib/qrScanner.test.ts`
- `docs/project-management/desktop.md`（仅补代码门禁阶段的证据链接，条目仍 `[ ]`）
- 本文件

### 1.1 实现要点

- 新增导出类型 `ScanResultHandler = (text: string) => boolean | void`，`startCameraScan` 的 `onResult` 改用该类型（生产 API 其余签名不变，仍 `deps` 可选、`stop()` 幂等）。
- 帧回调里 `onResult(text) !== false` 才 `stop()`；返回 `false` 时循环、轨道、`srcObject` 全部保持，下一帧继续扫。
- 新增唯一失败出口 `fail(error)`：`stopped` 时直接丢弃，否则先 `stop()` 再 `onError(error)` 一次。取流链尾、`play()` 的 reject、结果回调同步抛错三处都收敛到它。
- `stop()` 释放轨道后，仅当 `video.srcObject === stream` 时置空，避免本实例的晚到清理抹掉别的实例接上的流。
- 保留 `SCAN_INTERVAL_MS = 200`、默认取流/画布/jsQR 路径、单帧 decode 抛错继续的既有行为；未新增运行时依赖。

### 1.2 RED（先写测试，未改实现）

```sh
cd desktop && npm test -- --environment jsdom src/lib/qrScanner.test.ts
```

输出（节选，真实）：

```
 ✓ startCameraScan > A09 releases late streams without assigning video 1ms
 × startCameraScan > A10/A17 get-stop-reject errors=0 1ms
   → expected "spy" to be called +0 times, but got 1 times
 × startCameraScan > A10/A17 play-stop-reject errors=0 1ms
   → expected "spy" to be called +0 times, but got 1 times
 × startCameraScan > A10/A17 play-accept-reject errors=0 1ms
   → expected "spy" to be called +0 times, but got 1 times
 × startCameraScan > A10/A17 result-sync-throw errors=1 2ms
   → result failed
 Tests  5 failed | 11 passed (16)
```

失败点与预期原因：

| 用例 | 失败断言 | 旧实现的真实缺陷 |
|---|---|---|
| A06 | `expected "spy" to not be called at all, but actually been called 1 times`（`trackStop`，`qrScanner.test.ts:161`） | 无论回调返回什么都 `stop()` 后交付，返回 `false` 的帧也会停流 |
| A10 `get-stop-reject` | `error` 被调用 1 次（期望 0） | `stop()` 后晚到的取流 rejection 仍走末尾 `.catch(stop+onError)` |
| A10 `play-stop-reject` | 同上 | `stop()` 后晚到的 `play()` rejection 仍 `onError` |
| A10 `play-accept-reject` | 同上 | 接受结果并 `stop()` 后，同一实例的 `play()` rejection 仍上报 |
| A17 `result-sync-throw` | `Error: result failed` 从 `qrScanner.ts:133` 逃逸 | 结果回调同步抛错没有被接住，会从 interval 抛到全局 |

A09（stop 后迟到取流）在旧实现里已通过：原代码在 `.then` 里已有 `stopped` 早退并停掉迟到轨道，本次只是把它钉成回归（并要求 `srcObject` 不被赋值）。5 条红全部是行为断言失败，没有一条是 import/helper 缺失。

### 1.3 GREEN（实现后）

```sh
cd desktop && npm test -- --environment jsdom src/lib/qrScanner.test.ts
```

```
 ✓ src/lib/qrScanner.test.ts (16 tests) 15ms
 Test Files  1 passed (1)
      Tests  16 passed (16)
```

回归基线（既有 29 例 + 本任务 9 例新增）：

```sh
cd desktop && npm test -- --environment jsdom src/lib/qrScanner.test.ts src/components/PairingScreen.test.tsx
```

```
 ✓ src/lib/qrScanner.test.ts (16 tests) 9ms
 ✓ src/components/PairingScreen.test.tsx (22 tests) 236ms
 Test Files  2 passed (2)
      Tests  38 passed (38)
```

类型检查：

```sh
cd desktop && npx tsc --noEmit
```

输出为空，退出码 0。

### 1.4 资源与隔离断言（本任务落地的部分）

- A06：第一次 `false` 后 `trackStop` 0 次、循环仍在；第二次 `true` 后 `trackStop` 1 次；再推进 600ms `decode` 计数不变；重复 `stop()` 不重复停轨道。
- A09：`stop()` 后迟到的流每条轨道停 1 次，`video.srcObject` 保持原值，`play()` 未被调用，结果与错误回调均为 0 次。
- A10/A17 表驱动：stop 后的 get/play rejection 与接受后的 play rejection 均 0 次 `onError`；当前实例的 get/play 失败、play 同步抛错、结果同步抛错均恰好 1 次；停机后 `decode` 计数不再增长。

后续任务（QrScanner 稳定回调、PairingScreen 覆盖层、App 接线、真机验收）的证据请追加在本节之后。

---

## Task 2：稳定真实相机组件与 ScanHost 契约

改动文件：

- `desktop/src/components/QrScanner.tsx`
- `desktop/src/components/QrScanner.test.tsx`（新建）
- `desktop/src/components/PairingScreen.tsx`（仅类型与注释，ScanHost 转发逻辑未改）
- `docs/project-management/desktop.md`（仅补代码门禁阶段证据，条目仍 `[ ]`）
- 本文件

### 2.1 实现要点

- `QrScannerProps.onText` 改为 `ScanResultHandler`。组件把 `{ onText, onError }` 放进 `latest` ref 并每次渲染刷新，effect 内只传 `(text) => latest.current.onText(text)` / `(error) => latest.current.onError(error)`，依赖数组从 `[onText, onError, deps]` 收成 `[deps]`。
- 结果：父组件重渲染产生的**新闭包**不再触发 `stop()` + 重新取流；同一实例始终调用最新回调（A05 验证第二次渲染后旧回调 0 次、新回调收到命中文本）。
- 旧实例的异步失败仍由 Task 1 的实例内 `stopped` 隔离：effect 换 `deps` 时先 `stop()` 旧实例；旧流/旧 `play()` 晚到不会覆盖 `video.srcObject`、不调 `onError`（A11 两条）。
- `PairingScreenProps.scan` 的 `onText` 与私有 `ScanHost.onText` 改为 `ScanResultHandler`；**ScanHost 的 `(t) => ref.current.onText(t)` 转发与 `useEffect(..., [scan])` 原样保留，没有加入 `active` 终止规则**——该规则与 Task 3 的 boolean handler 原子交付。

### 2.2 RED（新测试，未改实现）

```sh
cd desktop && npm test -- --environment jsdom src/components/QrScanner.test.tsx
```

输出（真实）：

```
 ❯ src/components/QrScanner.test.tsx (3 tests | 1 failed) 19ms
   × A05 callback refresh does not reopen camera 12ms
     → expected "spy" to be called 1 times, but got 2 times
   ✓ A11 old stream cannot overwrite the new scan 4ms
   ✓ A11 old play cannot overwrite the new scan 3ms
 Test Files  1 failed (1)
      Tests  1 failed | 2 passed (3)
```

A05 红在 `expect(getUserMedia).toHaveBeenCalledTimes(1)`（实际 2）：旧 effect 依赖 `[onText, onError, deps]`，`rerender` 换了 `onText` identity 就停机重开相机。失败是行为断言，不是 import/helper 缺失。

同一时刻用一个临时探针（未提交、运行后删除）量到旧实现的另一半症状：

```
SCRATCH getUserMedia= 2 trackStop= 1
```

即旧 effect 在回调刷新时**提前停掉了仍在使用的轨道**，随后才取第二路流。

A11 两条在旧实现上已通过：Task 1 的 `stopped` 早退与 `srcObject` 归属检查已经覆盖了「停后的旧流/旧播放失败」，本任务只是把它钉成组件层回归（用真实 `QrScanner`，不 mock `startCameraScan`）。

### 2.3 GREEN（实现后）

```sh
cd desktop && npm test -- --environment jsdom src/components/QrScanner.test.tsx
```

```
 ✓ src/components/QrScanner.test.tsx (3 tests) 18ms
 Test Files  1 passed (1)
      Tests  3 passed (3)
```

定向回归（Task 1 基线 + 本任务）：

```sh
cd desktop && npm test -- --environment jsdom src/components/QrScanner.test.tsx src/lib/qrScanner.test.ts src/components/PairingScreen.test.tsx
```

```
 ✓ src/lib/qrScanner.test.ts (16 tests) 8ms
 ✓ src/components/QrScanner.test.tsx (3 tests) 18ms
 ✓ src/components/PairingScreen.test.tsx (22 tests) 225ms
 Test Files  3 passed (3)
      Tests  41 passed (41)
```

类型检查：

```sh
cd desktop && npx tsc --noEmit
```

输出为空，退出码 0：旧 `(t: string) => void` 的 `scan` 接缝与被删掉的 `onText` 依赖都仍编译通过。

### 2.4 接缝兼容

- 既有 `PairingScreen` 测试（启动异常回表单不黑屏、安全区 `.scan-overlay-bar`、手输回退、取消、覆盖层内错误提示）全部保留且通过，本次一条未改、未删。
- `ScanHost` 仍无终止规则：`useEffect(() => scan((t) => ref.current.onText(t), (e) => ref.current.onError(e)), [scan])` 与改动前逐字节一致，仅 `onText` 的类型从内联函数类型换成 `ScanResultHandler`。

## Task 3：同步配对准入、可见进度与离开失效

改动文件：

- `desktop/src/components/PairingScreen.tsx`
- `desktop/src/components/PairingScreen.test.tsx`
- `docs/project-management/desktop.md`（补代码门禁证据，条目仍 `[ ]`）
- 本文件

### 3.1 实现要点

- **同步准入**：`runRedeem(u, c, origin): boolean` 首行检查 `mounted` / `attempt`，
  然后立刻写入 `attempt.current = { active: true }`——在任何 `setState`、外部调用、
  `await` 之前。React 的 `busy` 要到下一次渲染才可见，仅靠它挡不住同一批次内的
  第二次提交（连点提交、扫码帧连发都会各发一次兑换）。
- **识别即离开相机**：`runRedeem` 在 `await` 之前 `scanActive=false; setScanning(false)`，
  覆盖层条件改为 `scanning && !busy`。此前覆盖层要等 `finally` 里 `setScanning(false)`
  才消失，兑换 pending 期间一直全屏黑——即用户报的「扫上码之后黑屏」。
- **晚到结果隔离**：`live()` 同时校验 `mounted`、本次 `active`、以及
  `attempt.current === current`。resolve/reject 晚到只丢弃，不落盘、不 `onPaired`、
  不写 UI。`invalidate()` 在卸载与「返回」时置 `active=false` 并清空 `attempt`。
- **保存与通知同块**：`saveRemoteConfig` 之后紧跟 `if (live()) onPaired?.()`，中间无
  `await`，不产生「已落盘但页面已离开」的窗口。
- **扫码结果契约**：`handleScanText` 在非 mounted / 非 scanActive / 已有 attempt 时返回
  `true`（本次帧被接管或忽略，循环停机）；非法码同步返回 `false` 让循环继续；有效码
  `runRedeem` 后返回 `true`。`handleScanError` 同样三重守卫，只处理第一次。
- **ScanHost 终止规则**（与 boolean handler 原子交付）：`active` 局部变量在 effect 内
  维护，`accepted !== false` 或抛错后不再转发；cleanup 先置 `active=false` 再 `stop()`。
- **输入与出口**：三个输入 `disabled={busy}`，`onChange` 也带 mounted/attempt 守卫；
  submit 同步 `preventDefault` + 三重闸；「返回」先 `invalidate()`、`mounted=false`，
  再 `onCancel()`，不清已有 storage。
- **进度可见**：`{busy && <p role="status" aria-label="配对进度">}`，扫码来源文案
  `已识别二维码，正在连接桌面…`，手输文案 `正在连接桌面…`；有 `onCancel` 且 busy 时
  额外提示 `返回不会撤销已发出的配对请求`。

### 3.2 RED（新测试，未改实现）

```sh
cd desktop && npm test -- --environment jsdom src/components/PairingScreen.test.tsx
```

```
 Failed Tests 9
 FAIL  A01 accepted QR removes overlay while redeem is pending
 FAIL  A02 synchronous manual submit admission survives stale busy closure
 FAIL  A02/A18 first accepted code wins and later frames are not forwarded
 FAIL  A15 manual pending shows its own progress text and no cancel reminder by default
 FAIL  A16 return-without-unmount ignores late resolve
 FAIL  A16 return-without-unmount ignores late reject
 FAIL  A16 unmount-new-page ignores late resolve
 FAIL  A16 unmount-new-page ignores late reject
 FAIL  A18 stale handlers after returning cannot touch the reopened scanner
      Tests  9 failed | 27 passed (36)
```

A01 红在「覆盖层残留」这一用户可见断言（不是先被 `undefined` 返回值截断）；
A02 红在 `redeem` 被同一批次调用 2 次；A16 四个分支全部独立红；A18 红在旧 handler
返回 `undefined` 且会串到新实例。

### 3.3 GREEN

```sh
cd desktop && npm test -- --environment jsdom src/components/PairingScreen.test.tsx
```

```
 Test Files  1 passed (1)
      Tests  36 passed (36)
```

定向三文件：

```sh
cd desktop && npm test -- --environment jsdom \
  src/lib/qrScanner.test.ts src/components/QrScanner.test.tsx src/components/PairingScreen.test.tsx
```

```
 Test Files  3 passed (3)
      Tests  55 passed (55)
```

前端全量回归：`cd desktop && npm test` → **173 files / 2416 tests passed**。
类型检查：`npx tsc --noEmit` — 无输出，exit 0。

### 3.4 门禁

- `cd yi-agent-rs && cargo fmt --all` 已执行。
- 除 `test` 外的全部 `just ci` 门禁 → **rc=0**（日志 `/tmp/ios-qr-task3-gates.log`）。
- 完整 `just ci` 仍被既有基线测试
  `yi-agent-app-server server::tests::transport::agent_factory_failure_returns_internal_error`
  阻塞；该测试在未含本改动的 main 上同样稳定失败（3/3），与本任务无关，如实记录。

### 3.5 覆盖的验收项

A01、A02、A03、A04、A15、A16（四个分支）、A18；既有 A12 启动错误、ErrorBoundary
就地收敛、安全区、桌面手输、返回目标断言全部保留并通过。

## Task 4：真实相机集成与宿主回归

改动文件（仅测试）：

- `desktop/src/components/PairingScreen.test.tsx`
- `desktop/src/App.test.tsx`
- `docs/project-management/desktop.md`（补证据，条目仍 `[ ]`）
- 本文件

无生产代码改动（`App.tsx` 未动）。

### 4.1 真实相机集成（A07/A08/A14）

新增独立 describe `real camera integration`：使用真实 `PairingScreen` + 真实
`QrScanner` + 真实 `startCameraScan`，只替换四个外界边界——`navigator.mediaDevices.getUserMedia`、
`HTMLMediaElement.play`、`canvas.getContext`（含 `videoWidth/videoHeight`）、`jsQR`。
不 mock `QrScanner`/`startCameraScan`/`PairingScreen`，也不传 `scan` 接缝。

- A07/A14（`strict=false` / `strict=true`）：进入扫码 → 父组件真实重渲染（新元素引用
  + 新 `notice`）→ jsQR 命中合法载荷。断言 `redeem` 恰 1 次、`getUserMedia` 调用数与
  StrictMode 收敛后的 baseline 相同（不重开相机）、dialog 与 `相机预览` 都消失、
  `配对进度` 文案为 `已识别二维码，正在连接桌面…`、每个 stream 的 track 恰停一次。
- A08（两种 strict）：先送无效码 → dialog 内可见 `不是有效的配对二维码`，**存活流未被
  stop**、无新增取流、`redeem` 为 0；再送合法码 → `redeem` 1 次、无新增取流、
  dialog 消失、无 alert、所有 track 各停一次。
- 媒体 mock 用 `Object.getOwnPropertyDescriptor` + `defineProperty(configurable)` 安装，
  `afterEach` 先 `cleanup()` 再恢复 descriptor 与 mocks/timers。

### 4.2 判别性验证（mutation）

临时把 `QrScanner` effect 回退为旧写法（`startCameraScan(video, onText, onError, deps)`，
依赖 `[onText, onError, deps]`），重跑真实相机测试：

```
 Failed Tests 4
 FAIL  A07/A14 pending recognition leaves the camera strict=false
 FAIL  A07/A14 pending recognition leaves the camera strict=true
 FAIL  A08 invalid then valid stays in one live stream strict=false
 FAIL  A08 invalid then valid stays in one live stream strict=true
      Tests  4 failed | 36 passed (40)
```

恢复后 40/40 通过。证明这些用例确实在判别「回调刷新重开相机」，不是恒绿。

### 4.3 宿主回归（A13/A16）

在 `App mobile repair path` 内新增：

- A16（`resolve`/`reject` 各一例）：已配对手机进重配页提交 → 进度节点出现 → 点「返回」
  后晚到的 JSON-RPC 回包被忽略：`localStorage` 保持原字符串、`clients.length` 仍为 1、
  配对表单不重现、无渲染错误。直接在 `App` 层验证「宿主未卸载时由返回 handler 自身失效」。
- A13：重配页提交后服务端回 `-32001` → 显示 `配对码无效或已过期，请在桌面重新生成`、
  进度节点消失、原有 `yi-agent.remote` 字符串不变、`clients.length` 仍为 1。
- 原有首启成功兑换、失败不落盘、重配成功后重建连接等用例全部保留并通过。

### 4.4 结果

```
cd desktop && npm test -- --environment jsdom src/components/PairingScreen.test.tsx
  → 40 passed
cd desktop && npm test -- --environment jsdom src/App.test.tsx
  → 279 passed
cd desktop && npm test
  → 173 files / 2423 tests passed
cd desktop && npx tsc --noEmit  → 无输出，exit 0
```

- `cargo fmt --all` 已执行。
- 除 `test` 外的全部 `just ci` 门禁 → **rc=0**（`/tmp/ios-qr-task4-gates.log`）。
- 完整 `just ci` 仍被既有基线测试
  `yi-agent-app-server server::tests::transport::agent_factory_failure_returns_internal_error`
  阻塞（main 上同样 3/3 失败），与本任务无关。
