//! MCP 工具对外暴露给 LLM 的限定名。
//!
//! provider 对工具名有字符集与长度限制(Anthropic):`^[a-zA-Z0-9_-]{1,64}$`。

/// 工具名前缀,用于与内置工具区分。
const PREFIX: &str = "mcp__";
/// provider 允许的工具名最大字节数。
const MAX_LEN: usize = 64;

/// 把任意字符映射到 provider 允许的字符集:非字母数字、`_`、`-` 一律替换为 `_`。
fn sanitize(part: &str) -> String {
    part.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// 构造 `mcp__{server}__{tool}`,先按 provider 字符集清洗,再截断到 64 字节。
///
/// 清洗后只剩 ASCII,因此按字节截断不会破坏 UTF-8 边界。
pub fn qualified_name(server: &str, tool: &str) -> String {
    let base = format!("{PREFIX}{}__{}", sanitize(server), sanitize(tool));
    if base.len() <= MAX_LEN {
        return base;
    }
    // 清洗后全是 ASCII,按字节截断安全。
    base[..MAX_LEN].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualified_name_uses_double_underscore() {
        assert_eq!(
            qualified_name("filesystem", "read_file"),
            "mcp__filesystem__read_file"
        );
    }

    #[test]
    fn sanitizes_invalid_chars() {
        assert_eq!(
            qualified_name("my server", "read/file"),
            "mcp__my_server__read_file"
        );
    }

    #[test]
    fn truncates_to_64_bytes() {
        let long = "a".repeat(200);
        let q = qualified_name(&long, &long);
        assert!(q.len() <= 64, "got {} bytes", q.len());
        assert!(q.starts_with("mcp__"));
    }

    #[test]
    fn sanitized_name_is_valid_tool_name_charset() {
        let q = qualified_name("srv..x", "weird*name");
        for ch in q.chars() {
            assert!(
                ch.is_ascii_alphanumeric() || ch == '_' || ch == '-',
                "bad char {ch:?}"
            );
        }
    }
}
