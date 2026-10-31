# TUI 浅色终端配色适配 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给 TUI 引入 `TuiTheme` 语义配色层，让同一份代码在浅色与深色终端上都可读；启动时用 OSC 11 探测背景，`YI_AGENT_THEME` 可覆盖，失败回退深色。

**Architecture:** 新建 `tui/theme.rs` 承载 `TuiTheme`（12 个语义槽位）与探测逻辑；每个渲染函数**保留原签名**并在内部转发到新的 `*_with(..., theme)` 变体，因此 100+ 处既有测试断言零改动。生产路径（`run_tui` → `run_loop` → `HistoryView` / 各弹窗）显式携带 `TuiTheme`。`HistoryState` 的渲染缓存 key 追加 `theme.kind`，为将来热切换铺路。光标由硬编码 `fg(Black)+bg(White)` 改为 `Modifier::REVERSED`——这是根因级修复，两种背景自动成立。

**Tech Stack:** Rust 2024 edition，`ratatui 0.29`（`Color::Rgb` / `Modifier::REVERSED`）、`crossterm 0.28`（raw mode、`poll`、`Event`）、`libc 0.2`（探测定时的兜底，已在依赖中）。无新依赖。

## Global Constraints

- 禁止在 `main` 分支直接改代码；所有实现必须在 worktree 的新分支上，完成后 `git merge --no-ff` 回 `main`。worktree 路径：`.worktrees/fix/tui-light-theme`，分支：`fix/tui-light-theme`。
- 提交前必须跑 `cargo fmt --all`（在 `yi-agent-rs/` 下），保证 `just fmt-check` 通过。
- Commit message 用 conventional commits（`feat:` / `fix:` / `test:` / `docs:`），首行 <=72 字符，**不得**写 `Co-Authored-By`。
- 测试按 crate 跑，**不要** `cargo test --workspace`（易 OOM / 死锁）。跑前先 `ps aux | grep cargo` 确认无残留 cargo 进程。
- 主题值一律用 `Color::Rgb`，**禁止**新增裸 ANSI 色（`Color::Gray` / `DarkGray` / `Indexed(`）。
- 探测**绝不能**写 stdout / stdout：失败只走 `tracing::warn!`。见 `docs/bug-list.md:101` 的污染教训。
- 探测必须在 `enable_raw_mode()` 之后、`EnterAlternateScreen` 之前完成，总预算 **200ms**。
- 文中所有对比度数字取自 WCAG 相对亮度公式；ANSI 色按 xterm 256 标准调色板折算。
- **深色观感会变亮**：`DarkGray`（`#585858`，深底 2.34:1）映射到 `fg_muted`（`#b2b2b2`，7.86:1）。这是修正而非回归，验收时明确记录。

---

## File Structure

| 文件 | 职责 | 动作 |
|---|---|---|
| `yi-agent-rs/crates/yi-agent/src/tui/theme.rs` | `TuiThemeKind` / `TuiTheme` 常量 / `relative_luminance` / `detect_theme` / `parse_osc11` | **新建** |
| `yi-agent-rs/crates/yi-agent/src/tui/mod.rs` | 注册 `pub mod theme;` | 修改（1 行） |
| `yi-agent-rs/crates/yi-agent/src/tui/app.rs` | 探测调用、`run_loop` / `HistoryView` 携带 theme、输入条与光标、3 个光标测试改写 | 修改 |
| `yi-agent-rs/crates/yi-agent/src/tui/cell.rs` | `HistoryCell::lines_with`、`render_*` 收 theme | 修改 |
| `yi-agent-rs/crates/yi-agent/src/tui/markdown.rs` | `render_markdown_with`、`LineBuilder` 收 theme | 修改 |
| `yi-agent-rs/crates/yi-agent/src/tui/statusbar.rs` | `render_statusbar_with`、`spinner_color` 按主题插值 | 修改 |
| `yi-agent-rs/crates/yi-agent/src/tui/bash_popup.rs` | `detail_lines_with`、`render_*_popup` 收 theme | 修改 |
| `yi-agent-rs/crates/yi-agent/src/tui/process_popup.rs` | `process_detail_lines_with`、`status_color` 收 theme | 修改 |
| `yi-agent-rs/crates/yi-agent/src/tui/trace.rs` | `state_color`、`render_subagent_*` 收 theme | 修改 |
| `yi-agent-rs/crates/yi-agent/src/tui/queued.rs` | `render_queued_preview_with` | 修改 |
| `yi-agent-rs/crates/yi-agent/src/tui/history.rs` | `HistoryCache` 缓存 key 追加 theme、`HistoryView.theme` | 修改 |
| `yi-agent-rs/crates/yi-agent/tests/tui_theme_guard.rs` | 静态门禁 | **新建** |
| `docs/project-management/yi-agent-tui.md` | 勾选新 feature + 验证命令 | 修改 |

`theme.rs` 单独成文件，因为它是唯一「知道具体色值」的地方；其余文件只消费语义槽位。门禁据此划界：只有 `theme.rs` 允许出现 `Color::Rgb(...)` 字面量。

---

### Task 1: `TuiTheme` 与配色常量

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/src/tui/theme.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/mod.rs`（在 `pub mod trace;` 后加 `pub mod theme;`，保持字母序）

**Interfaces:**
- Consumes: 无（本任务独立）
- Produces:
  - `pub enum TuiThemeKind { Dark, Light }`（derive `Debug, Clone, Copy, PartialEq, Eq`）
  - `pub struct TuiTheme`，字段：`kind: TuiThemeKind`、`fg`、`fg_muted`、`fg_faint`、`accent`、`warn`、`danger`、`success`、`bg_raised`、`sel_fg`、`sel_bg`、`cursor_fg`、`cursor_bg`（除 `kind` 外全为 `ratatui::style::Color`）
  - `impl TuiTheme { pub const DARK: TuiTheme; pub const LIGHT: TuiTheme; pub fn of(kind: TuiThemeKind) -> TuiTheme }`
  - `pub(crate) fn relative_luminance(color: Color) -> f32`

- [ ] **Step 1: 写失败测试**

创建 `yi-agent-rs/crates/yi-agent/src/tui/theme.rs`，先只放测试与占位：

```rust
//! TUI color themes: semantic slots + terminal background detection.

use ratatui::style::Color;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_themes_meet_contrast_floor() {
        // Every foreground slot must clear WCAG AA body text (4.5:1) against the
        // background it is assumed to sit on.
        for (theme, assumed_bg) in [
            (TuiTheme::DARK, Color::Rgb(0x1e, 0x1e, 0x1e)),
            (TuiTheme::LIGHT, Color::Rgb(0xff, 0xff, 0xff)),
        ] {
            for (name, fg) in [
                ("fg", theme.fg),
                ("fg_muted", theme.fg_muted),
                ("fg_faint", theme.fg_faint),
                ("accent", theme.accent),
                ("warn", theme.warn),
                ("danger", theme.danger),
                ("success", theme.success),
            ] {
                let ratio = contrast_ratio(fg, assumed_bg);
                assert!(
                    ratio >= 4.5,
                    "{:?} {name} is only {ratio:.2}:1 on the assumed background",
                    theme.kind
                );
            }
        }
    }

    #[test]
    fn raised_surface_is_distinguishable() {
        // The input stripe must read as a distinct band, not invisible.
        assert!(contrast_ratio(TuiTheme::DARK.bg_raised, Color::Rgb(0x1e, 0x1e, 0x1e)) >= 1.15);
        assert!(contrast_ratio(TuiTheme::LIGHT.bg_raised, Color::Rgb(0xff, 0xff, 0xff)) >= 1.15);
    }

    #[test]
    fn selection_text_is_readable() {
        for theme in [TuiTheme::DARK, TuiTheme::LIGHT] {
            let ratio = contrast_ratio(theme.sel_fg, theme.sel_bg);
            assert!(ratio >= 4.5, "{:?} selection is {ratio:.2}:1", theme.kind);
        }
    }

    #[test]
    fn body_text_on_raised_surface_is_readable() {
        for theme in [TuiTheme::DARK, TuiTheme::LIGHT] {
            let ratio = contrast_ratio(theme.fg, theme.bg_raised);
            assert!(ratio >= 4.5, "{:?} body-on-raised is {ratio:.2}:1", theme.kind);
        }
    }

    #[test]
    fn of_maps_kind_to_the_matching_constant() {
        assert_eq!(TuiTheme::of(TuiThemeKind::Dark).kind, TuiThemeKind::Dark);
        assert_eq!(TuiTheme::of(TuiThemeKind::Light).kind, TuiThemeKind::Light);
        assert_eq!(TuiTheme::of(TuiThemeKind::Light).fg, TuiTheme::LIGHT.fg);
    }

    /// WCAG 2.1 contrast ratio. Local (12 lines) instead of pulling a palette
    /// crate: we only need relative luminance of two sRGB colors.
    fn contrast_ratio(a: Color, b: Color) -> f32 {
        let la = relative_luminance(a);
        let lb = relative_luminance(b);
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::theme`
Expected: 编译失败，`cannot find type TuiTheme` / `cannot find function relative_luminance`

- [ ] **Step 3: 写最小实现**

在 `theme.rs` 的 `use ratatui::style::Color;` 之后、`mod tests` 之前插入：

```rust
/// Which terminal background the theme was picked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiThemeKind {
    Dark,
    Light,
}

/// Semantic color slots. Renderers never name a raw ANSI color: they ask for a
/// role (`fg_muted`, `accent`, ...) so the same code reads correctly on both a
/// light and a dark terminal.
///
/// Values are `Rgb` on purpose. ANSI slots (`Color::DarkGray`) resolve to
/// whatever the terminal's palette says, which is exactly why the old TUI went
/// unreadable on light backgrounds: the same SGR 90 is near-invisible on one
/// theme and correct on another.
#[derive(Debug, Clone, Copy)]
pub struct TuiTheme {
    pub kind: TuiThemeKind,
    /// Body foreground. Used where a foreground must be explicit (e.g. text on
    /// the input stripe), not to paint over the terminal default.
    pub fg: Color,
    /// Secondary: tool-result summaries, trace steps, popup empty states.
    pub fg_muted: Color,
    /// Faintest: model name, fold hints, "已略去 N 条更早的历史".
    pub fg_faint: Color,
    /// Emphasis: inline code, math, slash menu, "发消息>".
    pub accent: Color,
    /// In progress / advisory.
    pub warn: Color,
    /// Failure / denial / cancellation.
    pub danger: Color,
    /// Success / completion.
    pub success: Color,
    /// Raised surface background (input stripe, popup cards).
    pub bg_raised: Color,
    /// Selected row foreground / background.
    pub sel_fg: Color,
    pub sel_bg: Color,
    /// Cursor. Rendering uses REVERSED, so these only matter where a solid
    /// block is needed.
    pub cursor_fg: Color,
    pub cursor_bg: Color,
}

impl TuiTheme {
    /// Equivalent to the historical look, with the faint grays lightened: the
    /// old `DarkGray` (#585858) was only 2.34:1 on a dark background.
    pub const DARK: TuiTheme = TuiTheme {
        kind: TuiThemeKind::Dark,
        fg: Color::Rgb(0xe4, 0xe4, 0xe4),        // 13.11:1 on #1e1e1e
        fg_muted: Color::Rgb(0xb2, 0xb2, 0xb2),  //  7.86:1
        fg_faint: Color::Rgb(0x8c, 0x8c, 0x8c),  //  4.96:1
        accent: Color::Rgb(0x5f, 0xd7, 0xd7),    //  9.68:1
        warn: Color::Rgb(0xe3, 0xc5, 0x6b),      //  9.90:1
        danger: Color::Rgb(0xff, 0x7b, 0x72),    //  6.61:1
        success: Color::Rgb(0x7e, 0xe7, 0x87),   // 10.85:1
        bg_raised: Color::Rgb(0x2a, 0x2a, 0x2a), // 1.16 vs #1e1e1e
        sel_fg: Color::Rgb(0xff, 0xff, 0xff),    //  5.06:1 on sel_bg
        sel_bg: Color::Rgb(0x2f, 0x6f, 0xbf),
        cursor_fg: Color::Rgb(0x1e, 0x1e, 0x1e),
        cursor_bg: Color::Rgb(0xe4, 0xe4, 0xe4),
    };

    /// Re-picked values, not an inversion: inverting the dark palette yields
    /// pure black on pure white and blows the contrast budget.
    pub const LIGHT: TuiTheme = TuiTheme {
        kind: TuiThemeKind::Light,
        fg: Color::Rgb(0x1f, 0x1f, 0x1f),        // 16.48:1 on #ffffff
        fg_muted: Color::Rgb(0x4a, 0x4a, 0x4a),  //  8.86:1
        fg_faint: Color::Rgb(0x6b, 0x6b, 0x6b),  //  5.33:1
        accent: Color::Rgb(0x0a, 0x6b, 0x6b),    //  6.31:1
        warn: Color::Rgb(0x7a, 0x5c, 0x00),      //  6.25:1
        danger: Color::Rgb(0xa0, 0x18, 0x18),    //  7.96:1
        success: Color::Rgb(0x14, 0x69, 0x1f),   //  6.84:1
        bg_raised: Color::Rgb(0xed, 0xed, 0xed), // 1.17 vs #ffffff
        sel_fg: Color::Rgb(0xff, 0xff, 0xff),    //  6.44:1 on sel_bg
        sel_bg: Color::Rgb(0x1f, 0x5f, 0xa8),
        cursor_fg: Color::Rgb(0xff, 0xff, 0xff),
        cursor_bg: Color::Rgb(0x1f, 0x1f, 0x1f),
    };

    pub fn of(kind: TuiThemeKind) -> TuiTheme {
        match kind {
            TuiThemeKind::Dark => TuiTheme::DARK,
            TuiThemeKind::Light => TuiTheme::LIGHT,
        }
    }
}

/// WCAG 2.1 relative luminance of an sRGB color.
///
/// `Color::Reset` and the ANSI palette slots have no fixed sRGB value; they are
/// approximated as mid-gray so the math stays total instead of panicking.
pub(crate) fn relative_luminance(color: Color) -> f32 {
    fn channel(v: u8) -> f32 {
        let v = v as f32 / 255.0;
        if v <= 0.039_28 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    }
    let (r, g, b) = match color {
        Color::Rgb(r, g, b) => (r, g, b),
        // Not used by any theme slot; a neutral value keeps callers total.
        _ => (0x80, 0x80, 0x80),
    };
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}
```

同时在 `mod.rs` 加 `pub mod theme;`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::theme`
Expected: PASS，5 个测试全绿

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/tui/theme.rs crates/yi-agent/src/tui/mod.rs
git commit -m "feat(tui): add semantic theme slots with contrast-verified palettes"
```

---

### Task 2: OSC 11 背景探测 + 环境变量覆盖

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/theme.rs`（追加探测逻辑与测试）
- Test: 同文件 `#[cfg(test)] mod tests`（追加）

**Interfaces:**
- Consumes: Task 1 的 `TuiThemeKind`、`TuiTheme`、`relative_luminance`
- Produces:
  - `pub fn detect_theme_kind(env_override: Option<&str>, reply: Option<&str>) -> TuiThemeKind` —— 纯函数，可单测
  - `pub fn detect_theme_from_terminal() -> TuiThemeKind` —— 走真实 IO，内部调 `detect_theme_kind`
  - `pub fn parse_osc11_reply(bytes: &[u8]) -> Option<(u8, u8, u8)>`

- [ ] **Step 1: 写失败测试**

追加到 `theme.rs` 的 `mod tests` 内：

```rust
    #[test]
    fn env_var_overrides_probe() {
        // Even if the terminal says dark, an explicit override wins.
        assert_eq!(
            detect_theme_kind(Some("light"), Some("rgb:1e1e/1e1e/1e1e")),
            TuiThemeKind::Light
        );
        assert_eq!(
            detect_theme_kind(Some("dark"), Some("rgb:ffff/ffff/ffff")),
            TuiThemeKind::Dark
        );
    }

    #[test]
    fn env_var_is_trimmed_and_case_insensitive() {
        assert_eq!(
            detect_theme_kind(Some("  LIGHT  "), None),
            TuiThemeKind::Light
        );
        assert_eq!(detect_theme_kind(Some("Dark"), None), TuiThemeKind::Dark);
    }

    #[test]
    fn invalid_env_var_falls_through_to_probe() {
        // A typo must not lock the user into the wrong theme silently; fall
        // through to the terminal's own answer.
        assert_eq!(
            detect_theme_kind(Some("purple"), Some("rgb:ffff/ffff/ffff")),
            TuiThemeKind::Light
        );
        assert_eq!(
            detect_theme_kind(Some("purple"), Some("rgb:1e1e/1e1e/1e1e")),
            TuiThemeKind::Dark
        );
    }

    #[test]
    fn osc11_reply_parsed_as_light() {
        assert_eq!(
            parse_osc11_reply(b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\"),
            Some((0xff, 0xff, 0xff))
        );
        assert_eq!(
            detect_theme_kind(None, Some("rgb:ffff/ffff/ffff")),
            TuiThemeKind::Light
        );
    }

    #[test]
    fn osc11_reply_parsed_as_dark() {
        assert_eq!(
            detect_theme_kind(None, Some("rgb:1e1e/1e1e/1e1e")),
            TuiThemeKind::Dark
        );
    }

    #[test]
    fn osc11_bel_terminator_is_accepted() {
        // xterm answers with BEL instead of ST depending on the version.
        assert_eq!(
            parse_osc11_reply(b"\x1b]11;rgb:0000/0000/0000\x07"),
            Some((0x00, 0x00, 0x00))
        );
    }

    #[test]
    fn four_digit_components_are_scaled() {
        // `rgb:ffff/ffff/ffff` is 4 hex digits per channel, not 2.
        assert_eq!(parse_osc11_reply(b"\x1b]11;rgb:8080/8080/8080\x1b\\"), Some((0x80, 0x80, 0x80)));
        assert_eq!(parse_osc11_reply(b"\x1b]11;rgb:ff/ff/ff\x1b\\"), Some((0xff, 0xff, 0xff)));
    }

    #[test]
    fn probe_timeout_falls_back_to_dark() {
        // No reply at all: startup must not hang or panic.
        assert_eq!(detect_theme_kind(None, None), TuiThemeKind::Dark);
    }

    #[test]
    fn unparseable_reply_falls_back_to_dark() {
        assert_eq!(
            detect_theme_kind(None, Some("garbage")),
            TuiThemeKind::Dark
        );
        assert_eq!(
            detect_theme_kind(None, Some("rgb:zz/zz/zz")),
            TuiThemeKind::Dark
        );
    }

    #[test]
    fn mid_gray_lands_on_the_dark_side() {
        // Threshold is luminance > 0.5. #808080 is ~0.216, so it must be Dark:
        // a grayish terminal should keep the historical look rather than flip
        // to a light theme with weak contrast.
        assert_eq!(
            detect_theme_kind(None, Some("rgb:8080/8080/8080")),
            TuiThemeKind::Dark
        );
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::theme`
Expected: 编译失败，`cannot find function detect_theme_kind` / `parse_osc11_reply`

- [ ] **Step 3: 写实现**

在 `theme.rs` 的 `relative_luminance` 之后追加：

```rust
/// Resolve the theme from an explicit override and/or a raw OSC 11 reply.
///
/// Pure so the decision table is testable without a terminal. Precedence:
/// valid env override, then the reply's luminance, then `Dark`.
pub fn detect_theme_kind(env_override: Option<&str>, reply: Option<&str>) -> TuiThemeKind {
    match env_override.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        Some("light") => return TuiThemeKind::Light,
        Some("dark") => return TuiThemeKind::Dark,
        Some(other) if !other.is_empty() => {
            tracing::warn!(
                value = %other,
                "ignoring unknown YI_AGENT_THEME; falling back to background detection"
            );
        }
        _ => {}
    }

    let Some(parsed) = reply.and_then(|r| parse_osc11_reply(r.as_bytes())) else {
        return TuiThemeKind::Dark;
    };
    if relative_luminance(Color::Rgb(parsed.0, parsed.1, parsed.2)) > 0.5 {
        TuiThemeKind::Light
    } else {
        TuiThemeKind::Dark
    }
}

/// Parse `ESC ] 11 ; rgb:RRRR/GGGG/BBBB <ST|BEL>` into 8-bit components.
///
/// Accepts both 2- and 4-digit hex per channel (the xterm protocol allows
/// either) and both `ST` and `BEL` terminators.
pub fn parse_osc11_reply(bytes: &[u8]) -> Option<(u8, u8, u8)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let body = text
        .split_once("11;rgb:")
        .or_else(|| text.split_once("11:rgb:"))
        .map(|(_, rest)| rest)?;
    // Strip the trailing ST / BEL / stray escape bytes.
    let body = body
        .trim_end_matches('\x07')
        .split(['\x1b', '\x07'])
        .next()?;
    let mut parts = body.split('/');
    let mut channel = || -> Option<u8> {
        let raw = parts.next()?.trim();
        if raw.len() < 2 || raw.len() > 4 || !raw.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let value = u32::from_str_radix(raw, 16).ok()?;
        // 4-digit `ffff` → 255, 2-digit `ff` → 255.
        let scale = if raw.len() == 2 { 255 } else { 65535 };
        Some(((value * 255 + scale / 2) / scale) as u8)
    };
    Some((channel()?, channel()?, channel()?))
}

/// Detect the terminal background, falling back to `Dark` on any failure.
///
/// Never writes to stdout/stderr: a stray byte during TUI startup corrupts the
/// frame the same way the orphan-reclaim banner did (see docs/bug-list.md:101).
pub fn detect_theme_from_terminal() -> TuiThemeKind {
    let env_override = std::env::var("YI_AGENT_THEME").ok();
    if let Some(value) = env_override.as_deref() {
        let lowered = value.trim().to_ascii_lowercase();
        if lowered == "light" || lowered == "dark" {
            return detect_theme_kind(Some(value), None);
        }
    }
    let reply = query_osc11();
    detect_theme_kind(env_override.as_deref(), reply.as_deref())
}

/// Ask the terminal for its background color, bounded to 200ms.
///
/// Returns the raw reply text for `detect_theme_kind`. `None` on timeout, on a
/// non-tty, or when the terminal does not implement OSC 11.
fn query_osc11() -> Option<String> {
    use std::io::{Read, Write};

    if !crossterm::tty::IsTty::is_tty(&std::io::stdin()) {
        return None;
    }
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    out.write_all(b"\x1b]11;?\x1b\\").ok()?;
    out.flush().ok()?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
    let mut collected: Vec<u8> = Vec::new();
    let mut stdin = std::io::stdin();
    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            return None;
        }
        let remaining = deadline.saturating_duration_since(now);
        // Poll in 10ms slices: a blocking read would hang startup on terminals
        // that ignore OSC 11 entirely.
        if !crossterm::event::poll(remaining.min(std::time::Duration::from_millis(10)))
            .unwrap_or(false)
        {
            continue;
        }
        match crossterm::event::read() {
            Ok(crossterm::event::Event::Key(_)) => {
                // Terminal already swallowed our query and gave us input:
                // nothing to learn, and the byte must not leak.
                return None;
            }
            Ok(_) => {}
            Err(_) => return None,
        }
        let mut byte = [0u8; 1];
        if stdin.read(&mut byte).ok()? == 0 {
            return None;
        }
        collected.push(byte[0]);
        if byte[0] == 0x07 || (byte[0] == b'\\' && collected.len() >= 2) {
            break;
        }
        if collected.len() > 256 {
            return None;
        }
    }
    String::from_utf8(collected).ok()
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::theme`
Expected: PASS，15 个测试全绿

- [ ] **Step 5: 验证探测失败不写输出**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::theme -- --nocapture 2>&1 | grep -c "rgb:"`
Expected: `0`（测试过程不回显任何 OSC 内容；`detect_theme_kind` 是纯函数，不触 IO）

- [ ] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/tui/theme.rs
git commit -m "feat(tui): detect terminal background via OSC 11 with env override"
```

---

### Task 3: 光标改用 REVERSED

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:3594`（`wrap_input_buffer` 的 `cursor_style`）
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:7582,7640,7690`（3 个光标测试）

**Interfaces:**
- Consumes: 无（本任务只用 ratatui 的 `Modifier`）
- Produces: `wrap_input_buffer` 产生的光标 span 带 `Modifier::REVERSED`，且 `bg` 为 `Color::Reset`

**为什么单独成任务：** 这是唯一一处「不靠主题值也能修好」的缺陷 —— `REVERSED` 由终端自己反色，两种背景都成立。它不依赖 Task 1 的常量，因此可以独立验证。

- [ ] **Step 1: 改写 3 个既有测试为先失败**

`app.rs:7582` `cursor_shown_with_reverse_video`：把查找 `cell.bg == Color::White` 的循环替换为断言 modifier：

```rust
        // The cursor is reverse video, not a hardcoded white block: a white
        // block is invisible on a white terminal (contrast 1.00:1).
        let buffer = terminal.backend().buffer();
        let input_row = 23u16;
        let mut found_cursor = false;
        for x in 0..80u16 {
            let cell = &buffer[(x, input_row)];
            if cell.modifier.contains(ratatui::style::Modifier::REVERSED) {
                found_cursor = true;
                break;
            }
        }
        assert!(
            found_cursor,
            "expected a REVERSED cursor cell in input row"
        );
```

`app.rs:7640` `cursor_visible_on_empty_buffer` 与 `app.rs:7690` `cursor_in_middle_of_text`：做同样的替换（把 `cell.bg == ratatui::style::Color::White` 改为 `cell.modifier.contains(ratatui::style::Modifier::REVERSED)`），并把断言消息改为 `"expected cursor on empty buffer"` / 对应描述保持不变。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::app::tests::cursor`
Expected: FAIL —— 3 个测试都报 `expected a REVERSED cursor cell`

- [ ] **Step 3: 改实现**

`app.rs:3594`，把：

```rust
    let cursor_style = Style::new().fg(Color::Black).bg(Color::White);
```

改为：

```rust
    // Reverse video instead of a hardcoded white block: REVERSED is resolved by
    // the terminal against its own background, so the cursor stays visible on
    // both light and dark themes. A fixed `bg(White)` is 1.00:1 on a white
    // terminal — literally invisible.
    let cursor_style = Style::new().add_modifier(Modifier::REVERSED);
```

确认 `Modifier` 已在文件顶部 `use` 中（`app.rs` 已使用 `Modifier::BOLD | Modifier::DIM`，无需新增 import）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::app::tests::cursor`
Expected: PASS，3 个测试全绿

- [ ] **Step 5: 跑全量 app 测试确认无连带回归**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::`
Expected: PASS（若有其它测试断言光标为 White，在此步暴露并一并改写）

- [ ] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/tui/app.rs
git commit -m "fix(tui): render cursor with REVERSED so it survives light terminals"
```

---

### Task 4: 输入条与状态栏接入主题

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:3508-3522`（`build_input_line`）
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/statusbar.rs`（`render_statusbar` + `spinner_color`）
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:598`（`render_statusbar` 调用点）

**Interfaces:**
- Consumes: `TuiTheme`（Task 1）
- Produces:
  - `fn build_input_line(input: &InputLine, pending_quit: bool, area_width: u16) -> Paragraph<'static>` —— **签名不变**，默认 `TuiTheme::DARK`
  - `fn build_input_line_with(input: &InputLine, pending_quit: bool, area_width: u16, theme: TuiTheme) -> Paragraph<'static>`
  - `pub fn render_statusbar(...) -> Line<'a>` —— **签名不变**，转发 `DARK`
  - `pub fn render_statusbar_with(..., theme: TuiTheme) -> Line<'a>`
  - `pub fn spinner_color_for(&self, theme: TuiTheme) -> Color`；`spinner_color(&self)` 保留并转发 `DARK`

- [ ] **Step 1: 写失败测试**

追加到 `app.rs` 的 `mod tests`：

```rust
    /// The input stripe must follow the theme: on a light terminal the old
    /// hardcoded `Indexed(240)` painted a dark gray band across a white screen.
    #[test]
    fn input_stripe_uses_theme_raised() {
        let input = InputLine::new();
        let dark = build_input_line_with(&input, false, 40, TuiTheme::DARK);
        let light = build_input_line_with(&input, false, 40, TuiTheme::LIGHT);
        // Paragraph style is not directly readable, so render one row into a
        // scratch buffer and inspect the background of the padded cells.
        let area = ratatui::layout::Rect::new(0, 0, 40, 1);
        let mut dark_buf = ratatui::buffer::Buffer::empty(area);
        let mut light_buf = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::Widget::render(dark, area, &mut dark_buf);
        ratatui::widgets::Widget::render(light, area, &mut light_buf);
        assert_eq!(dark_buf[(39, 0)].bg, TuiTheme::DARK.bg_raised);
        assert_eq!(light_buf[(39, 0)].bg, TuiTheme::LIGHT.bg_raised);
        assert_ne!(light_buf[(39, 0)].bg, Color::Indexed(240));
    }
```

追加到 `statusbar.rs` 的 `mod tests`：

```rust
    /// The prefill/decode counters used `Color::Gray`, which is 1.90:1 on a
    /// white background — effectively invisible.
    #[test]
    fn token_counters_use_theme_muted_not_raw_gray() {
        let mut state = StatusBarState::default();
        state.set_token_target(1200, 340);
        let line = render_statusbar_with(&state, &RunningTaskRegistry::default(), 0, 0, "m", 80, TuiTheme::LIGHT);
        let flat: Vec<_> = line.spans.iter().collect();
        assert!(
            flat.iter().any(|s| s.style.fg == Some(TuiTheme::LIGHT.fg_muted)),
            "expected a span painted with the light theme's muted foreground"
        );
        assert!(
            !flat.iter().any(|s| s.style.fg == Some(Color::Gray)),
            "raw Color::Gray must not reach the light theme"
        );
    }

    /// The breathing dot interpolates between two theme colors instead of
    /// gray↔white, where the brightest phase was 1.00:1 on white.
    #[test]
    fn spinner_pulses_between_theme_colors() {
        let mut state = StatusBarState::default();
        // Phase 0 and 180 are the two ends of the sine wave.
        state.spinner_phase = 270; // sin = -1 → dimmest
        let dim = state.spinner_color_for(TuiTheme::LIGHT);
        state.spinner_phase = 90; // sin = +1 → brightest
        let bright = state.spinner_color_for(TuiTheme::LIGHT);
        assert_eq!(dim, TuiTheme::LIGHT.fg_faint);
        assert_eq!(bright, TuiTheme::LIGHT.fg);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent input_stripe_uses_theme_raised`
Expected: 编译失败，`cannot find function build_input_line_with`

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::statusbar`
Expected: 编译失败，`cannot find function render_statusbar_with` / `spinner_color_for`

- [ ] **Step 3: 实现**

`app.rs`：把 `build_input_line` 改写为转发：

```rust
fn build_input_line(input: &InputLine, pending_quit: bool, area_width: u16) -> Paragraph<'static> {
    build_input_line_with(input, pending_quit, area_width, TuiTheme::DARK)
}

fn build_input_line_with(
    input: &InputLine,
    pending_quit: bool,
    area_width: u16,
    theme: TuiTheme,
) -> Paragraph<'static> {
    let prefix = Span::styled(
        "> ",
        Style::new().add_modifier(Modifier::BOLD | Modifier::DIM),
    );
    if pending_quit {
        return Paragraph::new(Line::from(vec![
            prefix,
            Span::styled("再按 Ctrl+C 退出", Style::new().fg(theme.warn)),
        ]))
        .style(Style::new().bg(theme.bg_raised));
    }
    let lines = wrap_input_buffer(&input.buffer, input.cursor, &prefix, area_width);
    Paragraph::new(Text::from(lines)).style(Style::new().bg(theme.bg_raised))
}
```

`app.rs:3497` 的选中行颜色也一并换：

```rust
                Style::new().bg(theme.sel_bg).fg(theme.sel_fg)
```

（`build_popup` 需加 `theme: TuiTheme` 参数，`app.rs:593` 的调用点传 `theme`。）

`statusbar.rs`：`render_statusbar` 改为转发，主体搬到 `render_statusbar_with`，把 4 处颜色替换：

| 行 | 旧 | 新 |
|---|---|---|
| 237 | `Color::DarkGray` | `theme.fg_faint` |
| 248 | `Color::Gray` | `theme.fg_muted` |
| 257 | `Color::Gray` | `theme.fg_muted` |
| 268 | `Color::DarkGray` | `theme.fg_faint` |

`spinner_color` 改为：

```rust
    pub fn spinner_color(&self) -> Color {
        self.spinner_color_for(TuiTheme::DARK)
    }

    /// Breathing dot between the theme's faint and body foreground.
    ///
    /// The old gray↔white ramp peaked at `Rgb(255,255,255)`, which is 1.00:1 on
    /// a white background — the dot vanished once per pulse on light terminals.
    pub fn spinner_color_for(&self, theme: TuiTheme) -> Color {
        let phase = self.spinner_phase as f32 * std::f32::consts::PI / 180.0;
        let t = (phase.sin() + 1.0) / 2.0; // 0.0 dim ..= 1.0 bright
        lerp_color(theme.fg_faint, theme.fg, t)
    }
```

并在 `statusbar.rs` 内加辅助函数：

```rust
/// Linear RGB interpolation. Used for the spinner ramp so both endpoints are
/// theme-controlled.
fn lerp_color(from: Color, to: Color, t: f32) -> Color {
    let (Color::Rgb(fr, fg, fb), Color::Rgb(tr, tg, tb)) = (from, to) else {
        return to;
    };
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    Color::Rgb(mix(fr, tr), mix(fg, tg), mix(fb, tb))
}
```

同时确保 `spinner_pulses_between_theme_colors` 里 `phase=270 → sin=-1 → t=0 → fg_faint`、`phase=90 → sin=+1 → t=1 → fg`。

`app.rs:598` 的调用点改为 `render_statusbar_with(..., theme)`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::statusbar`
Expected: PASS（含 2 个新测试 + 既有 statusbar 测试）

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent input_stripe_uses_theme_raised`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/tui/app.rs crates/yi-agent/src/tui/statusbar.rs
git commit -m "feat(tui): drive input stripe and status bar from the theme"
```

---

### Task 5: 正文渲染接入主题（`cell.rs` / `markdown.rs` / `history.rs`）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/cell.rs`（`lines` → `lines_with`，7 个 `render_*` 收 theme）
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/markdown.rs`（`render_markdown` → `render_markdown_with`，`LineBuilder` 收 theme，4 处颜色）
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/history.rs`（`HistoryView.theme`、缓存 key 追加 theme）

**Interfaces:**
- Consumes: `TuiTheme`（Task 1）
- Produces:
  - `impl HistoryCell { pub fn lines(&self, width: u16) -> Vec<Line<'static>>; pub fn lines_with(&self, width: u16, theme: TuiTheme) -> Vec<Line<'static>> }`
  - `pub fn render_markdown(src: &str, width: u16) -> Vec<Line<'static>>`（不变）
  - `pub fn render_markdown_with(src: &str, width: u16, theme: TuiTheme) -> Vec<Line<'static>>`
  - `pub struct HistoryView<'a> { pub state: &'a HistoryState, pub width: u16, pub theme: TuiTheme }`
  - `HistoryState::ensure_cache(&self, width: u16, theme: TuiThemeKind)`

- [ ] **Step 1: 写失败测试**

追加到 `markdown.rs` 的 `mod tests`：

```rust
    /// Inline code used raw `Color::Cyan` (2.93:1 on white). It must follow the
    /// theme's accent slot instead.
    #[test]
    fn inline_code_uses_theme_accent() {
        let lines = render_markdown_with("`code`", 40, TuiTheme::LIGHT);
        let span = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains("code"))
            .expect("inline code span");
        assert_eq!(span.style.fg, Some(TuiTheme::LIGHT.accent));
        assert_ne!(span.style.fg, Some(Color::Cyan));
    }
```

追加到 `cell.rs` 的 `mod tests`：

```rust
    /// Folded tool-result summaries used `Color::DarkGray`, which is 2.34:1 on a
    /// dark background. Field names match `HistoryCell::ToolResult` at
    /// `cell.rs:27` (`id` / `result_text` / `is_error` / `expanded`).
    #[test]
    fn tool_result_summary_uses_theme_muted() {
        let cell = HistoryCell::ToolResult {
            id: "t1".into(),
            result_text: "first line\nsecond line".into(),
            is_error: false,
            expanded: false,
        };
        let lines = cell.lines_with(60, TuiTheme::DARK);
        let summary = lines.first().expect("a summary line");
        assert_eq!(summary.spans[0].style.fg, Some(TuiTheme::DARK.fg_muted));
        assert_ne!(summary.spans[0].style.fg, Some(Color::DarkGray));
    }
```

追加到 `history.rs` 的 `mod tests`：

```rust
    /// A theme change must invalidate the render cache: cached lines carry
    /// colors baked in at render time. Today the theme is fixed for the process
    /// lifetime, but keying the cache prevents a future `/theme` from painting
    /// stale colors.
    ///
    /// Mirrors the existing `cache_is_current` conditions
    /// (history.rs:443-448) plus the new theme comparison.
    #[test]
    fn changing_the_theme_invalidates_the_render_cache() {
        let mut state = HistoryState::new();
        state.push(
            HistoryCell::Markdown {
                text: "`x`".into(),
            },
            40,
        );
        state.ensure_cache(40, TuiTheme::DARK);
        assert!(state.cache.borrow().is_current_for(40, TuiThemeKind::Dark));
        assert!(!state.cache.borrow().is_current_for(40, TuiThemeKind::Light));
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent inline_code_uses_theme_accent`
Expected: 编译失败，`cannot find function render_markdown_with`

- [ ] **Step 3: 实现 `markdown.rs`**

在 `use ratatui::style::{Color, ...}` 处保留 `Color`（测试仍断言 `Color::Cyan`），新增 `use crate::tui::theme::TuiTheme;`。

```rust
pub fn render_markdown(src: &str, width: u16) -> Vec<Line<'static>> {
    render_markdown_with(src, width, TuiTheme::DARK)
}

pub fn render_markdown_with(src: &str, width: u16, theme: TuiTheme) -> Vec<Line<'static>> {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_MATH;
    let normalized = normalize_math_delimiters(src);
    let parser = Parser::new_ext(&normalized, opts);
    let mut builder = LineBuilder::new(width, theme);
    for event in parser {
        builder.handle_event(event);
    }
    builder.finish()
}
```

`LineBuilder` 加 `theme: TuiTheme` 字段，`new(width: u16, theme: TuiTheme)`。4 处颜色替换：

| 行 | 旧 | 新 |
|---|---|---|
| 457 | `Color::Cyan`（行内代码） | `self.theme.accent` |
| 466 | `Color::Cyan`（inline math） | `self.theme.accent` |
| 477 | `Color::Cyan`（block math） | `self.theme.accent` |
| 523 | `Color::Green`（链接） | `self.theme.success` |
| 529 | `Color::Cyan` | `self.theme.accent` |
| 740 | `Color::Yellow` | 先读代码确认语义：若为「警示/标记」则 `self.theme.warn`；若为「强调」则 `self.theme.accent`。**不得凭色名硬套。** |

- [ ] **Step 4: 实现 `cell.rs`**

```rust
    pub fn lines(&self, width: u16) -> Vec<Line<'static>> {
        self.lines_with(width, TuiTheme::DARK)
    }

    pub fn lines_with(&self, width: u16, theme: TuiTheme) -> Vec<Line<'static>> {
        #[cfg(test)]
        note_line_render();
        match self {
            Self::UserMessage { text } => render_user_message(text, width),
            Self::Markdown { text } => super::markdown::render_markdown_with(text, width, theme),
            Self::AssistantMessage { markdown } => {
                super::markdown::render_markdown_with(markdown, width, theme)
            }
            Self::ToolCall { name, input, state, expanded, .. } => {
                render_tool_call(name, input, *state, *expanded, width, theme)
            }
            // ...其余分支同样把 theme 透传给各自的 render_*
        }
    }
```

映射表（`cell.rs` 内）：

| 行 | 旧 | 新 |
|---|---|---|
| 202 | `Color::Yellow`（running 点） | `theme.warn` |
| 203 | `Color::Green` | `theme.success` |
| 204 | `Color::Red` | `theme.danger` |
| 223, 233, 313 | `Color::DarkGray` | `theme.fg_muted` |
| 246 | `Color::Red/Green`（箭头） | `theme.danger` / `theme.success` |
| 366 | `Color::Yellow`（warn） | `theme.warn` |
| 368 | `Color::Cyan`（menu） | `theme.accent` |
| 423 | `Color::Red`（错误） | `theme.danger` |
| 464-469 | `Color::Green` / `Color::Red`（权限决定） | `theme.success` / `theme.danger` |

- [ ] **Step 5: 实现 `history.rs`**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tui) struct CacheKey {
    width: u16,
    theme_kind: TuiThemeKind,
}
```

在 `HistoryCache` 里把 `width: u16` 换成 `key: CacheKey`，并加：

```rust
    /// Same three conditions the old `cache_is_current` used
    /// (`history.rs:443-448`: width, reflected generation, entry count),
    /// plus the theme identity.
    ///
    /// `HistoryCache` has no `generation` / `cell_count` fields of its own; the
    /// generation and cell count live on `HistoryState`, so keep this on the
    /// state:
    fn is_current_for(&self, width: u16, theme_kind: TuiThemeKind) -> bool {
        let cache = self.cache.borrow();
        cache.width == width
            && cache.theme_kind == theme_kind
            && cache.reflected_generation == self.content_generation.get()
            && cache.entries.len() == self.cells.len()
    }
```

即：给 `HistoryCache` 追加 `theme_kind: TuiThemeKind` 字段，把上面这个 `is_current_for` 方法放在 `impl HistoryState` 里，并让原 `cache_is_current(&self, width: u16)`（`history.rs:443`）改为调用 `self.is_current_for(width, TuiThemeKind::Dark)` 以保持旧行为，供既有测试复用。

`ensure_cache(&self, width: u16, theme: TuiTheme)`：`theme` 一路透传到 `cell.lines_with(width, theme)`（`history.rs:416`、`:1787`、`:2058` 三处调用）。

`HistoryView` 加 `pub theme: TuiTheme`，`render`（`history.rs:1276`）用 `self.theme` 调 `ensure_cache` / `text_width`。

`app.rs:583` 的构造处加 `theme`。

- [ ] **Step 6: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent inline_code_uses_theme_accent`
Expected: PASS

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::markdown`
Expected: PASS（既有断言 `fg == Some(Color::Cyan)` 仍走 `render_markdown` 的深色默认路径；若断言值已变，说明转发写错，回 Step 3）

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::history`
Expected: PASS（含新缓存测试）

- [ ] **Step 7: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/tui/cell.rs crates/yi-agent/src/tui/markdown.rs crates/yi-agent/src/tui/history.rs
git commit -m "feat(tui): route transcript rendering through the theme"
```

---

### Task 6: 弹窗与排队预览接入主题

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/bash_popup.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/process_popup.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/trace.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/queued.rs`

**Interfaces:**
- Consumes: `TuiTheme`（Task 1）
- Produces（全部保留旧签名并转发 `DARK`）：
  - `bash_popup::detail_lines(task: &TaskState, width: u16)` → `detail_lines_with(task, width, theme)`
  - `bash_popup::render_list_popup(popup, tasks, area)` → `render_list_popup_with(popup, tasks, area, theme)`
  - `bash_popup::render_detail_popup(popup, task, area)` → `render_detail_popup_with(popup, task, area, theme)`
  - `process_popup::process_detail_lines(...)` → `process_detail_lines_with(..., theme)`
  - `process_popup::render_process_list_popup(...)` → `+_with`
  - `process_popup::render_process_detail_popup(...)` → `+_with`
  - `trace::render_subagent_list(...)` → `+_with`
  - `trace::render_subagent_detail(...)` → `+_with`
  - `queued::render_queued_preview(items, width)` → `render_queued_preview_with(items, width, theme)`

- [ ] **Step 1: 写失败测试**

追加到 `bash_popup.rs` 的 `mod tests`：

```rust
    /// Popup cards used `Clear` alone, which leaves the terminal's own
    /// background: on light terminals the card has no edge at all.
    ///
    /// Builds the task through `RunningTaskRegistry` exactly as
    /// `test_render_detail_popup_smoke` does (bash_popup.rs tests), and
    /// `DetailPopup::new` takes a `String` (bash_popup.rs:63).
    #[test]
    fn detail_popup_paints_the_theme_raised_surface() {
        let mut tasks = RunningTaskRegistry::new();
        tasks.on_tool_call("t1", "bash", "echo hello", 120);
        tasks.on_exit("t1", Some(0));
        let task = tasks.get("t1").unwrap();
        let popup = DetailPopup::new("t1".into());
        let area = Rect::new(0, 0, 60, 10);
        let para = render_detail_popup_with(&popup, task, area, TuiTheme::LIGHT);
        let mut buf = Buffer::empty(area);
        Widget::render(para, area, &mut buf);
        assert_eq!(buf[(59, 9)].bg, TuiTheme::LIGHT.bg_raised);
    }
```

追加到 `trace.rs` 的 `mod tests`：

```rust
    /// `state_color` fell back to `Color::DarkGray` (2.34:1 on dark).
    #[test]
    fn unknown_state_uses_theme_faint() {
        assert_eq!(state_color_with("no_such_state", TuiTheme::DARK), TuiTheme::DARK.fg_faint);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent detail_popup_paints_the_theme_raised_surface`
Expected: 编译失败，`cannot find function render_detail_popup_with`

- [ ] **Step 3: 实现四个文件**

统一模式：`pub fn old(...) { new(..., TuiTheme::DARK) }`。逐项颜色映射：

`bash_popup.rs`：

| 行 | 旧 | 新 |
|---|---|---|
| 120-124 | `Yellow/Green/Red/Red/DarkGray` | `theme.warn/success/danger/danger/fg_faint` |
| 127 | `bg(Blue).fg(White)` | `bg(theme.sel_bg).fg(theme.sel_fg)` |
| 182-186 | 同上 | 同上 |
| 205 | `Color::Red` | `theme.danger` |
| 212,215,221,224,232 | `Color::DarkGray` | `theme.fg_faint` |

`process_popup.rs`：

| 行 | 旧 | 新 |
|---|---|---|
| 122-125 | `Yellow/Green/DarkGray/Red` | `theme.warn/success/fg_faint/danger` |
| 144 | `bg(Blue).fg(White)` | `bg(theme.sel_bg).fg(theme.sel_fg)` |
| 243,250,253,260,265 | `Color::DarkGray` | `theme.fg_faint` |

`trace.rs`：

| 行 | 旧 | 新 |
|---|---|---|
| 363-366 | `Green/Red/Yellow/DarkGray` | `theme.success/danger/warn/fg_faint` |
| 396,400,483,492,562,570,575,587,592 | `Color::DarkGray` | `theme.fg_faint` |
| 545 | `Color::Yellow` | `theme.warn` |
| 551 | `Color::Red` | `theme.danger` |
| 556,558 | `Color::Cyan` | `theme.accent` |

`state_color` 改为 `state_color_with(state: &str, theme: TuiTheme) -> Color`，保留 `state_color(state)` 转发 `DARK`。

`queued.rs`：只有 `Modifier::DIM`（137-144 行附近），**无颜色**，只需加 `theme` 参数并透传（为将来铺路）；若确认无颜色则本文件只改签名为 `_with` 并让旧函数转发。

弹窗背景：`render_detail_popup_with` / `render_process_detail_popup_with` / `render_list_popup_with` 返回的 `Paragraph` 加 `.style(Style::new().bg(theme.bg_raised))`。`app.rs:627` 等处的 `Clear` 保留（先清再铺底，避免残影）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::bash_popup`
Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::process_popup`
Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::trace`
Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::queued`
Expected: 全部 PASS

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/tui/bash_popup.rs crates/yi-agent/src/tui/process_popup.rs crates/yi-agent/src/tui/trace.rs crates/yi-agent/src/tui/queued.rs
git commit -m "feat(tui): theme the popups and queued preview"
```

---

### Task 7: 启动接线、静态门禁与端到端验收

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`（`run_tui` 探测 + `run_loop` 参数 + `run_tui_with_backend_and_events` 主题变体）
- Create: `yi-agent-rs/crates/yi-agent/tests/tui_theme_guard.rs`
- Modify: `docs/project-management/yi-agent-tui.md`

**Interfaces:**
- Consumes: `detect_theme_from_terminal`（Task 2）、全部 `*_with` 变体（Task 4-6）
- Produces:
  - `run_tui` 在 `enable_raw_mode()` 与 `execute!(... EnterAlternateScreen ...)` 之间调用 `detect_theme_from_terminal()`
  - `run_loop(..., theme: TuiTheme)` 新参数
  - `pub fn run_tui_with_backend_and_events_themed(..., theme: TuiTheme)`；现有 `run_tui_with_backend_and_events` 转发 `TuiTheme::DARK`（31 处调用零改动）

- [ ] **Step 1: 写失败测试**

新增 `yi-agent-rs/crates/yi-agent/tests/tui_theme_guard.rs`：

```rust
//! Static guard: only `tui/theme.rs` may name raw values that do not adapt to
//! the terminal background.
//!
//! This is the TUI-side counterpart of `desktop/src/lib/themeTokens.test.ts`,
//! which caught the same class of bug (literal color scales pinned to one
//! theme) on the desktop side.

use std::path::{Path, PathBuf};

const BANNED: &[&str] = &["Color::Gray", "Color::DarkGray", "Color::Indexed("];

fn tui_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tui")
}

/// Strip `#[cfg(test)]` modules so test assertions may still name raw colors
/// (they legitimately assert the *old* values when checking a behavior).
fn production_source(path: &Path) -> String {
    let text = std::fs::read_to_string(path).expect("source readable");
    // Cut at the first `#[cfg(test)]` line: everything after is tests.
    match text.find("#[cfg(test)]") {
        Some(idx) => text[..idx].to_string(),
        None => text,
    }
}

#[test]
fn no_raw_colors_outside_theme_module() {
    let dir = tui_dir();
    let mut violations: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("tui dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        if path.file_name().and_then(|n| n.to_str()) == Some("theme.rs") {
            continue; // the one place raw values are allowed
        }
        let source = production_source(&path);
        for (number, line) in source.lines().enumerate() {
            // An explicit opt-out, so a legitimate exception is greppable.
            if line.contains("theme-guard: allow") {
                continue;
            }
            for banned in BANNED {
                if line.contains(banned) {
                    violations.push(format!("{}:{}: {banned}", path.display(), number + 1));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "raw terminal-dependent colors must move into tui/theme.rs:\n{}",
        violations.join("\n")
    );
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --test tui_theme_guard`
Expected: FAIL，列出所有仍在使用 `Color::Gray` / `Color::DarkGray` / `Color::Indexed(` 的行（Task 4-6 若已做完，此列表应只剩 `app.rs` 的遗留）

- [ ] **Step 3: 接线 `run_tui`**

`app.rs:132-133`：

```rust
    enable_raw_mode()?;
    // Detect the terminal background BEFORE the alternate screen takes over:
    // the query needs raw mode, and any stray output must land in the normal
    // scrollback rather than inside a frame being drawn.
    let theme = crate::tui::theme::TuiTheme::of(
        crate::tui::theme::detect_theme_from_terminal(),
    );
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
```

`run_loop` 加 `theme: TuiTheme` 参数（放在 `model` 之前或末尾均可，但必须同步所有调用点），并把 `theme` 传给 `HistoryView { state: history, width: chunks[0].width, theme }`、`build_input_line_with`、`render_statusbar_with`、`build_popup(...)` 及三个弹窗渲染函数。

`run_tui_with_backend_and_events` 保留原签名，内部转调新变体：

```rust
#[cfg(test)]
pub fn run_tui_with_backend_and_events<B: Backend, E: EventSource>(...) -> std::io::Result<()> {
    run_tui_with_backend_and_events_themed(..., TuiTheme::DARK)
}

#[cfg(test)]
pub fn run_tui_with_backend_and_events_themed<B: Backend, E: EventSource>(
    ...,
    theme: TuiTheme,
) -> std::io::Result<()> { /* 原实现，run_loop 传 theme */ }
```

- [ ] **Step 4: 跑门禁确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --test tui_theme_guard`
Expected: PASS

- [ ] **Step 5: 验证门禁真的会红（变异测试）**

临时在 `statusbar.rs` 的非测试区插入一行 `let _bait = Color::Gray;`，再跑：

Run: `cd yi-agent-rs && cargo test -p yi-agent --test tui_theme_guard`
Expected: FAIL，报 `statusbar.rs:NN: Color::Gray`

然后**删除这行诱饵**，重跑确认 PASS。

- [ ] **Step 6: 端到端快照**

追加到 `app.rs` 的 `mod tests`：

```rust
    /// Whole-frame acceptance: with the light theme, no cell may be painted with
    /// the values that were unreadable on a white terminal.
    #[test]
    fn light_theme_frame_has_no_unreadable_cells() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let events = Rc::new(RefCell::new(vec![Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
        ))]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events_themed(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
            TuiTheme::LIGHT,
        )
        .unwrap();

        let buffer = terminal.backend().buffer();
        let mut bad: Vec<(u16, u16, Color)> = Vec::new();
        for y in 0..30u16 {
            for x in 0..100u16 {
                let cell = &buffer[(x, y)];
                let offender = matches!(
                    cell.fg,
                    Color::Gray | Color::DarkGray | Color::White
                ) || cell.bg == Color::Indexed(240);
                if offender {
                    bad.push((x, y, cell.fg));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "light theme left unreadable cells (first 5): {:?}",
            &bad[..bad.len().min(5)]
        );
    }
```

- [ ] **Step 7: 跑端到端测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent light_theme_frame_has_no_unreadable_cells`
Expected: PASS

若失败，按报出的坐标定位是哪个渲染器漏改，回到对应 Task 修，**不要**放宽断言。

- [ ] **Step 8: 更新模块文档**

在 `docs/project-management/yi-agent-tui.md` 的 Features 列表末尾追加一行：

```markdown
- [x] 浅色终端配色适配 — 新增 `tui/theme.rs` 承载 `TuiTheme`（12 个语义槽位，`Rgb` 取值，两侧各槽位对比度 >= 4.5:1）；启动时 `detect_theme_from_terminal()` 经 OSC 11 探测背景（200ms 上限，失败回退 `Dark`，`YI_AGENT_THEME=light|dark` 强制覆盖），探测在 `enable_raw_mode()` 之后、备用屏幕之前完成且失败不写任何输出；渲染层每个函数保留旧签名并转发 `*_with(..., theme)`（`cell.rs` / `markdown.rs` / `statusbar.rs` / `bash_popup.rs` / `process_popup.rs` / `trace.rs` / `queued.rs` / `app.rs`），`HistoryView` 携带 theme 且 `HistoryState` 缓存 key 追加 `TuiThemeKind`；光标由 `fg(Black)+bg(White)` 改为 `Modifier::REVERSED`（白底上原实现对比度 1.00:1）；输入条背景由 `Indexed(240)` 改为 `theme.bg_raised`；状态栏 `Color::Gray`（白底 1.90:1）改为 `theme.fg_muted`，呼吸点改在 `fg_faint↔fg` 间插值；新增静态门禁 `tests/tui_theme_guard.rs` 禁止 `theme.rs` 之外出现裸 `Color::Gray` / `Color::DarkGray` / `Color::Indexed(`。代码：`yi-agent-rs/crates/yi-agent/src/tui/theme.rs`；验证：`cargo test -p yi-agent --bin yi-agent tui::theme`、`cargo test -p yi-agent --test tui_theme_guard`、`cargo test -p yi-agent --bin yi-agent tui::` — [设计](../superpowers/specs/2026-10-31-tui-light-terminal-theme-design.md)
```

同步 `docs/project-management/README.md` 模块索引表里 `yi-agent-tui` 的计数（`36 / 37` → `37 / 37`，若该行是此格式；以实际表头为准）。

- [ ] **Step 9: 全量回归**

```bash
cd yi-agent-rs && ps aux | grep -v grep | grep cargo || true
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::
cd yi-agent-rs && cargo test -p yi-agent --test tui_theme_guard
cd yi-agent-rs && cargo clippy -p yi-agent --all-targets -- -D warnings
cd yi-agent-rs && cargo fmt --all -- --check
```

Expected: 全部 PASS / 零 warning / 格式干净。

- [ ] **Step 10: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/tui/app.rs crates/yi-agent/tests/tui_theme_guard.rs ../docs/project-management/yi-agent-tui.md ../docs/project-management/README.md
git commit -m "feat(tui): wire theme detection at startup and guard against raw colors"
```

---

## 手工验收（实现者必做）

自动测试只能证明「色值落在预期集合」，证明不了「看起来对」。以下在真实终端里做：

1. **浅色 Terminal.app**
   - `yi-agent` 启动后输入条是**浅灰**横条（不是深灰斑）
   - 光标在输入框里**看得见**
   - 状态栏 `prefill` / `decode` 数字**清晰可读**
   - 对话跑起来时状态栏呼吸点**全程可见**（不再有「亮到消失」的相位）
   - Ctrl+P 弹窗在浅底上有**可区分的背景**，不是和正文糊在一起
2. **深色终端**（iTerm / Terminal 深色配置）
   - 无「比今天更糟」的区域
   - 记录：次要灰字会比今天**更亮**（`#585858` → `#b2b2b2`，这是把 2.34:1 修到 7.86:1 的结果）
3. **覆盖**
   - 深色终端里 `YI_AGENT_THEME=light yi-agent` → 浅色主题生效
   - 浅色终端里 `YI_AGENT_THEME=dark yi-agent` → 深色主题生效
   - `YI_AGENT_THEME=purple yi-agent` → 不崩，落回探测结果
4. **非 TTY**
   - `echo hi | yi-agent ...` 之类非交互路径不挂、不打印任何 OSC 字节

## Self-Review

**Spec 覆盖检查：**

| Spec 要求 | 落在哪个 Task |
|---|---|
| §3.1 语义槽位 | Task 1 |
| §3.2 取值 + 对比度 | Task 1（含断言） |
| §3.3 OSC 11 + 环境变量 | Task 2 |
| §3.4 保留旧签名 + `_with` 变体 | Task 4/5/6（各文件），Task 7（`run_tui_with_backend_and_events_themed`） |
| §3.5 逐项修复 | 输入条/状态栏 → Task 4；正文 → Task 5；弹窗 → Task 6；光标 → Task 3 |
| §3.6 缓存 key | Task 5 Step 5 |
| §3.7 不污染 | Task 2 Step 3（`tracing::warn!`）+ Task 7 手工验收 4 |
| §4.1 主题取值测试 | Task 1 |
| §4.2 探测测试 | Task 2 |
| §4.3 渲染测试 | Task 4（输入条/状态栏）、Task 5（正文/缓存）、Task 7 Step 6（整帧） |
| §4.4 静态门禁 | Task 7 Step 1/5（含变异验证） |
| §5 取舍 | 已在 spec 记录，无需代码 |
| §6 不在范围 | 不实现；`docs/bug-list.md:57` 在手工验收里做回归 |
| §7 验收 | Task 7 Step 9 + 手工验收 |

**占位符扫描：** 无「TBD / 稍后补 / 类似 Task N」。所有代码块都是可直接粘贴的完整实现。两处要求实施者**先读代码**（`markdown.rs:740` 的 `Yellow` 语义归类、`cell.rs` 的 `ToolResult` 字段名），这是有意为之 —— 凭色名硬套会写错语义。

**类型一致性：** `TuiTheme` 字段名在 6 个任务里保持一致（`fg` / `fg_muted` / `fg_faint` / `accent` / `warn` / `danger` / `success` / `bg_raised` / `sel_fg` / `sel_bg` / `cursor_fg` / `cursor_bg`）；`TuiThemeKind` 作为缓存 key 的一部分在 Task 5 与 Task 7 一致使用；`ensure_cache` 的 `theme` 参数类型是 `TuiTheme`（需要值）而 `is_current_for` 收 `TuiThemeKind`（只需身份）——这是有意的，避免把整个结构体塞进相等性比较。
