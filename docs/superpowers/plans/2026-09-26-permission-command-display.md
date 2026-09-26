# 权限确认弹窗命令完整可见 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 TUI 权限确认弹窗完整、准确地展示待确认的工具调用内容，且渲染过程不改写命令文本。

**Architecture:** 新增 `tui/wrap.rs` 提供按显示宽度、逐字符保留原文的换行函数；`HistoryCell::PermissionRequest` 增加 `summary`/`full`/`expanded` 字段，渲染时启用此前被忽略的 `width` 参数并限制折叠行数；`app.rs` 在权限待确认时放行 `e`（展开/收起）与滚动键。

**Tech Stack:** Rust、ratatui 0.29、unicode-width、crossterm。

## Global Constraints

- 所有代码改动必须在 worktree `.worktrees/fix/permission-command-display`（branch `fix/permission-command-display`）内进行，严禁直接改 `main`。
- 提交前必须跑 `cargo fmt --all`（在 `yi-agent-rs/` 下）。
- commit message 用 conventional commits 风格，**不要**写 `Co-Authored-By` 行。
- 跑测试前先 `ps aux | grep -v grep | grep -E "cargo|rustc"` 确认没有其他 cargo 进程；不要跑 `cargo test --workspace` 全量。
- 测试命令统一用 `cargo test -p yi-agent --bin yi-agent <filter>`（TUI 代码在 `yi-agent` bin crate 内）。
- **不得使用 `cell.rs::wrap_with_prefix` 渲染权限内容**：它用 `split_whitespace()` 重组，会把连续空白折叠成单个空格，导致用户批准的文本与实际执行的命令不一致。
- 不改 `yi-agent-core` 权限判定逻辑，不改 `PermissionRequest`/`PermissionResolved` 事件形状。
- 折叠态 body 行数上界常量 `MAX_COLLAPSED_LINES = 4`；实测折叠态总高 11 行（黑名单 12 行），24 行终端 history 区为 21 行。

---

## File Structure

| 文件 | 职责 |
|---|---|
| `yi-agent-rs/crates/yi-agent/src/tui/wrap.rs` | **新建**。按显示宽度换行、保留原始字符的纯函数，无样式依赖 |
| `yi-agent-rs/crates/yi-agent/src/tui/mod.rs` | 注册 `wrap` 模块 |
| `yi-agent-rs/crates/yi-agent/src/tui/cell.rs` | `PermissionRequest` 字段调整 + 渲染（宽度感知、折叠、展开） |
| `yi-agent-rs/crates/yi-agent/src/tui/history.rs` | 由 `tool_input` 构造 `summary`/`full`；`toggle_pending_permission_expanded` |
| `yi-agent-rs/crates/yi-agent/src/tui/app.rs` | 权限待确认时的 `e` 键与滚动键放行 |
| `yi-agent-rs/crates/yi-agent/src/tui/bash_popup.rs` | `wrap_text` 改为委托 `wrap_by_display_width`（去重） |
| `docs/project-management/permission.md`、`yi-agent-tui.md`、`README.md` | 进度同步 |

---

### Task 1: 新增保留原文的宽度换行器

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/src/tui/wrap.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/mod.rs`

**Interfaces:**
- Consumes: 无
- Produces: `pub fn wrap_by_display_width(text: &str, width: usize, first_prefix: &str, cont_prefix: &str) -> Vec<String>`

- [ ] **Step 1: 写失败测试**

创建 `yi-agent-rs/crates/yi-agent/src/tui/wrap.rs`：

```rust
//! Display-width-aware text wrapping that preserves the source characters.
//!
//! `cell::wrap_with_prefix` is deliberately NOT used for permission prompts:
//! it splits on whitespace and rejoins with single spaces, so a command like
//! `git commit -m "fix:  two  spaces"` would be displayed differently from
//! what actually executes. Wrapping here never alters a character.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Wrap `text` into lines of at most `width` display columns.
///
/// Every character of `text` is preserved (including runs of spaces and
/// tabs). Explicit `\n` starts a new line. Only the first output line uses
/// `first_prefix`; all later lines use `cont_prefix`. Prefix width counts
/// toward `width`.
pub fn wrap_by_display_width(
    text: &str,
    width: usize,
    first_prefix: &str,
    cont_prefix: &str,
) -> Vec<String> {
    let _ = (text, width, first_prefix, cont_prefix);
    unimplemented!("Task 1 Step 3")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_runs_of_spaces_exactly() {
        let src = r#"git commit -m "fix:  two  spaces""#;
        let out = wrap_by_display_width(src, 200, "", "");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], src);
        assert!(out[0].contains("fix:  two  spaces"));
    }

    #[test]
    fn wraps_at_display_width_without_losing_characters() {
        let src = "abcdefghij";
        let out = wrap_by_display_width(src, 4, "", "");
        assert_eq!(out.concat(), src, "no character may be dropped");
        for line in &out {
            assert!(UnicodeWidthStr::width(line.as_str()) <= 4, "too wide: {line:?}");
        }
    }

    #[test]
    fn cjk_counts_as_two_columns() {
        let src = "一二三四五六七";
        let out = wrap_by_display_width(src, 6, "", "");
        for line in &out {
            assert!(UnicodeWidthStr::width(line.as_str()) <= 6, "line too wide: {line:?}");
        }
        assert_eq!(out.concat(), src);
    }

    #[test]
    fn words_are_not_split_when_they_fit_on_a_line() {
        let cmd = "cd /tmp && cargo test --lib permission";
        let out = wrap_by_display_width(cmd, 20, "", "");
        for line in &out {
            assert!(UnicodeWidthStr::width(line.as_str()) <= 20, "too wide: {line:?}");
        }
        assert!(
            out.iter().any(|l| l.trim_end().ends_with("permission")),
            "'permission' should survive intact: {out:?}"
        );
        assert_eq!(out.concat(), cmd, "characters must be preserved");
    }

    #[test]
    fn unbreakable_token_longer_than_a_line_is_char_broken() {
        let out = wrap_by_display_width(&"z".repeat(25), 10, "", "");
        assert_eq!(out.len(), 3);
        assert_eq!(out.concat(), "z".repeat(25));
    }

    #[test]
    fn explicit_newlines_are_kept() {
        let out = wrap_by_display_width("a\nb", 10, "", "");
        assert_eq!(out, vec!["a", "b"]);
    }

    #[test]
    fn prefix_width_is_subtracted_from_available_width() {
        let out = wrap_by_display_width("abcdefgh", 6, "  ", "  ");
        // available = 6 - 2 = 4 per line
        assert_eq!(out, vec!["  abcd", "  efgh"]);
    }

    #[test]
    fn only_first_line_uses_first_prefix() {
        let out = wrap_by_display_width("abcdefgh", 6, "> ", "  ");
        assert_eq!(out[0], "> abcd");
        assert_eq!(out[1], "  efgh");
    }

    #[test]
    fn empty_input_yields_one_empty_line() {
        let out = wrap_by_display_width("", 10, "", "");
        assert_eq!(out, vec![""]);
    }
}
```

在 `wrap.rs` 末尾（`#[cfg(test)]` 之前）加入折叠上限常量：

```rust
/// Maximum body lines rendered while collapsed. Keeps the decision menu on
/// screen in a 24-row terminal, where the history area is 21 rows.
pub(crate) const MAX_COLLAPSED_LINES: usize = 4;
```

在 `yi-agent-rs/crates/yi-agent/src/tui/mod.rs` 的 `pub mod state;` 之后（保持字母序，放在 `pub mod subagents;` 之前）加入一行：

```rust
pub mod wrap;
```

- [ ] **Step 2: 运行测试确认失败**

Run:
```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::wrap:: 2>&1 | tail -20
```
Expected: 编译通过但 8 个测试全部 FAIL，错误为 `not implemented: Task 1 Step 3`。

- [ ] **Step 3: 写最小实现**

把 `wrap.rs` 中 `wrap_by_display_width` 的函数体替换为：

```rust
/// Split into segments at whitespace runs, keeping each whitespace run
/// attached to the token that precedes it so no character is lost.
fn segments(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut in_ws = false;
    let mut seen_non_ws = false;
    for (i, ch) in s.char_indices() {
        let is_ws = ch.is_whitespace();
        if seen_non_ws && is_ws && !in_ws {
            in_ws = true;
        } else if !is_ws && in_ws {
            out.push(&s[start..i]);
            start = i;
            in_ws = false;
        }
        if !is_ws {
            seen_non_ws = true;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

pub fn wrap_by_display_width(
    text: &str,
    width: usize,
    first_prefix: &str,
    cont_prefix: &str,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut prefix = first_prefix;

    for physical in text.split('\n') {
        let mut avail = width.saturating_sub(UnicodeWidthStr::width(prefix)).max(1);
        let mut cur = String::new();

        for seg in segments(physical) {
            let seg_w = UnicodeWidthStr::width(seg);
            if !cur.is_empty() && UnicodeWidthStr::width(cur.as_str()) + seg_w > avail {
                out.push(format!("{prefix}{cur}"));
                prefix = cont_prefix;
                avail = width.saturating_sub(UnicodeWidthStr::width(prefix)).max(1);
                cur.clear();
            }
            if seg_w > avail {
                // Token longer than a whole line: break it character by character.
                for ch in seg.chars() {
                    let ch_w = UnicodeWidthChar::width(ch).unwrap_or(0);
                    if !cur.is_empty() && UnicodeWidthStr::width(cur.as_str()) + ch_w > avail {
                        out.push(format!("{prefix}{cur}"));
                        prefix = cont_prefix;
                        avail = width.saturating_sub(UnicodeWidthStr::width(prefix)).max(1);
                        cur.clear();
                    }
                    cur.push(ch);
                }
            } else {
                cur.push_str(seg);
            }
        }
        out.push(format!("{prefix}{cur}"));
        prefix = cont_prefix;
    }
    out
}
```

- [ ] **Step 4: 运行测试确认通过**

Run:
```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::wrap:: 2>&1 | tail -10
```
Expected: `test result: ok. 8 passed; 0 failed`

- [ ] **Step 5: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add yi-agent-rs/crates/yi-agent/src/tui/wrap.rs yi-agent-rs/crates/yi-agent/src/tui/mod.rs && git commit -m "feat(tui): add whitespace-preserving width wrapper"
```

---

### Task 2: 权限弹窗按宽度渲染并支持折叠/展开

本任务一次性改动 `cell.rs` 与 `history.rs`：结构体加字段必然影响唯一构造点，两文件必须同改才能编译。

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/cell.rs:34-42`（结构体）、`cell.rs:64-102`（`lines`）、`cell.rs:211-278`（`render_permission_request`）
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/history.rs:395-411`（构造点）

**Interfaces:**
- Consumes: `crate::tui::wrap::wrap_by_display_width`
- Produces:
  - `HistoryCell::PermissionRequest` 字段变为 `{ request_id: u64, tool_name: String, summary: String, full: String, prefix_suggestion: Option<String>, kind: PermissionKind, resolved: bool, expanded: bool }`
  - `const MAX_COLLAPSED_LINES: usize = 4`（`cell.rs` 内，`pub(crate)`）

- [ ] **Step 1: 写失败测试**

在 `cell.rs` 的 `mod tests`（`cell.rs:397`）内追加：

```rust
    fn perm_cell(summary: &str, full: &str, expanded: bool) -> HistoryCell {
        HistoryCell::PermissionRequest {
            request_id: 1,
            tool_name: "bash".into(),
            summary: summary.into(),
            full: full.into(),
            prefix_suggestion: Some("cd".into()),
            kind: yi_agent_core::permission::PermissionKind::Normal,
            resolved: false,
            expanded,
        }
    }

    fn line_widths(lines: &[Line<'static>]) -> Vec<usize> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                    .sum()
            })
            .collect()
    }

    fn joined_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect()
    }

    const LONG_CMD: &str = "cd /Users/someone/projects/personalProjects/yi-agent && cargo test -p yi-agent-core --lib permission";

    #[test]
    fn permission_body_never_exceeds_width() {
        // Includes narrow widths where the header and menu themselves wrap.
        for width in [20u16, 30, 40, 80] {
            for expanded in [false, true] {
                let cell = perm_cell(LONG_CMD, LONG_CMD, expanded);
                let widths = line_widths(&cell.lines(width));
                assert!(
                    widths.iter().all(|w| *w <= width as usize),
                    "width {width} expanded={expanded}: some line exceeds it: {widths:?}"
                );
            }
        }
    }

    #[test]
    fn permission_wrap_preserves_exact_command_whitespace() {
        let cmd = r#"git commit -m "fix:  two  spaces""#;
        let cell = perm_cell(cmd, cmd, true);
        let text = joined_text(&cell.lines(200));
        assert!(
            text.contains(cmd),
            "command text was rewritten during rendering: {text:?}"
        );
    }

    #[test]
    fn permission_collapsed_body_is_bounded() {
        // Hard requirement: fit the history area (21 rows in a 24-row terminal).
        for width in [20u16, 30, 40, 80] {
            let cell = perm_cell(LONG_CMD, LONG_CMD, false);
            let lines = cell.lines(width);
            assert!(
                lines.len() <= 21,
                "width {width}: collapsed cell too tall: {} lines",
                lines.len()
            );
        }
        // At a normal width only the body wraps, so the tight bound holds.
        let cell = perm_cell(LONG_CMD, LONG_CMD, false);
        assert!(cell.lines(80).len() <= 1 + MAX_COLLAPSED_LINES + 1 + 5 + 1);
    }

    #[test]
    fn permission_collapsed_shows_expand_hint_when_truncated() {
        let cell = perm_cell(LONG_CMD, LONG_CMD, false);
        let text = joined_text(&cell.lines(20));
        assert!(text.contains("[e]"), "expected expand hint: {text:?}");
    }

    #[test]
    fn permission_not_truncated_has_no_expand_hint() {
        let cell = perm_cell("ls", "ls", false);
        let text = joined_text(&cell.lines(80));
        assert!(!text.contains("[e]"), "unexpected hint: {text:?}");
    }

    #[test]
    fn permission_expanded_recovers_command_tail() {
        // Width 20 forces truncation; width 40 happens to fit in 4 lines.
        let cell = perm_cell(LONG_CMD, LONG_CMD, true);
        let text = joined_text(&cell.lines(20));
        assert!(
            text.contains("permission"),
            "expanded body should reach the command tail: {text:?}"
        );
        assert!(
            !text.contains("more lines"),
            "expanded body must not claim to be truncated: {text:?}"
        );
    }

    #[test]
    fn permission_menu_rendered_after_body() {
        let cell = perm_cell(LONG_CMD, LONG_CMD, false);
        let lines = cell.lines(80);
        let text = joined_text(&lines);
        for opt in ["[1] Allow once", "[2] Always allow tool", "[3] Always allow prefix: cd", "[4] Deny"] {
            assert!(text.contains(opt), "missing menu option {opt}: {text:?}");
        }
        let body_line = lines
            .iter()
            .position(|l| joined_text(std::slice::from_ref(l)).contains("&&"))
            .expect("body marker '&&' not found");
        let menu_line = lines
            .iter()
            .position(|l| joined_text(std::slice::from_ref(l)).contains("[1] Allow once"))
            .expect("menu not found");
        assert!(menu_line > body_line, "menu must render after body");
    }

    #[test]
    fn permission_cjk_body_wraps_at_display_width() {
        let cjk = "这是一条很长的中文命令需要按显示宽度换行";
        for width in [30u16, 80] {
            let cell = perm_cell(cjk, cjk, true);
            let widths = line_widths(&cell.lines(width));
            assert!(
                widths.iter().all(|w| *w <= width as usize),
                "width {width}: CJK line too wide: {widths:?}"
            );
        }
    }

    #[test]
    fn permission_resolved_line_wraps() {
        let mut cell = perm_cell(LONG_CMD, LONG_CMD, false);
        if let HistoryCell::PermissionRequest { resolved, .. } = &mut cell {
            *resolved = true;
        }
        let widths = line_widths(&cell.lines(40));
        assert!(widths.iter().all(|w| *w <= 40), "resolved line too wide: {widths:?}");
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run:
```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::cell::tests::permission_ 2>&1 | tail -20
```
Expected: 编译失败，报 `HistoryCell::PermissionRequest` 缺少 `summary` / `full` / `expanded` 字段，以及 `MAX_COLLAPSED_LINES` 未定义。

- [ ] **Step 3: 改结构体与渲染**

在 `cell.rs` 中，把 `PermissionRequest` 变体（`cell.rs:34-42`）替换为：

```rust
    /// Permission request prompt. Shows a menu for the user to choose a decision.
    PermissionRequest {
        request_id: u64,
        tool_name: String,
        /// Compact, always-visible description of what is being requested.
        summary: String,
        /// Complete, unabridged description shown when expanded.
        full: String,
        prefix_suggestion: Option<String>,
        kind: yi_agent_core::permission::PermissionKind,
        resolved: bool,
        /// Whether the body is shown in full instead of truncated.
        expanded: bool,
    },
```

在 `cell.rs` 顶部（`use` 之后）加入 import。`MAX_COLLAPSED_LINES` 已在
Task 1 的 `wrap.rs` 中定义为 `pub(crate)`，这里直接复用，不要重复定义：

```rust
use super::wrap::{MAX_COLLAPSED_LINES, wrap_by_display_width};
```

在 `lines()` 中把该分支（`cell.rs:85-99`）替换为：

```rust
            Self::PermissionRequest {
                tool_name,
                summary,
                full,
                prefix_suggestion,
                kind,
                resolved,
                expanded,
                ..
            } => render_permission_request(
                tool_name,
                summary,
                full,
                prefix_suggestion.as_deref(),
                kind,
                *resolved,
                *expanded,
                width,
            ),
```

把 `render_permission_request`（`cell.rs:211-278`）整体替换为：

```rust
#[allow(clippy::too_many_arguments)]
fn render_permission_request(
    tool_name: &str,
    summary: &str,
    full: &str,
    prefix_suggestion: Option<&str>,
    kind: &yi_agent_core::permission::PermissionKind,
    resolved: bool,
    expanded: bool,
    width: u16,
) -> Vec<Line<'static>> {
    let warn_style = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let dim_style = Style::new().add_modifier(Modifier::DIM);
    let menu_style = Style::new().fg(Color::Cyan);
    let w = width.max(1) as usize;

    if resolved {
        return wrap_by_display_width(summary, w, "  [resolved] ", "  ")
            .into_iter()
            .map(|chunk| Line::from(chunk).style(dim_style))
            .collect();
    }

    let body = if expanded { full } else { summary };
    let body_lines = wrap_by_display_width(body, w, "  ", "  ");
    let truncated = !expanded && body_lines.len() > MAX_COLLAPSED_LINES;

    // Every line goes through the wrapper, including the header and the
    // menu: at narrow widths those are themselves wider than the terminal
    // and would otherwise be truncated by ratatui.
    let mut lines: Vec<Line<'static>> = {
        let mut header: Vec<Line<'static>> = Vec::new();
        let header_text = format!("? Permission needed: {tool_name}");
        for (i, chunk) in wrap_by_display_width(&header_text, w, "", "  ")
            .into_iter()
            .enumerate()
        {
            if i == 0 {
                header.push(Line::from(Span::styled(chunk, warn_style)));
            } else {
                header.push(Line::from(Span::styled(chunk, warn_style)));
            }
        }
        header
    };

    let shown = if truncated {
        MAX_COLLAPSED_LINES
    } else {
        body_lines.len()
    };
    for line in body_lines.iter().take(shown) {
        lines.push(Line::from(line.clone()).style(dim_style));
    }
    if truncated {
        let hidden = body_lines.len() - MAX_COLLAPSED_LINES;
        for chunk in wrap_by_display_width(
            &format!("  … (+{hidden} more lines, [e] to expand)"),
            w,
            "",
            "  ",
        ) {
            lines.push(Line::from(chunk).style(dim_style));
        }
    } else if expanded {
        for chunk in wrap_by_display_width("  [e] to collapse", w, "", "  ") {
            lines.push(Line::from(chunk).style(dim_style));
        }
    }

    let blacklisted = matches!(
        kind,
        yi_agent_core::permission::PermissionKind::Blacklisted(_)
    );
    if blacklisted {
        let style = Style::new().fg(Color::Red).add_modifier(Modifier::BOLD);
        for chunk in wrap_by_display_width("  [!] Blacklisted command", w, "", "  ") {
            lines.push(Line::from(Span::styled(chunk, style)));
        }
    }

    let option_lines: Vec<(String, Style)> = match prefix_suggestion {
        Some(p) => vec![
            ("  [1] Allow once".to_string(), menu_style),
            ("  [2] Always allow tool".to_string(), menu_style),
            (format!("  [3] Always allow prefix: {p}"), menu_style),
            ("  [4] Deny".to_string(), menu_style),
        ],
        None => vec![
            ("  [1] Allow once".to_string(), menu_style),
            ("  [2] Always allow tool".to_string(), menu_style),
            ("  [4] Deny".to_string(), menu_style),
        ],
    };
    for (text, style) in option_lines {
        for chunk in wrap_by_display_width(&text, w, "", "  ") {
            lines.push(Line::from(Span::styled(chunk, style)));
        }
    }

    let default_hint = if blacklisted {
        "  Enter = Deny"
    } else {
        "  Enter = Allow once"
    };
    for chunk in wrap_by_display_width(default_hint, w, "", "  ") {
        lines.push(Line::from(chunk).style(dim_style));
    }

    lines
}
```

在 `history.rs` 中把构造点（`history.rs:402-410`）替换为：

```rust
                let summary = format!("{}: {}", tool_name, tool_input);
                let full = format!("{}: {}", tool_name, tool_input);
                self.cells.push(HistoryCell::PermissionRequest {
                    request_id,
                    tool_name,
                    summary,
                    full,
                    prefix_suggestion,
                    kind,
                    resolved: false,
                    expanded: false,
                });
```

（Task 3 会把这两个字符串换成按工具精简的版本；此处先保证编译与宽度修复生效。）

- [ ] **Step 4: 运行测试确认通过**

Run:
```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::cell::tests::permission_ 2>&1 | tail -15
```
Expected: `test result: ok. 9 passed; 0 failed`

再跑该 crate 的 TUI 全量，确认没破坏既有测试：
```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui:: 2>&1 | tail -5
```
Expected: 全部通过（基线 280 个 + 新增）。

- [ ] **Step 5: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A && git commit -m "fix(tui): render permission prompt within terminal width

The permission prompt put the whole JSON tool_input into one Line, which
ratatui truncates at the right edge; at 80 columns a bash command lost 69
columns of text. Render through a width-aware wrapper, cap the collapsed
body at 4 lines, and add an expanded mode that shows the full input."
```

---

### Task 3: 按工具精简 summary，并支持切换展开

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/history.rs:402-410`（构造点）、`history.rs:270-296` 之后新增方法

**Interfaces:**
- Consumes: `HistoryCell::PermissionRequest` 的 `summary` / `full` / `expanded` 字段
- Produces:
  - `fn permission_summary(tool_name: &str, input: &serde_json::Value) -> String`
  - `pub fn HistoryState::toggle_pending_permission_expanded(&mut self) -> bool`

- [ ] **Step 1: 写失败测试**

在 `history.rs` 的 `mod tests` 内追加：

```rust
    #[test]
    fn permission_summary_for_bash_drops_json_noise() {
        let mut s = HistoryState::new();
        s.push_event(
            AgentEvent::PermissionRequest {
                request_id: 1,
                tool_name: "bash".into(),
                tool_input: serde_json::json!({
                    "command": "cargo test",
                    "expected_timeout_sec": 120
                }),
                prefix_suggestion: Some("cargo".into()),
                kind: yi_agent_core::permission::PermissionKind::Normal,
            },
            80,
        );
        match &s.cells[0] {
            HistoryCell::PermissionRequest { summary, full, .. } => {
                assert_eq!(summary, "cargo test");
                assert!(!summary.contains("expected_timeout_sec"));
                assert!(full.contains("expected_timeout_sec"), "full keeps everything");
            }
            _ => panic!("expected PermissionRequest"),
        }
    }

    #[test]
    fn permission_summary_for_write_hides_content_blob() {
        let mut s = HistoryState::new();
        let content = "x".repeat(500);
        s.push_event(
            AgentEvent::PermissionRequest {
                request_id: 1,
                tool_name: "write".into(),
                tool_input: serde_json::json!({ "path": "src/main.rs", "content": content }),
                prefix_suggestion: None,
                kind: yi_agent_core::permission::PermissionKind::Normal,
            },
            80,
        );
        match &s.cells[0] {
            HistoryCell::PermissionRequest { summary, .. } => {
                assert!(summary.contains("src/main.rs"), "summary: {summary}");
                assert!(summary.contains("500 bytes"), "summary: {summary}");
                assert!(
                    !summary.contains(&"x".repeat(50)),
                    "summary must not inline the blob: {summary}"
                );
            }
            _ => panic!("expected PermissionRequest"),
        }
    }

    #[test]
    fn permission_summary_for_edit_reports_both_field_sizes() {
        let mut s = HistoryState::new();
        s.push_event(
            AgentEvent::PermissionRequest {
                request_id: 1,
                tool_name: "edit".into(),
                tool_input: serde_json::json!({
                    "path": "a.rs", "old_string": "aa", "new_string": "bbbb"
                }),
                prefix_suggestion: None,
                kind: yi_agent_core::permission::PermissionKind::Normal,
            },
            80,
        );
        match &s.cells[0] {
            HistoryCell::PermissionRequest { summary, .. } => {
                assert!(summary.contains("a.rs"), "summary: {summary}");
                assert!(summary.contains("old_string: 2 bytes"), "summary: {summary}");
                assert!(summary.contains("new_string: 4 bytes"), "summary: {summary}");
            }
            _ => panic!("expected PermissionRequest"),
        }
    }

    #[test]
    fn toggle_pending_permission_expanded_toggles_only_pending() {
        let mut s = HistoryState::new();
        s.push_event(
            AgentEvent::PermissionRequest {
                request_id: 1,
                tool_name: "bash".into(),
                tool_input: serde_json::json!({"command": "ls"}),
                prefix_suggestion: None,
                kind: yi_agent_core::permission::PermissionKind::Normal,
            },
            80,
        );
        assert!(s.toggle_pending_permission_expanded());
        match &s.cells[0] {
            HistoryCell::PermissionRequest { expanded, .. } => assert!(*expanded),
            _ => panic!("expected PermissionRequest"),
        }
        assert!(s.toggle_pending_permission_expanded());
        match &s.cells[0] {
            HistoryCell::PermissionRequest { expanded, .. } => assert!(!*expanded),
            _ => panic!("expected PermissionRequest"),
        }
    }

    #[test]
    fn toggle_pending_permission_expanded_returns_false_when_none() {
        let mut s = HistoryState::new();
        assert!(!s.toggle_pending_permission_expanded());
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run:
```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::history::tests::permission_summary tui::history::tests::toggle_pending 2>&1 | tail -20
```
Expected: 编译失败，`toggle_pending_permission_expanded` 方法不存在；`summary` 断言失败（当前是完整 JSON）。

- [ ] **Step 3: 实现 summary 与 toggle**

在 `history.rs` 中，把 Task 2 写入的构造点替换为：

```rust
                let summary = permission_summary(&tool_name, &tool_input);
                let full = format!(
                    "{}: {}",
                    tool_name,
                    serde_json::to_string_pretty(&tool_input)
                        .unwrap_or_else(|_| tool_input.to_string())
                );
                self.cells.push(HistoryCell::PermissionRequest {
                    request_id,
                    tool_name,
                    summary,
                    full,
                    prefix_suggestion,
                    kind,
                    resolved: false,
                    expanded: false,
                });
```

在 `history.rs` 中 `impl HistoryState` 之前（`has_spacer_after` 附近）加入：

```rust
/// Compact one-line description of a permission request.
///
/// Bash-like tools show only the command, since that is what the user is
/// judging. File tools show the target path plus the size of each payload
/// field rather than inlining the payload, which can be thousands of bytes.
fn permission_summary(tool_name: &str, input: &serde_json::Value) -> String {
    match tool_name {
        "bash" | "process_start" => input
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        "write" | "edit" => {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("<unknown path>");
            let mut parts = vec![format!("path: {path}")];
            for key in ["content", "old_string", "new_string"] {
                if let Some(value) = input.get(key).and_then(|v| v.as_str()) {
                    parts.push(format!("{key}: {} bytes", value.len()));
                }
            }
            parts.join(", ")
        }
        _ => input.to_string(),
    }
}
```

在 `impl HistoryState` 中，紧跟 `pending_permission_info` 之后加入：

```rust
    /// Toggle the expanded state of the most recent unresolved permission
    /// request. Returns `false` when no request is pending.
    pub fn toggle_pending_permission_expanded(&mut self) -> bool {
        for cell in self.cells.iter_mut().rev() {
            if let HistoryCell::PermissionRequest {
                resolved: false,
                expanded,
                ..
            } = cell
            {
                *expanded = !*expanded;
                return true;
            }
        }
        false
    }
```

- [ ] **Step 4: 运行测试确认通过**

Run:
```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::history::tests:: 2>&1 | tail -10
```
Expected: `test result: ok`，含新增 5 个测试。

- [ ] **Step 5: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A && git commit -m "feat(tui): summarize permission requests per tool

Bash-like tools now show only the command, and file tools show the target
path plus payload sizes instead of inlining file contents. Also add
toggle_pending_permission_expanded for the upcoming [e] key."
```

---

### Task 4: 权限待确认时放行 `e` 与滚动键

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:831-865`（`handle_key` 的权限分支）

**Interfaces:**
- Consumes: `HistoryState::toggle_pending_permission_expanded`
- Produces: 无新公开接口

- [ ] **Step 1: 写失败测试**

在 `app.rs` 的 `mod tests` 内追加：

```rust
    #[test]
    fn permission_key_e_toggles_expanded() {
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, mut decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = VecDeque::new();
        let mut pending_quit = false;
        let mut popup = None;

        history.push_event(make_permission_request_normal(1), 80);

        let outcome = handle_key(
            make_key(KeyCode::Char('e'), KeyModifiers::NONE),
            &mut input,
            &mut history,
            0,
            80,
            20,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
        );
        assert!(matches!(outcome, KeyOutcome::None));
        assert!(
            decision_rx.try_recv().is_err(),
            "'e' must not resolve the permission"
        );
        match &history.cells[0] {
            HistoryCell::PermissionRequest { expanded, .. } => {
                assert!(*expanded, "'e' should expand the pending request")
            }
            _ => panic!("expected PermissionRequest"),
        }
    }

    #[test]
    fn scroll_keys_work_while_permission_pending() {
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = VecDeque::new();
        let mut pending_quit = false;
        let mut popup = None;

        for _ in 0..120 {
            history.push(HistoryCell::Separator { label: None }, 80);
        }
        history.push_event(make_permission_request_normal(1), 80);
        history.scroll_offset = 0;

        let outcome = handle_key(
            make_key(KeyCode::Up, KeyModifiers::NONE),
            &mut input,
            &mut history,
            100,
            80,
            20,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
        );
        assert!(matches!(outcome, KeyOutcome::None));
        assert_eq!(
            history.scroll_offset, 1,
            "Up should scroll history while a permission is pending"
        );
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run:
```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::app::tests::permission_key_e_toggles_expanded tui::app::tests::scroll_keys_work_while_permission_pending 2>&1 | tail -20
```
Expected: 两个测试均 FAIL —— `'e'` 未展开（`expanded` 仍为 false），`scroll_offset` 仍为 0。

- [ ] **Step 3: 改按键分支**

把 `app.rs:831-865` 的权限分支替换为：

```rust
    // Check if there's a pending permission request. Clone the small fields
    // we need so the immutable borrow ends before we mutate history.
    let pending_permission = history.pending_permission_info().map(
        |(request_id, tool_name, prefix_suggestion, kind)| {
            (
                request_id,
                tool_name.to_string(),
                prefix_suggestion.map(str::to_string),
                kind.clone(),
            )
        },
    );
    if let Some((request_id, _tool_name, prefix_suggestion, kind)) = pending_permission {
        // Allow quit keys to pass through even when permission is pending
        let is_quit_key = matches!(key.code, KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL)
            || matches!(key.code, KeyCode::Esc);
        // Scrolling stays available so an expanded body can be read. These
        // keys are non-destructive and cannot resolve the request.
        let is_scroll_key = matches!(
            key.code,
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
        );
        if is_quit_key || is_scroll_key {
            // Fall through to global key handling below
        } else {
            if key.code == KeyCode::Char('e') && key.modifiers.is_empty() {
                history.toggle_pending_permission_expanded();
                return KeyOutcome::None;
            }
            let decision = match key.code {
                KeyCode::Char('1') => Some(yi_agent_core::permission::Decision::AllowOnce),
                KeyCode::Char('2') => Some(yi_agent_core::permission::Decision::AlwaysAllowTool),
                KeyCode::Char('3') => prefix_suggestion
                    .as_deref()
                    .map(|p| yi_agent_core::permission::Decision::AlwaysAllowPrefix(p.to_string())),
                KeyCode::Char('4') => Some(yi_agent_core::permission::Decision::Deny),
                KeyCode::Enter => {
                    let default = match kind {
                        yi_agent_core::permission::PermissionKind::Blacklisted(_) => {
                            yi_agent_core::permission::Decision::Deny
                        }
                        _ => yi_agent_core::permission::Decision::AllowOnce,
                    };
                    Some(default)
                }
                _ => None,
            };
            if let Some(d) = decision {
                let _ = decision_tx.blocking_send((request_id, d));
                return KeyOutcome::None;
            }
            // For other keys while permission pending, ignore (don't let user type input)
            return KeyOutcome::None;
        }
    }
```

- [ ] **Step 4: 运行测试确认通过**

Run:
```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::app::tests::permission 2>&1 | tail -10
```
Expected: 全部 `permission*` 测试通过，含既有的 `permission_other_keys_ignored_while_pending`（它用 `'a'`，仍被忽略）。

再跑 TUI 全量：
```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui:: 2>&1 | tail -5
```
Expected: 全部通过。

- [ ] **Step 5: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A && git commit -m "feat(tui): let [e] expand and scroll keys work during permission prompts

Expanding the body could push content above the viewport while every key
except the decision keys was swallowed. Route Up/Down/PageUp/PageDown to
history scrolling and bind [e] to expand or collapse the pending body."
```

---

### Task 5: 消除 bash 详情弹窗的重复换行实现

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/bash_popup.rs:228-262`（`wrap_text`）

**Interfaces:**
- Consumes: `crate::tui::wrap::wrap_by_display_width`
- Produces: `fn wrap_text(&str, u16, &str, &str) -> Vec<Line<'static>>`（签名不变）

- [ ] **Step 1: 确认现有测试先通过（重构前的安全网）**

Run:
```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::bash_popup:: 2>&1 | tail -10
```
Expected: 全部通过，其中 `detail_wraps_long_command_stdout_stderr_and_cjk` 是本次重构的等价性回归测试。

- [ ] **Step 2: 替换为委托实现**

把 `bash_popup.rs:229-262` 的 `wrap_text` 整体替换为：

```rust
/// Wrap text by terminal display width while preserving explicit newlines.
///
/// Delegates to the shared whitespace-preserving wrapper so the bash detail
/// view and the permission prompt cannot drift apart.
fn wrap_text(
    text: &str,
    width: u16,
    first_prefix: &str,
    continuation_prefix: &str,
) -> Vec<Line<'static>> {
    crate::tui::wrap::wrap_by_display_width(
        text,
        width as usize,
        first_prefix,
        continuation_prefix,
    )
    .into_iter()
    .map(Line::raw)
    .collect()
}
```

- [ ] **Step 3: 运行测试确认通过**

Run:
```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::bash_popup:: 2>&1 | tail -10
```
Expected: 与 Step 1 相同结果，全部通过（行为等价）。

- [ ] **Step 4: 检查未使用 import 并格式化提交**

`bash_popup.rs` 若因删除实现而不再使用 `UnicodeWidthChar`，需从 `use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};` 中移除该项（编译警告会提示）。

Run:
```bash
cd yi-agent-rs && cargo fmt --all && cargo clippy -p yi-agent --bin yi-agent 2>&1 | grep -E "warning: unused|^error" | head -10
```
Expected: 无 unused import 警告。

```bash
cd .. && git add -A && git commit -m "refactor(tui): share one width wrapper with bash detail view"
```

---

### Task 6: 同步项目进度文档

**Files:**
- Modify: `docs/project-management/permission.md`
- Modify: `docs/project-management/yi-agent-tui.md`
- Modify: `docs/project-management/README.md`

**Interfaces:**
- Consumes: 无
- Produces: 无

- [ ] **Step 1: 更新 permission.md**

在 `docs/project-management/permission.md` 的 Features 列表中追加一行（`[x]` 表示已完成，禁止用 `[~]`）：

```markdown
- [x] TUI 权限弹窗完整显示命令 — `tui/cell.rs` 按终端宽度换行、折叠 4 行 + `[e]` 展开，命令原文不被改写；`tui/history.rs` 按工具精简 summary；验证：`cargo test -p yi-agent --bin yi-agent tui::cell::tests::permission_` 和 `cargo test -p yi-agent --bin yi-agent tui::history::tests::permission_summary`
```

- [ ] **Step 2: 更新 yi-agent-tui.md**

在 `docs/project-management/yi-agent-tui.md` 的 Features 列表中追加一行：

```markdown
- [x] 权限确认弹窗完整可见 — `tui/wrap.rs::wrap_by_display_width` 逐字符换行不改写原文；`tui/cell.rs` 折叠 4 行 + `[e]` 展开；`tui/app.rs` 待确认时放行 `e` 与滚动键；验证：`cargo test -p yi-agent --bin yi-agent tui::wrap::` 和 `cargo test -p yi-agent --bin yi-agent tui::app::tests::permission_key_e_toggles_expanded`
```

- [ ] **Step 3: 同步 README 计数**

若 `docs/project-management/README.md` 的模块索引表含「完成 / 总计」计数，按本次新增的 feature 条数更新对应模块行的完成数。先查看当前数值：

```bash
grep -n "permission\|yi-agent-tui" docs/project-management/README.md
```
把 permission 与 yi-agent-tui 两行的完成数各加 1（若表中总计也随之变化，同步更新）。

- [ ] **Step 4: 全量验证**

Run:
```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui:: 2>&1 | tail -5 && cargo test -p yi-agent-core --lib 2>&1 | tail -5
```
Expected: 两个 crate 全部通过。

- [ ] **Step 5: 提交**

```bash
git add docs/project-management && git commit -m "docs: record permission prompt visibility work"
```

---

## 完成后的集成流程

按 `superpowers:finishing-a-development-branch`：

1. 在 worktree 内跑完整验证：`cargo test -p yi-agent --bin yi-agent tui::`、`cargo test -p yi-agent-core --lib`、`cargo fmt --all --check`。
2. 回 `main` 执行 `git merge --no-ff fix/permission-command-display`。
3. 合并后删除分支并 `git worktree remove .worktrees/fix/permission-command-display`。

## 备注：与 spec 的一处措辞偏差

Spec 中展开提示写作中文 `… (+M 行，按 [e] 展开)`。本计划实现为英文
`… (+N more lines, [e] to expand)`，因为该弹窗其余文案（`Permission needed`、
`Allow once`、`Deny`、`Enter = Allow once`）全为英文，混排会显得不一致。若希望
保留中文文案，只需改 Task 2 Step 3 中那一行字符串与对应测试断言。
