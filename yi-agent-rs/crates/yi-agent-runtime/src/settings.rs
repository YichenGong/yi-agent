//! `settings.json`：agent 可调参数的唯一持久化位置。
//!
//! 取代过去的 `~/.yi-agent/.env`。本模块同时承担三件事：类型化存储、
//! 字段元数据（供 web 与桌面端渲染表单）、以及一次性迁移所需的 `.env` 行解析。

use std::collections::BTreeMap;

/// 解析一行 `.env`，口径与 `dotenvy` 对齐。
///
/// 对齐的四条：
/// - 剥 `export ` 前缀（`dotenvy` 接受它，旧实现当成键名的一部分）；
/// - 未加引号的 ` #`（空格 + `#`）之后是注释（旧实现并进值）；
/// - 引号内的 `#` 是字面值；
/// - 值两端的配对引号被剥掉。
///
/// 返回 `None` 表示这一行不产生键值对（注释、空行、无 `=`、键为空）。
pub fn parse_env_line(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
    let eq = line.find('=')?;
    let key = line[..eq].trim();
    if key.is_empty() {
        return None;
    }
    Some((key.to_string(), parse_env_value(line[eq + 1..].trim())))
}

/// 解析 `=` 右侧。先按「是否被配对引号包裹」分流：引号内的一切都是字面值，
/// 否则在第一个前面有空格的 `#` 处截断。
///
/// **不支持转义序列。** `"a\\nb"` 会被读成字面的反斜杠加 `n`，而不是换行；
/// `"a\\"b"` 也不会被识别成转义引号。dotenvy 会展开这些。迁移场景里带转义的
/// 值几乎不存在（`.env` 里的都是数字、URL、API key），所以宁可保持实现简单，
/// 也不要手写一个半对的转义展开器。若将来真的遇到，正确做法是用 dotenvy 的
/// 解析器，而不是在这里加 `replace` 分支。
fn parse_env_value(raw: &str) -> String {
    let quoted = |s: &str, q: char| s.len() >= 2 && s.starts_with(q) && s.ends_with(q);
    if quoted(raw, '"') || quoted(raw, '\'') {
        return raw[1..raw.len() - 1].to_string();
    }
    // 找「前面是空白」的 `#`。没有则整串是值。
    let bytes = raw.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'#' || index == 0 {
            continue;
        }
        if bytes[index - 1].is_ascii_whitespace() {
            return raw[..index].trim_end().to_string();
        }
    }
    raw.trim_end().to_string()
}

/// 解析整份 `.env` 文本；同一键出现多次时后者覆盖前者（与 `dotenvy` 一致）。
pub fn parse_env_text(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        if let Some((key, value)) = parse_env_line(line) {
            out.insert(key, value);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_key_value() {
        assert_eq!(
            parse_env_line("YI_AGENT_MAX_TURNS=500"),
            Some(("YI_AGENT_MAX_TURNS".to_string(), "500".to_string()))
        );
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(
            parse_env_line("  KEY = value  "),
            Some(("KEY".to_string(), "value".to_string()))
        );
    }

    /// dotenvy 会把未加引号的 ` #` 之后当注释截掉。旧实现把它并进了值。
    #[test]
    fn unquoted_trailing_comment_is_stripped() {
        assert_eq!(
            parse_env_line("MODEL_API_KEY=sk-x #note"),
            Some(("MODEL_API_KEY".to_string(), "sk-x".to_string()))
        );
    }

    /// `#` 紧贴值、中间没有空格时不是注释——这是 dotenvy 的口径。
    #[test]
    fn hash_without_preceding_space_is_part_of_the_value() {
        assert_eq!(
            parse_env_line("KEY=ab#cd"),
            Some(("KEY".to_string(), "ab#cd".to_string()))
        );
    }

    /// 引号内的 `#` 是字面值，绝不能截断。
    #[test]
    fn hash_inside_quotes_is_kept() {
        assert_eq!(
            parse_env_line(r#"KEY="ab # cd""#),
            Some(("KEY".to_string(), "ab # cd".to_string()))
        );
        assert_eq!(
            parse_env_line("KEY='ab # cd'"),
            Some(("KEY".to_string(), "ab # cd".to_string()))
        );
    }

    /// dotenvy 接受 `export` 前缀；旧实现会把 `export KEY` 当成键名，等于整行丢失。
    #[test]
    fn export_prefix_is_stripped() {
        assert_eq!(
            parse_env_line("export MODEL_API_KEY=sk-x"),
            Some(("MODEL_API_KEY".to_string(), "sk-x".to_string()))
        );
    }

    #[test]
    fn paired_quotes_are_stripped() {
        assert_eq!(
            parse_env_line(r#"A="quoted""#),
            Some(("A".to_string(), "quoted".to_string()))
        );
        assert_eq!(
            parse_env_line("B='single'"),
            Some(("B".to_string(), "single".to_string()))
        );
    }

    #[test]
    fn comments_and_blank_lines_produce_nothing() {
        assert_eq!(parse_env_line("# just a comment"), None);
        assert_eq!(parse_env_line(""), None);
        assert_eq!(parse_env_line("   "), None);
    }

    #[test]
    fn a_line_without_equals_produces_nothing() {
        assert_eq!(parse_env_line("NO_EQUALS_HERE"), None);
    }

    #[test]
    fn a_line_with_an_empty_key_produces_nothing() {
        assert_eq!(parse_env_line("=value"), None);
        assert_eq!(parse_env_line(" =value"), None);
    }

    #[test]
    fn empty_value_is_allowed() {
        assert_eq!(
            parse_env_line("KEY="),
            Some(("KEY".to_string(), String::new()))
        );
    }

    #[test]
    fn text_parsing_collects_lines_and_later_wins() {
        let text = "\
# comment
A=1
export B=2
A=3
malformed line
";
        let parsed = parse_env_text(text);
        assert_eq!(parsed.get("A").map(String::as_str), Some("3"));
        assert_eq!(parsed.get("B").map(String::as_str), Some("2"));
        assert_eq!(parsed.len(), 2);
    }

    /// 迁移读的是真实 `.env`：非 ASCII 值与注释混排必须正确。
    #[test]
    fn non_ascii_values_are_handled() {
        assert_eq!(
            parse_env_line("A=中文值 # 注释"),
            Some(("A".to_string(), "中文值".to_string()))
        );
    }
}
