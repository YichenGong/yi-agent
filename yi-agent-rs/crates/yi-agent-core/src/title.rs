//! 首轮结束后生成 thread 标题：材料拼接与标题清洗。
//!
//! 与 `compact.rs` 同风格：纯逻辑（本模块）与"调用 provider 生成标题"分开放，
//! 便于单测覆盖清洗规则；生成失败由调用方静默回退到截断标题。

/// 标题字符数上限，与 `thread_store::title_from` 的截断上限保持一致。
pub const TITLE_MAX_CHARS: usize = 30;

/// 材料每一侧（提问 / 回复）截断后的最大字符数。
pub const TITLE_MATERIAL_SIDE_CHARS: usize = 2000;

/// 把首轮的两侧文本拼成标题模型的输入材料。
///
/// `assistant_text` 为 `None`（首轮无文本回复，例如纯工具调用）时省略该段。
pub fn build_title_material(user_text: &str, assistant_text: Option<&str>) -> String {
    let truncate = |s: &str| -> String { s.chars().take(TITLE_MATERIAL_SIDE_CHARS).collect() };
    let mut out = format!("用户提问：{}", truncate(user_text));
    if let Some(a) = assistant_text {
        out.push_str("\n\n助手回复：");
        out.push_str(&truncate(a));
    }
    out
}

/// 把模型返回的原始文本清洗成一个可用标题。
///
/// 步骤：去首尾空白 → 取首个非空行 → 去包裹引号 → 去 `标题:` / `Title:` 前缀
/// → 截断至 [`TITLE_MAX_CHARS`]。结果为空返回 `None`（调用方据此回退兜底标题）。
pub fn sanitize_title(raw: &str) -> Option<String> {
    let first_line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    let mut text = first_line.trim_matches(|c| matches!(c, '"' | '\'' | '「' | '」' | '“' | '”'));
    for prefix in ["标题:", "标题：", "Title:", "title:"] {
        if let Some(rest) = text.strip_prefix(prefix) {
            text = rest.trim();
            break;
        }
    }
    let capped: String = text.chars().take(TITLE_MAX_CHARS).collect();
    let capped = capped.trim().to_string();
    if capped.is_empty() {
        None
    } else {
        Some(capped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn material_joins_both_sides_with_labels() {
        let m = build_title_material("帮我修一下登录页的报错", Some("好的，我看下 AuthForm。"));
        assert!(m.starts_with("用户提问：帮我修一下登录页的报错"));
        assert!(m.contains("助手回复：好的，我看下 AuthForm。"));
    }

    #[test]
    fn material_omits_assistant_when_absent() {
        let m = build_title_material("你好", None);
        assert!(m.starts_with("用户提问：你好"));
        assert!(!m.contains("助手回复"), "absent assistant must be omitted");
    }

    #[test]
    fn material_truncates_each_side() {
        let long = "字".repeat(TITLE_MATERIAL_SIDE_CHARS + 500);
        let m = build_title_material(&long, Some(&long));
        // 每侧最多 2000 字符：总字符数上界 = 两侧材料 + 两个标签 + 一个空行。
        assert!(
            m.chars().count()
                <= TITLE_MATERIAL_SIDE_CHARS * 2 + "用户提问：助手回复：\n\n".chars().count()
        );
    }

    #[test]
    fn sanitize_trims_and_takes_first_nonempty_line() {
        assert_eq!(
            sanitize_title("  修复登录报错  \n解释一下").as_deref(),
            Some("修复登录报错")
        );
    }

    #[test]
    fn sanitize_strips_wrapping_quotes_and_prefix() {
        assert_eq!(
            sanitize_title("\"修复登录报错\"").as_deref(),
            Some("修复登录报错")
        );
        assert_eq!(
            sanitize_title("「修复登录报错」").as_deref(),
            Some("修复登录报错")
        );
        assert_eq!(
            sanitize_title("标题：修复登录报错").as_deref(),
            Some("修复登录报错")
        );
        assert_eq!(
            sanitize_title("Title: fix login").as_deref(),
            Some("fix login")
        );
    }

    #[test]
    fn sanitize_caps_length() {
        let long = "字".repeat(100);
        let out = sanitize_title(&long).unwrap();
        assert_eq!(out.chars().count(), TITLE_MAX_CHARS);
    }

    #[test]
    fn sanitize_blank_returns_none() {
        assert_eq!(sanitize_title("   \n  "), None);
    }
}
