# iOS 二维码识别后黑屏修复 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复 iOS 已显示相机画面、识别有效配对二维码后停流但黑色覆盖层残留及重复取流的问题，并保持无效码连续扫描和既有配对协议。

**Architecture:** 扫描循环用同步接收返回值决定是否停机，实例局部 stopped 隔离旧异步结果；真实相机组件通过 ref 转发最新回调而不重启 effect。配对页用同步 attempt ref 准入，在网络等待前关闭扫码层并显示进度，返回/卸载使迟到兑换结果失效。不引入全局状态机，不改 App 生产逻辑。

**Tech Stack:** React 19、TypeScript strict、Vitest 3、Testing Library、jsdom、jsQR、Tauri 2 / iOS WKWebView；沿用仓库 lockfile，不新增依赖。

## Global Constraints

- 依据：[修复 spec](../specs/2026-11-07-ios-qr-recognition-black-screen-design.md)；[spec 审查记录](../reports/2026-11-07-ios-qr-recognition-spec-review.md)；[plan 审查记录](../reports/2026-11-07-ios-qr-recognition-plan-review.md)。本轮计划审查基线 `c37189b3`，扫码生产代码未变；执行时核对文件变化，不盲用旧行号。
- 本文是待执行计划，所有 checkbox 初始未完成；文中命令、修复测试、回退测试和真机验收均不是本轮已执行证据。
- `ScanResultHandler = (text: string) => boolean | void`：false 继续，true/undefined 接受并停止；同步契约，不接受 Promise。
- 保持 `SCAN_INTERVAL_MS = 200`、现有默认取流/画布/jsQR 路径。生产不传 `deps`；测试保持 deps 引用稳定，换引用才能重建实例。
- 扫码进度：“已识别二维码，正在连接桌面…”；手输进度：“正在连接桌面…”；进度 `role="status" aria-label="配对进度"`，结束卸载节点。
- 原错误文案 `NOT_A_PAIRING_QR_TEXT`、`CAMERA_UNAVAILABLE_TEXT`、`INVALID_CODE_TEXT`、`CONNECT_FAILED_TEXT` 不变。返回提醒：“返回不会撤销已发出的配对请求”，只在有 onCancel 且 busy 时显示。
- 不强制立即 resolve/reject 渲染进度帧，不用 flushSync、真实 sleep 或人工最短等待。
- 不修改 URI、token/scope/存储格式、defaultRedeem 中继/直连选择。中继现有 timeout 15000ms，直连无应用层 deadline；不增加取消协议、自动重试或原生插件。
- 保留局部 ErrorBoundary、安全区、≥44px 返回目标；桌面默认无扫码入口，手输仍可用。
- 默认兑换失效后不落盘、不调用 onPaired；onSubmit 自有外部副作用不能撤销，不扩展其签名。
- 不记录真实配对码、token、完整含 session 的 URL；真机未通过 I01–I07 时 bug/模块项保持 `[ ]`。
- 全部代码变更走新 worktree；每个代码提交前 `cargo fmt --all` 和 `just ci`，合并后再跑 `just ci`；desktop 测试/构建另跑，不由 Rust CI 替代。

## 文件与依赖顺序

| 文件 | 职责/动作 |
|---|---|
| `desktop/src/lib/qrScanner.ts` | 修改：唯一结果类型、接受停机、幂等清理与迟到错误隔离 |
| `desktop/src/lib/qrScanner.test.ts` | 修改：纯循环的接收/错误/资源回归 |
| `desktop/src/components/QrScanner.tsx` | 修改：ref 最新回调、仅 deps 引用触发 effect |
| `desktop/src/components/QrScanner.test.tsx` | 新建：真实扫描循环的组件生命周期测试 |
| `desktop/src/components/PairingScreen.tsx` | 修改：同步 attempt、扫描回调失效、可见进度、返回；ScanHost 透传契约 |
| `desktop/src/components/PairingScreen.test.tsx` | 修改：接缝状态测试与独立 describe 内的真实相机集成 |
| `desktop/src/App.test.tsx` | 修改：首启/重配宿主回归，不拿 mock RpcClient 代替相机测试 |
| `docs/bug-list.md`、`docs/project-management/desktop.md`、`docs/project-management/README.md` | 同步代码门禁/真机状态、证据链接与计数 |
| `docs/superpowers/reports/2026-11-07-ios-qr-recognition-implementation-verification.md` | 实施时新建：红绿/回退/门禁/真机去敏证据 |

任务 1 → 2 → 3 → 4 → 5 顺序执行，避免多个写代理同时修改 PairingScreen 文件。每个任务经测试及独立审查再交下一项。Task 2 只同步 ScanHost 类型与现有 ref 转发，**不提前添加 active 终止规则**；Task 3 在 handler 返回 false/true 的同一提交中加入终止规则，避免旧 void 无效码回调被误当作已接收。

## 执行准备

- [ ] 加载 using-git-worktrees，确认 main 上用户修改不纳入本任务，检查目标分支不存在后执行：

```sh
git worktree add .worktrees/fix-ios-qr-recognition -b fix/ios-qr-recognition
cd .worktrees/fix-ios-qr-recognition
export PATH="/opt/homebrew/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
(cd desktop && npm ci && npm test -- --environment jsdom src/lib/qrScanner.test.ts src/components/PairingScreen.test.tsx)
```

预期旧测试通过（调查基线曾是 29 tests），数量以执行输出为准；若失败，先 systematic-debugging，不将基线失败算修复引入。所有后文命令从该 worktree 根执行。

## Task 1：扫描结果接收与停机隔离

**Files:** Modify `desktop/src/lib/qrScanner.ts:startCameraScan` 与 `desktop/src/lib/qrScanner.test.ts`。

**Interfaces:**
- Consumes：既有 `ScanDeps`、`startCameraScan(video, onResult, onError, deps?) => () => void`。
- Produces：导出 `ScanResultHandler`；startCameraScan 的 onResult 使用该类型。stop 幂等，false 不停机，当前失败仅报告一次，旧失败零报告；不新增外部函数。

- [ ] **Step 1：加入失败用例 A06/A09/A10/A17。** 在现有 fake timers describe 内添加如下接收测试（复用既有 fakeVideo/frame）：

```ts
it("A06 keeps rejected text alive and stops on acceptance", async () => {
  const trackStop = vi.fn();
  const stream = { getTracks: () => [{ stop: trackStop }] } as unknown as MediaStream;
  const result = vi.fn< (text: string) => boolean | void >()
    .mockReturnValueOnce(false).mockReturnValueOnce(true);
  const decode = vi.fn(() => "text");
  const stop = startCameraScan(fakeVideo(), result, vi.fn(), {
    getUserMedia: async () => stream, grabFrame: () => frame, decode,
  });
  await vi.advanceTimersByTimeAsync(200);
  expect(result).toHaveBeenCalledTimes(1);
  expect(trackStop).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(200);
  expect(result).toHaveBeenCalledTimes(2);
  expect(trackStop).toHaveBeenCalledTimes(1);
  await vi.advanceTimersByTimeAsync(600);
  expect(decode).toHaveBeenCalledTimes(2);
  stop();
  expect(trackStop).toHaveBeenCalledTimes(1);
});
```

在该文件声明局部可控 Promise（后续组件文件各自声明，不创建共享生产工具）：

```ts
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((res, rej) => { resolve = res; reject = rej; });
  return { promise, resolve, reject };
}

it("A09 releases late streams without assigning video", async () => {
  const pending = deferred<MediaStream>();
  const stops = [vi.fn(), vi.fn()];
  const stream = { getTracks: () => stops.map(stop => ({ stop })) } as unknown as MediaStream;
  const video = fakeVideo();
  const before = video.srcObject;
  const result = vi.fn();
  const error = vi.fn();
  const stop = startCameraScan(video, result, error, { getUserMedia: () => pending.promise });
  stop();
  pending.resolve(stream);
  await vi.advanceTimersByTimeAsync(400);
  for (const s of stops) expect(s).toHaveBeenCalledTimes(1);
  expect(video.srcObject).toBe(before);
  expect(video.play).not.toHaveBeenCalled();
  expect(result).not.toHaveBeenCalled();
  expect(error).not.toHaveBeenCalled();
});
```

追加当前/迟到错误的表驱动测试，复用本任务的 deferred、frame、fakeVideo：

```ts
it.each([
  ["get-stop-reject", 0], ["play-stop-reject", 0], ["play-accept-reject", 0],
  ["get-current-reject", 1], ["play-current-reject", 1],
  ["play-sync-throw", 1], ["result-sync-throw", 1],
] as const)("A10/A17 %s errors=%s", async (scenario, expectedErrors) => {
  const pendingStream = deferred<MediaStream>();
  const pendingPlay = deferred<void>();
  const trackStop = vi.fn();
  const stream = { getTracks: () => [{ stop: trackStop }] } as unknown as MediaStream;
  const error = vi.fn();
  const decode = vi.fn(() => "hit");
  const result = vi.fn(() => {
    if (scenario === "result-sync-throw") throw new Error("result failed");
    return scenario === "play-accept-reject";
  });
  const video = fakeVideo();
  video.play = vi.fn(() => {
    if (scenario === "play-sync-throw") throw new Error("play failed");
    return pendingPlay.promise;
  });
  const usesPendingStream = scenario.startsWith("get-");
  const stop = startCameraScan(video, result, error, {
    getUserMedia: () => usesPendingStream ? pendingStream.promise : Promise.resolve(stream),
    grabFrame: () => frame, decode,
  });
  await vi.advanceTimersByTimeAsync(0);
  if (scenario === "get-stop-reject") { stop(); pendingStream.reject(new Error("old get")); }
  if (scenario === "get-current-reject") pendingStream.reject(new Error("current get"));
  if (scenario === "play-stop-reject") { stop(); pendingPlay.reject(new Error("old play")); }
  if (scenario === "play-current-reject") pendingPlay.reject(new Error("current play"));
  if (scenario === "play-accept-reject") {
    await vi.advanceTimersByTimeAsync(200);
    expect(result).toHaveBeenCalledTimes(1);
    pendingPlay.reject(new Error("accepted scan play"));
  }
  await vi.advanceTimersByTimeAsync(400);
  expect(error).toHaveBeenCalledTimes(expectedErrors);
  expect(trackStop).toHaveBeenCalledTimes(usesPendingStream ? 0 : 1);
  const calls = decode.mock.calls.length;
  await vi.advanceTimersByTimeAsync(600);
  expect(decode).toHaveBeenCalledTimes(calls);
  stop();
});
```

保留已有 void 命中即停及 decoder 单帧抛错继续的测试。所有新循环在正常结束时 stop，失败用例的 fake timers 在 afterEach 恢复；不把 grabFrame 异常误称为现有 decode 容错。

- [ ] **Step 2：运行并确认红。**

```sh
(cd desktop && npm test -- --environment jsdom src/lib/qrScanner.test.ts)
```

预期 A06 第一次返回 false 后仍被旧实现 stop；A10 旧 rejection 会通知 error。红必须来自行为，不得仅因 helper/import 缺失。

- [ ] **Step 3：最小实现。** 保留默认 API，在模块导出类型；替换循环内部失败和接收分支：

```ts
export type ScanResultHandler = (text: string) => boolean | void;
// startCameraScan 参数 onResult: ScanResultHandler
const stop = () => {
  if (stopped) return;
  stopped = true;
  if (timer !== null) { clearInterval(timer); timer = null; }
  stream?.getTracks().forEach(t => t.stop());
  if (stream && video.srcObject === stream) video.srcObject = null;
};
const fail = (error: unknown) => {
  if (stopped) return;
  stop();
  onError(error);
};
// requestStream().then 内：stopped 时只停 s；否则赋 stream/video，play 同步调用
// 位于现有 Promise 链内，同步 throw 由外层 catch(fail) 收敛。
Promise.resolve(video.play()).catch(fail);
// interval 保留 grabFrame 与 decode 单帧容错的现有路径；text 分支改成：
if (text) {
  try {
    if (onResult(text) !== false) stop();
  } catch (error) {
    fail(error);
  }
}
// requestStream 链末尾改成 .catch(fail)，不能 stop() 后再无条件 onError。
```

在 interval 的回调前/后维持 stopped 检查，stop 先失效。此任务不扩大到新网络策略；修改注释为“接受即停”，不保留“任何文本都停止”的旧文档。

- [ ] **Step 4：运行绿与类型检查。** 定向命令同 Step 2；另 `(cd desktop && npx tsc --noEmit)`。结果必须含 A06/A09/A10/A17，不能跳过新测试。
- [ ] **Step 5：记录与提交。** 新建实施验证报告，记录测试命令、行为红/绿、资源断言，无真实码。更新 desktop 未完成项的代码门禁阶段链接但仍 `[ ]`；运行项目提交门禁：

```sh
(cd yi-agent-rs && cargo fmt --all && just ci)
git add desktop/src/lib/qrScanner.ts desktop/src/lib/qrScanner.test.ts docs/superpowers/reports/2026-11-07-ios-qr-recognition-implementation-verification.md docs/project-management/desktop.md
git commit -m "fix: isolate QR scan acceptance and late failures"
```

## Task 2：稳定真实相机组件与 ScanHost 契约

**Files:** Modify `desktop/src/components/QrScanner.tsx`、`desktop/src/components/PairingScreen.tsx:PairingScreenProps.scan/ScanHost`；Create `desktop/src/components/QrScanner.test.tsx`；Test `desktop/src/components/PairingScreen.test.tsx`。

**Interfaces:**
- Consumes：Task 1 的 `ScanResultHandler`、`ScanDeps`、stop。
- Produces：`QrScannerProps.onText: ScanResultHandler`；`PairingScreenProps.scan?: (onText: ScanResultHandler, onError: (e: unknown) => void) => () => void`。ScanHost 私有，不为测试新增导出；此任务只更新类型与透传，首次终止规则由 Task 3 与 boolean handler 原子交付。

- [ ] **Step 1：写失败测试 A05/A11。** 新文件用 jsdom 文件注解，导入 React、Testing Library、Vitest、真实 QrScanner，禁止 mock startCameraScan：

```tsx
/** @vitest-environment jsdom */
import { afterEach, expect, it, vi } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { QrScanner } from "./QrScanner";
import type { ScanDeps } from "../lib/qrScanner";
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); });
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((res, rej) => { resolve = res; reject = rej; });
  return { promise, resolve, reject };
}
it("A05 callback refresh does not reopen camera", async () => {
  vi.useFakeTimers();
  vi.spyOn(HTMLMediaElement.prototype, "play").mockResolvedValue(undefined);
  const trackStop = vi.fn();
  const stream = { getTracks: () => [{ stop: trackStop }] } as unknown as MediaStream;
  const getUserMedia = vi.fn(async () => stream);
  const decode = vi.fn<() => string | null>(() => null);
  const deps: ScanDeps = {
    getUserMedia, decode,
    grabFrame: () => ({ data: new Uint8ClampedArray(4), width: 1, height: 1 }) as ImageData,
  };
  const first = vi.fn(() => false);
  const latest = vi.fn(() => true);
  const view = render(<QrScanner deps={deps} onText={first} onError={vi.fn()} />);
  await vi.advanceTimersByTimeAsync(0);
  view.rerender(<QrScanner deps={deps} onText={latest} onError={vi.fn()} />);
  await vi.advanceTimersByTimeAsync(0);
  expect(getUserMedia).toHaveBeenCalledTimes(1);
  expect(trackStop).not.toHaveBeenCalled();
  decode.mockReturnValue("hit");
  await vi.advanceTimersByTimeAsync(200);
  expect(first).not.toHaveBeenCalled();
  expect(latest).toHaveBeenCalledWith("hit");
  view.unmount();
  expect(trackStop).toHaveBeenCalledTimes(1);
});
```

A11 在同一新文件追加，不需要修改生产导出：

```tsx
it.each(["stream", "play"] as const)("A11 old %s cannot overwrite the new scan", async phase => {
  vi.useFakeTimers();
  const oldStream = deferred<MediaStream>();
  const oldPlay = deferred<void>();
  const oldStop = vi.fn();
  const newStop = vi.fn();
  const oldMedia = { getTracks: () => [{ stop: oldStop }] } as unknown as MediaStream;
  const newMedia = { getTracks: () => [{ stop: newStop }] } as unknown as MediaStream;
  const play = vi.spyOn(HTMLMediaElement.prototype, "play");
  if (phase === "play") play.mockReturnValueOnce(oldPlay.promise);
  play.mockResolvedValue(undefined);
  const oldDeps: ScanDeps = { getUserMedia: () => phase === "stream" ? oldStream.promise : Promise.resolve(oldMedia), grabFrame: () => null };
  const newDeps: ScanDeps = { getUserMedia: async () => newMedia, grabFrame: () => null };
  const oldError = vi.fn();
  const newError = vi.fn();
  const view = render(<QrScanner deps={oldDeps} onText={() => false} onError={oldError} />);
  await vi.advanceTimersByTimeAsync(0);
  view.rerender(<QrScanner deps={newDeps} onText={() => false} onError={newError} />);
  await vi.advanceTimersByTimeAsync(0);
  const video = view.container.querySelector("video")!;
  expect(video.srcObject).toBe(newMedia);
  if (phase === "stream") oldStream.resolve(oldMedia);
  else oldPlay.reject(new Error("old play failed"));
  await vi.advanceTimersByTimeAsync(0);
  expect(video.srcObject).toBe(newMedia);
  expect(oldError).not.toHaveBeenCalled();
  expect(newError).not.toHaveBeenCalled();
  expect(oldStop).toHaveBeenCalledTimes(1);
  expect(newStop).not.toHaveBeenCalled();
  view.unmount();
  expect(newStop).toHaveBeenCalledTimes(1);
});
```

- [ ] **Step 2：运行确认红。** `(cd desktop && npm test -- --environment jsdom src/components/QrScanner.test.tsx)`；旧回调 effect 使取流=2、旧 track 被提前停止。
- [ ] **Step 3：实现 ref 转发与接缝终止规则。**

```tsx
// QrScanner：导入 ScanResultHandler，修改 onText 类型。
const latest = useRef({ onText, onError });
latest.current = { onText, onError };
useEffect(() => {
  const video = videoRef.current;
  if (!video) return;
  return startCameraScan(video, text => latest.current.onText(text),
    error => latest.current.onError(error), deps);
}, [deps]);
// PairingScreen：导入同一结果类型，更新 scan 和 ScanHost.onText。
// 此时保留 ScanHost 既有转发，不加入 active；旧 handler 仍返回 void。
useEffect(() => scan(
  text => ref.current.onText(text),
  error => ref.current.onError(error),
), [scan]);
```

此任务不改变 ScanHost 对无效码/迟到消息的既有行为；Task 3 的 handler 与 active 规则必须一起改。

- [ ] **Step 4：绿与接缝兼容。** 跑 QrScanner、qrScanner、PairingScreen 定向测试和 tsc；旧 void 接缝编译通过，false/true 的 PairingScreen 业务测试在任务 3 加入，不能删现有启动异常/安全区测试。
- [ ] **Step 5：更新实施验证报告、desktop 证据链接；fmt/just ci 后提交。**

```sh
(cd yi-agent-rs && cargo fmt --all && just ci)
git add desktop/src/components/QrScanner.tsx desktop/src/components/QrScanner.test.tsx desktop/src/components/PairingScreen.tsx docs/project-management/desktop.md docs/superpowers/reports/2026-11-07-ios-qr-recognition-implementation-verification.md
git commit -m "fix: stabilize QR scanner callback lifecycle"
```

## Task 3：同步配对准入、可见进度与离开失效

**Files:** Modify `desktop/src/components/PairingScreen.tsx:PairingScreen`、`desktop/src/components/PairingScreen.test.tsx`。

**Interfaces:**
- Consumes：Task 2 scan 结果契约、既有 parsePairUri、redeem、saveRemoteConfig、parsePairScope、isInvalidCode。
- Produces：私有 `runRedeem(u: string, c: string, source: "scan" | "manual"): boolean`（同步准入成功与否）；内部 async 完成，不对外改 props。attempt 为 `{ active: boolean }`，只用 identity 判断，不做全局 ID；scanActive ref 隔离 UI 返回到 cleanup 的窗口。

- [ ] **Step 1：写 A01/A02/A03/A04/A15/A16/A18 失败用例。** 在现有测试导入 act，声明 Task 1 的 deferred 完整函数。使用已有 storages/PAIRED/URL：

```tsx
it("A01 accepted QR removes overlay while redeem is pending", async () => {
  const pending = deferred<typeof PAIRED>();
  const redeem = vi.fn(() => pending.promise);
  let emit!: import("../lib/qrScanner").ScanResultHandler;
  const scan = vi.fn((onText: import("../lib/qrScanner").ScanResultHandler) => {
    emit = onText; return vi.fn();
  });
  render(<PairingScreen enableScan scan={scan} redeem={redeem}
    storage={storages()} notice="已被桌面移除" />);
  fireEvent.click(screen.getByRole("button", { name: "扫码" }));
  let accepted: boolean | void = undefined;
  act(() => { accepted = emit(`yiagent://pair?v=1&relay=${encodeURIComponent(URL)}&code=ABCD-EFGH`); });
  // 先检查用户可见行为，使旧实现因覆盖层残留而红，而非先被 undefined 返回值截断。
  expect(screen.queryByRole("dialog", { name: "扫描二维码" })).toBeNull();
  expect(accepted).toBe(true);
  expect(screen.getByRole("status", { name: "配对进度" }).textContent)
    .toBe("已识别二维码，正在连接桌面…");
  expect(screen.getByLabelText<HTMLInputElement>("服务器地址").value).toBe(URL);
  expect(redeem).toHaveBeenCalledTimes(1);
  await act(async () => { pending.reject(new Error("offline")); });
  expect(screen.queryByRole("status", { name: "配对进度" })).toBeNull();
  expect(screen.getByRole("alert").textContent).toBe(CONNECT_FAILED_TEXT);
});
```

表驱动 A16 明确返回/卸载后的默认成功和失败：**四个分支全部必测，不得仅保留 unmount 分支**；返回且宿主暂未卸载证明返回 handler 本身失效，直接卸载后挂新页证明独立的 cleanup 边界。

```tsx
const leaveCases = [
  ["return-without-unmount", "resolve"],
  ["return-without-unmount", "reject"],
  ["unmount-new-page", "resolve"],
  ["unmount-new-page", "reject"],
] as const;
it.each(leaveCases)("A16 %s ignores late %s", async (leave, outcome) => {
  const oldRaw = JSON.stringify({ url: URL, token: "yia_old", scope: "observe", projects: [] }, null, 2);
  const storage = storages({ [REMOTE_STORAGE_KEY]: oldRaw });
  expect(storedRemoteConfig(storage)?.token).toBe("yia_old");
  const pending = deferred<typeof PAIRED>();
  const onPaired = vi.fn();
  const onCancel = vi.fn();
  const view = render(<PairingScreen enableScan={false} storage={storage}
    redeem={() => pending.promise} onPaired={onPaired} onCancel={onCancel} />);
  const f = fields(); // busy 后按钮名变为“配对中…”，不能再调用只查“配对”的 fields()
  fill(f);
  fireEvent.click(f.submit);
  expect(screen.getByText("返回不会撤销已发出的配对请求")).toBeTruthy();
  if (leave === "return-without-unmount") {
    fireEvent.click(screen.getByRole("button", { name: "返回" }));
    expect(onCancel).toHaveBeenCalledTimes(1);
    // 宿主尚未卸载：必须由返回 handler 自身失效，不能靠 cleanup 掩盖错误。
  } else {
    view.unmount();
    render(<PairingScreen storage={storage} enableScan={false} />);
  }
  await act(async () => {
    if (outcome === "resolve") pending.resolve(PAIRED);
    else pending.reject(new Error("old request failed"));
  });
  expect(storage.getItem(REMOTE_STORAGE_KEY)).toBe(oldRaw);
  expect(onPaired).not.toHaveBeenCalled();
  expect(screen.queryByRole("alert")).toBeNull();
});
```

A02 的准入必须另测同一 act 内直接提交表单，不能只通过已终止的 ScanHost 测试：

```tsx
it("A02 synchronous manual submit admission survives stale busy closure", async () => {
  const pending = deferred<typeof PAIRED>();
  const redeem = vi.fn(() => pending.promise);
  render(<PairingScreen enableScan storage={storages()} redeem={redeem} />);
  const f = fields();
  fill(f);
  const form = screen.getByRole("form", { name: "配对" });
  act(() => {
    fireEvent.submit(form);
    fireEvent.submit(form); // 同一 React 批次，busy 闭包尚未提交
    fireEvent.change(f.url, { target: { value: "wss://other.test/ws" } });
    fireEvent.change(f.code, { target: { value: "ZZZZ-YYYY" } });
    fireEvent.change(f.name, { target: { value: "other phone" } });
  });
  expect(redeem).toHaveBeenCalledTimes(1);
  expect([f.url.value, f.code.value, f.name.value]).toEqual([URL, "ABCD-EFGH", "我的 iPhone"]);
  expect([f.url.disabled, f.code.disabled, f.name.disabled]).toEqual([true, true, true]);
  fireEvent.submit(form); // pending 全周期仍不得重新准入
  expect(redeem).toHaveBeenCalledTimes(1);
  expect(screen.queryByRole("button", { name: "扫码" })).toBeNull();
  await act(async () => { pending.reject(new Error("offline")); });
  expect([f.url.disabled, f.code.disabled, f.name.disabled]).toEqual([false, false, false]);
});
```

附加精确用例与断言：

| 测试 | 操作和断言 |
|---|---|
| A02/A18 | 同一 act 先 emit 普通码（false），再合法码（true），再同码/异码/非法码（均 true且不转发）；redeem=1，URL/code仍第一码，alert 不出现。pending 后直接 fireEvent.submit(form) 与 change 三输入仍不能第二次兑换/改值；三 disabled=true；扫码无入口 |
| A03 | redeem reject `{code:4401}` / `{code:-32001}` 为 INVALID_CODE_TEXT；普通 Error 为 CONNECT_FAILED_TEXT，三输入恢复，字段保留，进度卸载，重试或重新扫码可执行 |
| A04 | onPaired spy 内读取 storedRemoteConfig 为新 URL/token/scope；回调一次。onSubmit 存在时 redeem/storage.setItem 不调用，等待后只通知一次；scope/projects 沿用现有断言 |
| A15 | 扫码兑换立即 resolve/reject 都不留 dialog，成功落盘和回调一次；手输 pending 的 named status 文本准确；无 onCancel 时返回提醒不存在 |
| A16 卸载 | 不点返回直接 unmount，旧 resolve/reject 同样不落盘/通知。注入 storage.setItem 同步触发返回时不得继续 onPaired，不承诺撤销已经发生的 setItem |
| A18 | 保存旧 emit/error，扫描返回后旧 emit 返回 true、旧 error 不改 UI；重新扫码后旧回调不影响新实例；当前注入 scan 同步两次 error 只处理一次，首次 accepted 后立即再送 error 被忽略 |

- [ ] **Step 2：跑红。** `(cd desktop && npm test -- --environment jsdom src/components/PairingScreen.test.tsx)`；确认 A01 残留 dialog、返回晚结果写配置等行为红，不仅返回类型 undefined 红。
- [ ] **Step 3：实现同步入口与失效。** 在组件状态旁定义：

```tsx
type Attempt = { active: boolean };
const attempt = useRef<Attempt | null>(null);
const mounted = useRef(true);
const scanActive = useRef(false);
const [source, setSource] = useState<"scan" | "manual">("manual");
const invalidate = () => {
  if (attempt.current) attempt.current.active = false;
  attempt.current = null;
  scanActive.current = false;
};
useEffect(() => {
  mounted.current = true; // StrictMode setup-cleanup-setup 必须重新启用
  return () => { mounted.current = false; invalidate(); };
}, []);
const runRedeem = (u: string, c: string, origin: "scan" | "manual"): boolean => {
  if (!mounted.current || attempt.current) return false;
  const current: Attempt = { active: true };
  attempt.current = current; // 在 setState、外部调用和 await 之前占用
  const live = () => mounted.current && current.active && attempt.current === current;
  const name = deviceName.trim() || inferDeviceName();
  scanActive.current = false;
  setScanning(false);
  if (origin === "scan") { setUrl(u); setCode(c); }
  setSource(origin);
  setError(null);
  setBusy(true);
  void (async () => {
    try {
      if (onSubmit) {
        await onSubmit(u, c, name);
      } else {
        const device = await (redeem ?? defaultRedeem)(u, c, name);
        if (!live()) return;
        const config: RemoteConfig = { url: u, token: device.token, scope: parsePairScope(device.scope) };
        saveRemoteConfig(storage ?? defaultStorage(), config);
      }
      if (live()) onPaired?.();
    } catch (err) {
      if (live()) setError(isInvalidCode(err) ? INVALID_CODE_TEXT : CONNECT_FAILED_TEXT);
    } finally {
      if (live()) { attempt.current = null; current.active = false; setBusy(false); }
    }
  })();
  return true;
};
const handleScanText: ScanResultHandler = text => {
  if (!mounted.current || !scanActive.current || attempt.current) return true;
  const parsed = parsePairUri(text); // 同步纯函数，没有让出执行权
  if (!parsed) { setError(NOT_A_PAIRING_QR_TEXT); return false; }
  runRedeem(parsed.relay, parsed.code, "scan");
  return true;
};
const handleScanError = () => {
  if (!mounted.current || !scanActive.current || attempt.current) return;
  scanActive.current = false;
  setError(CAMERA_UNAVAILABLE_TEXT);
  setScanning(false);
};
```

同一任务修改 `ScanHost` effect，保留其最新回调 ref：

```tsx
useEffect(() => {
  let active = true;
  const stop = scan(text => {
    if (!active) return true;
    try {
      const accepted = ref.current.onText(text);
      if (accepted !== false) active = false;
      return accepted;
    } catch (error) {
      active = false;
      ref.current.onError(error);
      return true;
    }
  }, error => {
    if (!active) return;
    active = false;
    ref.current.onError(error);
  });
  return () => { active = false; stop(); };
}, [scan]);
```

同步 scan throw 仍由既有 ErrorBoundary 捕获。scan 在返回 stop 前同步给结果也必须先关闭 active；非法码 false 必须继续转发。

submit 改同步 preventDefault + `if (!canSubmit || attempt.current || !mounted.current) return; runRedeem(trimmedUrl, trimmedCode, "manual")`。扫码打开按钮先检查 mounted/attempt，再设 scanActive=true、清 error、setScanning(true)；扫描返回先 scanActive=false 再 setScanning(false)。重配返回先 invalidate、mounted=false，再 onCancel，不清已有 storage。

三输入均增加 `disabled={busy}`；onChange 分别用 `if (mounted.current && !attempt.current) setUrl/setCode/setDeviceName(e.target.value)`。覆盖层条件 `scanning && !busy`；保留安全区/边界样式。表单新增：

```tsx
{busy && <p role="status" aria-label="配对进度" className="mt-3 text-xs text-fg-muted">
  {source === "scan" ? "已识别二维码，正在连接桌面…" : "正在连接桌面…"}
</p>}
{onCancel && busy && <p className="mt-2 text-xs text-fg-subtle">
  返回不会撤销已发出的配对请求
</p>}
```

保留 onCancel 返回按钮可用，不加 disabled；立即完成不另安排 timer。更新 props 注释，避免“首启 DOM 逐字不变”误描述新增进度节点。

- [ ] **Step 4：绿与回归。** 跑 PairingScreen/QrScanner/qrScanner 三文件及 tsc；保留 A12 原启动错误、局部 ErrorBoundary、桌面手输、安全区与返回目标断言。
- [ ] **Step 5：记录 A01–A04/A12/A15/A16/A18 结果，更新模块证据；fmt/just ci 后提交。**

```sh
(cd yi-agent-rs && cargo fmt --all && just ci)
git add desktop/src/components/PairingScreen.tsx desktop/src/components/PairingScreen.test.tsx docs/project-management/desktop.md docs/superpowers/reports/2026-11-07-ios-qr-recognition-implementation-verification.md
git commit -m "fix: leave QR preview before pairing awaits"
```

## Task 4：持久化真实相机集成与宿主回归

**Files:** Modify `desktop/src/components/PairingScreen.test.tsx`、`desktop/src/App.test.tsx`；No production App.tsx changes。

**Interfaces:**
- Consumes：Tasks 1–3 生产组件；既有 App 测试 FakeSocket/pairedPhone/settle 与首启、mobile repair describe。
- Produces：A07/A08/A14 真实相机回归；A13/A16 宿主切换/旧配置保留回归。测试文件局部 harness 无新增生产导出。

- [ ] **Step 1：在 PairingScreen 测试文件建立独立 describe。** 文件顶层可 hoist jsQR mock，但媒体/画布 mock 只在此 describe 安装；不 mock QrScanner/startCameraScan/PairingScreen，不传 scan。新增 imports `StrictMode`、`beforeEach`、`within`；用现有 act/storages/PAIRED/URL/deferred。

```tsx
const qr = vi.hoisted(() => ({ decode: vi.fn() }));
vi.mock("jsqr", () => ({ default: qr.decode }));
describe("real camera integration", () => {
  const streams: Array<{ stop: ReturnType<typeof vi.fn>; stream: MediaStream }> = [];
  let mediaDevicesBefore: PropertyDescriptor | undefined;
  const open = vi.fn(async () => {
    const stop = vi.fn();
    const stream = { getTracks: () => [{ stop }] } as unknown as MediaStream;
    streams.push({ stop, stream });
    return stream;
  });
  beforeEach(() => {
    vi.useFakeTimers(); streams.length = 0; open.mockClear(); qr.decode.mockReset();
    qr.decode.mockReturnValue(null);
    mediaDevicesBefore = Object.getOwnPropertyDescriptor(navigator, "mediaDevices");
    Object.defineProperty(navigator, "mediaDevices", {
      configurable: true, value: { getUserMedia: open },
    });
    vi.spyOn(HTMLMediaElement.prototype, "play").mockResolvedValue(undefined);
    vi.spyOn(HTMLVideoElement.prototype, "videoWidth", "get").mockReturnValue(1);
    vi.spyOn(HTMLVideoElement.prototype, "videoHeight", "get").mockReturnValue(1);
    const context = { drawImage: vi.fn(), getImageData: () => ({
      data: new Uint8ClampedArray(4), width: 1, height: 1,
    }) } as unknown as CanvasRenderingContext2D;
    vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue(context);
  });
  afterEach(() => {
    cleanup(); // 先停循环/卸载，再恢复 mocks/timers
    vi.useRealTimers(); vi.restoreAllMocks();
    if (mediaDevicesBefore) Object.defineProperty(navigator, "mediaDevices", mediaDevicesBefore);
    else Reflect.deleteProperty(navigator, "mediaDevices");
  });
  it.each([false, true])("A07/A14 pending recognition strict=%s", async strict => {
    const pending = deferred<typeof PAIRED>();
    const redeem = vi.fn(() => pending.promise);
    const element = <PairingScreen enableScan storage={storages()} redeem={redeem} />;
    const view = render(strict ? <StrictMode>{element}</StrictMode> : element);
    fireEvent.click(screen.getByRole("button", { name: "扫码" }));
    await act(async () => { await vi.advanceTimersByTimeAsync(0); });
    const baseline = open.mock.calls.length; // StrictMode 重放已经收敛
    const changed = <PairingScreen enableScan storage={storages()} redeem={redeem} notice="普通重渲染" />;
    view.rerender(strict ? <StrictMode>{changed}</StrictMode> : changed);
    qr.decode.mockReturnValue({ data: `yiagent://pair?v=1&relay=${encodeURIComponent(URL)}&code=ABCD-EFGH` });
    await act(async () => { await vi.advanceTimersByTimeAsync(200); });
    expect(redeem).toHaveBeenCalledTimes(1);
    expect(open).toHaveBeenCalledTimes(baseline);
    expect(screen.queryByRole("dialog", { name: "扫描二维码" })).toBeNull();
    expect(screen.queryByLabelText("相机预览")).toBeNull();
    expect(screen.getByRole("status", { name: "配对进度" }).textContent)
      .toBe("已识别二维码，正在连接桌面…");
    for (const entry of streams) expect(entry.stop).toHaveBeenCalledTimes(1);
  });
  it.each([false, true])("A08 invalid then valid stays in one live stream strict=%s", async strict => {
    const pending = deferred<typeof PAIRED>();
    const redeem = vi.fn(() => pending.promise);
    const element = <PairingScreen enableScan storage={storages()} redeem={redeem} />;
    render(strict ? <StrictMode>{element}</StrictMode> : element);
    fireEvent.click(screen.getByRole("button", { name: "扫码" }));
    await act(async () => { await vi.advanceTimersByTimeAsync(0); });
    const baseline = open.mock.calls.length;
    const live = streams.filter(entry => entry.stop.mock.calls.length === 0);
    expect(live).toHaveLength(1);
    qr.decode.mockReturnValue({ data: "https://ordinary.test" });
    await act(async () => { await vi.advanceTimersByTimeAsync(200); });
    const dialog = screen.getByRole("dialog", { name: "扫描二维码" });
    expect(within(dialog).getByRole("alert").textContent).toBe("不是有效的配对二维码");
    expect(live[0].stop).not.toHaveBeenCalled();
    expect(open).toHaveBeenCalledTimes(baseline);
    expect(redeem).not.toHaveBeenCalled();
    qr.decode.mockReturnValue({ data: `yiagent://pair?v=1&relay=${encodeURIComponent(URL)}&code=ABCD-EFGH` });
    await act(async () => { await vi.advanceTimersByTimeAsync(200); });
    expect(redeem).toHaveBeenCalledTimes(1);
    expect(open).toHaveBeenCalledTimes(baseline);
    expect(screen.queryByRole("dialog", { name: "扫描二维码" })).toBeNull();
    expect(screen.queryByRole("alert")).toBeNull();
    for (const entry of streams) expect(entry.stop).toHaveBeenCalledTimes(1);
  });
});
```

A14 的 rerender 使用新 notice/element 引用以真实触发父 render，不能只用同一 element 让 React bail out；变更 notice 后仍按上述 baseline 断言。A08 只断言最新存活流在无效码后未停止，不能误把 StrictMode 已释放的废弃流算成回归。

- [ ] **Step 2：验证判别性红。** 因实现已到位，新集成应先绿；使用 Task 5 临时回退 A07 完整旧行为，确认此真实测试红后恢复。不以任务 3 的 scan 接缝测试代替这里的红证据。
- [ ] **Step 3：宿主精确回归 A13/A16。** 保留首启 describe 中已有 `pairs over the relay by redeeming on a frame, then connects` 与 `reports a failed redemption without persisting a config`，它们分别验证首启成功切宿主与失败保留表单；在 mobile repair describe 添加以下完整用例，复用同作用域的 pairedPhone/settle：

```tsx
it.each(["resolve", "reject"] as const)("A16 late %s after repair return keeps old client", async outcome => {
  pairedPhone();
  const originalRaw = localStorage.getItem("yi-agent.remote");
  let reply!: () => void;
  class FakeSocket {
    onopen: (() => void) | null = null;
    onmessage: ((e: { data: unknown }) => void) | null = null;
    onclose: ((e: { code: number }) => void) | null = null;
    onerror: unknown = null;
    constructor() { queueMicrotask(() => this.onopen?.()); }
    send(raw: string) {
      const frame = JSON.parse(raw) as { id?: number; method?: string };
      if (frame.method === "initialize") {
        queueMicrotask(() => this.onmessage?.({
          data: JSON.stringify({ jsonrpc: "2.0", id: frame.id, result: {} }),
        }));
      } else if (frame.method === "pair/redeem") {
        reply = () => this.onmessage?.({
          data: JSON.stringify(outcome === "resolve"
            ? { jsonrpc: "2.0", id: frame.id,
                result: { device_id: "dev-2", token: "yia_late", scope: "control" } }
            : { jsonrpc: "2.0", id: frame.id,
                error: { code: -32001, message: "invalid or expired pairing code" } }),
        });
      }
    }
    close() {}
  }
  vi.stubGlobal("WebSocket", FakeSocket);
  try {
    render(<App />);
    await settle();
    expect(clients.length).toBe(1);
    fireEvent.click(screen.getByRole("button", { name: "重新配对" }));
    fireEvent.change(screen.getByLabelText("配对码"), { target: { value: "ABCD-EFGH" } });
    fireEvent.click(screen.getByRole("button", { name: "配对" }));
    await settle();
    expect(reply).toBeTypeOf("function");
    expect(screen.getByRole("status", { name: "配对进度" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "返回" }));
    await settle();
    act(() => { reply(); });
    await settle();
    expect(localStorage.getItem("yi-agent.remote")).toBe(originalRaw);
    expect(clients.length).toBe(1);
    expect(screen.queryByRole("form", { name: "配对" })).toBeNull();
    expect(screen.queryByText("app render failed")).toBeNull();
  } finally {
    cleanup();
    vi.unstubAllGlobals();
  }
});
```

补充仍停留重配页的 reject 用例：用相同 FakeSocket 结构回 -32001，不点返回，断言 INVALID_CODE_TEXT、进度卸载、原配置字符串未改且 clients=1；该文案若未导入则从 PairingScreen 导入。首启/重配的原成功用例都必须继续通过，不改 App 生产分支。相机行为仍只由前面真实测试证明。

- [ ] **Step 4：跑定向绿。**

```sh
(cd desktop && npm test -- --environment jsdom src/lib/qrScanner.test.ts src/components/QrScanner.test.tsx src/components/PairingScreen.test.tsx)
(cd desktop && npm test -- --environment jsdom src/App.test.tsx && npx tsc --noEmit)
```

若 getContext 的重载导致 strict 类型错误，使用 `mockImplementation((id: string) => id === "2d" ? context : null)` 并与 DOM 当前重载校验，不用 ts-ignore 或跳过 tsc。每个 pending Promise 可以保持未settle，但必须 cleanup 停相机；reject 的用例必须被组件 await 消费，不留 unhandled rejection。

- [ ] **Step 5：更新报告/模块映射；fmt/just ci 后提交。**

```sh
(cd yi-agent-rs && cargo fmt --all && just ci)
git add desktop/src/components/PairingScreen.test.tsx desktop/src/App.test.tsx docs/project-management/desktop.md docs/superpowers/reports/2026-11-07-ios-qr-recognition-implementation-verification.md
git commit -m "test: cover real iOS QR pairing lifecycle"
```

## Task 5：回退判别性、完整门禁与真机交付

**Files:** Modify 实施验证报告、bug/module/index；生产文件仅临时回退且必须还原，不提交回退状态。

**Interfaces:**
- Consumes：A01–A18 与 I01–I07；Tasks 1–4 的测试与修复 commit。
- Produces：可追踪的代码门禁证据，以及独立的真机通过/未验收结论；不把未执行项目勾选完成。

- [ ] **Step 1：A05 mutation。** 在 clean worktree 暂时把 QrScanner effect 依赖恢复 `[onText,onError,deps]`，运行 A05 测试，预期 open=2 或 track 提前 stop 的断言红；保存输出与退出码；仅还原这一文件的临时 mutation，不覆盖用户改动。
- [ ] **Step 2：A07 mutation。** 同时删掉 runRedeem 中的早期 setScanning(false)、把关闭搬至有效 finally，并把渲染条件恢复 `scanning`。pending 时真实集成必须因 dialog/preview 残留红；仅撤一道仍绿不算失败。还原 mutation，重跑两条判别性测试绿；记录 diff 确认无临时回退留存。
- [ ] **Step 3：全量前端与项目门禁。**

```sh
(cd desktop && npm test && npx tsc --noEmit && npm run build)
(cd yi-agent-rs && cargo fmt --all && just ci)
git diff --check
```

全部 exit=0 方可写“代码门禁通过”。若 CI 卡住先按 CLAUDE.md 查 cargo 持锁进程/测试运行阶段，不并发启动第二套全量 cargo；若门禁失败先修复再重跑，不以定向绿替代。

- [ ] **Step 4：交付验收包与 I01–I07。** worktree 不包含被忽略的 `src-tauri/binaries`，`src-tauri/build.rs` 又需要可执行 sidecar 读取 build id；先使用现有构建脚本准备**本 worktree 的 host sidecar**，不用主目录旧二进制冒充修复源码。再构建实际前端与 iOS 包：

```sh
(cd desktop && npm run sidecar:release && npm run build && npm run ios:build)
```

sidecar 脚本会按 rustc host triple 填充 binaries 并验证 identity。这里不新增 iOS sidecar运行能力；仅满足现有构建输入。`ios:build` 会同步三处版本文件，完成后检查 `git diff -- desktop/src-tauri`：只允许既有版本同步的预期改动，其他工程/签名变动不得混入修复提交。签名/设备安装沿用仓库既有 iOS 工程。记录源码 commit、实际 App version/build、iPhone/iPad 型号、iOS、授权状态、直连/中继。

执行 spec §7 的七条操作：正常中继；可控 3 秒慢响应；过期/离线/网络失败；普通码→合法码；返回再扫 5 次；重配 pending 返回/晚成功；直连与首启。分别记录可见状态、相机指示释放、配置是否保留、去敏日志/录像位置。每条有 pass/fail；未具备设备则写“未验收”，不虚构通过、不阻止仅代码门禁合并。若仍黑屏记录返回是否消失/发生阶段/WebContent 日志，另查独立根因。

- [ ] **Step 5：同步状态与提交。** A 全通过但 I 未完成时报告写“代码门禁通过，真机未验收”，bug/模块 `[ ]` 不改、desktop 计数不增加；I 全部通过才改本项 `[x]`，重新统计模块计数并更新 README。同一提交含报告/bug/module/index，fmt/just ci 后 conventional commit，不含 Co-Authored-By。
- [ ] **Step 6：审查与合并。** 加载 requesting-code-review 和 finishing-a-development-branch；独立审查无未解决项后按项目规则合 main，合并结果再跑前端测试/build与 `just ci`。main 有不相关未提交修改时不能暂存/丢弃它们；不重叠可安全合并但在 clean 合并树验证，若冲突请用户协调，不污染验证树。最后仅移除本任务 worktree/分支；不清理仍在开发的其他 target。

## 覆盖矩阵与计划自审

| Spec 判据 | 任务与门禁 |
|---|---|
| A01 / A02 / A03 / A04 | Task 3：pending/准入/失败/落盘顺序与 onSubmit |
| A05 | Task 2 真实组件回调刷新；Task 5 单故障 mutation |
| A06 / A09 / A10 / A17 | Task 1：拒绝结果/迟到流/迟到错误/同步 throw |
| A07 / A08 / A14 | Task 4：真实 API 边界集成与 StrictMode；Task 5 A07 复合 mutation |
| A11 | Task 2：deps 更换/unmount/旧流所有权 |
| A12 | Task 3 保留并跑启动错误/边界/安全区/桌面手输测试 |
| A13 | Task 4 App 仅宿主路径、配置与无根级崩溃 |
| A15 / A16 / A18 | Task 3：立即完成/返回卸载/接缝首终止；Task 4 真实宿主晚结果 |
| I01–I07 | Task 5 实际前端验收包与逐条去敏记录；不由 jsdom 代替 |

自审结果：所有 G1–G6、§4 状态/返回边界、§5 契约都有任务；使用同一个 ScanResultHandler，不引入异步接收返回值；mounted 在 StrictMode setup 重新启用；成功检查/默认落盘/通知之间无 await。图像/WebContent 崩溃未被本地测试证明，直连 deadline 和注入 onSubmit 副作用均维持 spec 边界。实现任务尚未执行，只有计划和 spec 审查可宣告完成。
