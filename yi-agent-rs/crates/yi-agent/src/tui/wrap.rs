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
/// Maximum body lines rendered while collapsed. Keeps the decision menu on
/// screen in a 24-row terminal, where the history area is 21 rows.
pub(crate) const MAX_COLLAPSED_LINES: usize = 4;

/// Truncate `text` to at most `width` display columns, appending `…` when
/// characters had to be dropped.
///
/// Needed wherever a value is embedded in a fixed-shape single line (status
/// bar, popup list rows): truncating by `chars().count()` keeps CJK strings at
/// twice the intended width, and ratatui then clips the rest of the line.
pub(crate) fn truncate_to_width(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    let budget = width.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let ch_w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_w > budget {
            break;
        }
        out.push(ch);
        used += ch_w;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

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
            assert!(
                UnicodeWidthStr::width(line.as_str()) <= 4,
                "too wide: {line:?}"
            );
        }
    }

    #[test]
    fn cjk_counts_as_two_columns() {
        let src = "一二三四五六七";
        let out = wrap_by_display_width(src, 6, "", "");
        for line in &out {
            assert!(
                UnicodeWidthStr::width(line.as_str()) <= 6,
                "line too wide: {line:?}"
            );
        }
        assert_eq!(out.concat(), src);
    }

    #[test]
    fn words_are_not_split_when_they_fit_on_a_line() {
        let cmd = "cd /tmp && cargo test --lib permission";
        let out = wrap_by_display_width(cmd, 20, "", "");
        for line in &out {
            assert!(
                UnicodeWidthStr::width(line.as_str()) <= 20,
                "too wide: {line:?}"
            );
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
    fn truncate_to_width_keeps_cjk_within_budget() {
        // 40 CJK chars are 80 display columns; the old `chars().count()` cut
        // kept them all and the terminal clipped the rest of the line.
        let text = "中".repeat(40);
        for width in [10usize, 20, 40] {
            let out = truncate_to_width(&text, width);
            assert!(
                UnicodeWidthStr::width(out.as_str()) <= width,
                "width {width}: {out:?} is {} cols",
                UnicodeWidthStr::width(out.as_str())
            );
        }
    }

    #[test]
    fn truncate_to_width_leaves_fitting_text_untouched() {
        assert_eq!(truncate_to_width("hello", 10), "hello");
        assert_eq!(truncate_to_width("hello", 5), "hello");
        assert_eq!(truncate_to_width("", 0), "");
    }

    #[test]
    fn empty_input_yields_one_empty_line() {
        let out = wrap_by_display_width("", 10, "", "");
        assert_eq!(out, vec![""]);
    }
    proptest! {
        #![proptest_config(ProptestConfig {
            cases: std::env::var("PROPTEST_CASES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(256),
            ..ProptestConfig::default()
        })]

        /// 字符守恒：剥掉 prefix 后逐行拼接，必须与输入（去掉显式换行）逐字符相同。
        ///
        /// 这是本模块文档承诺的核心不变量：
        /// "Wrapping here never alters a character."
        /// 它也是「权限确认框里显示的命令必须与真正执行的完全一致」的底层保证。
        #[test]
        fn prop_preserves_every_character(
            text in "\\PC{0,120}",
            width in 1usize..60,
            first_prefix in "[ ]{0,4}",
            cont_prefix in "[ ]{0,4}",
        ) {
            let out = wrap_by_display_width(&text, width, &first_prefix, &cont_prefix);
            let mut rebuilt = String::new();
            for (i, line) in out.iter().enumerate() {
                let p = if i == 0 { &first_prefix } else { &cont_prefix };
                let body = line.strip_prefix(p.as_str())
                    .unwrap_or_else(|| panic!("line {i} 缺少预期 prefix {p:?}: {line:?}"));
                rebuilt.push_str(body);
            }
            let expected: String = text.chars().filter(|c| *c != '\n').collect();
            prop_assert_eq!(rebuilt, expected, "字符被改动或丢失");
        }

        /// 不 panic：任意输入（含控制字符、超长 CJK、空串）都不得崩。
        #[test]
        fn prop_never_panics(
            text in "\\PC{0,200}",
            width in 0usize..120,
            first_prefix in "\\PC{0,8}",
            cont_prefix in "\\PC{0,8}",
        ) {
            let _ = wrap_by_display_width(&text, width, &first_prefix, &cont_prefix);
        }

        /// 宽度约束**仅在「可用宽度装得下一个最宽字符」时**成立。
        ///
        /// 两个前置条件都来自实测反例，缺任一条都会对正确实现报假失败：
        ///
        /// 1. `wrap_by_display_width("hello world", 5, ">>>>> ", ">>>>>> ")`
        ///    产出 11 行、11 行全部超宽——prefix 比 width 宽时，
        ///    `width.saturating_sub(prefix_width).max(1)` 把可用宽度兜到 1。
        /// 2. `wrap_by_display_width("🤀﹔", 4, "  ", "   ")`——可用宽度
        ///    `4-3=1`，而 CJK / emoji 字符本身占 2 列，单字符就必然超宽。
        ///
        /// 两者都是同一处 `.max(1)` 的有意降级：宁可超宽也要每行有内容，
        /// 且绝不能丢字符（字符守恒由另一条性质守着）。故前置条件取
        /// 「可用宽度 >= 2」即一个字符的最大显示宽度。
        #[test]
        fn prop_respects_width_when_prefix_fits(
            text in "\\PC{0,120}",
            width in 4usize..80,
            first_prefix in "[ ]{0,3}",
            cont_prefix in "[ ]{0,3}",
        ) {
            // 每个字符最宽占 2 列（CJK / emoji），可用宽度必须容得下它。
            prop_assume!(UnicodeWidthStr::width(first_prefix.as_str()) + 2 <= width);
            prop_assume!(UnicodeWidthStr::width(cont_prefix.as_str()) + 2 <= width);
            let out = wrap_by_display_width(&text, width, &first_prefix, &cont_prefix);
            for line in &out {
                prop_assert!(
                    UnicodeWidthStr::width(line.as_str()) <= width,
                    "行宽 {} 超过上限 {}：{:?}",
                    UnicodeWidthStr::width(line.as_str()), width, line
                );
            }
        }
    }
}
