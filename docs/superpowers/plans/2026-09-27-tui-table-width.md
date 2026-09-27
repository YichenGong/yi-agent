# TUI Markdown 表格宽度折叠实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `render_markdown` 的表格渲染总是折进请求宽度，消除右侧永久裁剪。

**Architecture:** `flush_table()` 改为宽度感知：先按最宽列优先收缩列宽使盒子放得下，
再把超宽单元格按显示宽度折成多个物理行（对齐保持）；若宽度连最小盒子都放不下
（`4*num_cols+1 > width`），退化为竖排 `表头: 值` 记录。全部改动限于渲染层。

**Tech Stack:** Rust、pulldown-cmark、ratatui、unicode-width、cargo test。

## Global Constraints

- 工作目录：`.worktrees/fix/tui-table-width`（本计划所有命令在此目录下执行）。
- 测试命令形如：`cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent -- <filter>`。
- 不改 `render_markdown` 签名、不改解析路径（`table_rows` 收集）、不改
  `HistoryCell` / `HistoryState` / 滚动逻辑。
- 提交前 `cargo fmt --all`（`yi-agent-rs/` 下）。
- 设计依据：`docs/superpowers/specs/2026-09-27-tui-table-width-design.md`。

---

### Task 1: 表格折叠（含竖排降级）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/markdown.rs`
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: `tui/wrap.rs::wrap_by_display_width(text, width, first_prefix, cont_prefix) -> Vec<String>`
- Produces:
  - `fit_column_widths(natural: &[usize], table_width: usize) -> Vec<usize>`
  - `wrap_cell(text: &str, width: usize) -> Vec<String>`
  - `pad_cell(segment: &str, width: usize, align: Alignment) -> String`
  - `LineBuilder::push_vertical_records(&mut self, rows: &[Vec<String>], alignments: &[Alignment])`

- [ ] **Step 1: 写失败测试**

在 `markdown.rs` 的 `mod tests` 末尾（`mod tests` 的收尾 `}` 之前）加入：

```rust
    /// Render a markdown table and return one string per display line.
    fn table_lines(lines: &[Line<'static>]) -> Vec<String> {
        lines.iter().map(spans_text).collect()
    }

    const BOX_CHARS: [char; 11] = ['─', '│', '┌', '┬', '┐', '├', '┼', '┤', '└', '┴', '┘'];

    /// 渲染结果的字符计数，排除边框与所有空白（折行会补 padding）。
    fn table_chars(rendered: &[String]) -> std::collections::BTreeMap<char, usize> {
        let mut counts = std::collections::BTreeMap::new();
        for ch in rendered.iter().flat_map(|line| line.chars()) {
            if ch.is_whitespace() || BOX_CHARS.contains(&ch) {
                continue;
            }
            *counts.entry(ch).or_insert(0) += 1;
        }
        counts
    }

    /// 源单元格字符计数，忽略空格（不含 markdown 语法）。
    fn source_chars(cells: &[&str]) -> std::collections::BTreeMap<char, usize> {
        let mut counts = std::collections::BTreeMap::new();
        for ch in cells.iter().flat_map(|cell| cell.chars()).filter(|ch| *ch != ' ') {
            *counts.entry(ch).or_insert(0) += 1;
        }
        counts
    }

    const WIDE_TABLE_HEADER: [&str; 3] = ["Name", "Description", "Owner"];
    const WIDE_TABLE_ROW: [&str; 3] = [
        "alpha-service",
        "Handles all inbound user authentication and session rotation",
        "platform-team",
    ];

    fn wide_table() -> String {
        format!(
            "| {} | {} | {} |\n| --- | --- | --- |\n| {} | {} | {} |\n",
            WIDE_TABLE_HEADER[0], WIDE_TABLE_HEADER[1], WIDE_TABLE_HEADER[2],
            WIDE_TABLE_ROW[0], WIDE_TABLE_ROW[1], WIDE_TABLE_ROW[2],
        )
    }

    #[test]
    fn table_wider_than_terminal_wraps_to_fit_width() {
        // 自然宽度约 96 列，任何更窄的终端都必须折进去而不是被裁。
        for width in [40u16, 60, 80] {
            for line in table_lines(&render_markdown(&wide_table(), width)) {
                let w = UnicodeWidthStr::width(line.as_str());
                assert!(w <= width as usize, "width {width}: line is {w} cols: {line:?}");
            }
        }
    }

    #[test]
    fn wrapped_table_preserves_every_cell_character() {
        let rendered = table_lines(&render_markdown(&wide_table(), 40));
        let source = source_chars(
            &WIDE_TABLE_HEADER.iter().chain(WIDE_TABLE_ROW.iter()).copied().collect::<Vec<_>>(),
        );
        assert_eq!(table_chars(&rendered), source, "content changed: {rendered:?}");
    }

    #[test]
    fn wrapped_table_rows_keep_aligned_left_and_right_borders() {
        let rendered = table_lines(&render_markdown(&wide_table(), 40));
        let widths: Vec<usize> = rendered
            .iter()
            .map(|line| UnicodeWidthStr::width(line.as_str()))
            .collect();
        let table_width = widths[0];
        assert!(table_width <= 40, "table too wide: {table_width}");
        assert!(widths.iter().all(|w| *w == table_width), "ragged: {widths:?}");
        assert!(rendered.iter().all(|l| matches!(l.chars().next(), Some('│' | '┌' | '├' | '└'))));
        assert!(rendered.iter().all(|l| matches!(l.chars().last(), Some('│' | '┐' | '┤' | '┘'))));
    }

    #[test]
    fn table_too_narrow_for_columns_renders_vertical_records() {
        // 6 列需 4*6+1 = 25 列最小盒子，20 列放不下，必须退化为竖排记录。
        let src = "| Model | input | output | cache_create | cache_read | calls |\n\
             | --- | ---: | ---: | ---: | ---: | ---: |\n\
             | claude-sonnet-4-5 | 1,000 | 200 | 30 | 40 | 5 |\n";
        let rendered = table_lines(&render_markdown(src, 20));
        for line in &rendered {
            let w = UnicodeWidthStr::width(line.as_str());
            assert!(w <= 20, "vertical record is {w} cols: {line:?}");
        }
        let joined = rendered.join("\n");
        assert!(!joined.contains('│'), "no box table here: {joined:?}");
        for label in ["Model:", "input:", "output:", "cache_create:", "calls:"] {
            assert!(joined.contains(label), "expected {label:?}: {joined:?}");
        }
        assert!(joined.contains("1,000"), "lost a value: {joined:?}");
    }

    #[test]
    fn table_that_fits_keeps_box_borders_and_column_alignment() {
        let src = "| Name | Age |\n| --- | ---: |\n| Alice | 30 |\n";
        let rendered = table_lines(&render_markdown(src, 40));
        assert!(rendered.iter().all(|l| UnicodeWidthStr::width(l.as_str()) <= 40));
        let data_row = rendered.iter().find(|l| l.contains("Alice")).expect("data row");
        assert!(data_row.ends_with('│'), "lost right border: {data_row:?}");
        assert!(data_row.contains("30"), "lost value: {data_row:?}");
    }

    #[test]
    fn narrow_table_with_cjk_wraps_at_display_width() {
        let src = "| 姓名 | 描述 |\n| --- | --- |\n\
             | 张三丰 | 这是一个很长的中文描述文本用于测试折行 |\n";
        let expected = source_chars(&["姓名", "描述", "张三丰", "这是一个很长的中文描述文本用于测试折行"]);
        for width in [16u16, 24] {
            let rendered = table_lines(&render_markdown(src, width));
            for line in &rendered {
                let w = UnicodeWidthStr::width(line.as_str());
                assert!(w <= width as usize, "width {width}: {w} cols: {line:?}");
            }
            assert_eq!(table_chars(&rendered), expected, "CJK content changed: {rendered:?}");
        }
    }

    #[test]
    fn single_column_table_keeps_words_intact_when_they_fit() {
        let src = "| Description |\n| --- |\n| Handles all inbound user authentication |\n";
        let rendered = table_lines(&render_markdown(src, 40));
        let joined: String = rendered.join("");
        for word in ["Handles", "all", "inbound", "user", "authentication"] {
            assert!(joined.contains(word), "lost word {word:?}: {rendered:?}");
        }
    }
```

- [ ] **Step 2: 运行测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent -- tui::markdown::tests:: 2>&1 | tail -25
```

预期：新增用例失败，报 `line is 96 cols`（40/60/80）与 `table too wide: 96` 等
裁剪类断言失败。既有表格用例仍通过。

- [ ] **Step 3: 实现**

顶部加入引入：

```rust
use super::wrap::wrap_by_display_width;
```

把 `flush_table()` 整体替换为（自然宽度计算改为 `natural_widths`，新增降级分支与
折行渲染，`data_line` 换成 `row_lines`）：

```rust
    fn flush_table(&mut self) {
        let rows = std::mem::take(&mut self.table_rows);
        let alignments = std::mem::take(&mut self.table_alignments);
        if rows.is_empty() {
            return;
        }
        let num_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        if num_cols == 0 {
            return;
        }
        let natural_widths: Vec<usize> = (0..num_cols)
            .map(|i| {
                rows.iter()
                    .filter_map(|row| row.get(i))
                    .map(|cell| UnicodeWidthStr::width(cell.as_str()))
                    .max()
                    .unwrap_or(0)
            })
            .collect();

        let table_width = self.width as usize;
        // 最小盒子：每列 1 内容 + 2 padding + 1 边框，再加 1 个收尾边框。
        let min_total = 4 * num_cols + 1;
        if min_total > table_width {
            self.push_vertical_records(&rows, &alignments);
            self.in_table = false;
            self.current_row.clear();
            self.current_cell.clear();
            return;
        }
        let col_widths = fit_column_widths(&natural_widths, table_width);

        let border = |left: char, mid: char, right: char| -> String { /* 原实现不变 */ };

        // 一个物理行 = 各列折行后同一子行拼接，短单元格补空格使边框对齐。
        let row_lines = |row: &[String]| -> Vec<String> {
            let wrapped: Vec<Vec<String>> = (0..num_cols)
                .map(|i| {
                    let cell = row.get(i).map(|s| s.as_str()).unwrap_or("");
                    wrap_cell(cell, col_widths[i])
                })
                .collect();
            let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
            (0..height)
                .map(|line| {
                    let mut s = String::from("│");
                    for (i, segments) in wrapped.iter().enumerate() {
                        let segment = segments.get(line).map(|s| s.as_str()).unwrap_or("");
                        s.push_str(&pad_cell(
                            segment,
                            col_widths[i],
                            alignments.get(i).copied().unwrap_or(Alignment::None),
                        ));
                        s.push('│');
                    }
                    s
                })
                .collect()
        };

        self.lines.push(Line::raw(border('┌', '┬', '┐')));
        for (ri, row) in rows.iter().enumerate() {
            for line in row_lines(row) {
                self.lines.push(Line::raw(line));
            }
            if ri == 0 {
                self.lines.push(Line::raw(border('├', '┼', '┤')));
            }
        }
        self.lines.push(Line::raw(border('└', '┴', '┘')));
        self.in_table = false;
        self.current_row.clear();
        self.current_cell.clear();
    }

    /// 放不下盒子时的降级布局：每行输出 `表头: 值` 记录，折到 `self.width`。
    fn push_vertical_records(&mut self, rows: &[Vec<String>], alignments: &[Alignment]) {
        let _ = alignments;
        let width = self.width as usize;
        let Some(header) = rows.first() else {
            return;
        };
        for (ri, row) in rows.iter().enumerate().skip(1) {
            if ri > 1 {
                self.lines.push(Line::raw(""));
            }
            for (i, value) in row.iter().enumerate() {
                let label = header.get(i).map(|s| s.as_str()).unwrap_or("");
                let label = if label.is_empty() { format!("col{i}") } else { label.to_string() };
                let text = format!("{label}: {value}");
                for chunk in wrap_by_display_width(&text, width.max(1), "", "  ") {
                    self.lines.push(Line::raw(chunk));
                }
            }
        }
    }
```

在 `render_math` 之前加入三个自由函数：

```rust
/// 按最宽列优先收缩自然列宽，直到整个盒子放得下 `table_width`。
///
/// 每列下限 1 内容列；固定开销为每列 2 padding + 每列 1 边框 + 1 收尾边框。
/// 从最宽列开始减可让窄列保持自然宽度。调用方已保证最小盒子放得下。
fn fit_column_widths(natural: &[usize], table_width: usize) -> Vec<usize> {
    let num_cols = natural.len();
    let overhead = num_cols * 2 + (num_cols + 1);
    let mut widths = natural.to_vec();
    let budget = table_width.saturating_sub(overhead);
    loop {
        let total: usize = widths.iter().sum();
        if total <= budget {
            return widths;
        }
        let Some(index) = widths
            .iter()
            .enumerate()
            .filter(|(_, w)| **w > 1)
            .max_by_key(|(i, w)| (**w, std::cmp::Reverse(*i)))
            .map(|(i, _)| i)
        else {
            return widths;
        };
        widths[index] -= 1;
    }
}

/// 把一个单元格折成不超过 `width` 显示列的多个子行。
fn wrap_cell(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for word in text.split(' ') {
        let word_width = UnicodeWidthStr::width(word);
        let separator = if current.is_empty() { 0 } else { 1 };
        if current_width + separator + word_width <= width {
            if separator == 1 {
                current.push(' ');
                current_width += 1;
            }
            current.push_str(word);
            current_width += word_width;
            continue;
        }
        if !current.is_empty() {
            out.push(std::mem::take(&mut current));
            current_width = 0;
        }
        if word_width <= width {
            current.push_str(word);
            current_width = word_width;
            continue;
        }
        // 单词本身超过列宽：按显示宽度逐字符切分（CJK/emoji 计 2 列）。
        for ch in word.chars() {
            let ch_width = UnicodeWidthStr::width(ch.to_string().as_str());
            if current_width + ch_width > width && !current.is_empty() {
                out.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current.push(ch);
            current_width += ch_width;
        }
    }
    if !current.is_empty() || out.is_empty() {
        out.push(current);
    }
    out
}

/// 把折行片段补齐到 `width` 显示列并施加列对齐（含两侧各 1 空格 padding）。
fn pad_cell(segment: &str, width: usize, align: Alignment) -> String {
    let segment_width = UnicodeWidthStr::width(segment);
    let pad_total = width.saturating_sub(segment_width);
    let (left_pad, right_pad) = match align {
        Alignment::Center => {
            let left = pad_total / 2;
            (left, pad_total - left)
        }
        Alignment::Right => (pad_total, 0),
        _ => (0, pad_total),
    };
    let mut out = String::with_capacity(width + 2);
    out.push(' ');
    for _ in 0..left_pad {
        out.push(' ');
    }
    out.push_str(segment);
    for _ in 0..right_pad {
        out.push(' ');
    }
    out.push(' ');
    out
}
```

- [ ] **Step 4: 运行测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent -- tui::markdown::tests:: 2>&1 | tail -20
```

预期：`51 passed`（含新增 7 个）。既有 `simple_table_renders_with_box_drawing`、
`table_preserves_cell_text_without_dropping_words` 等仍通过。

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/markdown.rs
git commit -m "fix(tui): fold markdown tables into the terminal width"
```

---

### Task 2: 端到端裁剪回归测试

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/history.rs`
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: Task 1 的渲染修复；既有 `HistoryView` 与 `TestBackend` 测试设施
- Produces: `wide_markdown_table_is_not_clipped_off_the_right_edge`

- [ ] **Step 1: 写失败的测试**

在 `history.rs` 的 `mod tests` 中，`render_overflow_reserves_rightmost_column_for_scrollbar`
之后加入：

```rust
    #[test]
    fn wide_markdown_table_is_not_clipped_off_the_right_edge() {
        // 回归：比终端宽的表格曾按每行一个超宽 Line 渲染，ratatui 从右侧截断，
        // 收尾边框与靠右单元格静默丢失。
        let mut state = HistoryState::new();
        state.push(
            HistoryCell::Markdown {
                text: "| Name | Description | Owner |\n| --- | --- | --- |\n\
                       | alpha-service | Handles all inbound user authentication | platform-team |\n"
                    .to_string(),
            },
            40,
        );

        let area_width = 40u16;
        let area_height = 20u16;
        let backend = TestBackend::new(area_width, area_height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.render_widget(
                    HistoryView { state: &state, width: area.width },
                    area,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let text_width = state.text_width(area_width, area_height);
        let rows: Vec<String> = (0..area_height)
            .map(|y| (0..text_width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let rendered = rows.join("\n");

        assert!(rendered.contains('┌'), "table never rendered: {rendered:?}");
        assert!(
            rows.iter().any(|row| row.ends_with('┐')),
            "the table's right border was clipped off: {rendered:?}"
        );
        assert!(rendered.contains('┘'), "bottom-right corner clipped: {rendered:?}");
        for (y, row) in rows.iter().enumerate() {
            assert!(
                row.trim_end().is_empty() || row.ends_with(['│', '┐', '┤', '┘']),
                "row {y} lost its right border: {row:?}"
            );
        }
        assert!(
            rendered.contains("Description") || rendered.contains("Descriptio"),
            "the wide column content was lost: {rendered:?}"
        );
    }
```

- [ ] **Step 2: 反向验证（证明测试覆盖本 bug）**

临时只还原生产代码，确认测试失败：

```bash
cd yi-agent-rs && git stash push -- crates/yi-agent/src/tui/markdown.rs \
  && cargo test -p yi-agent --bin yi-agent -- wide_markdown_table_is_not_clipped 2>&1 | tail -15 \
  && git stash pop
```

预期：`FAILED`，报 `the table's right border was clipped off`（与 bug 症状一致）。
恢复后重新通过。

- [ ] **Step 3: 运行测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent -- tui:: 2>&1 | tail -10
```

预期：全部通过。

- [ ] **Step 4: 提交**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/history.rs
git commit -m "test(tui): cover the wide-table clipping at the history view layer"
```

---

### Task 3: 文档与全量验证

**Files:**
- Modify: `docs/bug-list.md`
- Modify: `docs/project-management/yi-agent-tui.md`
- Test: 全量

- [ ] **Step 1: 关闭 bug-list 条目**

`docs/bug-list.md`：

```
- [ ] TUI，表格的渲染不会随着命令行窗口的宽度折叠。导致显示不完整
```

改为：

```
- [x] TUI，表格的渲染不会随着命令行窗口的宽度折叠。导致显示不完整（修复：`tui/markdown.rs` `flush_table()` 不再丢弃宽度，按最宽列优先收缩列宽并把超宽单元格按显示宽度折行（对齐保持），放不下最小盒子时（`4*num_cols+1 > width`，如 6 列 `/cost` 表在 20 列下）退化为竖排 `表头: 值` 记录。见 [设计](../superpowers/specs/2026-09-27-tui-table-width-design.md)。验证：`cargo test -p yi-agent --bin yi-agent -- tui::markdown::tests`、`cargo test -p yi-agent --bin yi-agent -- wide_markdown_table_is_not_clipped`（修复前失败））
```

- [ ] **Step 2: 更新 TUI 模块文件**

`docs/project-management/yi-agent-tui.md` 中：

```
- [x] Markdown 表格渲染 — commit `2e9da9e` 用 Unicode box drawing 修复
```

改为：

```
- [x] Markdown 表格渲染 — commit `2e9da9e` 用 Unicode box drawing 修复；`tui/markdown.rs` `flush_table()` 按终端宽度收缩列宽并折行单元格（保持对齐），放不下最小盒子时退化为竖排记录，消除右侧永久裁剪 — [设计](../superpowers/specs/2026-09-27-tui-table-width-design.md)
```

- [ ] **Step 3: 全量验证**

```bash
cd yi-agent-rs && cargo fmt --all \
  && cargo test -p yi-agent --bin yi-agent 2>&1 | tail -6
```

预期：`419 passed; 0 failed`（含 Task 1 的 7 个 + Task 2 的 1 个）。

- [ ] **Step 4: 提交**

```bash
git add docs/bug-list.md docs/project-management/yi-agent-tui.md docs/superpowers/specs/2026-09-27-tui-table-width-design.md docs/superpowers/plans/2026-09-27-tui-table-width.md
git commit -m "docs: record the TUI table-width fix and close the bug-list entry"
```

---

## 验收标准

1. `cargo test -p yi-agent --bin yi-agent` 全绿。
2. `cargo fmt --all -- --check`（`yi-agent-rs/` 下）无 diff。
3. `cargo clippy -p yi-agent --all-targets -- -D warnings` 不引入新错误
   （`main` 既有 3 个错误除外）。
4. `flush_table()` 内不再出现 `let _ = self.width;`。
5. `docs/bug-list.md` 该条目为 `[x]`。

## Self-Review 记录

- **Spec 覆盖：** §2 根因 → Task 1 Step 3（去掉宽度丢弃）；§4.1 列宽收缩 → Task 1
  Step 3 `fit_column_widths`；§4.2 竖排降级 → Task 1 Step 3 `push_vertical_records`；
  §4.3 单元格折行与对齐 → Task 1 Step 3 `wrap_cell` / `pad_cell`；§5 测试 1-7 →
  Task 1 Step 1，测试 8 → Task 2 Step 1；§6 反向验证 → Task 2 Step 2。
- **类型一致性：** `fit_column_widths` / `wrap_cell` / `pad_cell` 的签名在 Task 1
  Step 3 定义并同处调用；`push_vertical_records` 取 `&[Vec<String>]` 与
  `&[Alignment]`，与 `table_rows` / `table_alignments` 类型一致。
- **已知缺口：** 未做横向滚动（Spec §7 列为范围外）；`alignments` 在竖排降级中
  未使用（记录式布局无列对齐语义），已显式 `let _ = alignments;` 标注。
