//! 通知正文的摘要与消毒。纯函数，不碰 IO。

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
        let Some(mid) = rest[start..].find("](") else {
            break;
        };
        let Some(end) = rest[start..].find(')') else {
            break;
        };
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
        // 每个家庭 emoji 是单个字素簇（5 个 char + 2 个 ZWJ）。
        let family = "👨‍👩‍👧";
        assert_eq!(family.chars().count(), 5);
        assert_eq!(family.graphemes(true).count(), 1);

        // 10 个家庭 = 10 个字素；max=5 时留 2 个再补 "..."，共 5 个可见字素。
        let text = family.repeat(10);
        let out = truncate_graphemes(&text, 5);
        assert_eq!(out.graphemes(true).count(), 5);
        assert!(out.ends_with("..."));

        // 关键：不得出现「半个」家庭 emoji（截断点落在 ZWJ 序列中间）。
        let body = out.strip_suffix("...").unwrap();
        assert_eq!(body, family.repeat(2), "truncation split a ZWJ cluster");
        assert!(!body.ends_with('\u{200d}'));
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
