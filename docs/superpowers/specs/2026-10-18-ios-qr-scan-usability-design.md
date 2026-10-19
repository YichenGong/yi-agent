# 手机扫码配对的「黑屏」与「没有返回」(设计)

- 日期: 2026-10-18
- 状态: 已按用户指令（"你先修，我最后做确认"）先行实施，待用户终审
- 上游设计: [2026-10-02-qr-pairing-design.md](./2026-10-02-qr-pairing-design.md)

## 1. 问题

在桌面「设置 → 远程访问」生成配对二维码后，用手机（iOS App）点「扫码」：

1. **黑屏** —— 点下去整屏变黑，体验差，且看不出发生了什么。
2. **没有返回按钮** —— 扫描过程中找不到退出路径。

两个问题都在「扫码」这条 iOS 专属路径上，桌面不渲染该路径。

## 2. 根因（含证据）

### R1 黑屏（主因）：同步 `TypeError` 逃逸 → React 卸载整棵树

`desktop/src/lib/qrScanner.ts:57-58` 的默认取流实现：

```ts
const getUserMedia =
  deps.getUserMedia ?? ((c: MediaStreamConstraints) => navigator.mediaDevices.getUserMedia(c));
```

它在 `:74` 被**同步调用**，而 `.catch(...)` 挂在 `:99` 的 promise 链上。当
`navigator.mediaDevices` 为 `undefined` 时，读取 `.getUserMedia` 会**同步**抛
`TypeError`——这不是 promise rejection，`.catch` 接不住。本机复现（`navigator.mediaDevices`
缺失，与 iOS 非安全上下文下的 WKWebView 一致）：

```
SYNC THROW -> escapes promise .catch: TypeError - Cannot read properties of undefined (reading 'getUserMedia')
```

`MediaDevices` 在 WebIDL 里标了 `[SecureContext]`，上下文不安全时**整个属性消失**而非
报错；iOS WKWebView（Tauri 用自定义 scheme `tauri://localhost`，WebKit 默认不视为
potentially trustworthy origin）正好命中这一条。

该异常发生在 `desktop/src/components/QrScanner.tsx:16-21` 的 `useEffect` 里。React 的
effect 异常会冒泡到最近的 error boundary；全仓库没有 boundary（`ErrorBoundary` /
`componentDidCatch` / `unhandledrejection` 均无命中），React 19 于是**卸载整棵树**。手机
`body` 底色是 `--panel:#171717`（`desktop/src/index.css:6-7`，深色主题），用户看到的就是
**黑屏**，且没有任何恢复入口。

### R2 黑屏（次要假设）：预览停在第一帧

`qrScanner.ts:81-82`（**修复前**）先 `video.srcObject = s` 再 `void video.play()`：

```ts
video.srcObject = s;
void video.play();
```

`play()` 的 reject 被 `void` 丢弃。若 iOS 需要媒体元数据就绪才出画，预览会停在第一帧
（黑），且失败无声、无法上报。这与 R1 是**不同机制**，最终由真机区分；两者都在本次修复
的覆盖面内。

### R3 没有返回：覆盖层没补安全区，「取消」落在刘海下且过小

扫描覆盖层是 `fixed inset-0 z-50 flex flex-col bg-black`（`PairingScreen.tsx:279-284`（**修复前**）），
头部行 `px-4 py-3`（**修复前** `:285-294`）里放着**唯一**的出口「取消」（12px、`text-white/80`）。
`fixed` 元素脱离 `body` 的安全区内边距（仓库自己在 `index.css:127-143`、`:196-205` 记录过
这条），而该覆盖层**没有补任何安全区**。「取消」因此落在 y≈12..44px——正是 iPhone
刘海/灵动岛与状态栏覆盖的位置，又小又贴边，**看不见、也点不到**，体感就是「没有返回
按钮」。首启配对路径不传 `onCancel`，`取消` 是唯一出路：一旦不可达就是死路。

仓库既有做法是给 `fixed` 头部补 `padding-top: max(env(safe-area-inset-top, 0px), 12px)`
（`index.css:393-395` 的 `.settings-header`），此处漏了。

## 3. 目标 / 非目标

**目标**

- **G1** 任何「取流不可用/异常」都**不得**卸载 App；必须退化为可手输的表单并给出明确文案。
  这是 `2026-10-02-qr-pairing-design.md` §5 已写死的硬不变量。
- **G2** 扫描覆盖层的出口位于 iPhone 安全区内、可见、可点（≥44px 触控目标）。
- **G3** 桌面端零变化（该路径 iOS-only；所有新增样式挂在 `[data-mobile="true"]` 下）。

**非目标**

- 不改二维码载荷、配对协议、兑换流程。
- 不改「地址不是 `/connect` 结尾时二维码回退原值」这一**既有且被用例钉住**的行为
  （`SettingsRemoteTab.test.tsx` 有 `falls back to the raw url in the QR when it is not a
  /connect endpoint`）。它不是 bug，是刻意的降级。
- 不引入原生扫码插件、不重写相机权限申请。

## 4. 设计

### 4.1 `qrScanner` 取流守卫（纯逻辑层）

把「拿流」这一件事做成**永不抛异常**的异步入口：

- 默认实现改为读 `navigator.mediaDevices?.getUserMedia`，取其存在性判断；
- 缺失时返回被拒绝的 promise（`new Error("mediaDevices unavailable")`），交给既有的
  `.catch → onError` 路径；
- 用 `try/catch` 包住对 `getUserMedia` 的**调用**（注入的 fake 也可能同步抛），
  同步抛出的错误转换成 rejected promise。

于是 `startCameraScan` 对外仍是「成功则逐帧解码、失败则 `onError` 且不启动循环」，
`TypeError` 不再逃逸。文案沿用既有的 `CAMERA_UNAVAILABLE_TEXT`（「无法访问相机，可手输
配对码」）——它对「API 缺失」与「权限被拒」都成立，无需新增分支。

### 4.2 覆盖层局部 error boundary（渲染层保险）

新增一个可复用的类组件 `ErrorBoundary`（`desktop/src/ui/`，与 `Modal`/`Popover` 同层），
`getDerivedStateFromError` 落 state、`componentDidCatch` 调 `onError`。`PairingScreen`
用它包住扫描器：

- 出错时 `onError` 令 `setScanning(false)` + `setError(CAMERA_UNAVAILABLE_TEXT)`，即
  **回到可手输的表单**并说明原因；
- `fallback` 仍是本地化的兜底文案（万一宿主没收起覆盖层，也不会是空白）。

这样「扫描器抛任何错」的影响面收敛到覆盖层一处，永不再卸载整棵树（G1 的第二道保险）。

### 4.3 覆盖层安全区 + 可达出口（G2）

- 头部行加挂点类 `scan-overlay-bar`，`index.css` 增
  `[data-mobile="true"] .scan-overlay-bar { padding-top: max(env(safe-area-inset-top, 0px), 12px); }`；
- 出口按钮改为明确的**「返回」**，视觉加粗、触控目标 ≥44px（`min-h-11 min-w-11`），
  保持左上角（iOS 返回惯例）；
- 覆盖层中部的错误提示（`role="alert"`）加挂点类 `scan-overlay-alert` 并补
  `bottom: max(env(safe-area-inset-bottom, 0px), 24px)`，避免贴到 Home Indicator。

所有新样式都在 `[data-mobile="true"]` 前缀下，桌面不匹配（G3）。

### 4.4 预览硬化（R2）

`QrScanner` 的 `<video>` 加 `autoPlay` / `muted` / `playsInline`；`play()` 的 reject 改为
上报（命中即 `onError`，让失败可见、可退出），不再被 `void` 吞掉。

## 5. 测试策略（Tier 0，mock，无真机）

| 层 | 用例 |
| --- | --- |
| `qrScanner.test.ts` | 无 `mediaDevices` 时**不抛**、走 `onError`；`getUserMedia` **同步抛出**也被接住并 `onError` |
| `ErrorBoundary.test.tsx` | 正常渲染 children；子组件抛错时渲染 fallback 且调 `onError` |
| `PairingScreen.test.tsx` | 抛错的扫描器不黑屏：回到表单并显示相机文案；覆盖层含安全区的头部挂点类且「返回」可达 |
| `mobileLayout.test.ts` | 静态门禁：`index.css` 里 `[data-mobile="true"] .scan-overlay-bar` 带 `max(env(safe-area-inset-top...` |

硬不变量：这些改动不得让既有的 36 条配对/扫码用例变红；文本手输路径逐字节不受影响。

## 6. 已知限制（如实记录）

- **真机不可自动化**：`getUserMedia` 是否可用、WKWebView 的相机授权、真实黑屏属于哪一类
  （R1 还是 R2），CI 无法覆盖，必须真机手验。本次修复对**两类机制都做防御**，因此不依赖
  先确定是哪一个。
- 若真机上 `navigator.mediaDevices` 恒为 `undefined`（iOS 自定义 scheme 不是安全上下文），
  则扫码最终仍只能优雅降级为手输——本次修复保证这种情况**不再黑屏**，但要在 iPhone 上
  真正「扫得动」，需要另行评估（改用 https scheme / 原生扫码插件），不在本次范围。
