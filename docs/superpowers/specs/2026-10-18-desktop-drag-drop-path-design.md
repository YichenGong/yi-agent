# 拖入文件/文件夹自动录入绝对路径（桌面端）

日期：2026-10-18
状态：设计已确认，待复核

## 背景与目标

Mac 桌面端（Tauri）里，用户想把一个文件或文件夹的**全局绝对路径**喂给
agent 时，目前只能手打或从终端复制粘贴。目标：把文件/文件夹从 Finder 拖到
输入框卡片上，路径自动录入到当前会话的输入框（草稿）里，落在光标处。

## 关键约束（实测确认）

1. **Tauri 2 拦截 OS 拖放。** `tauri.conf.json` 未设 `dragDropEnabled: false`，
   走默认值 `true`，即 OS 拖放由 Tauri 接管，webview **收不到** HTML5
   `drop` / `dataTransfer`。所以不能用 React 的 `onDrop`。
   - 可靠通路：`@tauri-apps/api/webview` 的
     `getCurrentWebview().onDragDropEvent()` 返回 `UnlistenFn`。
   - 事件形状（`node_modules/@tauri-apps/api/webview.d.ts:22`）：
     `{type:'enter', paths, position}` / `{type:'over', position}` /
     `{type:'drop', paths, position}` / `{type:'leave'}`；`position` 是
     `PhysicalPosition`（**物理像素**）。
   - 权限：事件监听走 `core:event`，`core:default` 已含
     `core:event:default`（`allow-listen`/`allow-unlisten`），
     `capabilities/default.json` 无需改动。
2. **`@tauri-apps/api` 在本仓库是动态 import 的**
   （`desktop/src/App.tsx:127`、`desktop/src/hooks/useSettingsState.ts:105`）。
   新代码沿用同一模式，不新增静态依赖。
3. **远端（iOS）构建没有 Tauri webview API**，且与桌面不共享文件系统。
   现有平台判据 `isRemoteClient()`（`desktop/src/lib/platform.ts:20`）是唯一入口，
   远端一律不装监听。
4. **草稿属于会话，不属于输入框。** 输入框受控：`value` + `onDraftChange`
   （`desktop/src/components/MessageInput.tsx:41`），父级 `App` 的 `changeDraft`
   把文本写进 `view(id).draft`（`desktop/src/App.tsx` `changeDraft`）。录入路径
   必须走这条既有通路，才能"切走再切回，路径还在"。
5. **现有附件白名单只认文档/图片扩展名**（`desktop/src/lib/attachmentLimits.ts`），
   文件夹必然被拒。本次**不做**附件 chip，与"文件夹只能录路径"自洽。

## 已确认的产品决策

- **行为 A**：拖入任何东西（文件或文件夹）都只把**绝对路径文本**插入输入框，
  不生成附件 chip。
- **落点**：只有落在**输入框卡片矩形内**才插入；落在别处无反应。
- **高亮**：拖拽悬停在卡片上时整卡高亮，作为"可落点"反馈。
- **插入格式**：光标处插入；前面有内容时补一个空格；一次拖入多条时
  每条各占一行（`\n` 分隔）；路径含空格时用双引号包住。

## 架构

```
OS 拖拽
  │
  ▼
Tauri webview onDragDropEvent (enter / over / drop / leave)
  │
  ▼
useDragDropPaths（新 hook，desktop/src/hooks/useDragDropPaths.ts）
  · isRemoteClient() → 不注册监听，返回无高亮
  · enter/over：position ÷ devicePixelRatio 与卡片 rect 比对 → 命中则 active=true
  · drop：命中 → onDrop(paths)；随后清 active
  · leave：清 active
  │
  ▼
App.onDropPaths(paths)（新回调）
  · 无当前会话（disabled）→ 忽略
  · 取当前草稿 draft 与记录的光标 caret
  · text = insertDroppedPaths(draft, caret, paths)（纯函数）
  · changeDraft(text) + 把光标移到 text.caret
  │
  ▼
MessageInput（受控）显示新文本
```

### 职责边界

- **`insertDroppedPaths`（纯函数，`desktop/src/lib/droppedPaths.ts`）**：
  唯一知道"怎么排版路径"的地方。无 React、无 Tauri、无 DOM。签名
  `(draft: string, caret: number, paths: readonly string[]) => { text: string; caret: number }`。
  可被单测穷举，且将来 TUI 或其它入口想复用同一排版规则也能直接调。
- **`useDragDropPaths`（hook，`desktop/src/hooks/useDragDropPaths.ts`）**：
  唯一知道 Tauri 拖放事件形状的地方。注入 `subscribe`（返回 `UnlistenFn`
  的函数）与 `isRemote`，让 jsdom 测试能直接驱动事件、不碰真实 Tauri。
  它只负责"哪一次拖放落在卡片上"与高亮状态。
- **`App`**：把两者接起来 —— 决定"落到哪个会话的草稿"，并复用 `changeDraft`。
  App 是唯一知道"当前会话"的地方，hook 不该知道。
- **`MessageInput`**：新增三个 props —— `dropActive`（由 `App` 从 hook 状态传入，
  驱动高亮）、`caretSeek`（由 `App` 从纯函数返回值传入的 `{ seq, pos }`，供受控
  textarea 定位光标）、`onCaretChange`（把用户当前光标位置报给父级）。它只做
  "渲染高亮"与"读写光标"，不订阅任何 Tauri 事件。

### 为什么监听挂在 App 而不是 MessageInput

`MessageInput` 只在聊天视图且非看板时渲染（`desktop/src/App.tsx:2170`），
而拖放是**窗口级**事件：控件卸载时监听会跟着卸掉，再挂回时容易漏掉
`leave`/时序边界。挂在顶层 hook 一次订阅、整窗口有效，落点仍由卡片 rect 判定。

## 关键实现细节

### 落点判定

`position` 是物理像素，DOM 的 `getBoundingClientRect()` 是 CSS 像素：
`x = position.x / devicePixelRatio`、`y = position.y / devicePixelRatio`，
再与卡片 rect 做包含判断（左闭右开，与 `contains` 语义一致的
`x >= left && x < right && y >= top && y < bottom`）。

注意：高 DPI Mac（Retina）上不除这个系数会让判定整体偏移约一倍，
**这是必须实测的点**。

### 光标位置

受控 textarea 插入后需要把光标放回正确位置。时机：`onDropPaths` 里
`changeDraft(newText)` 之后，`caret` 由 `insertDroppedPaths` 返回；
`App` 通过 seek 计数（一个自增的 `{pos}` 值）传给 `MessageInput`，
后者在 effect 里 `inputRef.current.setSelectionRange(pos, pos)`。
用 seek 计数而非直接用 pos，是为了让"连续两次拖入落在同一位置"也能各触发一次
定位效果。

### 高亮

`MessageInput` 的卡片 `div` 上加 `data-drop-active`（有无属性即真/假）
与条件 class（如 `border-blue-500 ring-1 ring-blue-500/40`）。用 `data-*` 而非
只加 class，是为了让 jsdom 断言有稳定锚点（与仓库既有做法一致，如
`data-composer-toolbar`）。

### 边界

- `disabled`（无当前会话）：忽略 drop，不插入、不报错 —— 与输入框置灰护栏一致。
- `turnActive`：**允许**插入。拖入是编辑草稿，不是发送；草稿在发送前随时可改。
- 路径为空数组 / `paths` 缺失：忽略。
- 符号链接：按 OS 交给的路径原样录入（Tauri `paths` 是否已解引用随 macOS 版本
  有差异）。本次不做额外解引用，实测为准；若实测是链接路径，在跟进项里记录。

## 测试策略

1. **纯函数单测**（`desktop/src/lib/droppedPaths.test.ts`）：单条、多条各占一行、
   含空格加引号、不含空格不加引号、光标处插入、空 draft、caret 越界夹取、
   空数组无变化、路径首尾空白。
2. **hook 单测**（`desktop/src/hooks/useDragDropPaths.test.ts`）：
   注入假 `subscribe`：`enter` 命中 → active；`over` 移出 → 不 active；
   `drop` 命中 → 回调收到 paths 并清 active；`drop` 未命中 → 不回调；
   `leave` → 清 active；`isRemote` → 完全不 subscribe；卸载 → 调用 `unlisten`。
3. **App / MessageInput 集成**（jsdom）：mock 动态 import 的 webview 模块
   （与 `desktop/src/App.test.tsx:102` mock dialog 同一套路），断言 drop 后
   当前会话 `draft` 含该路径，且切到别的会话时不被污染。
4. **回归**：`npx vitest run src/components/MessageInput.test.tsx`（受控行为不变）。

真机验证（不进 CI）：`npm run tauri dev`，从 Finder 拖文件与文件夹、
拖到卡片外、Retina 屏下判定正确性、含空格目录名。

## 非目标

- 不做附件 chip / 图片摄取（拖入的 pdf、png 一律只录路径）。
- 不做 HTML5 `dataTransfer` 路径（Tauri 下拿不到真实绝对路径）。
- 不做拖拽预览图、幽灵元素、多文件聚合 chip。
- 不做路径解引用（符号链接）、不做 tilde 展开、不做相对路径。
- 不涉及 TUI、服务端、协议。

## 验收标准

- 拖一个文件夹到输入卡片上 → 输入框出现该文件夹绝对路径，卡片高亮在拖入前可见。
- 拖到卡片外（侧栏/消息区）→ 无反应。
- 一次拖多条 → 各占一行。
- 目录名含空格 → 路径被双引号包住。
- 切会话再切回 → 路径仍在原会话草稿里。
- 远端（iOS）构建：拖拽不触发任何行为、不新增异常。
- `cd desktop && npx tsc --noEmit && npx vitest run` 全绿。
