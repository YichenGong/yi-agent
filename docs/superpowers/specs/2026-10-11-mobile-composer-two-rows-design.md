# 手机端输入区工具栏：压到两行 + Send 右对齐 — 设计

日期：2026-10-11
状态：设计已确认（方案 A）
分支：`fix/mobile-composer-two-rows`
范围：仅前端 `desktop/`（`MessageInput.tsx`、`ModelPicker.tsx`、两处测试、`docs/bug-list.md`、`docs/project-management/desktop.md`）

---

## 1. 问题与已核实事实

用户反馈：手机端输入框的四颗控件（附加文件 / Mode / 模型 / Send）占了**三行**，浪费布局
空间；希望「尽可能排在两行」，且「发送按钮向右对齐」。

核实后的事实（含实测量化）：

- 工具栏结构（`desktop/src/components/MessageInput.tsx:294`）：外层 `flex-wrap`，内含
  两颗平级元素——「附加文件」按钮，以及承载 `ModeChip` / 模型下拉 / `Send` 的**一组**
  （`:313`）。该组带 `max-md:basis-full`（手机端独占整行）。
- 因此手机端固定为：第 1 行「附加文件」；第 2 行起是该组。组内三件**不可压缩**：
  模型名标签上限 `max-w-[16rem]`（256px，`desktop/src/components/ModelPicker.tsx:141`）、
  Mode chip（~56px）、`px-4` 的 Send（~66px），加两处 `gap-2`（16px）≈ 394px。
- 390pt iPhone 的卡片内宽 ≈ 342px（卡片左右各 12px 外边距 + 内边距，实测 366px 卡宽、
  内可用 ≈ 342px），394 > 342，于是组内再折一次：第 2 行「Mode + 模型」，第 3 行 Send。
- Send 之所以不贴右，是因为组内是 `flex-start`（此前的修复把对齐落在了外层工具栏的
  `max-md:justify-start` 上，而非 Send 自身）。
- 真实 Chrome（CDP `Emulation.setDeviceMetricsOverride`）实测**修复前**：
  390pt = 3 行（Send 独占第 3 行、距卡片右缘 287px）；320pt = 4 行。

## 2. 决策（方案 A）

1. **模型名标签手机端收紧到 `max-md:max-w-[8rem]`**：三件得以在同一行放下（256→128px，
   394→266px ≤ 342px）。
2. **Send 加 `max-md:ml-auto`**：把它钉到该行右缘。
3. **右对齐只作用于单颗 Send，不给整组 `justify-end`**：整组 `justify-end` 会把最左的
   `ModeChip` 连同它 `left-0` 的下拉一并右移，重蹈此前「ModeChip 出画面」的覆辙；且整组
   换行时未被吸收的空行会把单件行推回左端。
4. **桌面端（≥768px）只加 `max-md:` 变体，逐字节不变。**

最终布局：**2 行** —— 第 1 行「附加文件」，第 2 行「Mode / 模型 …… Send（右缘）」。

被否的备选（方案 B）：取消组的 `basis-full`，把「附加文件 + Mode」放第 1 行。需要额外把
模型下拉从 `right-0` 改为手机端左展（`max-md:right-auto max-md:left-0`），复现前一处 bug
的风险点且需更多真机几何验证，收益（第 1 行更满）不抵风险。

## 3. 分层设计

### 3.1 `MessageInput`（`desktop/src/components/MessageInput.tsx`）

- Send 的两支 className（`turnActive` 红 / 蓝态）各追加 `max-md:ml-auto`。
- 更新工具栏注释，写明「两行 + Send 右对齐」及为何不给整组 `justify-end`。

### 3.2 `ModelPicker`（`desktop/src/components/ModelPicker.tsx`）

- 触发按钮 className 追加 `max-md:max-w-[8rem]`（`max-w-[16rem]` 保留为桌面值）。

### 3.3 不改的部分

`data-composer-toolbar` 的 `max-md:flex-wrap max-md:justify-start`、承载组的
`max-md:basis-full max-md:flex-wrap max-md:min-w-0`、ModeChip、发送/命令逻辑、后端，全部不动。

## 4. 边界与不变量

- 桌面端：模型名保持完整宽度（`max-w-[16rem]`）、单行布局、类名逐字节不变。
- ModeChip 出屏修复（前一处）不回退：右对齐只挪 Send，chip 仍贴左。
- 极窄屏（320pt）：`max-md:flex-wrap` 兜底仍在，若极端情况放不下会再折行，不会横向溢出。

## 5. 判据

- `cd desktop && npx vitest run src/components/MessageInput.test.tsx src/components/ModelPicker.test.tsx`
  —— 新增「pins Send to the phone line's right edge…」与「caps the model label width on
  the phone…」；回退 `max-md:ml-auto` 或 `max-md:max-w-[8rem]` 即红。
- `cd desktop && npx vitest run && npx tsc --noEmit && npm run build`。
- 真实 Chrome（CDP 手机视口）实测 320/360/375/390/393/430pt 一律 2 行、Send 距卡片右缘
  13px（= 内边距）；桌面 768/1024/1440pt 仍 1 行。
- 产物 CSS 中 `max-width:8rem` 与 `margin-left:auto` 均落在
  `@media not all and (width>=48rem)` 内。
