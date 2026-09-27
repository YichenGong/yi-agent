# TUI Markdown 表格按终端宽度折叠设计

**目标：** TUI 中的 Markdown 表格在终端比表格窄时，**不丢内容**——要么折行加高，
要么退化为竖排记录，绝不把右侧裁掉。

**状态：** 已实现并验证（见 §6）。
---

## 1. 问题

终端宽度小于表格自然宽度时，表格右侧被永久裁掉：右边框 `│`/`┐`/`┘` 和靠右的列
完全看不见，且无法换行或滚动看到。

`docs/bug-list.md` 登记为：「TUI，表格的渲染不会随着命令行窗口的宽度折叠。导致
显示不完整」。

复现（临时探针，`render_markdown` 输出行宽 vs 请求宽度）：

```
请求 width=40  行宽=96  "┌───────────────┬──────────────────────────────────────────────┬───────────────┐"
请求 width=60  行宽=96  "┌───────────────┬──────────────────────────────────────────────┬───────────────┐"
请求 width=80  行宽=96  "┌───────────────┬──────────────────────────────────────────────┬───────────────┐"
```

经 `HistoryView` 渲染进 40×12 `TestBackend` 的真实结果（修复前）：

```
row 0: "┌───────────────┬───────────────────────"   <- ┐ 从未出现
row 3: "│ alpha-service │ Handles all inbound us"   <- 单词中间被切断
row 4: "└───────────────┴───────────────────────"   <- ┘ 从未出现
```

## 2. 根因（已核实）

1. **渲染层丢弃宽度**：`tui/markdown.rs` `flush_table()` 末尾
   `let _ = self.width;`，并用注释把这个行为**当作有意为之**：
   「We don't hard-wrap cells here; if the table is wider than the terminal,
   we let it overflow... the wrapping layer above this handles wrapping」。
2. **该前提不成立**：表格行在 `flush_line()` 的换行逻辑**之后**才由
   `flush_table()` 以完整 `Line::raw(..)` 压入，下游没有任何换行机会。
3. **每个 Line 只占 1 行高**：`tui/history.rs` 把每个 `Line` 渲染进
   `Rect { height: 1, width: text_width }`。
4. **ratatui 对超宽 Line 是截断而非换行**：
   `ratatui-0.29.0/src/text/line.rs` 注释明确「As the right side is truncated
   by the area width, only truncate the left」——area 宽度只用于左侧偏移，
   右侧被裁。

因此溢出是**永久且静默**的：不换行、不滚动、不报错，用户直接看不到右侧内容。

受影响路径不止助手回复：`tui/cost.rs` 构造一个 **6 列** Markdown 表格，
`/cost` 在窄终端同样被裁。

## 3. 现状关键事实（实现约束）

- **`render_markdown(src, width)` 已经接收宽度**（`tui/markdown.rs:15`），
  只是表格路径没用它。调用方 `HistoryCell::lines(width)`（`tui/cell.rs:72`）
  已按 `text_width` 传入正确宽度，含滚动条预留列。无需改调用链。
- **非表格 markdown 已经正确换行**（`flush_line` 覆盖呈现数学公式、CJK、
  超长单词逐字符切分）。这是表格路径的遗漏，不是系统性问题。
- **已有可复用的换行原语**：`tui/wrap.rs::wrap_by_display_width`（按显示宽度、
  保留原始字符）是权限弹窗修复时抽出的正确模型。
- **表格在 AST 层是 `Vec<Vec<String>>`**：`table_rows` 已按行列处理好，
  且 CJK 用显示宽度计入，故本设计只改渲染布局，不改解析。

## 4. 设计

`flush_table()` 改为**总是**折叠到 `self.width`，两级降级：

### 4.1 列宽收缩（能画出盒子时）

- 固定开销 = `2*num_cols`（每列两侧各 1 空格 padding）+ `num_cols + 1`（列
  分隔与首尾边框）。
- 可用内容预算 = `width - 固定开销`。自然总宽超出预算时，**从最宽的列开始**
  逐列减 1，直到放得下；每列下限 1 内容列。最宽优先使窄列保持自然宽度，
  盒子尽可能可读。
- 判定「画不出盒子」的阈值：最小盒子 `4*num_cols + 1`（每列 1 内容 + 2
  padding + 1 边框，再加 1 个收尾边框）超过 `width` 时走 §4.2。

### 4.2 竖排记录（画不出盒子时）

按行输出 `表头: 值` 记录，每条经 `wrap_by_display_width` 折到 `width`，
续行缩进 2 空格。行与行之间空一行分隔。这保证任意宽度都能完整显示。

### 4.3 单元格折行

- 单元格文本按词折行；词长不超过列宽时**不拆词**；单词超过列宽时按显示宽度
  逐字符切分（CJK/emoji 计 2 列，用 `UnicodeWidthStr`），保证列边界对齐。
- 一行的物理高度 = 该行所有单元格折行后的最大行数；短的单元格补空格，使
  **每一物理行的左右边框都对齐**。
- **对齐保持**：右对齐/居中列仍按其对齐方式补齐，故数值列在首个子行上仍对齐。

## 5. 测试

先写失败测试再实现。`tui/markdown.rs`：

1. `table_wider_than_terminal_wraps_to_fit_width` —— 40/60/80 下所有行宽 ≤ width
2. `wrapped_table_preserves_every_cell_character` —— 折行后源字符逐一存活
   （列在物理行上交错，故用字符计数多重集断言，而非子串）
3. `wrapped_table_rows_keep_aligned_left_and_right_borders` —— 行等宽且首尾都是边框
4. `table_too_narrow_for_columns_renders_vertical_records` —— 20 列下退化为竖排记录
5. `table_that_fits_keeps_box_borders_and_column_alignment` —— 放得下时行为不变
6. `narrow_table_with_cjk_wraps_at_display_width` —— CJK 按显示宽度折行且字符不丢
7. `single_column_table_keeps_words_intact_when_they_fit` —— 单列时完整词不拆

`tui/history.rs`（端到端，覆盖真实裁剪路径）：

8. `wide_markdown_table_is_not_clipped_off_the_right_edge` —— 经 `HistoryView`
   渲染进 `TestBackend` 后，`┐` / `┘` 必须可见。修复前该测试失败（正是本 bug）。

## 6. 验证

- `cargo test -p yi-agent --bin yi-agent` —— 419 passed, 0 failed
- `cargo test -p yi-agent --bin yi-agent -- tui::markdown::tests` —— 51 passed
- 反向验证：仅还原 `markdown.rs`、保留 `history.rs` 测试，测试以
  `the table's right border was clipped off` 失败，证明该测试真的覆盖本 bug。
- 目视核对 80/44/34/24/16 列宽度输出：放得下时保持盒子表格、列对齐；
  折行时行等宽、双边框完整；过窄时竖排 `模型: claude-sonnet-4-5` 记录。

## 7. 范围

**改动文件：** `tui/markdown.rs`（`flush_table` + 新增 `fit_column_widths`
`wrap_cell` `pad_cell` 辅助函数、引入 `wrap_by_display_width`）、
`tui/history.rs`（端到端回归测试）；文档 `docs/bug-list.md`、
`docs/project-management/yi-agent-tui.md`。

**不做：**

- 不改解析层（`pulldown_cmark` 事件处理、表格 AST 收集）已正确
- 不改 `HistoryCell` / `HistoryState` / 滚动与锚点逻辑
- 不改 `render_markdown` 签名与调用链
- 不引入横向滚动：折行 + 竖排降级已保证内容完整可见，横向滚动收益低而
  交互成本高（影响滚动键路由）
