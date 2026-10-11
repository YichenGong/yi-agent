# 手机端：去掉左侧菜单竖条，改为浮动菜单按钮 — 设计

日期：2026-10-11
状态：已实现（方案 A），待合并
分支：`feat/mobile-floating-sidebar-toggle`
范围：仅前端 `desktop/`（`App.tsx`、`index.css`、两条测试）

---

## 1. 问题与已核实事实

用户反馈：手机 APP 上，折叠（抽屉收起）状态下左侧那条**竖条占位区**很占地方，
理论上只需要那颗菜单按钮，按钮下面那截竖条应该去掉。

核实后的事实：

- 手机端（`isMobile` 为真，即配对的 iPhone 客户端）的抽屉入口是
  `App.tsx:1724`–`1738` 的 `<button aria-label="会话列表">`，class 为
  `app-mobile-toggle flex w-11 shrink-0 items-start justify-center border-r border-line bg-panel pt-2 text-fg-muted hover:text-fg`。
- 它是 `flex-row`（`App.tsx:1723`）里的一个 **in-flow、通栏高** 的列：
  `w-11` = 44px、`shrink-0`、`items-start` + `pt-2`。**按钮只坐在最顶端，下面整段
  都是这条 44px 宽竖条的空白**，右侧还有 `border-r border-line` 一条竖线。它常驻文档
  流，因此**永远吃掉约 44px 横向宽度**（390pt 的屏上聊天列只剩约 346pt）。
- 这 44px 确实是纯占位：抽屉本身（`.app-sidebar`）在手机上被 `index.css:105`–`113`
  改为 `position: fixed` 的离屏抽屉，收起时 `translateX(-100%)`，**不占任何 flex
  宽度**。唯一真正占宽的只有这条竖条。抽屉的展开入口也只有它。
- 历史成因：竖条与 `env(safe-area-inset-top)` 补白是同一笔引入的
  （`ec9bcad7`「phone layout — session drawer」，随后 `678aaa70` 给 `body` 也加了
  同一段安全区）。两处叠加导致手机上按钮图标被下推了**两倍**顶部安全区——这条也一并修掉
  （见 §3）。

## 2. 决策（方案 A：浮动按钮，取消竖条列）

把入口从「列内竖条」改为**浮在聊天区左上角的一颗按钮**：

- 竖条列整块移除 → 聊天列拿满整宽（横向省下 44px）。
- 按钮用绝对定位浮在主列左上角，不占独立列，纵向也省下按钮下方的全部空白。
- 按钮仍只在 `isMobile` 渲染；桌面端不渲染该节点，逐字节不变。

被否的备选（仅记录）：B「聊天列顶部一条细横栏」横向拿满但顶部多约 40px 横条；
C「放进已有 StatusBar 那一行」不遮内容但位置偏下、不显眼。

## 3. 改动

### 3.1 `App.tsx`

1. 从 `flex-row` 中移除 `isMobile &&` 的 in-flow `<button>`（现 `1724`–`1738`）。
2. 在主列容器（现 `1786` 的
   `<div className="relative flex min-w-0 flex-1 flex-col">`）**内**新增同一颗按钮，
   改为绝对定位浮层：

   ```
   absolute left-1 top-1 z-10 flex size-11 items-center justify-center
   rounded-md bg-panel/80 text-fg-muted hover:text-fg
   ```

   保留原属性：`type="button"`、`aria-label="会话列表"`、`aria-expanded={sidebarOpen}`、
   `onClick={() => setSidebarOpen(true)}`，以及原汉堡图标 `<svg>`（`size-5`）。
3. 给主列加一个 `app-main` class（桌面端无对应 CSS，零影响），供 §3.2 的让位规则挂载。
4. 更新 `1730`–`1731` 那段说明 44px 的注释：它现在是浮层，不再是一条竖条。

**按钮底色（已定）**：`bg-panel/80` + `rounded-md`。滚动时内容从按钮下方经过，淡底 +
圆角让图标始终可读，也不遮住下面文字的可辨识轮廓。此点若你想改成完全无底色，实现时
去掉 `bg-panel/80` 即可，其余不变。

### 3.2 `index.css`

1. **删除** 已失效的规则：

   ```css
   .app-mobile-toggle { padding-top: env(safe-area-inset-top, 0px); }
   ```

   这条顺带修掉隐性重复：`[data-mobile="true"] body`（`index.css:131`）已加同一段
   `env(safe-area-inset-top)`，竖条按钮再加一遍 = 手机上图标下推两倍安全区。
2. **新增** 让位规则（放在既有 `[data-mobile="true"]` 分组内）：

   ```css
   [data-mobile="true"] .app-main { padding-top: 48px; }
   ```

   按钮 `top`(4px) + 高 44px = 48px，正好落在这段空白内。首条消息与看板顶栏都不会被
   按钮压住。桌面端不匹配该选择器，逐字节不变。

### 3.3 测试

- `App.test.tsx` 的三条手机端用例（`:2210` keep-closed、`:2227` closes-on-select、
  `:2238` desktop-untouched）引用的 `会话列表` / `.app-sidebar` /
  `.app-sidebar-backdrop` **全部保留**，无需改动。
- `mobileLayout.test.ts`（读真实 `index.css` 的静态门禁）**新增一条**断言：真实产物里
  存在 `[data-mobile="true"] .app-main { … padding-top … }`，锁住「首条消息不被浮动
  按钮压住」。
- `mobileLayout.test.ts` 如需，可再加一条断言：`index.css` 中**不再**存在
  `app-mobile-toggle` 的 `padding-top` 规则（防止重复安全区回归）——可选。

## 4. 边界情况

- **看板页**：`selectedBoard !== null` 时主列是看板 `section`，其顶栏 padding 约 12px，
  会被 `.app-main` 的 48px 内边距向下推，按钮下沿（48px）不再压住它。行为正确。
- **浮层 `z-10` 与对话框**：审批弹窗/设置弹窗/配对页若用高于 10 的 z-index，浮层按钮
  在其之下，不抢焦点、不遮挡；`z-10` 仅高于聊天/看板内容，符合「浮在内容上方」的预期。
- **横屏**：`[data-mobile="true"]` 在横屏同样成立，浮层按钮与让位规则照常适用，
  不新增 `@media` 分叉；安全区因 `viewport-fit=cover` 左右生效，与本次改动无冲突。
- **锚点**：主列 `relative`；抽屉展开时 `.app-sidebar` 是 `position: fixed`，不参与主列
  布局，故按钮绝对定位始终相对主列左上角，位置稳定。

## 5. 不做

- 不动抽屉开合、背板、选中即收、长按菜单、宽度拖拽等任何行为。
- 不新增/删除颜色 token；只用既有 `--panel`。
- 不改桌面端任何像素或节点。
- 不引入 `@media` 断点，沿用 `[data-mobile="true"]` 单一开关。

## 6. 验收标准

1. 手机端抽屉收起时，左侧**不再有 44px 竖条**（无 `w-11` 列、无 `border-r` 竖线），
   聊天区横向拿满整宽。
2. 手机端左上角有且只有一颗 44px 的菜单按钮，点击仍能打开抽屉；`aria-label="会话列表"`
   与 `aria-expanded` 正确。
3. 首条消息 / 看板顶栏不被浮动按钮遮挡（`.app-main` 顶内边距 48px）。
4. 手机端按钮图标不再被下推两倍安全区（`app-mobile-toggle` 补白规则已删）。
5. 桌面端：不渲染按钮、无 `app-main` 规则命中，布局与行为逐字节不变。
6. `mobileLayout.test.ts` 新断言通过；`App.test.tsx` 既有手机端用例全绿。
