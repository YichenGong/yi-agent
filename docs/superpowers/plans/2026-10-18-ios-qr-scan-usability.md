# 手机扫码配对「黑屏 + 无返回」修复 (计划)

- 日期: 2026-10-18
- 设计: [2026-10-18-ios-qr-scan-usability-design.md](../specs/2026-10-18-ios-qr-scan-usability-design.md)
- 分支: `fix/mobile-qr-scan-black-screen`（worktree `.worktrees/fix-mobile-qr-scan`）
- 原则: TDD（先写红的用例，再实现）；桌面端逐字节不变；`cargo fmt`/`tsc`/`vitest` 全绿才提交

## Task 1：`qrScanner` 取流守卫（防同步 `TypeError` 逃逸）

**文件**：`desktop/src/lib/qrScanner.ts`、`desktop/src/lib/qrScanner.test.ts`

- [ ] 1.1 先加两条红用例：
  - `reports an error instead of throwing when mediaDevices is missing`：不给
    `getUserMedia`，把 `globalThis.navigator.mediaDevices` 藏起来（`vi.stubGlobal`），
    断言 `startCameraScan(...)` **不抛**、且 `onError` 被调用。
  - `survives a getUserMedia that throws synchronously`：注入的 `getUserMedia` 同步
    `throw`，断言 `onError` 被调用（而非逃逸）。
- [ ] 1.2 实现：默认实现改为 `navigator.mediaDevices?.getUserMedia` 存在性守卫；调用点
  包 `try/catch`，同步异常转 rejected promise 交给 `.catch → onError`。
- [ ] 1.3 跑 `npx vitest run src/lib/qrScanner.test.ts`，6/6 绿。

## Task 2：可复用 `ErrorBoundary`

**文件**：`desktop/src/ui/ErrorBoundary.tsx`（新）、`desktop/src/ui/ErrorBoundary.test.tsx`（新）

- [ ] 2.1 先写红用例：正常渲染 children；子组件抛错时渲染 fallback 并调 `onError`。
- [ ] 2.2 实现：类组件，`getDerivedStateFromError` + `componentDidCatch(error)` 调
  `onError?.(error)`；`fallback` 可传入。
- [ ] 2.3 跑 `npx vitest run src/ui/ErrorBoundary.test.tsx` 绿。

## Task 3：`PairingScreen` 接线（不黑屏 + 返回可达）

**文件**：`desktop/src/components/PairingScreen.tsx`、`desktop/src/components/PairingScreen.test.tsx`、
`desktop/src/index.css`、`desktop/src/mobileLayout.test.ts`

- [ ] 3.1 先加红用例：
  - 抛错的扫描器不再黑屏：`scan` 立即抛/通过 boundary 传播后，断言仍能回到表单
    并看到「无法访问相机，可手输配对码」（`scanning` 收起）。
  - 覆盖层头部含安全区挂点类 `scan-overlay-bar`，且出口按钮名称为「返回」。
- [ ] 3.2 实现：
  - 用 `ErrorBoundary` 包住 `<QrScanner>`/`ScanHost`，`onError` → `setScanning(false)`
    + `setError(CAMERA_UNAVAILABLE_TEXT)`。
  - 覆盖层头部加 `scan-overlay-bar`；出口按钮文案「返回」、`min-h-11 min-w-11`。
  - 覆盖层错误提示加 `scan-overlay-alert`。
  - `index.css` 增 `[data-mobile="true"] .scan-overlay-bar{ padding-top: max(env(safe-area-inset-top,0px),12px) }`
    与 `.scan-overlay-alert{ bottom: max(env(safe-area-inset-bottom,0px),24px) }`。
- [ ] 3.3 静态门禁：`mobileLayout.test.ts` 增一条断言上述两条 CSS 存在且带 `env(safe-area-inset-*)`。

## Task 4：`QrScanner` 预览硬化

**文件**：`desktop/src/components/QrScanner.tsx`、`desktop/src/lib/qrScanner.ts`

- [ ] 4.1 红用例：`play()` reject 时 `onError` 被调用（注入 fake video 的 `play` 返回
  rejected promise）。
- [ ] 4.2 实现：`<video>` 加 `autoPlay`/`muted`/`playsInline`；`play()` 的
  `.catch(onError)` 取代 `void`。

## Task 5：验证与收尾

- [ ] 5.1 `cd desktop && npx tsc --noEmit`
- [ ] 5.2 `cd desktop && npx vitest run`（全量，必须全绿；记录条数）
- [ ] 5.3 `cd desktop && npm run build`
- [ ] 5.4 文档同步：`docs/bug-list.md` 加两条（黑屏、无返回）、
  `docs/project-management/desktop.md` 增条目、`README.md` 计数按需
- [ ] 5.5 commit（conventional、无 `Co-Authored-By`）；**不**自动合并——用户终审后由用户确认

## 真机验收（用户执行）

1. `tauri ios dev`（或已装包）在 iPhone 上打开。
2. **先取证**：Safari → 开发 → 设备 → WebView，控制台执行
   `navigator.mediaDevices`、`window.isSecureContext`：
   - `undefined` / `false` → 命中 R1（本次修复应表现为：不再黑屏，回表单并提示可手输）。
   - 有对象 / `true` → 命令执行 `getUserMedia` 能否出画，区分 R2。
3. 点「扫码」：
   - [ ] 不再黑屏（最坏也是「回到表单 + 提示」，可继续操作）。
   - [ ] 左上角「返回」在安全区内、清晰可见、一点即回表单。
4. 若画面前置可用：对准桌面二维码，应自动回填并配对。
