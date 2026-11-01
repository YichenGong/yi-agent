# 终端通知（BEL / OSC 9）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `yi-agent` 在回合完成与等待权限批准时发出终端原生通知（OSC 9 或 BEL），默认只在窗口失焦时触发。

**Architecture:** 新增 `yi-agent/src/tui/notify/` 子模块，分四层：`text.rs` 纯函数做摘要与消毒，`bel.rs`/`osc9.rs` 只做字节编码，`mod.rs` 持后端选择与失败自禁。触发点挂在 `tui/app.rs` 的事件循环里：事件先变成「候选通知」压进 `pending_notification`，等这一帧 `terminal.draw()` 结束后才 flush，保证通知字节永远落在两帧之间。

**Tech Stack:** Rust 2024、crossterm 0.28 的 `Command::write_ansi`、ratatui 0.29、`unicode-segmentation`（新增直接依赖）、`unicode-width`（已有）。

**设计文档:** `docs/superpowers/specs/2026-11-01-terminal-notification-design.md`（所有 `file:line` 引用以此为准）

## Global Constraints

- **平台**: macOS / Linux / Windows 都要能编译通过。`#[cfg(windows)]` 分支必须存在，但其正确性只能靠 CI 编译验证，本任务不做 Windows 实机测试。
- **配置键名固定**：`tui.notifications` / `tui.notification_method` / `tui.notification_condition`。三者在 `settings.json` 里都是 `Option<T>`，`None` 表示「用默认值」，恢复默认等于删键。
- **取值固定**：`notification_method` ∈ {`auto`, `osc9`, `bel`}，默认 `auto`；`notification_condition` ∈ {`unfocused`, `always`}，默认 `unfocused`。
- **OSC 9 白名单固定为五家**：Ghostty、iTerm2、Kitty、Warp、WezTerm。**不得**扩大到 Alacritty / VS Code / Windows Terminal / Apple Terminal —— 它们只走 BEL。
- **通知文案上限 200 字素**，截断后补 ASCII `...` 使可见字素总数恰为 200。
- **所有写进终端字节的通文案必须先经 `sanitize`**，剥掉 C0 / DEL / C1。这是安全约束，不是风格偏好。
- **提交前必须**在 `yi-agent-rs/` 下跑 `cargo fmt --all`。
- **提交信息**：conventional commits，首行 ≤ 72 字符，**不得**含 `Co-Authored-By`。

---

## File Structure

| 文件 | 职责 |
|---|---|
| `yi-agent-rs/crates/yi-agent/src/tui/notify/mod.rs` | 后端枚举、`auto` 选择、焦点条件、失败自禁、`TerminalEnv` 终端识别 |
| `yi-agent-rs/crates/yi-agent/src/tui/notify/text.rs` | `summarize` / `sanitize` / `truncate_graphemes`，纯函数，无 IO |
| `yi-agent-rs/crates/yi-agent/src/tui/notify/bel.rs` | BEL 后端（`\x07`） |
| `yi-agent-rs/crates/yi-agent/src/tui/notify/osc9.rs` | OSC 9 后端，含 tmux / zellij DCS 包裹 |
| `yi-agent-rs/crates/yi-agent/src/tui/history.rs` | 新增 `last_assistant_text`；`permission_summary` 改 `pub(crate)` |
| `yi-agent-rs/crates/yi-agent/src/tui/app.rs` | 事件循环挂触发点、焦点事件、绘制间隙 flush、退出清理 |
| `yi-agent-rs/crates/yi-agent-runtime/src/settings.rs` | `TuiSettings` 三个字段 + `FIELDS` 三条 + 枚举白名单校验 |
| `yi-agent-rs/crates/yi-agent-runtime/src/config.rs` | `RuntimeConfig` 三个字段 + 从 settings 读取 |
| `yi-agent-rs/crates/yi-agent/src/tui/slash.rs` | `TuiConfigSnapshot` 五个新字段 + `/config` 渲染 |
| `yi-agent-rs/crates/yi-agent-web/src/api.rs` | 三个键的读值分支 |
| `yi-agent-rs/crates/yi-agent-app-server/src/settings_rpc.rs` | 三个键的读值分支 |

拆成这样是因为「协议编码」与「何时发送」必须能分开测试：前者断言字节，后者断言状态机。

---

### Task 1: `text.rs` — 摘要与消毒纯函数

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/src/tui/notify/text.rs`
- Create: `yi-agent-rs/crates/yi-agent/src/tui/notify/mod.rs`（本任务先只放 `pub mod text;` 与空骨架，Task 3 填充）
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/mod.rs:3-21`（加 `pub mod notify;`）
- Modify: `yi-agent-rs/crates/yi-agent/Cargo.toml`（加 `unicode-segmentation`）
- Modify: `yi-agent-rs/Cargo.toml`（workspace 依赖加 `unicode-segmentation`）

**Interfaces:**
- Produces:
  - `pub fn sanitize(text: &str) -> String` — 剥 C0/DEL/C1
  - `pub fn truncate_graphemes(text: &str, max: usize) -> String` — 超长时留 `max-3` 字素再补 `...`
  - `pub fn summarize(text: &str) -> String` — 去围栏、去 markdown、折叠空白、截 200；空则 `"yi-agent 回合完成"`

- [x] **Step 1: 加依赖**

在 `yi-agent-rs/Cargo.toml` 的 `[workspace.dependencies]` 里，`unicode-width` 那行附近加：

```toml
unicode-segmentation = "1"
```

在 `yi-agent-rs/crates/yi-agent/Cargo.toml` 的 `[dependencies]` 里，`unicode-width = { workspace = true }` 那行下面加：

```toml
unicode-segmentation = { workspace = true }
```

理由：`unicode-segmentation` v1.13.3 已在 `Cargo.lock`（经 `unicode-truncate` 传递引入），这里只是把它提为直接依赖，不引入新的下载。

- [x] **Step 2: 注册模块**

在 `yi-agent-rs/crates/yi-agent/src/tui/mod.rs` 的 `pub mod markdown;` 之后按字母序插入：

```rust
pub mod notify;
```

- [x] **Step 3: 写失败的测试**

创建 `yi-agent-rs/crates/yi-agent/src/tui/notify/mod.rs`，本步只写最小骨架让模块能编译：

```rust
//! 终端通知：BEL / OSC 9。

pub mod text;
```

创建 `yi-agent-rs/crates/yi-agent/src/tui/notify/text.rs`，只写测试（实现留空会编译失败，这正是我们要的红灯）：

```rust
//! 通知正文的摘要与消毒。纯函数，不碰 IO。

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_all_control_characters() {
        let dirty = "a\u{0}b\u{1b}]0;pwned\u{7}c\u{7f}d\u{9b}e";
        assert_eq!(sanitize(dirty), "ab]0;pwnedcde");
    }

    #[test]
    fn sanitize_keeps_printable_unicode() {
        assert_eq!(sanitize("中文 ok 🎉"), "中文 ok 🎉");
    }

    #[test]
    fn truncate_short_text_is_unchanged() {
        assert_eq!(truncate_graphemes("hello", 200), "hello");
    }

    #[test]
    fn truncate_exactly_at_limit_is_unchanged() {
        let text = "a".repeat(200);
        assert_eq!(truncate_graphemes(&text, 200), text);
    }

    #[test]
    fn truncate_reserves_three_for_ellipsis() {
        let text = "a".repeat(201);
        let out = truncate_graphemes(&text, 200);
        assert_eq!(out.chars().count(), 200);
        assert!(out.ends_with("..."));
        assert_eq!(&out[..197], &"a".repeat(197));
    }

    #[test]
    fn truncate_does_not_split_zwj_emoji() {
        // 家庭 emoji 是单个字素簇，占 1 个而非 5 个 char。
        let out = truncate_graphemes("👨‍👩‍👧👨‍👩‍👧", 3);
        // max=3 时留 0 个字素 + "..."；不得产出半个 ZWJ 序列。
        assert_eq!(out, "...");
        assert!(!out.contains('\u{200d}'));
    }

    #[test]
    fn summarize_drops_code_fences() {
        let md = "```bash\ncargo test\n```\n全部测试通过";
        assert_eq!(summarize(md), "全部测试通过");
    }

    #[test]
    fn summarize_strips_heading_and_list_markers() {
        assert_eq!(summarize("# 结论\n已完成"), "结论 已完成");
        assert_eq!(summarize("- 第一条\n- 第二条"), "第一条 第二条");
    }

    #[test]
    fn summarize_strips_inline_markdown() {
        assert_eq!(summarize("**重点**：见 `main.rs`"), "重点：见 main.rs");
        assert_eq!(summarize("看 [文档](https://example.com)"), "看 文档");
    }

    #[test]
    fn summarize_folds_newlines_into_spaces() {
        // Codex 用 split_whitespace 折叠，不是只取首行。
        assert_eq!(summarize("第一行\n第二行"), "第一行 第二行");
    }

    #[test]
    fn summarize_empty_falls_back_to_fixed_copy() {
        assert_eq!(summarize(""), "yi-agent 回合完成");
        assert_eq!(summarize("   \n\n  "), "yi-agent 回合完成");
        assert_eq!(summarize("```\nonly code\n```"), "yi-agent 回合完成");
    }

    #[test]
    fn summarize_then_sanitize_leaves_no_control_bytes() {
        let out = sanitize(&summarize("\u{1b}]0;pwned\u{7}\n正文"));
        assert!(out.chars().all(|c| {
            let n = c as u32;
            !(n < 0x20 || n == 0x7f || (0x80..=0x9f).contains(&n))
        }));
    }
}
```

- [x] **Step 4: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent --lib tui::notify::text
```

Expected: 编译失败，报 `cannot find function `sanitize` in this scope`。

- [x] **Step 5: 写实现**

把 `text.rs` 的测试模块之前补上实现：

```rust
use unicode_segmentation::UnicodeSegmentation;

/// 通知正文的字素上限（对齐 Codex `AGENT_NOTIFICATION_PREVIEW_GRAPHEMES`）。
pub const MAX_GRAPHEMES: usize = 200;

/// 无正文可摘要时的固定文案。
pub const EMPTY_FALLBACK: &str = "yi-agent 回合完成";

/// 剥掉全部 C0、DEL 与 C1 控制字符。
///
/// 通知字节会被直接写进终端，正文里一个裸 ESC 就能截断 OSC 序列并注入
/// 终端指令，所以这一步是安全边界，不是格式美化。
pub fn sanitize(text: &str) -> String {
    text.chars()
        .filter(|c| {
            let n = *c as u32;
            !(n < 0x20 || n == 0x7f || (0x80..=0x9f).contains(&n))
        })
        .collect()
}

/// 按字素簇截断，超长时保留 `max - 3` 个再补 ASCII `...`。
///
/// 用字素而非 `char`：ZWJ 序列（如家庭 emoji）被 `chars()` 切断后会显示成
/// 方块。`max < 3` 时直接返回前 `max` 个字素，不补省略号。
pub fn truncate_graphemes(text: &str, max: usize) -> String {
    let mut graphemes = text.grapheme_indices(true);
    if graphemes.nth(max).is_none() {
        return text.to_string();
    }
    if max < 3 {
        return text.graphemes(true).take(max).collect();
    }
    let mut out: String = text.graphemes(true).take(max - 3).collect();
    out.push_str("...");
    out
}

/// 把 markdown 正文压成一行通知文案。
///
/// 顺序：去围栏 → 逐行去标记 → 折叠空白 → 截断。空白折叠对齐 Codex
/// （`split_whitespace`），保留多行回复的结论段而不是只取首行。
pub fn summarize(text: &str) -> String {
    let mut kept: Vec<String> = Vec::new();
    let mut in_fence = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || line.is_empty() {
            continue;
        }
        kept.push(strip_markers(line));
    }

    let folded = kept.join(" ");
    let folded = folded.split_whitespace().collect::<Vec<_>>().join(" ");
    if folded.is_empty() {
        return EMPTY_FALLBACK.to_string();
    }
    truncate_graphemes(&folded, MAX_GRAPHEMES)
}

/// 去掉行首的 `#` / `>` / `-` / `*` / `1.`，以及行内的强调与链接标记。
fn strip_markers(line: &str) -> String {
    let mut s = line;

    // 行首：反复剥「引用 + 标题 + 列表」前缀，`> - # x` 这类叠加也要处理。
    loop {
        let trimmed = s.trim_start();
        let stripped = trimmed
            .strip_prefix("> ")
            .or_else(|| trimmed.strip_prefix('#'))
            .or_else(|| trimmed.strip_prefix("- "))
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| strip_ordered_list(trimmed));
        match stripped {
            Some(rest) => s = rest,
            None => break,
        }
    }

    // 行内：链接先展开成文字，再剥强调与行内代码。
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find('[') {
        let Some(mid) = rest[start..].find("](") else { break };
        let Some(end) = rest[start..].find(')') else { break };
        out.push_str(&rest[..start]);
        out.push_str(&rest[start + 1..start + mid]);
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);

    out.replace("**", "").replace('`', "")
}

/// `1. foo` / `12. foo` → `foo`。
fn strip_ordered_list(line: &str) -> Option<&str> {
    let digits_end = line.find(|c: char| !c.is_ascii_digit())?;
    if digits_end == 0 {
        return None;
    }
    line[digits_end..].strip_prefix(". ")
}
```

- [x] **Step 6: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent --lib tui::notify::text
```

Expected: PASS（11 个测试）。

- [x] **Step 7: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A && git commit -m "feat(tui): 通知正文摘要与消毒纯函数"
```

---

### Task 2: `bel.rs` / `osc9.rs` — 协议字节编码

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/src/tui/notify/bel.rs`
- Create: `yi-agent-rs/crates/yi-agent/src/tui/notify/osc9.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/notify/mod.rs`（加两个 `pub mod`）

**Interfaces:**
- Consumes: 无
- Produces:
  - `bel::BEL_SEQUENCE: &str` = `"\x07"`
  - `osc9::encode(message: &str, dcs_passthrough: bool) -> String` — 纯函数，返回完整字节串
  - `osc9::Osc9Backend::new(dcs_passthrough: bool)` / `notify(&mut self, message: &str) -> io::Result<()>`
  - `bel::BelBackend::new()` / `notify(&mut self, _message: &str) -> io::Result<()>`

- [x] **Step 1: 写失败的测试**

创建 `yi-agent-rs/crates/yi-agent/src/tui/notify/osc9.rs`，只写测试：

```rust
//! OSC 9 通知后端（含 tmux / zellij DCS 透传）。

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_encodes_osc9() {
        assert_eq!(encode("hello", false), "\u{1b}]9;hello\u{7}");
    }

    #[test]
    fn tmux_wraps_in_dcs_passthrough() {
        assert_eq!(
            encode("done", true),
            "\u{1b}Ptmux;\u{1b}\u{1b}]9;done\u{7}\u{1b}\\"
        );
    }

    #[test]
    fn tmux_doubles_esc_inside_payload() {
        assert_eq!(
            encode("danger\u{1b}[31m", true),
            "\u{1b}Ptmux;\u{1b}\u{1b}]9;danger\u{1b}\u{1b}[31m\u{7}\u{1b}\\"
        );
    }

    #[test]
    fn plain_path_never_emits_raw_esc_from_message() {
        // Codex 的 bug：非 tmux 路径不翻倍 ESC。我们靠调用方先 sanitize，
        // 这里断言 encode 本身对已消毒输入不再引入 ESC。
        let sanitized = crate::tui::notify::text::sanitize("a\u{1b}]0;x\u{7}");
        let out = encode(&sanitized, false);
        assert_eq!(out.matches('\u{1b}').count(), 1, "out: {out:?}");
    }
}
```

创建 `yi-agent-rs/crates/yi-agent/src/tui/notify/bel.rs`，只写测试：

```rust
//! BEL 通知后端。

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bel_is_a_single_byte() {
        assert_eq!(BEL_SEQUENCE, "\u{7}");
    }
}
```

在 `mod.rs` 里补：

```rust
pub mod bel;
pub mod osc9;
```

- [x] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent --lib tui::notify
```

Expected: 编译失败，报 `cannot find function `encode`` 与 `cannot find value `BEL_SEQUENCE``。

- [x] **Step 3: 写实现**

`osc9.rs` 在测试模块之前补：

```rust
use std::io::{self, stdout};

use crossterm::Command;

/// 把通知文案编码为 OSC 9 序列。
///
/// `dcs_passthrough` 为真时（tmux / zellij 内）整段包进
/// `ESC P tmux; ... ESC \`，并把正文里的 ESC 翻倍——多路复用器会剥掉一层，
/// 不翻倍就会被 tmux 当成自己的控制序列吃掉。
pub fn encode(message: &str, dcs_passthrough: bool) -> String {
    if dcs_passthrough {
        let escaped = message.replace('\u{1b}', "\u{1b}\u{1b}");
        format!("\u{1b}Ptmux;\u{1b}\u{1b}]9;{escaped}\u{7}\u{1b}\\")
    } else {
        format!("\u{1b}]9;{message}\u{7}")
    }
}

/// 通过 crossterm 的 `Command` 写 stdout，测试可断言字节。
#[derive(Debug, Clone)]
pub struct PostNotification {
    pub message: String,
    pub dcs_passthrough: bool,
}

impl Command for PostNotification {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str(&encode(&self.message, self.dcs_passthrough))
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "tried to execute PostNotification using WinAPI; use ANSI instead",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

/// OSC 9 后端。`dcs_passthrough` 在构造时定死，会话内不变。
#[derive(Debug, Clone)]
pub struct Osc9Backend {
    dcs_passthrough: bool,
}

impl Osc9Backend {
    pub fn new(dcs_passthrough: bool) -> Self {
        Self { dcs_passthrough }
    }

    pub fn notify(&mut self, message: &str) -> io::Result<()> {
        crossterm::execute!(
            stdout(),
            PostNotification {
                message: message.to_string(),
                dcs_passthrough: self.dcs_passthrough,
            }
        )
    }
}
```

`bel.rs` 在测试模块之前补：

```rust
use std::io::{self, stdout};

use crossterm::Command;

/// BEL 是单字节，不带文案——终端的 BEL 通知无法承载正文。
pub const BEL_SEQUENCE: &str = "\u{7}";

#[derive(Debug, Clone)]
pub struct PostNotification;

impl Command for PostNotification {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str(BEL_SEQUENCE)
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "tried to execute PostNotification using WinAPI; use ANSI instead",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[derive(Debug, Default, Clone)]
pub struct BelBackend;

impl BelBackend {
    pub fn new() -> Self {
        Self
    }

    /// 忽略文案：BEL 协议本身不携带消息体。签名与 `Osc9Backend::notify`
    /// 保持一致，好让 `mod.rs` 用同一处代码分派。
    pub fn notify(&mut self, _message: &str) -> io::Result<()> {
        crossterm::execute!(stdout(), PostNotification)
    }
}
```

- [x] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent --lib tui::notify
```

Expected: PASS（5 个测试）。

- [x] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A && git commit -m "feat(tui): BEL 与 OSC 9 协议字节编码"
```

---

### Task 3: `mod.rs` — 终端识别、后端选择、失败自禁

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/notify/mod.rs`（在本任务内完整实现）

**Interfaces:**
- Consumes: `text::sanitize`、`bel::BelBackend`、`osc9::Osc9Backend`
- Produces:
  - `pub enum Method { Auto, Osc9, Bel }` + `Method::parse(&str) -> Option<Method>` + `Method::as_str(&self) -> &'static str`
  - `pub enum Condition { Unfocused, Always }` + 同上的 `parse` / `as_str`
  - `pub struct TerminalEnv(BTreeMap<String, String>)` + `from_process()` / `from_pairs(..)`
  - `pub fn detect_osc9_supported(env: &TerminalEnv) -> bool`
  - `pub fn is_multiplexed(env: &TerminalEnv) -> bool`
  - `pub struct Notifier { backend: Option<Backend>, condition: Condition }` + `Notifier::new(method, condition, env)` / `notify(&mut self, message: &str)` / `is_disabled(&self) -> bool`

- [x] **Step 1: 写失败的测试**

在 `mod.rs` 末尾追加测试模块（此时实现还不存在，先看红）：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> TerminalEnv {
        TerminalEnv::from_pairs(pairs.iter().copied())
    }

    #[test]
    fn whitelist_terminals_support_osc9() {
        for program in ["iTerm.app", "iTerm2", "ghostty", "Ghostty", "kitty", "Warp", "WarpTerminal", "WezTerm"] {
            assert!(
                detect_osc9_supported(&env(&[("TERM_PROGRAM", program)])),
                "{program} should support OSC 9"
            );
        }
    }

    #[test]
    fn non_whitelist_terminals_use_bel() {
        for program in ["Apple_Terminal", "alacritty", "vscode", "Hyper", "unknown-term"] {
            assert!(
                !detect_osc9_supported(&env(&[("TERM_PROGRAM", program)])),
                "{program} should not support OSC 9"
            );
        }
    }

    #[test]
    fn warp_normalization_removes_separators() {
        assert!(detect_osc9_supported(&env(&[("TERM_PROGRAM", "warp-terminal")])));
        assert!(detect_osc9_supported(&env(&[("TERM_PROGRAM", "warp_terminal")])));
    }

    #[test]
    fn fallback_probes_work_without_term_program() {
        assert!(detect_osc9_supported(&env(&[("GHOSTTY_RESOURCES_DIR", "/x")])));
        assert!(detect_osc9_supported(&env(&[("WEZTERM_VERSION", "20240101")])));
        assert!(detect_osc9_supported(&env(&[("ITERM_SESSION_ID", "w0t0p0")])));
        assert!(detect_osc9_supported(&env(&[("ITERM_PROFILE", "Default")])));
        assert!(detect_osc9_supported(&env(&[("KITTY_WINDOW_ID", "1")])));
        assert!(detect_osc9_supported(&env(&[("TERM", "xterm-kitty")])));
    }

    #[test]
    fn term_program_tmux_is_skipped_in_favour_of_probes() {
        // tmux 自身不是终端名，不能把 tmux 当成 WhiteTerminal。
        let inside_tmux_kitty = env(&[
            ("TERM_PROGRAM", "tmux"),
            ("KITTY_WINDOW_ID", "3"),
            ("TMUX", "/tmp/tmux-501/default,1,0"),
        ]);
        assert!(detect_osc9_supported(&inside_tmux_kitty));
        assert!(is_multiplexed(&inside_tmux_kitty));
    }

    #[test]
    fn alacritty_and_wt_are_recognized_but_not_whitelisted() {
        assert!(!detect_osc9_supported(&env(&[("ALACRITTY_SOCKET", "/tmp/a")])));
        assert!(!detect_osc9_supported(&env(&[("TERM", "alacritty")])));
        assert!(!detect_osc9_supported(&env(&[("WT_SESSION", "guid")])));
    }

    #[test]
    fn multiplexer_detection_requires_non_empty() {
        assert!(is_multiplexed(&env(&[("TMUX", "/tmp/x,1,0")])));
        assert!(is_multiplexed(&env(&[("TMUX_PANE", "%0")])));
        assert!(is_multiplexed(&env(&[("ZELLIJ", "0")])));
        assert!(is_multiplexed(&env(&[("ZELLIJ_SESSION_NAME", "s")])));
        // 空串是 shell 残留，不算。
        assert!(!is_multiplexed(&env(&[("TMUX", "")])));
        assert!(!is_multiplexed(&env(&[("ZELLIJ", "")])));
    }

    #[test]
    fn method_parse_accepts_three_values() {
        assert_eq!(Method::parse("auto"), Some(Method::Auto));
        assert_eq!(Method::parse("osc9"), Some(Method::Osc9));
        assert_eq!(Method::parse("bel"), Some(Method::Bel));
        assert_eq!(Method::parse("OSC9"), Some(Method::Osc9));
        assert_eq!(Method::parse("nope"), None);
    }

    #[test]
    fn condition_parse_accepts_two_values() {
        assert_eq!(Condition::parse("unfocused"), Some(Condition::Unfocused));
        assert_eq!(Condition::parse("always"), Some(Condition::Always));
        assert_eq!(Condition::parse(""), None);
    }

    #[test]
    fn explicit_bel_overrides_whitelist() {
        let notifier = Notifier::new(Method::Bel, Condition::Always, env(&[("TERM_PROGRAM", "WezTerm")]));
        assert!(!notifier.is_disabled());
        assert!(!notifier.uses_osc9());
    }

    #[test]
    fn auto_picks_osc9_for_wezterm() {
        let notifier = Notifier::new(Method::Auto, Condition::Always, env(&[("TERM_PROGRAM", "WezTerm")]));
        assert!(notifier.uses_osc9());
    }
}
```

- [x] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent --lib tui::notify::tests
```

Expected: 编译失败，报 `cannot find type TerminalEnv`。

- [x] **Step 3: 写实现**

把 `mod.rs` 现有内容整体替换为：

```rust
//! 终端通知：BEL / OSC 9。
//!
//! 分三层：`text` 做摘要与消毒（纯函数）、`bel`/`osc9` 做字节编码、
//! 本模块持有策略（后端选择、焦点条件、失败自禁）。

pub mod bel;
pub mod osc9;
pub mod text;

use std::collections::BTreeMap;
use std::io::IsTerminal;

use bel::BelBackend;
use osc9::Osc9Backend;

/// 通知协议。`Auto` 在构造时解析成具体后端。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Auto,
    Osc9,
    Bel,
}

impl Method {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "osc9" => Some(Self::Osc9),
            "bel" => Some(Self::Bel),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Osc9 => "osc9",
            Self::Bel => "bel",
        }
    }
}

/// 何时发送。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Condition {
    /// 只在窗口失焦时发。
    Unfocused,
    /// 总是发。
    Always,
}

impl Condition {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "unfocused" => Some(Self::Unfocused),
            "always" => Some(Self::Always),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unfocused => "unfocused",
            Self::Always => "always",
        }
    }
}

/// 注入式环境，测试传 `HashMap` 即可，不碰进程环境。
///
/// 用 `std::env::set_var` 写测试会污染并行测试，且 Rust 2024 里它已是
/// `unsafe`。注入式环境让全部识别逻辑可以在无终端环境下断言。
#[derive(Debug, Clone, Default)]
pub struct TerminalEnv(BTreeMap<String, String>);

impl TerminalEnv {
    pub fn from_process() -> Self {
        Self(std::env::vars().collect())
    }

    pub fn from_pairs(pairs: impl IntoIterator<Item = (&'static str, &'static str)>) -> Self {
        Self(
            pairs
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    fn var(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    fn non_empty(&self, name: &str) -> Option<&str> {
        self.var(name).filter(|v| !v.trim().is_empty())
    }

    fn has_non_empty(&self, name: &str) -> bool {
        self.non_empty(name).is_some()
    }

    fn has(&self, name: &str) -> bool {
        self.var(name).is_some()
    }
}

/// 归一化 `TERM_PROGRAM`：trim 后剔除空格、`-`、`_`、`.`，转小写。
fn normalize_term_program(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '_' | '.'))
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// 终端是否在白名单里（Ghostty / iTerm2 / Kitty / Warp / WezTerm）。
///
/// 顺序照 Codex `detect_terminal_identity_from_env`。不在白名单里的一律
/// 走 BEL，包括 Alacritty 与 Windows Terminal——它们是否弹系统通知未经
/// 实测，不猜。
pub fn detect_osc9_supported(env: &TerminalEnv) -> bool {
    if let Some(program) = env.non_empty("TERM_PROGRAM") {
        if !program.eq_ignore_ascii_case("tmux") {
            return matches!(
                normalize_term_program(program).as_str(),
                "iterm" | "iterm2" | "itermapp" | "ghostty" | "kitty" | "warp" | "warpterminal"
                    | "wezterm"
            );
        }
    }

    env.has_non_empty("GHOSTTY_RESOURCES_DIR")
        || env.has("WEZTERM_VERSION")
        || env.has("ITERM_SESSION_ID")
        || env.has("ITERM_PROFILE")
        || env.has("ITERM_PROFILE_NAME")
        || env.has("KITTY_WINDOW_ID")
        || env.var("TERM").is_some_and(|t| t.contains("kitty"))
}

/// 是否处在多路复用器里（tmux 或 zellij）。
///
/// 判「非空」而非「存在」：`TMUX=` 空串在部分 shell 里是残留，按存在判会
/// 误把普通终端当成 tmux 环境，导致 OSC 9 被套上一层无用的 DCS 包裹。
pub fn is_multiplexed(env: &TerminalEnv) -> bool {
    env.has_non_empty("TMUX")
        || env.has_non_empty("TMUX_PANE")
        || env.has_non_empty("ZELLIJ")
        || env.has_non_empty("ZELLIJ_SESSION_NAME")
        || env.has_non_empty("ZELLIJ_VERSION")
}

#[derive(Debug)]
enum Backend {
    Osc9(Osc9Backend),
    Bel(BelBackend),
}

/// 通知器。持有后端与条件，`notify` 失败后自禁。
pub struct Notifier {
    /// `None` 表示已失效（写失败一次后不再尝试）。
    backend: Option<Backend>,
    condition: Condition,
}

impl Notifier {
    pub fn new(method: Method, condition: Condition, env: &TerminalEnv) -> Self {
        let resolved = match method {
            Method::Auto => {
                if detect_osc9_supported(env) {
                    Method::Osc9
                } else {
                    Method::Bel
                }
            }
            other => other,
        };
        let backend = match resolved {
            Method::Osc9 => Backend::Osc9(Osc9Backend::new(is_multiplexed(env))),
            _ => Backend::Bel(BelBackend::new()),
        };
        Self {
            backend: Some(backend),
            condition,
        }
    }

    pub fn condition(&self) -> Condition {
        self.condition
    }

    /// 当前生效的后端名，供 `/config` 展示。失效后返回 `"disabled"`。
    pub fn effective_backend(&self) -> &'static str {
        match &self.backend {
            Some(Backend::Osc9(_)) => "osc9",
            Some(Backend::Bel(_)) => "bel",
            None => "disabled",
        }
    }

    pub fn is_disabled(&self) -> bool {
        self.backend.is_none()
    }

    #[cfg(test)]
    pub fn uses_osc9(&self) -> bool {
        matches!(self.backend, Some(Backend::Osc9(_)))
    }

    /// 发一条通知。
    ///
    /// 三个短路条件：后端已失效、stdout 不是 TTY、文案消毒后为空。写失败
    /// 时置 `backend = None`——终端已不可用，继续每回合重试只会刷日志。
    pub fn notify(&mut self, message: &str) -> std::io::Result<()> {
        let Some(backend) = self.backend.as_mut() else {
            return Ok(());
        };
        if !std::io::stdout().is_terminal() {
            return Ok(());
        }
        let payload = text::sanitize(message);
        let result = match backend {
            Backend::Osc9(b) => b.notify(&payload),
            Backend::Bel(b) => b.notify(&payload),
        };
        if let Err(error) = &result {
            tracing::warn!(error = %error, "terminal notification failed; disabling for this session");
            self.backend = None;
        }
        result
    }
}
```

- [x] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent --lib tui::notify
```

Expected: PASS（全部 notify 测试）。若 `uses_osc9` 在非 test 构建下报 dead_code，说明它被错误地加上了 `#[cfg(test)]`——保留该属性即可，它只在测试里用。

- [x] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A && git commit -m "feat(tui): 终端识别与通知后端选择"
```

---

### Task 4: `history.rs` — `last_assistant_text`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/history.rs`（加方法；`permission_summary` 改可见性）
- Test: 同文件内 `#[cfg(test)] mod tests`

**Interfaces:**
- Produces:
  - `HistoryState::last_assistant_text(&self) -> Option<&str>`
  - `pub(crate) fn permission_summary(tool_name: &str, input: &serde_json::Value) -> String`（仅改可见性，行为不变）

- [x] **Step 1: 写失败的测试**

在 `history.rs` 的测试模块里追加：

```rust
#[test]
fn last_assistant_text_returns_trailing_message() {
    let mut s = HistoryState::new();
    s.push_event(AgentEvent::AssistantText("第一段".into()), 80);
    assert_eq!(s.last_assistant_text(), Some("第一段"));
}

#[test]
fn last_assistant_text_skips_trailing_separator() {
    let mut s = HistoryState::new();
    s.push_event(AgentEvent::AssistantText("结论".into()), 80);
    s.push_event(AgentEvent::Done { reason: DoneReason::EndTurn }, 80);
    // push_event 会把 Separator 压到末尾，仍要能找到 assistant 正文。
    assert_eq!(s.last_assistant_text(), Some("结论"));
}

#[test]
fn last_assistant_text_is_none_when_no_message() {
    let mut s = HistoryState::new();
    assert_eq!(s.last_assistant_text(), None);

    let mut only_tools = HistoryState::new();
    only_tools.push_event(AgentEvent::Done { reason: DoneReason::MaxTurns }, 80);
    assert_eq!(only_tools.last_assistant_text(), None);
}

#[test]
fn last_assistant_text_ignores_reasoning_blocks() {
    let mut s = HistoryState::new();
    s.push_event(
        AgentEvent::ReasoningDelta {
            text: "思考".into(),
            kind: ReasoningKind::Summary,
        },
        80,
    );
    assert_eq!(s.last_assistant_text(), None);
}
```

**注意事件名**：`AgentEvent` 里没有 `AssistantMessage`。流式正文事件是 `AgentEvent::AssistantText(String)`（`yi-agent-core/src/agent.rs:269`），而 `history.rs` 在 `push_event` 内部把它累积进 `HistoryCell::AssistantMessage { markdown }`。`ReasoningKind` 有两个变体：`Content` / `Summary`（`provider.rs:92-97`），上面用 `Summary`。

- [x] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent --lib tui::history::tests::last_assistant_text
```

Expected: 编译失败，报 `no method named `last_assistant_text``。

- [x] **Step 3: 写实现**

在 `impl HistoryState` 块内加（放在 `push_event` 附近）：

```rust
    /// 最后一条助手正文的原文。
    ///
    /// 回合结束通知需要它：`AgentEvent::Done` 只带 `reason`、不带正文，
    /// 正文在流式阶段已写进 `AssistantMessage` cell。从尾部反向找，是因为
    /// `push_event` 收到 `Done` 会在末尾压一条 `Separator`。
    ///
    /// 跳过 `Reasoning`：思维链是显示专用的，不是回复内容，不能出现在通知里。
    pub fn last_assistant_text(&self) -> Option<&str> {
        self.cells.iter().rev().find_map(|cell| match cell {
            HistoryCell::AssistantMessage { markdown } => Some(markdown.as_str()),
            _ => None,
        })
    }
```

把 `permission_summary` 的 `fn` 改为 `pub(crate) fn`（`history.rs:119`）。

- [x] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent --lib tui::history
```

Expected: PASS，且原有 `permission_summary_*` 三条测试仍绿。

- [x] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A && git commit -m "feat(tui): last_assistant_text 供回合通知取摘要"
```

---

### Task 5: 配置层 — `settings.rs` 三个字段

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/settings.rs`（`Settings` 加 `tui` 组、`FIELDS` 加三条、`normalized` 加枚举校验）
- Test: 同文件 `#[cfg(test)] mod tests`

**Interfaces:**
- Produces:
  - `pub struct TuiSettings { pub notifications: Option<bool>, pub notification_method: Option<String>, pub notification_condition: Option<String> }`
  - `Settings.tui: TuiSettings`
  - `FIELDS` 新增 `tui.notifications` / `tui.notification_method` / `tui.notification_condition`

- [x] **Step 1: 写失败的测试**

在 `settings.rs` 测试模块追加：

```rust
#[test]
fn tui_defaults_are_all_none() {
    let settings = Settings::default();
    assert!(settings.tui.notifications.is_none());
    assert!(settings.tui.notification_method.is_none());
    assert!(settings.tui.notification_condition.is_none());
}

#[test]
fn tui_fields_are_registered() {
    for key in [
        "tui.notifications",
        "tui.notification_method",
        "tui.notification_condition",
    ] {
        assert!(
            find_spec(key).is_some(),
            "{key} must be in FIELDS or the settings UI will not show it"
        );
    }
}

#[test]
fn invalid_notification_method_is_dropped_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(
        &path,
        r#"{"tui":{"notification_method":"carrier-pigeon","notifications":"false"},
            "agent":{"max_turns":42}}"#,
    )
    .unwrap();

    let settings = load_from(&path);
    // 非法值被清成 None（回退默认），而不是让整份文件失败。
    assert_eq!(settings.tui.notification_method, None);
    // 同一份文件里的其它字段必须还在。
    assert_eq!(settings.tui.notifications.as_deref(), Some("false"));
    assert_eq!(settings.agent.max_turns, Some(42));
}

#[test]
fn invalid_notification_condition_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, r#"{"tui":{"notification_condition":"sometimes"}}"#).unwrap();
    assert_eq!(load_from(&path).tui.notification_condition, None);
}

#[test]
fn valid_notification_values_survive() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(
        &path,
        r#"{"tui":{"notifications":"true","notification_method":"osc9","notification_condition":"always"}}"#,
    )
    .unwrap();
    let settings = load_from(&path);
    assert_eq!(settings.tui.notifications.as_deref(), Some("true"));
    assert_eq!(settings.tui.notification_method.as_deref(), Some("osc9"));
    assert_eq!(settings.tui.notification_condition.as_deref(), Some("always"));
}
```

`tempfile` 已是 `yi-agent-runtime` 的 dev-dependency（`Cargo.toml:26`），无需新增。

- [x] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-runtime --lib settings::tests::tui
```

Expected: 编译失败，报 `no field `tui` on type `Settings``。

- [x] **Step 3: 写实现**

在 `Settings` 结构体里加字段：

```rust
    pub tui: TuiSettings,
```

在 `StartupSettings` 之后新增结构体：

```rust
/// 终端 TUI 的行为设置。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiSettings {
    /// `true` / `false`；`None` = 默认 `true`。
    ///
    /// **为什么是字符串而不是 `Option<bool>`**：`FIELDS` 的三种控件
    /// （`Number`/`Text`/`Select`）里，`Text` 与 `Select` 都只接受 JSON
    /// 字符串（`validate_value`，`settings.rs:525-588`）。用 `Option<bool>`
    /// 会导致设置页写进 `"true"` 后反序列化读不回来。存字符串与
    /// `startup.sandbox` 完全同构，也让非法值能按字段 warn 回退。
    #[serde(default, deserialize_with = "de_optional_string")]
    pub notifications: Option<String>,
    /// `auto` / `osc9` / `bel`；`None` = `auto`。
    ///
    /// 存字符串而非枚举：非法值要能被加载侧识别并 warn 回退，而不是让整份
    /// JSON 反序列化失败，连带丢掉用户的其它字段。
    #[serde(default, deserialize_with = "de_optional_string")]
    pub notification_method: Option<String>,
    /// `unfocused` / `always`；`None` = `unfocused`。
    #[serde(default, deserialize_with = "de_optional_string")]
    pub notification_condition: Option<String>,
}
```

三个字段全部复用既有的 `de_optional_string`（`settings.rs:287`），**不需要**新增反序列化函数。

在 `FIELDS` 数组末尾（`startup.sandbox` 之后）追加三条：

```rust
    FieldSpec {
        key: "tui.notifications",
        group: "tui",
        label: "终端通知",
        field_type: FieldType::Select,
        default_display: "true",
        description: "回合完成或等待批准时发送终端通知；空表示用默认（开）",
        options: &["true", "false"],
        min: None,
        max: None,
    },
    FieldSpec {
        key: "tui.notification_method",
        group: "tui",
        label: "通知协议",
        field_type: FieldType::Select,
        default_display: "auto",
        description: "auto 按终端自动选择；osc9 带正文，bel 只响铃",
        options: &["auto", "osc9", "bel"],
        min: None,
        max: None,
    },
    FieldSpec {
        key: "tui.notification_condition",
        group: "tui",
        label: "通知触发条件",
        field_type: FieldType::Select,
        default_display: "unfocused",
        description: "unfocused 只在窗口失焦时发；always 总是发",
        options: &["unfocused", "always"],
        min: None,
        max: None,
    },
```

`tui.notifications` 用 `FieldType::Select` + `["true", "false"]`。三个类型都不完美贴合布尔：`Number` 会走数值边界校验（`min`/`max` 对布尔无意义），`Text` 是自由输入框（用户能填 `maybe`），只有 `Select` 能把取值限死在两选一，并在 `validate_value` 里靠 `spec.options.contains` 拒绝非法值（`settings.rs:571-585`）。

**这条让 `every_field_in_fields_can_be_read_back` 测试自动通过**：该测试给 `Select` 写 `spec.options[0]`（`settings_rpc.rs:115-133`），对这三个键分别是 `"true"`、`"auto"`、`"unfocused"`——都合法，读回非 null。

在 `Settings::normalized` 里，`startup.sandbox` 校验之后追加：

```rust
        // 枚举白名单：非法值只清这一个字段，不影响同文件其它键。
        if let Some(enabled) = self.tui.notifications.as_deref() {
            if !matches!(enabled.to_ascii_lowercase().as_str(), "true" | "false") {
                tracing::warn!(
                    value = enabled,
                    "settings.json has an unknown tui.notifications; falling back to the default"
                );
                self.tui.notifications = None;
            }
        }
        if let Some(method) = self.tui.notification_method.as_deref() {
            if !matches!(
                method.to_ascii_lowercase().as_str(),
                "auto" | "osc9" | "bel"
            ) {
                tracing::warn!(
                    value = method,
                    "settings.json has an unknown tui.notification_method; falling back to the default"
                );
                self.tui.notification_method = None;
            }
        }
        if let Some(condition) = self.tui.notification_condition.as_deref() {
            if !matches!(
                condition.to_ascii_lowercase().as_str(),
                "unfocused" | "always"
            ) {
                tracing::warn!(
                    value = condition,
                    "settings.json has an unknown tui.notification_condition; falling back to the default"
                );
                self.tui.notification_condition = None;
            }
        }
```

- [x] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-runtime --lib settings
```

Expected: PASS，包括既有的 `FIELDS` 唯一性与默认值护栏测试。

- [x] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A && git commit -m "feat(settings): 新增 tui 通知三项配置"
```

---

### Task 6: 设置读写通路 — web / RPC / migrate

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-web/src/api.rs:78-91`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/settings_rpc.rs:44-64`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/migrate.rs:508` 附近与 `:356` 附近
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs`

**Interfaces:**
- Consumes: `Settings.tui`（Task 5）
- Produces: 三个键在 settings RPC 与 web API 上可读可写

- [x] **Step 1: 写失败的测试**

在 `rpc_tests.rs` 里，紧邻既有的 `startup.sandbox` 断言（约 `:16265`）加一条：

```rust
    #[test]
    fn every_tui_notification_field_is_writable_and_readable() {
        // FIELDS 驱动 UI，current_value 决定读出的值。漏一个就会静默返回
        // null：设置页显示空白，用户改完像没生效。
        for (key, value) in [
            ("tui.notifications", serde_json::json!("false")),
            ("tui.notification_method", serde_json::json!("bel")),
            ("tui.notification_condition", serde_json::json!("always")),
        ] {
            let patch = serde_json::json!({ "patch": { key: value } });
            let response = call_settings_write(&patch);
            assert!(response.get("error").is_none(), "write {key} failed: {response}");

            let listed = call_settings_list();
            let field = listed["fields"]
                .as_array()
                .unwrap()
                .iter()
                .find(|f| f["key"] == key)
                .unwrap_or_else(|| panic!("{key} must be listed"));
            assert!(
                !field["value"].is_null(),
                "{key} must read back non-null, got {field}"
            );
        }
    }
```

上方 `call_settings_write` / `call_settings_list` 用该文件里既有的同类辅助函数替换（`rpc_tests.rs` 已有调用 `settings/write` 与 `settings/list` 的现成写法，照它改）。

- [x] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib every_tui_notification_field
```

Expected: FAIL，`tui.notifications must be listed` 或读回 `null`。

- [x] **Step 3: 写实现**

`yi-agent-app-server/src/settings_rpc.rs` 的 `current_value`，在 `"startup.sandbox"` 分支后加：

```rust
        "tui.notifications" => option_to_json(current.tui.notifications.as_deref()),
        "tui.notification_method" => option_to_json(current.tui.notification_method.as_deref()),
        "tui.notification_condition" => {
            option_to_json(current.tui.notification_condition.as_deref())
        }
```

`yi-agent-web/src/api.rs` 的同一个 match 里加：

```rust
        "tui.notifications" => opt(current.tui.notifications.clone().map(Value::from)),
        "tui.notification_method" => opt(current.tui.notification_method.clone().map(Value::from)),
        "tui.notification_condition" => {
            opt(current.tui.notification_condition.clone().map(Value::from))
        }
```

`yi-agent-runtime/src/migrate.rs` 里有两处需要补（`:356` 附近的 env 映射表、`:508` 附近的 has-key 判定）。按 `startup.sandbox` 的同款写法加三个键；`tui.*` 没有对应环境变量，env 映射表**不加**，只加 `:508` 那侧的判定：

```rust
        "tui.notifications" => settings.tui.notifications.is_some(),
        "tui.notification_method" => settings.tui.notification_method.is_some(),
        "tui.notification_condition" => settings.tui.notification_condition.is_some(),
```

- [x] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib settings && cargo test -p yi-agent-web --lib
```

Expected: PASS。

- [x] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A && git commit -m "feat(settings): 通知三项接入 web 与 RPC 通路"
```

---

### Task 7: `RuntimeConfig` 与 `TuiConfigSnapshot`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/config.rs`（`RuntimeConfig` 加三字段并赋值）
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/slash.rs:281-296`（snapshot 加字段）
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:2070-2082`（填值）
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:220`（`snapshot_for_tests` 补齐）

**Interfaces:**
- Consumes: `Settings.tui`（Task 5）
- Produces:
  - `RuntimeConfig.notifications: bool`、`.notification_method: String`、`.notification_condition: String`
  - `TuiConfigSnapshot.notifications: bool`、`.notification_method: String`、`.notification_condition: String`、`.effective_backend: String`（**四个**字段；焦点降级不进快照，见 Task 8）

- [x] **Step 1: 加 `RuntimeConfig` 字段**

在 `RuntimeConfig` 里 `model_injected` 之后加：

```rust
    /// 是否发送终端通知（`tui.notifications`，默认 true）。
    pub notifications: bool,
    /// `auto` / `osc9` / `bel`（`tui.notification_method`，默认 `auto`）。
    pub notification_method: String,
    /// `unfocused` / `always`（`tui.notification_condition`，默认 `unfocused`）。
    pub notification_condition: String,
```

在 `load_inner` 里，`settings.agent.skills_catalog_budget` 那行（`config.rs:408`）附近加：

```rust
        // 存储侧是字符串（见 Task 5 的类型说明），这里解析成 bool；
        // 空串与非法值已在 `normalized` 里清成 None，故只需处理默认。
        let notifications = match settings.tui.notifications.as_deref() {
            Some("false") => false,
            _ => true,
        };
        let notification_method = settings
            .tui
            .notification_method
            .clone()
            .unwrap_or_else(|| "auto".to_string());
        let notification_condition = settings
            .tui
            .notification_condition
            .clone()
            .unwrap_or_else(|| "unfocused".to_string());
```

再在 `Ok(Self { ... })` 里对应处填入这三个变量。

**注意**：`RuntimeConfig` 若有 `Default` 实现或其它构造点（`grep -rn "RuntimeConfig {" crates/`），必须逐个补齐，否则编译失败。这是好事——编译器会指出所有构造点。

- [x] **Step 2: 加 snapshot 字段**

`TuiConfigSnapshot` 加：

```rust
    /// 是否发送终端通知。
    pub notifications: bool,
    /// 配置里的协议值（`auto` / `osc9` / `bel`）。
    pub notification_method: String,
    /// 配置里的触发条件（`unfocused` / `always`）。
    pub notification_condition: String,
    /// `auto` 解析后的实际后端。
    pub effective_backend: String,
```

**不要把 `focus_degraded` 放进快照**：它由 `run_tui` 里的 `EnableFocusChange` 结果决定，而快照在 `main.rs` 就已构造完并以不可变引用传进 `run_loop`。放进去只能恒为 `false`，是死字段。改用 Task 8 的 `run_loop` 入参。

`main.rs` 的构造处（`:2070`）加。**解析后的后端必须在这里算好**，因为 `TuiConfigSnapshot` 以 `&TuiConfigSnapshot` 传给 `run_loop`，循环内无法回填：

```rust
            notifications: config.notifications,
            notification_method: config.notification_method.clone(),
            notification_condition: config.notification_condition.clone(),
            effective_backend: {
                let method = crate::tui::notify::Method::parse(&config.notification_method)
                    .unwrap_or(crate::tui::notify::Method::Auto);
                let notifier = crate::tui::notify::Notifier::new(
                    method,
                    crate::tui::notify::Condition::Unfocused,
                    &crate::tui::notify::TerminalEnv::from_process(),
                );
                notifier.effective_backend().to_string()
            },
```

`app.rs` 的 `snapshot_for_tests`（`:220`）同样补**四个**字段，测试用途下取 `true` / `"auto"` / `"unfocused"` / `"bel"`。

**`focus_degraded` 走另一条路**：它取决于 `execute!(stdout, EnableFocusChange)` 的返回值，那发生在 `run_tui` 里、`main.rs` 之后。因此它作为**值参**传给 `run_loop`（Task 8），在 `/config` 渲染后追加一行提示。`TuiConfigSnapshot.render` 保持纯函数。

- [x] **Step 3: 在 `/config` 里展示**

`slash.rs` 的 `TuiConfigSnapshot::render` 里，照 `startup.sandbox` 那行的样式加一行：

```rust
        lines.push(format!(
            "通知          : {}（协议 {}，{}）",
            if self.notifications { "开" } else { "关" },
            if self.effective_backend.is_empty() {
                self.notification_method.as_str()
            } else {
                self.effective_backend.as_str()
            },
            self.notification_condition
        ));
```

具体 `lines.push` 形式与既有代码保持一致；若有宽度对齐辅助函数，用之。

- [x] **Step 4: 编译并跑快照测试**

```bash
cd yi-agent-rs && cargo test -p yi-agent --lib tui::slash
```

Expected: PASS。若 `/config` 已有整屏快照测试，会因新增一行而失败——此时更新该快照的期望值，**不要**删掉通知行。

- [x] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A && git commit -m "feat(config): 通知配置贯通到 RuntimeConfig 与 TUI 快照"
```

---

### Task 8: 焦点跟踪

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:132-190`（`run_tui` 的进出）
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:340-360`（`run_loop` 签名与状态）

**Interfaces:**
- Consumes: `TuiConfigSnapshot.notification_condition`
- Produces:
  - `run_loop` 内 `let focused = Arc::new(AtomicBool::new(true));`
  - `run_loop` 新签名参数 `focus_degraded: bool`（`app.rs:410`），供 `/config` 展示降级提示

- [x] **Step 1: 写失败的测试**

在 `app.rs` 测试模块加（用既有假 `EventSource` 模式）：

```rust
#[test]
fn focus_lost_marks_unfocused() {
    let focused = Arc::new(AtomicBool::new(true));
    let events = FakeEventSource::new(vec![Event::FocusLost]);
    let mut value = true;

    // 直接验证「事件 → 标志」的映射函数，不跑整个 TUI 循环。
    apply_focus_event(&Event::FocusLost, &focused);
    value = focused.load(Ordering::Relaxed);
    assert!(!value);

    apply_focus_event(&Event::FocusGained, &focused);
    assert!(focused.load(Ordering::Relaxed));
    let _ = events;
}

#[test]
fn should_notify_respects_condition() {
    assert!(should_notify(Condition::Always, false));
    assert!(should_notify(Condition::Always, true));
    assert!(should_notify(Condition::Unfocused, false));
    assert!(!should_notify(Condition::Unfocused, true));
}
```

`FakeEventSource` 若不存在，用 `app.rs` 里现有的假事件源类型替换（grep `impl EventSource for`）。本步的两个测试其实只依赖 `apply_focus_event` 与 `should_notify` 两个纯函数，`FakeEventSource` 那句可以删掉。

- [x] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent --lib tui::app::tests::should_notify
```

Expected: 编译失败，`cannot find function `should_notify``。

- [x] **Step 3: 写实现**

在 `app.rs` 里加两个自由函数：

```rust
/// 焦点事件 → 原子标志。
fn apply_focus_event(event: &Event, focused: &std::sync::atomic::AtomicBool) {
    use std::sync::atomic::Ordering;
    match event {
        Event::FocusGained => focused.store(true, Ordering::Relaxed),
        Event::FocusLost => focused.store(false, Ordering::Relaxed),
        _ => {}
    }
}

/// 当前是否应当发送通知。`focused` 为真表示窗口在前台。
fn should_notify(condition: crate::tui::notify::Condition, focused: bool) -> bool {
    match condition {
        crate::tui::notify::Condition::Always => true,
        crate::tui::notify::Condition::Unfocused => !focused,
    }
}
```

`run_tui` 里，`execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?` 改为：

```rust
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;

    // 焦点事件让 `unfocused` 条件才有意义。老 conhost 不支持这个序列，
    // 失败不致命：Windows 上退化成 always（宁可多响，不静默），其它平台
    // 只在日志里留痕。
    let focus_degraded = match execute!(stdout, EnableFocusChange) {
        Ok(()) => false,
        Err(error) => {
            if cfg!(windows) {
                tracing::info!(error = %error, "focus tracking unavailable; notifications will always fire");
                true
            } else {
                tracing::debug!(error = %error, "focus tracking unavailable");
                false
            }
        }
    };
```

顶部 `use crossterm::event::{...}` 里补 `EnableFocusChange`、`DisableFocusChange`。**这两个类型只在 `crossterm::event` 下**（`crossterm 0.28.1` 的 `event.rs:381` 定义 `EnableFocusChange`），不在 `crossterm::terminal`，所以必须加进本文件已有的那条 `crossterm::event` 导入里，不能另起一条 `terminal::` 导入。

清理段（`app.rs:177-186`）加一步，放在 `DisableBracketedPaste` 之后：

```rust
    if let Err(error) = execute!(terminal.backend_mut(), DisableFocusChange) {
        cleanup_error.get_or_insert(error);
    }
```

`focus_degraded` 必须传给 `run_loop` 才能在 `/config` 里显示。改动方式：给 `run_loop` 的签名加一个参数 `focus_degraded: bool`（`app.rs:410` 附近），`run_tui` 调用处传刚算出的值，`run_tui_with_backend_and_events_themed`（测试入口）传 `false`。

`run_loop` 里在 `/config` 分支（`app.rs:2663` 附近）拼接降级提示：

```rust
            SlashCommand::Config => {
                if args.is_some() {
                    history.push(
                        HistoryCell::Separator {
                            label: Some("用法: /config".to_string()),
                        },
                        width,
                    );
                } else {
                    let mut text = config.render(current_model);
                    if focus_degraded && config.notification_condition == "unfocused" {
                        text.push_str(
                            "\n  注意: 本终端不支持焦点事件，notify condition 已从 unfocused 退化为 always",
                        );
                    }
                    history.push(HistoryCell::Markdown { text }, width);
                }
                KeyOutcome::None
            }
```

- [x] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent --lib tui::app::tests::should_notify && cargo check -p yi-agent
```

Expected: PASS + 编译无错。

- [x] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A && git commit -m "feat(tui): 焦点跟踪与通知条件判定"
```

---

### Task 9: 事件循环接入 — 候选通知与绘制间隙 flush

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:340-380`（`run_loop` 初始化）
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:555-570`（事件映射）
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:700-720`（draw 之后 flush）

**Interfaces:**
- Consumes: `Notifier`、`HistoryState::last_assistant_text`、`permission_summary`、`should_notify`、`apply_focus_event`
- Produces: 无新公开接口（内部状态机）

- [x] **Step 1: 写失败的测试**

在 `app.rs` 测试模块加：

```rust
#[test]
fn turn_complete_uses_last_assistant_text() {
    let mut state = NotificationState::new();
    let mut history = HistoryState::new();
    history.push_event(AgentEvent::AssistantText("已完成重构".into()), 80);

    state.observe(
        &AgentEvent::Done { reason: DoneReason::EndTurn },
        &history,
        true,
    );
    assert_eq!(state.pending_text(), Some("已完成重构"));
}

#[test]
fn turn_complete_without_text_falls_back() {
    let mut state = NotificationState::new();
    let history = HistoryState::new();
    state.observe(
        &AgentEvent::Done { reason: DoneReason::EndTurn },
        &history,
        true,
    );
    assert_eq!(state.pending_text(), Some("yi-agent 回合完成"));
}

#[test]
fn permission_request_outranks_turn_complete() {
    let mut state = NotificationState::new();
    let history = HistoryState::new();

    state.observe(
        &AgentEvent::Done { reason: DoneReason::EndTurn },
        &history,
        true,
    );
    state.observe(
        &AgentEvent::PermissionRequest {
            request_id: 1,
            tool_name: "bash".into(),
            tool_input: serde_json::json!({ "command": "cargo test" }),
            prefix_suggestion: None,
            kind: yi_agent_core::permission::PermissionKind::Normal,
        },
        &history,
        true,
    );
    assert_eq!(state.pending_text(), Some("等待批准: cargo test"));
}

#[test]
fn lower_priority_candidate_does_not_replace_higher() {
    let mut state = NotificationState::new();
    let history = HistoryState::new();

    state.observe(
        &AgentEvent::PermissionRequest {
            request_id: 1,
            tool_name: "bash".into(),
            tool_input: serde_json::json!({ "command": "rm -rf /" }),
            prefix_suggestion: None,
            kind: yi_agent_core::permission::PermissionKind::Normal,
        },
        &history,
        true,
    );
    state.observe(
        &AgentEvent::Done { reason: DoneReason::EndTurn },
        &history,
        true,
    );
    assert_eq!(state.pending_text(), Some("等待批准: rm -rf /"));
}

#[test]
fn non_notifying_events_are_ignored() {
    let history = HistoryState::new();
    for event in [
        AgentEvent::Done { reason: DoneReason::MaxTurns },
        AgentEvent::Done { reason: DoneReason::Interrupted { reason: "user".into() } },
        AgentEvent::Cancelled,
        AgentEvent::Error(yi_agent_core::AgentError::ProviderTurnAdmission("boom".into())),
    ] {
        let mut state = NotificationState::new();
        state.observe(&event, &history, true);
        assert_eq!(state.pending_text(), None, "{event:?} must not notify");
    }
}

#[test]
fn disabled_notifications_produce_nothing() {
    let mut state = NotificationState::new();
    let history = HistoryState::new();
    state.observe(
        &AgentEvent::Done { reason: DoneReason::EndTurn },
        &history,
        false,
    );
    assert_eq!(state.pending_text(), None);
}

#[test]
fn flush_clears_pending() {
    let mut state = NotificationState::new();
    let history = HistoryState::new();
    state.observe(
        &AgentEvent::Done { reason: DoneReason::EndTurn },
        &history,
        true,
    );
    let taken = state.take();
    assert!(taken.is_some());
    assert_eq!(state.pending_text(), None);
}
```

**注意枚举形态**（已核对 `yi-agent-core/src/agent.rs`）：
- `AgentEvent::Error(AgentError)` 带的是 `AgentError` 枚举而非 `String`，且**没有** `Other` 变体。已核对的三个变体是 `Provider(ProviderError)`、`ProviderTurnAdmission(String)`、`Compact(CompactError)`（`agent.rs:541-548`）；上面用 `ProviderTurnAdmission` 构造，因为它直接收 `String`。
- `DoneReason::Interrupted { reason: String }` 已核对（`agent.rs:436-440`）。
- `ReasoningKind` 只有 `Content` 与 `Summary` 两变体（`provider.rs:92-97`），上面用 `Summary`。
- `AgentEvent` 没有 `AssistantMessage` 变体，流式正文是 `AssistantText(String)`。

- [x] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::app::notification_tests::turn_complete
```

Expected: 编译失败，`cannot find type `NotificationState``。

- [x] **Step 3: 写实现**

在 `app.rs` 里新增状态机（放在 `should_notify` 旁边）：

```rust
/// 待发通知。优先级数字小的先发、压过数字大的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum NotificationPriority {
    TurnComplete = 0,
    ApprovalRequested = 1,
}

#[derive(Debug, Clone)]
struct PendingNotification {
    priority: NotificationPriority,
    text: String,
}

/// 把 `AgentEvent` 收敛成「至多一条待发通知」。
///
/// 不直接写字节：通知必须等这一帧画完再发，否则会插进 ANSI 序列中间，
/// 把一帧切开造成渲染残影。
#[derive(Debug, Default)]
struct NotificationState {
    pending: Option<PendingNotification>,
}

impl NotificationState {
    fn new() -> Self {
        Self::default()
    }

    /// 观察一个事件，可能需要更新候选通知。
    ///
    /// `enabled` 为假时什么都不做；`history` 用于取回合正文（`Done` 不带
    /// 正文），必须在 `push_event` **之前**调用——`push_event` 会在末尾压
    /// 一条 `Separator`。
    fn observe(&mut self, event: &AgentEvent, history: &HistoryState, enabled: bool) {
        if !enabled {
            return;
        }
        let candidate = match event {
            AgentEvent::Done {
                reason: yi_agent_core::event::DoneReason::EndTurn,
            } => Some(PendingNotification {
                priority: NotificationPriority::TurnComplete,
                text: history
                    .last_assistant_text()
                    .map(crate::tui::notify::text::summarize)
                    .unwrap_or_else(|| crate::tui::notify::text::EMPTY_FALLBACK.to_string()),
            }),
            AgentEvent::PermissionRequest {
                tool_name,
                tool_input,
                ..
            } => Some(PendingNotification {
                priority: NotificationPriority::ApprovalRequested,
                text: format!(
                    "等待批准: {}",
                    crate::tui::notify::text::truncate_graphemes(
                        &crate::tui::history::permission_summary(tool_name, tool_input),
                        crate::tui::notify::text::MAX_GRAPHEMES,
                    )
                ),
            }),
            _ => None,
        };

        let Some(candidate) = candidate else { return };
        match &self.pending {
            Some(existing) if existing.priority >= candidate.priority => {}
            _ => self.pending = Some(candidate),
        }
    }

    /// 取出并清空。绘制完成后调用。
    fn take(&mut self) -> Option<PendingNotification> {
        self.pending.take()
    }

    #[cfg(test)]
    fn pending_text(&self) -> Option<&str> {
        self.pending.as_ref().map(|p| p.text.as_str())
    }
}
```

`DoneReason` 的实际路径按 `yi-agent-core` 的导出调整（若已 `use yi_agent_core::{AgentEvent, DoneReason}` 就直接用 `DoneReason`）。

在 `run_loop` 的局部状态初始化区（`app.rs:441-450`，`let mut popup: Option<CommandPopup> = None;` 那一串之后）加。注意 `history` 是 `run_loop` 的**入参**（`&mut HistoryState`），不是局部变量——事件循环里用 `history` 即可：

```rust
    let mut notifications = NotificationState::new();
    let notifications_enabled = config.notifications;
    let focused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let mut notifier = {
        let method = crate::tui::notify::Method::parse(&config.notification_method)
            .unwrap_or(crate::tui::notify::Method::Auto);
        let condition = crate::tui::notify::Condition::parse(&config.notification_condition)
            .unwrap_or(crate::tui::notify::Condition::Unfocused);
        crate::tui::notify::Notifier::new(
            method,
            condition,
            &crate::tui::notify::TerminalEnv::from_process(),
        )
    };
```

在事件处理循环里，`history.push_event(event, draw_text_width)` 这行**之前**插入：

```rust
            notifications.observe(&event, history, notifications_enabled);
```

在 `terminal.draw(|f| { ... })?;` 之后（`app.rs:706` 附近，`sync_trace_streams` 之前）插入：

```rust
        // 通知只在两帧之间发：插在帧中间会把 ANSI 序列切断。
        if let Some(pending) = notifications.take() {
            let is_focused = focused.load(std::sync::atomic::Ordering::Relaxed);
            if should_notify(notifier.condition(), is_focused) {
                let _ = notifier.notify(&pending.text);
            }
        }
```

在事件分派的开头（`match events.poll(...)? {` 里，`Some(Event::Key(key))` 之前）加一支：

```rust
            Some(event @ (Event::FocusGained | Event::FocusLost)) => {
                apply_focus_event(&event, &focused);
            }
```

- [x] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::app
```

Expected: PASS（10 个新测试 + 既有测试）。

> 注：`yi-agent` crate 没有 lib target，实际用 `--bin yi-agent`。

- [x] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A && git commit -m "feat(tui): 事件循环发出终端通知"
```

---

### Task 10: 收尾 — 快照回填、夹具与全量验证

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:2070`（若 Task 8 的动态值需要回填）
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:220`（`snapshot_for_tests`）
- Test: 全 crate

**Interfaces:**
- Consumes: 前九个任务的全部产物
- Produces: 可交付的完整功能

- [x] **Step 1: 确认 `snapshot_for_tests` 与 `main.rs` 的字段齐全**

```bash
cd yi-agent-rs && cargo check -p yi-agent
```

Expected: 无错。若有 `missing field` 报错，按 Task 7 Step 2 的取值逐项补齐。

- [x] **Step 2: 跑全 crate 测试**

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui
```

Expected: PASS。

- [x] **Step 3: 跑相邻 crate 测试**

```bash
cd yi-agent-rs && cargo test -p yi-agent-runtime && cargo test -p yi-agent-app-server --lib settings && cargo test -p yi-agent-web --lib
```

Expected: PASS。

- [x] **Step 4: 检查格式与 clippy**

```bash
cd yi-agent-rs && cargo fmt --all --check && cargo clippy -p yi-agent -p yi-agent-runtime -p yi-agent-web -p yi-agent-app-server --all-targets -- -D warnings
```

Expected: 无输出（fmt 通过）+ clippy 无 warning。

- [x] **Step 5: 手工验收（本机 macOS）**

在真实终端里跑：

```bash
cd yi-agent-rs && cargo run -p yi-agent -- --model "$YI_AGENT_TEST_MODEL"
```

然后：

1. 发一条会结束回合的消息（如「说一句话」），**把焦点切到别的窗口**，等回合结束 → 应弹系统通知，正文是回复摘要。
2. 窗口保持在前台再发一条 → **不应**弹通知。
3. 触发一次需要批准的 bash 命令 → 切走焦点 → 应弹「等待批准: <命令>」。

再验配置：

```bash
cat "$HOME/.yi-agent/settings.json"   # 或 $YI_AGENT_HOME 下的同名文件
```

把 `"notification_condition": "always"` 写进去，重跑第 2 步 → 这次应弹通知。

- [x] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A && git commit -m "test(tui): 通知功能收尾与全量验证"
```

---

## 附：验收清单

功能完成后，下面每条都应能独立复现：

| # | 判据 | 命令 / 操作 |
|---|---|---|
| 1 | 摘要与消毒纯函数正确 | `cargo test -p yi-agent --lib tui::notify::text` |
| 2 | 协议字节正确（含 tmux 包裹） | `cargo test -p yi-agent --lib tui::notify` |
| 3 | 五家白名单 / 其余 BEL | 同上（`whitelist_terminals_support_osc9` 等） |
| 4 | 焦点条件生效 | `cargo test -p yi-agent --lib tui::app::tests::should_notify` |
| 5 | 优先级与去抖 | `cargo test -p yi-agent --lib tui::app::tests::permission_request_outranks_turn_complete` |
| 6 | 配置非法值只清单字段 | `cargo test -p yi-agent-runtime --lib settings::tests::invalid_notification_method_is_dropped_not_fatal` |
| 7 | 设置页能读写三项 | `cargo test -p yi-agent-app-server --lib every_tui_notification_field` |
| 8 | 通知出现在两帧之间 | 手工验收第 1-3 步（无残影） |
| 9 | 退出后终端状态正常 | 手工：`q` 退出后 `echo test` 光标正常、无残留 ANSI |
