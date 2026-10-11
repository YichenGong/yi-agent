# iOS 主屏应用显示名改为 `Yi-Agent` 设计方案

## 背景与问题

iPhone 主屏上 app 图标下方的名字目前显示为 `yi-agent`，全小写、带连字符，
与 App 内各处品牌文案（窗口标题 `Yi-Agent`、引导页 `欢迎使用 Yi-Agent`）
不一致。目标是让主屏显示名变成 `Yi-Agent`。

## 现状（证据）

主屏图标名由打包进 app 的 `Info.plist` 决定，解析优先级为
`CFBundleDisplayName`（有则用）→ `CFBundleName`（无则回退）。

- `desktop/src-tauri/gen/apple/desktop_iOS/Info.plist`：
  - `CFBundleName = $(PRODUCT_NAME)`（第 13-14 行）
  - **没有** `CFBundleDisplayName` 键
- `PRODUCT_NAME = yi-agent`：
  - `desktop/src-tauri/gen/apple/project.yml:13`（xcodeproj 的生成源）
  - `desktop/src-tauri/gen/apple/desktop.xcodeproj/project.pbxproj:400` / `:432`

因此当前显示 `yi-agent`。

固有限制（见 `docs/relay-deploy.md` §4.1 与 §4.2 坑 2）：`project.yml` 是
`desktop.xcodeproj` 与 `desktop_iOS/Info.plist` 的生成源，跑一次
`npx tauri ios init` 会按它重生成工程、覆盖手改。任何改动必须同时落在
`project.yml` 与已提交的生成产物中。

## 方案（已确认）

**只加 `CFBundleDisplayName`，不改 `PRODUCT_NAME`。**

`CFBundleDisplayName` 优先级高于 `CFBundleName`，所以主屏显示名会变成我们
指定的值，而物理可执行文件名、`.app` / `.ipa` 名、dSYM 路径保持 `yi-agent`
不变——`relay-deploy.md` 里记录的构建/安装命令（`yi-agent.ipa`、
`yi-agent.app`）全部继续有效。

备选「把 `PRODUCT_NAME` 改成 `Yi-Agent`」被否决：会连带改掉可执行文件名、
产物名、dSYM 路径，破坏已记录的构建/安装流程，风险大且无收益。

## 实现清单

1. `desktop/src-tauri/gen/apple/project.yml` —— `desktop_iOS.targets.info.properties`
   下新增 `CFBundleDisplayName: Yi-Agent`（生成源，防 `tauri ios init` 回退）
2. `desktop/src-tauri/gen/apple/desktop_iOS/Info.plist` —— 新增
   `CFBundleDisplayName` = `Yi-Agent`（本次构建实际读取的文件）
3. `desktop/src/mobileLayout.test.ts`（或新增专用测试文件）—— 静态门禁：
   读取真实 `Info.plist`，断言 `CFBundleDisplayName == "Yi-Agent"`，
   防止将来重生成悄悄抹掉。与仓库既有「读真实产物做门禁」的风格一致
   （`mobileLayout.test.ts` 直接读真实 `index.html` / `index.css`）。
4. 文档同步：`docs/relay-deploy.md` §4 与 `docs/project-management/desktop.md`
   的 iOS 条目记录显示名变更。
5. 走 worktree + 分支 `fix/ios-app-display-name`，`main` 上不直接改。

## 范围外

- macOS 桌面版 app 名（来自 `tauri.conf.json` 的 `productName` / 窗口 `title`）
  本次不动。
- 不改 `PRODUCT_NAME`、不改 bundle identifier、不改图标资源。

## 验证

- `cd desktop && npx vitest run src/mobileLayout.test.ts`（含新门禁断言）通过。
- `cd desktop && npx tsc --noEmit` 通过。
- 手工核对：`plutil -p desktop/src-tauri/gen/apple/desktop_iOS/Info.plist`
  能看到 `CFBundleDisplayName => Yi-Agent`。
- （可选，需真机/签名）重打包安装后，iPhone 主屏图标下方显示 `Yi-Agent`。
