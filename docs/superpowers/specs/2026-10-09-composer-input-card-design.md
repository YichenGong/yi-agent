# 桌面端输入区改造为「输入卡片」— 设计

日期：2026-10-09
状态：已批准（方案 A）
分支：`feat/desktop-composer-card`

## 1. 背景与问题

桌面端输入区（composer，`desktop/src/components/MessageInput.tsx`）当前是**框内外混排**：

```
┌──────────────────────────────────────┐   ┌───────────┐
│                                      │   │ 附加文件   │
│            textarea                  │   │  Mode     │
│            （带自己的边框）            │   │           │
│                                      │   │ [模型 ▾]  │
│                                      │   │ [ Send ]  │
└──────────────────────────────────────┘   └───────────┘
        ↑ 输入框                          ↑ 控件在框外，另起一列
```

问题：

1. **视觉零碎**：模型下拉、附加文件、Mode 与输入框是两块分离的容器，各自的边框与底色
   拼在一起，看起来像一组互不相干的控件，而不是「一个输入的地方」。
2. **横向占用**：右侧那一列在窗口变窄时先挤压 textarea（手机上真机实测只剩 55pt，
   已有 `max-md` 应对），桌面窗口缩到 720px 时同样吃紧。
3. 用户明确诉求：把「选模型」和「附加文件」挪进输入框里，更好看。

术语说明：仓库里 `inbox` 只出现在看板插件（`plugins/superpowers-kanban/.../inbox.rs`），
与 desktop UI 无关。本设计中的「框内」指 composer 输入卡内部。

## 2. 决策（方案 A：整卡输入区）

把输入区改造成**一个整卡**：textarea 无边框地放在卡内上方，底部一行工具栏；
原先框外的「附加文件」「Mode」「模型」与「Send」全部收进卡内。

```
┌─────────────────────────────────────────────────────────┐
│  [report.pdf ×]  [notes.md ×]        ← 附件 chip 在卡内   │
│                                                         │
│   Type a message… (Enter to send, Shift+Enter newline)  │
│                                    ← 无边框 textarea     │
│                                                         │
│  附加文件                        Mode  [模型 ▾]  [ Send ]│
└─────────────────────────────────────────────────────────┘
   ↑ 左：附件入口        ↑ 右组：Mode / 模型 / Send 贴右
```

布局决策（用户已确认）：

- **左组**：仅「附加文件」。
- **右组**：`Mode` → `模型 ▾` → `Send/Stop`，整体贴右（`justify-between` 两组）。
- **聚焦高亮**：焦点落在卡内任意控件（textarea 或工具栏按钮）时，**整卡边框**变成更亮的
  `border-fg-subtle`（用 `focus-within:` 实现）。整卡即「输入框」的视觉边界。

## 3. 方案细节

### 3.1 结构（`MessageInput.tsx`）

```tsx
<div className="relative border-t border-line bg-panel p-3">          {/* 不变：把 composer 钉在底部 */}
  {showPopup && <SlashPopup ... />}                                    {/* 不变，绝对定位浮层 */}
  <div className="rounded-lg border border-line-strong bg-surface
                  focus-within:border-fg-subtle transition-colors">   {/* 卡片：原 textarea 的边框搬到这里 */}
    <AttachmentChips ... />                                            {/* 移入卡内顶部 */}
    {/* 竖排：textarea 在上，工具栏是它下面**满宽的一行**。
        不能写成 `flex items-end`（那是旧布局的两列并排），否则「附加文件」会被挤到右端。 */}
    <div className="flex flex-col">
      <textarea ... className="min-w-0 w-full resize-none bg-transparent
                               px-3 pt-2 ... focus:outline-none" />     {/* 去掉自身边框/底色 */}
      <div className="flex items-center justify-between gap-2 px-3 pb-2
                      max-md:flex-wrap max-md:justify-end">
        <button aria-label="附加文件">附加文件</button>
        <div className="flex items-center gap-2">
          <ModeChip ... />
          {modelPicker}
          <button>{turnActive ? "Stop" : "Send"}</button>
        </div>
      </div>
    </div>
  </div>
</div>
```

要点：

- 卡片自身承载边框、圆角、底色与聚焦高亮；textarea 退化成无边框、透明底的文本区。
- **卡片内是竖排**（`flex flex-col`）：textarea 在上，工具栏是它下面**满宽的一行**。这里**不能**沿用旧布局的 `flex items-end gap-2`——那会把 textarea 与工具栏并排成两列，工具栏作为 flex item 只占自身内容宽，`justify-between` 便无空间可分配，「附加文件」会被挤到右端。实测（1280px 视口，`max-md` 未激活）：并排时「附加文件」距卡左缘 924px、工具栏未满宽；竖排时为 13px、工具栏满宽。
- 工具栏是**一行**（不是当前的两行两组）：`justify-between` 把「附加文件」钉在左、把 `Mode / 模型 / Send` 那组钉在右。
- textarea 的 `min-w-0` 与 `w-full` 都必要：`min-w-0` 解掉 `<textarea>` 默认 `min-width: auto`（否则窄屏下内容宽度会顶破卡片），`w-full` 让它在竖排列里占满整行。
- 卡片「随内容长高」由 textarea 的 `rows={3}` 决定（与现状一致，不引入 auto-resize）。
- `SlashPopup` 仍在卡片外层，绝对定位于 textarea 上方，位置语义不变。

### 3.2 手机端（`max-md`）

工具栏本身就是满宽的一行，窄屏只需让它**换行**（右组换行后仍贴右），无需再改列向：

```
┌─────────────────────────────┐
│ [report.pdf ×]              │
│  Type a message…            │
│                             │
│ 附加文件                     │
│      Mode [模型 ▾]   [Send] │
└─────────────────────────────┘
```

- 工具栏用 `max-md:flex-wrap max-md:justify-end`，让右侧那组在窄屏换行后仍然贴右。
- 卡片内**恒为竖排**，故 `max-md:flex-col` 与 `max-md:items-stretch` 已无必要（保留亦无害）。
- 桌面（> 768px，含可缩到 720px 的窗口）不匹配 `max-md`，视觉上就是上面那张桌面图。

### 3.3 不变项 / YAGNI

- 不改 `App.tsx` 的接线：`modelPicker`、`attachments`、`onPickFiles` 等 props 与语义全不动。
- 不改 `AttachmentChips`、`ModeChip`、`ModelPicker` 三个组件内部实现；模型下拉的浮层位置
  （`bottom-full` 向上弹出）在卡片内仍然成立，因为触发按钮仍在底部工具栏。
- 不改 `SlashPopup` 的定位与键盘交互。
- 不引入自适应高度 textarea（现状是 `rows={3}` 固定三行，本次不动）。
- 不改发送/打断的键位与鼠标语义（Enter / Shift+Enter / Stop）。

## 4. 测试

前端（vitest，`desktop/src`）：

- **既有用例保持通过**：`MessageInput.test.tsx` 的 ModeChip、附件、slash、Enter/IME、
  发送按钮禁用态等用例锚点（role/label 文案）全部不变。
- **需随结构更新的既有用例**（2 处，均靠 DOM 层级定位）：
  - `MessageInput.test.tsx:177` `const row = textarea.parentElement!` — textarea 的父节点
    从外层行变成卡片内的行，断言 `max-md:flex-col` / `max-md:items-stretch` 的位置要跟着改。
  - `MessageInput.test.tsx:185` `const controls = textarea.nextElementSibling` — 工具栏不再是
    textarea 的兄弟，而是它的兄弟节点的子节点；层级断言改到新位置上。
- **新增用例**（断类名契约，jsdom 不编译 Tailwind）：
  1. 卡片承载边框与 `focus-within` 高亮：textarea 的祖父/卡片元素含 `border-line-strong`
     与 `focus-within:border-fg-subtle`；textarea 自身**不含** `border-line-strong`。
  2. 底部工具栏是单行 `justify-between`：左组含「附加文件」，右组含 Mode 与 modelPicker。
  3. 附件 chip 行位于卡片**内部**（`attachment-chips` 是卡片的后代，而非卡片的兄弟）。
- 全量回归：`cd desktop && npx tsc --noEmit && npx vitest run`。

## 5. 验证命令

- `cd desktop && npx tsc --noEmit`
- `cd desktop && npx vitest run`
- 视觉：`cd desktop && npm run dev`，看深/浅两套主题下的卡片与聚焦高亮。

## 6. 人工验收清单

- [ ] 桌面：输入区是一个整卡，边框在卡上，textarea 无独立边框。
- [ ] 桌面：底部一行——左「附加文件」，右依次 `Mode`、模型下拉、`Send`。
- [ ] 桌面：点进 textarea 或点卡内按钮，整卡边框变亮；移出后复原。
- [ ] 桌面：选附件后 chip 出现在卡内顶部，可单独移除。
- [ ] 模型下拉向上弹出且可用；选中后生效，行为与改造前一致。
- [ ] 手机宽度：textarea 独占一行、控件在下方换行且不挤压输入框。
- [ ] 深/浅主题下边框与底色都成立。
- [ ] 720px 桌面窗口：工具栏同时容下「附加文件」+ `Mode` + 模型 + `Send`（这一行比旧的两行任一都宽），不出现挤压或换行错位。
- [ ] 多个附件 chip（≥3 个）时，chip 行与 textarea 之间不显局促（`AttachmentChips` 只有 `px-2 pt-2`、无下内边距）。
- [ ] 点「附加文件」按钮（而非 textarea）时，整卡同样高亮——`focus-within` 覆盖工具栏按钮，不只是输入区。
