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
