# TUI 回复被右侧截断（显示不完整）设计

**目标：** TUI 里任何一行文本都不能超过可用列宽。超出即被 ratatui 从右侧永久裁掉
——不换行、不滚动、不提示，用户看到的是"回复被截断了一截"。

**状态：** 已实现并验证（见 §6）。

---

## 1. 问题

用户报告：TUI 上每次看到的内容都会"被截断一截"。

`docs/bug-list.md` 已修复过两条同类问题（有界事件通道丢 delta、Markdown 表格不折
宽），但**同一根因的其它渲染路径没有一起修**，所以问题仍然出现。

## 2. 根因（已核实）

ratatui 的 `Line::render_with_alignment`（`ratatui-0.29.0/src/text/line.rs:731`）注
释写得很明白：

> There is not enough space to render the whole line. As the right side is
> truncated by the area width, only truncate the left.

而 `tui/history.rs` 的 `HistoryView::render` 对每一个 `Line` 都用
`Rect { height: 1, width: text_width }` 渲染——**一行只有一格宽、一格高**，没有换行
机会。任何比 `text_width` 更宽的 `Line` 都会在右侧被静默裁掉。

因此不变式是：**每个 cell 的 `lines(width)` 必须保证每行显示宽度 <= width**。
下列生产者违反了它（探针实测，width=60）：

```
OVERFLOW [fenced code]        width=97  "  fn main() { println!(\"a very long line of rust code ...\"); }"
OVERFLOW [indented code]      width=83  "  let x = some_function(argument_one, ...);"
OVERFLOW [tool call expanded] width=103 "  └   \"old_string\": \"aaaa...\","
OVERFLOW [tool result exp.]   width=84  "  └ line one that is fairly long and will overflow ..."
OVERFLOW [tool call summary]  width=113 "● bash({\"command\":\"这是一条很长的中文...\"})"
OVERFLOW [separator label]    width=138 "─ Error: 这是一条很长的中文内容... "
OVERFLOW [queued preview]     width=132
```

逐条对应代码：

| 生产点 | 文件 | 问题 |
| --- | --- | --- |
| 代码块 | `tui/markdown.rs` `flush_code_block` | 直接 `Line::styled("  {line}")`，完全不换行 |
| 工具调用摘要 / 展开详情 | `tui/cell.rs` `render_tool_call` | 摘要用 `truncate(max_len=60)` 按**字符数**截断（CJK 会到 120 列），展开详情整行直出 |
| 工具结果摘要 / 展开详情 | `tui/cell.rs` `render_tool_result` | 同上；`truncate(80)` 对 CJK 是 160 列 |
| 分隔线与错误文本 | `tui/cell.rs` `render_separator` | `label` 直接拼到一行；provider 错误 / 中断原因往往是长文本 |
| 排队预览 | `tui/queued.rs` `render_queued_preview` | 形参就叫 `_width`，从未使用 |
| 输入框（大段粘贴） | `tui/app.rs` `wrap_input_buffer` | 只按换行符切分而不按宽度折行，粘贴的整段文本超宽被裁 |
| popup 列表/详情 | `tui/bash_popup.rs`、`tui/process_popup.rs` | 命令/stdout/stderr 的 `Line::raw` 未折宽（列表行靠 `truncate_str` 兜底） |

端到端证据（80x24 `TestBackend`，走真实 `run_loop`）：

```
 2|  fn main() { println!("a very long line of rust code that goes past eighty colu|
     ^ "colu" 之后被裁掉，剩下的 "mns for certain\"); }" 永远看不到
```

## 3. 修复方案

两层：

1. **按生产点逐个修（根治）**——让每个 producer 自己保证行宽。
   - 复用已有原语 `tui/wrap.rs::wrap_by_display_width`（按显示宽度折行、不改字符），
     在 `LineBuilder` 上新增 `push_wrapped(text, style)`，代码块与其它"原样文本"走它。
   - `tui/cell.rs` 的摘要/展开详情/分隔线改为按显示宽度折行，而不是按字符数截断。
   - `tui/queued.rs` 用上 `width`，长消息折行显示。
   - `tui/app.rs` 的输入缓冲按显示宽度折行，`compute_input_height` 与渲染共用同一个
     折行结果（顺手修掉 CJK 高度少算的 bug：原实现用 `div_ceil` 按簇数估行）。
2. **渲染层兜底（防回归）**——`HistoryView::render` 在渲染前断言
   `line.width() <= text_width`（`debug_assert`，零 release 开销）。新增生产点若忘记
   折行，测试会立刻失败，而不是等到用户看见被裁的文字。

## 4. 关键事实（实现约束）

- `render_markdown(src, width)` 已接收宽度；调用方 `HistoryCell::lines(width)`
  传入的是 `HistoryState::text_width()`（含滚动条预留列），链路不用改。
- `Line::width()`（ratatui）用 `unicode-width` 计宽，与 `wrap_by_display_width`
  同源；工作区 `unicode-width = "0.2"`，与 ratatui 0.29 的 0.2.x 一致，
  CJK/emoji 计宽不会出现两套标准。
- 表格路径已在 `2026-09-27-tui-table-width-design.md` 修完，本次不动。

## 5. 不做的事

- 不引入"横向滚动"（终端没有横向滚动，折行才是终端语义）。
- 不改事件通道/背压（已在 `fix(core): stop dropping streamed text when the
  consumer lags` 修完，另有一条 bug-list 记录）。

## 6. 验证

- `cargo test -p yi-agent --bin yi-agent -- tui::` 全绿。
- 逐生产点回归用例：
  - `markdown::tests::code_block_lines_fold_to_width`
  - `cell::tests::tool_call_and_result_never_exceed_width`
  - `cell::tests::separator_label_folds_long_error_text`
  - `queued::tests::queued_preview_folds_to_width`
  - `app::tests::pasted_long_line_is_wrapped_not_clipped`
  - `history::tests::rendered_history_never_exceeds_terminal_width`（真实
    `TestBackend` 断言，覆盖 guard 与所有可能溢出的 cell 变体）
