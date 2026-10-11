# 会话区在旋转 / 视口高度变化后保持贴底 — 设计

日期：2026-10-11
状态：设计已确认，待转实现计划
分支：`fix/chat-scroll-on-rotate`
范围：仅前端 `desktop/`（`src/components/ChatView.tsx`、新增 `src/lib/useStickToBottom.ts` 及其测试）

---

## 1. 问题与已核实事实

用户反馈：手机 APP 里，把手机从竖向转到横向、再转回竖向之后，session 里显示的
内容**大幅偏移**。

核实后的事实：

- 会话内容由 `ChatView`（`desktop/src/components/ChatView.tsx`）渲染，它是这条
  会话**唯一的滚动容器**（`:173` 的 `overflow-y-auto`）。整个仓库里对滚动位置
  做自动管理的也**只有它一处**（`grep scrollIntoView` 仅命中 `:169`）。
- 现有的自动贴底是 `:168`–`:170`：

  ```tsx
  useEffect(() => {
    endRef.current?.scrollIntoView({ block: "end" });
  }, [items.length, lastText, error]);
  ```

  它只在**新消息 / 流式文本变化 / 出错**时把视图拉到底。旋转既不改 `items.length`、
  也不改 `lastText`、也不改 `error`，因此这个 effect **一次都不会重跑**。
- 前端**没有任何** `orientationchange` / `resize` / `visualViewport` 监听
  （`desktop/src` 全量搜索仅命中 `App.tsx:1249` 的 `visibilitychange`，那是给重连用的，
  与旋转无关）。
- 机制：旋转时竖屏动态视口高度（≈ 844）与横屏（≈ 390）差别很大，`ChatView` 的
  `clientHeight` 从 ≈ 795 变到 ≈ 341。浏览器保留的是**绝对像素** `scrollTop`。
  转回竖向、`clientHeight` 变回 ≈ 795 时，多出来的那 ≈ 4346px 就凭空多出来了：
  内容整体被推离底部，且不再是贴底状态。
- 关键差异：**Chrome / Firefox 有 scroll anchoring（滚动锚定）会自动把位置“粘”住，
  Safari / WKWebView 不实现它**。所以这个 bug 在桌面 Web 上几乎看不到，一到
  iOS 客户端（Tauri WKWebView 壳）就暴露——与用户描述一致。

### 1.1 实测证据（真实构建产物 + 真实 CSS，已复现）

用 `desktop/dist/assets/index-BCoo4IGd.css`（真正会打包进去的那份）按
`App.tsx` / `ChatView.tsx` 的真实类名复刻同构 DOM，在无头 Chrome 里**关闭
scroll anchoring**（逼近 Safari）并用 CDP 模拟竖→横→竖：

| 时刻 | scrollTop | clientHeight | 是否贴底 |
|---|---|---|---|
| 竖向（用户停在底部） | 12989 | 795 | 是 |
| 转横屏 | 8643 | 341 | 是（恰好仍是底） |
| **转回竖向** | **8643** | 795 | **否（差 4346px）** |

`scrollTop` 掉到 8643 后再没回来，差 4346px = 12989 − 8643，即用户看到的“大幅偏移”。
反向（横→竖→横）在贴底状态下不暴露；用户报的**竖→横→竖**正好命中。

对照：**开启** anchoring 时同样的操作 `scrollTop` 全程贴底（12989 → 12989），
故桌面浏览器上难以察觉。

### 1.2 诚实边界

- 无头 Chrome 无法 100% 等价 iOS：`dvh` 计算、Safari 工具栏动画都不同。本次是通过
  **关闭 scroll anchoring** 来逼近 Safari 的机制，用真实 CSS 与同构 DOM 观察滚动行为。
  “`scrollTop` 是绝对像素、Safari 不做锚定”是确定行为，机制成立。
- 现有 `ChatView.test.tsx` 用 `/** @vitest-environment jsdom */`，jsdom 无布局引擎
  （无 `clientHeight` / `scrollHeight`），故**旋转导致的偏移本身无法在单测里复现**，
  只能测本次新增的 hook 行为（见 §5）。

## 2. 目标与非目标

**目标：** 视口高度变化（旋转、`dvh` 变化、键盘顶起、窗口拖拽）后，会话区保持用户的
“贴底意图”——**原本贴底的保持贴底；原本在翻历史的保持大致位置，不被拽走**。

**非目标（YAGNI）：**

- 不做真正的逐 item 锚定（“同一个 item 仍在视口顶部”）。贴底意图已覆盖用户报的场景，
  逐 item 锚定需记住锚点并在布局变化后重算偏移，复杂度高且对贴底场景反而不如本方案自然。
- 不改其余滚动区（设置面板正文、`GitDiffView`、`SubagentTrace`、侧栏会话列表、
  工具卡内 `pre`）。它们旋转时同样会丢位置，但各期望语义不同，另立条目。
- 不改 `useIsMobile` 的移动端判定、不动 iOS 布局 CSS。
- 不引入第三方依赖。

## 3. 决策与取舍

- **范围：所有客户端统一启用同一套逻辑**（桌面与 iOS 同一份代码）。理由：桌面端
  （macOS WKWebView）拖拽改变窗口高度时存在同样问题，“原本贴底保持贴底、原本看历史
  不动”在桌面端也是更好的行为，且能统一测试、统一回归。
  说明：这条与本仓库对桌面端常用的“逐字节不变”约束方向相反，是**有意**选择；
  逐字节不变约束的是**渲染结构 / 文案 / 既有测试断言**，本方案不改这些
  （只新增一个 hook 与一个元素 `ref`），故与那些约束不冲突（见 §6 回归期望）。
- **信号：容器自身的 `ResizeObserver`**（而非 `window.resize` / `orientationchange`
  / `visualViewport`）。理由：旋转、`dvh` 变化、键盘顶起都会改变容器盒高，以容器
  `clientHeight` 为唯一事实来源最准；`window.resize` 在 iOS 上时序偏抖且不覆盖
  “父容器变化”类的高度突变。iOS 14.0 部署目标 ≥ `ResizeObserver` 所需的 13.4，
  无需回退。
- **判定“是否贴底”用容器自身**：`scrollHeight − clientHeight − scrollTop < 阈值`。
  阈值取一个小的像素容差（见 §4 常量），避免因亚像素/抗锯齿产生误判。
- **贴底意图的来源**：容器的 `scroll` 事件里持续维护 `atBottomRef`。旋转本身**不派发**
  `scroll`（已实测），因此该 ref 恰好保留了“旋转前是否贴底”的状态。

## 4. 设计

### 4.1 新增 `desktop/src/lib/useStickToBottom.ts`

一个可复用 hook，接口：

```ts
export const STICK_BOTTOM_THRESHOLD_PX = 4; // 判定“贴底”的容差

export function useStickToBottom<T extends HTMLElement>(): {
  ref: RefObject<T | null>;   // 挂到滚动容器上
  scrollToBottom: () => void; // 命令式贴底（供流式 effect 复用）
};
```

职责与内部结构（保持小、单一职责、可单测）：

1. 用 `ResizeObserver` 观察**容器本身**，以 `entry.contentRect.height` 的**变化**作为
   “高度变了”的信号。注意该容器带有 `py-3`（上下各 12px padding），而 `contentRect.height`
   是**内容盒**高度、比 `clientHeight`（含 padding）小一个常量（24px）——它只用作
   **变化信号**（增量与 `clientHeight` 相同），不用于贴底判定；贴底判定一律直接读
   `el.clientHeight` / `el.scrollHeight` / `el.scrollTop`（见第 2 点），避免两套坐标混淆。
2. `atBottomRef`：初始为 `true`（首屏按贴底处理，与现状一致）；容器 `scroll` 事件里
   更新为 `scrollHeight − clientHeight − scrollTop <= STICK_BOTTOM_THRESHOLD_PX`
   （内容不足一屏时 `scrollHeight <= clientHeight`、`scrollTop = 0`，该式恒为真，
   视为贴底）。`ResizeObserver` 挂载时的**首次回调只用于初始化基准高度**，不算
   “高度变化”，不再额外贴底——首屏贴底仍由既有流式 effect 负责。
3. 高度变化、且**变化前** `atBottomRef` 为 `true` 时，在布局落定后重新贴底。为避免
   iOS 上 `dvh` 变化晚于 `ResizeObserver` 回调一帧，用 `requestAnimationFrame` 延后一帧
   再赋值 `scrollTop = scrollHeight`。
4. 合并突发回调：若一帧内多次触发（旋转过程中 iOS 会连续吐几个尺寸），用单个挂起的
   rAF 去重，避免抖动。
5. `scrollToBottom` 是给既有流式逻辑复用的命令式贴底（等价于当前
   `endRef.current.scrollIntoView({ block: "end" })`，改直接赋值 `scrollTop = scrollHeight`；
   仓库无 `scroll-behavior: smooth`（已核实），故为即时、无动画）。
6. 卸载时 `observer.disconnect()` 并移除 `scroll` 监听、取消挂起的 rAF。

### 4.2 改造 `desktop/src/components/ChatView.tsx`

- 用 `useStickToBottom` 取得 `ref` 与 `scrollToBottom`，把 `ref` 挂到滚动容器
  （`:173` 的 `<div>`）。
- 现有流式 effect（`:168`–`:170`）改为调用 `scrollToBottom()`，**依赖数组保持不变**
  （`[items.length, lastText, error]`）：它仍负责“有新内容就贴底”，本次新增的是
  “高度变化时也维持贴底”。
- 移除现在仅用于流式贴底的末尾锚点 `<div ref={endRef} />`（`:197`）；若它还被别处
  引用则保留，仅去掉贴底用途。
- 不改容器类名（`:173` 的 `flex min-w-0 flex-1 flex-col overflow-x-hidden overflow-y-auto px-4 py-3`），
  “原本在翻历史”时**不做任何事**，位置交给浏览器（`dvh` 变化时的相对保持与桌面一致）。

### 4.3 数据流

```
用户滚动 ──▶ scroll 事件 ──▶ atBottomRef = (距底 <= 阈值)
旋转/视口变化 ──▶ 容器 ResizeObserver ──▶ 高度变化?
                                      └─ atBottomRef 为真 ──▶ rAF 一帧后 scrollTop = scrollHeight
新消息/流式文本 ──▶ 既有 effect ──▶ scrollToBottom()
```

## 5. 测试与验证

**单元测试 `desktop/src/lib/useStickToBottom.test.ts`**（jsdom + 可控桩）：

- `ResizeObserver` 用可手动触发的桩：断言“高度变化且 `atBottomRef` 为真”时贴底
  （`scrollTop === scrollHeight`）；“高度变化但 `atBottomRef` 为假”时**不**动
  `scrollTop`。
- `scroll` 事件把 `atBottomRef` 更新正确：贴底为真、上滚一段为假、再回到底部为真。
- `scrollToBottom()` 命令式贴底生效（`scrollTop === scrollHeight`）。
- 同一帧内多次尺寸回调只触发一次贴底（rAF 去重）。
- 卸载后不再响应（`observer.disconnect` / 监听移除）。

**回归测试：**

- 现有 `desktop/src/components/ChatView.test.tsx` 必须**原样通过**（含
  “keeps the streamed text visible” 与横向溢出裁剪那条）。该文件已对
  `Element.prototype.scrollIntoView` 打桩；改造后该桩可能不再被使用，**不得**因此
  让文件里的断言变红——必要时仅更新 test stub 使其与新实现一致，不改断言语义。

**命令：**

- `cd desktop && npx vitest run src/lib/useStickToBottom.test.ts src/components/ChatView.test.tsx`
- `cd desktop && npx tsc --noEmit && npx vitest run`

**真机/真实引擎（人工，落计划时作为验收项）：** 在 iOS 客户端（或可旋转的 WKWebView
宿主）验证：贴底时竖→横→竖后仍贴底；上翻历史后旋转，位置大致不变、不被拽到底部。
本轮排查用的无头 Chrome 复现台（真实 CSS + 同构 DOM + 关 anchoring）可作过程证据，
但**不作为**最终判据。

## 6. 回归与兼容性

- 不改 `App.tsx` 的会话渲染结构与任何文案。
- 不匹配 `[data-mobile]` 的新增 CSS（本次**不加** CSS）。
- 桌面端行为变化仅限：窗口高度变化后，原本贴底的会话会重新贴底（新行为，符合预期）；
  其余滚动区与所有既有断言不变。
- 无新增 npm 依赖。

## 7. 明确不做（YAGNI）

- 不做逐 item 相对位置锚定。
- 不修其余滚动容器。
- 不做“工具栏伸缩”时的逐像素补偿（`ResizeObserver` 已能覆盖高度变化带来的贴底维持）。
- 不为 `ResizeObserver` 缺失做 polyfill / 回退（部署目标 iOS 14.0 已支持）。

## 8. 已知限制

- 单测环境无布局引擎，旋转偏移只能在真实引擎/真机验证（§5）。
- 阈值 `STICK_BOTTOM_THRESHOLD_PX` 取 4px；若个别设备出现“差 1–2px 就不算贴底”的回弹，
  可在计划阶段按实测微调该常量。
