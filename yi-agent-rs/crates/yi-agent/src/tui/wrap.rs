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
}
