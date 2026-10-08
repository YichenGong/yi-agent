# 手机端会话面板长按（= 右键）设计

**目标：** 配对后的 iPhone 客户端上，左侧会话面板的**工作区分组头**（文件夹那行）
与**会话行**都支持长按，弹出与桌面右键等价的操作菜单，让「长按 = 右键」在触屏上成立。

**状态：** 设计已确认，待转实现计划。

**范围：** 仅前端 `desktop/`。新增一个手势 hook、改造 `ThreadSidebar` 的菜单状态与
挂点、补一条 `[data-mobile]` 下的 CSS。

**不做** 见 §9。

---

## 1. 问题与已核实事实

用户反馈：手机 APP 左侧 session 面板**长按不支持电脑上的右键功能**，比较鸡肋。
核实后，这个面板在触屏上有**两处**能力缺失，且都能用同一套「长按」补上：

- **组头的右键菜单触达不到。** 面板里唯一带「电脑右键功能」的地方是工作区分组头：
  `ThreadSidebar.tsx:615` 的 `onContextMenu`，弹出三项菜单
  （`New thread here` / `创建或移除 Superpowers 看板` / `Remove from list`，
  `ThreadSidebar.tsx:652`–`:697`）。但 iOS 的 WKWebView 长按**不派发 `contextmenu`**，
  而是弹文字选择/放大镜 callout，因此手机上这个菜单**没有任何进入路径**。
- **会话行的行内操作触达不到。** 会话行本身桌面就没有右键菜单；它的「置顶 / 删除」
  按钮是 `group-hover` 才显示的（`ThreadSidebar.tsx:439`、`:454`）。而 Tailwind v4
  把 `hover:` 变体编译成 `@media (hover: hover)`（已在
  `node_modules/tailwindcss/dist/lib.mjs` 的 variants 定义中核实；编译产物
  `desktop/dist/assets/index-DBe2qWck.css` 里可 grep 到 `hover:hover`）。iPhone 报
  `hover: none`，故这两个按钮在手机上初始就是 `opacity-0 pointer-events-none`，
  **永远不显示也点不到**；改名只有双击（`ThreadSidebar.tsx:410`），触屏上也不可靠。

其它已核实事实：

- 手机端布局由 `useIsMobile()`（`desktop/src/lib/useIsMobile.ts`）驱动，它把
  `<html data-mobile="true">` 写到根节点；`App.tsx:154` 已持有 `isMobile`。
- 手机端侧栏是左侧抽屉：`[data-mobile="true"] .app-sidebar`（`index.css:84`）。
- 现有面板只有一个自绘菜单状态：`contextWs`（组菜单，`ThreadSidebar.tsx:142`）+
  `newMenuOpen`（新建下拉，`ThreadSidebar.tsx:141`），二者互斥，`closeMenus`
  （`:200`）与 Escape effect（`:265`）负责关闭。
- 抽屉的滚动监听挂在 `.app-sidebar aside`（`index.css:160`），滚动容器是
  `aside > div.flex-1.overflow-y-auto`（`ThreadSidebar.tsx:585`）。
- 仓库**无任何既有长按实现**（全仓 grep `longpress`/`touchstart` 为空）。

## 2. 关键决策（逐条已确认）

1. **长按挂在「组头」与「会话行」两个目标上**，语义分别是：
   - 组头长按 → 打开**桌面右键那份一模一样的三项组菜单**（同一状态、同一渲染）。
   - 会话行长按 → 打开一个**新的会话菜单**：`重命名` / `置顶·取消置顶` / `删除`。
   - 置顶区、看板会话行走同一套会话行逻辑（`renderThread` 共享），一并覆盖。
2. **不做「整组区域长按」**：若整组区域长按都出文件夹菜单，就会与会话行的会话菜单
   冲突。定为一处一义：行 → 会话菜单，组头 → 文件夹菜单。
3. **基于 Pointer Events 自实现，不依赖 `contextmenu`**：iOS 不派发、Android 会派发，
   行为不一致；Pointer Events 两端一致且能精确控制取消条件。
4. **阈值 500ms / 位移 10px**；取消条件见 §4。
5. **触发后吞掉紧跟的 click**，避免长按会话行又触发 `onSelect`（即一次多余的
   `thread/resume`）。
6. **手机端才挂长按**（由 `isMobile` 门控）；桌面右键、hover 按钮、拖拽行为
   逐字节不变。

## 3. 方案总览

三块，边界清晰、各自可测：

```
新 hook  desktop/src/lib/useLongPress.ts   —— 手势识别（纯计时/阈值 + 最小 DOM 监听）
   ↑ 复用
ThreadSidebar.tsx                          —— 两个长按挂点 + 统一菜单状态与渲染
   ↑ 门控
index.css  [data-mobile="true"]            —— 抑制 iOS 长按的原生 callout/文字选择
```

`useLongPress` 只回答「这是一次长按」，不认识 thread/workspace；`ThreadSidebar`
只把长按转成「打开哪个菜单」，不关心怎么识别手势；CSS 只负责让原生手势别抢。
三者可独立测试与修改。

## 4. 手势 hook 契约（`src/lib/useLongPress.ts`）

基于 Pointer Events。

**配置：**

- `enabled`：为 false 时不装任何监听（桌面端即此路径）。
- `thresholdMs` 默认 500，`moveTolerancePx` 默认 10。
- `onLongPress()`：到点且未取消时回调一次。

**触发：** `pointerdown`（主指针）起，`setTimeout(thresholdMs)`；到点时若未取消 →
调 `onLongPress()`，并置「吞 click」标记。

**取消（以下任一即放弃，绝不触发）：**

- `pointerup` / `pointercancel`（有滚动时系统会发 `pointercancel`）。
- 位移超过 `moveTolerancePx`（`pointermove` 时按起点算）。
- 第二根指针按下（多指 → 不当作长按）。
- `window` 失焦 / 组件卸载（清 timer）。

**与滚动的配合：** **不** `preventDefault` pointerdown（否则会破坏抽屉滚动）；
滚动由 `pointercancel` 自然取消长按。触发长按**不**阻止后续滚动。

**吞 click：** 触发后用一次性 `click` 捕获监听吞掉紧随的那一次 click（并在短超时后
自动失效，避免标记悬挂到下一次无关点击）。

**返回值：** 一组可直接展开到目标元素的 props
（`onPointerDown`/`onPointerMove`/`onPointerUp`/`onPointerCancel`），外加
`consumeClick` 逻辑内置。hook 不 `preventDefault`，不写全局样式。

**可测性：** 计时与阈值判定抽为不依赖 DOM 的纯逻辑；组件侧通过
`vi.useFakeTimers()` + `fireEvent.pointer*` 驱动。

## 5. 菜单状态与渲染（`ThreadSidebar.tsx`）

**状态提升。** 把现有 `contextWs: string | null` 提升为一个联合：

```ts
type SidebarMenu =
  | { kind: "group"; ws: string }
  | { kind: "thread"; id: string }
  | null;
```

`newMenuOpen` 与新 `menu` **三者互斥**；`closeMenus`（`:200`）与 Escape effect
（`:265`）同步扩展为「关掉全部」。组头现有 `onContextMenu` 与键盘 Enter/Space
路径（`:615`、`:620`）改为设置 `{kind:"group", ws}`，桌面行为不变。

**挂点。**

- 组头：桌面保留 `onContextMenu`；新增 `isMobile` 时挂 `useLongPress` →
  `{kind:"group", ws: g.workspace}`。
- 会话行（`renderThread` 内）：`isMobile && editingId !== t.thread_id` 时挂
  `useLongPress` → `{kind:"thread", id: t.thread_id}`。因 `renderThread` 被置顶区
  与会话区共用，两处一并覆盖。

**渲染。** 会话菜单复用行内已有的 `relative` 定位（`absolute left-3 top-full`）
+ 透明 backdrop，与现有组菜单同款结构（`fixed inset-0 z-10` backdrop +
  `role="menu"` 面板），保证 Escape、backdrop 点击关闭一致。

**菜单项接线（会话菜单）：**

| 项 | 动作 | 复用 |
|---|---|---|
| 重命名 | 进入编辑态 | 与双击同路径：`setEditingId(id)` + `setDraft(title ?? "")` |
| 置顶 / 取消置顶 | 切换置顶 | `onTogglePin(id, !isPinned)` |
| 删除 | 删除 | `onDelete(id)` |

三项都先 `closeMenus()` 再执行，避免菜单层残留。

## 6. 桌面不受影响

- 长按挂点由 `isMobile` 门控；`App.tsx` 已有 `useIsMobile()`（`:154`），新增
  `isMobile` prop 直接下传。为 false 时 `useLongPress` 不装监听，且不渲染会话菜单。
- 组头右键、进入编辑态的双击、hover 才显示的置顶/删除按钮、置顶拖拽排序
  （`draggable`）全部保持原样。
- 既有 `ThreadSidebar.test.tsx`（52 例，含看板/置顶/宽度/拖拽）应在桌面默认下全绿。

## 7. iOS 原生手势抑制（`index.css`，`[data-mobile="true"]` 下）

按住会话行时，iOS 会先弹文字选择/放大镜 callout、并可能起文字选择拖拽，抢走
长按。这一条是功能成立的前提。给行补一个 `data-thread-row` 标记属性，然后：

```css
[data-mobile="true"] .app-sidebar [data-thread-row] {
  -webkit-touch-callout: none;
  -webkit-user-select: none;
  user-select: none;
}
/* 重命名输入框仍是可选中文本，否则改名时选不中字。 */
[data-mobile="true"] .app-sidebar [data-thread-row] input {
  -webkit-user-select: text;
  user-select: text;
}
```

只作用于手机（桌面不匹配 `[data-mobile="true"]`，逐字节不变）。

## 8. 测试与验证

- **`src/lib/useLongPress.test.ts`**：到点触发 / 短按不触发 / 位移超限取消 /
  `pointercancel` 取消 / 第二指针忽略 / 触发后吞掉紧随 click / `enabled=false`
  不装监听 / 卸载清 timer。
- **`ThreadSidebar.test.tsx` 新增**：
  - 手机：长按会话行开菜单且 `onSelect` **不被调用**；三项菜单各自接线
    （重命名进编辑态、置顶调 `onTogglePin`、删除调 `onDelete`）；
    Escape 与 backdrop 关闭菜单。
  - 手机：长按组头开组菜单（三项可见）。
  - 桌面（`isMobile=false`）：长按**不开**任何菜单；既有右键断言不变。
- **`mobileLayout.test.ts` 新增静态门禁**：断言 §7 的 CSS 规则存在于
  `[data-mobile="true"]` 作用域下（含 `-webkit-touch-callout: none`）。
- **命令：** `cd desktop && npx tsc --noEmit && npx vitest run`。

## 9. 明确不做（YAGNI）

- 不做菜单超出滚动区时的上下翻转定位（与既有组菜单局限一致，见 §10）。
- 不给会话行补键盘入口（桌面本就没有行级键盘菜单，属既有缺口，不属本次范围）。
- 不做「整组区域长按出文件夹菜单」（与决策 2 冲突）。
- 不做长按震动反馈（`navigator.vibrate` 在 WKWebView 不可用）。

## 10. 已知限制

- 菜单定位沿用现有组菜单的做法：`absolute` 贴在被长按元素内、可能被
  `overflow-y-auto` 裁剪或顶出可视区。既有组菜单已有此局限，本次不扩大处理范围。
- 长按识别依赖 Pointer Events；若某 webview 不派发 `pointercancel`，滚动取消依赖
  位移阈值兜底（10px 内不触发）。
